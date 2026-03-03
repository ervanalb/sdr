use crate::hardware::{ReceiveStreamDescriptor, ReceiveStreamId};
use std::collections::BTreeMap;
use std::time::Instant;
use wgpu::{
    Device, Extent3d, Origin3d, Queue, TexelCopyTextureInfo, Texture, TextureAspect,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
};

const TEXTURE_HEIGHT: u32 = 1024;

pub struct WaterfallGpu {
    texture_groups: BTreeMap<ReceiveStreamId, TextureGroup>,
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
            texture_groups: BTreeMap::new(),
            blank_texture,
        }
    }

    pub fn add_row(
        &mut self,
        stream_id: ReceiveStreamId,
        descriptor: &ReceiveStreamDescriptor,
        time: Instant,
        spectrum: &[f32],
        min: f32,
        max: f32,
        device: &Device,
        queue: &Queue,
    ) {
        let group = self
            .texture_groups
            .entry(stream_id)
            .or_insert_with(|| TextureGroup {
                descriptor: descriptor.clone(),
                active_texture: ActiveTexture::new(
                    device,
                    spectrum,
                    descriptor.start_time,
                    self.blank_texture.clone(),
                ),
                finished_textures: vec![],
                min,
                max,
            });
        group.add_row(device, queue, spectrum, min, max, time);
    }

    pub fn close_stream(&mut self, stream: ReceiveStreamId) {
        // XXX TODO
    }

    pub fn prune_old_textures(&mut self, time: Instant) {
        self.texture_groups
            .retain(|_, group| group.prune_old_textures(time));
    }

    pub fn draw_list(&self) -> impl Iterator<Item = ChunkDrawInfo> {
        // Collect all chunks into a draw list
        self.texture_groups.iter().flat_map(|(_stream_id, group)| {
            let freq_min = group.descriptor.frequency - 0.5 * group.descriptor.sample_rate;
            let freq_max = group.descriptor.frequency + 0.5 * group.descriptor.sample_rate;
            group
                .finished_textures
                .iter()
                .map(move |texture_info| ChunkDrawInfo {
                    freq_min,
                    freq_max,
                    start_time: texture_info.start_time,
                    end_time: texture_info.end_time,
                    texture: texture_info.texture.clone(),
                    prev_texture: texture_info.prev_texture.clone(),
                    next_texture: texture_info.next_texture.clone(),
                    min: group.min,
                    max: group.max,
                    v_end: 1.,
                })
                .chain(Some(ChunkDrawInfo {
                    freq_min,
                    freq_max,
                    start_time: group.active_texture.start_time,
                    end_time: group.active_texture.end_time,
                    texture: group.active_texture.texture.clone(),
                    prev_texture: group.active_texture.prev_texture.clone(),
                    next_texture: self.blank_texture.clone(),
                    min: group.min,
                    max: group.max,
                    v_end: group.active_texture.current_row as f32 / TEXTURE_HEIGHT as f32,
                }))
        })
    }
}

#[derive(Debug)]
struct ActiveTexture {
    prev_texture: Texture,
    texture: Texture,
    current_row: usize,
    start_time: Instant,
    end_time: Instant,
    mip_level_count: u32,
    mip_buffer: Vec<f32>,
}

impl ActiveTexture {
    pub fn new(
        device: &Device,
        spectrum: &[f32],
        start_time: Instant,
        prev_texture: Texture,
    ) -> Self {
        let mip_level_count = TEXTURE_HEIGHT.ilog2().max(1);
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("Waterfall Texture"),
            size: Extent3d {
                width: spectrum.len() as u32,
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
            start_time,
            end_time: start_time, // Haven't actually pushed the row yet
            mip_level_count,
            // Allocate some extra space in the mip_buffer
            // in case waterfall_row.len() is very small
            mip_buffer: vec![0.; 2 * spectrum.len() + mip_level_count as usize],
        }
    }

    fn add_row(&mut self, queue: &Queue, spectrum: &[f32]) {
        let mut row_index = self.current_row as u32;
        let mut row_len = self.texture.width();
        let mut buffer_offset = 0;
        self.mip_buffer[0..row_len as usize].clone_from_slice(spectrum);
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
    descriptor: ReceiveStreamDescriptor,
    active_texture: ActiveTexture,
    finished_textures: Vec<TextureInfo>,
    min: f32,
    max: f32,
}

impl TextureGroup {
    fn add_row(
        &mut self,
        device: &Device,
        queue: &Queue,
        spectrum: &[f32],
        min: f32,
        max: f32,
        end_time: Instant,
    ) {
        if self.active_texture.current_row >= TEXTURE_HEIGHT as usize {
            self.swap_active_texture(device, queue, spectrum);
        }

        self.active_texture.add_row(queue, spectrum);
        self.active_texture.end_time = end_time;
        self.min = min;
        self.max = max;
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
        spectrum: &[f32],
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
        let prev_end_time = self.active_texture.end_time;
        let prev_texture = self.active_texture.texture.clone();
        let old_active_texture = std::mem::replace(
            &mut self.active_texture,
            ActiveTexture::new(device, spectrum, prev_end_time, prev_texture),
        );
        let next_texture = self.active_texture.texture.clone();

        self.finished_textures.push(TextureInfo {
            texture: old_active_texture.texture,
            prev_texture: old_active_texture.prev_texture,
            next_texture,
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
    start_time: Instant,
    end_time: Instant,
}

#[derive(Debug, Clone)]
pub struct ChunkDrawInfo {
    pub freq_min: f64,
    pub freq_max: f64,
    pub start_time: Instant,
    pub end_time: Instant,
    pub texture: Texture,
    pub prev_texture: Texture,
    pub next_texture: Texture,
    pub min: f32,
    pub max: f32,
    pub v_end: f32, // for active (partially filled) texture, the highest valid V component of UV coordinate
}
