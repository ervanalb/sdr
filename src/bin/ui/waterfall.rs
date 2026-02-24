use eframe::wgpu;
use sdr::waterfall_gpu::ChunkDrawInfo;
use std::{ops::Range, time::Instant};

const BUFFER_LEN: usize = 4096;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct WaterfallVertex {
    position: [f32; 2],    // x = frequency, y = time
    uv: [f32; 2],          // in texel coordinates (u=0..width, v=0..height)
    color_range: [f32; 2], // should be a uniform but its less overhead to pass it here
}

pub struct WaterfallRenderer {
    render_pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    vertex_buffer: wgpu::Buffer,
    vertices: Vec<WaterfallVertex>,
    draw_calls: Vec<DrawCall>,
}

impl WaterfallRenderer {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Waterfall Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("waterfall_shader.wgsl").into()),
        });

        // Create bind group layout for uniform buffer, texture, and sampler
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Waterfall Bind Group Layout"),
            entries: &[
                // Viewport uniforms
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // Textures
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // Sampler
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let render_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Waterfall Pipeline Layout"),
                bind_group_layouts: &[&bind_group_layout],
                push_constant_ranges: &[],
            });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Waterfall Render Pipeline"),
            layout: Some(&render_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<WaterfallVertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        // position
                        wgpu::VertexAttribute {
                            offset: 0,
                            shader_location: 0,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                        // uv
                        wgpu::VertexAttribute {
                            offset: 8,
                            shader_location: 1,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                        // color_range
                        wgpu::VertexAttribute {
                            offset: 16,
                            shader_location: 2,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                    ],
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

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Waterfall Sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Create vertex buffer
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Waterfall Vertex Buffer"),
            size: (BUFFER_LEN * std::mem::size_of::<WaterfallVertex>() * 6) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            render_pipeline,
            bind_group_layout,
            sampler,
            vertex_buffer,
            vertices: vec![],
            draw_calls: vec![],
        }
    }

    pub fn prepare(
        &mut self,
        chunks: Vec<ChunkDrawInfo>,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        uniform_buffer: &wgpu::Buffer,
        reference_time: Instant,
    ) {
        self.draw_calls.clear();
        self.vertices.clear();

        for chunk in chunks {
            // Build vertices for all chunks using this texture
            // Calculate time coordinates (Y axis)
            let y_start = reference_time.duration_since(chunk.end_time).as_secs_f32();
            let y_end = reference_time
                .duration_since(chunk.start_time)
                .as_secs_f32();

            if y_end < y_start {
                continue;
            }

            // Calculate frequency coordinates (X axis)
            let x_left = (chunk.receive_stream_descriptor_ptr.frequency
                - 0.5 * chunk.receive_stream_descriptor_ptr.sample_rate)
                as f32;
            let x_right = (chunk.receive_stream_descriptor_ptr.frequency
                + 0.5 * chunk.receive_stream_descriptor_ptr.sample_rate)
                as f32;

            // Calculate the V coordinate of one texel
            let color_range = [chunk.min, chunk.max];

            // Create quad as two triangles
            let vertices_start = self.vertices.len();
            self.vertices.push(WaterfallVertex {
                position: [x_left, y_start],
                uv: [0., chunk.v_end],
                color_range,
            });
            self.vertices.push(WaterfallVertex {
                position: [x_right, y_start],
                uv: [1., chunk.v_end],
                color_range,
            });
            self.vertices.push(WaterfallVertex {
                position: [x_left, y_end],
                uv: [0., 0.],
                color_range,
            });

            self.vertices.push(WaterfallVertex {
                position: [x_left, y_end],
                uv: [0., 0.],
                color_range,
            });
            self.vertices.push(WaterfallVertex {
                position: [x_right, y_start],
                uv: [1., chunk.v_end],
                color_range,
            });
            self.vertices.push(WaterfallVertex {
                position: [x_right, y_end],
                uv: [1., 0.],
                color_range,
            });

            // Create texture view
            let texture_view = chunk.texture.create_view(&Default::default());
            let prev_texture_view = chunk.prev_texture.create_view(&Default::default());
            let next_texture_view = chunk.next_texture.create_view(&Default::default());

            // Create bind group
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Waterfall Bind Group"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniform_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&prev_texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&next_texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });

            self.draw_calls.push(DrawCall {
                vertices_range: vertices_start as u32..self.vertices.len() as u32,
                bind_group,
                _texture_view: texture_view,
            });
        }

        // Write the vertex buffer
        queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&self.vertices));
    }

    pub fn render(&self, render_pass: &mut wgpu::RenderPass) {
        render_pass.set_pipeline(&self.render_pipeline);
        render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));

        for draw_call in &self.draw_calls {
            render_pass.set_bind_group(0, &draw_call.bind_group, &[]);
            render_pass.draw(draw_call.vertices_range.clone(), 0..1);
        }
    }
}

pub struct DrawCall {
    pub vertices_range: Range<u32>,
    pub bind_group: wgpu::BindGroup,
    pub _texture_view: wgpu::TextureView,
}
