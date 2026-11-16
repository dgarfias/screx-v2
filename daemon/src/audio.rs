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
/// Longest run of lost mic packets that is filled with Opus packet-loss
/// concealment. Longer gaps (a stall, or the client restarting its counter)
/// are not worth synthesizing; decoding just resumes.
const MIC_MAX_CONCEALED_PACKETS: u32 = 5;
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
        let mut encoder = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )
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
/// work right now? Always available (PulseAudio null-sink).
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
    let decoder =
        OpusDecoder::new(MIC_RATE, 1).map_err(|e| anyhow::anyhow!("opus decoder init: {e:?}"))?;
    Ok(MicWriter {
        decoder,
        sink,
        pcm_buf: vec![0i16; MIC_MAX_FRAME],
        next_seq: None,
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
        let start_time = shared.start_time;

        // Monotonic sample clock in the same epoch as the video timestamps.
        // PulseAudio (tsched) can hand over several 10 ms chunks at once, and
        // stamping each with the wall clock produces a staircase of identical
        // timestamps that the iPad's A/V sync misreads as drift, discarding
        // ring data (audible crackle at IDR times). Advance 10 ms per chunk
        // instead, re-anchoring to the wall clock only if the two diverge by
        // more than AUDIO_CLOCK_REANCHOR_MS (capture stall or restart).
        let mut sample_clock_ms: Option<u32> = None;

        // Created once per capture session, not per chunk.
        let mut speaker_opus = match SpeakerOpus::new() {
            Ok(opus) => opus,
            Err(e) => {
                eprintln!("[audio] {e:#}; speaker forwarding unavailable");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

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
                let result = speaker_opus
                    .encode(chunk)
                    .and_then(|packet| sender.send_audio(packet, addr, ts));
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
    /// Mic sequence number expected next; `None` until the first packet.
    next_seq: Option<u32>,
}

unsafe impl Send for MicWriter {}

impl MicWriter {
    /// Decode one mic packet. `seq` is the mic-only sequence number: missing
    /// numbers mean lost packets, which are filled with Opus packet-loss
    /// concealment so the virtual mic doesn't click or shorten. Packets
    /// older than the last one decoded are dropped.
    pub fn write_opus_packet(&mut self, seq: u32, opus_data: &[u8]) -> Result<()> {
        if let Some(expected) = self.next_seq {
            let gap = seq.wrapping_sub(expected);
            if gap > u32::MAX / 2 {
                return Ok(()); // late or duplicate packet
            }
            if gap > 0 && gap <= MIC_MAX_CONCEALED_PACKETS {
                crate::vlog!("[mic] concealing {gap} lost packet(s) before seq {seq}");
                for _ in 0..gap {
                    self.decode_into_sink(&[])?;
                }
            }
        }
        self.next_seq = Some(seq.wrapping_add(1));
        self.decode_into_sink(opus_data)
    }

    /// Decode `opus_data` (empty = conceal one lost packet) and write the PCM.
    fn decode_into_sink(&mut self, opus_data: &[u8]) -> Result<()> {
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

    /// Records how many samples each `write_pcm` call delivered.
    struct CountingSink(std::sync::Arc<std::sync::Mutex<Vec<usize>>>);

    impl MicSink for CountingSink {
        fn write_pcm(&mut self, s16_mono_48k: &[i16]) {
            self.0.lock().unwrap().push(s16_mono_48k.len());
        }
    }

    #[test]
    fn mic_gaps_are_concealed_and_late_packets_dropped() {
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mic = MicWriter {
            decoder: OpusDecoder::new(MIC_RATE, 1).unwrap(),
            sink: Box::new(CountingSink(writes.clone())),
            pcm_buf: vec![0i16; MIC_MAX_FRAME],
            next_seq: None,
        };
        let mut encoder =
            opus::Encoder::new(MIC_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let pcm = vec![0i16; 960]; // 20 ms mono
        let mut out = vec![0u8; OPUS_MAX_PACKET];
        let mut packet = || {
            let len = encoder.encode(&pcm, &mut out).unwrap();
            out[..len].to_vec()
        };

        mic.write_opus_packet(10, &packet()).unwrap();
        mic.write_opus_packet(11, &packet()).unwrap();
        // 12 and 13 lost: two concealed frames, then 14 itself.
        mic.write_opus_packet(14, &packet()).unwrap();
        // A late copy of 12 is dropped.
        mic.write_opus_packet(12, &packet()).unwrap();

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 5);
        assert!(writes.iter().all(|&n| n == 960), "{writes:?}");
    }

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
