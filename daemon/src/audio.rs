use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use opus_decoder::OpusDecoder;

use crate::stream_server::{AudioSender, SharedState};

pub const SAMPLE_RATE: u32 = 48000;
pub const CHANNELS: u16 = 2;
pub const CHUNK_DURATION_MS: u32 = 10;
pub const SAMPLES_PER_CHUNK: usize = (SAMPLE_RATE / 1000 * CHUNK_DURATION_MS) as usize;
pub const BYTES_PER_CHUNK: usize = SAMPLES_PER_CHUNK * CHANNELS as usize * 2;
pub const MIC_RATE: u32 = 48000;
const MIC_MAX_FRAME: usize = 5760;

/// Platform-specific audio backend.
pub trait AudioBackend: Send {
    fn create_sink(&mut self) -> Result<()>;
    fn destroy_sink(&mut self);
    fn run_loopback(&mut self, stop: &AtomicBool, on_chunk: &mut dyn FnMut(&[u8])) -> Result<()>;
    fn create_mic(&mut self) -> Result<Box<dyn MicSink>>;
    fn destroy_mic(&mut self);
}

/// Sink for decoded microphone PCM.
pub trait MicSink: Send {
    fn write_pcm(&mut self, s16_mono_48k: &[i16]);
}

/// Create the platform audio backend.
pub fn create_audio_backend() -> Box<dyn AudioBackend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(crate::platform::linux::pulse::PulseAudioBackend::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(crate::platform::windows::wasapi::WasapiBackend::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(crate::platform::macos::audio::MacAudioBackend::new())
    }
}

/// Cheap capability probe: is virtual-microphone forwarding likely to work
/// right now? Linux: PipeWire/PulseAudio mic support is always available
/// when the daemon can run at all. Windows: requires the VB-CABLE driver.
pub fn probe_mic_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        true
    }
    #[cfg(target_os = "windows")]
    {
        crate::platform::windows::wasapi::probe_mic_available()
    }
    #[cfg(target_os = "macos")]
    {
        // Mic forwarding is deferred — needs a third-party virtual audio
        // driver on macOS (no built-in equivalent to PulseAudio null-sinks).
        false
    }
}

/// Cheap capability probe: is speaker/system-audio forwarding likely to
/// work right now? Linux: always available (PulseAudio null-sink). Windows:
/// requires the Steam Streaming Speakers endpoint.
pub fn probe_speaker_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        true
    }
    #[cfg(target_os = "windows")]
    {
        crate::platform::windows::wasapi::probe_speaker_available()
    }
    #[cfg(target_os = "macos")]
    {
        // ScreenCaptureKit system-audio capture (M3) — verified end-to-end
        // via the `macos_audio_smoke` test in platform/macos/audio.rs.
        true
    }
}

/// Remove any leftover audio state from a previous crash.
pub fn cleanup_stale_modules() {
    #[cfg(target_os = "linux")]
    {
        crate::platform::linux::pulse::cleanup_stale_modules();
    }
    #[cfg(target_os = "windows")]
    {
        crate::platform::windows::wasapi::cleanup_stale_audio_endpoints();
    }
    #[cfg(target_os = "macos")]
    {
        // Nothing to clean up yet.
    }
}

/// Create the virtual speaker sink.
pub fn create_virtual_sink() -> Result<u32> {
    let mut backend = create_audio_backend();
    backend.create_sink()?;
    // The Linux backend stores the module id internally; for the existing API
    // we return a placeholder id. Newer code should hold the backend directly.
    Ok(1)
}

/// Remove the virtual speaker sink.
pub fn remove_virtual_sink(_module_id: u32) {
    let mut backend = create_audio_backend();
    backend.destroy_sink();
}

/// Create the virtual microphone.
pub fn create_virtual_mic() -> Result<MicWriter> {
    let mut backend = create_audio_backend();
    let sink = backend.create_mic()?;
    let decoder =
        OpusDecoder::new(MIC_RATE, 1).map_err(|e| anyhow::anyhow!("opus decoder init: {e:?}"))?;
    Ok(MicWriter {
        decoder,
        sink,
        pcm_buf: vec![0i16; MIC_MAX_FRAME],
    })
}

/// Remove the virtual microphone.
pub fn remove_virtual_mic(mic: &mut MicWriter) {
    let _ = mic;
}

// ---------------------------------------------------------------------------
// Audio capture — reads from virtual sink monitor, sends to iPad
// ---------------------------------------------------------------------------

pub fn run_audio_capture(
    socket: UdpSocket,
    shared: Arc<SharedState>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut sender = AudioSender::new(socket);
    let mut backend = create_audio_backend();

    while !stop.load(Ordering::Relaxed) {
        {
            let guard = shared.audio_module_id.lock().unwrap();
            if !shared.has_active_client.load(Ordering::Relaxed)
                || !shared.audio_output_enabled.load(Ordering::SeqCst)
                || *guard == 0
            {
                let _ = shared
                    .audio_notify
                    .wait_timeout(guard, std::time::Duration::from_millis(200));
                continue;
            }
        }

        let mut active_session_key = *shared.session_key.lock().unwrap();
        if let Some(key) = active_session_key {
            sender.set_cipher(crate::crypto::SessionCipher::new(&key));
        } else {
            sender.clear_cipher();
        }
        sender.set_source(*shared.udp_source.lock().unwrap());

        let session_key = &shared.session_key;
        let udp_source = &shared.udp_source;
        let client_addr = &shared.client_addr;
        let usb_active = &shared.usb_active;
        let usb_sender = &shared.usb_sender;
        let audio_output_enabled = &shared.audio_output_enabled;
        let start_time = shared.start_time;

        let mut on_chunk = |chunk: &[u8]| {
            if chunk.len() != BYTES_PER_CHUNK {
                return;
            }
            let ts = start_time.elapsed().as_millis() as u32;

            if usb_active.load(Ordering::Relaxed) {
                let mut usb = usb_sender.lock().unwrap();
                if let Some(ref mut tcp) = *usb {
                    if let Err(e) = tcp.send_audio(chunk, ts) {
                        eprintln!("[audio] USB send error: {e}");
                        drop(usb);
                        usb_active.store(false, Ordering::SeqCst);
                    }
                    return;
                }
            }

            let current_session_key = *session_key.lock().unwrap();
            if current_session_key != active_session_key {
                active_session_key = current_session_key;
                sender.set_source(*udp_source.lock().unwrap());
                if let Some(key) = active_session_key {
                    sender.set_cipher(crate::crypto::SessionCipher::new(&key));
                } else {
                    sender.clear_cipher();
                }
            }

            if let Some(addr) = *client_addr.lock().unwrap() {
                if let Err(e) = sender.send_audio(chunk, addr, ts) {
                    eprintln!("[audio] send error: {e}");
                }
            }
        };

        println!("[audio] starting platform audio loopback capture");
        let loopback_stop = Arc::new(AtomicBool::new(false));

        let result = {
            let loopback_stop = Arc::clone(&loopback_stop);
            backend.run_loopback(&loopback_stop, &mut |chunk: &[u8]| {
                if !audio_output_enabled.load(Ordering::SeqCst) {
                    loopback_stop.store(true, Ordering::SeqCst);
                    return;
                }
                on_chunk(chunk);
            })
        };

        if let Err(e) = result {
            eprintln!("[audio] loopback error: {e:#}");
        }

        println!("[audio] capture session stopped, waiting for next client...");
        if !shared.audio_output_enabled.load(Ordering::SeqCst) {
            continue;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    println!("[audio] capture thread exiting");
    Ok(())
}

// ---------------------------------------------------------------------------
// Virtual microphone — receives Opus from iPad, decodes, feeds to platform sink
// ---------------------------------------------------------------------------

pub struct MicWriter {
    decoder: OpusDecoder,
    sink: Box<dyn MicSink>,
    pcm_buf: Vec<i16>,
}

unsafe impl Send for MicWriter {}

impl MicWriter {
    pub fn write_opus_packet(&mut self, opus_data: &[u8]) -> Result<()> {
        let samples = self
            .decoder
            .decode(opus_data, &mut self.pcm_buf, false)
            .map_err(|e| anyhow::anyhow!("opus decode: {e:?}"))?;
        self.sink.write_pcm(&self.pcm_buf[..samples]);
        Ok(())
    }
}
