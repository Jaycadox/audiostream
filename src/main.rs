mod skip_verification;
use std::{
    collections::VecDeque,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender},
        Arc, Mutex,
    },
    time::Duration,
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Host, InputCallbackInfo, OutputCallbackInfo, StreamConfig,
};
use eframe::egui::{self, Vec2b};
use egui_plot::{AxisHints, Line, Plot, PlotBounds, PlotPoints};
use quinn::{
    crypto::rustls::QuicClientConfig,
    rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    ClientConfig, Connection, Endpoint, RecvStream, SendStream, ServerConfig, VarInt,
};
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb,
};
use skip_verification::SkipServerVerification;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use anyhow::{anyhow, Result};

async fn start_audio_channel<
    A: Producer<Item = f32> + Send + 'static,
    B: Consumer<Item = f32> + Send + 'static,
>(
    mut tx: SendStream,
    mut rx: RecvStream,
    producer: Arc<Mutex<A>>,
    consumer: Arc<Mutex<B>>,
    connection: Connection,
    stopped: Arc<AtomicBool>,
    settings: Arc<ConnectionSettings>,
    audio_update: Sender<AudioUpdate>,
) -> Result<()> {
    let connection = Arc::new(Mutex::new(connection));
    let connection_1 = Arc::clone(&connection);
    let connection_2 = Arc::clone(&connection);
    let stopped_1 = Arc::clone(&stopped);
    let &AudioEncoding::Opus {
        purpose,
        bandwidth,
        bit_rate,
        signal_type,
    } = &settings.encoding;
    let write = tokio::spawn(async move {
        let mut encoder = audiopus::coder::Encoder::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Mono,
            purpose,
        )
        .unwrap();
        encoder.set_bandwidth(bandwidth).unwrap();
        encoder.set_bitrate(bit_rate).unwrap();
        encoder.set_signal(signal_type).unwrap();
        while match connection.lock() {
            Ok(x) => x,
            _ => return Err(anyhow!("connection lost")),
        }
        .close_reason()
        .is_none()
            && !stopped.load(Ordering::Relaxed)
        {
            //println!("sent");
            let mut samples = vec![0.0; 2880];
            let mut samples_enc = vec![0; 2880 * 4];
            let mut cursor = 0;
            while cursor < 2880 {
                let len = match consumer.lock() {
                    Ok(x) => x,
                    _ => break,
                }
                .pop_slice(&mut samples[cursor..]);
                cursor += len;
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
            let len = 2880;
            if len == 0 {
                tokio::task::yield_now().await;
                tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                continue;
            }
            let len = encoder.encode_float(&samples, &mut samples_enc).unwrap();
            let samples = samples_enc;
            let samples = &samples[0..len];
            let Ok(_) = tx.write_u64_le(samples.len() as u64).await else {
                break;
            };
            let Ok(_) = tx.write_all(&samples).await else {
                break;
            };
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        println!("stopping audio write");
        Ok(())
    });
    let read = tokio::spawn(async move {
        let mut decoder =
            audiopus::coder::Decoder::new(audiopus::SampleRate::Hz48000, audiopus::Channels::Mono)
                .unwrap();
        while match connection_1.lock() {
            Ok(x) => x,
            _ => return Err(anyhow!("connection lost")),
        }
        .close_reason()
        .is_none()
            && !stopped_1.load(Ordering::Relaxed)
        {
            let Ok(len) = rx.read_u64_le().await else {
                break;
            };
            let mut buf = vec![0; len as usize];
            let Ok(_) = rx.read_exact(&mut buf).await else {
                break;
            };
            let mut samples = vec![0.0; 2880 * 5];
            let len = decoder
                .decode_float(Some(&buf), &mut samples, false)
                .expect("decode error");

            // Calculate root mean of squares
            let sum =
                samples[..len].iter().map(|x| f32::powi(*x, 2)).sum::<f32>() * (1.0 / len as f32);
            let rms = f32::sqrt(sum);
            // Convert to decibel scale
            let db = 20.0 * f32::log10(rms);
            let _ = audio_update.send(AudioUpdate::DecibelReading(db));

            match producer.lock() {
                Ok(x) => x,
                _ => break,
            }
            .push_slice(&samples[..len]);
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        println!("stopping audio read");
        Ok(())
    });
    tokio::select! {
        res = write => {
            connection_2.lock().unwrap().close(VarInt::from_u32(0), b"closed");
            return res?;
        }
        res = read => {
            connection_2.lock().unwrap().close(VarInt::from_u32(0), b"closed");
            return res?;
        }
    }
}

enum UiUpdate {
    Kill,
}

enum AudioUpdate {
    Dead,
    ServerState(ServerState),
    DecibelReading(f32),
}

#[derive(Debug)]
enum ServerState {
    Init,
    InitAudioSystem,
    WaitingForPeer,
    CreatingChannel,
    Active,
}

#[derive(Debug, Clone)]
enum AudioEncoding {
    Opus {
        purpose: audiopus::Application,
        bandwidth: audiopus::Bandwidth,
        bit_rate: audiopus::Bitrate,
        signal_type: audiopus::Signal,
    },
}

#[derive(Clone)]
struct ConnectionSettings {
    encoding: AudioEncoding,
    latency: f32,
    input_device_name: String,
    output_device_name: String,
}

impl ConnectionSettings {
    fn new(host: &Host) -> Self {
        Self {
            encoding: AudioEncoding::Opus {
                purpose: audiopus::Application::Voip,
                bandwidth: audiopus::Bandwidth::Auto,
                bit_rate: audiopus::Bitrate::Auto,
                signal_type: audiopus::Signal::Voice,
            },
            latency: 0.1,
            input_device_name: host.default_input_device().unwrap().name().unwrap(),
            output_device_name: host.default_output_device().unwrap().name().unwrap(),
        }
    }
}

struct Server {
    ui_tx: Sender<UiUpdate>,
    aud_rx: Receiver<AudioUpdate>,
    is_server: bool,
    state: ServerState,
    muted: Arc<AtomicBool>,
    dbs: VecDeque<f32>,
}

struct MyApp {
    listen_str: String,
    servers: Vec<Server>,
    current_settings: ConnectionSettings,
    host: Arc<Host>,
    cached_input_device_names: Vec<String>,
    cached_output_device_names: Vec<String>,
}

impl MyApp {
    fn new() -> Self {
        let host = Arc::new(cpal::default_host());

        println!("Selected host: {}", host.id().name());
        Self {
            listen_str: String::new(),
            servers: vec![],
            current_settings: ConnectionSettings::new(&host),
            host,
            cached_input_device_names: vec![],
            cached_output_device_names: vec![],
        }
    }
}

fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Basic eframe UI",
        options,
        Box::new(|_cc| Ok(Box::new(MyApp::new()))),
    )
    .unwrap();
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
                                    );
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
                                    );
                                });
                            }
                        });
                        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.heading("Settings");
                            });
                            ui.add(
                                egui::Slider::new(&mut self.current_settings.latency, 0.01..=3.0)
                                    .text("Audio stream latency (higher = more reliable)"),
                            );
                            if egui::ComboBox::from_label("Input device")
                                .selected_text(self.current_settings.input_device_name.clone())
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
                                                        audiopus::Bitrate::BitsPerSecond(98_000),
                                                        "Custom",
                                                    );
                                                });
                                            if let audiopus::Bitrate::BitsPerSecond(num) = bit_rate
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

fn start(
    addr: SocketAddr,
    server: bool,
    rx: Receiver<UiUpdate>,
    tx: Sender<AudioUpdate>,
    mute_sender: Arc<AtomicBool>,
    settings: Arc<ConnectionSettings>,
    host: Arc<Host>,
) -> Result<()> {
    tx.send(AudioUpdate::ServerState(ServerState::InitAudioSystem))?;
    let input_device = host
        .input_devices()?
        .find(|x| x.name().unwrap() == settings.input_device_name)
        .ok_or(anyhow!("can't find input device"))?;
    let output_device = host
        .output_devices()?
        .find(|x| x.name().unwrap() == settings.output_device_name)
        .ok_or(anyhow!("can't find output device"))?;
    let mut config: StreamConfig = input_device.default_input_config()?.into();
    config.channels = 1;
    config.sample_rate = cpal::SampleRate(48000);
    println!("Config: {config:?}");

    println!(
        "Input device: {}, output device: {}",
        input_device.name()?,
        output_device.name()?
    );

    let latency_frames = settings.latency * config.sample_rate.0 as f32;
    let latency_samples = latency_frames as usize * config.channels as usize;
    println!("Frames: {latency_frames}, samples: {latency_samples}");
    let send_ring = HeapRb::<f32>::new(latency_samples * 2);
    let recv_ring = HeapRb::<f32>::new(latency_samples * 2);

    let (send_producer, send_consumer) = send_ring.split();
    let (recv_producer, recv_consumer) = recv_ring.split();
    let send_producer = Arc::new(Mutex::new(send_producer));
    let send_consumer = Arc::new(Mutex::new(send_consumer));

    let recv_producer = Arc::new(Mutex::new(recv_producer));
    let recv_consumer = Arc::new(Mutex::new(recv_consumer));

    for _ in 0..latency_samples {
        recv_producer.lock().unwrap().try_push(0.0).unwrap();
    }
    let producer_1 = Arc::clone(&send_producer);
    let input_data_fn = move |data: &[f32], _: &InputCallbackInfo| {
        if mute_sender.load(Ordering::Relaxed) {
            let zeros = vec![0.0; data.len()];
            producer_1.lock().unwrap().push_slice(&zeros);
            return;
        }
        producer_1.lock().unwrap().push_slice(data);
    };

    let consumer_1 = Arc::clone(&recv_consumer);
    let output_data_fn = move |data: &mut [f32], _: &OutputCallbackInfo| {
        consumer_1.lock().unwrap().pop_slice(data);
    };

    // 3762 wil be the audio sample buffer size
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let der = CertificateDer::from(cert.cert);
    let der_1 = der.clone();

    let producer_2 = Arc::clone(&recv_producer);
    let consumer_2 = Arc::clone(&send_consumer);

    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_1 = Arc::clone(&stopped);
    let stopped_2 = Arc::clone(&stopped);
    let tx_ = tx.clone();
    let settings_1 = Arc::clone(&settings);
    let input_stream = input_device.build_input_stream(&config, input_data_fn, err_fn, None)?;
    let output_stream = output_device.build_output_stream(&config, output_data_fn, err_fn, None)?;
    input_stream.play()?;
    output_stream.play()?;
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            if server {
                tokio::spawn(async move {
                    let priv_key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
                    let mut server_config =
                        ServerConfig::with_single_cert(vec![der.clone()], priv_key.into()).unwrap();
                    let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
                    transport_config.max_concurrent_uni_streams(0_u8.into());
                    transport_config
                        .max_idle_timeout(Some(Duration::from_secs(1).try_into().unwrap()));
                    let endpoint = Endpoint::server(server_config, addr).unwrap();

                    tx.send(AudioUpdate::ServerState(ServerState::WaitingForPeer))
                        .unwrap();
                    let incoming = match tokio::time::timeout(
                        tokio::time::Duration::from_secs(100), //HACK
                        endpoint.accept(),
                    )
                    .await
                    {
                        Ok(x) => x,
                        _ => return,
                    }
                    .unwrap();
                    tx.send(AudioUpdate::ServerState(ServerState::CreatingChannel))
                        .unwrap();

                    let con =
                        match tokio::time::timeout(tokio::time::Duration::from_secs(1), incoming)
                            .await
                        {
                            Ok(x) => x,
                            _ => return,
                        }
                        .unwrap();
                    println!("accepted: {:?}", con.remote_address());
                    let (tx1, rx) = con.open_bi().await.unwrap();

                    tx.send(AudioUpdate::ServerState(ServerState::Active))
                        .unwrap();
                    start_audio_channel(
                        tx1,
                        rx,
                        producer_2.clone(),
                        consumer_2.clone(),
                        con,
                        Arc::clone(&stopped),
                        Arc::clone(&settings_1),
                        tx.clone(),
                    )
                    .await
                    .unwrap();
                    println!("ended");
                    drop(endpoint);
                    stopped.store(true, Ordering::Relaxed);
                })
                .await
                .unwrap();
            } else {
                tokio::spawn(async move {
                    let mut certs = rustls::RootCertStore::empty();
                    certs.add(der_1).unwrap();
                    let mut endpoint = Endpoint::client(SocketAddr::V4(SocketAddrV4::new(
                        Ipv4Addr::new(0, 0, 0, 0),
                        0,
                    )))
                    .unwrap();
                    let client_config = ClientConfig::new(Arc::new(
                        QuicClientConfig::try_from(
                            rustls::ClientConfig::builder()
                                .dangerous()
                                .with_custom_certificate_verifier(SkipServerVerification::new())
                                .with_no_client_auth(),
                        )
                        .unwrap(),
                    ));
                    endpoint.set_default_client_config(client_config);

                    tx.send(AudioUpdate::ServerState(ServerState::WaitingForPeer))
                        .unwrap();
                    let connection = endpoint.connect(addr, "localhost").unwrap().await.unwrap();
                    tx.send(AudioUpdate::ServerState(ServerState::CreatingChannel))
                        .unwrap();
                    println!("connected: {:?} test", connection.remote_address());
                    println!("trying to accept con");
                    let (tx1, rx) = connection.accept_bi().await.unwrap();
                    tx.send(AudioUpdate::ServerState(ServerState::Active))
                        .unwrap();
                    start_audio_channel(
                        tx1,
                        rx,
                        producer_2,
                        consumer_2,
                        connection,
                        Arc::clone(&stopped),
                        Arc::clone(&settings_1),
                        tx.clone(),
                    )
                    .await
                    .unwrap();
                    stopped.store(true, Ordering::Relaxed);
                })
                .await
                .unwrap();
            }
        });
    });
    while !stopped_1.load(Ordering::Relaxed) {
        if let Ok(update) = rx.try_recv() {
            match update {
                UiUpdate::Kill => {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(input_stream);
    drop(output_stream);
    stopped_2.store(true, Ordering::Relaxed);
    println!("Done!");
    tx_.send(AudioUpdate::Dead)?;
    Ok(())
}

fn err_fn(err: cpal::StreamError) {
    eprintln!("an error occurred on stream: {}", err);
}
