use crate::{
    document::{ClipDescriptor, ClipId, Document},
    dsp::{Fft, OverlapExpand, hann_window, log_mix_f32},
    hardware::RawIqSamples,
    ui::Viewport,
    waterfall_renderer::{WaterfallDrawInfo, WaterfallRenderer},
};
use egui::Rect;
use num_complex::Complex;
use rayon::prelude::*;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    mem,
};
use wgpu::{
    Device, Extent3d, Origin3d, Queue, Texture, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages,
};

const TARGET_BIN_SIZE: f64 = 2.5e3; // 2.5 KHz
const MIN_QUANTILE: f64 = 0.1;
const MAX_QUANTILE: f64 = 0.999;
const MIN_MAX_TIME_CONSTANT: f64 = 1.;
const TEXTURE_HEIGHT: u32 = 256; //1024;

pub struct DocumentGraphics {
    pub clips: BTreeMap<ClipId, ClipGraphics>,
    pub prev_document: Document,
    pub selected: BTreeSet<ClipId>,
    pub hovered: BTreeSet<ClipId>,
    pub draw_order: Vec<ClipId>,
}

impl DocumentGraphics {
    pub fn new() -> Self {
        DocumentGraphics {
            clips: BTreeMap::new(),
            prev_document: Document::new(),
            selected: BTreeSet::new(),
            hovered: BTreeSet::new(),
            draw_order: Vec::new(),
        }
    }

    pub fn process(
        &mut self,
        device: &Device,
        queue: &Queue,
        document: &Document,
        active_clips: &BTreeSet<ClipId>,
    ) {
        // Remove clip graphics for deleted clips
        for (clip_id, _prev_clip) in self.prev_document.removed_clips(document) {
            self.clips.remove(&clip_id);
            self.draw_order.retain(|&i| i != clip_id);
            self.hovered.remove(&clip_id);
        }

        // Reset the graphics for clips that changed in a meaningful way,
        // or update graphics for clips that changed in a trivial way
        for (clip_id, prev_clip, new_clip) in self.prev_document.modified_clips(document) {
            let ClipDescriptor {
                name: _,
                frequency: _,
                sample_rate: prev_sample_rate,
                reference_time: _,
                chunk_size: prev_chunk_size,
            } = prev_clip.descriptor;
            let ClipDescriptor {
                name: _,
                frequency: _,
                sample_rate,
                reference_time: _,
                chunk_size,
            } = new_clip.descriptor;

            let clip_graphics = self.clips.get_mut(&clip_id).unwrap();

            if sample_rate != prev_sample_rate
                || chunk_size != prev_chunk_size
                || !new_clip.chunks.is_continuation_of(&prev_clip.chunks)
                || (clip_graphics.active_segment.is_none()
                    && new_clip.chunks.end_index() > clip_graphics.end_index)
            {
                *clip_graphics = ClipGraphics::new(
                    device,
                    new_clip.descriptor.clone(),
                    new_clip.chunks.start_index(),
                )
            } else {
                clip_graphics.descriptor = new_clip.descriptor.clone();
            }
        }

        // Add clip graphics for new clips
        for (clip_id, new_clip) in self.prev_document.added_clips(document) {
            self.clips.insert(
                clip_id,
                ClipGraphics::new(
                    device,
                    new_clip.descriptor.clone(),
                    new_clip.chunks.start_index(),
                ),
            );
            self.draw_order.push(clip_id);
        }

        self.prev_document = document.clone();

        // Gather work items
        let work: Vec<_> = self
            .clips
            .iter_mut()
            .filter_map(|(clip_id, clip_graphics)| {
                let new_clip = document.clips.get(clip_id).unwrap();
                (clip_graphics.end_index < new_clip.chunks.end_index())
                    .then_some((clip_graphics, new_clip))
            })
            .collect();

        // Run the work in parallel
        work.into_par_iter().for_each(|(clip_graphics, clip)| {
            let work_start_index = clip_graphics.end_index;
            let work_end_index = clip.chunks.end_index();
            for chunk in clip.chunks.range(work_start_index..work_end_index) {
                clip_graphics.process(device, queue, chunk.as_ref());
            }
            assert_eq!(clip_graphics.end_index, work_end_index);
        });

        // Set active_segment to None for any clips that are no longer active
        for (&clip_id, clip_graphics) in self.clips.iter_mut() {
            if clip_graphics.active_segment.is_some() && !active_clips.contains(&clip_id) {
                clip_graphics.active_segment = None;
            }
        }
    }

    pub fn bring_to_top(&mut self, clip_id: ClipId) {
        if let Some(pos) = self.draw_order.iter().position(|id| *id == clip_id) {
            // Remove the clip from its current position
            self.draw_order.remove(pos);
            // Add it to the end of list (visually, on top)
            self.draw_order.push(clip_id);
        }
    }

    // Call this after calling .expire() on the document.
    // It is invalid to call this if the document has been modified
    // in a way that is not consistent with .expire()
    // since the last call to .process().
    pub fn process_expiry(&mut self, document: &Document, _retain_time: f64) {
        // Remove clip graphics for deleted clips
        for (clip_id, _prev_clip) in self.prev_document.removed_clips(document) {
            self.clips.remove(&clip_id);
            self.draw_order.retain(|&i| i != clip_id);
            self.hovered.remove(&clip_id);
        }

        if self.prev_document.added_clips(document).next().is_some() {
            panic!("Clips added--expiry should not add clips");
        }

        // Shrink any clip graphics that have had content removed from the start
        for (clip_id, clip) in self.clips.iter_mut() {
            let remove_start_index = clip.start_index;
            let remove_end_index = document.clips.get(clip_id).unwrap().chunks.start_index();
            let segments_to_remove: usize = (remove_end_index.div_euclid(TEXTURE_HEIGHT as isize)
                - remove_start_index.div_euclid(TEXTURE_HEIGHT as isize))
            .try_into()
            .unwrap();
            clip.segments.drain(..segments_to_remove);
            clip.start_index = remove_end_index;
        }

        self.prev_document = document.clone();
    }
}

pub struct ClipGraphics {
    pub descriptor: ClipDescriptor,
    pub start_index: isize,
    pub end_index: isize,
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
    segments: VecDeque<Segment>,
    blank_texture: Texture,
}

impl ClipGraphics {
    fn new(device: &Device, descriptor: ClipDescriptor, start_index: isize) -> ClipGraphics {
        // Pick a FFT size that is a power of 2 that is at least `sample_rate / target_bin_size`
        let min_fft_size = (descriptor.sample_rate / TARGET_BIN_SIZE).ceil() as usize;
        let fft_size = min_fft_size.next_power_of_two();

        let overlap_expand = OverlapExpand::new(fft_size);
        let fft = Fft::new(fft_size);

        let min_index = (MIN_QUANTILE * fft_size as f64).clamp(0., fft_size as f64 - 1.) as usize;
        let max_index = (MAX_QUANTILE * fft_size as f64).clamp(0., fft_size as f64 - 1.) as usize;

        let chunk_period = descriptor.chunk_size as f64 / descriptor.sample_rate;
        let min_max_alpha = (chunk_period / (MIN_MAX_TIME_CONSTANT + chunk_period)) as f32;

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
            active_segment: Some(ActiveSegment::new(fft_size as u32)),
            segments: VecDeque::new(),
            blank_texture,
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

        let active_segment = self
            .active_segment
            .as_mut()
            .expect("Data was pushed to a clip after it was finalized");

        // Calculate which segment this row belongs to
        let segment_index = self.end_index.div_euclid(TEXTURE_HEIGHT as isize)
            - self.start_index.div_euclid(TEXTURE_HEIGHT as isize);
        let row_in_segment = self.end_index.rem_euclid(TEXTURE_HEIGHT as isize) as u32;

        // Create new segment if needed
        if segment_index >= self.segments.len() as isize {
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

            self.segments.push_back(Segment { texture });
        }

        // Push the row to the appropriate segment
        let segment = &self.segments[segment_index as usize];
        active_segment.push_row(queue, &segment.texture, row_in_segment, &spectrum);
        self.end_index += 1;
    }

    pub fn draw(
        &self,
        ui: &mut egui::Ui,
        figure_painter: &egui::Painter,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        is_selected: bool,
        is_hovered: bool,
    ) -> (egui::Response, egui::Response) {
        let y_top = viewport.screen_space_y(self.descriptor.freq_max());
        let y_bottom = viewport.screen_space_y(self.descriptor.freq_min());
        let x_left = viewport.screen_space_x(self.descriptor.time(self.start_index as f64));
        let x_right = viewport.screen_space_x(self.descriptor.time(self.end_index as f64));

        let draw_list = self
            .segments
            .iter()
            .enumerate()
            .map(move |(seg_idx, segment)| {
                // Calculate the unclamped start and end indices for this segment
                let seg_start_index = self.start_index.div_euclid(TEXTURE_HEIGHT as isize)
                    * TEXTURE_HEIGHT as isize
                    + seg_idx as isize * TEXTURE_HEIGHT as isize;
                let seg_end_index = seg_start_index + TEXTURE_HEIGHT as isize;

                // Calculate v_start and clamp seg_start_index
                let v_start = ((self.start_index - seg_start_index) as f32
                    * (1. / TEXTURE_HEIGHT as f32))
                    .max(0.);
                let clamped_start_index = seg_start_index.max(self.start_index);

                // Calculate v_end and clamp seg_end_index
                let v_end = (1.0
                    - (seg_end_index - self.end_index) as f32 * (1. / TEXTURE_HEIGHT as f32))
                    .min(1.);
                let clamped_end_index = seg_end_index.min(self.end_index);

                let x_start =
                    viewport.screen_space_x(self.descriptor.time(clamped_start_index as f64));
                let x_end = viewport.screen_space_x(self.descriptor.time(clamped_end_index as f64));

                // Get prev and next textures from array
                let prev_texture = if seg_idx > 0 {
                    self.segments[seg_idx - 1].texture.clone()
                } else {
                    self.blank_texture.clone()
                };

                let next_texture = if seg_idx + 1 < self.segments.len() {
                    self.segments[seg_idx + 1].texture.clone()
                } else {
                    self.blank_texture.clone()
                };

                WaterfallDrawInfo {
                    rect: egui::Rect::from_min_max(
                        egui::pos2(x_start, y_top),
                        egui::pos2(x_end, y_bottom),
                    ),
                    texture: segment.texture.clone(),
                    prev_texture,
                    next_texture,
                    min: self.min,
                    max: self.max,
                    v_start,
                    v_end,
                }
            })
            .collect();

        let id = ui.id().with("waterfall");

        // Draw waterfall
        figure_painter.add(egui_wgpu::Callback::new_paint_callback(
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

        // Extend interaction area to include name bar above the clip
        let bar_height = 20.0;
        let clip_rect_with_bar =
            Rect::from_min_max(clip_rect.min - egui::vec2(0.0, bar_height), clip_rect.max);

        // Intersect with figure_rect to prevent interaction outside the visible area
        let clip_interact_rect = clip_rect_with_bar.intersect(figure_rect);

        let clip_interact_id = ui.id().with("clip_interact");
        let response = ui.interact(clip_interact_rect, clip_interact_id, egui::Sense::click());

        // Create separate interaction area for the head bar to enable dragging
        let head_bar_rect = egui::Rect::from_min_max(
            egui::pos2(clip_rect.min.x, clip_rect.min.y - bar_height),
            egui::pos2(clip_rect.max.x, clip_rect.min.y),
        )
        .intersect(figure_rect);

        let head_bar_id = ui.id().with("clip_head_bar");
        let head_bar_response = ui
            .interact(head_bar_rect, head_bar_id, egui::Sense::click_and_drag())
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);

        // Draw border based on selection and hover state
        let stroke = if is_selected {
            // Selected clips get a brighter border
            egui::Stroke::new(2.0, ui.visuals().widgets.active.fg_stroke.color)
        } else if is_hovered {
            // Hovered clips get the standard hover color
            egui::Stroke::new(1.0, ui.visuals().widgets.hovered.bg_stroke.color)
        } else {
            // No border for non-selected, non-hovered clips
            egui::Stroke::NONE
        };

        if stroke != egui::Stroke::NONE {
            figure_painter.rect_stroke(clip_rect, 0.0, stroke, egui::StrokeKind::Outside);

            // Draw name bar above the clip
            let name_bar_rect = egui::Rect::from_min_max(
                egui::pos2(clip_rect.min.x, clip_rect.min.y - bar_height),
                egui::pos2(clip_rect.max.x, clip_rect.min.y),
            )
            .expand(stroke.width);

            // Fill the bar with the border color
            figure_painter.rect_filled(name_bar_rect, 0.0, stroke.color);

            // Calculate visible text area (keep onscreen)
            let visible_bar_rect = name_bar_rect.intersect(figure_rect);

            if visible_bar_rect.width() > 0.0 {
                // Create a rect for left-aligned text with padding
                let padding = 4.0;
                let text_rect = egui::Rect::from_min_max(
                    egui::pos2(visible_bar_rect.min.x + padding, visible_bar_rect.min.y),
                    egui::pos2(visible_bar_rect.max.x - padding, visible_bar_rect.max.y),
                );

                // Draw the clip name
                crate::ui::paint_elided_text(
                    &figure_painter,
                    text_rect,
                    self.descriptor.name.clone(),
                    egui::FontId::proportional(14.0),
                    egui::Color32::BLACK,
                    false,
                    false,
                );
            }
        }

        (response, head_bar_response)
    }
}

#[derive(Debug)]
pub struct ActiveSegment {
    mip_level_count: u32,
    mip_buffer: Vec<f32>,
}

impl ActiveSegment {
    fn new(spectrum_len: u32) -> Self {
        let mip_level_count = TEXTURE_HEIGHT.ilog2().max(1);

        Self {
            mip_level_count,
            // Allocate some extra space in the mip_buffer
            // in case waterfall_row.len() is very small
            mip_buffer: vec![0.; (2 * spectrum_len + mip_level_count) as usize],
        }
    }

    fn push_row(&mut self, queue: &Queue, texture: &Texture, row_index: u32, spectrum: &[f32]) {
        let mut row_idx = row_index;
        let mut row_len = texture.width();
        let mut buffer_offset = 0;
        self.mip_buffer[0..row_len as usize].clone_from_slice(spectrum);
        for mip_level in 0..self.mip_level_count {
            let (row, rest) = self.mip_buffer[buffer_offset..].split_at_mut(row_len as usize);

            // Upload the row data to the GPU
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level,
                    origin: Origin3d {
                        x: 0,
                        y: row_idx,
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
            if row_idx % 2 == 0 {
                // Not ready yet
                break;
            }

            // Ready--loop
            row_idx = row_idx / 2;
            buffer_offset += row_len as usize;
            row_len = next_row_len;
        }
    }
}

struct Segment {
    texture: Texture,
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
