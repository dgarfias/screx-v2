use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::stream_server::{AudioSender, SharedState};

pub const SAMPLE_RATE: u32 = 48000;
pub const CHANNELS: u16 = 2;
pub const CHUNK_DURATION_MS: u32 = 10;
pub const SAMPLES_PER_CHUNK: usize = (SAMPLE_RATE / 1000 * CHUNK_DURATION_MS) as usize;
pub const BYTES_PER_CHUNK: usize = SAMPLES_PER_CHUNK * CHANNELS as usize * 2;
pub const MIC_RATE: u32 = 48000;
const MIC_MAX_FRAME: usize = 5760;
/// Opus recommends a worst-case output buffer of 4000 bytes for any encode
/// call regardless of bitrate/complexity.
const OPUS_MAX_PACKET: usize = 4000;

/// Platform-specific audio backend.
pub trait AudioBackend: Send {
    fn create_sink(&mut self) -> Result<()>;
    fn destroy_sink(&mut self);
    fn run_loopback(&mut self, stop: &AtomicBool, on_chunk: &mut dyn FnMut(&[u8])) -> Result<()>;
    fn create_mic(&mut self) -> Result<Box<dyn MicSink>>;
}

/// Sink for decoded microphone PCM.
pub trait MicSink: Send {
    fn write_pcm(&mut self, s16_mono_48k: &[i16]);
}

/// Create the platform audio backend.
pub fn create_audio_backend() -> Box<dyn AudioBackend> {
    Box::new(crate::platform::linux::pulse::PulseAudioBackend::new())
}

/// Cheap capability probe: is virtual-microphone forwarding likely to work
/// right now? Linux: PipeWire/PulseAudio mic support is always available
/// when the daemon can run at all.
pub fn probe_mic_available() -> bool {
    true
}

/// Cheap capability probe: is speaker/system-audio forwarding likely to
/// work right now? Linux: always available (PulseAudio null-sink).
pub fn probe_speaker_available() -> bool {
    true
}

/// Remove any leftover audio state from a previous crash.
pub fn cleanup_stale_modules() {
    crate::platform::linux::pulse::cleanup_stale_modules();
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
    let decoder = opus::Decoder::new(MIC_RATE, opus::Channels::Mono)
        .map_err(|e| anyhow::anyhow!("opus decoder init: {e:?}"))?;
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
        // Session-clock anchor for the audio capture session. Each Opus chunk
        // is stamped base + n*10ms so the timeline is smooth and in the same
        // epoch as the video timestamps (the iPad's A/V sync compares them).
        let base_time_ms = shared.start_time.elapsed().as_millis() as u32;

        // Opus encoder for this capture session, plus its reusable scratch
        // buffers — created once here (not per 10 ms chunk) to avoid
        // allocating on the hot path.
        let mut encoder = match opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        ) {
            Ok(enc) => enc,
            Err(e) => {
                eprintln!("[audio] opus encoder init failed: {e:?}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        if let Err(e) = encoder.set_bitrate(opus::Bitrate::Bits(128_000)) {
            eprintln!("[audio] opus set_bitrate failed: {e:?}");
        }
        // In-band FEC kept OFF: the iPad's swift-opus decoder does not expose
        // the FEC flag (`OPUS_SET_INBAND_FEC`), so the redundancy would never
        // be used. Audio ride-ahead of video bursts is handled by the socket
        // pacing in the video sender instead.
        if let Err(e) = encoder.set_inband_fec(false) {
            eprintln!("[audio] opus set_inband_fec failed: {e:?}");
        }
        if let Err(e) = encoder.set_dtx(false) {
            eprintln!("[audio] opus set_dtx failed: {e:?}");
        }
        let mut pcm_scratch = vec![0i16; SAMPLES_PER_CHUNK * CHANNELS as usize];
        let mut opus_out = vec![0u8; OPUS_MAX_PACKET];

        // Monotonic sample clock for the audio stream. PulseAudio/tsched can
        // deliver parec fragments in bursts (several 10ms chunks at once), and
        // stamping each with the wall clock (`elapsed()`) produces a staircase
        // of identical timestamps. The client compares those against its
        // smooth video-anchored clock and misreads the staircase as drift,
        // then discards ring data -> audible gaps. A sample clock advances
        // 10ms per chunk toward our session-clock anchor no matter how the
        // chunks arrive, so the client sees a steady, in-epoch timeline.
        let mut chunk_index: u32 = 0;

        let mut on_chunk = |chunk: &[u8]| {
            if chunk.len() != BYTES_PER_CHUNK {
                return;
            }
            let ts = base_time_ms.wrapping_add(chunk_index.wrapping_mul(CHUNK_DURATION_MS));
            chunk_index = chunk_index.wrapping_add(1);

            // `chunk` is little-endian i16 PCM with no alignment guarantee —
            // widen it into the i16 scratch buffer sample by sample rather
            // than transmuting the byte slice.
            for (dst, src) in pcm_scratch.iter_mut().zip(chunk.chunks_exact(2)) {
                *dst = i16::from_le_bytes([src[0], src[1]]);
            }

            let len = match encoder.encode(&pcm_scratch, &mut opus_out) {
                Ok(len) => len,
                Err(e) => {
                    eprintln!("[audio] opus encode error: {e:?}");
                    return;
                }
            };
            let payload = &opus_out[..len];

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
                if let Err(e) = sender.send_audio(payload, addr, ts) {
                    eprintln!("[audio] send error: {e}");
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
    decoder: opus::Decoder,
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
