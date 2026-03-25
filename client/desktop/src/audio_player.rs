//! Audio playback adapter — receives raw PCM s16le 48kHz stereo from daemon,
//! jitters it in a ring buffer, and drives cpal output.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::StreamConfig;

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
    pub fn start() -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("no audio output device found"))?;

        let config = StreamConfig {
            channels: CHANNELS,
            sample_rate: SAMPLE_RATE,
            buffer_size: cpal::BufferSize::Default,
        };

        let ring: AudioSlot = Arc::new(Mutex::new(AudioRingBuffer::new()));
        let ring_pull = Arc::clone(&ring);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let stream = device.build_output_stream(
            &config,
            move |data: &mut [i16], _info: &cpal::OutputCallbackInfo| {
                if stop_flag.load(Ordering::Relaxed) {
                    for sample in data.iter_mut() {
                        *sample = 0;
                    }
                    return;
                }

                let byte_count = data.len() * BYTES_PER_SAMPLE;
                let mut buf = vec![0u8; byte_count];

                let read = if let Ok(mut ring) = ring_pull.lock() {
                    ring.read(&mut buf)
                } else {
                    0
                };

                // Convert bytes to i16 samples (little-endian).
                for (i, sample) in data.iter_mut().enumerate() {
                    let byte_off = i * 2;
                    if byte_off + 1 < read {
                        *sample = i16::from_le_bytes([buf[byte_off], buf[byte_off + 1]]);
                    } else {
                        *sample = 0;
                    }
                }
            },
            move |err| {
                eprintln!("[audio_player] stream error: {err}");
            },
            None,
        )?;

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

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
