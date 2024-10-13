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
    let server_ring = HeapRb::<u16>::new(latency_samples * 2);
    let client_ring = HeapRb::<u16>::new(latency_samples * 2);

    let (mut server_producer, mut server_consumer) = server_ring.split();
    let (mut client_producer, mut client_consumer) = client_ring.split();

    for _ in 0..latency_samples {
        server_producer.try_push(0).unwrap();
    }
    let mut input_error_count = 0;
    let input_data_fn = move |data: &[u16], _: &InputCallbackInfo| {
        let count = server_producer.push_slice(data);
        if count < data.len() {
            input_error_count += 1;
            //println!("input falling behind {input_error_count}");
        } else {
            input_error_count = 0;
        }
    };
    let output_data_fn = move |data: &mut [u16], _: &OutputCallbackInfo| {
        //println!("len: {}", data.len());
        let count = client_consumer.pop_slice(data);
        if count < data.len() {
            //println!("output falling behind");
        }
    };

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
