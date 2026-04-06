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

/// A custom egui widget for drawing a transmission rectangle on a stream
pub struct StreamTransmission {
    pub start_time: f64,
    pub end_time: f64,
    pub freq_min: f64,
    pub freq_max: f64,
}

#[derive(Clone, Default)]
struct StreamTransmissionState<T> {
    inspector: Option<StreamTransmissionInspector<T>>,
}

#[derive(Clone)]
struct StreamTransmissionInspector<T> {
    pub time: f64,
    pub dragging: bool,
    pub play_lock: bool,
    pub user_data: T,
}

pub struct StreamInspectorParameters {
    pub time: f64,
    pub seek: bool,
    pub play: bool,
}

pub struct StreamInspectorResponse {
    pub time_adj: f64,
}

impl StreamTransmission {
    pub fn new(start_time: f64, end_time: f64, freq_min: f64, freq_max: f64) -> Self {
        Self {
            start_time,
            end_time,
            freq_min,
            freq_max,
        }
    }

    /// Draw the transmission widget with an optional inspector panel
    ///
    /// The inspector_content callback is called when the panel is open, receiving the inspected timestamp.
    pub fn show<F, T>(
        self,
        ui: &mut egui::Ui,
        figure_painter: &egui::Painter,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        dt: f64,
        id: egui::Id,
        mut inspector_content: F,
    ) -> egui::Response
    where
        F: FnMut(&mut egui::Ui, StreamInspectorParameters, &mut T) -> StreamInspectorResponse,
        T: Clone + Default + Send + Sync + 'static,
    {
        // Convert to screen coordinates (X=time, Y=frequency)
        // Y axis is flipped: max freq (larger value) has smaller Y pixel coordinate
        let left = figure_rect.left() + viewport.screen_space_x(self.start_time);
        let right = figure_rect.left() + viewport.screen_space_x(self.end_time);
        let top = figure_rect.top() + viewport.screen_space_y(self.freq_max);
        let bottom = figure_rect.top() + viewport.screen_space_y(self.freq_min);

        // Draw a rectangle around the channel
        let rect = egui::Rect {
            min: egui::pos2(left, top),
            max: egui::pos2(right, bottom),
        };

        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        let visuals = ui.visuals().widgets.style(&response);

        figure_painter.rect_stroke(
            rect,
            visuals.corner_radius,
            visuals.fg_stroke,
            egui::StrokeKind::Outside,
        );

        // Possible memory leak here--
        // consider moving this state to History
        let mut state = ui
            .ctx()
            .data_mut(|d| d.get_temp::<StreamTransmissionState<T>>(id))
            .unwrap_or_default();

        let mut seek = false;

        match &mut state.inspector {
            None => {
                if let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
                    && response.hovered()
                    && ui.ctx().input(|i| i.pointer.primary_down())
                {
                    let x = pointer_pos.x - figure_rect.left();
                    let time = viewport.canvas_x(x);
                    state.inspector = Some(StreamTransmissionInspector {
                        time,
                        dragging: true,
                        play_lock: false,
                        user_data: Default::default(),
                    });
                }
            }
            Some(inspector) => {
                if inspector.dragging {
                    if let Some(pointer_pos) = ui.ctx().pointer_interact_pos() {
                        if !ui.ctx().input(|i| i.pointer.primary_down()) {
                            // Set inspector time on mouse button release
                            if !inspector.play_lock {
                                let x = pointer_pos.x - figure_rect.left();
                                let time = viewport.canvas_x(x);
                                inspector.time = time;
                            }
                            inspector.dragging = false;
                        }
                    }
                } else {
                    if let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
                        && response.hovered()
                        && ui.ctx().input(|i| i.pointer.primary_down())
                    {
                        let x = pointer_pos.x - figure_rect.left();
                        let time = viewport.canvas_x(x);
                        inspector.time = time;
                        inspector.dragging = true;
                        seek = true;
                    }
                }
            }
        }

        // Close inspector if its time is out of bounds
        if let Some(inspector) = &state.inspector
            && (inspector.time < self.start_time || inspector.time > self.end_time)
        {
            state.inspector = None;
        }

        let mut close = false;
        if let Some(inspector) = &mut state.inspector {
            // Draw vertical line across the rectangle in the same color as the outline
            let x = figure_rect.left() + viewport.screen_space_x(inspector.time);
            figure_painter.line_segment(
                [egui::pos2(x, top), egui::pos2(x, bottom)],
                egui::Stroke::new(2.0, visuals.fg_stroke.color),
            );

            // Draw inspector panel to the right of the rectangle
            let panel_pos = egui::pos2(x, bottom + 10.0);
            egui::Area::new(id.with("inspector"))
                .fixed_pos(panel_pos)
                .order(egui::Order::Foreground)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(4.0);
                            let close_button = ui.button("✖");
                            if close_button.clicked() {
                                close = true;
                            }
                            let (enabled, play_text) = if inspector.dragging {
                                (false, "PLAYING")
                            } else {
                                if inspector.play_lock {
                                    (true, "PAUSE")
                                } else {
                                    (true, "PLAY")
                                }
                            };
                            let play_button = ui.add_enabled(enabled, egui::Button::new(play_text));
                            if play_button.clicked() {
                                inspector.play_lock = !inspector.play_lock;
                                seek = true;
                            }
                        });
                        ui.separator();
                        let StreamInspectorResponse { time_adj } = inspector_content(
                            ui,
                            StreamInspectorParameters {
                                time: inspector.time,
                                play: inspector.play_lock || inspector.dragging,
                                seek,
                            },
                            &mut inspector.user_data,
                        );
                        inspector.time += time_adj;
                    });
                });
        }
        // Close inspector if button clicked
        if close {
            state.inspector = None;
        }

        // Advance inspector if play = true
        if let Some(inspector) = &mut state.inspector
            && (inspector.play_lock || inspector.dragging)
        {
            inspector.time += dt;
        }

        ui.ctx().data_mut(|d| d.insert_temp(id, state));
        response
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
