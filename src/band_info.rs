use serde::{Deserialize, Serialize};

use crate::format::format_freq;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BandsInfo {
    pub bands: Vec<BandInfo>,
    pub allocations: Vec<BandInfo>,
    pub channels: Vec<ChannelGroupInfo>,
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

//#[derive(Debug, Serialize, Deserialize)]
//pub struct ChannelProbeParams {
//    pub bandwidth: f64,
//    pub squelch_time_constant: f64,
//    pub squelch_threshold_db: f64,
//    pub squelch_hysteresis_db: f64,
//}

//#[derive(Debug, Serialize, Deserialize)]
//pub struct ChannelConvertParams {
//}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelGroupInfo {
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub naming: NamingConvention,
    //pub probe: ChannelProbeParams,
    //pub convert: ChannelConvertParams,
    pub bandwidth: f64,
}

impl ChannelGroupInfo {
    pub fn iter(&self) -> ChannelInfoIter {
        let num_channels = if self.step == 0.0 {
            1
        } else {
            ((self.max - self.min) / self.step).round().max(0.) as usize + 1
        };

        ChannelInfoIter {
            channel_group_info: self,
            current_index: 0,
            num_channels,
        }
    }
}

pub struct ChannelInfoIter<'a> {
    channel_group_info: &'a ChannelGroupInfo,
    current_index: usize,
    num_channels: usize,
}

impl<'a> Iterator for ChannelInfoIter<'a> {
    type Item = ChannelInfo;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_index >= self.num_channels {
            return None;
        }

        let center_frequency = self.channel_group_info.min
            + (self.current_index as f64 * self.channel_group_info.step);

        let name = match self.channel_group_info.naming {
            NamingConvention::Number => {
                format!(
                    "{} {}",
                    self.channel_group_info.name,
                    self.current_index + 1
                )
            }
            NamingConvention::Frequency(precision) => {
                format!(
                    "{} {}",
                    self.channel_group_info.name,
                    format_freq(center_frequency, precision)
                )
            }
        };

        self.current_index += 1;

        Some(ChannelInfo {
            center_frequency,
            name,
        })
    }
}
