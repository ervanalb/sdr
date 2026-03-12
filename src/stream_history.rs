use crate::hardware::{ReceiveStreamDescriptor, StreamId};
use crate::processor::WaterfallRow;
use std::collections::{BTreeMap, VecDeque};
use std::mem;
use std::sync::Arc;
use chrono::{DateTime, Utc};
use wgpu::{
    Device, Extent3d, Origin3d, Queue, TexelCopyTextureInfo, Texture, TextureAspect,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
};

const TEXTURE_HEIGHT: u32 = 1024;

pub struct StreamHistory {
    pub active_streams: BTreeMap<StreamId, ActiveStream>,
    pub finished_streams: BTreeMap<StreamId, FinishedStream>,
    blank_texture: Texture,
}

impl StreamHistory {
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
            active_streams: BTreeMap::new(),
            finished_streams: BTreeMap::new(),
            blank_texture,
        }
    }

    pub fn push(
        &mut self,
        stream_id: StreamId,
        descriptor: Arc<ReceiveStreamDescriptor>,
        spectrum_len: usize,
        waterfall_rows: Vec<WaterfallRow>,
        device: &Device,
        queue: &Queue,
    ) {
        let active_texture = self.active_streams.entry(stream_id).or_insert_with(|| {
            let start_time = descriptor.start_time;
            ActiveStream::new(
                device,
                descriptor,
                spectrum_len as u32,
                start_time,
                self.blank_texture.clone(),
            )
        });
        for waterfall_row in waterfall_rows.iter() {
            if let Some(finished_texture) = active_texture.push(device, queue, waterfall_row) {
                Self::push_finished_texture(
                    &mut self.finished_streams,
                    stream_id,
                    active_texture.descriptor.clone(),
                    active_texture.min,
                    active_texture.max,
                    finished_texture,
                );
            }
        }
    }

    pub fn retain(
        &mut self,
        device: &Device,
        queue: &Queue,
        mut predicate: impl FnMut(StreamId) -> bool,
    ) {
        let closed: BTreeMap<_, _> = self
            .active_streams
            .extract_if(.., |stream_id, _| !predicate(*stream_id))
            .collect();

        // Move closed active textures to finished_streams
        for (stream_id, active_texture) in closed.into_iter() {
            let descriptor = active_texture.descriptor.clone();
            let min = active_texture.min;
            let max = active_texture.max;
            if let Some(finished_texture) =
                active_texture.finish(device, queue, self.blank_texture.clone())
            {
                Self::push_finished_texture(
                    &mut self.finished_streams,
                    stream_id,
                    descriptor,
                    min,
                    max,
                    finished_texture,
                );
            }
        }
    }

    pub fn prune_old_data(&mut self, time: DateTime<Utc>) {
        self.finished_streams
            .retain(|_, stream| stream.prune_old_data(time));
    }

    fn push_finished_texture(
        finished_streams: &mut BTreeMap<StreamId, FinishedStream>,
        stream_id: StreamId,
        descriptor: Arc<ReceiveStreamDescriptor>,
        min: f32,
        max: f32,
        finished_texture: FinishedTexture,
    ) {
        let finished_stream = finished_streams
            .entry(stream_id)
            .or_insert_with(|| FinishedStream {
                descriptor,
                textures: VecDeque::new(),
                min: f32::NAN,
                max: f32::NAN,
            });
        finished_stream.min = min;
        finished_stream.max = max;
        finished_stream.textures.push_back(finished_texture);
    }

    pub fn draw_list(&self) -> impl Iterator<Item = WaterfallDrawInfo> {
        // Collect all chunks into a draw list

        self.active_streams
            .values()
            .map(|active_texture| {
                let freq_min = active_texture.descriptor.frequency
                    - 0.5 * active_texture.descriptor.sample_rate;
                let freq_max = active_texture.descriptor.frequency
                    + 0.5 * active_texture.descriptor.sample_rate;
                WaterfallDrawInfo {
                    freq_min,
                    freq_max,
                    start_time: active_texture.start_time,
                    end_time: active_texture.end_time,
                    texture: active_texture.texture.clone(),
                    prev_texture: active_texture.prev_texture.clone(),
                    next_texture: self.blank_texture.clone(),
                    min: active_texture.min,
                    max: active_texture.max,
                    v_end: active_texture.current_row as f32 / TEXTURE_HEIGHT as f32,
                }
            })
            .chain(self.finished_streams.values().flat_map(|stream| {
                let freq_min = stream.descriptor.frequency - 0.5 * stream.descriptor.sample_rate;
                let freq_max = stream.descriptor.frequency + 0.5 * stream.descriptor.sample_rate;
                stream
                    .textures
                    .iter()
                    .map(move |finished_texture| WaterfallDrawInfo {
                        freq_min,
                        freq_max,
                        start_time: finished_texture.start_time,
                        end_time: finished_texture.end_time,
                        texture: finished_texture.texture.clone(),
                        prev_texture: finished_texture.prev_texture.clone(),
                        next_texture: finished_texture.next_texture.clone(),
                        min: stream.min,
                        max: stream.max,
                        v_end: 1.,
                    })
            }))
    }
}

#[derive(Debug)]
pub struct ActiveStream {
    pub descriptor: Arc<ReceiveStreamDescriptor>,
    prev_texture: Texture,
    texture: Texture,
    current_row: usize,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub spectrum_len: u32,
    mip_level_count: u32,
    mip_buffer: Vec<f32>,
    pub min: f32,
    pub max: f32,
    pub peak: f32,
    pub overload: bool,
}

impl ActiveStream {
    fn new(
        device: &Device,
        descriptor: Arc<ReceiveStreamDescriptor>,
        spectrum_len: u32,
        start_time: DateTime<Utc>,
        prev_texture: Texture,
    ) -> Self {
        let mip_level_count = TEXTURE_HEIGHT.ilog2().max(1);
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("Waterfall Texture"),
            size: Extent3d {
                width: spectrum_len,
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
            descriptor,
            prev_texture,
            texture,
            current_row: 0,
            start_time,
            end_time: start_time, // Haven't actually pushed the row yet
            spectrum_len,
            mip_level_count,
            // Allocate some extra space in the mip_buffer
            // in case waterfall_row.len() is very small
            mip_buffer: vec![0.; (2 * spectrum_len + mip_level_count) as usize],
            min: f32::NAN,
            max: f32::NAN,
            peak: f32::NAN,
            overload: false,
        }
    }

    fn new_following(device: &Device, prev: &ActiveStream) -> Self {
        Self::new(
            device,
            prev.descriptor.clone(),
            prev.spectrum_len,
            prev.end_time,
            prev.texture.clone(),
        )
    }

    fn push(
        &mut self,
        device: &Device,
        queue: &Queue,
        waterfall_row: &WaterfallRow,
    ) -> Option<FinishedTexture> {
        let finished_texture = if self.current_row >= TEXTURE_HEIGHT as usize {
            self.swap(device, queue)
        } else {
            None
        };

        let mut row_index = self.current_row as u32;
        let mut row_len = self.texture.width();
        let mut buffer_offset = 0;
        self.mip_buffer[0..row_len as usize].clone_from_slice(&waterfall_row.spectrum);
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
        self.end_time = waterfall_row.time;
        self.min = waterfall_row.min;
        self.max = waterfall_row.max;
        self.peak = waterfall_row.peak;
        self.overload = waterfall_row.overload;

        finished_texture
    }

    fn finish(
        self,
        device: &Device,
        queue: &Queue,
        next_texture: Texture,
    ) -> Option<FinishedTexture> {
        let Self {
            texture,
            prev_texture,
            current_row,
            start_time,
            end_time,
            ..
        } = self;

        if current_row == 0 {
            return None;
        }

        let texture = if current_row < TEXTURE_HEIGHT as usize {
            // If partial, copy this texture into an appropriately sized one
            // to free up space

            let mut smaller_size = Extent3d {
                width: texture.width(),
                height: current_row as u32,
                depth_or_array_layers: 1,
            };
            let mip_level_count = current_row.ilog2().max(1);
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
                        texture: &texture,
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
            smaller_texture
        } else {
            texture
        };

        Some(FinishedTexture {
            texture,
            prev_texture,
            next_texture,
            start_time,
            end_time,
        })
    }

    // When this texture fills up, swap it for a new one
    fn swap(&mut self, device: &Device, queue: &Queue) -> Option<FinishedTexture> {
        // Create a new active texture
        let prev = mem::replace(self, ActiveStream::new_following(device, self));
        prev.finish(device, queue, self.texture.clone())
    }
}

#[derive(Debug)]
pub struct FinishedStream {
    pub descriptor: Arc<ReceiveStreamDescriptor>,
    textures: VecDeque<FinishedTexture>,
    pub min: f32,
    pub max: f32,
}

impl FinishedStream {
    // Returns true if there are still chunks
    fn prune_old_data(&mut self, time: DateTime<Utc>) -> bool {
        let first_index = self.textures.partition_point(|texture| texture.end_time <= time);
        self.textures.drain(..first_index);
        !self.textures.is_empty()
    }
}

#[derive(Debug)]
struct FinishedTexture {
    texture: Texture,
    prev_texture: Texture,
    next_texture: Texture,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct WaterfallDrawInfo {
    pub freq_min: f64,
    pub freq_max: f64,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub texture: Texture,
    pub prev_texture: Texture,
    pub next_texture: Texture,
    pub min: f32,
    pub max: f32,
    pub v_end: f32, // for active (partially filled) texture, the highest valid V component of UV coordinate
}
