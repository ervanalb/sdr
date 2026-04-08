use crate::{
    analysis::{Analysis, ProcessorId},
    document::ClipId,
    processor::{ProcessorParameters, ProcessorUi, SpecificProcessorParameters},
    ui::Viewport,
};
use std::collections::{BTreeMap, BTreeSet};

type ProcessorInstanceId = usize;

pub struct ProcessorGraphics {
    uis: BTreeMap<ProcessorInstanceId, Box<dyn ProcessorUi>>,
    search_text: String,
    filter_enabled: bool,
    filter_has_data: bool,
}

impl ProcessorGraphics {
    pub fn new() -> Self {
        ProcessorGraphics {
            uis: BTreeMap::new(),
            search_text: String::new(),
            filter_enabled: false,
            filter_has_data: false,
        }
    }

    /// Render the entire right sidebar with all processors
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        processor_parameters: &mut BTreeMap<ProcessorId, ProcessorParameters>,
        analysis: &mut Analysis,
        dt: f64,
        delete_confirmation_processor: &mut Option<(ProcessorId, String)>,
    ) {
        ui.add_space(8.0);
        let processors_root_ui_id = ui.id();

        // Header with "Processors" title, search box, and "+" button
        ui.horizontal(|ui| {
            ui.heading("Processors");

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("➕", |ui| {
                    for (type_name, create_fn) in SpecificProcessorParameters::available_types() {
                        if ui.button(type_name).clicked() {
                            // Find the next available processor ID
                            let next_id = processor_parameters
                                .keys()
                                .max()
                                .map(|&id| id + 1)
                                .unwrap_or(1);

                            // Count existing processors of this type to generate unique name
                            let type_count = processor_parameters
                                .values()
                                .filter(|p| p.specific_parameters.type_name() == type_name)
                                .count();

                            let specific_params = create_fn();
                            let name = format!("{} {}", type_name, type_count + 1);

                            processor_parameters.insert(
                                next_id,
                                ProcessorParameters {
                                    name: name.clone(),
                                    enabled: true,
                                    specific_parameters: specific_params,
                                },
                            );

                            // Mark this processor for initial editing and setup
                            ui.data_mut(|d| {
                                d.insert_temp(
                                    processors_root_ui_id
                                        .with(("processor", next_id))
                                        .with("processor_name_editing"),
                                    Some(name),
                                );
                                d.insert_temp(
                                    processors_root_ui_id
                                        .with(("processor", next_id))
                                        .with("processor_setup_open"),
                                    true,
                                );
                            });

                            ui.close();
                        }
                    }
                });

                // Search textbox
                ui.add(
                    egui::TextEdit::singleline(&mut self.search_text)
                        .hint_text("Search...")
                        .desired_width(120.0),
                );
            });
        });

        // Filter row with checkboxes
        ui.horizontal(|ui| {
            // Disable filters when search is active
            let has_search = !self.search_text.is_empty();

            ui.add_enabled_ui(!has_search, |ui| {
                ui.label("Filter:");
                // "Enabled" checkbox
                let mut filter_enabled = self.filter_enabled && !has_search;
                if ui.checkbox(&mut filter_enabled, "enabled").changed() {
                    if filter_enabled {
                        // Checking this unchecks the other
                        self.filter_has_data = false;
                    }
                    self.filter_enabled = filter_enabled;
                }

                // "Has data" checkbox
                let mut filter_has_data = self.filter_has_data && !has_search;
                if ui.checkbox(&mut filter_has_data, "has data").changed() {
                    if filter_has_data {
                        // Checking this unchecks the other
                        self.filter_enabled = false;
                    }
                    self.filter_has_data = filter_has_data;
                }
            });
        });

        ui.separator();

        let mut active_instance_ids = BTreeSet::new();

        // Prepare search term (case-insensitive)
        let search_term_lower = self.search_text.to_lowercase();
        let has_search = !search_term_lower.is_empty();

        egui::ScrollArea::vertical().show(ui, |ui| {
            for (processor_id, parameters) in processor_parameters.iter_mut() {
                let id = processors_root_ui_id.with(("processor", processor_id));
                let setup_id = id.with("processor_setup_open");
                let mut show_setup = ui.data(|d| d.get_temp::<bool>(setup_id).unwrap_or(false));

                // Filter by search term if present
                if !show_setup
                    && has_search
                    && !parameters.name.to_lowercase().contains(&search_term_lower)
                {
                    continue;
                }

                // Apply "enabled" filter
                if !show_setup && !has_search && self.filter_enabled && !parameters.enabled {
                    continue;
                }

                // Apply "has data" filter
                if !show_setup && !has_search && self.filter_has_data {
                    // Check if processor exists in analysis and has data
                    let has_data = analysis
                        .get_processor_instance_and_history_mut(*processor_id)
                        .map(|(_, history)| history.has_data())
                        .unwrap_or(false);

                    if !has_data {
                        continue;
                    }
                }

                ui.push_id(id, |ui| {
                    egui::Frame::group(ui.style())
                        .fill(ui.visuals().window_fill)
                        .show(ui, |ui| {
                            // Header row with checkbox, name, setup toggle, and delete button
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), 26.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.checkbox(&mut parameters.enabled, "");

                                    let name_edit_id = id.with("processor_name_editing");
                                    let editing_name = ui.data(|d| {
                                        d.get_temp::<Option<String>>(name_edit_id).flatten()
                                    });

                                    if let Some(mut temp_name) = editing_name {
                                        // We're in edit mode
                                        let available_width =
                                            (ui.available_width() - 100.0).max(0.); // Leave space for Setup and X buttons
                                        let response = ui.add(
                                            egui::TextEdit::singleline(&mut temp_name)
                                                .desired_width(available_width),
                                        );

                                        let accept = ui.input(|i| i.key_pressed(egui::Key::Enter));
                                        let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape))
                                            || response.lost_focus();

                                        if accept {
                                            parameters.name = temp_name;
                                            ui.data_mut(|d| {
                                                d.insert_temp(name_edit_id, None::<String>)
                                            });
                                        } else if cancel {
                                            ui.data_mut(|d| {
                                                d.insert_temp(name_edit_id, None::<String>)
                                            });
                                        } else {
                                            // Update the temp value and request focus
                                            ui.data_mut(|d| {
                                                d.insert_temp(name_edit_id, Some(temp_name))
                                            });
                                            if !response.has_focus() {
                                                response.request_focus();
                                            }
                                        }
                                    } else {
                                        // Not editing - show as heading
                                        let response = ui.heading(&parameters.name);
                                        if response.clicked() {
                                            ui.data_mut(|d| {
                                                d.insert_temp(
                                                    name_edit_id,
                                                    Some(parameters.name.clone()),
                                                )
                                            });
                                        }
                                    }

                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui.button("🗑").clicked() {
                                                *delete_confirmation_processor =
                                                    Some((*processor_id, parameters.name.clone()));
                                            }

                                            ui.toggle_value(&mut show_setup, "Setup");
                                        },
                                    );
                                },
                            );

                            ui.data_mut(|d| d.insert_temp(setup_id, show_setup));

                            // Draw setup UI if toggle is on
                            if show_setup {
                                egui::Frame::new()
                                    .stroke(egui::Stroke::new(
                                        1.0,
                                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                                    ))
                                    .inner_margin(egui::Margin::same(8))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        parameters.specific_parameters.draw_setup(ui);
                                    });
                            }

                            // Draw history UI if processor is enabled and exists
                            if parameters.enabled
                                && let Some((instance_id, history)) =
                                    analysis.get_processor_instance_and_history_mut(*processor_id)
                            {
                                // Get or create UI for this instance
                                let processor_ui = self
                                    .uis
                                    .entry(instance_id)
                                    .or_insert_with(|| history.new_ui());
                                active_instance_ids.insert(instance_id);

                                // Draw the processor UI
                                processor_ui.draw(history, ui, egui::Id::new(processor_id), dt);
                            }
                        });
                });
            }
        });

        // Clean up UIs for processors that are no longer active
        self.uis
            .retain(|instance_id, _| active_instance_ids.contains(instance_id));
    }

    /// Draw processor UI overlays on the canvas for a specific clip
    pub fn draw_clip(
        &mut self,
        ui: &mut egui::Ui,
        figure_painter: &egui::Painter,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        dt: f64,
        clip_id: ClipId,
        clip_response: &mut egui::Response,
        processor_parameters: &BTreeMap<ProcessorId, ProcessorParameters>,
        analysis: &mut Analysis,
    ) {
        for (processor_id, _parameters) in processor_parameters.iter() {
            if let Some((instance_id, history)) =
                analysis.get_processor_instance_and_history_mut(*processor_id)
                && let Some(processor_ui) = self.uis.get_mut(&instance_id)
            {
                ui.push_id(ui.id().with(("processor", processor_id)), |ui| {
                    processor_ui.draw_clip(
                        history,
                        ui,
                        figure_painter,
                        figure_rect,
                        viewport,
                        dt,
                        clip_id,
                        clip_response,
                    );
                });
            }
        }
    }
}
