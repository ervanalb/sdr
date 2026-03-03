#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use num_complex::Complex;
use sdr::band_info::BandsInfo;
use sdr::channels_gpu::ChannelsGpu;
use sdr::dsp::Rechunker;
use sdr::hardware::{Hardware, HardwareParams, ReceiveStreamId};
use sdr::processor::Processor;
use sdr::waterfall_gpu::WaterfallGpu;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod ui;

const STREAM_TARGET_BIN_SIZE: f64 = 2_500.0; // 2.5 KHz
const STREAM_TARGET_OUTPUT_PERIOD: f64 = 0.01; // 100 chunks per second
const STREAM_MIN_MAX_TIME_CONSTANT: f64 = 1.;
const STREAM_OFFSET_REJECT_TIME_CONSTANT: f64 = 0.1;
//const CHANNEL_MESSAGE_CAPACITY: usize = 32768;

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
    viewport_state: ui::canvas::Viewport,
    waterfall_gpu: WaterfallGpu,
    channels_gpu: ChannelsGpu,
    reference_time: Instant,
    prev_reference_time: Instant,
    temp_random_instant: Instant,
    bands_info: Arc<Mutex<BandsInfo>>,
    streams: BTreeMap<ReceiveStreamId, (Rechunker<Complex<f32>>, Processor)>,
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
        let bands_info = Arc::new(Mutex::new(bands_info));

        Self {
            hardware: Some(Hardware::new()),
            hardware_params: HardwareParams::default(),
            viewport_state: ui::canvas::Viewport::default(),
            waterfall_gpu: WaterfallGpu::new(device),
            channels_gpu: ChannelsGpu::new(),
            reference_time: now,
            prev_reference_time: now,
            temp_random_instant: now,
            bands_info,
            streams: BTreeMap::new(),
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

        let Some(wgpu_render_state) = frame.wgpu_render_state() else {
            return;
        };

        let Some(hardware) = &mut self.hardware else {
            return;
        };

        self.prev_reference_time = self.reference_time;
        if self.hardware_params.run {
            self.reference_time = Instant::now();
        }

        // Update hardware every frame
        hardware.update(&mut self.hardware_params);

        // TODO: consider moving rechunker & processor into Hardware

        // Remove / close any streams that no longer exist
        self.streams.retain(|&k, _| {
            if !hardware.receive_streams.contains_key(k) {
                self.waterfall_gpu.close_stream(k);
                return false;
            }
            true
        });

        // Process all streams
        for (stream_id, stream) in hardware.receive_streams.iter_mut() {
            let (rechunker, processor) = self.streams.entry(stream_id).or_insert_with(|| {
                let channels = { &self.bands_info.lock().unwrap().channels };
                let processor = Processor::new(
                    stream.descriptor.frequency,
                    stream.descriptor.sample_rate,
                    STREAM_TARGET_BIN_SIZE,
                    STREAM_TARGET_OUTPUT_PERIOD,
                    STREAM_MIN_MAX_TIME_CONSTANT,
                    STREAM_OFFSET_REJECT_TIME_CONSTANT,
                    stream.descriptor.start_time,
                    channels,
                );
                let rechunker = Rechunker::new(processor.chunk_size());
                (rechunker, processor)
            });

            while let Some(message) = stream.try_recv() {
                rechunker.process(&message.iq_data, |chunk| {
                    processor.process(&chunk, message.time);

                    self.waterfall_gpu.add_row(
                        stream_id,
                        &stream.descriptor,
                        message.time,
                        &processor.spectrum,
                        processor.min,
                        processor.max,
                        &wgpu_render_state.device,
                        &wgpu_render_state.queue,
                    );

                    for (channel_id, channel) in processor.channels.iter() {
                        self.channels_gpu
                            .add_chunk(stream_id, channel_id, channel, message.time);
                    }
                });
            }
        }
        self.waterfall_gpu
            .prune_old_textures(self.reference_time - Duration::from_secs_f64(CANVAS_DURATION));
        self.channels_gpu
            .prune(self.reference_time - Duration::from_secs_f64(CANVAS_DURATION));

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
            let force_live = self.hardware_params.run && !prev_run;
            self::ui::canvas::ui(
                ui,
                "canvas",
                &mut self.viewport_state,
                &self.waterfall_gpu,
                &self.channels_gpu,
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
