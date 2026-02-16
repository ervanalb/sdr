use crate::hardware::WaterfallMessage;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use wgpu::{
    Device, Extent3d, Origin3d, Queue, TexelCopyTextureInfo, Texture, TextureAspect,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
};

const TEXTURE_HEIGHT: u32 = 1024;
const MAX_DURATION: Duration = Duration::from_secs(60);

type DeviceId = String;
type ChannelIndex = usize;
type TextureKey = (DeviceId, ChannelIndex);

pub struct WaterfallGpu {
    texture_groups: HashMap<TextureKey, TextureGroup>,
    blank_texture: Texture,
}

impl WaterfallGpu {
    pub fn new(device: &Device) -> Self {
        let blank_texture = device.create_texture(&TextureDescriptor {
            label: Some("Blank Waterfall Texture"),
            size: Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::R32Float,
            usage: TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        Self {
            texture_groups: HashMap::new(),
            blank_texture,
        }
    }

    pub fn add_row(&mut self, msg: &WaterfallMessage, device: &Device, queue: &Queue) {
        let key = (msg.device_id.clone(), msg.channel_index);

        let group = self
            .texture_groups
            .entry(key)
            .or_insert_with(|| TextureGroup {
                active_texture: ActiveTexture::new(device, msg, self.blank_texture.clone()),
                finished_textures: vec![],
            });
        group.add_row(device, queue, self.blank_texture.clone(), msg);
    }

    pub fn draw_list(&mut self) -> impl Iterator<Item = ChunkDrawInfo> {
        let now = Instant::now();

        // Prune old textures across all groups
        self.texture_groups
            .retain(|_, group| group.prune_old_textures(now));

        // Collect all chunks into a draw list
        self.texture_groups
            .iter()
            .flat_map(|((device_id, channel_index), group)| {
                group
                    .finished_textures
                    .iter()
                    .map(|texture_info| ChunkDrawInfo {
                        device_id: device_id.clone(),
                        channel_index: *channel_index,
                        texture: texture_info.texture.clone(),
                        prev_texture: texture_info.prev_texture.clone(),
                        next_texture: texture_info.next_texture.clone(),
                        v_end: 1.,
                        period: texture_info.period,
                        center_frequency: texture_info.center_frequency,
                        width: texture_info.width,
                        start_time: texture_info.start_time,
                        end_time: texture_info.end_time,
                    })
                    .chain(Some(ChunkDrawInfo {
                        device_id: device_id.clone(),
                        channel_index: *channel_index,
                        texture: group.active_texture.texture.clone(),
                        prev_texture: group.active_texture.prev_texture.clone(),
                        next_texture: self.blank_texture.clone(),
                        v_end: group.active_texture.current_row as f32 / TEXTURE_HEIGHT as f32,
                        period: group.active_texture.period,
                        center_frequency: group.active_texture.center_frequency,
                        width: group.active_texture.width,
                        start_time: group.active_texture.start_time,
                        end_time: group.active_texture.end_time,
                    }))
            })
    }
}

#[derive(Debug)]
struct ActiveTexture {
    prev_texture: Texture,
    texture: Texture,
    current_row: usize,
    period: f64,
    center_frequency: f64,
    width: f64,
    start_time: Instant,
    end_time: Instant,
}

impl ActiveTexture {
    pub fn new(device: &Device, msg: &WaterfallMessage, prev_texture: Texture) -> Self {
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("Waterfall Texture"),
            size: Extent3d {
                width: msg.waterfall_row.len() as u32,
                height: TEXTURE_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::R32Float,
            usage: TextureUsages::TEXTURE_BINDING
                | TextureUsages::COPY_DST
                | TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        Self {
            prev_texture,
            texture,
            current_row: 0,
            period: msg.period,
            center_frequency: msg.center_frequency,
            width: msg.width,
            start_time: msg.start_time,
            end_time: msg.start_time, // since we aren't yet loading in this message
        }
    }
}

#[derive(Debug)]
struct TextureGroup {
    active_texture: ActiveTexture,
    finished_textures: Vec<TextureInfo>,
}

impl TextureGroup {
    fn add_row(
        &mut self,
        device: &Device,
        queue: &Queue,
        blank_texture: Texture,
        msg: &WaterfallMessage,
    ) {
        if self.active_texture.period != msg.period
            || self.active_texture.center_frequency != msg.center_frequency
            || self.active_texture.width != msg.width
            || self.active_texture.end_time != msg.start_time
            || self.active_texture.texture.width() as usize != msg.waterfall_row.len()
        {
            self.swap_active_texture(device, queue, blank_texture, msg, false);
        } else if self.active_texture.current_row >= TEXTURE_HEIGHT as usize {
            self.swap_active_texture(device, queue, blank_texture, msg, true);
        }

        // Upload the row data to the GPU
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.active_texture.texture,
                mip_level: 0,
                origin: Origin3d {
                    x: 0,
                    y: self.active_texture.current_row as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&msg.waterfall_row),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(msg.waterfall_row.len() as u32 * 4), // 4 bytes per f32
                rows_per_image: Some(1),
            },
            Extent3d {
                width: msg.waterfall_row.len() as u32,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        // Update info
        self.active_texture.end_time = msg.end_time;
        self.active_texture.current_row += 1;
    }

    // Returns true if there are still chunks
    fn prune_old_textures(&mut self, now: Instant) -> bool {
        let mut any_remain = false;
        self.finished_textures.retain(|texture| {
            if now.duration_since(texture.end_time) < MAX_DURATION {
                any_remain = true;
                true
            } else {
                false
            }
        });
        any_remain || now.duration_since(self.active_texture.end_time) < MAX_DURATION
    }

    fn swap_active_texture(
        &mut self,
        device: &Device,
        queue: &Queue,
        blank_texture: Texture,
        msg: &WaterfallMessage,
        chain: bool,
    ) {
        if self.active_texture.current_row < TEXTURE_HEIGHT as usize {
            // If partial, copy this texture into an appropriately sized one
            // to free up space

            let smaller_size = Extent3d {
                width: self.active_texture.texture.width(),
                height: self.active_texture.current_row as u32,
                depth_or_array_layers: 1,
            };
            let smaller_texture = device.create_texture(&TextureDescriptor {
                label: Some("Waterfall Texture"),
                size: smaller_size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: TextureFormat::R32Float,
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                view_formats: &[],
            });

            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Texture copy command encoder"),
            });

            encoder.copy_texture_to_texture(
                TexelCopyTextureInfo {
                    texture: &self.active_texture.texture,
                    mip_level: 0,
                    origin: Origin3d::ZERO,
                    aspect: TextureAspect::All,
                },
                TexelCopyTextureInfo {
                    texture: &smaller_texture,
                    mip_level: 0,
                    origin: Origin3d::ZERO,
                    aspect: TextureAspect::All,
                },
                smaller_size,
            );

            queue.submit(Some(encoder.finish()));

            self.active_texture.texture = smaller_texture;
        }

        // Create a new active texture
        let prev_texture = if chain {
            self.active_texture.texture.clone()
        } else {
            blank_texture.clone()
        };
        let old_active_texture = std::mem::replace(
            &mut self.active_texture,
            ActiveTexture::new(device, msg, prev_texture),
        );
        let next_texture = if chain {
            self.active_texture.texture.clone()
        } else {
            blank_texture.clone()
        };

        self.finished_textures.push(TextureInfo {
            texture: old_active_texture.texture,
            prev_texture: old_active_texture.prev_texture,
            next_texture,
            period: old_active_texture.period,
            center_frequency: old_active_texture.center_frequency,
            width: old_active_texture.width,
            start_time: old_active_texture.start_time,
            end_time: old_active_texture.end_time,
        });
    }
}

#[derive(Debug)]
struct TextureInfo {
    texture: Texture,
    prev_texture: Texture,
    next_texture: Texture,
    period: f64,
    center_frequency: f64,
    width: f64,
    start_time: Instant,
    end_time: Instant,
}

#[derive(Debug, Clone)]
pub struct ChunkDrawInfo {
    pub device_id: String,
    pub channel_index: usize,
    pub texture: Texture,
    pub prev_texture: Texture,
    pub next_texture: Texture,
    pub v_end: f32, // for active (partially filled) texture, the highest valid V component of UV coordinate
    pub period: f64,
    pub center_frequency: f64,
    pub width: f64,
    pub start_time: Instant,
    pub end_time: Instant,
}
