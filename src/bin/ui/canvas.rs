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

fn paint_elided_text(
    painter: &egui::Painter,
    rect: egui::Rect,
    text: String,
    font_id: egui::FontId,
    color: egui::Color32,
) {
    let galley = painter.layout_no_wrap(text.clone(), font_id.clone(), color);
    let text_width = galley.rect.width();

    if text_width > rect.width() {
        let ellipsis = painter.layout_no_wrap("...".to_string(), font_id.clone(), color);
        let ellipsis_width = ellipsis.rect.width();
        let available_width = rect.width() - ellipsis_width;

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
            painter.galley(
                rect.center()
                    - egui::vec2(
                        final_galley.rect.width() / 2.0,
                        final_galley.rect.height() / 2.0,
                    ),
                final_galley,
                color,
            );
        }
    } else {
        painter.galley(
            rect.center() - egui::vec2(galley.rect.width() / 2.0, galley.rect.height() / 2.0),
            galley,
            color,
        );
    }
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
        .with_min_x(ui_rect.min.x + 48.)
        .with_min_y(ui_rect.min.y + 100.);
    let figure_size = figure_rect.size();

    let overall_size = egui::vec2(highest_freq as f32, 120.);
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
            let old_scale = viewport.scale;
            viewport.scale = viewport.scale * zoom_delta.powf(WHEEL_ZOOM_SPEED);
            viewport.scale = viewport.scale.clamp(min_scale, min_scale * max_zoom);

            // Keep pointer position stationary
            if let Some(pointer_pos) = pointer_pos {
                let old_translation = viewport.translation;
                let pointer_canvas = pointer_pos - figure_rect.min;
                viewport.translation = pointer_canvas
                    - (pointer_canvas - viewport.translation) * (viewport.scale / old_scale);
                if old_translation.y >= 0. {
                    viewport.translation.y = old_translation.y;
                }
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
        let pointer_pos = ui.input(|i| i.pointer.latest_pos());
        let drag = response.drag_delta();
        let old_scale = viewport.scale;
        viewport.scale =
            old_scale * egui::vec2(DRAG_ZOOM_SPEED.powf(drag.x), DRAG_ZOOM_SPEED.powf(drag.y));
        viewport.scale = viewport.scale.clamp(min_scale, min_scale * max_zoom);
        // Keep pointer position stationary
        if let Some(pointer_pos) = pointer_pos {
            let old_translation = viewport.translation;
            let pointer_canvas = pointer_pos - figure_rect.min;
            viewport.translation = pointer_canvas
                - (pointer_canvas - viewport.translation) * (viewport.scale / old_scale);
            if old_translation.y >= 0. {
                viewport.translation.y = old_translation.y;
            }
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
    let gridline_stroke = ui.visuals().widgets.noninteractive.bg_stroke;
    let gridline_text_color = ui.visuals().widgets.noninteractive.fg_stroke.color;

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
                egui::pos2(x, figure_rect.top() - 6.),
                egui::Align2::CENTER_BOTTOM,
                format_freq(val, precision),
                egui::FontId::proportional(12.),
                gridline_text_color,
            );

            painter.vline(
                x,
                (figure_rect.top() - 4.)..=figure_rect.bottom(),
                gridline_stroke,
            );
        }
    }
    // Horizontal gridlines
    {
        let target_gridline_period = TARGET_GRIDLINE_SEPARATION / viewport.scale.y;
        let i = AVAILABLE_TIME_GRIDLINES
            .partition_point(|&period| period < target_gridline_period as f64);
        let i = i.min(AVAILABLE_TIME_GRIDLINES.len() - 1);
        let period = AVAILABLE_TIME_GRIDLINES[i];
        let precision = period.log10() as i32;

        let top_time = viewport.canvas_y(0.);
        let bottom_time = viewport.canvas_y(figure_rect.height());

        let top = (top_time / period).ceil() as i32;
        let bottom = (bottom_time / period).floor() as i32;

        for i in bottom..top {
            let val = i as f64 * period;
            let y = figure_rect.top() + viewport.screen_space_y(val);

            painter.text(
                egui::pos2(figure_rect.left() - 6., y),
                egui::Align2::RIGHT_CENTER,
                format_time(val, precision),
                egui::FontId::proportional(12.),
                gridline_text_color,
            );

            painter.hline(
                (figure_rect.left() - 4.)..=figure_rect.right(),
                y,
                gridline_stroke,
            );
        }
    }

    // RX Streams
    for (device_id, device_params) in &mut hardware_params.devices {
        for (channel_idx, rx_channel_params) in device_params.rx_streams.iter_mut().enumerate() {
            let channel_left_freq =
                rx_channel_params.frequency.unwrap() - 0.5 * rx_channel_params.sample_rate.unwrap();
            let channel_right_freq =
                rx_channel_params.frequency.unwrap() + 0.5 * rx_channel_params.sample_rate.unwrap();

            let rect_left = figure_rect.left() + viewport.screen_space_x(channel_left_freq as f32);
            let rect_right =
                figure_rect.left() + viewport.screen_space_x(channel_right_freq as f32);
            let rect_bottom = figure_rect.top() - 26.;
            let rect_top = figure_rect.top() - 42.;
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
                *rx_channel_params.frequency.as_mut().unwrap() +=
                    (drag.x / viewport.scale.x) as f64;
            }
            paint_elided_text(
                &painter,
                rect.intersect(ui_rect),
                format!("{} RX stream {}", device_id, channel_idx),
                egui::FontId::proportional(12.),
                visuals.fg_stroke.color,
            );
        }
    }

    // Bands
    {
        let visuals = ui.visuals().widgets.noninteractive;
        for (bands_or_allocations, offset) in
            [(&bands_info.bands, 64.), (&bands_info.allocations, 46.)]
        {
            for band in bands_or_allocations {
                let rect_left = figure_rect.left() + viewport.screen_space_x(band.min as f32);
                let rect_right = figure_rect.left() + viewport.screen_space_x(band.max as f32);
                let rect_bottom = figure_rect.top() - offset;
                let rect_top = figure_rect.top() - offset - 14.;
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
                    );
                }
            }
        }
    }

    // Draw processors
    analysis.draw(ui, figure_rect, viewport, dt);
}
