use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use opus_decoder::OpusDecoder;

use crate::platform::linux::screx_snd::{self, Card, Direction};
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
/// How late the speaker loop may fall behind its 10 ms schedule before it
/// gives up catching up and restarts the schedule from now.
const SPEAKER_MAX_LAG: Duration = Duration::from_millis(100);
/// libopus' recommended worst-case output buffer for one encode call.
const OPUS_MAX_PACKET: usize = 4000;

/// Speaker-audio Opus encoder plus its reusable scratch buffers.
struct SpeakerOpus {
    encoder: opus::Encoder,
    bitrate_bps: u32,
    pcm: Vec<i16>,
    out: Vec<u8>,
}

impl SpeakerOpus {
    fn new(bitrate_bps: u32) -> Result<Self> {
        let mut encoder = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )
        .map_err(|e| anyhow::anyhow!("opus encoder init: {e:?}"))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(bitrate_bps as i32))
            .map_err(|e| anyhow::anyhow!("opus set_bitrate: {e:?}"))?;
        // No in-band FEC: the iPad decoder can't use it. No DTX: the client's
        // jitter buffer expects a packet every 10 ms.
        let _ = encoder.set_inband_fec(false);
        let _ = encoder.set_dtx(false);
        Ok(Self {
            encoder,
            bitrate_bps,
            pcm: vec![0i16; SAMPLES_PER_CHUNK * CHANNELS as usize],
            out: vec![0u8; OPUS_MAX_PACKET],
        })
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        if self.bitrate_bps != bitrate_bps {
            self.encoder
                .set_bitrate(opus::Bitrate::Bits(bitrate_bps as i32))
                .map_err(|e| anyhow::anyhow!("opus set_bitrate: {e:?}"))?;
            self.bitrate_bps = bitrate_bps;
        }
        Ok(())
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

/// Sink for decoded microphone PCM.
pub trait MicSink: Send {
    fn write_pcm(&mut self, s16_mono_48k: &[i16]);
}

/// Speakers and microphone need the `screx_snd` kernel module.
pub fn probe_mic_available() -> bool {
    screx_snd::probe_available()
}

pub fn probe_speaker_available() -> bool {
    screx_snd::probe_available()
}

/// Plugs in the iPad's microphone, named after the iPad.
pub fn create_virtual_mic(name: &str) -> Result<MicWriter> {
    let card = Card::create(&format!("{name} Microphone"), 1, Direction::Microphone)?;
    crate::log_detail!("[mic] microphone card plugged in");
    let decoder =
        OpusDecoder::new(MIC_RATE, 1).map_err(|e| anyhow::anyhow!("opus decoder init: {e:?}"))?;
    Ok(MicWriter {
        decoder,
        sink: Box::new(MicCard {
            card,
            bytes: Vec::with_capacity(MIC_MAX_FRAME * 2),
        }),
        pcm_buf: vec![0i16; MIC_MAX_FRAME],
        next_seq: None,
    })
}

struct MicCard {
    card: Card,
    bytes: Vec<u8>,
}

impl MicSink for MicCard {
    fn write_pcm(&mut self, s16_mono_48k: &[i16]) {
        self.bytes.clear();
        self.bytes
            .extend(s16_mono_48k.iter().flat_map(|s| s.to_le_bytes()));
        if let Err(e) = self.card.write(&self.bytes) {
            eprintln!("[mic] {e:#}");
        }
    }
}

// ---------------------------------------------------------------------------
// Speakers — reads the iPad's speaker card, sends it to the iPad
// ---------------------------------------------------------------------------

/// While a client has speakers on, plugs in a speaker card named after the iPad,
/// reads 10 ms of what applications play into it every 10 ms, and sends it to the
/// iPad as Opus. Unplugs the card when speakers go off or the client leaves.
pub fn run_speaker(socket: UdpSocket, shared: Arc<SharedState>, stop: Arc<AtomicBool>) -> Result<()> {
    let mut sender = AudioSender::new(socket);
    let speakers_on = || {
        shared.has_active_client.load(Ordering::Relaxed)
            && shared.audio_output_enabled.load(Ordering::SeqCst)
    };

    while !stop.load(Ordering::Relaxed) {
        let name = {
            let name = shared.audio_device_name.lock().unwrap();
            if !speakers_on() {
                let _ = shared
                    .audio_notify
                    .wait_timeout(name, Duration::from_millis(200));
                continue;
            }
            name.clone()
        };

        let mut speaker_opus = match SpeakerOpus::new(shared.audio_bitrate_bps.load(Ordering::Relaxed)) {
            Ok(opus) => opus,
            Err(e) => {
                eprintln!("[audio] {e:#}; speaker forwarding unavailable");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let mut card = match Card::create(&name, u32::from(CHANNELS), Direction::Speakers) {
            Ok(card) => card,
            Err(e) => {
                eprintln!("[audio] {e:#}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        crate::log_detail!("[audio] speaker card '{name}' plugged in");

        let mut active_session_key = None;
        let mut chunk = [0u8; BYTES_PER_CHUNK];
        let mut next = Instant::now();
        while speakers_on() && !stop.load(Ordering::Relaxed) {
            // This schedule is the card's clock.
            next += Duration::from_millis(u64::from(CHUNK_DURATION_MS));
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else if now - next > SPEAKER_MAX_LAG {
                crate::log_detail!("[audio] speaker loop stalled {:?}; resuming", now - next);
                next = now;
            }
            if let Err(e) = card.read(&mut chunk) {
                eprintln!("[audio] {e:#}");
                break;
            }

            let session_key = *shared.session_key.lock().unwrap();
            if session_key != active_session_key {
                active_session_key = session_key;
                sender.set_source(*shared.udp_source.lock().unwrap());
                match active_session_key {
                    Some(key) => sender.set_cipher(crate::crypto::SessionCipher::new(&key)),
                    None => sender.clear_cipher(),
                }
            }
            if let Some(addr) = *shared.client_addr.lock().unwrap() {
                // Stamped with when the chunk was due to play, on the video timestamps' clock.
                let ts = next.saturating_duration_since(shared.start_time).as_millis() as u32;
                let result = speaker_opus
                    .set_bitrate(shared.audio_bitrate_bps.load(Ordering::Relaxed))
                    .and_then(|()| speaker_opus.encode(&chunk))
                    .and_then(|packet| sender.send_audio(packet, addr, ts));
                if let Err(e) = result {
                    eprintln!("[audio] send error: {e:#}");
                }
            }
        }
        drop(card);
        crate::log_detail!("[audio] speaker card unplugged");
    }

    crate::log_detail!("[audio] speaker thread exiting");
    Ok(())
}

// ---------------------------------------------------------------------------
// Microphone — receives Opus from iPad, decodes, feeds the microphone card
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
                crate::log_trace!("[mic] concealing {gap} lost packet(s) before seq {seq}");
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
        let mut opus = SpeakerOpus::new(128_000).unwrap();
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut chunk = vec![0u8; BYTES_PER_CHUNK];
        for (i, sample) in chunk.chunks_exact_mut(2).enumerate() {
            let v = ((i as f32 * 0.05).sin() * 8000.0) as i16;
            sample.copy_from_slice(&v.to_le_bytes());
        }
        let mut pcm = vec![0i16; SAMPLES_PER_CHUNK * CHANNELS as usize];
        for bitrate in [96_000, 128_000, 192_000, 256_000, 96_000] {
            opus.set_bitrate(bitrate).unwrap();
            assert_eq!(opus.bitrate_bps, bitrate);
            let packet = opus.encode(&chunk).unwrap().to_vec();
            assert!(packet.len() < 1400, "fits in one UDP chunk");
            let frames = decoder.decode(&packet, &mut pcm, false).unwrap();
            assert_eq!(frames, SAMPLES_PER_CHUNK);
        }
    }
}
