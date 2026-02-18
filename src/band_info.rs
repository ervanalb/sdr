#[derive(Debug, Default)]
pub struct BandsInfo {
    pub bands: Vec<BandInfo>,
    pub allocations: Vec<BandInfo>,
    pub channels: Vec<ChannelsInfo>,
}

#[derive(Debug)]
pub struct BandInfo {
    pub min: f64,
    pub max: f64,
    pub description: String,
}

#[derive(Debug)]
pub struct ChannelsInfo {
    // TODO
}
