use crate::duration_ext::DurationExt;
use chrono::{DateTime, Duration, TimeDelta, Utc};

/// A custom egui widget for drawing a transmission rectangle on a stream
pub struct StreamTransmission {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub freq_min: f32,
    pub freq_max: f32,
}

#[derive(Clone, Default)]
struct StreamTransmissionState {
    inspector: Option<StreamTransmissionInspector>,
}

#[derive(Clone)]
struct StreamTransmissionInspector {
    pub time: DateTime<Utc>,
    pub dragging: bool,
    pub play: bool,
}

impl StreamTransmission {
    pub fn new(
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        freq_min: f32,
        freq_max: f32,
    ) -> Self {
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
    pub fn show<F>(
        self,
        ui: &mut egui::Ui,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        dt: TimeDelta,
        id: egui::Id,
        mut inspector_content: F,
    ) -> egui::Response
    where
        F: FnMut(&mut egui::Ui, DateTime<Utc>),
    {
        // Convert to screen coordinates
        let left = figure_rect.left() + viewport.screen_space_x(self.freq_min);
        let right = figure_rect.left() + viewport.screen_space_x(self.freq_max);
        let bottom = figure_rect.top() + viewport.screen_space_y(self.start_time);
        let top = figure_rect.top() + viewport.screen_space_y(self.end_time);

        // Draw a rectangle around the channel
        let rect = egui::Rect {
            min: egui::pos2(left, top),
            max: egui::pos2(right, bottom),
        };

        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        let visuals = ui.visuals().widgets.style(&response);
        let painter = ui.painter().with_clip_rect(figure_rect);

        painter.rect_stroke(
            rect,
            visuals.corner_radius,
            visuals.fg_stroke,
            egui::StrokeKind::Outside,
        );

        let mut state = ui
            .ctx()
            .data_mut(|d| d.get_temp::<StreamTransmissionState>(id))
            .unwrap_or_default();

        match &mut state.inspector {
            None => {
                if let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
                    && response.hovered()
                    && ui.ctx().input(|i| i.pointer.primary_down())
                {
                    let y = pointer_pos.y - figure_rect.top();
                    let time = viewport.canvas_y(y);
                    state.inspector = Some(StreamTransmissionInspector {
                        time,
                        dragging: true,
                        play: false,
                    });
                }
            }
            Some(inspector) => {
                if inspector.dragging {
                    if let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
                        && ui.ctx().input(|i| i.pointer.primary_down())
                    {
                        if !viewport.is_live {
                            let y = pointer_pos.y - figure_rect.top();
                            let time = viewport.canvas_y(y);
                            inspector.time = time;
                        }
                    } else {
                        inspector.dragging = false;
                    }
                } else {
                    if let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
                        && response.hovered()
                        && ui.ctx().input(|i| i.pointer.primary_down())
                    {
                        let y = pointer_pos.y - figure_rect.top();
                        let time = viewport.canvas_y(y);
                        inspector.time = time;
                        inspector.dragging = true;
                    }
                }
            }
        }

        // Advance inspector if play = true
        if let Some(inspector) = &mut state.inspector
            && (inspector.play || inspector.dragging && viewport.is_live)
        {
            inspector.time += dt;
        }

        // Close inspector if its time is out of bounds
        if let Some(inspector) = &state.inspector
            && (inspector.time < self.start_time || inspector.time > self.end_time)
        {
            state.inspector = None;
        }

        let mut close = false;
        if let Some(inspector) = &mut state.inspector {
            // Draw horizontal line across the rectangle in the same color as the outline
            let y = figure_rect.top() + viewport.screen_space_y(inspector.time);
            painter.line_segment(
                [egui::pos2(left, y), egui::pos2(right, y)],
                egui::Stroke::new(2.0, visuals.fg_stroke.color),
            );

            // Draw inspector panel to the right of the rectangle
            let panel_pos = egui::pos2(right + 10.0, y);
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
                                if viewport.is_live {
                                    (false, "PLAYING")
                                } else {
                                    (true, "PAUSED")
                                }
                            } else {
                                if inspector.play {
                                    (true, "PAUSE")
                                } else {
                                    (true, "PLAY")
                                }
                            };
                            let play_button = ui.add_enabled(enabled, egui::Button::new(play_text));
                            if play_button.clicked() {
                                inspector.play = !inspector.play;
                            }
                        });
                        ui.separator();
                        inspector_content(ui, inspector.time);
                    });
                });
        }
        // Close inspector if button clicked
        if close {
            state.inspector = None;
        }

        ui.ctx().data_mut(|d| d.insert_temp(id, state));
        response
    }
}

pub struct Viewport {
    pub translation: egui::Vec2,
    pub scale: egui::Vec2,
    pub reference_time: DateTime<Utc>,
    pub is_live: bool,
}

impl Viewport {
    pub fn new(reference_time: DateTime<Utc>) -> Self {
        Self {
            translation: egui::Vec2::ZERO,
            scale: egui::vec2(1e-3, 1e3),
            reference_time,
            is_live: false,
        }
    }

    pub fn update_reference_time(&mut self, reference_time: DateTime<Utc>, force_live: bool) {
        let dt = reference_time
            .signed_duration_since(self.reference_time)
            .as_seconds_f32();
        if force_live {
            self.translation.y = 0.
        }
        self.is_live = self.translation.y >= 0.;
        if !self.is_live {
            // Auto-scroll to keep viewport stationary
            self.translation.y -= self.scale.y * dt;
        }
        self.reference_time = reference_time;
    }

    pub fn screen_space_x(&self, x: f32) -> f32 {
        x * self.scale.x + self.translation.x
    }
    fn screen_space_y_secs(&self, y: f32) -> f32 {
        -y * self.scale.y + self.translation.y
    }
    pub fn screen_space_y(&self, y: DateTime<Utc>) -> f32 {
        self.screen_space_y_secs(
            y.signed_duration_since(self.reference_time)
                .as_seconds_f32(),
        )
    }
    pub fn canvas_x(&self, x: f32) -> f32 {
        (x - self.translation.x) / self.scale.x
    }
    fn canvas_y_secs(&self, y: f32) -> f32 {
        -(y - self.translation.y) / self.scale.y
    }
    pub fn canvas_y(&self, y: f32) -> DateTime<Utc> {
        self.reference_time + Duration::from_secs_f64(self.canvas_y_secs(y) as f64)
    }
}
