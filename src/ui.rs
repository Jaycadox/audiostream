use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use cpal::{
    traits::{DeviceTrait, HostTrait},
    Host,
};
use eframe::egui::{self, menu::SubMenu, CollapsingHeader};
use egui_plot::{Line, Plot, PlotBounds, PlotPoints};

use crate::{
    audio::{AudioEncoding, ConnectionSettings, Mixer, NoiseGate},
    server::{start, AudioUpdate, Server, ServerState, UiUpdate},
};

pub struct MyApp {
    listen_str: String,
    servers: Vec<Server>,
    current_settings: ConnectionSettings,
    host: Arc<Host>,
    cached_input_device_names: Vec<String>,
    cached_output_device_names: Vec<String>,
    mixer: Arc<Mutex<Mixer>>,
}

impl MyApp {
    pub fn new() -> Self {
        let host = Arc::new(cpal::default_host());

        let mixer = Arc::new(Mutex::new(Mixer { effects: vec![] }));
        println!("Selected host: {}", host.id().name());
        Self {
            listen_str: String::new(),
            servers: vec![],
            current_settings: ConnectionSettings::new(&host, &mixer),
            host,
            cached_input_device_names: vec![],
            cached_output_device_names: vec![],
            mixer,
        }
    }
}
impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            // Define custom percentages for left and right columns
            let left_percentage = 0.7; // 70% of the width for the left panel
            let right_percentage = 0.3; // 30% of the width for the right panel

            // Get the available width to work with
            let available_width = ui.available_width();

            // Create a horizontal layout to contain the two panels
            ui.horizontal(|ui| {
                // Left panel
                ui.allocate_ui_with_layout(
                    egui::vec2(available_width * left_percentage, 100.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.heading("Audiostream -- high quality peer-to-peer voice calls.");
                        ui.separator();
                        ui.label("Address");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut self.listen_str);
                            if ui.button("Connect").clicked() {
                                let settings = Arc::new(self.current_settings.clone());
                                let (ui_tx, ui_rx) = std::sync::mpsc::channel();
                                let (aud_tx, aud_rx) = std::sync::mpsc::channel();
                                let muted = Arc::new(AtomicBool::new(false));
                                self.servers.push(Server {
                                    ui_tx: ui_tx.clone(),
                                    aud_rx,
                                    is_server: false,
                                    state: ServerState::Init,
                                    muted: Arc::clone(&muted),
                                    dbs: VecDeque::new(),
                                });
                                let listen_str = self.listen_str.clone();
                                let host = Arc::clone(&self.host);

                                std::thread::spawn(move || {
                                    start(
                                        listen_str.parse().unwrap(),
                                        false,
                                        ui_rx,
                                        aud_tx,
                                        Arc::clone(&muted),
                                        settings,
                                        host,
                                    )
                                    .unwrap();
                                });
                            }
                            ui.label("or");
                            if ui.button("Start server").clicked() {
                                let settings = Arc::new(self.current_settings.clone());
                                let (ui_tx, ui_rx) = std::sync::mpsc::channel();
                                let (aud_tx, aud_rx) = std::sync::mpsc::channel();
                                let muted = Arc::new(AtomicBool::new(false));
                                self.servers.push(Server {
                                    ui_tx: ui_tx.clone(),
                                    aud_rx,
                                    is_server: true,
                                    state: ServerState::Init,
                                    muted: Arc::clone(&muted),
                                    dbs: VecDeque::new(),
                                });
                                let listen_str = self.listen_str.clone();
                                let host = Arc::clone(&self.host);
                                std::thread::spawn(move || {
                                    start(
                                        listen_str.parse().unwrap(),
                                        true,
                                        ui_rx,
                                        aud_tx,
                                        Arc::clone(&muted),
                                        settings,
                                        host,
                                    )
                                    .unwrap();
                                });
                            }
                        });
                        egui::TopBottomPanel::bottom("bottom_panel")
                            .min_height(300.0)
                            .show(ctx, |ui| {
                                egui::ScrollArea::vertical().show(ui, |ui| {
                                    ui.vertical_centered(|ui| {
                                        ui.heading("Settings");
                                    });
                                    ui.add(
                                        egui::Slider::new(
                                            &mut self.current_settings.latency,
                                            0.01..=3.0,
                                        )
                                        .text("Audio stream latency (higher = more reliable)"),
                                    );
                                    if egui::ComboBox::from_label("Input device")
                                        .selected_text(
                                            self.current_settings.input_device_name.clone(),
                                        )
                                        .show_ui(ui, |ui| {
                                            for dev in &self.cached_input_device_names {
                                                ui.selectable_value(
                                                    &mut self.current_settings.input_device_name,
                                                    dev.clone(),
                                                    dev,
                                                );
                                            }
                                        })
                                        .response
                                        .clicked()
                                    {
                                        self.cached_input_device_names = self
                                            .host
                                            .input_devices()
                                            .unwrap()
                                            .map(|x| x.name().unwrap())
                                            .collect();
                                    };

                                    let mut mixer = self.mixer.lock().unwrap();
                                    ui.group(|ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.label("Effect rack");
                                        if ui.button("Noise gate").clicked() {
                                            let len = mixer.effects.len();
                                            mixer.effects.push(Box::new(NoiseGate {
                                                idx: len as u32,
                                                threshold: -40.0,
                                                attack: 2,
                                                decay: 10,
                                                strength: 0.8,
                                                current: 1.0,
                                            }));
                                        }
                                    });
                                    ui.separator();
                                    let mut removed = None;
                                    let resp = egui_dnd::dnd(ui, "Effects").show(
                                        mixer.effects.iter_mut().enumerate(),
                                        |ui, (i, effect), handle, state| {
                                            ui.horizontal(|ui| {
                                                handle.ui(ui, |ui| {
                                                    ui.label("*");
                                                });
                                                CollapsingHeader::new(effect.name())
                                                    .default_open(true)
                                                    .show(ui, |ui| {
                                                        if ui.button("Remove").clicked() {
                                                            removed = Some(i);
                                                        }
                                                        ui.separator();
                                                        effect.show(ui);
                                                    });
                                            });
                                            ui.separator();
                                        },
                                    );
                                    if let Some(i) = removed {
                                        mixer.effects.remove(i);
                                    }
                                    if resp.is_drag_finished() {
                                        resp.update_vec(&mut mixer.effects);
                                    }
                                    ui.group(|ui| {
                                        ui.vertical_centered_justified(|ui| {
                                            ui.label("Opus encoding");
                                            if let AudioEncoding::Opus {
                                                purpose,
                                                bandwidth,
                                                bit_rate,
                                                signal_type,
                                            } = &mut self.current_settings.encoding
                                            {
                                                egui::ComboBox::from_label("Application")
                                                    .selected_text(format!("{purpose:?}"))
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            purpose,
                                                            audiopus::Application::Audio,
                                                            "Audio",
                                                        );
                                                        ui.selectable_value(
                                                            purpose,
                                                            audiopus::Application::LowDelay,
                                                            "LowDelay",
                                                        );
                                                        ui.selectable_value(
                                                            purpose,
                                                            audiopus::Application::Voip,
                                                            "Voip",
                                                        );
                                                    });
                                                egui::ComboBox::from_label("Bandwidth")
                                                    .selected_text(format!("{bandwidth:?}"))
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            bandwidth,
                                                            audiopus::Bandwidth::Auto,
                                                            "Auto",
                                                        );
                                                        ui.selectable_value(
                                                            bandwidth,
                                                            audiopus::Bandwidth::Fullband,
                                                            "Fullband",
                                                        );
                                                        ui.selectable_value(
                                                            bandwidth,
                                                            audiopus::Bandwidth::Mediumband,
                                                            "Mediumband",
                                                        );
                                                        ui.selectable_value(
                                                            bandwidth,
                                                            audiopus::Bandwidth::Narrowband,
                                                            "Narrowband",
                                                        );
                                                        ui.selectable_value(
                                                            bandwidth,
                                                            audiopus::Bandwidth::Superwideband,
                                                            "Superwideband",
                                                        );
                                                        ui.selectable_value(
                                                            bandwidth,
                                                            audiopus::Bandwidth::Wideband,
                                                            "Wideband",
                                                        );
                                                    });
                                                ui.horizontal(|ui| {
                                                    egui::ComboBox::from_label("Bitrate")
                                                        .selected_text(format!("{bit_rate:?}"))
                                                        .show_ui(ui, |ui| {
                                                            ui.selectable_value(
                                                                bit_rate,
                                                                audiopus::Bitrate::Auto,
                                                                "Auto",
                                                            );
                                                            ui.selectable_value(
                                                                bit_rate,
                                                                audiopus::Bitrate::Max,
                                                                "Max",
                                                            );
                                                            ui.selectable_value(
                                                                bit_rate,
                                                                audiopus::Bitrate::BitsPerSecond(
                                                                    98_000,
                                                                ),
                                                                "Custom",
                                                            );
                                                        });
                                                    if let audiopus::Bitrate::BitsPerSecond(num) =
                                                        bit_rate
                                                    {
                                                        ui.add(
                                                            egui::Slider::new(num, 1000..=256_000)
                                                                .text("Custom bitrate (bits/sec)"),
                                                        );
                                                    }
                                                });
                                                egui::ComboBox::from_label("Signal type")
                                                    .selected_text(format!("{signal_type:?}"))
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            signal_type,
                                                            audiopus::Signal::Auto,
                                                            "Auto",
                                                        );
                                                        ui.selectable_value(
                                                            signal_type,
                                                            audiopus::Signal::Music,
                                                            "Music",
                                                        );
                                                        ui.selectable_value(
                                                            signal_type,
                                                            audiopus::Signal::Voice,
                                                            "Voice",
                                                        );
                                                    });
                                            }
                                        });
                                    });
                                });
                            });
                    },
                );

                // Right panel
                ui.allocate_ui_with_layout(
                    egui::vec2(available_width * right_percentage, 100.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.vertical_centered(|ui| {
                            ui.heading("Server rack");
                        });
                        if self.servers.is_empty() {
                            ui.label(
                                "Start a server or connect to a server for something to show here",
                            );
                        }
                        let mut dead_servers = vec![];
                        for (i, server) in self.servers.iter_mut().enumerate() {
                            while let Ok(msg) = server.aud_rx.try_recv() {
                                match msg {
                                    AudioUpdate::Dead => {
                                        dead_servers.push(i);
                                    }
                                    AudioUpdate::ServerState(server_state) => {
                                        server.state = server_state;
                                    }
                                    AudioUpdate::DecibelReading(db) => {
                                        if server.dbs.len() > 80 {
                                            let _ = server.dbs.pop_front();
                                        }
                                        server.dbs.push_back(db);
                                    }
                                }
                            }
                            ui.group(|ui| {
                                ui.set_min_width(ui.available_width() - 80.0);
                                if server.is_server {
                                    ui.label("Server");
                                } else {
                                    ui.label("Client");
                                }
                                ui.label(format!("State: {:?}", server.state));
                                ui.horizontal_centered(|ui| {
                                    if ui.button("Kill").clicked() {
                                        let _ = server.ui_tx.send(UiUpdate::Kill);
                                    }
                                    if !server.muted.load(Ordering::Relaxed) {
                                        if ui.button("Mute").clicked() {
                                            server.muted.store(true, Ordering::Relaxed);
                                        }
                                    } else {
                                        if ui.button("Unmute").clicked() {
                                            server.muted.store(false, Ordering::Relaxed);
                                        }
                                    }
                                });
                                let sin: PlotPoints = server
                                    .dbs
                                    .iter()
                                    .enumerate()
                                    .map(|(i, x)| {
                                        let x = *x as f64;
                                        [i as f64, x]
                                    })
                                    .collect();
                                let line = Line::new(sin);
                                Plot::new(format!("db{i}")).show(ui, |plot_ui| {
                                    plot_ui.set_plot_bounds(PlotBounds::from_min_max(
                                        [0.0, -60.0],
                                        [80.0, 0.0],
                                    ));
                                    plot_ui.line(line);
                                });
                            });
                        }
                        dead_servers.reverse();
                        for i in dead_servers {
                            self.servers.remove(i);
                        }
                    },
                );
            });
        });
        ctx.request_repaint_after(Duration::from_millis(50));
    }
}
