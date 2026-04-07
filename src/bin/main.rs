#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chrono::{DateTime, Utc};
use eframe::egui;
use sdr::analysis::{Analysis, ProcessorId};
use sdr::band_info::BandsInfo;
use sdr::document::{ActiveDocument, RecordingId};
use sdr::hardware::{Hardware, HardwareParams};
use sdr::processor::fm::FmProcessorParameters;
use sdr::processor::{ProcessorParameters, SpecificProcessorParameters};
use sdr::ui::Viewport;
use std::collections::BTreeMap;
use std::rc::Rc;

mod ui;

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
    document: ActiveDocument,
    recording: Option<Rc<RecordingId>>,
    analysis: Analysis,
    prev_time: DateTime<Utc>,
    bands_info: BandsInfo,
    playhead: f64,
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

        let mut processor_parameters = BTreeMap::<ProcessorId, ProcessorParameters>::new();

        let tmp_freq = 90.9e6;

        processor_parameters.insert(
            1,
            ProcessorParameters {
                name: "FM Demodulator".to_string(),
                enabled: true,
                specific_parameters: SpecificProcessorParameters::Fm(FmProcessorParameters {
                    frequency: tmp_freq,
                    bandwidth: 200e3,
                    squelch_db: -100.,
                    squelch_hysteresis_db: 3.,
                }),
            },
        );

        Self {
            hardware: Some(Hardware::new()),
            hardware_params: HardwareParams::default(),
            viewport_state: Viewport::new(),
            processor_parameters,
            document: ActiveDocument::new(),
            recording: None,
            analysis: Analysis::new(&wgpu_render_state.device, &wgpu_render_state.queue),
            prev_time: now,
            bands_info,
            playhead: 0.,
        }
    }
}

impl eframe::App for SdrApp {
    fn on_exit(&mut self) {
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

        let now = Utc::now();
        let dt_duration = now.signed_duration_since(self.prev_time);
        let dt = dt_duration.as_seconds_f64();
        self.prev_time = now;

        // Update hardware every frame
        let hardware_results = hardware.update(&mut self.hardware_params);

        // Add new content to the document
        if let Some(recording_id) = &self.recording {
            self.document
                .update_recording(recording_id, hardware_results);

            // Advance playhead during recording
            self.playhead += dt;
        }

        // Update document
        self.document.update();

        // Expire old chunks
        // TODO: bring back expire
        //self.document.expire(todo!());

        // Document graphics processing now happens in canvas.rs

        self.analysis.process(
            &mut self.processor_parameters,
            &self.document.document,
            &self.document.active_clips,
        );

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
            .show(ctx, |ui| {
                ui.heading("Hardware Control");
                ui.separator();

                let mut record_enabled = self.recording.is_some();
                if ui.checkbox(&mut record_enabled, "Record").changed() {
                    if record_enabled && self.recording.is_none() {
                        self.recording = Some(self.document.record(now, self.playhead));
                    } else if !record_enabled {
                        self.recording = None;
                    }
                }

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

                        ui.group(|ui| {
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

                                                if let Some(sample_rate) = &mut stream.sample_rate {
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
                                                        ui.label(format!("{:.1} dB", gain.value));
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
            .default_width(350.0)
            .show(ctx, |ui| {
                ui.heading("Processors");
                ui.separator();

                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.analysis.draw(ui, &mut self.processor_parameters, dt);
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let wgpu_render_state = frame.wgpu_render_state().unwrap();

            self::ui::canvas::ui(
                ui,
                &mut self.viewport_state,
                &mut self.document,
                &mut self.analysis,
                &mut self.playhead,
                dt,
                &mut self.hardware_params,
                &self.bands_info,
                self.recording.is_some(),
                &wgpu_render_state,
            );
        });
    }
}
