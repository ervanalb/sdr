//pub mod fm;
pub mod waterfall;

use crate::{
    preprocessor::{PreprocessedChunk, PreprocessedStreamDescriptor},
    ui::Viewport,
};
use chrono::{DateTime, TimeDelta, Utc};
use dyn_clone::{DynClone, clone_trait_object};
use std::any::Any;

clone_trait_object!(ProcessorParameters);

#[typetag::serde(tag = "type")]
pub trait ProcessorParameters: std::fmt::Debug + Send + Sync + DynClone {
    fn create_history(&self) -> Box<dyn ProcessorHistory>;
    fn create_processor(&self) -> Box<dyn Processor>;
}

pub trait Processor: Send {
    fn reset(&mut self);
    fn start_stream(&mut self, stream_id: usize, descriptor: &PreprocessedStreamDescriptor);
    fn process_chunk(&mut self, chunk: &PreprocessedChunk) -> Option<Box<dyn Any + Send>>;
    fn end_stream(&mut self, stream_id: usize);
}

pub trait ProcessorHistory {
    fn push(&mut self, data: Box<dyn Any>);
    fn reset(&mut self);
    fn expire(&mut self, retain_time: DateTime<Utc>);

    /// Draw this processor history onto the canvas
    fn draw(&self, ui: &mut egui::Ui, figure_rect: egui::Rect, viewport: &Viewport, dt: TimeDelta);
}
