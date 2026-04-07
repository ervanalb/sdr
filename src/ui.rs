pub fn paint_elided_text(
    painter: &egui::Painter,
    rect: egui::Rect,
    text: String,
    font_id: egui::FontId,
    color: egui::Color32,
    centered: bool,
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

    let galley_anchor = match (centered, rotated) {
        (false, false) => egui::vec2(0., galley.rect.height() / 2.0),
        (false, true) => todo!(),
        (true, false) => egui::vec2(galley.rect.width() / 2.0, galley.rect.height() / 2.0),
        (true, true) => egui::vec2(galley.rect.height() / 2.0, -galley.rect.width() / 2.0),
    };

    let rect_anchor = match centered {
        false => rect.left_center(),
        true => rect.center(),
    };

    let angle = if rotated {
        -0.25 * std::f32::consts::TAU
    } else {
        0.
    };

    let shape = egui::epaint::TextShape {
        pos: rect_anchor - galley_anchor,
        galley,
        underline: egui::Stroke::NONE,
        fallback_color: color,
        override_text_color: None,
        opacity_factor: 1.,
        angle,
    };
    painter.add(shape);
}

/// Inspector state that can be stored in processor history to share across transmissions
#[derive(Clone)]
pub struct TransmissionInspectorState {
    pub transmission_id: usize,
    pub time: f64,
    pub play_temp: Option<f64>, // value = seek on release
    pub play_lock: bool,
}

/// Response from stream_transmission_ui
pub struct StreamTransmissionResponse {
    pub response: egui::Response,
    pub pressed_at: Option<f64>,
}

/// Draw a transmission rectangle on a stream.
///
/// Returns a response containing the egui response and optionally the time where the transmission was pressed.
pub fn stream_transmission_ui(
    start_time: f64,
    end_time: f64,
    freq_min: f64,
    freq_max: f64,
    playhead: Option<f64>,
    ui: &mut egui::Ui,
    figure_painter: &egui::Painter,
    figure_rect: egui::Rect,
    viewport: &Viewport,
) -> StreamTransmissionResponse {
    // Convert to screen coordinates (X=time, Y=frequency)
    // Y axis is flipped: max freq (larger value) has smaller Y pixel coordinate
    let left = figure_rect.left() + viewport.screen_space_x(start_time);
    let right = figure_rect.left() + viewport.screen_space_x(end_time);
    let top = figure_rect.top() + viewport.screen_space_y(freq_max);
    let bottom = figure_rect.top() + viewport.screen_space_y(freq_min);

    // Draw a rectangle around the channel
    let rect = egui::Rect {
        min: egui::pos2(left, top),
        max: egui::pos2(right, bottom),
    };

    let interact_id = ui.id().with("transmission_interact");
    let response = ui.interact(rect, interact_id, egui::Sense::click_and_drag());

    let visuals = ui.visuals().widgets.style(&response);

    figure_painter.rect_stroke(
        rect,
        visuals.corner_radius,
        visuals.fg_stroke,
        egui::StrokeKind::Outside,
    );

    // Check if the transmission was pressed and calculate the time
    let pressed_at = if let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
        && response.hovered()
        && ui.ctx().input(|i| i.pointer.primary_pressed())
    {
        let x = pointer_pos.x - figure_rect.left();
        Some(viewport.canvas_x(x))
    } else {
        None
    };

    // Draw vertical playhead line if provided
    if let Some(playhead_time) = playhead {
        let x = figure_rect.left() + viewport.screen_space_x(playhead_time);
        figure_painter.line_segment(
            [egui::pos2(x, top), egui::pos2(x, bottom)],
            egui::Stroke::new(2.0, visuals.fg_stroke.color),
        );
    }

    StreamTransmissionResponse {
        response,
        pressed_at,
    }
}

#[derive(Clone, Debug)]
pub struct Viewport {
    pub translation_x: f64,
    pub translation_y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
}

impl Viewport {
    pub fn new() -> Self {
        Self {
            translation_x: 0.0,
            translation_y: 0.0,
            scale_x: 1e3,  // X is time
            scale_y: 1e-3, // Y is frequency
        }
    }

    // X axis is time
    pub fn screen_space_x(&self, time: f64) -> f32 {
        (time * self.scale_x + self.translation_x) as f32
    }
    // Y axis is frequency (negated so high frequencies are at top)
    pub fn screen_space_y(&self, freq: f64) -> f32 {
        (-freq * self.scale_y + self.translation_y) as f32
    }
    pub fn canvas_x(&self, x: f32) -> f64 {
        (x as f64 - self.translation_x) / self.scale_x
    }
    pub fn canvas_y(&self, y: f32) -> f64 {
        -(y as f64 - self.translation_y) / self.scale_y
    }
}
