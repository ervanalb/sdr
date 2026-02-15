#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;

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
    viewport_state: ui::canvas::Viewport,
}

impl SdrApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        ui::canvas::init(cc);
        Self {
            viewport_state: ui::canvas::Viewport::default(),
        }
    }
}

impl eframe::App for SdrApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
            .default_width(250.0)
            .show(ctx, |ui| {
                ui.heading("Controls");
                ui.separator();

                ui.group(|ui| {
                    ui.label("Device");
                    ui.horizontal(|ui| {
                        ui.label("Status:");
                        ui.label("Not connected");
                    });
                    if ui.button("Connect").clicked() {
                        // Placeholder
                    }
                });

                ui.add_space(10.0);

                ui.group(|ui| {
                    ui.label("Frequency");
                    ui.add(egui::Slider::new(&mut 100.0, 0.0..=1000.0).text("MHz"));
                });

                ui.add_space(10.0);

                ui.group(|ui| {
                    ui.label("Gain");
                    ui.add(egui::Slider::new(&mut 50.0, 0.0..=100.0).text("%"));
                });

                ui.add_space(10.0);

                ui.group(|ui| {
                    ui.label("Sample Rate");
                    ui.add(egui::Slider::new(&mut 2.4, 0.1..=20.0).text("Msps"));
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Waterfall Display");
            ui.separator();

            self::ui::canvas::ui(ui, "canvas", &mut self.viewport_state);
        });
    }
}
