use anyhow::{anyhow, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait},
    Host,
};
use eframe::egui::{self, Id, Ui};
use egui_dnd::DragDropItem;
use quinn::{Connection, RecvStream, SendStream, VarInt};
use ringbuf::traits::{Consumer, Producer};
use std::hash::Hash;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
    Arc, Mutex,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::server::AudioUpdate;

pub struct Mixer {
    pub effects: Vec<Box<dyn Effect>>,
}

pub trait Effect: Send + 'static {
    fn name(&self) -> String;
    fn show(&mut self, ui: &mut Ui);
    fn apply(&mut self, volume: f32, frame: &mut [f32]);
}

pub struct NoiseGate {
    pub idx: u32,
    pub threshold: f32,
    pub attack: u32,
    pub decay: u32,
    pub strength: f32,
    pub current: f32,
}

impl Effect for NoiseGate {
    fn name(&self) -> String {
        format!("Noise gate {}", self.idx)
    }

    fn show(&mut self, ui: &mut Ui) {
        ui.add(egui::Slider::new(&mut self.threshold, -64.0..=0.0).text("Threshold (dB)"));
        ui.add(egui::Slider::new(&mut self.attack, 0..=500).text("Attack (samples)"));
        ui.add(egui::Slider::new(&mut self.decay, 0..=500).text("Decay (samples)"));
        ui.add(egui::Slider::new(&mut self.strength, 0.0..=1.0).text("Strength (%)"));
        ui.group(|ui| {
            ui.disable();
            ui.add(egui::Slider::new(&mut self.current, 0.0..=1.0).text("Current (%)"));
        });
    }

    fn apply(&mut self, volume: f32, frame: &mut [f32]) {
        let mut new = self.current;
        let attack_speed = self.strength / self.attack as f32;
        let decay_speed = self.strength / self.decay as f32;
        if volume < self.threshold {
            new -= decay_speed;
        } else {
            new += attack_speed;
        }
        let new = f32::min(1.0, new);
        let new = f32::max(1.0 - self.strength, new);
        self.current = new;
        for sample in frame {
            *sample *= new;
        }
    }
}
impl DragDropItem for (dyn Effect + 'static) {
    fn id(&self) -> eframe::egui::Id {
        Id::new(self.name())
    }
}

impl Hash for (dyn Effect + 'static) {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name().hash(state);
    }
}

#[derive(Debug, Clone)]
pub enum AudioEncoding {
    Opus {
        purpose: audiopus::Application,
        bandwidth: audiopus::Bandwidth,
        bit_rate: audiopus::Bitrate,
        signal_type: audiopus::Signal,
    },
}

#[derive(Clone)]
pub struct ConnectionSettings {
    pub encoding: AudioEncoding,
    pub latency: f32,
    pub input_device_name: String,
    pub output_device_name: String,
    pub mixer: Arc<Mutex<Mixer>>,
}

impl ConnectionSettings {
    pub fn new(host: &Host, mixer: &Arc<Mutex<Mixer>>) -> Self {
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
            mixer: Arc::clone(mixer),
        }
    }
}
pub async fn start_audio_channel<
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
            {
                let mut mixer = settings.mixer.lock().unwrap();

                let sum = samples[..len].iter().map(|x| f32::powi(*x, 2)).sum::<f32>()
                    * (1.0 / len as f32);
                let rms = f32::sqrt(sum);
                // Convert to decibel scale
                let mut db = 20.0 * f32::log10(rms);
                for effect in mixer.effects.iter_mut() {
                    let sum = samples[..len].iter().map(|x| f32::powi(*x, 2)).sum::<f32>()
                        * (1.0 / len as f32);
                    let rms = f32::sqrt(sum);
                    // Convert to decibel scale
                    db = 20.0 * f32::log10(rms);
                    effect.apply(db, &mut samples);
                }

                let sum = samples[..len].iter().map(|x| f32::powi(*x, 2)).sum::<f32>()
                    * (1.0 / len as f32);
                let rms = f32::sqrt(sum);
                // Convert to decibel scale
                let db = 20.0 * f32::log10(rms);
                let _ = audio_update.send(AudioUpdate::DecibelReading(db));
            }

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
