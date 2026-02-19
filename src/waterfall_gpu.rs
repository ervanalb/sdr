use crate::hardware::WaterfallMessage;
use std::collections::HashMap;
use std::time::Instant;
use wgpu::{
    Device, Extent3d, Origin3d, Queue, TexelCopyTextureInfo, Texture, TextureAspect,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
};

const TEXTURE_HEIGHT: u32 = 1024;

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
                min: msg.min,
                max: msg.max,
            });
        group.add_row(device, queue, self.blank_texture.clone(), msg);
    }

    pub fn prune_old_textures(&mut self, time: Instant) {
        self.texture_groups
            .retain(|_, group| group.prune_old_textures(time));
    }

    pub fn draw_list(&self) -> impl Iterator<Item = ChunkDrawInfo> {
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
                        min: group.min as f32,
                        max: group.max as f32,
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
                        min: group.min as f32,
                        max: group.max as f32,
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
    mip_level_count: u32,
    mip_buffer: Vec<f32>,
}

impl ActiveTexture {
    pub fn new(device: &Device, msg: &WaterfallMessage, prev_texture: Texture) -> Self {
        let mip_level_count = TEXTURE_HEIGHT.ilog2().max(1);
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("Waterfall Texture"),
            size: Extent3d {
                width: msg.waterfall_row.len() as u32,
                height: TEXTURE_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count,
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
            mip_level_count,
            // Allocate some extra space in the mip_buffer
            // in case waterfall_row.len() is very small
            mip_buffer: vec![0.; 2 * msg.waterfall_row.len() + mip_level_count as usize],
        }
    }

    fn add_row(&mut self, queue: &Queue, row: &[f32]) {
        let mut row_index = self.current_row as u32;
        let mut row_len = self.texture.width();
        let mut buffer_offset = 0;
        self.mip_buffer[0..row_len as usize].clone_from_slice(row);
        for mip_level in 0..self.mip_level_count {
            let (row, rest) = self.mip_buffer[buffer_offset..].split_at_mut(row_len as usize);

            // Upload the row data to the GPU
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level,
                    origin: Origin3d {
                        x: 0,
                        y: row_index,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&row),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_len * 4), // 4 bytes per f32
                    rows_per_image: Some(1),
                },
                Extent3d {
                    width: row_len,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );

            // Accumulate into the next row
            let next_row_len = (row_len / 2).max(1);
            let next_row = &mut rest[..next_row_len as usize];
            if row_len == 1 {
                next_row[0] = 0.5 * row[0];
            } else {
                let (chunked_row, _) = row.as_chunks::<2>();
                for ([a, b], out) in chunked_row.iter().zip(next_row.iter_mut()) {
                    *out += 0.25 * (a + b);
                }
            }

            // Zero out this row
            row.fill(0.);

            // See if the next mip level is done accumulating
            if row_index % 2 == 0 {
                // Not ready yet
                break;
            }

            // Ready--loop
            row_index = row_index / 2;
            buffer_offset += row_len as usize;
            row_len = next_row_len;
        }
        self.current_row += 1;
    }
}

#[derive(Debug)]
struct TextureGroup {
    active_texture: ActiveTexture,
    finished_textures: Vec<TextureInfo>,
    min: f64,
    max: f64,
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

        self.active_texture.add_row(queue, &msg.waterfall_row);
        self.active_texture.end_time = msg.end_time;
        self.min = msg.min;
        self.max = msg.max;
    }

    // Returns true if there are still chunks
    fn prune_old_textures(&mut self, time: Instant) -> bool {
        let mut any_remain = false;
        self.finished_textures.retain(|texture| {
            if texture.end_time > time {
                any_remain = true;
                true
            } else {
                false
            }
        });
        any_remain || self.active_texture.end_time > time
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

            let mut smaller_size = Extent3d {
                width: self.active_texture.texture.width(),
                height: self.active_texture.current_row as u32,
                depth_or_array_layers: 1,
            };
            let mip_level_count = self.active_texture.current_row.ilog2().max(1);
            let smaller_texture = device.create_texture(&TextureDescriptor {
                label: Some("Waterfall Texture"),
                size: smaller_size,
                mip_level_count,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: TextureFormat::R32Float,
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                view_formats: &[],
            });

            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Texture copy command encoder"),
            });

            for mip_level in 0..mip_level_count {
                encoder.copy_texture_to_texture(
                    TexelCopyTextureInfo {
                        texture: &self.active_texture.texture,
                        mip_level,
                        origin: Origin3d::ZERO,
                        aspect: TextureAspect::All,
                    },
                    TexelCopyTextureInfo {
                        texture: &smaller_texture,
                        mip_level,
                        origin: Origin3d::ZERO,
                        aspect: TextureAspect::All,
                    },
                    smaller_size,
                );

                smaller_size = Extent3d {
                    width: (smaller_size.width / 2).max(1),
                    height: smaller_size.height / 2,
                    depth_or_array_layers: 1,
                };
            }

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
    pub min: f32,
    pub max: f32,
    pub v_end: f32, // for active (partially filled) texture, the highest valid V component of UV coordinate
    pub period: f64,
    pub center_frequency: f64,
    pub width: f64,
    pub start_time: Instant,
    pub end_time: Instant,
}
