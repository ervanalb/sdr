use egui::Stroke;
use egui::epaint::TextShape;
use sdr::analysis::Analysis;
use sdr::band_info::BandsInfo;
use sdr::document::Document;
use sdr::format::{format_freq, format_time};
use sdr::hardware::HardwareParams;
use sdr::ui::Viewport;

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

const BAR_WIDTH: f32 = 14.;

fn paint_elided_text(
    painter: &egui::Painter,
    rect: egui::Rect,
    text: String,
    font_id: egui::FontId,
    color: egui::Color32,
    rotated: bool,
) {
    let rect_width = if rotated { rect.height() } else { rect.width() };

    let galley = painter.layout_no_wrap(text.clone(), font_id.clone(), color);
    let text_width = galley.rect.width();

    let galley = if text_width > rect_width {
        let ellipsis = painter.layout_no_wrap("...".to_string(), font_id.clone(), color);
        let ellipsis_width = ellipsis.rect.width();
        let available_width = rect_width - ellipsis_width;

        if available_width > 0.0 {
            let mut truncated_text = text.clone();
            while !truncated_text.is_empty() {
                let test_galley =
                    painter.layout_no_wrap(truncated_text.clone(), font_id.clone(), color);
                if test_galley.rect.width() <= available_width {
                    break;
                }
                truncated_text.pop();
            }
            let combined = format!("{}...", truncated_text);
            let final_galley = painter.layout_no_wrap(combined, font_id, color);
            Some(final_galley)
        } else {
            None
        }
    } else {
        Some(galley)
    };

    let Some(galley) = galley else {
        return;
    };

    let galley_center = if rotated {
        egui::vec2(galley.rect.height() / 2.0, -galley.rect.width() / 2.0)
    } else {
        egui::vec2(galley.rect.width() / 2.0, galley.rect.height() / 2.0)
    };

    let angle = if rotated {
        -0.25 * std::f32::consts::TAU
    } else {
        0.
    };

    let shape = TextShape {
        pos: rect.center() - galley_center,
        galley,
        underline: Stroke::NONE,
        fallback_color: color,
        override_text_color: None,
        opacity_factor: 1.,
        angle,
    };
    painter.add(shape);
}

pub fn ui(
    ui: &mut egui::Ui,
    viewport: &mut Viewport,
    _document: &Document,
    analysis: &Analysis,
    _playhead: &mut f64,
    dt: f64,
    hardware_params: &mut HardwareParams,
    bands_info: &BandsInfo,
) {
    let highest_freq = bands_info.highest_freq;

    let ui_size = ui.available_size();
    let (ui_rect, response) = ui.allocate_exact_size(ui_size, egui::Sense::click_and_drag());
    let figure_rect = ui_rect
        .clone()
        .with_min_x(ui_rect.min.x + 154.)
        .with_min_y(ui_rect.min.y + 24.);
    let figure_size = figure_rect.size();

    let overall_size = egui::vec2(120., highest_freq as f32);
    let min_scale = figure_size / overall_size;
    let max_zoom = 1e9;

    // Handle scroll and zoom
    if ui.rect_contains_pointer(ui_rect) {
        let (scroll_delta, zoom_delta, pointer_pos) = ui.input(|i| {
            (
                i.smooth_scroll_delta,
                i.zoom_delta(),
                i.pointer.latest_pos(),
            )
        });

        // Ctrl + scroll wheel: zoom
        if zoom_delta != 1.0 {
            let old_scale_x = viewport.scale_x;
            let old_scale_y = viewport.scale_y;
            let zoom = zoom_delta.powf(WHEEL_ZOOM_SPEED) as f64;
            viewport.scale_x *= zoom;
            viewport.scale_y *= zoom;
            viewport.scale_x = viewport
                .scale_x
                .clamp(min_scale.x as f64, (min_scale.x * max_zoom) as f64);
            viewport.scale_y = viewport
                .scale_y
                .clamp(min_scale.y as f64, (min_scale.y * max_zoom) as f64);

            // Keep pointer position stationary
            if let Some(pointer_pos) = pointer_pos {
                let pointer_canvas = pointer_pos - figure_rect.min;
                viewport.translation_x = pointer_canvas.x as f64
                    - (pointer_canvas.x as f64 - viewport.translation_x)
                        * (viewport.scale_x / old_scale_x);
                viewport.translation_y = pointer_canvas.y as f64
                    - (pointer_canvas.y as f64 - viewport.translation_y)
                        * (viewport.scale_y / old_scale_y);
            }
        }
        // Regular scroll: pan the canvas
        viewport.translation_x += (scroll_delta.x * SCROLL_SPEED) as f64;
        viewport.translation_y += (scroll_delta.y * SCROLL_SPEED) as f64;
    }

    // Handle mouse button drag for panning
    if response.dragged_by(egui::PointerButton::Primary) {
        let drag = response.drag_delta();
        viewport.translation_x += drag.x as f64;
        viewport.translation_y += drag.y as f64;
    }

    // Handle right mouse button drag for zooming
    if response.dragged_by(egui::PointerButton::Secondary) {
        let pointer_pos = ui.input(|i| i.pointer.latest_pos());
        let drag = response.drag_delta();
        let old_scale_x = viewport.scale_x;
        let old_scale_y = viewport.scale_y;
        viewport.scale_x *= DRAG_ZOOM_SPEED.powf(drag.x) as f64;
        viewport.scale_y *= DRAG_ZOOM_SPEED.powf(drag.y) as f64;
        viewport.scale_x = viewport
            .scale_x
            .clamp(min_scale.x as f64, (min_scale.x * max_zoom) as f64);
        viewport.scale_y = viewport
            .scale_y
            .clamp(min_scale.y as f64, (min_scale.y * max_zoom) as f64);
        // Keep pointer position stationary
        if let Some(pointer_pos) = pointer_pos {
            let pointer_canvas = pointer_pos - figure_rect.min;
            viewport.translation_x = pointer_canvas.x as f64
                - (pointer_canvas.x as f64 - viewport.translation_x)
                    * (viewport.scale_x / old_scale_x);
            viewport.translation_y = pointer_canvas.y as f64
                - (pointer_canvas.y as f64 - viewport.translation_y)
                    * (viewport.scale_y / old_scale_y);
        }
    }

    // Handle multi-touch gestures
    if let Some(multi_touch) = ui.input(|i| i.multi_touch()) {
        // Pinch to zoom
        if multi_touch.zoom_delta != 1.0 {
            let old_scale_x = viewport.scale_x;
            let old_scale_y = viewport.scale_y;
            let zoom = multi_touch.zoom_delta as f64;
            viewport.scale_x *= zoom;
            viewport.scale_y *= zoom;
            viewport.scale_x = viewport
                .scale_x
                .clamp(min_scale.x as f64, (min_scale.x * max_zoom) as f64);
            viewport.scale_y = viewport
                .scale_y
                .clamp(min_scale.y as f64, (min_scale.y * max_zoom) as f64);

            let gesture_center = multi_touch.translation_delta;
            viewport.translation_x = gesture_center.x as f64
                - (gesture_center.x as f64 - viewport.translation_x)
                    * (viewport.scale_x / old_scale_x);
            viewport.translation_y = gesture_center.y as f64
                - (gesture_center.y as f64 - viewport.translation_y)
                    * (viewport.scale_y / old_scale_y);
        }

        // Two-finger pan
        viewport.translation_x += multi_touch.translation_delta.x as f64;
        viewport.translation_y += multi_touch.translation_delta.y as f64;
    }

    let max_translation_x =
        (viewport.scale_x * overall_size.x as f64 - figure_size.x as f64).max(0.0);
    let max_translation_y =
        (viewport.scale_y * overall_size.y as f64 - figure_size.y as f64).max(0.0);

    viewport.translation_x = viewport.translation_x.clamp(-max_translation_x, 0.0);
    viewport.translation_y = viewport.translation_y.clamp(-max_translation_y, 0.0);

    let painter = ui.painter().with_clip_rect(ui_rect);
    let gridline_stroke = ui.visuals().widgets.noninteractive.bg_stroke;
    let gridline_text_color = ui.visuals().widgets.noninteractive.fg_stroke.color;

    // Vertical gridlines (time)
    {
        let target_gridline_period = TARGET_GRIDLINE_SEPARATION as f64 / viewport.scale_x;
        let i = AVAILABLE_TIME_GRIDLINES
            .partition_point(|&period| period < target_gridline_period as f64);
        let i = i.min(AVAILABLE_TIME_GRIDLINES.len() - 1);
        let period = AVAILABLE_TIME_GRIDLINES[i];
        let precision = period.log10() as i32;
        let left = (viewport.canvas_x(0.) as f64 / period).ceil() as i32;
        let right = (viewport.canvas_x(figure_rect.width()) as f64 / period).floor() as i32;

        for i in left..right {
            let val = i as f64 * period;
            let x = figure_rect.left() + viewport.screen_space_x(val);

            painter.text(
                egui::pos2(x, figure_rect.top() - 6.),
                egui::Align2::CENTER_BOTTOM,
                format_time(val, precision),
                egui::FontId::proportional(12.),
                gridline_text_color,
            );

            painter.vline(x, figure_rect.top()..=figure_rect.bottom(), gridline_stroke);
        }
    }
    // Horizontal gridlines (frequency)
    {
        let target_gridline_period = TARGET_GRIDLINE_SEPARATION as f64 / viewport.scale_y;
        let i = AVAILABLE_FREQUENCY_GRIDLINES
            .partition_point(|&period| period < target_gridline_period as f64);
        let i = i.min(AVAILABLE_FREQUENCY_GRIDLINES.len() - 1);
        let period = AVAILABLE_FREQUENCY_GRIDLINES[i];
        let precision = period.log10() as i32;

        let top_freq = viewport.canvas_y(0.);
        let bottom_freq = viewport.canvas_y(figure_rect.height());

        let top = (top_freq / period).ceil() as i32;
        let bottom = (bottom_freq / period).floor() as i32;

        for i in top..bottom {
            let val = i as f64 * period;
            let y = figure_rect.top() + viewport.screen_space_y(val);

            painter.text(
                egui::pos2(figure_rect.left() - 18., y),
                egui::Align2::RIGHT_CENTER,
                format_freq(val, precision),
                egui::FontId::proportional(12.),
                gridline_text_color,
            );

            painter.hline(figure_rect.left()..=figure_rect.right(), y, gridline_stroke);
        }
    }

    // RX Streams
    for (device_id, device_params) in &mut hardware_params.devices {
        for (channel_idx, rx_channel_params) in device_params.rx_streams.iter_mut().enumerate() {
            let channel_min_freq =
                rx_channel_params.frequency.unwrap() - 0.5 * rx_channel_params.sample_rate.unwrap();
            let channel_max_freq =
                rx_channel_params.frequency.unwrap() + 0.5 * rx_channel_params.sample_rate.unwrap();

            let rect_left = figure_rect.left() - 114.;
            let rect_right = rect_left + BAR_WIDTH;
            let rect_top = figure_rect.top() + viewport.screen_space_y(channel_min_freq);
            let rect_bottom = figure_rect.top() + viewport.screen_space_y(channel_max_freq);
            let rect = egui::Rect {
                min: egui::pos2(rect_left, rect_top),
                max: egui::pos2(rect_right, rect_bottom),
            };
            let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
            let visuals = ui.visuals().widgets.style(&response);
            painter.rect(
                rect,
                visuals.corner_radius,
                visuals.bg_fill,
                visuals.fg_stroke,
                egui::StrokeKind::Outside,
            );
            if response.dragged_by(egui::PointerButton::Primary) {
                let drag = response.drag_delta();
                *rx_channel_params.frequency.as_mut().unwrap() += drag.y as f64 / viewport.scale_y;
            }
            paint_elided_text(
                &painter,
                rect.intersect(ui_rect),
                format!("{} RX stream {}", device_id, channel_idx),
                egui::FontId::proportional(12.),
                visuals.fg_stroke.color,
                true,
            );
        }
    }

    // Bands
    {
        let visuals = ui.visuals().widgets.noninteractive;
        for (bands_or_allocations, offset) in
            [(&bands_info.bands, 150.), (&bands_info.allocations, 132.)]
        {
            for band in bands_or_allocations {
                let rect_left = figure_rect.left() - offset;
                let rect_right = figure_rect.left() - offset + BAR_WIDTH;
                let rect_top = figure_rect.top() + viewport.screen_space_y(band.min);
                let rect_bottom = figure_rect.top() + viewport.screen_space_y(band.max);
                let rect = egui::Rect {
                    min: egui::pos2(rect_left, rect_top),
                    max: egui::pos2(rect_right, rect_bottom),
                };
                if rect.intersects(ui_rect) {
                    painter.rect(
                        rect,
                        visuals.corner_radius,
                        visuals.bg_fill,
                        visuals.fg_stroke,
                        egui::StrokeKind::Outside,
                    );
                    paint_elided_text(
                        &painter,
                        rect.intersect(ui_rect),
                        band.description.clone(),
                        egui::FontId::proportional(12.),
                        visuals.fg_stroke.color,
                        true,
                    );
                }
            }
        }
    }

    // Draw processors
    analysis.draw(ui, figure_rect, viewport, dt);
}
