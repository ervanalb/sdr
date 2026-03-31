use crate::{
    document::{ClipDescriptor, ClipId, Document},
    dsp::{Fft, OverlapExpand, hann_window, log_mix_f32},
    hardware::RawIqSamples,
    waterfall_renderer::{WaterfallDrawInfo, WaterfallRenderer},
};
use egui::Rect;
use num_complex::Complex;
use rayon::prelude::*;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap},
    mem,
};
use wgpu::{
    Device, Extent3d, Origin3d, Queue, TexelCopyTextureInfo, Texture, TextureAspect,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
};

const TARGET_BIN_SIZE: f64 = 2.5e3; // 2.5 KHz
const MIN_QUANTILE: f64 = 0.1;
const MAX_QUANTILE: f64 = 0.999;
const MIN_MAX_TIME_CONSTANT: f64 = 1.;
const TEXTURE_HEIGHT: u32 = 1024;

/// Selection state stored in egui memory
#[derive(Clone, Default)]
struct ClipSelection {
    selected: BTreeSet<ClipId>,
}

pub struct DocumentGraphics {
    clips: BTreeMap<ClipId, ClipGraphics>,
    device: Device,
    queue: Queue,
    blank_texture: Texture,
}

impl DocumentGraphics {
    pub fn new(device: &Device, queue: &Queue) -> DocumentGraphics {
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

        DocumentGraphics {
            clips: BTreeMap::new(),
            device: device.clone(),
            queue: queue.clone(),
            blank_texture,
        }
    }

    pub fn process(&mut self, document: &Document) {
        // Remove processors for deleted clips
        self.clips
            .retain(|&clip_id, _| document.clips.contains_key(&clip_id));

        // Add processors for new clips
        for (&clip_id, clip) in document.clips.iter() {
            self.clips.entry(clip_id).or_insert_with(|| {
                ClipGraphics::new(
                    &self.device,
                    self.blank_texture.clone(),
                    clip.descriptor.clone(),
                    clip.chunks.start_index(),
                )
            });
        }

        // Gather work items
        let mut work = Vec::new();
        for (&clip_id, processor) in self.clips.iter_mut() {
            let clip = document.clips.get(&clip_id).unwrap();
            let next_chunk = processor.end_index;
            if next_chunk < clip.chunks.end_index() {
                work.push((processor, clip, next_chunk, clip.chunks.end_index()));
            }
        }

        // Run the work in parallel
        work.into_par_iter()
            .for_each(|(processor, clip, start_index, end_index)| {
                for chunk in clip.chunks.range(start_index..end_index) {
                    processor.process(&self.device, &self.queue, &chunk.data);
                }
                processor.end_index = end_index;
            });

        // Finalize processing for any clips that are no longer active
        for (&clip_id, processor) in self.clips.iter_mut() {
            if !processor.finalized() && !document.active_clips.contains(&clip_id) {
                processor.finalize(&self.device, &self.queue, &self.blank_texture);
            }
        }
    }

    pub fn expire(&mut self, _retain_time: f64) {
        todo!();
    }

    pub fn draw(
        &self,
        ui: &mut egui::Ui,
        figure_rect: egui::Rect,
        viewport: &crate::ui::Viewport,
        id: egui::Id,
    ) {
        // Get selection from egui memory
        let mut selection = ui
            .ctx()
            .memory_mut(|mem| mem.data.get_temp::<ClipSelection>(id))
            .unwrap_or_default();

        // Draw all clips and handle interactions in one loop
        for (&clip_id, clip) in self.clips.iter() {
            let is_selected = selection.selected.contains(&clip_id);
            let response = clip.draw(
                ui,
                figure_rect,
                viewport,
                clip_id,
                &self.blank_texture,
                is_selected,
            );

            if response.clicked() {
                let modifiers = ui.input(|i| i.modifiers);

                if modifiers.shift {
                    // Shift-click: add to selection
                    selection.selected.insert(clip_id);
                } else if modifiers.ctrl || modifiers.command {
                    // Ctrl-click (or Cmd on Mac): toggle in selection
                    if selection.selected.contains(&clip_id) {
                        selection.selected.remove(&clip_id);
                    } else {
                        selection.selected.insert(clip_id);
                    }
                } else {
                    // Regular click: replace selection
                    selection.selected.clear();
                    selection.selected.insert(clip_id);
                }
            }
        }

        // Store updated selection back to egui memory
        ui.ctx()
            .memory_mut(|mem| mem.data.insert_temp(id, selection));
    }
}

struct ClipGraphics {
    pub descriptor: ClipDescriptor,
    pub start_index: usize,
    pub end_index: usize,
    buffer: Vec<Complex<f32>>,
    overlap_expand: OverlapExpand<Complex<f32>>,
    hann_window: Box<[f32]>,
    fft: Fft,
    min_index: usize,
    max_index: usize,
    min_max_alpha: f32,
    min: f32,
    max: f32,
    active_segment: Option<ActiveSegment>,
    finished_segments: Vec<FinishedSegment>,
}

impl ClipGraphics {
    fn new(
        device: &Device,
        blank_texture: Texture,
        descriptor: ClipDescriptor,
        start_index: usize,
    ) -> ClipGraphics {
        // Pick a FFT size that is a power of 2 that is at least `sample_rate / target_bin_size`
        let min_fft_size = (descriptor.sample_rate / TARGET_BIN_SIZE).ceil() as usize;
        let fft_size = min_fft_size.next_power_of_two();

        let overlap_expand = OverlapExpand::new(fft_size);
        let fft = Fft::new(fft_size);

        let min_index = (MIN_QUANTILE * fft_size as f64).clamp(0., fft_size as f64 - 1.) as usize;
        let max_index = (MAX_QUANTILE * fft_size as f64).clamp(0., fft_size as f64 - 1.) as usize;

        let chunk_period = descriptor.chunk_size as f64 / descriptor.sample_rate;
        let min_max_alpha = (chunk_period / (MIN_MAX_TIME_CONSTANT + chunk_period)) as f32;

        ClipGraphics {
            descriptor,
            start_index,
            end_index: start_index,
            buffer: vec![],
            overlap_expand,
            hann_window: hann_window(fft_size),
            fft,
            min_index,
            max_index,
            min_max_alpha,
            min: f32::NAN,
            max: f32::NAN,
            active_segment: Some(ActiveSegment::new(
                device,
                fft_size as u32,
                start_index,
                blank_texture,
            )),
            finished_segments: vec![],
        }
    }

    fn process(&mut self, device: &Device, queue: &Queue, data: &RawIqSamples) {
        // Convert data to Complex<f32>
        match data {
            RawIqSamples::CS8(samples) => {
                self.buffer.extend(samples.iter().map(|&sample| {
                    (1. / 127.)
                        * Complex {
                            re: sample.re as f32,
                            im: sample.im as f32,
                        }
                }));
            }
            RawIqSamples::CF32(samples) => {
                self.buffer.extend(samples);
            }
        }

        // Split off an integer number of FFTs
        let fft_count = self.buffer.len() / self.fft.size();
        let split_pt = fft_count * self.fft.size();
        let mut samples = self.buffer.split_off(split_pt);
        mem::swap(&mut self.buffer, &mut samples); // split_off works the opposite way from what we want

        // Process incoming data into overlapping chunks
        let mut samples = self.overlap_expand.process(&samples);

        // Apply Hann window
        for one_fft in samples.chunks_exact_mut(self.fft.size()) {
            for (sample, win) in one_fft.iter_mut().zip(self.hann_window.iter()) {
                *sample *= win;
            }
        }

        // FFT
        self.fft.process_inplace(&mut samples);

        let mut spectrum = vec![0.; self.fft.size()].into_boxed_slice();

        // Accumulate power, for waterfall
        for one_fft in samples.chunks_exact(self.fft.size()) {
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

        if let Some(finished_segment) = self
            .active_segment
            .as_mut()
            .expect("Data was pushed to a clip after it was finalized")
            .push(&device, &queue, &spectrum)
        {
            self.finished_segments.push(finished_segment);
        }
    }

    fn finalized(&self) -> bool {
        self.active_segment.is_none()
    }

    fn finalize(&mut self, device: &Device, queue: &Queue, blank_texture: &Texture) {
        if let Some(finished_segment) = self
            .active_segment
            .take()
            .expect("Finalize called twice")
            .finalize(device, queue, blank_texture.clone())
        {
            self.finished_segments.push(finished_segment);
        }
    }

    pub fn draw(
        &self,
        ui: &mut egui::Ui,
        figure_rect: egui::Rect,
        viewport: &crate::ui::Viewport,
        clip_id: ClipId,
        blank_texture: &Texture,
        is_selected: bool,
    ) -> egui::Response {
        let y_top = viewport.screen_space_y(self.descriptor.freq_max());
        let y_bottom = viewport.screen_space_y(self.descriptor.freq_min());
        let x_left = viewport.screen_space_x(self.descriptor.time(self.start_index as f64));
        let x_right = viewport.screen_space_x(self.descriptor.time(self.end_index as f64));

        let draw_list = self
            .active_segment
            .as_ref()
            .map(move |active_segment| {
                let x_left =
                    viewport.screen_space_x(self.descriptor.time(active_segment.start_row as f64));
                let x_right =
                    viewport.screen_space_x(self.descriptor.time(active_segment.end_row as f64));

                WaterfallDrawInfo {
                    rect: egui::Rect::from_min_max(
                        egui::pos2(x_left, y_top),
                        egui::pos2(x_right, y_bottom),
                    ),
                    texture: active_segment.texture.clone(),
                    prev_texture: active_segment.prev_texture.clone(),
                    next_texture: blank_texture.clone(),
                    min: self.min,
                    max: self.max,
                    v_end: (active_segment.end_row - active_segment.start_row) as f32
                        / TEXTURE_HEIGHT as f32,
                }
            })
            .into_iter()
            .chain(self.finished_segments.iter().map(move |finished_segment| {
                let x_start = viewport
                    .screen_space_x(self.descriptor.time(finished_segment.start_row as f64));
                let x_end =
                    viewport.screen_space_x(self.descriptor.time(finished_segment.end_row as f64));

                WaterfallDrawInfo {
                    rect: egui::Rect::from_min_max(
                        egui::pos2(x_start, y_top),
                        egui::pos2(x_end, y_bottom),
                    ),
                    texture: finished_segment.texture.clone(),
                    prev_texture: finished_segment.prev_texture.clone(),
                    next_texture: finished_segment.next_texture.clone(),
                    min: self.min,
                    max: self.max,
                    v_end: 1.,
                }
            }))
            .collect();

        let id = ui.id().with(("clip", clip_id));
        let painter = ui.painter().with_clip_rect(figure_rect);

        // Draw waterfall
        painter.add(egui_wgpu::Callback::new_paint_callback(
            figure_rect,
            Callback {
                id,
                viewport_size: figure_rect.size(),
                waterfall_chunks: draw_list,
            },
        ));

        // Create interaction area for clip
        let clip_rect = Rect::from_min_max(
            figure_rect.min + egui::vec2(x_left, y_top),
            figure_rect.min + egui::vec2(x_right, y_bottom),
        );

        let clip_interact_id = ui.id().with(("clip_interact", clip_id));
        let response = ui.interact(clip_rect, clip_interact_id, egui::Sense::click());

        // Draw border based on selection and hover state
        let stroke = if is_selected {
            // Selected clips get a brighter border
            egui::Stroke::new(2.0, ui.visuals().widgets.active.fg_stroke.color)
        } else if response.hovered() {
            // Hovered clips get the standard hover color
            egui::Stroke::new(1.0, ui.visuals().widgets.hovered.fg_stroke.color)
        } else {
            // No border for non-selected, non-hovered clips
            egui::Stroke::NONE
        };

        if stroke != egui::Stroke::NONE {
            painter.rect_stroke(clip_rect, 0.0, stroke, egui::StrokeKind::Outside);
        }

        response
    }
}

#[derive(Debug)]
pub struct ActiveSegment {
    prev_texture: Texture,
    texture: Texture,
    start_row: usize,
    end_row: usize,
    mip_level_count: u32,
    mip_buffer: Vec<f32>,
}

impl ActiveSegment {
    fn new(device: &Device, spectrum_len: u32, start_row: usize, prev_texture: Texture) -> Self {
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
            prev_texture,
            texture,
            start_row,
            end_row: start_row,
            mip_level_count,
            // Allocate some extra space in the mip_buffer
            // in case waterfall_row.len() is very small
            mip_buffer: vec![0.; (2 * spectrum_len + mip_level_count) as usize],
        }
    }

    fn new_following(device: &Device, spectrum_len: u32, prev: &ActiveSegment) -> Self {
        Self::new(device, spectrum_len, prev.end_row, prev.texture.clone())
    }

    fn push(
        &mut self,
        device: &Device,
        queue: &Queue,
        spectrum: &[f32],
    ) -> Option<FinishedSegment> {
        let finished_texture = if self.end_row - self.start_row >= TEXTURE_HEIGHT as usize {
            self.swap(device, queue, spectrum.len() as u32)
        } else {
            None
        };

        let mut row_index = (self.end_row - self.start_row) as u32;
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
        self.end_row += 1;

        finished_texture
    }

    fn finalize(
        self,
        device: &Device,
        queue: &Queue,
        next_texture: Texture,
    ) -> Option<FinishedSegment> {
        let Self {
            texture,
            prev_texture,
            start_row,
            end_row,
            ..
        } = self;

        if end_row == start_row {
            return None;
        }

        let texture = if end_row - start_row < TEXTURE_HEIGHT as usize {
            // If partial, copy this texture into an appropriately sized one
            // to free up space

            let mut smaller_size = Extent3d {
                width: texture.width(),
                height: (end_row - start_row) as u32,
                depth_or_array_layers: 1,
            };
            let mip_level_count = (end_row - start_row).ilog2().max(1);
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

        Some(FinishedSegment {
            texture,
            prev_texture,
            next_texture,
            start_row,
            end_row,
        })
    }

    // When this texture fills up, swap it for a new one
    fn swap(
        &mut self,
        device: &Device,
        queue: &Queue,
        spectrum_len: u32,
    ) -> Option<FinishedSegment> {
        // Create a new active texture
        let prev = mem::replace(
            self,
            ActiveSegment::new_following(device, spectrum_len, self),
        );
        prev.finalize(device, queue, self.texture.clone())
    }
}

struct FinishedSegment {
    texture: Texture,
    prev_texture: Texture,
    next_texture: Texture,
    start_row: usize,
    end_row: usize,
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
    waterfall_chunks: Vec<WaterfallDrawInfo>,
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
