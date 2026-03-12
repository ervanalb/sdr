use crate::modulation::{ModulationHistory, ModulationUiFn};
use crate::processor::ChannelDescriptor;
use crate::ui::Viewport;
use crate::{
    hardware::StreamId,
    processor::{ChannelId, ChannelResult},
};
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct History {
    pub channels: BTreeMap<(StreamId, ChannelId), ChannelHistory>,
}

impl History {
    pub fn new() -> Self {
        Self {
            channels: BTreeMap::new(),
        }
    }

    pub fn add_chunks(
        &mut self,
        stream_id: StreamId,
        channel_id: ChannelId,
        channel_result: ChannelResult,
    ) {
        let channel_history = self
            .channels
            .entry((stream_id, channel_id))
            .or_insert_with(|| ChannelHistory {
                descriptor: channel_result.descriptor.clone(),
                modulation: channel_result.descriptor.modulation.create_history(),
            });

        for demodulation in channel_result.demodulation.into_iter() {
            channel_history.modulation.add(demodulation);
        }
    }

    pub fn prune(&mut self, retain_time: DateTime<Utc>) {
        self.channels
            .retain(|_, history| history.modulation.prune_old_data(retain_time));
    }

    pub fn draw(
        &self,
        ui: &mut egui::Ui,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        dt: TimeDelta,
    ) {
        for (&(stream_id, channel_id), history) in self.channels.iter() {
            //let freq_min =
            //    (history.descriptor.center_frequency - 0.5 * history.descriptor.bandwidth) as f32;
            //let freq_max =
            //    (history.descriptor.center_frequency + 0.5 * history.descriptor.bandwidth) as f32;
            history.modulation.draw(
                stream_id,
                channel_id,
                &history.descriptor,
                ui,
                figure_rect,
                viewport,
                dt,
            );
        }
    }
}

pub struct ChannelHistory {
    pub descriptor: Arc<ChannelDescriptor>,
    pub modulation: Box<dyn ModulationHistory>,
}

pub struct ChannelDrawInfo<'a> {
    pub freq_min: f32,
    pub freq_max: f32,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub name: String,
    pub ui: ModulationUiFn<'a>,
}
