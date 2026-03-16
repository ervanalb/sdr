use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    preprocessor::PreprocessedStreamDescriptor,
    processor::{Processor, ProcessorHistory, ProcessorParameters},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
// #[serde(rename = "fm")] -- Doesn't work?
pub struct WaterfallProcessorParameters {}

#[typetag::serde]
impl ProcessorParameters for WaterfallProcessorParameters {
    fn create_processor(&self) -> Box<dyn Processor> {
        Box::new(WaterfallProcessor {})
    }

    fn create_history(&self) -> Box<dyn ProcessorHistory> {
        Box::new(WaterfallHistory::new())
    }
}

pub struct WaterfallProcessor {}

impl Processor for WaterfallProcessor {
    fn reset(&mut self) {
        todo!()
    }

    fn start_stream(&mut self, stream_id: usize, descriptor: &PreprocessedStreamDescriptor) {
        todo!()
    }

    fn process_chunk(
        &mut self,
        chunk: &crate::preprocessor::PreprocessedChunk,
    ) -> Option<Box<dyn std::any::Any + Send>> {
        todo!()
    }

    fn end_stream(&mut self, stream_id: usize) {
        todo!()
    }
}

pub struct WaterfallHistory {}

impl WaterfallHistory {
    pub fn new() -> Self {
        WaterfallHistory {}
    }
}

impl ProcessorHistory for WaterfallHistory {
    fn push(&mut self, data: Box<dyn std::any::Any>) {
        todo!()
    }

    fn reset(&mut self) {
        todo!()
    }

    fn expire(&mut self, retain_time: chrono::DateTime<chrono::Utc>) {
        todo!()
    }

    fn draw(
        &self,
        ui: &mut egui::Ui,
        figure_rect: egui::Rect,
        viewport: &crate::ui::Viewport,
        dt: chrono::TimeDelta,
    ) {
        todo!()
    }
}

// OLD WATERFALL STUFF, FOR REFERENCE
/*

pub struct StaticResources {
    target_format: wgpu::TextureFormat,
    instances: HashMap<egui::Id, CanvasResources>,
}

struct CanvasResources {
    viewport_uniform_buffer: wgpu::Buffer,
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
    waterfall_chunks: Vec<WaterfallDrawInfo>,
    reference_time: DateTime<Utc>,
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
                // Create uniform buffer
                let viewport_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Viewport Uniform Buffer"),
                    size: std::mem::size_of::<ViewportUniforms>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });

                // Create waterfall renderer
                let waterfall_renderer = WaterfallRenderer::new(device, target_format);

                CanvasResources {
                    viewport_uniform_buffer,
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
            &resources.viewport_uniform_buffer,
            0,
            bytemuck::cast_slice(&[uniforms]),
        );

        // Prepare waterfall draw calls
        resources.waterfall_renderer.prepare(
            self.waterfall_chunks.clone(),
            device,
            queue,
            &resources.viewport_uniform_buffer,
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
            // Draw waterfall
            resources.waterfall_renderer.render(render_pass);
        }
    }
}

// DRAW WATERFALL
    // Waterfall
    ui.painter()
        .with_clip_rect(figure_rect)
        .add(egui_wgpu::Callback::new_paint_callback(
            figure_rect,
            Callback {
                id,
                viewport_size: figure_size,
                translation: viewport.translation,
                scale: viewport.scale,
                waterfall_chunks: waterfall_gpu.draw_list().collect(),
                reference_time,
            },
        ));


*/
