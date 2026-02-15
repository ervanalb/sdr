use std::hash::Hash;

use eframe::wgpu;

const SCROLL_SPEED: f32 = 1.0;
const ZOOM_SPEED: f32 = 1.0;

pub struct StaticResources {
    render_pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    resources: Option<Resources>,
}

pub struct Resources {
    vertex_buffer: wgpu::Buffer,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    num_vertices: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniforms {
    viewport_size: [f32; 2],
    translation: [f32; 2],
    scale: f32,
    _padding: [f32; 3],
}

pub fn init(cc: &eframe::CreationContext<'_>) {
    let wgpu_render_state = cc.wgpu_render_state.as_ref().unwrap();
    let device = &wgpu_render_state.device;
    let target_format = wgpu_render_state.target_format;

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Canvas Shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("canvas_shader.wgsl").into()),
    });

    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("Viewport Bind Group Layout"),
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

    let render_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("Canvas Render Pipeline Layout"),
        bind_group_layouts: &[&bind_group_layout],
        push_constant_ranges: &[],
    });

    let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("Canvas Render Pipeline"),
        layout: Some(&render_pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
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
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: Some(wgpu::Face::Back),
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

    wgpu_render_state
        .renderer
        .write()
        .callback_resources
        .insert(StaticResources {
            render_pipeline,
            bind_group_layout,
            resources: None,
        });
}

struct Callback {
    viewport_size: egui::Vec2,
    translation: egui::Vec2,
    scale: f32,
}

impl egui_wgpu::CallbackTrait for Callback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let static_resources: &mut StaticResources = resources.get_mut().unwrap();

        if static_resources.resources.is_none() {
            // Create placeholder geometry: a simple grid
            let grid_size = 10;
            let spacing = 50.0;
            let mut vertices = Vec::new();

            // Vertical lines
            for i in -grid_size..=grid_size {
                let x = i as f32 * spacing;
                vertices.push([x, -grid_size as f32 * spacing]);
                vertices.push([x, grid_size as f32 * spacing]);
            }

            // Horizontal lines
            for i in -grid_size..=grid_size {
                let y = i as f32 * spacing;
                vertices.push([-grid_size as f32 * spacing, y]);
                vertices.push([grid_size as f32 * spacing, y]);
            }

            let num_vertices = vertices.len() as u32;
            let vertex_buffer_size = std::mem::size_of::<[f32; 2]>() as u64 * num_vertices as u64;

            let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("Canvas Vertex Buffer"),
                size: vertex_buffer_size,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("Viewport Uniform Buffer"),
                size: std::mem::size_of::<ViewportUniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Viewport Bind Group"),
                layout: &static_resources.bind_group_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buffer.as_entire_binding(),
                }],
            });

            queue.write_buffer(&vertex_buffer, 0, bytemuck::cast_slice(&vertices));

            static_resources.resources = Some(Resources {
                vertex_buffer,
                uniform_buffer,
                bind_group,
                num_vertices,
            });
        }

        let resources = static_resources.resources.as_ref().unwrap();

        // Update uniform buffer with viewport parameters
        let uniforms = ViewportUniforms {
            viewport_size: [self.viewport_size.x, self.viewport_size.y],
            translation: [self.translation.x, self.translation.y],
            scale: self.scale,
            _padding: [0.0; 3],
        };
        queue.write_buffer(
            &resources.uniform_buffer,
            0,
            bytemuck::cast_slice(&[uniforms]),
        );

        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let static_resources: &StaticResources = resources.get().unwrap();
        if let Some(resources) = &static_resources.resources {
            render_pass.set_pipeline(&static_resources.render_pipeline);
            render_pass.set_bind_group(0, &resources.bind_group, &[]);
            render_pass.set_vertex_buffer(0, resources.vertex_buffer.slice(..));
            render_pass.draw(0..resources.num_vertices, 0..1);
        }
    }
}

pub struct Viewport {
    pub translation: egui::Vec2,
    pub scale: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            translation: egui::Vec2::ZERO,
            scale: 1.0,
        }
    }
}

pub fn ui(ui: &mut egui::Ui, _id_source: impl Hash + std::fmt::Debug, viewport: &mut Viewport) {
    let size = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());

    // Handle scroll and zoom
    if response.hovered() {
        let (scroll_delta, zoom_delta) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));

        // Ctrl + scroll wheel: zoom
        if zoom_delta != 1.0 {
            let old_scale = viewport.scale;
            viewport.scale *= zoom_delta.powf(ZOOM_SPEED);
            viewport.scale = viewport.scale.clamp(0.01, 100.0);

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
            viewport.scale *= multi_touch.zoom_delta;
            viewport.scale = viewport.scale.clamp(0.01, 100.0);

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
            viewport_size: size,
            translation: viewport.translation,
            scale: viewport.scale,
        },
    ));
}
