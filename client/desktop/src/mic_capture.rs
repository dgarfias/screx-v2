//! Microphone capture adapter — captures audio from system mic,
//! encodes to Opus mono 48kHz 20ms frames, and sends as MIC UDP packets.
//!
//! Linux: uses libpulse-simple for recording (works with PipeWire compat).
//! macOS/Windows: uses cpal.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, Result};
use opus::{Application, Channels, Encoder};

use crate::backend::UdpSender;

const OPUS_SAMPLE_RATE: u32 = 48_000;
const OPUS_FRAME_SAMPLES: usize = 960; // 20ms at 48kHz
const OPUS_MAX_PACKET: usize = 4000;
const MIC_MAGIC: &[u8] = b"MIC";

/// Handle to the running mic capture.
pub struct MicCapture {
    stop: Arc<AtomicBool>,
    _thread: Option<thread::JoinHandle<()>>,
}

impl MicCapture {
    pub fn start(udp: Arc<UdpSender>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            #[cfg(target_os = "linux")]
            {
                if let Err(e) = pulse_capture_loop(&udp, &stop_clone) {
                    eprintln!("[mic_capture] pulseaudio capture failed: {e:#}");
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                if let Err(e) = cpal_capture_loop(&udp, &stop_clone) {
                    eprintln!("[mic_capture] cpal capture failed: {e:#}");
                }
            }
        });

        Ok(Self {
            stop,
            _thread: Some(handle),
        })
    }
}

impl Drop for MicCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

struct MicEncoder {
    encoder: Encoder,
    accumulator: Vec<f32>,
    mic_seq: u32,
    encode_buf: Vec<u8>,
}

impl MicEncoder {
    fn new() -> Result<Self> {
        let encoder = Encoder::new(OPUS_SAMPLE_RATE, Channels::Mono, Application::Voip)
            .map_err(|e| anyhow!("opus encoder init: {e}"))?;
        Ok(Self {
            encoder,
            accumulator: Vec::with_capacity(OPUS_FRAME_SAMPLES * 2),
            mic_seq: 0,
            encode_buf: vec![0u8; OPUS_MAX_PACKET],
        })
    }

    fn push_mono_s16(&mut self, samples: &[i16], udp: &UdpSender) {
        for &s in samples {
            self.accumulator.push(s as f32 / i16::MAX as f32);
        }
        while self.accumulator.len() >= OPUS_FRAME_SAMPLES {
            let frame: Vec<f32> = self.accumulator.drain(..OPUS_FRAME_SAMPLES).collect();
            self.encode_and_send(&frame, udp);
        }
    }

    fn encode_and_send(&mut self, frame: &[f32], udp: &UdpSender) {
        match self.encoder.encode_float(frame, &mut self.encode_buf) {
            Ok(len) => {
                let opus_data = &self.encode_buf[..len];
                let mut packet = Vec::with_capacity(MIC_MAGIC.len() + 4 + opus_data.len());
                packet.extend_from_slice(MIC_MAGIC);
                packet.extend_from_slice(&self.mic_seq.to_be_bytes());
                packet.extend_from_slice(opus_data);
                self.mic_seq = self.mic_seq.wrapping_add(1);
                if self.mic_seq == 1 || self.mic_seq % 100 == 0 {
                    println!(
                        "[mic_capture] sending opus packet seq={} opus_bytes={} total_bytes={}",
                        self.mic_seq,
                        len,
                        packet.len()
                    );
                }
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

// ── Linux: libpulse-simple recording ──

#[cfg(target_os = "linux")]
fn pulse_capture_loop(udp: &UdpSender, stop: &Arc<AtomicBool>) -> Result<()> {
    use std::ffi::CString;
    use std::ptr;

    #[repr(C)]
    #[allow(non_camel_case_types)]
    enum pa_stream_direction_t {
        PA_STREAM_RECORD = 2,
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
    #[allow(non_camel_case_types)]
    enum pa_simple {}

    #[allow(clashing_extern_declarations)]
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
        fn pa_simple_read(
            s: *mut pa_simple,
            data: *mut libc::c_void,
            bytes: usize,
            error: *mut libc::c_int,
        ) -> libc::c_int;
        fn pa_simple_free(s: *mut pa_simple);
        fn pa_strerror(error: libc::c_int) -> *const libc::c_char;
    }

    let app_name = CString::new("Screx").unwrap();
    let stream_name = CString::new("Mic Capture").unwrap();
    let ss = pa_sample_spec {
        format: pa_sample_format_t::PA_SAMPLE_S16LE,
        rate: OPUS_SAMPLE_RATE,
        channels: 1,
    };
    let fragsize = (OPUS_FRAME_SAMPLES * 2) as u32; // 20ms of s16 mono
    let attr = pa_buffer_attr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize,
    };

    let mut err: libc::c_int = 0;
    let pa = unsafe {
        pa_simple_new(
            ptr::null(),
            app_name.as_ptr(),
            pa_stream_direction_t::PA_STREAM_RECORD,
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
        return Err(anyhow!("pa_simple_new (record) failed: {msg}"));
    }

    println!(
        "[mic_capture] using PulseAudio: 1ch @ {} Hz s16le",
        OPUS_SAMPLE_RATE
    );

    let mut encoder = MicEncoder::new()?;
    let chunk_samples = OPUS_FRAME_SAMPLES;
    let chunk_bytes = chunk_samples * 2; // s16 mono
    let mut buf = vec![0i16; chunk_samples];

    while !stop.load(Ordering::Relaxed) {
        let ret = unsafe {
            pa_simple_read(
                pa,
                buf.as_mut_ptr() as *mut libc::c_void,
                chunk_bytes,
                &mut err,
            )
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
            eprintln!("[mic_capture] pa_simple_read error: {msg}");
            break;
        }
        encoder.push_mono_s16(&buf, udp);
    }

    unsafe { pa_simple_free(pa) };
    Ok(())
}

// ── macOS / Windows: cpal recording ──

#[cfg(not(target_os = "linux"))]
fn cpal_capture_loop(udp: &UdpSender, stop: &Arc<AtomicBool>) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::StreamConfig;

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

    let encoder = Arc::new(Mutex::new(MicEncoder::new()?));
    let encoder_cb = Arc::clone(&encoder);
    let udp_cb = Arc::clone(udp);
    let stop_flag = Arc::clone(stop);

    let stream = device.build_input_stream(
        &config,
        move |data: &[f32], _info: &cpal::InputCallbackInfo| {
            if stop_flag.load(Ordering::Relaxed) {
                return;
            }
            // Convert to mono
            let frames = data.len() / hw_channels;
            let mut mono = Vec::with_capacity(frames);
            for i in 0..frames {
                let mut sum = 0.0f32;
                for ch in 0..hw_channels {
                    sum += data[i * hw_channels + ch];
                }
                mono.push(sum / hw_channels as f32);
            }
            // TODO: resample if hw_rate != 48kHz
            if let Ok(mut enc) = encoder_cb.lock() {
                enc.push_mono_f32(&mono, &udp_cb);
            }
        },
        move |err| {
            eprintln!("[mic_capture] stream error: {err}");
        },
        None,
    )?;

    stream.play()?;

    println!(
        "[mic_capture] using cpal: {}ch @ {} Hz",
        hw_channels, hw_rate.0
    );

    while !stop.load(Ordering::Relaxed) {
        thread::sleep(std::time::Duration::from_millis(100));
    }

    Ok(())
}
