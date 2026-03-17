use crate::{
    dsp::log_mix_f32,
    hardware::StreamId,
    preprocessor::PreprocessedStreamDescriptor,
    processor::{CreationContext, Processor, ProcessorHistory, ProcessorParameters},
    ui::Viewport,
    waterfall_renderer::WaterfallRenderer,
};
use chrono::{DateTime, TimeDelta, Utc};
use num_complex::Complex;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, VecDeque, btree_map::Entry},
    mem,
    sync::mpsc::{Receiver, Sender, channel},
};
use wgpu::{
    Device, Extent3d, Origin3d, Queue, TexelCopyTextureInfo, Texture, TextureAspect,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
};

const MIN_QUANTILE: f64 = 0.1;
const MAX_QUANTILE: f64 = 0.99;
const MIN_MAX_TIME_CONSTANT: f64 = 1.;

#[derive(Clone, Debug, Serialize, Deserialize)]
// #[serde(rename = "waterfall")] -- Doesn't work?
pub struct WaterfallProcessorParameters {}

#[typetag::serde]
impl ProcessorParameters for WaterfallProcessorParameters {
    fn create_processor(
        &self,
        cc: &CreationContext<'_>,
    ) -> (Box<dyn Processor>, Box<dyn ProcessorHistory>) {
        let (sender, receiver) = channel();
        (
            Box::new(WaterfallProcessor::new(sender)),
            Box::new(WaterfallHistory::new(cc.device, cc.queue, receiver)),
        )
    }
}

pub struct WaterfallProcessor {
    streams: BTreeMap<StreamId, StreamProcessor>,
    sender: Sender<WaterfallMessage>,
}

impl WaterfallProcessor {
    pub fn new(sender: Sender<WaterfallMessage>) -> WaterfallProcessor {
        WaterfallProcessor {
            streams: BTreeMap::new(),
            sender,
        }
    }
}

impl Processor for WaterfallProcessor {
    fn reset(&mut self) {
        self.streams.clear();
        self.sender.send(WaterfallMessage::Reset).ok();
    }

    fn start_stream(&mut self, stream_id: usize, descriptor: &PreprocessedStreamDescriptor) {
        match self.streams.entry(stream_id) {
            Entry::Vacant(e) => {
                let processor = StreamProcessor::new(descriptor);
                let spectrum_len = processor.fft_size as u32;
                e.insert(processor);
                self.sender
                    .send(WaterfallMessage::StartStream {
                        stream_id,
                        spectrum_len,
                        start_time: descriptor.start_time,
                        freq_min: descriptor.frequency - 0.5 * descriptor.sample_rate,
                        freq_max: descriptor.frequency + 0.5 * descriptor.sample_rate,
                    })
                    .ok();
            }
            Entry::Occupied(_) => {
                panic!("start_stream() called with a stream that already exists");
            }
        }
    }

    fn process_chunk(
        &mut self,
        stream_id: StreamId,
        time: DateTime<chrono::Utc>,
        preprocessed_data: &[Complex<f32>],
    ) {
        let processor = self
            .streams
            .get_mut(&stream_id)
            .expect("process_chunk() called with a stream that doesn't exist");
        let (spectrum, min, max) = processor.process_chunk(preprocessed_data);
        let waterfall_row = WaterfallRow {
            time,
            spectrum,
            min,
            max,
        };
        self.sender
            .send(WaterfallMessage::PushRow {
                stream_id,
                waterfall_row,
            })
            .ok();
    }

    fn end_stream(&mut self, stream_id: usize) {
        self.streams
            .remove(&stream_id)
            .expect("end_stream() called with a stream that doesn't exist");
        self.sender
            .send(WaterfallMessage::EndStream(stream_id))
            .ok();
    }
}

pub struct StreamProcessor {
    fft_size: usize,
    min_index: usize,
    max_index: usize,
    min_max_alpha: f32,
    min: f32,
    max: f32,
}

impl StreamProcessor {
    fn new(descriptor: &PreprocessedStreamDescriptor) -> StreamProcessor {
        let fft_size = descriptor.fft_size;
        let min_index = (MIN_QUANTILE * fft_size as f64).clamp(0., fft_size as f64 - 1.) as usize;
        let max_index = (MAX_QUANTILE * fft_size as f64).clamp(0., fft_size as f64 - 1.) as usize;

        let min_max_alpha =
            (descriptor.chunk_period / (MIN_MAX_TIME_CONSTANT + descriptor.chunk_period)) as f32;

        StreamProcessor {
            fft_size,
            min_index,
            max_index,
            min_max_alpha,
            min: f32::NAN,
            max: f32::NAN,
        }
    }

    fn process_chunk(&mut self, preprocessed_data: &[Complex<f32>]) -> (Box<[f32]>, f32, f32) {
        let mut spectrum = vec![0.; self.fft_size].into_boxed_slice();

        let fft_count = preprocessed_data.len() / self.fft_size;

        // Accumulate power, for waterfall
        for one_fft in preprocessed_data.chunks_exact(self.fft_size) {
            for (&sample, spectrum_sample) in one_fft.iter().zip(spectrum.iter_mut()) {
                *spectrum_sample += sample.re * sample.re + sample.im * sample.im;
            }
        }

        let inv_fft_count = 1.0 / (fft_count as f32);
        for sample in spectrum.iter_mut() {
            *sample *= inv_fft_count;
        }

        let mut spectrum_for_quantiles = spectrum.clone();
        let (_, &mut new_min, _) = spectrum_for_quantiles
            .select_nth_unstable_by(self.min_index, |a, b| {
                a.partial_cmp(b).unwrap_or(Ordering::Equal)
            });
        let (_, &mut new_max, _) = spectrum_for_quantiles
            .select_nth_unstable_by(self.max_index, |a, b| {
                a.partial_cmp(b).unwrap_or(Ordering::Equal)
            });

        let new_min = new_min.max(1e-10);
        let new_max = new_max.max(1e-10);

        // Compute min/max with LPF
        if self.min <= self.max {
            // Normal case:

            // LPF in log space
            self.min = log_mix_f32(self.min, new_min, self.min_max_alpha);
            self.max = log_mix_f32(self.max, new_max, self.min_max_alpha);
        } else {
            // On startup, or if something goes wrong:
            self.min = new_min;
            self.max = new_max;
        }

        (spectrum, self.min, self.max)
    }
}

pub enum WaterfallMessage {
    Reset,
    StartStream {
        stream_id: StreamId,
        spectrum_len: u32,
        start_time: DateTime<Utc>,
        freq_min: f64,
        freq_max: f64,
    },
    EndStream(StreamId),
    PushRow {
        stream_id: StreamId,
        waterfall_row: WaterfallRow,
    },
}

pub struct WaterfallRow {
    pub time: DateTime<Utc>,
    pub spectrum: Box<[f32]>,
    pub min: f32,
    pub max: f32,
}

///////////////////////////////////////////////////////////////////////////////

const TEXTURE_HEIGHT: u32 = 1024;

pub struct WaterfallHistory {
    receiver: Receiver<WaterfallMessage>,
    device: Device,
    queue: Queue,
    pub active_streams: BTreeMap<StreamId, ActiveStream>,
    pub finished_streams: BTreeMap<StreamId, FinishedStream>,
    blank_texture: Texture,
}

impl WaterfallHistory {
    pub fn new(device: &Device, queue: &Queue, receiver: Receiver<WaterfallMessage>) -> Self {
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

        WaterfallHistory {
            device: device.clone(),
            queue: queue.clone(),
            receiver,
            active_streams: BTreeMap::new(),
            finished_streams: BTreeMap::new(),
            blank_texture,
        }
    }
}

impl ProcessorHistory for WaterfallHistory {
    fn update(&mut self) {
        for msg in self.receiver.try_iter() {
            match msg {
                WaterfallMessage::Reset => {
                    self.active_streams.clear();
                    self.finished_streams.clear();
                }
                WaterfallMessage::StartStream {
                    stream_id,
                    spectrum_len,
                    start_time,
                    freq_min,
                    freq_max,
                } => match self.active_streams.entry(stream_id) {
                    Entry::Vacant(e) => {
                        e.insert(ActiveStream::new(
                            &self.device,
                            spectrum_len,
                            start_time,
                            freq_min,
                            freq_max,
                            self.blank_texture.clone(),
                        ));
                    }
                    Entry::Occupied(_) => {
                        panic!("Tried to add a new stream that already exists");
                    }
                },
                WaterfallMessage::EndStream(stream_id) => {
                    let active_texture = self
                        .active_streams
                        .remove(&stream_id)
                        .expect("Tried to end a stream that doesn't exist");
                    let freq_min = active_texture.freq_min;
                    let freq_max = active_texture.freq_max;
                    let min = active_texture.min;
                    let max = active_texture.max;
                    let finished_texture = active_texture.finish(
                        &self.device,
                        &self.queue,
                        self.blank_texture.clone(),
                    );
                    Self::update_finished(
                        &mut self.finished_streams,
                        freq_min,
                        freq_max,
                        stream_id,
                        min,
                        max,
                        finished_texture,
                    );
                }
                WaterfallMessage::PushRow {
                    stream_id,
                    waterfall_row,
                } => {
                    let active_texture = self
                        .active_streams
                        .get_mut(&stream_id)
                        .expect("Tried to push a waterfall row to a stream that doesn't exist");

                    let finished_texture =
                        active_texture.push(&self.device, &self.queue, waterfall_row);
                    Self::update_finished(
                        &mut self.finished_streams,
                        active_texture.freq_min,
                        active_texture.freq_max,
                        stream_id,
                        active_texture.min,
                        active_texture.max,
                        finished_texture,
                    );
                }
            }
        }
    }

    fn expire(&mut self, retain_time: DateTime<chrono::Utc>) {
        self.finished_streams
            .retain(|_, stream| stream.prune_old_data(retain_time));
    }

    fn draw(
        &self,
        ui: &mut egui::Ui,
        id: egui::Id,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        _dt: TimeDelta,
    ) {
        // Collect all chunks into a draw list

        let draw_list = self
            .active_streams
            .values()
            .map(|active_texture| WaterfallDrawInfo {
                freq_min: active_texture.freq_min,
                freq_max: active_texture.freq_max,
                start_time: active_texture.start_time,
                end_time: active_texture.end_time,
                texture: active_texture.texture.clone(),
                prev_texture: active_texture.prev_texture.clone(),
                next_texture: self.blank_texture.clone(),
                min: active_texture.min,
                max: active_texture.max,
                v_end: active_texture.current_row as f32 / TEXTURE_HEIGHT as f32,
            })
            .chain(self.finished_streams.values().flat_map(|stream| {
                stream
                    .textures
                    .iter()
                    .map(move |finished_texture| WaterfallDrawInfo {
                        freq_min: stream.freq_min,
                        freq_max: stream.freq_max,
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
            .collect();

        ui.painter()
            .with_clip_rect(figure_rect)
            .add(egui_wgpu::Callback::new_paint_callback(
                figure_rect,
                Callback {
                    id,
                    viewport_size: figure_rect.size(),
                    translation: viewport.translation,
                    scale: viewport.scale,
                    waterfall_chunks: draw_list,
                    reference_time: viewport.reference_time,
                },
            ));
    }
}

impl WaterfallHistory {
    fn update_finished(
        finished_streams: &mut BTreeMap<StreamId, FinishedStream>,
        freq_min: f64,
        freq_max: f64,
        stream_id: StreamId,
        min: f32,
        max: f32,
        finished_texture: Option<FinishedTexture>,
    ) {
        match finished_streams.entry(stream_id) {
            Entry::Occupied(mut e) => {
                let finished_stream = e.get_mut();
                // Even if we don't have a new finished texture to push,
                // update the min & max values
                finished_stream.min = min;
                finished_stream.max = max;
                if let Some(finished_texture) = finished_texture {
                    finished_stream.textures.push_back(finished_texture);
                }
            }
            Entry::Vacant(e) => {
                if let Some(finished_texture) = finished_texture {
                    let mut textures = VecDeque::new();
                    textures.push_back(finished_texture);
                    e.insert(FinishedStream {
                        freq_min,
                        freq_max,
                        textures,
                        min,
                        max,
                    });
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct ActiveStream {
    pub freq_min: f64,
    pub freq_max: f64,
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
}

impl ActiveStream {
    fn new(
        device: &Device,
        spectrum_len: u32,
        start_time: DateTime<Utc>,
        freq_min: f64,
        freq_max: f64,
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
            freq_min,
            freq_max,
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
        }
    }

    fn new_following(device: &Device, prev: &ActiveStream) -> Self {
        Self::new(
            device,
            prev.spectrum_len,
            prev.end_time,
            prev.freq_min,
            prev.freq_max,
            prev.texture.clone(),
        )
    }

    fn push(
        &mut self,
        device: &Device,
        queue: &Queue,
        waterfall_row: WaterfallRow,
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
    pub freq_min: f64,
    pub freq_max: f64,
    textures: VecDeque<FinishedTexture>,
    pub min: f32,
    pub max: f32,
}

impl FinishedStream {
    // Returns true if there are still chunks
    fn prune_old_data(&mut self, retain_time: DateTime<Utc>) -> bool {
        // TODO: actually shrink textures with data older than retain_time
        let first_index = self
            .textures
            .partition_point(|texture| texture.end_time <= retain_time);
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

///////////////////////////////////////////////////////////////////////////////

pub struct StaticResources {
    target_format: wgpu::TextureFormat,
    instances: HashMap<egui::Id, CanvasResources>,
}

struct CanvasResources {
    viewport_uniform_buffer: wgpu::Buffer,
    waterfall_renderer: WaterfallRenderer,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniforms {
    viewport_size: [f32; 2],
    translation: [f32; 2],
    scale: [f32; 2],
    _padding: [f32; 2],
}

pub fn static_init(cc: &eframe::CreationContext<'_>) {
    let wgpu_render_state = cc.wgpu_render_state.as_ref().unwrap();
    let target_format = wgpu_render_state.target_format;

    wgpu_render_state
        .renderer
        .write()
        .callback_resources
        .insert(StaticResources {
            target_format,
            instances: HashMap::new(),
        });
}

struct Callback {
    id: egui::Id,
    viewport_size: egui::Vec2,
    translation: egui::Vec2,
    scale: egui::Vec2,
    waterfall_chunks: Vec<WaterfallDrawInfo>,
    reference_time: DateTime<Utc>,
}

impl egui_wgpu::CallbackTrait for Callback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let static_resources: &mut StaticResources = callback_resources.get_mut().unwrap();
        let target_format = static_resources.target_format;

        // Get or create canvas resources
        let resources = static_resources
            .instances
            .entry(self.id)
            .or_insert_with(|| {
                // Create uniform buffer
                let viewport_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Viewport Uniform Buffer"),
                    size: std::mem::size_of::<ViewportUniforms>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });

                // Create waterfall renderer
                let waterfall_renderer = WaterfallRenderer::new(device, target_format);

                CanvasResources {
                    viewport_uniform_buffer,
                    waterfall_renderer,
                }
            });

        // Update uniform buffer with viewport parameters
        let uniforms = ViewportUniforms {
            viewport_size: [self.viewport_size.x, self.viewport_size.y],
            translation: [self.translation.x, self.translation.y],
            scale: [self.scale.x, self.scale.y],
            _padding: [0.0; 2],
        };
        queue.write_buffer(
            &resources.viewport_uniform_buffer,
            0,
            bytemuck::cast_slice(&[uniforms]),
        );

        // Prepare waterfall draw calls
        resources.waterfall_renderer.prepare(
            self.waterfall_chunks.clone(),
            device,
            queue,
            &resources.viewport_uniform_buffer,
            self.reference_time,
        );

        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        let static_resources: &StaticResources = callback_resources.get().unwrap();

        if let Some(resources) = static_resources.instances.get(&self.id) {
            // Draw waterfall
            resources.waterfall_renderer.render(render_pass);
        }
    }
}
