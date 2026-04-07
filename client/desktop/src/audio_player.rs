//! Audio playback adapter — receives raw PCM s16le 48kHz stereo from daemon,
//! jitters it in a ring buffer, and writes to system audio output.
//!
//! Linux: uses libpulse-simple (works with PipeWire via its PulseAudio compat).
//! macOS/Windows: uses cpal.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{anyhow, Result};

/// Daemon audio format constants.
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const BYTES_PER_SAMPLE: usize = 2; // i16
const BYTES_PER_MS: usize = (SAMPLE_RATE as usize * CHANNELS as usize * BYTES_PER_SAMPLE) / 1000;

/// Ring buffer capacity (~500 ms).
const RING_CAPACITY: usize = BYTES_PER_MS * 500;

pub type AudioSlot = Arc<Mutex<AudioRingBuffer>>;

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
        let len = data.len();
        if len == 0 {
            return;
        }
        // If data is larger than capacity, only keep the tail
        let data = if len > RING_CAPACITY {
            &data[len - RING_CAPACITY..]
        } else {
            data
        };
        let len = data.len();

        // Overwrite oldest data if needed
        if self.count + len > RING_CAPACITY {
            let overflow = self.count + len - RING_CAPACITY;
            self.read_pos = (self.read_pos + overflow) % RING_CAPACITY;
            self.count -= overflow;
        }

        // Write in up to 2 contiguous chunks (wrap-around)
        let first = (RING_CAPACITY - self.write_pos).min(len);
        self.storage[self.write_pos..self.write_pos + first].copy_from_slice(&data[..first]);
        if first < len {
            self.storage[..len - first].copy_from_slice(&data[first..]);
        }
        self.write_pos = (self.write_pos + len) % RING_CAPACITY;
        self.count += len;
    }

    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.count);
        if n == 0 {
            return 0;
        }
        let first = (RING_CAPACITY - self.read_pos).min(n);
        out[..first].copy_from_slice(&self.storage[self.read_pos..self.read_pos + first]);
        if first < n {
            out[first..n].copy_from_slice(&self.storage[..n - first]);
        }
        self.read_pos = (self.read_pos + n) % RING_CAPACITY;
        self.count -= n;
        n
    }
}

/// Handle to the running audio playback stream.
pub struct AudioPlayer {
    ring: AudioSlot,
    stop: Arc<AtomicBool>,
    _thread: Option<thread::JoinHandle<()>>,
}

impl AudioPlayer {
    pub fn start() -> Result<Self> {
        let ring: AudioSlot = Arc::new(Mutex::new(AudioRingBuffer::new()));
        let stop = Arc::new(AtomicBool::new(false));

        #[cfg(target_os = "linux")]
        let handle = {
            let ring_clone = Arc::clone(&ring);
            let stop_clone = Arc::clone(&stop);
            let handle = thread::spawn(move || {
                if let Err(e) = pulse_playback_loop(&ring_clone, &stop_clone) {
                    eprintln!("[audio_player] pulseaudio playback failed: {e:#}");
                }
            });
            Some(handle)
        };

        #[cfg(not(target_os = "linux"))]
        let handle = {
            let ring_clone = Arc::clone(&ring);
            let stop_clone = Arc::clone(&stop);
            let handle = thread::spawn(move || {
                if let Err(e) = cpal_playback_loop(&ring_clone, &stop_clone) {
                    eprintln!("[audio_player] cpal playback failed: {e:#}");
                }
            });
            Some(handle)
        };

        Ok(Self {
            ring,
            stop,
            _thread: handle,
        })
    }

    pub fn enqueue(&self, pcm: &[u8]) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.write(pcm);
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

// ── Linux: libpulse-simple ──

#[cfg(target_os = "linux")]
fn pulse_playback_loop(ring: &AudioSlot, stop: &Arc<AtomicBool>) -> Result<()> {
    use std::ffi::CString;
    use std::ptr;

    // libpulse-simple FFI
    #[repr(C)]
    #[allow(non_camel_case_types)]
    enum pa_stream_direction_t {
        PA_STREAM_PLAYBACK = 1,
    }

    #[repr(C)]
    #[allow(non_camel_case_types)]
    enum pa_sample_format_t {
        PA_SAMPLE_S16LE = 3,
    }

    #[repr(C)]
    #[allow(non_camel_case_types)]
    struct pa_sample_spec {
        format: pa_sample_format_t,
        rate: u32,
        channels: u8,
    }

    #[repr(C)]
    #[allow(non_camel_case_types)]
    struct pa_buffer_attr {
        maxlength: u32,
        tlength: u32,
        prebuf: u32,
        minreq: u32,
        fragsize: u32,
    }

    #[allow(non_camel_case_types)]
    enum pa_simple {}

    extern "C" {
        fn pa_simple_new(
            server: *const libc::c_char,
            name: *const libc::c_char,
            dir: pa_stream_direction_t,
            dev: *const libc::c_char,
            stream_name: *const libc::c_char,
            ss: *const pa_sample_spec,
            map: *const libc::c_void,
            attr: *const pa_buffer_attr,
            error: *mut libc::c_int,
        ) -> *mut pa_simple;
        fn pa_simple_write(
            s: *mut pa_simple,
            data: *const libc::c_void,
            bytes: usize,
            error: *mut libc::c_int,
        ) -> libc::c_int;
        fn pa_simple_free(s: *mut pa_simple);
        fn pa_strerror(error: libc::c_int) -> *const libc::c_char;
    }

    let app_name = CString::new("Screx").unwrap();
    let stream_name = CString::new("Desktop Audio").unwrap();
    let ss = pa_sample_spec {
        format: pa_sample_format_t::PA_SAMPLE_S16LE,
        rate: SAMPLE_RATE,
        channels: CHANNELS as u8,
    };
    // Low-latency buffer attrs
    let frame_bytes = (BYTES_PER_MS * 20) as u32; // 20ms
    let attr = pa_buffer_attr {
        maxlength: u32::MAX,
        tlength: frame_bytes,
        prebuf: 0,
        minreq: u32::MAX,
        fragsize: u32::MAX,
    };

    let mut err: libc::c_int = 0;
    let pa = unsafe {
        pa_simple_new(
            ptr::null(),
            app_name.as_ptr(),
            pa_stream_direction_t::PA_STREAM_PLAYBACK,
            ptr::null(),
            stream_name.as_ptr(),
            &ss,
            ptr::null(),
            &attr,
            &mut err,
        )
    };
    if pa.is_null() {
        let msg = unsafe {
            let p = pa_strerror(err);
            if p.is_null() {
                "unknown error".to_string()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        return Err(anyhow!("pa_simple_new failed: {msg}"));
    }

    println!(
        "[audio_player] using PulseAudio: {}ch @ {} Hz s16le",
        CHANNELS, SAMPLE_RATE
    );

    // Write loop: pull from ring buffer, write to PA
    let chunk_bytes = BYTES_PER_MS * 10; // 10ms chunks
    let mut buf = vec![0u8; chunk_bytes];

    while !stop.load(Ordering::Relaxed) {
        let read = if let Ok(mut ring) = ring.lock() {
            ring.read(&mut buf)
        } else {
            0
        };

        if read == 0 {
            // No data — write silence to keep the stream alive
            buf.iter_mut().for_each(|b| *b = 0);
        }

        let write_len = if read > 0 { read } else { chunk_bytes };
        let ret = unsafe {
            pa_simple_write(pa, buf.as_ptr() as *const libc::c_void, write_len, &mut err)
        };
        if ret < 0 {
            let msg = unsafe {
                let p = pa_strerror(err);
                if p.is_null() {
                    "unknown error".to_string()
                } else {
                    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            eprintln!("[audio_player] pa_simple_write error: {msg}");
            break;
        }
    }

    unsafe { pa_simple_free(pa) };
    Ok(())
}

// ── macOS / Windows: cpal ──

#[cfg(not(target_os = "linux"))]
fn cpal_playback_loop(ring: &AudioSlot, stop: &Arc<AtomicBool>) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::SampleFormat;

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no audio output device found"))?;

    let config = device.default_output_config()?;
    let sample_format = config.sample_format();
    let stream_config = config.config();

    let ring_pull = Arc::clone(ring);
    let stop_flag = Arc::clone(stop);
    let channels = stream_config.channels as usize;

    // Pre-allocate a scratch buffer for the audio callback — avoids heap
    // allocation on every invocation (~200×/sec at 48 kHz / 256 frames).
    // Wrapped in a RefCell so the non-mut closure can mutate it.
    use std::cell::RefCell;
    let scratch = RefCell::new(Vec::<u8>::new());

    let stream = match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            &stream_config,
            move |data: &mut [f32], _| {
                cpal_write::<f32>(data, channels, &ring_pull, &stop_flag, &scratch)
            },
            |err| eprintln!("[audio_player] stream error: {err}"),
            None,
        )?,
        SampleFormat::I16 => {
            let ring_pull = Arc::clone(ring);
            let stop_flag = Arc::clone(stop);
            let scratch = RefCell::new(Vec::<u8>::new());
            device.build_output_stream(
                &stream_config,
                move |data: &mut [i16], _| {
                    cpal_write::<i16>(data, channels, &ring_pull, &stop_flag, &scratch)
                },
                |err| eprintln!("[audio_player] stream error: {err}"),
                None,
            )?
        }
        SampleFormat::U16 => {
            let ring_pull = Arc::clone(ring);
            let stop_flag = Arc::clone(stop);
            let scratch = RefCell::new(Vec::<u8>::new());
            device.build_output_stream(
                &stream_config,
                move |data: &mut [u16], _| {
                    cpal_write::<u16>(data, channels, &ring_pull, &stop_flag, &scratch)
                },
                |err| eprintln!("[audio_player] stream error: {err}"),
                None,
            )?
        }
        other => return Err(anyhow!("unsupported sample format: {other:?}")),
    };

    println!(
        "[audio_player] using cpal: {}ch @ {:?} ({:?})",
        stream_config.channels, stream_config.sample_rate, sample_format
    );

    stream.play()?;

    // Block until stopped
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(std::time::Duration::from_millis(100));
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn cpal_write<T>(
    data: &mut [T],
    channels: usize,
    ring: &AudioSlot,
    stop: &Arc<AtomicBool>,
    scratch: &std::cell::RefCell<Vec<u8>>,
) where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    if stop.load(Ordering::Relaxed) {
        for s in data.iter_mut() {
            *s = T::from_sample(0.0);
        }
        return;
    }

    let frames = if channels == 0 {
        0
    } else {
        data.len() / channels
    };
    let needed = frames * CHANNELS as usize * BYTES_PER_SAMPLE;
    let mut buf = scratch.borrow_mut();
    if buf.len() < needed {
        buf.resize(needed, 0);
    }
    let read = if let Ok(mut ring) = ring.lock() {
        ring.read(&mut buf[..needed])
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
