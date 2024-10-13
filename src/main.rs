mod skip_verification;
use std::{
    mem,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender},
        Arc, Mutex,
    },
    time::Duration,
};

use byteorder::{ByteOrder, NetworkEndian};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    InputCallbackInfo, OutputCallbackInfo, StreamConfig,
};
use eframe::egui;
use quinn::{
    crypto::rustls::QuicClientConfig,
    rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    ClientConfig, Connection, Endpoint, RecvStream, SendStream, ServerConfig,
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
) -> Result<()> {
    let connection = Arc::new(Mutex::new(connection));
    let connection_1 = Arc::clone(&connection);
    let write = tokio::spawn(async move {
        let encoder = audiopus::coder::Encoder::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Mono,
            audiopus::Application::Voip,
        )
        .unwrap();
        while match connection.lock() {
            Ok(x) => x,
            _ => return Err(anyhow!("connection lost")),
        }
        .close_reason()
        .is_none()
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
        {
            let Ok(len) = rx.read_u64_le().await else {
                break;
            };
            let mut buf = vec![0; len as usize];
            let Ok(_) = rx.read_exact(&mut buf).await else {
                break;
            };
            let mut samples = vec![];
            decoder
                .decode_float(Some(&buf), &mut samples, false)
                .unwrap();
            match producer.lock() {
                Ok(x) => x,
                _ => break,
            }
            .push_slice(&samples);
            tokio::task::yield_now().await;
        }
        println!("stopping audio read");
        Ok(())
    });
    tokio::select! {
        res = write => {
            return res?;
        }
        res = read => {
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
}

#[derive(Debug)]
enum ServerState {
    Init,
    InitAudioSystem,
    WaitingForPeer,
    CreatingChannel,
    Active,
}

struct Server {
    ui_tx: Sender<UiUpdate>,
    aud_rx: Receiver<AudioUpdate>,
    is_server: bool,
    state: ServerState,
}

#[derive(Default)]
struct MyApp {
    loopback_sender: Option<(Sender<UiUpdate>, Sender<UiUpdate>)>,
    latency: f32,
    listen_str: String,
    servers: Vec<Server>,
}
fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Basic eframe UI",
        options,
        Box::new(|_cc| Ok(Box::new(MyApp::default()))),
    );
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
                                let latency = self.latency.clone();
                                let (ui_tx, ui_rx) = std::sync::mpsc::channel();
                                let (aud_tx, aud_rx) = std::sync::mpsc::channel();
                                self.servers.push(Server {
                                    ui_tx: ui_tx.clone(),
                                    aud_rx,
                                    is_server: false,
                                    state: ServerState::Init,
                                });
                                let listen_str = self.listen_str.clone();

                                std::thread::spawn(move || {
                                    start(
                                        listen_str.parse().unwrap(),
                                        false,
                                        ui_rx,
                                        aud_tx,
                                        false,
                                        latency,
                                    );
                                });
                            }
                            ui.label("or");
                            if ui.button("Start server").clicked() {
                                let latency = self.latency.clone();
                                let (ui_tx, ui_rx) = std::sync::mpsc::channel();
                                let (aud_tx, aud_rx) = std::sync::mpsc::channel();
                                self.servers.push(Server {
                                    ui_tx: ui_tx.clone(),
                                    aud_rx,
                                    is_server: true,
                                    state: ServerState::Init,
                                });
                                let listen_str = self.listen_str.clone();

                                std::thread::spawn(move || {
                                    start(
                                        listen_str.parse().unwrap(),
                                        true,
                                        ui_rx,
                                        aud_tx,
                                        false,
                                        latency,
                                    );
                                });
                            }
                        });
                        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
                            ui.heading("Settings");
                            ui.add(
                                egui::Slider::new(&mut self.latency, 0.01..=3.0)
                                    .text("Audio stream latency (higher = more reliable)"),
                            );
                            match &self.loopback_sender {
                                Some(sender) => {
                                    if ui.button("Kill loopback server").clicked() {
                                        sender.0.send(UiUpdate::Kill).unwrap();
                                        sender.1.send(UiUpdate::Kill).unwrap();
                                        self.loopback_sender = None;
                                    }
                                }
                                None => {
                                    if ui.button("Start loopback server (hear yourself)").clicked()
                                    {
                                        let (ui_tx, ui_rx) = std::sync::mpsc::channel();
                                        let (audio_tx, _audio_rx) = std::sync::mpsc::channel();

                                        let (ui_tx_1, ui_rx_1) = std::sync::mpsc::channel();
                                        let (audio_tx_1, _audio_rx_1) = std::sync::mpsc::channel();
                                        self.loopback_sender =
                                            Some((ui_tx.clone(), ui_tx_1.clone()));
                                        let local_addr = SocketAddr::V4(SocketAddrV4::new(
                                            Ipv4Addr::new(127, 0, 0, 1),
                                            5000,
                                        ));
                                        let local_addr_1 = local_addr.clone();
                                        let latency = self.latency;

                                        std::thread::spawn(move || {
                                            start(
                                                local_addr, true, ui_rx, audio_tx, false, latency,
                                            );
                                        });

                                        std::thread::spawn(move || {
                                            start(
                                                local_addr_1,
                                                false,
                                                ui_rx_1,
                                                audio_tx_1,
                                                true,
                                                latency,
                                            );
                                        });
                                    }
                                }
                            }
                        });
                    },
                );

                // Right panel
                ui.allocate_ui_with_layout(
                    egui::vec2(available_width * right_percentage, 100.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.heading("Server rack");
                        if self.servers.is_empty() {
                            ui.label(
                                "Start a server or connect to a server for something to show here",
                            );
                        }
                        let mut dead_servers = vec![];
                        for (i, server) in self.servers.iter_mut().enumerate() {
                            if let Ok(msg) = server.aud_rx.try_recv() {
                                match msg {
                                    AudioUpdate::Dead => {
                                        dead_servers.push(i);
                                    }
                                    AudioUpdate::ServerState(server_state) => {
                                        server.state = server_state;
                                    }
                                }
                            }
                            ui.group(|ui| {
                                if server.is_server {
                                    ui.label("Server");
                                } else {
                                    ui.label("Client");
                                }
                                ui.label(format!("State: {:?}", server.state));
                                if ui.button("Kill").clicked() {
                                    server.ui_tx.send(UiUpdate::Kill).unwrap();
                                }
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
    }
}

fn start(
    addr: SocketAddr,
    server: bool,
    rx: Receiver<UiUpdate>,
    tx: Sender<AudioUpdate>,
    mute_sender: bool,
    latency: f32,
) {
    tx.send(AudioUpdate::ServerState(ServerState::InitAudioSystem))
        .unwrap();
    let host = cpal::default_host();
    println!("Selected host: {}", host.id().name());

    println!(
        "Input devices: {:?}",
        host.input_devices()
            .unwrap()
            .map(|x| x.name().unwrap())
            .collect::<Vec<_>>()
    );

    println!(
        "Output devices: {:?}",
        host.output_devices()
            .unwrap()
            .map(|x| x.name().unwrap())
            .collect::<Vec<_>>()
    );

    let input_device = host.default_input_device().unwrap();
    let output_device = host.default_output_device().unwrap();

    let mut config: StreamConfig = input_device.default_input_config().unwrap().into();
    config.channels = 1;
    config.sample_rate = cpal::SampleRate(48000);
    println!("Config: {config:?}");

    println!(
        "Input device: {}, output device: {}",
        input_device.name().unwrap(),
        output_device.name().unwrap()
    );

    let latency_frames = latency * config.sample_rate.0 as f32;
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
    let mut input_error_count = 0;
    let producer_1 = Arc::clone(&send_producer);
    let input_data_fn = move |data: &[f32], _: &InputCallbackInfo| {
        if mute_sender {
            return;
        }
        let count = producer_1.lock().unwrap().push_slice(data);
        if count < data.len() {
            input_error_count += 1;
            //println!("input falling behind {input_error_count}");
        } else {
            input_error_count = 0;
        }
    };

    let consumer_1 = Arc::clone(&recv_consumer);
    let output_data_fn = move |data: &mut [f32], _: &OutputCallbackInfo| {
        //println!("len: {}", data.len());
        let count = consumer_1.lock().unwrap().pop_slice(data);
        if count < data.len() {
            //println!("output falling behind");
        }
    };

    // 3762 wil be the audio sample buffer size
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let der = CertificateDer::from(cert.cert);
    let der_1 = der.clone();

    let producer_2 = Arc::clone(&recv_producer);
    let consumer_2 = Arc::clone(&send_consumer);

    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_1 = Arc::clone(&stopped);
    let tx_ = tx.clone();

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

                    while !stopped.load(Ordering::Relaxed) {
                        tx.send(AudioUpdate::ServerState(ServerState::WaitingForPeer))
                            .unwrap();
                        let incoming = match tokio::time::timeout(
                            tokio::time::Duration::from_secs(1),
                            endpoint.accept(),
                        )
                        .await
                        {
                            Ok(x) => x,
                            _ => continue,
                        }
                        .unwrap();
                        tx.send(AudioUpdate::ServerState(ServerState::CreatingChannel))
                            .unwrap();

                        let con = match tokio::time::timeout(
                            tokio::time::Duration::from_secs(1),
                            incoming,
                        )
                        .await
                        {
                            Ok(x) => x,
                            _ => continue,
                        }
                        .unwrap();
                        println!("accepted: {:?}", con.remote_address());
                        let (tx1, rx) = con.open_bi().await.unwrap();

                        tx.send(AudioUpdate::ServerState(ServerState::Active))
                            .unwrap();
                        start_audio_channel(tx1, rx, producer_2.clone(), consumer_2.clone(), con)
                            .await
                            .unwrap();
                    }
                    println!("ended");
                    drop(endpoint);
                    stopped.store(true, Ordering::Relaxed);
                })
                .await;
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
                    start_audio_channel(tx1, rx, producer_2, consumer_2, connection)
                        .await
                        .unwrap();
                    stopped.store(true, Ordering::Relaxed);
                })
                .await;
            }
        });
    });

    let input_stream = input_device
        .build_input_stream(&config, input_data_fn, err_fn, None)
        .unwrap();
    let output_stream = output_device
        .build_output_stream(&config, output_data_fn, err_fn, None)
        .unwrap();
    input_stream.play().unwrap();
    output_stream.play().unwrap();
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
    println!("Done!");
    tx_.send(AudioUpdate::Dead).unwrap();
}

fn err_fn(err: cpal::StreamError) {
    eprintln!("an error occurred on stream: {}", err);
}

fn f32_to_u8_slice(floats: &[f32]) -> Vec<u8> {
    let mut bytes = vec![0u8; floats.len() * mem::size_of::<f32>()];
    for (i, &float) in floats.iter().enumerate() {
        NetworkEndian::write_f32(&mut bytes[i * mem::size_of::<f32>()..], float);
    }
    bytes
}

fn u8_to_f32_slice(bytes: &[u8]) -> Vec<f32> {
    assert!(bytes.len() % mem::size_of::<f32>() == 0);
    let mut floats = Vec::with_capacity(bytes.len() / mem::size_of::<f32>());
    for chunk in bytes.chunks_exact(mem::size_of::<f32>()) {
        let float = NetworkEndian::read_f32(chunk);
        floats.push(float);
    }
    floats
}
