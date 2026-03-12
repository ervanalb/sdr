// TODO: rename this file, it has nothing to do with GPU

use chrono::{DateTime, Utc};
use std::{collections::BTreeMap, sync::Arc};

use crate::{
    hardware::StreamId,
    processor::{ChannelDescriptor, ChannelId, ChannelResult},
};

pub struct ChannelsGpu {
    pub channels: BTreeMap<(StreamId, ChannelId), ChannelHistory>,
}

impl ChannelsGpu {
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
        let history = self
            .channels
            .entry((stream_id, channel_id))
            .or_insert_with(|| ChannelHistory::new(channel_result.descriptor.clone()));

        for chunk in channel_result.iq.into_iter() {
            history.add_chunk(chunk);
        }
    }

    pub fn prune(&mut self, retain_time: DateTime<Utc>) {
        self.channels.retain(|_, history| {
            history.prune(retain_time);
            !history.chunks.is_empty()
        });
    }
}

pub struct ChannelHistory {
    pub descriptor: Arc<ChannelDescriptor>,
    pub end_time: DateTime<Utc>,
    chunks: Vec<IqChunk>,
}

impl ChannelHistory {
    fn new(descriptor: Arc<ChannelDescriptor>) -> Self {
        let end_time = descriptor.start_time;
        Self {
            descriptor,
            end_time,
            chunks: Vec::new(),
        }
    }

    fn add_chunk(&mut self, chunk: IqChunk) {
        self.end_time = self.end_time.max(chunk.time);
        self.chunks.push(chunk);
    }

    fn prune(&mut self, retain_time: DateTime<Utc>) {
        self.chunks.retain(|chunk| chunk.time >= retain_time);
    }

    pub fn export_iq_data(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        use std::io::Write;
        let mut file = std::fs::File::create(path)?;

        for chunk in &self.chunks {
            dbg!(chunk.data.len());
            for sample in &chunk.data {
                file.write_all(&sample.re.to_le_bytes())?;
                file.write_all(&sample.im.to_le_bytes())?;
            }
        }

        file.flush()?;

        Ok(())
    }
}
