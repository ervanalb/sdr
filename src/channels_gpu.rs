// TODO: rename this file, it has nothing to do with GPU

use crate::hardware::{ChannelMessage, ReceiveChannelDescriptorPtr};
use num_complex::Complex;
use std::collections::HashMap;
use std::time::Instant;

pub struct ChannelsGpu {
    channels: HashMap<ReceiveChannelDescriptorPtr, ChannelHistory>,
}

impl ChannelsGpu {
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    pub fn add_chunk(&mut self, msg: ChannelMessage) {
        let key = msg.receive_channel_descriptor_ptr.clone();

        let history = self
            .channels
            .entry(key)
            .or_insert_with(|| ChannelHistory::new());

        history.add_chunk(msg.iq_data, msg.start_time, msg.end_time);
    }

    pub fn prune(&mut self, retain_time: Instant) {
        self.channels.retain(|_, history| {
            history.prune(retain_time);
            !history.chunks.is_empty()
        });
    }

    pub fn draw_list(&self) -> impl Iterator<Item = ChannelDrawInfo> + '_ {
        self.channels
            .iter()
            .filter_map(|(descriptor_ptr, history)| {
                if history.chunks.is_empty() {
                    return None;
                }

                let start_time = history.chunks.first().unwrap().start_time;
                let end_time = history.chunks.last().unwrap().end_time;

                Some(ChannelDrawInfo {
                    receive_channel_descriptor_ptr: descriptor_ptr.clone(),
                    start_time,
                    end_time,
                })
            })
    }

    pub fn export_iq_data(
        &self,
        descriptor_ptr: &ReceiveChannelDescriptorPtr,
        path: &std::path::Path,
    ) -> Result<(), std::io::Error> {
        use std::io::Write;

        if let Some(history) = self.channels.get(descriptor_ptr) {
            let mut file = std::fs::File::create(path)?;

            for chunk in &history.chunks {
                for sample in &chunk.iq_data {
                    file.write_all(&sample.re.to_le_bytes())?;
                    file.write_all(&sample.im.to_le_bytes())?;
                }
            }

            file.flush()?;
        }

        Ok(())
    }
}

pub struct ChannelHistory {
    chunks: Vec<ChannelChunk>,
}

impl ChannelHistory {
    fn new() -> Self {
        Self {
            chunks: Vec::new(),
        }
    }

    fn add_chunk(&mut self, iq_data: Vec<Complex<f32>>, start_time: Instant, end_time: Instant) {
        self.chunks.push(ChannelChunk {
            iq_data,
            start_time,
            end_time,
        });
    }

    fn prune(&mut self, retain_time: Instant) {
        self.chunks.retain(|chunk| chunk.end_time >= retain_time);
    }
}

struct ChannelChunk {
    iq_data: Vec<Complex<f32>>,
    start_time: Instant,
    end_time: Instant,
}

#[derive(Debug, Clone)]
pub struct ChannelDrawInfo {
    pub receive_channel_descriptor_ptr: ReceiveChannelDescriptorPtr,
    pub start_time: Instant,
    pub end_time: Instant,
}
