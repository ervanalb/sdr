use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BandsInfo {
    pub bands: Vec<BandInfo>,
    pub allocations: Vec<BandInfo>,
    pub channels: Vec<ChannelsInfo>,
    pub highest_freq: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BandInfo {
    pub min: f64,
    pub max: f64,
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelsInfo {
    // TODO
}
