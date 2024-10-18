use anyhow::{anyhow, Result};
use quinn::{crypto::rustls::QuicClientConfig, ClientConfig, Endpoint, ServerConfig};
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
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
    Host, InputCallbackInfo, OutputCallbackInfo, StreamConfig,
};

use crate::{
    audio::{start_audio_channel, ConnectionSettings},
    skip_verification::SkipServerVerification,
};

#[derive(Debug)]
pub enum ServerState {
    Init,
    InitAudioSystem,
    WaitingForPeer,
    CreatingChannel,
    Active,
}

pub enum UiUpdate {
    Kill,
}

pub enum AudioUpdate {
    Dead,
    ServerState(ServerState),
    DecibelReading(f32),
}

pub struct Server {
    pub ui_tx: Sender<UiUpdate>,
    pub aud_rx: Receiver<AudioUpdate>,
    pub is_server: bool,
    pub state: ServerState,
    pub muted: Arc<AtomicBool>,
    pub dbs: VecDeque<f32>,
}

pub fn start(
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
