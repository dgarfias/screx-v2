//! Audio playback adapter — receives raw PCM s16le 48kHz stereo from daemon,
//! jitters it in a ring buffer, and drives cpal output.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

/// Daemon audio format constants.
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const BYTES_PER_SAMPLE: usize = 2; // i16
const BYTES_PER_MS: usize = (SAMPLE_RATE as usize * CHANNELS as usize * BYTES_PER_SAMPLE) / 1000;

/// Ring buffer capacity (~500 ms).
const RING_CAPACITY: usize = BYTES_PER_MS * 500;

/// Target pre-buffer before first playback pull (ms).
const TARGET_BUFFER_MS: usize = 30;
const TARGET_BUFFER_BYTES: usize = TARGET_BUFFER_MS * BYTES_PER_MS;

/// Drift thresholds (ms).
const DRIFT_DROP_MS: i64 = -40; // too far behind → discard
const DRIFT_TRIM_MS: i64 = 60; // too far ahead → trim down

pub type AudioSlot = Arc<Mutex<AudioRingBuffer>>;

/// Lock-free-ish ring buffer for PCM audio.
pub struct AudioRingBuffer {
    storage: Vec<u8>,
    read_pos: usize,
    write_pos: usize,
    count: usize,
}

impl AudioRingBuffer {
    pub fn new() -> Self {
        Self {
            storage: vec![0u8; RING_CAPACITY],
            read_pos: 0,
            write_pos: 0,
            count: 0,
        }
    }

    pub fn write(&mut self, data: &[u8]) {
        for &byte in data {
            if self.count >= RING_CAPACITY {
                // Overwrite oldest
                self.read_pos = (self.read_pos + 1) % RING_CAPACITY;
                self.count -= 1;
            }
            self.storage[self.write_pos] = byte;
            self.write_pos = (self.write_pos + 1) % RING_CAPACITY;
            self.count += 1;
        }
    }

    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.count);
        for slot in out[..n].iter_mut() {
            *slot = self.storage[self.read_pos];
            self.read_pos = (self.read_pos + 1) % RING_CAPACITY;
            self.count -= 1;
        }
        n
    }

    pub fn discard(&mut self, bytes: usize) {
        let n = bytes.min(self.count);
        self.read_pos = (self.read_pos + n) % RING_CAPACITY;
        self.count -= n;
    }

    pub fn clear(&mut self) {
        self.read_pos = 0;
        self.write_pos = 0;
        self.count = 0;
    }

    pub fn available(&self) -> usize {
        self.count
    }

    /// Apply drift correction similar to the iPad audio player.
    /// `drift_ms` = packet_timestamp - expected_daemon_time_now.
    pub fn apply_drift(&mut self, drift_ms: i64) {
        if drift_ms < DRIFT_DROP_MS {
            // Client is behind; discard some audio to catch up.
            let discard = ((-drift_ms) as usize * BYTES_PER_MS) & !3; // align to 4 bytes
            self.discard(discard);
        } else if drift_ms > DRIFT_TRIM_MS {
            // Client has excess buffered; trim down to target.
            if self.count > TARGET_BUFFER_BYTES {
                let excess = self.count - TARGET_BUFFER_BYTES;
                self.discard(excess);
            }
        }
    }
}

/// Handle to the running audio playback stream.
pub struct AudioPlayer {
    _stream: cpal::Stream,
    ring: AudioSlot,
    stop: Arc<AtomicBool>,
}

impl AudioPlayer {
    /// Start audio playback. Returns a handle; drop it to stop.
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no audio output device found"))?;

        let ring: AudioSlot = Arc::new(Mutex::new(AudioRingBuffer::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stream = build_best_output_stream(&device, Arc::clone(&ring), Arc::clone(&stop))?;

        stream.play()?;

        Ok(Self {
            _stream: stream,
            ring,
            stop,
        })
    }

    /// Enqueue raw PCM s16le stereo 48kHz bytes from the daemon.
    pub fn enqueue(&self, pcm: &[u8]) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.write(pcm);
        }
    }

    /// Enqueue with optional drift correction.
    pub fn enqueue_with_drift(&self, pcm: &[u8], drift_ms: i64) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.write(pcm);
            ring.apply_drift(drift_ms);
        }
    }

    /// Clear the buffer (e.g. on gap resume).
    pub fn clear(&self) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.clear();
        }
    }
}

fn build_best_output_stream(
    device: &cpal::Device,
    ring: AudioSlot,
    stop: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    let mut attempts: Vec<(StreamConfig, SampleFormat, String)> = Vec::new();

    if let Ok(configs) = device.supported_output_configs() {
        for range in configs {
            if range.channels() != CHANNELS {
                continue;
            }
            if range.min_sample_rate() <= SAMPLE_RATE && SAMPLE_RATE <= range.max_sample_rate() {
                let cfg = range.with_sample_rate(SAMPLE_RATE).config();
                attempts.push((
                    cfg,
                    range.sample_format(),
                    "matched 48k stereo config".into(),
                ));
            }
        }
    }

    if let Ok(default_cfg) = device.default_output_config() {
        attempts.push((
            default_cfg.config(),
            default_cfg.sample_format(),
            "default output config".into(),
        ));
    }

    if attempts.is_empty() {
        return Err(anyhow!("no usable audio output configurations found"));
    }

    let mut last_error = None;
    for (config, sample_format, label) in attempts {
        match build_output_stream_for_format(
            device,
            &config,
            sample_format,
            Arc::clone(&ring),
            Arc::clone(&stop),
        ) {
            Ok(stream) => {
                println!(
                    "[audio_player] using {}: {}ch @ {} Hz ({:?})",
                    label, config.channels, config.sample_rate, sample_format
                );
                return Ok(stream);
            }
            Err(error) => {
                eprintln!(
                    "[audio_player] output config failed: {}: {}ch @ {} Hz ({:?}): {error:#}",
                    label, config.channels, config.sample_rate, sample_format
                );
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("failed to open audio output stream")))
}

fn build_output_stream_for_format(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    ring: AudioSlot,
    stop: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    match sample_format {
        SampleFormat::I16 => build_typed_output_stream::<i16>(device, config, ring, stop),
        SampleFormat::U16 => build_typed_output_stream::<u16>(device, config, ring, stop),
        SampleFormat::F32 => build_typed_output_stream::<f32>(device, config, ring, stop),
        other => Err(anyhow!("unsupported sample format: {other:?}")),
    }
}

fn build_typed_output_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    ring: AudioSlot,
    stop: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;
    let ring_pull = Arc::clone(&ring);
    let stop_flag = Arc::clone(&stop);

    device
        .build_output_stream(
            config,
            move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                write_output_data::<T>(data, channels, &ring_pull, &stop_flag);
            },
            move |err| {
                eprintln!("[audio_player] stream error: {err}");
            },
            None,
        )
        .context("build output stream")
}

fn write_output_data<T>(data: &mut [T], channels: usize, ring: &AudioSlot, stop: &Arc<AtomicBool>)
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    if stop.load(Ordering::Relaxed) {
        for sample in data.iter_mut() {
            *sample = T::from_sample(0.0);
        }
        return;
    }

    let frames = if channels == 0 {
        0
    } else {
        data.len() / channels
    };
    let mut buf = vec![0u8; frames * CHANNELS as usize * BYTES_PER_SAMPLE];
    let read = if let Ok(mut ring) = ring.lock() {
        ring.read(&mut buf)
    } else {
        0
    };

    for frame_idx in 0..frames {
        let src_off = frame_idx * CHANNELS as usize * BYTES_PER_SAMPLE;
        let left = if src_off + 1 < read {
            i16::from_le_bytes([buf[src_off], buf[src_off + 1]]) as f32 / i16::MAX as f32
        } else {
            0.0
        };
        let right = if src_off + 3 < read {
            i16::from_le_bytes([buf[src_off + 2], buf[src_off + 3]]) as f32 / i16::MAX as f32
        } else {
            0.0
        };

        for ch in 0..channels {
            let sample = match ch {
                0 => left,
                1 => right,
                _ => 0.0,
            };
            data[frame_idx * channels + ch] = T::from_sample(sample);
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
