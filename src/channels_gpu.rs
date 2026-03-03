// TODO: rename this file, it has nothing to do with GPU

use num_complex::Complex;
use std::collections::BTreeMap;
use std::time::Instant;

use crate::waterfall::{Channel, ChannelDescriptor, ChannelId};

pub struct ChannelsGpu {
    pub channels: BTreeMap<ChannelId, ChannelHistory>,
}

impl ChannelsGpu {
    pub fn new() -> Self {
        Self {
            channels: BTreeMap::new(),
        }
    }

    pub fn add_chunk(&mut self, channel_id: ChannelId, channel: &Channel, end_time: Instant) {
        let history = self
            .channels
            .entry(channel_id)
            .or_insert_with(|| ChannelHistory::new(channel.descriptor.clone()));

        history.add_chunk(&channel.iq_data, end_time);
    }

    pub fn close_channel(&mut self, channel: ChannelId) {
        // XXX TODO
    }

    pub fn prune(&mut self, retain_time: Instant) {
        self.channels.retain(|_, history| {
            history.prune(retain_time);
            !history.chunks.is_empty()
        });
    }
}

pub struct ChannelHistory {
    pub descriptor: ChannelDescriptor,
    pub end_time: Instant,
    chunks: Vec<ChannelChunk>,
}

impl ChannelHistory {
    fn new(descriptor: ChannelDescriptor) -> Self {
        let end_time = descriptor.start_time;
        Self {
            descriptor,
            end_time,
            chunks: Vec::new(),
        }
    }

    fn add_chunk(&mut self, iq_data: &[Complex<f32>], end_time: Instant) {
        self.chunks.push(ChannelChunk {
            iq_data: iq_data.to_vec(),
            end_time,
        });
        self.end_time = self.end_time.max(end_time);
    }

    fn prune(&mut self, retain_time: Instant) {
        self.chunks.retain(|chunk| chunk.end_time >= retain_time);
    }

    pub fn export_iq_data(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        use std::io::Write;
        let mut file = std::fs::File::create(path)?;

        for chunk in &self.chunks {
            dbg!(chunk.iq_data.len());
            for sample in &chunk.iq_data {
                file.write_all(&sample.re.to_le_bytes())?;
                file.write_all(&sample.im.to_le_bytes())?;
            }
        }

        file.flush()?;

        Ok(())
    }
}

struct ChannelChunk {
    iq_data: Vec<Complex<f32>>,
    end_time: Instant,
}
