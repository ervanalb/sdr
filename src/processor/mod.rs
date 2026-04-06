pub mod fm;
use fm::FmProcessorParameters;

use crate::{document::ClipId, preprocessor::PreprocessedClipDescriptor, ui::Viewport};
use num_complex::Complex;

#[derive(Clone, Debug, PartialEq)]
pub enum ProcessorParameters {
    Fm(FmProcessorParameters),
}

impl ProcessorParameters {
    pub fn create_processor(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> (Box<dyn Processor>, Box<dyn ProcessorHistory>) {
        match self {
            ProcessorParameters::Fm(p) => p.create_processor(device, queue),
        }
    }
}

pub trait Processor: Send {
    fn reset(&mut self);
    fn start_clip(&mut self, clip_id: ClipId, descriptor: &PreprocessedClipDescriptor);
    fn process_chunk(&mut self, clip_id: ClipId, preprocessed_data: &[Complex<f32>]);
    fn end_clip(&mut self, clip_id: ClipId);
}

pub trait ProcessorHistory {
    fn update(&mut self);
    fn expire(&mut self, retain_time: f64);

    /// Draw UI for a given clip onto the canvas
    fn draw_clip(
        &mut self,
        ui: &mut egui::Ui,
        id: egui::Id,
        figure_painter: &egui::Painter,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        dt: f64,
        clip_id: ClipId,
        clip_response: &mut egui::Response,
    );

    /// Draw the sidebar UI for this processor (e.g., inspector panels, controls)
    fn draw(&mut self, ui: &mut egui::Ui, id: egui::Id);

    /// Get a human-readable name for this processor type
    fn name(&self) -> &str;
}
