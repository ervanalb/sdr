use std::collections::HashMap;
use std::hash::Hash;
use std::time::Instant;

use eframe::wgpu;
use sdr::waterfall_gpu::ChunkDrawInfo;

use super::waterfall::WaterfallRenderer;

const SCROLL_SPEED: f32 = 1.0;
const WHEEL_ZOOM_SPEED: f32 = 1.0;
const DRAG_ZOOM_SPEED: f32 = 1.01;

pub struct StaticResources {
    target_format: wgpu::TextureFormat,
    instances: HashMap<egui::Id, CanvasResources>,
}

struct CanvasResources {
    grid_pipeline: wgpu::RenderPipeline,
    grid_vertex_buffer: wgpu::Buffer,
    grid_num_vertices: u32,
    uniform_buffer: wgpu::Buffer,
    grid_bind_group: wgpu::BindGroup,
    waterfall_renderer: WaterfallRenderer,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniforms {
    viewport_size: [f32; 2],
    translation: [f32; 2],
    scale: [f32; 2],
    _padding: [f32; 2],
}

pub fn init(cc: &eframe::CreationContext<'_>) {
    let wgpu_render_state = cc.wgpu_render_state.as_ref().unwrap();
    let target_format = wgpu_render_state.target_format;

    wgpu_render_state
        .renderer
        .write()
        .callback_resources
        .insert(StaticResources {
            target_format,
            instances: HashMap::new(),
        });
}

struct Callback {
    id: egui::Id,
    viewport_size: egui::Vec2,
    translation: egui::Vec2,
    scale: egui::Vec2,
    waterfall_chunks: Vec<ChunkDrawInfo>,
    reference_time: Instant,
}

impl egui_wgpu::CallbackTrait for Callback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let static_resources: &mut StaticResources = callback_resources.get_mut().unwrap();
        let target_format = static_resources.target_format;

        // Get or create canvas resources
        let resources = static_resources
            .instances
            .entry(self.id)
            .or_insert_with(|| {
                // Create grid shader and pipeline
                let grid_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("Grid Shader"),
                    source: wgpu::ShaderSource::Wgsl(include_str!("canvas_shader.wgsl").into()),
                });

                let grid_bind_group_layout =
                    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: Some("Grid Bind Group Layout"),
                        entries: &[wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::VERTEX,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        }],
                    });

                let grid_pipeline_layout =
                    device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("Grid Pipeline Layout"),
                        bind_group_layouts: &[&grid_bind_group_layout],
                        push_constant_ranges: &[],
                    });

                let grid_pipeline =
                    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some("Grid Render Pipeline"),
                        layout: Some(&grid_pipeline_layout),
                        vertex: wgpu::VertexState {
                            module: &grid_shader,
                            entry_point: Some("vs_main"),
                            buffers: &[wgpu::VertexBufferLayout {
                                array_stride: std::mem::size_of::<[f32; 2]>()
                                    as wgpu::BufferAddress,
                                step_mode: wgpu::VertexStepMode::Vertex,
                                attributes: &[wgpu::VertexAttribute {
                                    offset: 0,
                                    shader_location: 0,
                                    format: wgpu::VertexFormat::Float32x2,
                                }],
                            }],
                            compilation_options: Default::default(),
                        },
                        fragment: Some(wgpu::FragmentState {
                            module: &grid_shader,
                            entry_point: Some("fs_main"),
                            targets: &[Some(wgpu::ColorTargetState {
                                format: target_format,
                                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                                write_mask: wgpu::ColorWrites::ALL,
                            })],
                            compilation_options: Default::default(),
                        }),
                        primitive: wgpu::PrimitiveState {
                            topology: wgpu::PrimitiveTopology::LineList,
                            strip_index_format: None,
                            front_face: wgpu::FrontFace::Ccw,
                            cull_mode: None,
                            polygon_mode: wgpu::PolygonMode::Fill,
                            unclipped_depth: false,
                            conservative: false,
                        },
                        depth_stencil: None,
                        multisample: wgpu::MultisampleState {
                            count: 1,
                            mask: !0,
                            alpha_to_coverage_enabled: false,
                        },
                        multiview: None,
                        cache: None,
                    });

                // Create grid geometry: a simple grid
                let x_grid_count = 100;
                let x_spacing = 1e7;
                let y_grid_count = 12;
                let y_spacing = 10.;
                let mut vertices = Vec::new();

                // Vertical lines
                for i in 0..=x_grid_count {
                    let x = i as f32 * x_spacing;
                    vertices.push([x, 0.]);
                    vertices.push([x, y_grid_count as f32 * y_spacing]);
                }

                // Horizontal lines
                for i in 0..=y_grid_count {
                    let y = i as f32 * y_spacing;
                    vertices.push([0., y]);
                    vertices.push([x_grid_count as f32 * x_spacing, y]);
                }

                let grid_num_vertices = vertices.len() as u32;

                let grid_vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Grid Vertex Buffer"),
                    size: (vertices.len() * std::mem::size_of::<[f32; 2]>()) as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });

                queue.write_buffer(&grid_vertex_buffer, 0, bytemuck::cast_slice(&vertices));

                // Create uniform buffer
                let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Viewport Uniform Buffer"),
                    size: std::mem::size_of::<ViewportUniforms>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });

                let grid_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("Grid Bind Group"),
                    layout: &grid_bind_group_layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniform_buffer.as_entire_binding(),
                    }],
                });

                // Create waterfall renderer
                let waterfall_renderer = WaterfallRenderer::new(device, target_format);

                CanvasResources {
                    grid_pipeline,
                    grid_vertex_buffer,
                    grid_num_vertices,
                    uniform_buffer,
                    grid_bind_group,
                    waterfall_renderer,
                }
            });

        // Update uniform buffer with viewport parameters
        let uniforms = ViewportUniforms {
            viewport_size: [self.viewport_size.x, self.viewport_size.y],
            translation: [self.translation.x, self.translation.y],
            scale: [self.scale.x, self.scale.y],
            _padding: [0.0; 2],
        };
        queue.write_buffer(
            &resources.uniform_buffer,
            0,
            bytemuck::cast_slice(&[uniforms]),
        );

        // Prepare waterfall draw calls
        resources.waterfall_renderer.prepare(
            self.waterfall_chunks.clone(),
            device,
            queue,
            &resources.uniform_buffer,
            self.reference_time,
        );

        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        let static_resources: &StaticResources = callback_resources.get().unwrap();

        if let Some(resources) = static_resources.instances.get(&self.id) {
            // Draw grid
            render_pass.set_pipeline(&resources.grid_pipeline);
            render_pass.set_bind_group(0, &resources.grid_bind_group, &[]);
            render_pass.set_vertex_buffer(0, resources.grid_vertex_buffer.slice(..));
            render_pass.draw(0..resources.grid_num_vertices, 0..1);

            // Draw waterfall
            resources.waterfall_renderer.render(render_pass);
        }
    }
}

pub struct Viewport {
    pub translation: egui::Vec2,
    pub scale: egui::Vec2,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            translation: egui::Vec2::ZERO,
            scale: egui::vec2(1e-3, 1e3),
        }
    }
}

pub fn ui(
    ui: &mut egui::Ui,
    id_source: impl Hash + std::fmt::Debug,
    viewport: &mut Viewport,
    waterfall_chunks: Vec<ChunkDrawInfo>,
    reference_time: Instant,
    //band_info: BandInfo,
) {
    let id = ui.id().with(&id_source);
    let size = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());

    // Handle scroll and zoom
    if response.hovered() {
        let (scroll_delta, zoom_delta) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));

        // Ctrl + scroll wheel: zoom
        if zoom_delta != 1.0 {
            let old_scale = viewport.scale;
            viewport.scale = viewport.scale * zoom_delta.powf(WHEEL_ZOOM_SPEED);

            // Keep pointer position stationary
            if let Some(pointer_pos) = response.hover_pos() {
                let pointer_canvas = pointer_pos - rect.min;
                viewport.translation = pointer_canvas
                    - (pointer_canvas - viewport.translation) * (viewport.scale / old_scale);
            }
        }
        // Regular scroll: pan the canvas
        viewport.translation += scroll_delta * SCROLL_SPEED;
    }

    // Handle mouse button drag for panning
    if response.dragged_by(egui::PointerButton::Primary) {
        let drag = response.drag_delta();
        viewport.translation += drag;
    }

    // Handle right mouse button drag for zooming
    if response.dragged_by(egui::PointerButton::Secondary) {
        let drag = response.drag_delta();
        let old_scale = viewport.scale;
        viewport.scale =
            old_scale * egui::vec2(DRAG_ZOOM_SPEED.powf(drag.x), DRAG_ZOOM_SPEED.powf(drag.y));
        // Keep pointer position stationary
        if let Some(pointer_pos) = response.hover_pos() {
            let pointer_canvas = pointer_pos - rect.min;
            viewport.translation = pointer_canvas
                - (pointer_canvas - viewport.translation) * (viewport.scale / old_scale);
        }
    }

    // Handle multi-touch gestures
    if let Some(multi_touch) = ui.input(|i| i.multi_touch()) {
        // Pinch to zoom
        if multi_touch.zoom_delta != 1.0 {
            let old_scale = viewport.scale;
            viewport.scale = old_scale * multi_touch.zoom_delta;

            let gesture_center = multi_touch.translation_delta;
            viewport.translation = gesture_center
                - (gesture_center - viewport.translation) * (viewport.scale / old_scale);
        }

        // Two-finger pan
        viewport.translation += multi_touch.translation_delta;
    }

    let overall_size = egui::vec2(1e9, 120.);

    let min_scale = size / overall_size;
    let max_zoom = 1000.;
    viewport.scale = viewport.scale.clamp(min_scale, min_scale * max_zoom);

    let max_translation = (viewport.scale * overall_size - size).max(egui::Vec2::ZERO);

    viewport.translation = viewport
        .translation
        .clamp(-max_translation, egui::Vec2::ZERO);

    ui.painter().add(egui_wgpu::Callback::new_paint_callback(
        rect,
        Callback {
            id,
            viewport_size: size,
            translation: viewport.translation,
            scale: viewport.scale,
            waterfall_chunks,
            reference_time,
        },
    ));
}
