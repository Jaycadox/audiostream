use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    str::FromStr,
    sync::Arc,
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    InputCallbackInfo, OutputCallbackInfo, StreamConfig,
};
use quinn::{
    rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    ClientConfig, Endpoint, ServerConfig,
};
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb,
};
use rustls::crypto::CryptoProvider;

fn main() {
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
    println!("Config: {config:?}");

    println!(
        "Input device: {}, output device: {}",
        input_device.name().unwrap(),
        output_device.name().unwrap()
    );

    // Example: 200ms latency
    let latency_frames = 0.5215 * config.sample_rate.0 as f32;
    let latency_samples = latency_frames as usize * config.channels as usize;
    println!("Frames: {latency_frames}, samples: {latency_samples}");
    let server_ring = HeapRb::<u8>::new(latency_samples * 2);
    let client_ring = HeapRb::<u8>::new(latency_samples * 2);

    let (mut server_producer, mut server_consumer) = server_ring.split();
    let (mut client_producer, mut client_consumer) = client_ring.split();

    for _ in 0..latency_samples {
        server_producer.try_push(0).unwrap();
    }
    let mut input_error_count = 0;
    let input_data_fn = move |data: &[u8], _: &InputCallbackInfo| {
        let count = server_producer.push_slice(data);
        if count < data.len() {
            input_error_count += 1;
            //println!("input falling behind {input_error_count}");
        } else {
            input_error_count = 0;
        }
    };
    let output_data_fn = move |data: &mut [u8], _: &OutputCallbackInfo| {
        //println!("len: {}", data.len());
        let count = client_consumer.pop_slice(data);
        if count < data.len() {
            //println!("output falling behind");
        }
    };

    // 3762 wil be the audio sample buffer size
    std::thread::spawn(move || {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("Failed to install rustls crypto provider");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let der = CertificateDer::from(cert.cert);
            let der_1 = der.clone();
            let server_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 5000));

            tokio::spawn(async move {
                let priv_key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
                let mut server_config =
                    ServerConfig::with_single_cert(vec![der.clone()], priv_key.into()).unwrap();
                let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
                transport_config.max_concurrent_uni_streams(0_u8.into());
                let endpoint = Endpoint::server(server_config, server_addr).unwrap();

                let incoming = endpoint.accept().await.unwrap();
                let con = incoming.await.unwrap();
                println!("accepted: {:?}", con.remote_address());
                let mut tx = con.open_uni().await.unwrap();
                println!("opened uni");
                loop {
                    //println!("sent");
                    let mut samples = [0; 4410];
                    let len = server_consumer.pop_slice(&mut samples);
                    let samples = &samples[0..len];
                    tx.write_all(&samples[..len]).await.unwrap();
                    tokio::task::yield_now().await;
                }
            });
            tokio::spawn(async move {
                let mut certs = rustls::RootCertStore::empty();
                certs.add(der_1).unwrap();
                let client_config = ClientConfig::with_root_certificates(Arc::new(certs)).unwrap();
                let mut endpoint = Endpoint::client(SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(0, 0, 0, 0),
                    0,
                )))
                .unwrap();
                endpoint.set_default_client_config(client_config);
                let connection = endpoint
                    .connect(server_addr, "localhost")
                    .unwrap()
                    .await
                    .unwrap();
                println!("connected: {:?} test", connection.remote_address());
                println!("trying to accept con");
                let mut rx = connection.accept_uni().await.unwrap();
                loop {
                    println!("reading in");
                    let mut buf = [0; 4410];
                    rx.read_exact(&mut buf).await.unwrap();
                    client_producer.push_slice(&buf);
                    tokio::task::yield_now().await;
                }
            });
            tokio::spawn(async move {
                loop {
                    tokio::task::yield_now().await;
                }
            })
            .await;
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
    println!("Playing for 3 seconds... ");
    std::thread::sleep(std::time::Duration::from_secs(20));
    drop(input_stream);
    drop(output_stream);
    println!("Done!");
}

fn err_fn(err: cpal::StreamError) {
    eprintln!("an error occurred on stream: {}", err);
}
