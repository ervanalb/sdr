use std::collections::HashMap;
use std::hash::Hash;
use std::time::Instant;

use eframe::wgpu;
use sdr::waterfall_gpu::ChunkDrawInfo;

use super::waterfall::WaterfallRenderer;

const SCROLL_SPEED: f32 = 1.0;
const ZOOM_SPEED: f32 = 1.0;

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
    current_time: Instant,
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
                let x_grid_size = 100;
                let x_spacing = 1e6;
                let y_grid_size = 10;
                let y_spacing = 1.;
                let mut vertices = Vec::new();

                // Vertical lines
                for i in -x_grid_size..=x_grid_size {
                    let x = i as f32 * x_spacing;
                    vertices.push([x, -y_grid_size as f32 * y_spacing]);
                    vertices.push([x, y_grid_size as f32 * y_spacing]);
                }

                // Horizontal lines
                for i in -y_grid_size..=y_grid_size {
                    let y = i as f32 * y_spacing;
                    vertices.push([-x_grid_size as f32 * x_spacing, y]);
                    vertices.push([x_grid_size as f32 * x_spacing, y]);
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
            self.current_time,
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

fn clamp_scale(mut scale: egui::Vec2) -> egui::Vec2 {
    scale.x = scale.x.clamp(1e-6, 1e0);
    scale.y = scale.y.clamp(1e-1, 1e5);
    scale
}

pub fn ui(
    ui: &mut egui::Ui,
    id_source: impl Hash + std::fmt::Debug,
    viewport: &mut Viewport,
    waterfall_chunks: Vec<ChunkDrawInfo>,
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
            viewport.scale = clamp_scale(old_scale * zoom_delta.powf(ZOOM_SPEED));

            // Keep pointer position stationary
            if let Some(pointer_pos) = response.hover_pos() {
                let pointer_canvas = pointer_pos - rect.center();
                let pointer_canvas = egui::vec2(pointer_canvas.x, -pointer_canvas.y);
                viewport.translation = pointer_canvas
                    - (pointer_canvas - viewport.translation) * (viewport.scale / old_scale);
            }
        }
        // Regular scroll: pan the canvas
        viewport.translation += egui::vec2(scroll_delta.x, -scroll_delta.y) * SCROLL_SPEED;
    }

    // Handle middle mouse button drag for panning
    if response.dragged_by(egui::PointerButton::Middle) {
        let drag = response.drag_delta();
        viewport.translation += egui::vec2(drag.x, -drag.y);
    }

    // Handle multi-touch gestures
    if let Some(multi_touch) = ui.input(|i| i.multi_touch()) {
        // Pinch to zoom
        if multi_touch.zoom_delta != 1.0 {
            let old_scale = viewport.scale;
            viewport.scale = clamp_scale(old_scale * multi_touch.zoom_delta);

            let gesture_center = multi_touch.translation_delta + rect.center().to_vec2();
            let gesture_center = egui::vec2(gesture_center.x, -gesture_center.y);
            viewport.translation = gesture_center
                - (gesture_center - viewport.translation) * (viewport.scale / old_scale);
        }

        // Two-finger pan
        viewport.translation += egui::vec2(
            multi_touch.translation_delta.x,
            -multi_touch.translation_delta.y,
        );
    }

    ui.painter().add(egui_wgpu::Callback::new_paint_callback(
        rect,
        Callback {
            id,
            viewport_size: size,
            translation: viewport.translation,
            scale: viewport.scale,
            waterfall_chunks,
            current_time: Instant::now(),
        },
    ));
}
