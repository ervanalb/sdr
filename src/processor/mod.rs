pub mod fm;
use fm::FmProcessorParameters;

use crate::{document::ClipId, preprocessor::PreprocessedClipDescriptor, ui::Viewport};
use num_complex::Complex;
use serde::{Deserialize, Serialize};
use std::any::Any;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SpecificProcessorParameters {
    Fm(FmProcessorParameters),
}

impl SpecificProcessorParameters {
    pub fn create_instance(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> (Box<dyn Processor>, Box<dyn ProcessorHistory>) {
        match self {
            SpecificProcessorParameters::Fm(p) => p.create_processor(device, queue),
        }
    }

    pub fn draw_setup(&mut self, ui: &mut egui::Ui) {
        match self {
            SpecificProcessorParameters::Fm(p) => p.draw_setup(ui),
        }
    }

    /// Get a list of available processor types
    pub fn available_types() -> Vec<(&'static str, fn() -> Self)> {
        vec![("FM Demodulator", || {
            SpecificProcessorParameters::Fm(FmProcessorParameters::default())
        })]
    }

    /// Get the display name for this processor type
    pub fn type_name(&self) -> &'static str {
        match self {
            SpecificProcessorParameters::Fm(_) => "FM Demodulator",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessorParameters {
    pub name: String,
    pub enabled: bool,
    pub specific_parameters: SpecificProcessorParameters,
}

pub trait Processor: Send {
    fn reset(&mut self);
    fn start_clip(&mut self, clip_id: ClipId, descriptor: &PreprocessedClipDescriptor);
    fn process_chunk(&mut self, clip_id: ClipId, index: isize, preprocessed_data: &[Complex<f32>]);
    fn end_clip(&mut self, clip_id: ClipId);
}

pub trait ProcessorHistory {
    fn update(&mut self);
    fn expire(&mut self, retain_time: f64);

    /// Create a new UI instance for this processor type
    fn new_ui(&self) -> Box<dyn ProcessorUi>;

    /// Get a human-readable name for this processor type
    fn name(&self) -> &str;

    /// Check if this processor has any data (e.g., transmissions, recordings, etc.)
    fn has_data(&self) -> bool;

    /// Downcast this trait object to Any for type-specific operations
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

pub trait ProcessorUi {
    /// Draw UI for a given clip onto the canvas
    fn draw_clip(
        &mut self,
        history: &mut Box<dyn ProcessorHistory>,
        ui: &mut egui::Ui,
        figure_painter: &egui::Painter,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        dt: f64,
        clip_id: ClipId,
        clip_response: &mut egui::Response,
    );

    /// Draw the sidebar UI for this processor (e.g., inspector panels, controls)
    fn draw(
        &mut self,
        history: &mut Box<dyn ProcessorHistory>,
        ui: &mut egui::Ui,
        id: egui::Id,
        dt: f64,
    );
}
