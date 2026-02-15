#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use sdr::{Hardware, HardwareParams};

mod ui;

fn main() -> eframe::Result<()> {
    env_logger::init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("SDR"),
        renderer: eframe::Renderer::Wgpu,
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
}

impl SdrApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        ui::canvas::init(cc);
        Self {
            hardware: Some(Hardware::new()),
            hardware_params: HardwareParams::default(),
            viewport_state: ui::canvas::Viewport::default(),
        }
    }
}

impl eframe::App for SdrApp {
    fn on_exit(&mut self) {
        if let Some(hardware) = self.hardware.take() {
            hardware.shutdown();
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Request continuous repaints
        ctx.request_repaint();

        // Update hardware every frame
        if let Some(hardware) = &mut self.hardware {
            hardware.update(&mut self.hardware_params);

            // Poll waterfall messages
            let mut waterfalls = 0;
            while let Some(_msg) = hardware.waterfall_try_recv() {
                //println!(
                //    "Waterfall: device={}, channel={}, t={:?}..{:?} freq={:.2} MHz, width={:.2} MHz, samples={}",
                //    msg.device_id,
                //    msg.channel_index,
                //    msg.start_time,
                //    msg.end_time,
                //    msg.center_frequency / 1e6,
                //    msg.width / 1e6,
                //    msg.waterfall_row.len()
                //);
                waterfalls += 1;
            }
            println!("Got {waterfalls} waterfall messages this frame");
        }

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
                            ui.label(format!("Device: {}", device_id));
                            ui.checkbox(&mut device_params.active, "Active");

                            if device_params.active {
                                ui.separator();

                                for (channel_idx, rx_channel) in
                                    device_params.rx_channels.iter_mut().enumerate()
                                {
                                    ui.collapsing(format!("RX Channel {}", channel_idx), |ui| {
                                        ui.checkbox(&mut rx_channel.active, "Active");

                                        if let Some(frequency) = &mut rx_channel.frequency {
                                            ui.add(
                                                egui::Slider::new(
                                                    frequency,
                                                    rx_channel.frequency_min..=rx_channel.frequency_max,
                                                )
                                                .text("Frequency (Hz)"),
                                            );
                                            ui.label(format!("{:.3} MHz", *frequency / 1e6));
                                        }

                                        if let Some(sample_rate) = &mut rx_channel.sample_rate {
                                            ui.add(
                                                egui::Slider::new(
                                                    sample_rate,
                                                    rx_channel.sample_rate_min..=rx_channel.sample_rate_max,
                                                )
                                                .text("Sample Rate (Hz)")
                                                .logarithmic(true),
                                            );
                                            ui.label(format!(
                                                "{:.3} Msps",
                                                *sample_rate / 1e6
                                            ));
                                        }

                                        if let Some(bandwidth) = &mut rx_channel.bandwidth {
                                            ui.add(
                                                egui::Slider::new(
                                                    bandwidth,
                                                    rx_channel.bandwidth_min..=rx_channel.bandwidth_max,
                                                )
                                                .text("Bandwidth (Hz)")
                                                .logarithmic(true),
                                            );
                                            ui.label(format!("{:.3} MHz", *bandwidth / 1e6));
                                        }
                                    });
                                }
                            }
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

            self::ui::canvas::ui(ui, "canvas", &mut self.viewport_state);
        });
    }
}
