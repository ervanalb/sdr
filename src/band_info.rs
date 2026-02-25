use serde::{Deserialize, Serialize};

use crate::format::format_freq;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BandsInfo {
    pub bands: Vec<BandInfo>,
    pub allocations: Vec<BandInfo>,
    pub channels: Vec<ChannelsInfo>,
    pub highest_freq: f64,
}

#[derive(Debug)]
pub struct ChannelInfo {
    pub center_frequency: f64,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BandInfo {
    pub min: f64,
    pub max: f64,
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum NamingConvention {
    Number,
    Frequency(i32),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelProbeParams {
    pub bandwidth: f64,
    pub squelch_time_constant: f64,
    pub squelch_threshold_db: f64,
    pub squelch_hysteresis_db: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelConvertParams {
    pub bandwidth: f64,
    pub target_sample_rate: f64,
    pub target_chunk_period: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelsInfo {
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub naming: NamingConvention,
    pub probe: ChannelProbeParams,
    pub convert: ChannelConvertParams,
}

impl ChannelsInfo {
    pub fn iter(&self) -> ChannelsInfoIter {
        let num_channels = if self.step == 0.0 {
            1
        } else {
            ((self.max - self.min) / self.step).round().max(0.) as usize + 1
        };

        ChannelsInfoIter {
            channels_info: self,
            current_index: 0,
            num_channels,
        }
    }
}

pub struct ChannelsInfoIter<'a> {
    channels_info: &'a ChannelsInfo,
    current_index: usize,
    num_channels: usize,
}

impl<'a> Iterator for ChannelsInfoIter<'a> {
    type Item = ChannelInfo;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_index >= self.num_channels {
            return None;
        }

        let center_frequency =
            self.channels_info.min + (self.current_index as f64 * self.channels_info.step);

        let name = match self.channels_info.naming {
            NamingConvention::Number => {
                format!("{} {}", self.channels_info.name, self.current_index + 1)
            }
            NamingConvention::Frequency(precision) => {
                format!("{} {}", self.channels_info.name, format_freq(center_frequency, precision))
            }
        };

        self.current_index += 1;

        Some(ChannelInfo {
            center_frequency,
            name,
        })
    }
}
