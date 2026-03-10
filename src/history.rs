use crate::modulation::{ModulationHistory, ModulationUiFn};
use crate::processor::ChannelDescriptor;
use crate::{
    hardware::StreamId,
    processor::{ChannelId, ChannelResult},
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

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
                modulation: channel_result
                    .descriptor
                    .modulation
                    .create_history(channel_result.descriptor.start_time),
            });

        for demodulation in channel_result.demodulation.into_iter() {
            channel_history.modulation.add(demodulation);
        }
    }

    pub fn prune(&mut self, retain_time: Instant) {
        self.channels
            .retain(|_, history| history.modulation.prune(retain_time));
    }

    pub fn draw_list(&self) -> impl Iterator<Item = ChannelDrawInfo<'_>> + '_ {
        self.channels
            .iter()
            .flat_map(|(&(stream_id, channel_id), history)| {
                let freq_min = (history.descriptor.center_frequency
                    - 0.5 * history.descriptor.bandwidth) as f32;
                let freq_max = (history.descriptor.center_frequency
                    + 0.5 * history.descriptor.bandwidth) as f32;
                history
                    .modulation
                    .draw_list(stream_id, channel_id, &history.descriptor)
                    .map(move |(start_time, end_time, ui)| ChannelDrawInfo {
                        freq_min,
                        freq_max,
                        start_time,
                        end_time,
                        name: history.descriptor.name.clone(),
                        ui,
                    })
            })
    }
}

pub struct ChannelHistory {
    pub descriptor: Arc<ChannelDescriptor>,
    pub modulation: Box<dyn ModulationHistory>,
}

pub struct ChannelDrawInfo<'a> {
    pub freq_min: f32,
    pub freq_max: f32,
    pub start_time: Instant,
    pub end_time: Instant,
    pub name: String,
    pub ui: ModulationUiFn<'a>,
}
