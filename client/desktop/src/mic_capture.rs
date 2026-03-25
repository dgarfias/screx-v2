//! Microphone capture adapter — captures audio via cpal, resamples if needed,
//! encodes to Opus mono 48kHz 20ms frames, and sends as MIC UDP packets.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::StreamConfig;
use opus::{Application, Channels, Encoder};

use crate::backend::UdpSender;

const OPUS_SAMPLE_RATE: u32 = 48_000;
const OPUS_CHANNELS: usize = 1;
const OPUS_FRAME_SAMPLES: usize = 960; // 20ms at 48kHz
const OPUS_MAX_PACKET: usize = 4000;
const MIC_MAGIC: &[u8] = b"MIC";

/// Handle to the running mic capture.
pub struct MicCapture {
    _stream: cpal::Stream,
    stop: Arc<AtomicBool>,
}

impl MicCapture {
    /// Start microphone capture and Opus encoding.
    /// Encoded packets are sent via `udp` as `MIC` payloads.
    pub fn start(udp: Arc<UdpSender>) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no audio input device found"))?;

        let supported = device
            .default_input_config()
            .context("query default input config")?;

        let hw_rate = supported.sample_rate();
        let hw_channels = supported.channels() as usize;

        let config = StreamConfig {
            channels: hw_channels as u16,
            sample_rate: hw_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        // Accumulator and encoder live behind a mutex shared with the cpal callback.
        let shared = Arc::new(Mutex::new(MicState::new(hw_rate, hw_channels)?));
        let shared_cb = Arc::clone(&shared);
        let udp_cb = Arc::clone(&udp);

        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _info: &cpal::InputCallbackInfo| {
                if stop_flag.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(mut state) = shared_cb.lock() {
                    state.process_samples(data, &udp_cb);
                }
            },
            move |err| {
                eprintln!("[mic_capture] stream error: {err}");
            },
            None,
        )?;

        stream.play()?;

        Ok(Self {
            _stream: stream,
            stop,
        })
    }
}

impl Drop for MicCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

struct MicState {
    encoder: Encoder,
    accumulator: Vec<f32>,
    hw_rate: u32,
    hw_channels: usize,
    mic_seq: u32,
    encode_buf: Vec<u8>,
}

impl MicState {
    fn new(hw_rate: u32, hw_channels: usize) -> Result<Self> {
        let encoder = Encoder::new(OPUS_SAMPLE_RATE, Channels::Mono, Application::Voip)
            .map_err(|e| anyhow!("opus encoder init: {e}"))?;
        Ok(Self {
            encoder,
            accumulator: Vec::with_capacity(OPUS_FRAME_SAMPLES * 2),
            hw_rate,
            hw_channels,
            mic_seq: 0,
            encode_buf: vec![0u8; OPUS_MAX_PACKET],
        })
    }

    fn process_samples(&mut self, data: &[f32], udp: &UdpSender) {
        // Convert to mono and accumulate.
        let frames = data.len() / self.hw_channels;
        for i in 0..frames {
            let mut mono = 0.0f32;
            for ch in 0..self.hw_channels {
                mono += data[i * self.hw_channels + ch];
            }
            mono /= self.hw_channels as f32;

            // If hardware rate matches Opus rate, just push directly.
            // Otherwise do simple sample-rate conversion by nearest-neighbor.
            self.accumulator.push(mono);
        }

        // If hw_rate != 48kHz, we need to resample the accumulator.
        // For simplicity, do a batch resample when we have enough samples.
        if self.hw_rate != OPUS_SAMPLE_RATE && !self.accumulator.is_empty() {
            let ratio = OPUS_SAMPLE_RATE as f64 / self.hw_rate as f64;
            let out_len = (self.accumulator.len() as f64 * ratio) as usize;
            let mut resampled = Vec::with_capacity(out_len);
            for i in 0..out_len {
                let src_idx = (i as f64 / ratio).min((self.accumulator.len() - 1) as f64) as usize;
                resampled.push(self.accumulator[src_idx]);
            }
            self.accumulator = resampled;
        }

        // Drain in 960-sample (20ms) chunks.
        while self.accumulator.len() >= OPUS_FRAME_SAMPLES {
            let frame: Vec<f32> = self.accumulator.drain(..OPUS_FRAME_SAMPLES).collect();
            self.encode_and_send(&frame, udp);
        }
    }

    fn encode_and_send(&mut self, frame: &[f32], udp: &UdpSender) {
        match self.encoder.encode_float(frame, &mut self.encode_buf) {
            Ok(len) => {
                let opus_data = &self.encode_buf[..len];
                // Build MIC packet: "MIC" + seq(4 BE) + opus_data
                let mut packet = Vec::with_capacity(MIC_MAGIC.len() + 4 + opus_data.len());
                packet.extend_from_slice(MIC_MAGIC);
                packet.extend_from_slice(&self.mic_seq.to_be_bytes());
                packet.extend_from_slice(opus_data);
                self.mic_seq = self.mic_seq.wrapping_add(1);

                if let Err(e) = udp.send_encrypted(&packet) {
                    eprintln!("[mic_capture] send failed: {e}");
                }
            }
            Err(e) => {
                eprintln!("[mic_capture] opus encode error: {e}");
            }
        }
    }
}
