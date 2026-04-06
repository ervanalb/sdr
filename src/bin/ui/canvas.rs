use egui::epaint::MarginF32;
use egui::vec2;
use sdr::analysis::Analysis;
use sdr::band_info::BandsInfo;
use sdr::document::ActiveDocument;
use sdr::document_graphics::DocumentGraphics;
use sdr::format::{format_freq, format_time};
use sdr::hardware::HardwareParams;
use sdr::ui::{Viewport, paint_elided_text};
use std::sync::{Arc, Mutex};

const SCROLL_SPEED: f32 = 1.0;
const WHEEL_ZOOM_SPEED: f32 = 1.0;
const DRAG_ZOOM_SPEED: f32 = 1.01;
const TARGET_GRIDLINE_SEPARATION: f32 = 40.;

const AVAILABLE_FREQUENCY_GRIDLINES: [f64; 15] = [
    1e2, 5e2, 1e3, 5e3, 1e4, 5e4, 1e5, 5e5, 1e6, 5e6, 1e7, 5e7, 1e8, 5e8, 1e9,
];
const AVAILABLE_TIME_GRIDLINES: [f64; 15] = [
    1e-3, 5e-3, 1e-2, 5e-2, 1e-1, 5e-1, 1., 5., 10., 30., 60., 300., 600., 1800., 3600.,
];

const BAR_WIDTH: f32 = 14.;

#[derive(Clone)]
struct RxStreamDragState {
    device_id: String,
    channel_idx: usize,
    proposed_frequency: f64,
}

#[derive(Clone)]
struct ClipDragState {
    clip_id: usize,
    proposed_start_time: f64,
}

pub fn ui(
    ui: &mut egui::Ui,
    viewport: &mut Viewport,
    document: &mut ActiveDocument,
    analysis: &mut Analysis,
    playhead: &mut f64,
    dt: f64,
    hardware_params: &mut HardwareParams,
    bands_info: &BandsInfo,
    is_recording: bool,
    wgpu_render_state: &egui_wgpu::RenderState,
) {
    let highest_freq = bands_info.highest_freq;

    let ui_size = ui.available_size();
    let (ui_rect, response) = ui.allocate_exact_size(ui_size, egui::Sense::click_and_drag());
    let figure_rect = ui_rect
        + MarginF32 {
            left: -154.,
            right: 0.,
            top: -40.,
            bottom: -12.,
        };

    // Define timeline region (above figure)
    let timeline_rect = egui::Rect::from_min_max(
        egui::pos2(figure_rect.left(), ui_rect.top()),
        egui::pos2(figure_rect.right(), figure_rect.top()),
    );
    let timeline_response = ui
        .allocate_rect(timeline_rect, egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::Text);

    // Allocate the figure region
    let figure_response = ui.allocate_rect(figure_rect, egui::Sense::click());

    let figure_size = figure_rect.size();
    let min_scale_y = figure_size.y / highest_freq as f32;
    let min_scale_x = min_scale_y * 1e6; // Difference in dynamic range between default scales of X and Y axes
    let min_scale = vec2(min_scale_x, min_scale_y);
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

    // Handle middle mouse button drag for panning
    if response.dragged_by(egui::PointerButton::Middle) {
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

    //let max_translation_x =
    //    (viewport.scale_x * overall_size.x as f64 - figure_size.x as f64).max(0.0);
    let max_translation_y = (viewport.scale_y * highest_freq - figure_size.y as f64).max(0.0);
    let offset_y = figure_size.y as f64;

    viewport.translation_x = viewport.translation_x.min(0.0);
    viewport.translation_y = viewport
        .translation_y
        .clamp(offset_y, max_translation_y + offset_y);

    let painter = ui.painter().with_clip_rect(ui_rect);
    let figure_painter = ui.painter().with_clip_rect(figure_rect.expand(2.));
    let gridline_stroke = ui.visuals().widgets.noninteractive.bg_stroke;
    let gridline_text_color = ui.visuals().widgets.noninteractive.fg_stroke.color;

    // X-axis gridlines
    {
        let target_gridline_period = TARGET_GRIDLINE_SEPARATION as f64 / viewport.scale_x;
        let i = AVAILABLE_TIME_GRIDLINES
            .partition_point(|&period| period < target_gridline_period as f64);
        let i = i.min(AVAILABLE_TIME_GRIDLINES.len() - 1);
        let period = AVAILABLE_TIME_GRIDLINES[i];
        let precision = period.log10() as i32;
        let left = (viewport.canvas_x(0.) as f64 / period).ceil() as i32;
        let right = (viewport.canvas_x(figure_rect.width()) as f64 / period).floor() as i32;

        for i in left..=right {
            let val = i as f64 * period;
            let x = figure_rect.left() + viewport.screen_space_x(val);

            painter.text(
                egui::pos2(x, figure_rect.top() - 22.),
                egui::Align2::CENTER_BOTTOM,
                format_time(val, precision),
                egui::FontId::proportional(12.),
                gridline_text_color,
            );

            painter.vline(x, figure_rect.top()..=figure_rect.bottom(), gridline_stroke);
        }
    }
    // Y-axis gridlines
    {
        let target_gridline_period = TARGET_GRIDLINE_SEPARATION as f64 / viewport.scale_y;
        let i = AVAILABLE_FREQUENCY_GRIDLINES
            .partition_point(|&period| period < target_gridline_period as f64);
        let i = i.min(AVAILABLE_FREQUENCY_GRIDLINES.len() - 1);
        let period = AVAILABLE_FREQUENCY_GRIDLINES[i];
        let precision = period.log10() as i32;

        let max_freq = viewport.canvas_y(0.);
        let min_freq = viewport.canvas_y(figure_rect.height());

        let bottom = (min_freq / period).ceil() as i32;
        let top = (max_freq / period).floor() as i32;

        for i in bottom..=top {
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
            let current_freq = rx_channel_params.frequency.unwrap();
            let channel_min_freq = current_freq - 0.5 * rx_channel_params.sample_rate.unwrap();
            let channel_max_freq = current_freq + 0.5 * rx_channel_params.sample_rate.unwrap();

            let rect_left = figure_rect.left() - 114.;
            let rect_right = rect_left + BAR_WIDTH;
            // Y axis is flipped: max freq (larger value) has smaller Y pixel coordinate
            let rect_top = figure_rect.top() + viewport.screen_space_y(channel_max_freq);
            let rect_bottom = figure_rect.top() + viewport.screen_space_y(channel_min_freq);
            let rect = egui::Rect {
                min: egui::pos2(rect_left, rect_top),
                max: egui::pos2(rect_right, rect_bottom),
            };
            let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
            let visuals = ui.visuals().widgets.style(&response);

            // Draw the original rect
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
                format!("{} RX stream {}", device_id, channel_idx),
                egui::FontId::proportional(12.),
                visuals.fg_stroke.color,
                true,
                true,
            );

            // Get RX stream drag state from egui memory
            let drag_state_id = ui.id().with("rx_stream_drag_state");
            let mut drag_state: Option<RxStreamDragState> = ui.ctx().memory_mut(|m| {
                m.data
                    .get_temp::<Option<RxStreamDragState>>(drag_state_id)
                    .flatten()
                    .clone()
            });

            // Handle starting drag (disabled during recording)
            if !is_recording && response.drag_started() {
                drag_state = Some(RxStreamDragState {
                    device_id: device_id.clone(),
                    channel_idx,
                    proposed_frequency: current_freq,
                });

                // Store drag state back to egui memory
                ui.ctx().memory_mut(|m| {
                    m.data.insert_temp(drag_state_id, drag_state.clone());
                });
            }

            // Handle active drag
            if let Some(ref mut state) = drag_state
                && &state.device_id == device_id
                && state.channel_idx == channel_idx
            {
                if response.dragged() {
                    let drag = response.drag_delta();
                    // Y axis is flipped, so negate the drag delta
                    let freq_delta = -drag.y as f64 / viewport.scale_y;
                    state.proposed_frequency += freq_delta;
                }

                // Draw ghost outline if this stream is being dragged
                let ghost_min_freq =
                    state.proposed_frequency - 0.5 * rx_channel_params.sample_rate.unwrap();
                let ghost_max_freq =
                    state.proposed_frequency + 0.5 * rx_channel_params.sample_rate.unwrap();
                let ghost_rect_top = figure_rect.top() + viewport.screen_space_y(ghost_max_freq);
                let ghost_rect_bottom = figure_rect.top() + viewport.screen_space_y(ghost_min_freq);
                let ghost_rect = egui::Rect {
                    min: egui::pos2(rect_left, ghost_rect_top),
                    max: egui::pos2(rect_right, ghost_rect_bottom),
                };
                painter.rect_stroke(
                    ghost_rect,
                    visuals.corner_radius,
                    egui::Stroke::new(1.5, egui::Color32::WHITE),
                    egui::StrokeKind::Outside,
                );

                if response.drag_stopped() {
                    // Apply the frequency change
                    if let Some(ref state) = drag_state {
                        *rx_channel_params.frequency.as_mut().unwrap() = state.proposed_frequency;
                    }
                    drag_state = None;
                }

                // Store drag state back to egui memory
                ui.ctx().memory_mut(|m| {
                    m.data.insert_temp(drag_state_id, drag_state.clone());
                });
            }
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
                // Y axis is flipped: max freq (larger value) has smaller Y pixel coordinate
                let rect_top = figure_rect.top() + viewport.screen_space_y(band.max);
                let rect_bottom = figure_rect.top() + viewport.screen_space_y(band.min);
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
                        true,
                    );
                }
            }
        }
    }

    // Get or create DocumentGraphics from egui memory
    let document_graphics_id = ui.id().with("document_graphics");

    let document_graphics = ui.ctx().memory_mut(|m| {
        m.data
            .get_temp_mut_or_default::<Arc<Mutex<DocumentGraphics>>>(document_graphics_id)
            .clone()
    });

    let mut document_graphics = document_graphics.lock().unwrap();

    // Process the document into graphical representation
    document_graphics.process(
        &wgpu_render_state.device,
        &wgpu_render_state.queue,
        &document.document,
        &document.active_clips,
    );

    // Get clip drag state from egui memory
    let clip_drag_state_id = ui.id().with("clip_drag_state");
    let mut clip_drag_state: Option<ClipDragState> = ui.ctx().memory_mut(|m| {
        m.data
            .get_temp::<Option<ClipDragState>>(clip_drag_state_id)
            .flatten()
            .clone()
    });

    // Sort draw_order by hover state (non-hovered first, hovered last)
    let mut sorted_draw_order: Vec<_> = document_graphics
        .draw_order
        .iter()
        .map(|&clip_id| (clip_id, document_graphics.hovered.contains(&clip_id)))
        .collect();
    sorted_draw_order.sort_by_key(|&(_clip_id, is_hovered)| is_hovered);

    document_graphics.hovered.clear();

    for (clip_id, is_hovered) in sorted_draw_order.into_iter() {
        ui.push_id(("clip", clip_id), |ui| {
            let clip = document_graphics.clips.get(&clip_id).unwrap();
            let is_selected = document_graphics.selected.contains(&clip_id);
            let (response, head_bar_response) = clip.draw(
                ui,
                &figure_painter,
                figure_rect,
                viewport,
                clip_id,
                is_selected,
                is_hovered,
            );

            // Draw processor UI on top of clip
            let mut focus_response = response.clone();
            analysis.draw_clip(
                ui,
                &figure_painter,
                figure_rect,
                viewport,
                dt,
                clip_id,
                &mut focus_response,
            );

            // Handle hover
            if focus_response.hovered() {
                document_graphics.hovered.insert(clip_id);
            }

            // Handle click interactions
            if focus_response.clicked() {
                let modifiers = ui.input(|i| i.modifiers);

                // Bring clicked clip to front
                document_graphics.bring_to_top(clip_id);

                if modifiers.shift {
                    // Shift-click: add to selection
                    document_graphics.selected.insert(clip_id);
                } else if modifiers.ctrl || modifiers.command {
                    // Ctrl-click (or Cmd on Mac): toggle in selection
                    if document_graphics.selected.contains(&clip_id) {
                        document_graphics.selected.remove(&clip_id);
                    } else {
                        document_graphics.selected.insert(clip_id);
                    }
                } else {
                    // Regular click: replace selection
                    document_graphics.selected.clear();
                    document_graphics.selected.insert(clip_id);
                }
            }

            // Refresh for the borrow checker
            let clip = document_graphics.clips.get(&clip_id).unwrap();

            // Handle starting drag (disabled during recording)
            if !is_recording && head_bar_response.drag_started() {
                clip_drag_state = Some(ClipDragState {
                    clip_id,
                    proposed_start_time: clip.descriptor.start_time,
                });

                // Store drag state back to egui memory
                ui.ctx().memory_mut(|m| {
                    m.data
                        .insert_temp(clip_drag_state_id, clip_drag_state.clone());
                });
            }

            // Handle active drag
            if let Some(ref mut state) = clip_drag_state
                && state.clip_id == clip_id
            {
                if head_bar_response.dragged() {
                    let drag = head_bar_response.drag_delta();
                    let time_delta = drag.x as f64 / viewport.scale_x;
                    state.proposed_start_time = (state.proposed_start_time + time_delta).max(0.0);
                }

                // Draw ghost outline if this clip is being dragged
                let time_offset = state.proposed_start_time - clip.descriptor.start_time;
                let y_top = viewport.screen_space_y(clip.descriptor.freq_max());
                let y_bottom = viewport.screen_space_y(clip.descriptor.freq_min());
                let x_left = viewport.screen_space_x(clip.descriptor.time(clip.start_index as f64))
                    + time_offset as f32 * viewport.scale_x as f32;
                let x_right = viewport.screen_space_x(clip.descriptor.time(clip.end_index as f64))
                    + time_offset as f32 * viewport.scale_x as f32;

                let ghost_clip_rect = egui::Rect::from_min_max(
                    figure_rect.min + egui::vec2(x_left, y_top),
                    figure_rect.min + egui::vec2(x_right, y_bottom),
                );

                figure_painter.rect_stroke(
                    ghost_clip_rect,
                    0.0,
                    egui::Stroke::new(1.5, egui::Color32::WHITE),
                    egui::StrokeKind::Outside,
                );

                if head_bar_response.drag_stopped() {
                    // Apply the time change
                    if let Some(doc_clip) = document.document.clips.get_mut(&clip_id) {
                        Arc::make_mut(doc_clip).descriptor.start_time = state.proposed_start_time;
                    }
                    clip_drag_state = None;
                }

                // Store drag state back to egui memory
                ui.ctx().memory_mut(|m| {
                    m.data
                        .insert_temp(clip_drag_state_id, clip_drag_state.clone());
                });
            }
        });
    }

    // Draw hover line in X-axis label region (when not recording)
    if !is_recording && timeline_response.hovered() {
        if let Some(pointer_pos) = timeline_response.hover_pos() {
            let hover_x = pointer_pos.x;
            if hover_x >= figure_rect.left() && hover_x <= figure_rect.right() {
                painter.vline(
                    hover_x,
                    figure_rect.top()..=figure_rect.bottom(),
                    egui::Stroke::new(1.0, egui::Color32::GRAY),
                );
            }
        }
    }

    // Draw playhead as a thick vertical line
    let playhead_x = figure_rect.left() + viewport.screen_space_x(*playhead);
    if playhead_x >= figure_rect.left() && playhead_x <= figure_rect.right() {
        let playhead_color = ui.visuals().widgets.noninteractive.fg_stroke.color;
        painter.vline(
            playhead_x,
            figure_rect.top()..=figure_rect.bottom(),
            egui::Stroke::new(2.0, playhead_color),
        );
    }

    // Handle primary button click in X-axis label region to set playhead and deselect clips (only when not recording)
    if !is_recording && timeline_response.clicked_by(egui::PointerButton::Primary) {
        if let Some(pointer_pos) = timeline_response.interact_pointer_pos() {
            let canvas_x = pointer_pos.x - figure_rect.left();
            let time = viewport.canvas_x(canvas_x);
            *playhead = time;
            document_graphics.selected.clear();
        }
    }

    // Clear selection if background is clicked
    if figure_response.clicked_by(egui::PointerButton::Primary) {
        let modifiers = ui.input(|i| i.modifiers);
        if !modifiers.shift && !modifiers.ctrl && !modifiers.command {
            document_graphics.selected.clear();
        }
    }

    // Handle Delete/Backspace to delete selected clips
    let delete_pressed =
        ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace));
    if delete_pressed && !document_graphics.selected.is_empty() {
        // Delete selected clips (skips active clips and keeps them selected)
        document.delete_selection(&mut document_graphics.selected);
    }
}
