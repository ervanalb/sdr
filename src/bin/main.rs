#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chrono::{DateTime, Utc};
use eframe::egui;
use sdr::analysis::{Analysis, ProcessorId};
use sdr::band_info::BandsInfo;
use sdr::document::{ActiveDocument, RecordingId};
use sdr::document_graphics::DocumentGraphics;
use sdr::hardware::{Hardware, HardwareParams};
use sdr::processor::ProcessorParameters;
use sdr::processor_graphics::ProcessorGraphics;
use sdr::ui::Viewport;
use std::collections::BTreeMap;
use std::rc::Rc;

mod ui;

/// How often to check if processor parameters need to be saved (in seconds)
const PROCESSOR_AUTOSAVE_INTERVAL_SECONDS: i64 = 10;

/// Length of the loop buffer in seconds
const LOOP_LENGTH: f64 = 10.0;

#[derive(Debug, Clone)]
enum PlaybackAction {
    Play,
    Record(Rc<RecordingId>),
    PlayAndRecord(Rc<RecordingId>),
}

#[derive(Debug, Clone)]
struct PlaybackState {
    r#loop: bool,
    action: PlaybackAction,
}

#[derive(Debug, Clone)]
enum PendingLoopAction {
    Play,
    Record,
    PlayAndRecord,
}

fn get_processors_path() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let config_dir = dirs::data_local_dir()
        .ok_or("Could not find local data directory")?
        .join("sdr");

    std::fs::create_dir_all(&config_dir)?;
    Ok(config_dir.join("processors.json"))
}

fn save_processors(
    processors: &BTreeMap<ProcessorId, ProcessorParameters>,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = get_processors_path()?;
    let json = serde_json::to_string_pretty(processors)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn load_processors()
-> Result<BTreeMap<ProcessorId, ProcessorParameters>, Box<dyn std::error::Error>> {
    let path = get_processors_path()?;
    let data = std::fs::read_to_string(path)?;
    let processors: BTreeMap<ProcessorId, ProcessorParameters> = serde_json::from_str(&data)?;
    Ok(processors)
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("SDR"),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            wgpu_setup: eframe::egui_wgpu::WgpuSetup::CreateNew(
                eframe::egui_wgpu::WgpuSetupCreateNew {
                    device_descriptor: std::sync::Arc::new(|_| wgpu::DeviceDescriptor {
                        required_features: wgpu::Features::FLOAT32_FILTERABLE,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
            ..Default::default()
        },
        ..Default::default()
    };

    let result = eframe::run_native(
        "sdr-gui",
        native_options,
        Box::new(|cc| Ok(Box::new(SdrApp::new(cc)))),
    );

    result
}

struct SdrApp {
    hardware: Option<Hardware>,
    hardware_params: HardwareParams,
    viewport_state: Viewport,
    processor_parameters: BTreeMap<ProcessorId, ProcessorParameters>,
    last_saved_processors: BTreeMap<ProcessorId, ProcessorParameters>,
    last_save_check: DateTime<Utc>,
    document: ActiveDocument,
    document_graphics: DocumentGraphics,
    playback_state: Option<PlaybackState>,
    loop_enabled: bool,
    loop_confirmation_pending: Option<PendingLoopAction>,
    analysis: Analysis,
    processor_graphics: ProcessorGraphics,
    prev_time: DateTime<Utc>,
    bands_info: BandsInfo,
    playhead: f64,
    delete_confirmation_processor: Option<(ProcessorId, String)>,
}

impl SdrApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        sdr::document_graphics::static_init(cc);
        let wgpu_render_state = cc.wgpu_render_state.as_ref().unwrap();
        let now = Utc::now();

        // Load bands info from JSON file included at compile time
        const BANDS_JSON: &str = include_str!("../../bands.json");
        let bands_info: BandsInfo = serde_json::from_str(BANDS_JSON).unwrap();

        // Try to load processors from disk, or use defaults
        let processor_parameters = load_processors().unwrap_or_default();

        Self {
            hardware: Some(Hardware::new()),
            hardware_params: HardwareParams::default(),
            viewport_state: Viewport::new(),
            last_saved_processors: processor_parameters.clone(),
            processor_parameters,
            last_save_check: now,
            document: ActiveDocument::new(),
            document_graphics: DocumentGraphics::new(),
            playback_state: None,
            loop_enabled: false,
            loop_confirmation_pending: None,
            analysis: Analysis::new(&wgpu_render_state.device, &wgpu_render_state.queue),
            processor_graphics: ProcessorGraphics::new(),
            prev_time: now,
            bands_info,
            playhead: 0.,
            delete_confirmation_processor: None,
        }
    }
}

impl eframe::App for SdrApp {
    fn on_exit(&mut self) {
        // Save processors before exiting
        if let Err(e) = save_processors(&self.processor_parameters) {
            eprintln!("Failed to save processors on exit: {}", e);
        }

        if let Some(hardware) = self.hardware.take() {
            hardware.shutdown();
        }
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Request continuous repaints
        ctx.request_repaint();

        let Some(hardware) = &mut self.hardware else {
            return;
        };

        let wgpu_render_state = frame.wgpu_render_state().unwrap();

        let now = Utc::now();
        let dt_duration = now.signed_duration_since(self.prev_time);
        let dt = dt_duration.as_seconds_f64();
        self.prev_time = now;

        // Periodically check if we need to save processors
        let save_check_duration = now.signed_duration_since(self.last_save_check);
        if save_check_duration.num_seconds() >= PROCESSOR_AUTOSAVE_INTERVAL_SECONDS {
            self.last_save_check = now;
            if self.processor_parameters != self.last_saved_processors {
                if let Err(e) = save_processors(&self.processor_parameters) {
                    eprintln!("Failed to save processors: {}", e);
                } else {
                    self.last_saved_processors = self.processor_parameters.clone();
                }
            }
        }

        // Update hardware every frame
        let hardware_results = hardware.update(&mut self.hardware_params);

        // Add new content to the document
        if let Some(state) = &self.playback_state {
            let recording_id = match &state.action {
                PlaybackAction::Record(id) | PlaybackAction::PlayAndRecord(id) => Some(id),
                PlaybackAction::Play => None,
            };

            if let Some(id) = recording_id {
                self.document.update_recording(id, hardware_results);
            }

            // Advance playhead during any playback state
            self.playhead += dt;
        }

        // Update document
        self.document.update();

        // Process the document into graphical representation
        self.document_graphics.process(
            &wgpu_render_state.device,
            &wgpu_render_state.queue,
            &self.document.document,
            &self.document.active_clips,
        );

        // Analyze the document with all processors
        self.analysis.process(
            &mut self.processor_parameters,
            &self.document.document,
            &self.document.active_clips,
        );

        // Handle expiration (only in loop mode)
        if let Some(state) = &self.playback_state {
            if state.r#loop {
                let retain_time = self.playhead - LOOP_LENGTH;
                self.document.expire(retain_time);
                self.document_graphics
                    .process_expiry(&self.document.document, retain_time);
                self.analysis
                    .process_expiry(&self.document.document, retain_time);
            }
        }

        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New").clicked() {
                        self.document = ActiveDocument::new();
                        ui.close();
                    }
                    if ui.button("Open...").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("SDR Document", &["sdr"])
                            .pick_file()
                        {
                            match sdr::document::ActiveDocument::load_from_file(&path) {
                                Ok(document) => {
                                    self.document = document;
                                    ui.close();
                                }
                                Err(e) => {
                                    eprintln!("Failed to load document: {}", e);
                                }
                            }
                        }
                    }
                    if ui.button("Save As...").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("SDR Document", &["sdr"])
                            .save_file()
                        {
                            if let Err(e) = self.document.save_to_file(&path) {
                                eprintln!("Failed to save document: {}", e);
                            }
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("View", |ui| {
                    ui.label("Placeholder menu");
                });
            });
        });

        egui::SidePanel::left("left_sidebar")
            .default_width(300.0)
            .frame(egui::Frame::side_top_panel(&ctx.style()).fill(egui::Color32::from_gray(40)))
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("Hardware Control");
                ui.separator();

                if ui.button("Enumerate Devices").clicked() {
                    self.hardware_params.enumerate = true;
                }

                ui.add_space(10.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    let device_ids: Vec<String> =
                        self.hardware_params.devices.keys().cloned().collect();
                    let has_devices = !device_ids.is_empty();

                    for device_id in &device_ids {
                        let device_params =
                            self.hardware_params.devices.get_mut(device_id).unwrap();

                        egui::Frame::group(ui.style())
                            .fill(ui.visuals().window_fill)
                            .show(ui, |ui| {
                                ui.push_id(device_id, |ui| {
                                    ui.label(format!("Device: {}", device_id));
                                    ui.checkbox(&mut device_params.active, "Active");

                                    if device_params.active {
                                        ui.separator();

                                        for (stream_index, stream) in
                                            device_params.rx_streams.iter_mut().enumerate()
                                        {
                                            ui.collapsing(
                                                format!("RX Stream {}", stream_index),
                                                |ui| {
                                                    ui.checkbox(&mut stream.active, "Active");

                                                    if let Some(frequency) = &mut stream.frequency {
                                                        ui.add(
                                                            egui::Slider::new(
                                                                frequency,
                                                                stream.frequency_min
                                                                    ..=stream.frequency_max,
                                                            )
                                                            .text("Frequency (Hz)"),
                                                        );
                                                        ui.label(format!(
                                                            "{:.3} MHz",
                                                            *frequency / 1e6
                                                        ));
                                                    }

                                                    if let Some(sample_rate) =
                                                        &mut stream.sample_rate
                                                    {
                                                        ui.add(
                                                            egui::Slider::new(
                                                                sample_rate,
                                                                stream.sample_rate_min
                                                                    ..=stream.sample_rate_max,
                                                            )
                                                            .text("Sample Rate (Hz)")
                                                            .logarithmic(true),
                                                        );
                                                        ui.label(format!(
                                                            "{:.3} Msps",
                                                            *sample_rate / 1e6
                                                        ));
                                                    }

                                                    if let Some(bandwidth) = &mut stream.bandwidth {
                                                        ui.add(
                                                            egui::Slider::new(
                                                                bandwidth,
                                                                stream.bandwidth_min
                                                                    ..=stream.bandwidth_max,
                                                            )
                                                            .text("Bandwidth (Hz)")
                                                            .logarithmic(true),
                                                        );
                                                        ui.label(format!(
                                                            "{:.3} MHz",
                                                            *bandwidth / 1e6
                                                        ));
                                                    }

                                                    // Gain controls
                                                    if !stream.gains.is_empty() {
                                                        ui.separator();
                                                        ui.label("Gains:");

                                                        let mut gain_names: Vec<String> =
                                                            stream.gains.keys().cloned().collect();
                                                        gain_names.sort();

                                                        for gain_name in gain_names {
                                                            let gain = stream
                                                                .gains
                                                                .get_mut(&gain_name)
                                                                .unwrap();
                                                            ui.add(
                                                                egui::Slider::new(
                                                                    &mut gain.value,
                                                                    gain.min..=gain.max,
                                                                )
                                                                .text(&gain_name),
                                                            );
                                                            ui.label(format!(
                                                                "{:.1} dB",
                                                                gain.value
                                                            ));
                                                        }
                                                    }

                                                    // Peak meter
                                                    // TODO: fix by moving into hardware
                                                    /*
                                                    let mut peak = None;
                                                    for active_stream in
                                                        self.stream_history.active_streams.values()
                                                    {
                                                        if &active_stream.descriptor.device_id
                                                            == device_id
                                                            && active_stream.descriptor.stream_index
                                                                == stream_index
                                                        {
                                                            peak = Some((
                                                                active_stream.peak,
                                                                active_stream.overload,
                                                            ));
                                                        }
                                                    }
                                                    if let Some((peak, overload)) = peak {
                                                        ui.label(format!(
                                                            "Peak: {:.1} dBFS {}",
                                                            20. * peak.log10(),
                                                            if overload { "O" } else { "" }
                                                        ));
                                                    }
                                                    */
                                                },
                                            );
                                        }
                                    }
                                });
                            });

                        ui.add_space(10.0);
                    }

                    if !has_devices {
                        ui.label("No devices enumerated. Click 'Enumerate Devices' to scan.");
                    }
                });
            });

        egui::SidePanel::right("right_sidebar")
            .default_width(450.0)
            .frame(egui::Frame::side_top_panel(&ctx.style()).fill(egui::Color32::from_gray(40)))
            .show(ctx, |ui| {
                self.processor_graphics.ui(
                    ui,
                    &mut self.processor_parameters,
                    &mut self.analysis,
                    dt,
                    &mut self.delete_confirmation_processor,
                );
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let wgpu_render_state = frame.wgpu_render_state().unwrap();

            // Playback control buttons at the top
            let mut scroll_to_playhead = false;

            ui.horizontal(|ui| {
                ui.add_space(4.0);

                let button_size = egui::vec2(40.0, 40.0);

                let is_recording = self.playback_state.as_ref().map_or(false, |state| {
                    matches!(
                        state.action,
                        PlaybackAction::Record(_) | PlaybackAction::PlayAndRecord(_)
                    )
                });

                ui.add_enabled_ui(!is_recording, |ui| {
                    if ui.add_sized(button_size, egui::Button::new("⏮")).clicked() {
                        // Seek to beginning: find earliest clip start time
                        if let Some(time) = self.document.document.earliest_time() {
                            self.playhead = time;
                            scroll_to_playhead = true;
                        }
                    }

                    if ui.add_sized(button_size, egui::Button::new("⏭")).clicked() {
                        // Seek to end: find latest clip end time
                        if let Some(time) = self.document.document.latest_time() {
                            self.playhead = time;
                            scroll_to_playhead = true;
                        }
                    }
                });

                let record_button = egui::Button::new("⏺").fill(if is_recording {
                    ui.visuals().selection.bg_fill
                } else {
                    ui.visuals().widgets.inactive.weak_bg_fill
                });

                if ui.add_sized(button_size, record_button).clicked() {
                    // Toggle record
                    match &self.playback_state {
                        None => {
                            // Starting new recording from None
                            if self.loop_enabled && !self.document.document.clips.is_empty() {
                                // Show confirmation modal only if document has clips
                                self.loop_confirmation_pending = Some(PendingLoopAction::Record);
                            } else {
                                let recording_id = self.document.record(now, self.playhead);
                                self.playback_state = Some(PlaybackState {
                                    r#loop: self.loop_enabled,
                                    action: PlaybackAction::Record(recording_id),
                                });
                            }
                        }
                        Some(state) => {
                            self.playback_state = match &state.action {
                                PlaybackAction::Play => {
                                    let recording_id = self.document.record(now, self.playhead);
                                    Some(PlaybackState {
                                        r#loop: state.r#loop,
                                        action: PlaybackAction::PlayAndRecord(recording_id),
                                    })
                                }
                                PlaybackAction::Record(_) => {
                                    self.loop_enabled = false;
                                    None
                                }
                                PlaybackAction::PlayAndRecord(_) => Some(PlaybackState {
                                    r#loop: state.r#loop,
                                    action: PlaybackAction::Play,
                                }),
                            };
                        }
                    };
                }

                if ui.add_sized(button_size, egui::Button::new("⏹")).clicked() {
                    // Stop
                    self.loop_enabled = false;
                    self.playback_state = None;
                }

                let is_playing = self.playback_state.as_ref().map_or(false, |state| {
                    matches!(
                        state.action,
                        PlaybackAction::Play | PlaybackAction::PlayAndRecord(_)
                    )
                });

                let play_button = egui::Button::new("▶").fill(if is_playing {
                    ui.visuals().selection.bg_fill
                } else {
                    ui.visuals().widgets.inactive.weak_bg_fill
                });

                if ui.add_sized(button_size, play_button).clicked() {
                    // Toggle play
                    match &self.playback_state {
                        None => {
                            // Starting new playback from None
                            if self.loop_enabled && !self.document.document.clips.is_empty() {
                                // Show confirmation modal only if document has clips
                                self.loop_confirmation_pending = Some(PendingLoopAction::Play);
                            } else {
                                self.playback_state = Some(PlaybackState {
                                    r#loop: self.loop_enabled,
                                    action: PlaybackAction::Play,
                                });
                            }
                        }
                        Some(state) => {
                            self.playback_state = match &state.action {
                                PlaybackAction::Play => {
                                    self.loop_enabled = false;
                                    None
                                }
                                PlaybackAction::Record(id) => Some(PlaybackState {
                                    r#loop: state.r#loop,
                                    action: PlaybackAction::PlayAndRecord(id.clone()),
                                }),
                                PlaybackAction::PlayAndRecord(id) => Some(PlaybackState {
                                    r#loop: state.r#loop,
                                    action: PlaybackAction::Record(id.clone()),
                                }),
                            };
                        }
                    };
                }

                ui.add_enabled_ui(self.playback_state.is_none(), |ui| {
                    let loop_button = egui::Button::new("🔁").fill(if self.loop_enabled {
                        ui.visuals().selection.bg_fill
                    } else {
                        ui.visuals().widgets.inactive.weak_bg_fill
                    });

                    if ui.add_sized(button_size, loop_button).clicked() {
                        // Toggle loop (only when not playing/recording)
                        self.loop_enabled = !self.loop_enabled;
                    }
                });
            });

            let playback_enabled = self.playback_state.is_some();
            let loop_active = self
                .playback_state
                .as_ref()
                .map_or(false, |state| state.r#loop);

            self::ui::canvas::ui(
                ui,
                &mut self.viewport_state,
                &mut self.document,
                &mut self.document_graphics,
                &mut self.analysis,
                &mut self.playhead,
                dt,
                &mut self.hardware_params,
                &self.bands_info,
                playback_enabled,
                loop_active,
                scroll_to_playhead,
                &wgpu_render_state,
                &mut self.processor_graphics,
                &self.processor_parameters,
            );
        });

        // Loop mode confirmation modal
        if let Some(pending_action) = self.loop_confirmation_pending.clone() {
            egui::Window::new("Enable Loop Mode")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("Loop mode will clear all existing content.");
                    ui.label("Do you want to proceed?");
                    ui.add_space(10.0);

                    ui.horizontal(|ui| {
                        if ui.button("No").clicked() {
                            self.loop_confirmation_pending = None;
                        }

                        if ui.button("Yes").clicked() {
                            // Clear document and reset playhead
                            self.document = ActiveDocument::new();
                            self.playhead = 0.0;

                            // Start playback/recording with loop enabled
                            self.playback_state = match pending_action {
                                PendingLoopAction::Play => Some(PlaybackState {
                                    r#loop: true,
                                    action: PlaybackAction::Play,
                                }),
                                PendingLoopAction::Record => {
                                    let recording_id = self.document.record(now, 0.0);
                                    Some(PlaybackState {
                                        r#loop: true,
                                        action: PlaybackAction::Record(recording_id),
                                    })
                                }
                                PendingLoopAction::PlayAndRecord => {
                                    let recording_id = self.document.record(now, 0.0);
                                    Some(PlaybackState {
                                        r#loop: true,
                                        action: PlaybackAction::PlayAndRecord(recording_id),
                                    })
                                }
                            };

                            self.loop_confirmation_pending = None;
                        }
                    });
                });
        }

        // Delete confirmation modal
        if let Some((processor_id, processor_name)) = self.delete_confirmation_processor.clone() {
            egui::Window::new("Delete Processor")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Are you sure you want to delete '{}'?",
                        processor_name
                    ));
                    ui.add_space(10.0);

                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.delete_confirmation_processor = None;
                        }

                        if ui.button("Delete").clicked() {
                            self.processor_parameters.remove(&processor_id);
                            self.delete_confirmation_processor = None;
                        }
                    });
                });
        }
    }
}
