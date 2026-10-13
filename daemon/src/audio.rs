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
/// How far the audio sample clock may drift from the wall clock before it is
/// re-anchored (see `run_audio_capture`).
const AUDIO_CLOCK_REANCHOR_MS: u32 = 200;
/// Speaker audio Opus bitrate (48 kHz stereo, 10 ms frames).
const OPUS_SPEAKER_BITRATE: i32 = 128_000;
/// libopus' recommended worst-case output buffer for one encode call.
const OPUS_MAX_PACKET: usize = 4000;

/// Speaker-audio Opus encoder plus its reusable scratch buffers.
struct SpeakerOpus {
    encoder: opus::Encoder,
    pcm: Vec<i16>,
    out: Vec<u8>,
}

impl SpeakerOpus {
    fn new() -> Result<Self> {
        let mut encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::Audio)
                .map_err(|e| anyhow::anyhow!("opus encoder init: {e:?}"))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(OPUS_SPEAKER_BITRATE))
            .map_err(|e| anyhow::anyhow!("opus set_bitrate: {e:?}"))?;
        // No in-band FEC: the iPad decoder can't use it. No DTX: the client's
        // jitter buffer expects a packet every 10 ms.
        let _ = encoder.set_inband_fec(false);
        let _ = encoder.set_dtx(false);
        Ok(Self {
            encoder,
            pcm: vec![0i16; SAMPLES_PER_CHUNK * CHANNELS as usize],
            out: vec![0u8; OPUS_MAX_PACKET],
        })
    }

    /// Encode one 10 ms chunk of little-endian s16 stereo PCM.
    fn encode(&mut self, chunk: &[u8]) -> Result<&[u8]> {
        for (dst, src) in self.pcm.iter_mut().zip(chunk.chunks_exact(2)) {
            *dst = i16::from_le_bytes([src[0], src[1]]);
        }
        let len = self
            .encoder
            .encode(&self.pcm, &mut self.out)
            .map_err(|e| anyhow::anyhow!("opus encode: {e:?}"))?;
        Ok(&self.out[..len])
    }
}

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
        let start_time = shared.start_time;

        // Monotonic sample clock in the same epoch as the video timestamps.
        // PulseAudio (tsched) can hand over several 10 ms chunks at once, and
        // stamping each with the wall clock produces a staircase of identical
        // timestamps that the iPad's A/V sync misreads as drift, discarding
        // ring data (audible crackle at IDR times). Advance 10 ms per chunk
        // instead, re-anchoring to the wall clock only if the two diverge by
        // more than AUDIO_CLOCK_REANCHOR_MS (capture stall or restart).
        let mut sample_clock_ms: Option<u32> = None;

        // Created once per capture session, not per chunk. If libopus fails
        // to initialize, the network path falls back to PCM.
        let mut speaker_opus = match SpeakerOpus::new() {
            Ok(opus) => Some(opus),
            Err(e) => {
                eprintln!("[audio] {e:#}; sending PCM");
                None
            }
        };
        let client_features = &shared.client_features;

        let mut on_chunk = |chunk: &[u8]| {
            if chunk.len() != BYTES_PER_CHUNK {
                return;
            }
            let wall_ms = start_time.elapsed().as_millis() as u32;
            let ts = match sample_clock_ms {
                Some(clock)
                    if (wall_ms.wrapping_sub(clock) as i32).unsigned_abs()
                        <= AUDIO_CLOCK_REANCHOR_MS =>
                {
                    clock
                }
                _ => wall_ms,
            };
            sample_clock_ms = Some(ts.wrapping_add(CHUNK_DURATION_MS));

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
                let wants_opus = client_features.load(Ordering::Relaxed)
                    & crate::stream_server::CLIENT_FEATURE_OPUS_AUDIO
                    != 0;
                let result = match speaker_opus.as_mut().filter(|_| wants_opus) {
                    Some(opus) => opus.encode(chunk).and_then(|packet| {
                        sender.send_audio(packet, addr, ts, crate::stream_server::AUDIO_CODEC_OPUS)
                    }),
                    None => sender.send_audio(chunk, addr, ts, crate::stream_server::AUDIO_CODEC_PCM),
                };
                if let Err(e) = result {
                    eprintln!("[audio] send error: {e:#}");
                }
            }
        };

        println!("[audio] starting platform audio loopback capture");
        let loopback_stop = Arc::new(AtomicBool::new(false));
        let watchdog_done = Arc::new(AtomicBool::new(false));

        let result = std::thread::scope(|scope| {
            let watchdog_stop = Arc::clone(&loopback_stop);
            let watchdog_done_for_thread = Arc::clone(&watchdog_done);
            let global_stop = &stop;
            let shared_state = &shared;
            scope.spawn(move || {
                while !watchdog_done_for_thread.load(Ordering::Relaxed) {
                    if global_stop.load(Ordering::Relaxed)
                        || !shared_state.has_active_client.load(Ordering::Relaxed)
                        || !shared_state.audio_output_enabled.load(Ordering::SeqCst)
                    {
                        watchdog_stop.store(true, Ordering::SeqCst);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });

            let result = backend.run_loopback(&loopback_stop, &mut on_chunk);
            watchdog_done.store(true, Ordering::Relaxed);
            result
        });

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_opus_encodes_10ms_chunks_that_decode_to_480_frames() {
        let mut opus = SpeakerOpus::new().unwrap();
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut chunk = vec![0u8; BYTES_PER_CHUNK];
        for (i, sample) in chunk.chunks_exact_mut(2).enumerate() {
            let v = ((i as f32 * 0.05).sin() * 8000.0) as i16;
            sample.copy_from_slice(&v.to_le_bytes());
        }
        let mut pcm = vec![0i16; SAMPLES_PER_CHUNK * CHANNELS as usize];
        for _ in 0..5 {
            let packet = opus.encode(&chunk).unwrap().to_vec();
            assert!(packet.len() < 1400, "fits in one UDP chunk");
            let frames = decoder.decode(&packet, &mut pcm, false).unwrap();
            assert_eq!(frames, SAMPLES_PER_CHUNK);
        }
    }
}
