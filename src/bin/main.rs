#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::time::{Duration, Instant};

use eframe::egui;
use sdr::band_info::BandsInfo;
use sdr::hardware::{Hardware, HardwareParams};
use sdr::waterfall_gpu::WaterfallGpu;

mod ui;

const CANVAS_DURATION: f64 = 120.;

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

    eframe::run_native(
        "sdr-gui",
        native_options,
        Box::new(|cc| Ok(Box::new(SdrApp::new(cc)))),
    )
}

struct SdrApp {
    hardware: Option<Hardware>,
    hardware_params: HardwareParams,
    viewport_state: ui::canvas::Viewport,
    waterfall_gpu: WaterfallGpu,
    reference_time: Instant,
    prev_reference_time: Instant,
    temp_random_instant: Instant,
    bands_info: BandsInfo,
    iq_writer: BufWriter<File>,
}

impl SdrApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        ui::canvas::init(cc);
        let device = &cc.wgpu_render_state.as_ref().unwrap().device;
        let now = Instant::now();

        // Load bands info from JSON file included at compile time
        const BANDS_JSON: &str = include_str!("../../bands.json");
        let bands_info: BandsInfo = serde_json::from_str(BANDS_JSON).unwrap();

        // TEMP: write IQ samples to a file
        let file = File::create("iq_samples.raw").expect("Failed to create iq_samples.raw");
        let iq_writer = BufWriter::new(file);

        Self {
            hardware: Some(Hardware::new()),
            hardware_params: HardwareParams::default(),
            viewport_state: ui::canvas::Viewport::default(),
            waterfall_gpu: WaterfallGpu::new(device),
            reference_time: now,
            prev_reference_time: now,
            temp_random_instant: now,
            bands_info,
            iq_writer,
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
        self.prev_reference_time = self.reference_time;
        if self.hardware_params.run {
            self.reference_time = Instant::now();
        }

        // Update hardware every frame
        if let Some(hardware) = &mut self.hardware {
            hardware.update(&mut self.hardware_params);

            // Upload waterfall messages to GPU
            if let Some(wgpu_render_state) = frame.wgpu_render_state() {
                let device = &wgpu_render_state.device;
                let queue = &wgpu_render_state.queue;

                while let Some(msg) = hardware.stream_try_recv() {
                    self.waterfall_gpu.add_row(&msg, device, queue);
                }

                while let Some(msg) = hardware.channel_try_recv() {
                    for sample in msg.iq_data.iter() {
                        self.iq_writer.write_all(&sample.re.to_le_bytes()).ok();
                        self.iq_writer.write_all(&sample.im.to_le_bytes()).ok();
                    }
                }
                self.iq_writer.flush().ok();
            }
            self.waterfall_gpu
                .prune_old_textures(self.reference_time - Duration::from_secs_f64(CANVAS_DURATION));
        }

        let prev_run = self.hardware_params.run;
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
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

                ui.checkbox(&mut self.hardware_params.run, "Run");

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

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Waterfall Display");
            ui.separator();

            // Get draw list from waterfall GPU
            let waterfall_chunks = self.waterfall_gpu.draw_list().collect();

            let force_live = self.hardware_params.run && !prev_run;
            self::ui::canvas::ui(
                ui,
                "canvas",
                &mut self.viewport_state,
                waterfall_chunks,
                self.reference_time,
                self.reference_time.duration_since(self.prev_reference_time),
                self.temp_random_instant,
                force_live,
                &mut self.hardware_params,
                &self.bands_info,
            );
        });
    }
}
