use crate::hardware::WaterfallMessage;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use wgpu::{
    Device, Extent3d, Origin3d, Queue, Texture, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages,
};

const TEXTURE_HEIGHT: u32 = 1024;
const MAX_DURATION: Duration = Duration::from_secs(60);

type DeviceId = String;
type ChannelIndex = usize;
type RowLength = usize;
type TextureKey = (DeviceId, ChannelIndex, RowLength);

pub struct WaterfallGpu {
    texture_groups: HashMap<TextureKey, TextureGroup>,
}

impl WaterfallGpu {
    pub fn new() -> Self {
        Self {
            texture_groups: HashMap::new(),
        }
    }

    pub fn add_row(&mut self, msg: &WaterfallMessage, device: &Device, queue: &Queue) {
        let key = (
            msg.device_id.clone(),
            msg.channel_index,
            msg.waterfall_row.len(),
        );

        let group = self.texture_groups.entry(key).or_default();
        group.add_row(msg, device, queue);
    }

    pub fn draw_list(&mut self) -> impl Iterator<Item = ChunkDrawInfo> {
        let now = Instant::now();

        // Prune old textures across all groups
        self.texture_groups
            .retain(|_, group| group.prune_old_textures(now));

        // Collect all chunks into a draw list
        self.texture_groups
            .iter()
            .flat_map(|((device_id, channel_index, _row_length), group)| {
                group.textures.iter().flat_map(|texture_info| {
                    texture_info.chunks.iter().map(|chunk| ChunkDrawInfo {
                        device_id: device_id.clone(),
                        channel_index: *channel_index,
                        texture: texture_info.texture.clone(),
                        start_row: chunk.start_row,
                        end_row: chunk.end_row,
                        period: chunk.period,
                        center_frequency: chunk.center_frequency,
                        width: chunk.width,
                        start_time: chunk.start_time,
                        end_time: chunk.end_time,
                    })
                })
            })
    }
}

#[derive(Debug, Default)]
struct TextureGroup {
    textures: Vec<TextureInfo>,
}

impl TextureGroup {
    fn add_row(&mut self, msg: &WaterfallMessage, device: &Device, queue: &Queue) {
        // Check if the last texture has space
        let texture = if let Some(last_texture) = self.textures.last_mut()
            && last_texture.current_row < TEXTURE_HEIGHT as usize
        {
            last_texture
        } else {
            // Create a new texture
            let new_texture = TextureInfo::new(device, msg.waterfall_row.len());
            self.textures.push(new_texture);
            self.textures.last_mut().unwrap()
        };
        texture.append_row(msg, queue);
    }

    // Returns true if there are still chunks
    fn prune_old_textures(&mut self, now: Instant) -> bool {
        let mut any_remain = false;
        self.textures.retain(|texture| {
            if let Some(last_chunk) = texture.chunks.last()
                && now.duration_since(last_chunk.end_time) < MAX_DURATION
            {
                any_remain = true;
                true
            } else {
                // Empty texture with no chunks, can be removed
                false
            }
        });
        any_remain
    }
}

#[derive(Debug)]
struct TextureInfo {
    texture: Texture,
    row_length: usize,
    current_row: usize,
    chunks: Vec<Chunk>,
}

impl TextureInfo {
    fn new(device: &Device, row_length: usize) -> Self {
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("Waterfall Texture"),
            size: Extent3d {
                width: row_length as u32,
                height: TEXTURE_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::R32Float,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });

        Self {
            texture,
            row_length,
            current_row: 0,
            chunks: Vec::new(),
        }
    }

    fn append_row(&mut self, msg: &WaterfallMessage, queue: &Queue) {
        // Upload the row data to the GPU
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: Origin3d {
                    x: 0,
                    y: self.current_row as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&msg.waterfall_row),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.row_length as u32 * 4), // 4 bytes per f32
                rows_per_image: Some(1),
            },
            Extent3d {
                width: self.row_length as u32,
                height: 1,
                depth_or_array_layers: 1,
            },
        );

        // Update or create chunk
        if let Some(last_chunk) = self.chunks.last_mut()
            && last_chunk.period == msg.period
            && last_chunk.center_frequency == msg.center_frequency
            && last_chunk.width == msg.width
        {
            // Extend the last chunk
            last_chunk.end_row = self.current_row + 1;
            last_chunk.end_time = msg.end_time;
        } else {
            // Create a new chunk
            self.chunks.push(Chunk {
                start_row: self.current_row,
                end_row: self.current_row + 1,
                period: msg.period,
                center_frequency: msg.center_frequency,
                width: msg.width,
                start_time: msg.start_time,
                end_time: msg.end_time,
            });
        };

        self.current_row += 1;
    }
}

#[derive(Debug)]
struct Chunk {
    start_row: usize,
    end_row: usize,
    period: f64,
    center_frequency: f64,
    width: f64,
    start_time: Instant,
    end_time: Instant,
}

#[derive(Debug)]
pub struct ChunkDrawInfo {
    pub device_id: String,
    pub channel_index: usize,
    pub texture: Texture,
    pub start_row: usize,
    pub end_row: usize,
    pub period: f64,
    pub center_frequency: f64,
    pub width: f64,
    pub start_time: Instant,
    pub end_time: Instant,
}
