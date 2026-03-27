pub mod fm;
pub mod waterfall;
use fm::FmProcessorParameters;
use waterfall::WaterfallProcessorParameters;

use crate::{document::ClipId, preprocessor::PreprocessedClipDescriptor, ui::Viewport};
use num_complex::Complex;

#[derive(Clone, Debug, PartialEq)]
pub enum ProcessorParameters {
    Waterfall(WaterfallProcessorParameters),
    Fm(FmProcessorParameters),
}

impl ProcessorParameters {
    pub fn create_processor(
        &self,
        cc: &CreationContext<'_>,
    ) -> (Box<dyn Processor>, Box<dyn ProcessorHistory>) {
        match self {
            ProcessorParameters::Waterfall(p) => p.create_processor(cc),
            ProcessorParameters::Fm(p) => p.create_processor(cc),
        }
    }
}

pub trait Processor: Send {
    fn reset(&mut self);
    fn start_clip(&mut self, clip_id: ClipId, descriptor: &PreprocessedClipDescriptor);
    fn process_chunk(
        &mut self,
        clip_id: ClipId,
        preprocessed_data: &[Complex<f32>],
    );
    fn end_clip(&mut self, clip_id: ClipId);
}

pub trait ProcessorHistory {
    fn update(&mut self);
    fn expire(&mut self, retain_time: f64);

    /// Draw this processor history onto the canvas
    fn draw(
        &self,
        ui: &mut egui::Ui,
        id: egui::Id,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        dt: f64,
    );
}

pub struct CreationContext<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
}
