pub mod fm;
pub mod waterfall;

use crate::{hardware::StreamId, preprocessor::PreprocessedStreamDescriptor, ui::Viewport};
use chrono::{DateTime, TimeDelta, Utc};
use dyn_clone::{DynClone, clone_trait_object};
use num_complex::Complex;

clone_trait_object!(ProcessorParameters);

#[typetag::serde(tag = "type")]
pub trait ProcessorParameters: std::fmt::Debug + Send + Sync + DynClone {
    fn create_processor(
        &self,
        cc: &CreationContext<'_>,
    ) -> (Box<dyn Processor>, Box<dyn ProcessorHistory>);
}

pub trait Processor: Send {
    fn reset(&mut self);
    fn start_stream(&mut self, stream_id: StreamId, descriptor: &PreprocessedStreamDescriptor);
    fn process_chunk(
        &mut self,
        stream_id: StreamId,
        time: DateTime<Utc>,
        preprocessed_data: &[Complex<f32>],
    );
    fn end_stream(&mut self, stream_id: StreamId);
}

pub trait ProcessorHistory {
    fn update(&mut self);
    fn expire(&mut self, retain_time: DateTime<Utc>);

    /// Draw this processor history onto the canvas
    fn draw(
        &self,
        ui: &mut egui::Ui,
        id: egui::Id,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        dt: TimeDelta,
    );
}

pub struct CreationContext<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
}
