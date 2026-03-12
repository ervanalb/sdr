pub mod fm;

use crate::{
    hardware::StreamId,
    processor::{ChannelDescriptor, ChannelId},
    ui::{StreamTransmission, Viewport},
};
use chrono::{DateTime, Utc};
use dyn_clone::{DynClone, clone_trait_object};
use egui::Response;
use num_complex::Complex;
use std::{any::Any, sync::Arc};

pub type ModulationUiFn<'a> = Box<dyn FnOnce(&Response) + 'a>;

#[typetag::serde(tag = "type")]
pub trait ModulationParameters: std::fmt::Debug + Send + Sync + DynClone {
    fn create_demodulator(&self, ifft_size: usize) -> Box<dyn Demodulator>;
    fn create_history(&self) -> Box<dyn ModulationHistory>;
}

clone_trait_object!(ModulationParameters);

pub trait Demodulator: Any + Send + 'static {
    fn process(
        &mut self,
        time: DateTime<Utc>,
        fft_data: Vec<Complex<f32>>,
        noise_floor: f32,
    ) -> Option<Box<dyn Any + Send>>;
}

pub trait ModulationHistory: Any + Send + 'static {
    fn add(&mut self, demodulation: Box<dyn Any + Send>);

    /// Remove history entries older than retain_time and return true if any remain
    fn prune_old_data(&mut self, retain_time: DateTime<Utc>) -> bool;

    /// Remove history entries older than retain_time and return true if any remain
    fn draw(
        &self,
        stream_id: StreamId,
        channel_id: ChannelId,
        descriptor: &Arc<ChannelDescriptor>,
        ui: &mut egui::Ui,
        figure_rect: egui::Rect,
        viewport: &Viewport,
    );
}
