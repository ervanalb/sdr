use std::collections::HashMap;
use std::hash::Hash;
use std::time::Instant;

use eframe::wgpu;
use sdr::waterfall_gpu::ChunkDrawInfo;

use super::waterfall::WaterfallRenderer;

const SCROLL_SPEED: f32 = 1.0;
const WHEEL_ZOOM_SPEED: f32 = 1.0;
const DRAG_ZOOM_SPEED: f32 = 1.01;
const TARGET_GRIDLINE_SEPARATION: f32 = 80.;

const AVAILABLE_FREQUENCY_GRIDLINES: [f64; 15] = [
    1e2, 5e2, 1e3, 5e3, 1e4, 5e4, 1e5, 5e5, 1e6, 5e6, 1e7, 5e7, 1e8, 5e8, 1e9,
];
const AVAILABLE_TIME_GRIDLINES: [f64; 15] = [
    1e-3, 5e-3, 1e-2, 5e-2, 1e-1, 5e-1, 1., 5., 10., 30., 60., 300., 600., 1800., 3600.,
];

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

pub fn init(cc: &eframe::CreationContext<'_>) {
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
    waterfall_chunks: Vec<ChunkDrawInfo>,
    reference_time: Instant,
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

pub struct Viewport {
    pub translation: egui::Vec2,
    pub scale: egui::Vec2,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            translation: egui::Vec2::ZERO,
            scale: egui::vec2(1e-3, 1e3),
        }
    }
}

impl Viewport {
    //fn screen_space(&self, pt: egui::Vec2) -> egui::Vec2 {
    //    pt * self.scale + self.translation
    //}
    fn screen_space_x(&self, x: f32) -> f32 {
        x * self.scale.x + self.translation.x
    }
    fn screen_space_y(&self, y: f32) -> f32 {
        y * self.scale.y + self.translation.y
    }
    fn canvas_x(&self, x: f32) -> f32 {
        (x - self.translation.x) / self.scale.x
    }
    fn canvas_y(&self, y: f32) -> f32 {
        (y - self.translation.y) / self.scale.y
    }
}

pub fn ui(
    ui: &mut egui::Ui,
    id_source: impl Hash + std::fmt::Debug,
    viewport: &mut Viewport,
    waterfall_chunks: Vec<ChunkDrawInfo>,
    reference_time: Instant,
    temp_random_instant: Instant,
    //band_info: BandInfo,
) {
    let id = ui.id().with(&id_source);
    let ui_size = ui.available_size();
    let (ui_rect, response) = ui.allocate_exact_size(ui_size, egui::Sense::click_and_drag());
    let figure_rect = ui_rect
        .clone()
        .with_min_x(ui_rect.min.x + 50.)
        .with_min_y(ui_rect.min.y + 12.);
    let figure_size = figure_rect.size();

    let overall_size = egui::vec2(1e9, 120.);
    let min_scale = figure_size / overall_size;
    let max_zoom = 1e9;

    // Handle scroll and zoom
    if response.hovered() {
        let (scroll_delta, zoom_delta) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));

        // Ctrl + scroll wheel: zoom
        if zoom_delta != 1.0 {
            let old_scale = viewport.scale;
            viewport.scale = viewport.scale * zoom_delta.powf(WHEEL_ZOOM_SPEED);
            viewport.scale = viewport.scale.clamp(min_scale, min_scale * max_zoom);

            // Keep pointer position stationary
            if let Some(pointer_pos) = response.hover_pos() {
                let pointer_canvas = pointer_pos - figure_rect.min;
                viewport.translation = pointer_canvas
                    - (pointer_canvas - viewport.translation) * (viewport.scale / old_scale);
            }
        }
        // Regular scroll: pan the canvas
        viewport.translation += scroll_delta * SCROLL_SPEED;
    }

    // Handle mouse button drag for panning
    if response.dragged_by(egui::PointerButton::Primary) {
        let drag = response.drag_delta();
        viewport.translation += drag;
    }

    // Handle right mouse button drag for zooming
    if response.dragged_by(egui::PointerButton::Secondary) {
        let drag = response.drag_delta();
        let old_scale = viewport.scale;
        viewport.scale =
            old_scale * egui::vec2(DRAG_ZOOM_SPEED.powf(drag.x), DRAG_ZOOM_SPEED.powf(drag.y));
        viewport.scale = viewport.scale.clamp(min_scale, min_scale * max_zoom);
        // Keep pointer position stationary
        if let Some(pointer_pos) = response.hover_pos() {
            let pointer_canvas = pointer_pos - figure_rect.min;
            viewport.translation = pointer_canvas
                - (pointer_canvas - viewport.translation) * (viewport.scale / old_scale);
        }
    }

    // Handle multi-touch gestures
    if let Some(multi_touch) = ui.input(|i| i.multi_touch()) {
        // Pinch to zoom
        if multi_touch.zoom_delta != 1.0 {
            let old_scale = viewport.scale;
            viewport.scale = old_scale * multi_touch.zoom_delta;
            viewport.scale = viewport.scale.clamp(min_scale, min_scale * max_zoom);

            let gesture_center = multi_touch.translation_delta;
            viewport.translation = gesture_center
                - (gesture_center - viewport.translation) * (viewport.scale / old_scale);
        }

        // Two-finger pan
        viewport.translation += multi_touch.translation_delta;
    }

    let max_translation = (viewport.scale * overall_size - figure_size).max(egui::Vec2::ZERO);

    viewport.translation = viewport
        .translation
        .clamp(-max_translation, egui::Vec2::ZERO);

    let painter = ui.painter().with_clip_rect(ui_rect);

    // Vertical gridlines
    {
        let target_gridline_period = TARGET_GRIDLINE_SEPARATION / viewport.scale.x;
        let i = AVAILABLE_FREQUENCY_GRIDLINES
            .partition_point(|&period| period < target_gridline_period as f64);
        let i = i.min(AVAILABLE_FREQUENCY_GRIDLINES.len() - 1);
        let period = AVAILABLE_FREQUENCY_GRIDLINES[i];
        let precision = period.log10() as i32;
        let left = (viewport.canvas_x(0.) as f64 / period).ceil() as i32;
        let right = (viewport.canvas_x(figure_rect.width()) as f64 / period).floor() as i32;

        for i in left..right {
            let val = i as f64 * period;
            let x = figure_rect.left() + viewport.screen_space_x(val as f32);

            painter.text(
                egui::pos2(x, figure_rect.top()),
                egui::Align2::CENTER_BOTTOM,
                format_freq(val, precision),
                egui::FontId::proportional(12.),
                egui::Color32::WHITE,
            );

            painter.vline(x, figure_rect.y_range(), (1., egui::Color32::WHITE));
        }
    }
    // Horizontal gridlines
    {
        let target_gridline_period = TARGET_GRIDLINE_SEPARATION / viewport.scale.y;
        let i = AVAILABLE_TIME_GRIDLINES
            .partition_point(|&period| period < target_gridline_period as f64);
        let i = i.min(AVAILABLE_TIME_GRIDLINES.len() - 1);
        let period = AVAILABLE_TIME_GRIDLINES[i];
        // TODO: Reference everything from day or hour start instead of this random instant
        let offset = reference_time
            .duration_since(temp_random_instant)
            .as_secs_f64();
        let top = ((offset - viewport.canvas_y(0.) as f64) / period).ceil() as i32;
        let bottom =
            ((offset - viewport.canvas_y(figure_rect.height()) as f64) / period).floor() as i32;

        for i in bottom..top {
            let val = i as f64 * period;
            let y = figure_rect.top() + viewport.screen_space_y((offset - val) as f32);

            painter.text(
                egui::pos2(figure_rect.left(), y),
                egui::Align2::RIGHT_CENTER,
                format!("{:.3}", val),
                egui::FontId::proportional(12.),
                egui::Color32::WHITE,
            );

            painter.hline(figure_rect.x_range(), y, (1., egui::Color32::WHITE));
        }
    }

    ui.painter().add(egui_wgpu::Callback::new_paint_callback(
        figure_rect,
        Callback {
            id,
            viewport_size: figure_size,
            translation: viewport.translation,
            scale: viewport.scale,
            waterfall_chunks,
            reference_time,
        },
    ));
}

fn format_freq(freq: f64, precision: i32) -> String {
    if freq < 0. {
        format!("XXX Hz")
    } else if freq < 1e3 {
        format!("{:.*} Hz", (0 - precision).max(0) as usize, freq)
    } else if freq < 1e6 {
        format!("{:.*} kHz", (3 - precision).max(0) as usize, freq * 1e-3)
    } else if freq < 1e9 {
        format!("{:.*} MHz", (6 - precision).max(0) as usize, freq * 1e-6)
    } else if freq < 1e12 {
        format!("{:.*} GHz", (12 - precision).max(0) as usize, freq * 1e-12)
    } else {
        format!("XXX Hz")
    }
}
