use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
/// The iPad sends 20 ms mic packets.
const MIC_PACKET_MS: f64 = 20.0;
/// The mic card is fed 10 ms at a time.
const MIC_TICK_SAMPLES: usize = 480;
/// How late a paced audio loop may fall behind its schedule before it gives up catching up
/// and restarts the schedule from now.
const MAX_LAG: Duration = Duration::from_millis(100);
/// libopus' recommended worst-case output buffer for one encode call.
const OPUS_MAX_PACKET: usize = 4000;
/// Redundancy distances: always the previous packet; after a reported burst also the one three
/// back, which together recover any burst of up to 3 lost packets.
const REDUNDANCY_NORMAL: &[u8] = &[1];
const REDUNDANCY_BURSTY: &[u8] = &[1, 3];
/// How long the bursty level stays after the last report of a burst.
const BURST_HOLD: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Shared pieces: clock following and jitter estimation
// ---------------------------------------------------------------------------

/// Turns jitter-buffer depth error into a small clock correction, so a paced loop runs at the
/// rate of the clock on the other side of the network. A PI controller: the proportional part
/// pulls the depth back with a ~20 s time constant, the integral part settles on the constant
/// rate difference between the two clocks. Large, fast errors are left to the jitter buffer.
#[derive(Debug, Default)]
pub struct PaceController {
    integral_ppm: f64,
    ppm: f64,
}

impl PaceController {
    /// ppm of correction per ms of depth error.
    const KP: f64 = 50.0;
    /// Integral time of 60 s.
    const KI: f64 = Self::KP / 60.0;
    const MAX_INTEGRAL_PPM: f64 = 400.0;
    pub const MAX_PPM: f64 = 500.0;
    /// Errors beyond this are the jitter buffer's to fix (it merges or inserts once the depth
    /// is 15 ms off), not the clock's.
    const MAX_ERROR_MS: f64 = 5.0;

    /// `error_ms` is depth minus target; `dt` the time since the last update. Returns the
    /// correction in ppm: positive means the consumer should run faster (or the producer
    /// slower) to bring the depth down.
    pub fn update(&mut self, error_ms: f64, dt: Duration) -> f64 {
        let error = error_ms.clamp(-Self::MAX_ERROR_MS, Self::MAX_ERROR_MS);
        let dt = dt.as_secs_f64().min(1.0);
        self.integral_ppm = (self.integral_ppm + Self::KI * error * dt)
            .clamp(-Self::MAX_INTEGRAL_PPM, Self::MAX_INTEGRAL_PPM);
        self.ppm = (Self::KP * error + self.integral_ppm).clamp(-Self::MAX_PPM, Self::MAX_PPM);
        self.ppm
    }

    pub fn ppm(&self) -> f64 {
        self.ppm
    }
}

/// `nominal` stretched or shrunk by `ppm` parts per million.
fn scaled(nominal: Duration, factor_ppm: f64) -> Duration {
    Duration::from_secs_f64(nominal.as_secs_f64() * (1.0 + factor_ppm * 1e-6))
}

/// Estimates how deep a jitter buffer must be from packet arrival times. Each packet's offset
/// (arrival time minus its nominal send time) is compared with the smallest offset in the last
/// few seconds; the 99th percentile of that extra delay, plus one packet, is the target. The
/// target rises as soon as the estimate does and falls by at most 2 ms per second, so a bursty
/// link doesn't make it bounce.
#[derive(Debug)]
pub struct JitterEstimator {
    offsets: VecDeque<f64>,
    capacity: usize,
    since_update: usize,
    update_every: usize,
    packet_ms: f64,
    floor_ms: f64,
    ceiling_ms: f64,
    target_ms: f64,
    last_update_ms: Option<f64>,
}

impl JitterEstimator {
    const WINDOW_MS: f64 = 4000.0;
    const UPDATE_MS: f64 = 500.0;
    const PERCENTILE: f64 = 0.99;
    const FALL_MS_PER_S: f64 = 2.0;

    pub fn new(packet_ms: f64, floor_ms: f64, ceiling_ms: f64, initial_ms: f64) -> Self {
        let capacity = (Self::WINDOW_MS / packet_ms) as usize;
        Self {
            offsets: VecDeque::with_capacity(capacity + 1),
            capacity,
            since_update: 0,
            update_every: (Self::UPDATE_MS / packet_ms) as usize,
            packet_ms,
            floor_ms,
            ceiling_ms,
            target_ms: initial_ms.clamp(floor_ms, ceiling_ms),
            last_update_ms: None,
        }
    }

    pub fn reset(&mut self) {
        self.offsets.clear();
        self.since_update = 0;
        self.last_update_ms = None;
    }

    /// `offset_ms` is the arrival time minus the packet's nominal time, on any fixed base.
    pub fn on_arrival(&mut self, offset_ms: f64, now_ms: f64) {
        self.offsets.push_back(offset_ms);
        if self.offsets.len() > self.capacity {
            self.offsets.pop_front();
        }
        self.since_update += 1;
        if self.since_update >= self.update_every {
            self.since_update = 0;
            self.recompute(now_ms);
        }
    }

    fn recompute(&mut self, now_ms: f64) {
        let min = self.offsets.iter().copied().fold(f64::INFINITY, f64::min);
        let mut extra: Vec<f64> = self.offsets.iter().map(|o| o - min).collect();
        extra.sort_by(f64::total_cmp);
        let index = ((extra.len() - 1) as f64 * Self::PERCENTILE).round() as usize;
        let wanted = (extra[index] + self.packet_ms).clamp(self.floor_ms, self.ceiling_ms);
        if wanted >= self.target_ms {
            self.target_ms = wanted;
        } else {
            let elapsed_s = self.last_update_ms.map_or(0.0, |t| (now_ms - t).max(0.0) / 1000.0);
            self.target_ms = wanted.max(self.target_ms - Self::FALL_MS_PER_S * elapsed_s);
        }
        self.last_update_ms = Some(now_ms);
    }

    pub fn target_ms(&self) -> f64 {
        self.target_ms
    }
}

/// Best effort: a steady 10 ms schedule matters more than anything else these threads do.
fn make_realtime(label: &str) {
    // SAFETY: plain syscalls on the calling thread with valid arguments.
    unsafe {
        libc::prctl(libc::PR_SET_TIMERSLACK, 1 as libc::c_ulong, 0, 0, 0);
        let param = libc::sched_param { sched_priority: 10 };
        if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) != 0 {
            crate::log_detail!(
                "[{label}] real-time priority unavailable: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Sleeps until `next`, advancing it by `period` first. Restarts the schedule if it fell more
/// than `MAX_LAG` behind.
fn wait_for_tick(next: &mut Instant, period: Duration, label: &str) {
    *next += period;
    let now = Instant::now();
    if *next > now {
        std::thread::sleep(*next - now);
    } else if now - *next > MAX_LAG {
        crate::log_detail!("[{label}] loop stalled {:?}; resuming", now - *next);
        *next = now;
    }
}

// ---------------------------------------------------------------------------
// Speakers — reads the iPad's speaker card, sends it to the iPad
// ---------------------------------------------------------------------------

/// The iPad's speaker playout report (UDP `AUDIO_REPORT`), see docs/ARCHITECTURE.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioReport {
    pub generation: u32,
    pub depth_us: u32,
    pub target_us: u32,
    pub due: u16,
    pub primary_lost: u16,
    pub recovered: u16,
    pub concealed: u16,
    pub late: u16,
    pub max_burst: u8,
    /// The iPad's jitter estimate before its render quantum and video cap.
    pub jitter_us: u32,
    /// The video cap on the iPad's target; 0 when there is none.
    pub cap_us: u32,
}

impl AudioReport {
    pub const LEN: usize = 31;

    pub fn parse(body: &[u8]) -> Option<Self> {
        if body.len() != Self::LEN {
            return None;
        }
        let u32_at = |i: usize| u32::from_be_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
        let u16_at = |i: usize| u16::from_be_bytes([body[i], body[i + 1]]);
        Some(Self {
            generation: u32_at(0),
            depth_us: u32_at(4),
            target_us: u32_at(8),
            due: u16_at(12),
            primary_lost: u16_at(14),
            recovered: u16_at(16),
            concealed: u16_at(18),
            late: u16_at(20),
            max_burst: body[22],
            jitter_us: u32_at(23),
            cap_us: u32_at(27),
        })
    }
}

/// What the speaker loop does with the iPad's reports: follow its clock, pick redundancy, and
/// tell the encoder the loss rate.
#[derive(Debug, Default)]
struct SpeakerControl {
    pace: PaceController,
    last_report: Option<Instant>,
    bursty_until: Option<Instant>,
    loss_fraction: f64,
    /// Totals since the last periodic log line.
    window: ReportTotals,
    last_report_seen: Option<AudioReport>,
}

#[derive(Debug, Default)]
struct ReportTotals {
    due: u32,
    primary_lost: u32,
    recovered: u32,
    concealed: u32,
    late: u32,
    max_burst: u8,
}

impl SpeakerControl {
    fn on_report(&mut self, report: &AudioReport, now: Instant) {
        let dt = self
            .last_report
            .map_or(Duration::from_millis(200), |t| now.saturating_duration_since(t));
        self.last_report = Some(now);
        let error_ms = (f64::from(report.depth_us) - f64::from(report.target_us)) / 1000.0;
        self.pace.update(error_ms, dt);
        if report.max_burst >= 2 {
            self.bursty_until = Some(now + BURST_HOLD);
        }
        if report.due > 0 {
            let loss = f64::from(report.primary_lost.min(report.due)) / f64::from(report.due);
            self.loss_fraction = 0.8 * self.loss_fraction + 0.2 * loss;
        }
        self.window.due += u32::from(report.due);
        self.window.primary_lost += u32::from(report.primary_lost);
        self.window.recovered += u32::from(report.recovered);
        self.window.concealed += u32::from(report.concealed);
        self.window.late += u32::from(report.late);
        self.window.max_burst = self.window.max_burst.max(report.max_burst);
        self.last_report_seen = Some(*report);
    }

    /// The read interval: positive ppm (iPad too deep) reads slower, so the card and the
    /// stream run at the rate the iPad plays.
    fn read_period(&self) -> Duration {
        scaled(Duration::from_millis(u64::from(CHUNK_DURATION_MS)), self.pace.ppm())
    }

    fn redundancy(&self, now: Instant) -> &'static [u8] {
        match self.bursty_until {
            Some(until) if now < until => REDUNDANCY_BURSTY,
            _ => REDUNDANCY_NORMAL,
        }
    }

    /// For `OPUS_SET_PACKET_LOSS_PERC`.
    fn loss_percent(&self) -> i32 {
        (self.loss_fraction * 100.0).round().clamp(0.0, 50.0) as i32
    }

    fn log_and_reset_window(&mut self, now: Instant) {
        if let Some(r) = self.last_report_seen {
            let w = &self.window;
            let ms = |us: u32| f64::from(us) / 1000.0;
            let cap = if r.cap_us == 0 { "none".to_string() } else { format!("{:.1} ms", ms(r.cap_us)) };
            crate::log_detail!(
                "[audio] iPad depth {:.1}/{:.1} ms (jitter {:.1} ms, video cap {cap}), clock {:+.0} ppm, \
                 redundancy {:?}, loss hint {}%; last 5 s: {} due, {} lost, {} recovered, \
                 {} concealed, {} late, longest burst {}",
                ms(r.depth_us),
                ms(r.target_us),
                ms(r.jitter_us),
                -self.pace.ppm(),
                self.redundancy(now),
                self.loss_percent(),
                w.due,
                w.primary_lost,
                w.recovered,
                w.concealed,
                w.late,
                w.max_burst
            );
        }
        self.window = ReportTotals::default();
    }
}

/// Speaker-audio Opus encoder plus its reusable scratch buffers.
struct SpeakerOpus {
    encoder: opus::Encoder,
    bitrate_bps: u32,
    loss_percent: i32,
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
        // No in-band FEC: it only exists in Opus' speech modes, not at music bitrates; redundant
        // copies replace it. No DTX: the client's jitter buffer expects a packet every 10 ms.
        let _ = encoder.set_inband_fec(false);
        let _ = encoder.set_dtx(false);
        Ok(Self {
            encoder,
            bitrate_bps,
            loss_percent: 0,
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

    /// Makes packets depend less on earlier ones while packets are being lost.
    fn set_loss_percent(&mut self, percent: i32) {
        if self.loss_percent != percent && self.encoder.set_packet_loss_perc(percent).is_ok() {
            self.loss_percent = percent;
        }
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

/// Speakers and microphone need the `screx_snd` kernel module.
pub fn probe_mic_available() -> bool {
    screx_snd::probe_available()
}

pub fn probe_speaker_available() -> bool {
    screx_snd::probe_available()
}

/// While a client has speakers on, plugs in a speaker card named after the iPad, reads 10 ms
/// of what applications play into it about every 10 ms, and sends it to the iPad as Opus.
/// The read interval follows the iPad's playback clock through its `AUDIO_REPORT`s. Unplugs the
/// card when speakers go off or the client leaves.
pub fn run_speaker(shared: Arc<SharedState>, stop: Arc<AtomicBool>) -> Result<()> {
    make_realtime("audio");
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

        // Speakers are only on while a session is authenticated, so this is a momentary race at
        // session teardown; wait for the next notification instead of spinning.
        let Some(media) = shared.quic_media.lock().unwrap().clone() else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let speaker_session = shared.network_session_id.load(Ordering::Acquire);
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
        // A fresh sender per card session: a new generation, and no copies of earlier audio.
        let mut sender = AudioSender::from_quic(media);
        shared.audio_reports.lock().unwrap().clear();
        let mut control = SpeakerControl::default();
        let mut last_log = Instant::now();
        let mut chunk = [0u8; BYTES_PER_CHUNK];
        let mut next = Instant::now();
        while speakers_on()
            && shared.network_session_id.load(Ordering::Acquire) == speaker_session
            && !stop.load(Ordering::Relaxed)
        {
            // This schedule is the card's clock.
            wait_for_tick(&mut next, control.read_period(), "audio");
            if let Err(e) = card.read(&mut chunk) {
                eprintln!("[audio] {e:#}");
                break;
            }

            let now = Instant::now();
            let generation = sender.generation();
            for report in shared.audio_reports.lock().unwrap().drain(..) {
                if report.generation == generation {
                    control.on_report(&report, now);
                }
            }
            if now - last_log >= Duration::from_secs(5) {
                control.log_and_reset_window(now);
                last_log = now;
            }

            // Stamped with when the chunk was due to play, on the video timestamps' clock.
            let ts = next.saturating_duration_since(shared.start_time).as_millis() as u32;
            speaker_opus.set_loss_percent(control.loss_percent());
            let redundancy = control.redundancy(now);
            let result = speaker_opus
                .set_bitrate(shared.audio_bitrate_bps.load(Ordering::Relaxed))
                .and_then(|()| speaker_opus.encode(&chunk))
                .and_then(|packet| sender.send_audio(packet, ts, redundancy));
            if let Err(e) = result {
                eprintln!("[audio] send error: {e:#}");
            }
        }
        drop(card);
        crate::log_detail!("[audio] speaker card unplugged");
    }

    crate::log_detail!("[audio] speaker thread exiting");
    Ok(())
}

// ---------------------------------------------------------------------------
// Microphone — receives Opus from iPad, buffers it, feeds the microphone card on a steady clock
// ---------------------------------------------------------------------------

/// Sink for decoded microphone PCM.
pub trait MicSink: Send {
    fn write_pcm(&mut self, s16_mono_48k: &[i16]);
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

/// What the paced mic loop should produce next.
#[derive(Debug, PartialEq, Eq)]
enum MicAction {
    /// Decode this packet.
    Decode(Vec<u8>),
    /// Fill one packet with Opus packet-loss concealment.
    Conceal,
    /// Decode two consecutive packets (or conceal the missing one) and crossfade them into one
    /// packet's length, to drain excess depth without a click.
    Merge(Option<Vec<u8>>, Option<Vec<u8>>),
    /// Nothing to play yet; write one tick of silence.
    Silence,
}

/// The mic jitter buffer: Opus packets by sequence number, played in order on the mic loop's
/// clock. See "Microphone forwarding" in docs/ARCHITECTURE.md.
struct MicJitter {
    packets: HashMap<u32, Vec<u8>>,
    /// Sequence number to play next; `None` while (re)filling.
    next: Option<u32>,
    highest: Option<u32>,
    estimator: JitterEstimator,
    smoothed_depth_ms: f64,
    concealed_run: u32,
    packets_since_adjust: u32,
    late: u64,
}

impl MicJitter {
    const FLOOR_MS: f64 = 30.0;
    const CEILING_MS: f64 = 200.0;
    const INITIAL_MS: f64 = 40.0;
    /// Concealment allowed with nothing buffered before going silent.
    const MAX_CONCEALED_RUN: u32 = 3;
    /// A jump this large (2 s) is a restarted stream, not loss.
    const RESYNC_GAP: i32 = 100;
    const MAX_PACKETS: usize = 64;
    /// Merge or insert once the smoothed depth is this far from the target (or this fraction
    /// of it, if larger); smaller errors are clock drift, which the pace follows.
    const ADJUST_BAND_MS: f64 = 20.0;
    const ADJUST_BAND_FRACTION: f64 = 0.2;
    /// At most one merge or insert per this many packets (200 ms).
    const ADJUST_COOLDOWN: u32 = 10;

    fn new() -> Self {
        Self {
            packets: HashMap::new(),
            next: None,
            highest: None,
            estimator: JitterEstimator::new(
                MIC_PACKET_MS,
                Self::FLOOR_MS,
                Self::CEILING_MS,
                Self::INITIAL_MS,
            ),
            smoothed_depth_ms: 0.0,
            concealed_run: 0,
            packets_since_adjust: 0,
            late: 0,
        }
    }

    fn reset(&mut self) {
        self.packets.clear();
        self.next = None;
        self.highest = None;
        self.estimator.reset();
        self.concealed_run = 0;
    }

    /// Signed distance from `b` to `a`.
    fn diff(a: u32, b: u32) -> i32 {
        a.wrapping_sub(b) as i32
    }

    fn insert(&mut self, seq: u32, opus: &[u8], now_ms: f64) {
        if let Some(highest) = self.highest {
            if Self::diff(seq, highest).abs() > Self::RESYNC_GAP {
                self.reset();
            }
        }
        if let Some(next) = self.next {
            if Self::diff(seq, next) < 0 {
                self.late += 1;
                return;
            }
        }
        if self.packets.contains_key(&seq) {
            return;
        }
        self.estimator
            .on_arrival(now_ms - f64::from(seq) * MIC_PACKET_MS, now_ms);
        self.packets.insert(seq, opus.to_vec());
        if self.highest.is_none_or(|h| Self::diff(seq, h) > 0) {
            self.highest = Some(seq);
        }
        if self.packets.len() > Self::MAX_PACKETS {
            let base = self.next.unwrap_or(seq);
            if let Some(&oldest) = self.packets.keys().min_by_key(|&&k| Self::diff(k, base)) {
                self.packets.remove(&oldest);
            }
        }
    }

    /// Packets from `next` through the newest received, holes included.
    fn depth_packets(&self) -> i32 {
        match (self.next, self.highest) {
            (Some(next), Some(highest)) => (Self::diff(highest, next) + 1).max(0),
            _ => 0,
        }
    }

    /// Current depth minus target, for the pace controller.
    fn depth_error_ms(&self) -> f64 {
        if self.next.is_none() {
            return 0.0;
        }
        self.smoothed_depth_ms - self.estimator.target_ms()
    }

    /// Called whenever the loop needs another packet's worth of audio. `queued_ms` is decoded
    /// audio not yet written to the card.
    fn pull(&mut self, queued_ms: f64) -> MicAction {
        let target = self.estimator.target_ms();
        if self.next.is_none() && !self.start(target) {
            return MicAction::Silence;
        }
        let Some(next) = self.next else {
            return MicAction::Silence;
        };
        let depth = self.depth_packets();
        let depth_ms = f64::from(depth) * MIC_PACKET_MS + queued_ms;
        self.smoothed_depth_ms += 0.05 * (depth_ms - self.smoothed_depth_ms);

        if depth == 0 {
            self.concealed_run += 1;
            if self.concealed_run > Self::MAX_CONCEALED_RUN {
                self.next = None;
                return MicAction::Silence;
            }
            self.next = Some(next.wrapping_add(1));
            return MicAction::Conceal;
        }
        self.concealed_run = 0;
        self.packets_since_adjust += 1;

        let band = Self::ADJUST_BAND_MS.max(Self::ADJUST_BAND_FRACTION * target);
        if self.packets_since_adjust >= Self::ADJUST_COOLDOWN {
            if self.smoothed_depth_ms > target + band && depth >= 2 {
                self.packets_since_adjust = 0;
                let first = self.packets.remove(&next);
                let second = self.packets.remove(&next.wrapping_add(1));
                self.next = Some(next.wrapping_add(2));
                self.smoothed_depth_ms -= MIC_PACKET_MS;
                return MicAction::Merge(first, second);
            }
            if self.smoothed_depth_ms < target - band {
                self.packets_since_adjust = 0;
                self.smoothed_depth_ms += MIC_PACKET_MS;
                return MicAction::Conceal;
            }
        }

        self.next = Some(next.wrapping_add(1));
        match self.packets.remove(&next) {
            Some(opus) => MicAction::Decode(opus),
            None => MicAction::Conceal,
        }
    }

    /// Starts playback once the buffered span reaches the target, skipping anything older
    /// than the target behind the newest packet.
    fn start(&mut self, target_ms: f64) -> bool {
        let Some(highest) = self.highest else {
            return false;
        };
        let Some(&lowest) = self.packets.keys().min_by_key(|&&k| Self::diff(k, highest)) else {
            return false;
        };
        let span = Self::diff(highest, lowest) + 1;
        let target_packets = (target_ms / MIC_PACKET_MS).ceil() as i32;
        if span < target_packets {
            return false;
        }
        let first = highest.wrapping_sub((target_packets - 1).max(0) as u32);
        let first = if Self::diff(first, lowest) < 0 { lowest } else { first };
        self.packets.retain(|&k, _| Self::diff(k, first) >= 0);
        self.next = Some(first);
        self.smoothed_depth_ms = f64::from(Self::diff(highest, first) + 1) * MIC_PACKET_MS;
        self.concealed_run = 0;
        self.packets_since_adjust = 0;
        true
    }
}

/// Plugs in the iPad's microphone, named after the iPad, and starts the paced loop that feeds
/// it. Dropping the writer stops the loop and unplugs the card.
pub fn create_virtual_mic(name: &str) -> Result<MicWriter> {
    let card = Card::create(&format!("{name} Microphone"), 1, Direction::Microphone)?;
    crate::log_detail!("[mic] microphone card plugged in");
    MicWriter::start(Box::new(MicCard {
        card,
        bytes: Vec::with_capacity(MIC_TICK_SAMPLES * 2),
    }))
}

pub struct MicWriter {
    jitter: Arc<Mutex<MicJitter>>,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MicWriter {
    fn start(sink: Box<dyn MicSink>) -> Result<Self> {
        // Fail here rather than in the thread if Opus can't initialize.
        drop(OpusDecoder::new(MIC_RATE, 1).map_err(|e| anyhow::anyhow!("opus decoder init: {e:?}"))?);
        let jitter = Arc::new(Mutex::new(MicJitter::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("mic".into())
            .spawn({
                let jitter = Arc::clone(&jitter);
                let stop = Arc::clone(&stop);
                move || run_mic(jitter, sink, stop)
            })?;
        Ok(Self {
            jitter,
            epoch: Instant::now(),
            stop,
            thread: Some(thread),
        })
    }

    /// Queues one mic packet. `seq` is the mic-only sequence number: missing numbers are lost
    /// packets, which the loop fills with concealment; packets that arrive after their turn
    /// are dropped.
    pub fn write_opus_packet(&mut self, seq: u32, opus_data: &[u8]) -> Result<()> {
        let now_ms = self.epoch.elapsed().as_secs_f64() * 1000.0;
        self.jitter.lock().unwrap().insert(seq, opus_data, now_ms);
        Ok(())
    }
}

impl Drop for MicWriter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Decodes one packet, or conceals one if `opus` is `None` or empty. Returns the samples.
fn decode_mic(decoder: &mut OpusDecoder, opus: Option<&[u8]>, pcm: &mut [i16]) -> usize {
    match decoder.decode(opus.unwrap_or(&[]), pcm, false) {
        Ok(samples) => samples,
        Err(e) => {
            crate::log_trace!("[mic] opus decode: {e:?}");
            0
        }
    }
}

/// Crossfades `a` into `b` over their common length, writing into `a`.
fn crossfade(a: &mut [i16], b: &[i16]) -> usize {
    let n = a.len().min(b.len());
    for i in 0..n {
        let w = i as f32 / n as f32;
        a[i] = (f32::from(a[i]) * (1.0 - w) + f32::from(b[i]) * w) as i16;
    }
    n
}

/// The paced mic loop: writes 10 ms to the card per tick, on a schedule that follows the
/// iPad's microphone clock by holding the jitter buffer at its target.
fn run_mic(jitter: Arc<Mutex<MicJitter>>, mut sink: Box<dyn MicSink>, stop: Arc<AtomicBool>) {
    make_realtime("mic");
    let mut decoder = match OpusDecoder::new(MIC_RATE, 1) {
        Ok(decoder) => decoder,
        Err(e) => {
            eprintln!("[mic] opus decoder init: {e:?}");
            return;
        }
    };
    let mut pcm = vec![0i16; MIC_MAX_FRAME];
    let mut second = vec![0i16; MIC_MAX_FRAME];
    let mut queued: VecDeque<i16> = VecDeque::with_capacity(MIC_MAX_FRAME * 2);
    let mut tick = vec![0i16; MIC_TICK_SAMPLES];
    let mut pace = PaceController::default();
    let nominal = Duration::from_millis(10);
    let mut period = nominal;
    let mut next = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        wait_for_tick(&mut next, period, "mic");

        while queued.len() < MIC_TICK_SAMPLES {
            let queued_ms = queued.len() as f64 / f64::from(MIC_RATE) * 1000.0;
            let action = jitter.lock().unwrap().pull(queued_ms);
            let samples = match action {
                MicAction::Decode(opus) => decode_mic(&mut decoder, Some(&opus), &mut pcm),
                MicAction::Conceal => decode_mic(&mut decoder, None, &mut pcm),
                MicAction::Merge(a, b) => {
                    let n = decode_mic(&mut decoder, a.as_deref(), &mut pcm);
                    let m = decode_mic(&mut decoder, b.as_deref(), &mut second);
                    crossfade(&mut pcm[..n], &second[..m])
                }
                MicAction::Silence => break,
            };
            if samples == 0 {
                // Decoder failure: don't spin; play silence this tick.
                break;
            }
            queued.extend(&pcm[..samples]);
        }

        // Whatever is decoded, then silence if the buffer is (re)filling.
        tick.fill(0);
        let take = queued.len().min(MIC_TICK_SAMPLES);
        for (dst, src) in tick.iter_mut().zip(queued.drain(..take)) {
            *dst = src;
        }
        sink.write_pcm(&tick);

        let error_ms = jitter.lock().unwrap().depth_error_ms();
        // Positive: too deep, so write faster.
        period = scaled(nominal, -pace.update(error_ms, period));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_report_round_trips_the_documented_layout() {
        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_be_bytes());
        body.extend_from_slice(&25_000u32.to_be_bytes());
        body.extend_from_slice(&20_000u32.to_be_bytes());
        for v in [20u16, 3, 2, 1, 4] {
            body.extend_from_slice(&v.to_be_bytes());
        }
        body.push(2);
        for v in [15_000u32, 0] {
            body.extend_from_slice(&v.to_be_bytes());
        }
        // Same bytes as `reportEncoding` in client/Tests/AudioJitterBufferTests.swift.
        assert_eq!(
            body,
            [
                0, 0, 0, 7, 0, 0, 0x61, 0xA8, 0, 0, 0x4E, 0x20, 0, 20, 0, 3, 0, 2, 0, 1, 0, 4, 2, 0,
                0, 0x3A, 0x98, 0, 0, 0, 0
            ]
        );
        let report = AudioReport::parse(&body).unwrap();
        assert_eq!(
            report,
            AudioReport {
                generation: 7,
                depth_us: 25_000,
                target_us: 20_000,
                due: 20,
                primary_lost: 3,
                recovered: 2,
                concealed: 1,
                late: 4,
                max_burst: 2,
                jitter_us: 15_000,
                cap_us: 0,
            }
        );
        assert!(AudioReport::parse(&body[..30]).is_none());
    }

    #[test]
    fn pace_settles_on_a_constant_clock_difference() {
        // The iPad plays 100 ppm faster than the daemon reads: depth falls unless the daemon
        // reads faster. Simulate depth under the controller's correction.
        let mut pace = PaceController::default();
        let dt = Duration::from_millis(200);
        let mut error_ms = 0.0;
        for _ in 0..3000 {
            let producer_ppm = -pace.update(error_ms, dt);
            error_ms += (producer_ppm - 100.0) * 1e-6 * dt.as_secs_f64() * 1000.0;
        }
        assert!(error_ms.abs() < 0.5, "error {error_ms}");
        assert!((pace.ppm() + 100.0).abs() < 5.0, "ppm {}", pace.ppm());
    }

    #[test]
    fn pace_is_bounded() {
        let mut pace = PaceController::default();
        for _ in 0..1000 {
            pace.update(1000.0, Duration::from_secs(1));
        }
        assert_eq!(pace.ppm(), PaceController::MAX_PPM);
    }

    #[test]
    fn jitter_target_rises_at_once_and_falls_slowly() {
        let mut est = JitterEstimator::new(10.0, 15.0, 200.0, 15.0);
        let mut now = 0.0;
        // Steady arrivals: target stays at the floor.
        for _ in 0..400 {
            now += 10.0;
            est.on_arrival(0.0, now);
        }
        assert_eq!(est.target_ms(), 15.0);
        // 10% of packets 50 ms late: target jumps to cover them.
        for i in 0..100 {
            now += 10.0;
            est.on_arrival(if i % 10 == 0 { 50.0 } else { 0.0 }, now);
        }
        assert!(est.target_ms() >= 60.0, "target {}", est.target_ms());
        let high = est.target_ms();
        // Clean for 4 s: the late packets are still in the window, so it holds.
        for _ in 0..390 {
            now += 10.0;
            est.on_arrival(0.0, now);
        }
        assert_eq!(est.target_ms(), high);
        // 2 s after they left the window: down by about 4 ms, not straight to the floor.
        for _ in 0..210 {
            now += 10.0;
            est.on_arrival(0.0, now);
        }
        assert!(est.target_ms() < high && est.target_ms() > high - 6.0, "target {}", est.target_ms());
    }

    fn packet(n: u8) -> Vec<u8> {
        vec![n]
    }

    /// Feeds packets `seqs` 20 ms apart starting at `t`, then returns the jitter buffer.
    fn feed(j: &mut MicJitter, seqs: impl IntoIterator<Item = u32>, t: &mut f64) {
        for seq in seqs {
            j.insert(seq, &packet(seq as u8), *t);
            *t += MIC_PACKET_MS;
        }
    }

    #[test]
    fn mic_waits_for_target_then_plays_in_order() {
        let mut j = MicJitter::new();
        let mut t = 0.0;
        feed(&mut j, [10], &mut t);
        assert_eq!(j.pull(0.0), MicAction::Silence);
        feed(&mut j, [11], &mut t);
        assert_eq!(j.pull(0.0), MicAction::Decode(packet(10)));
        feed(&mut j, [12], &mut t);
        assert_eq!(j.pull(0.0), MicAction::Decode(packet(11)));
    }

    #[test]
    fn mic_conceals_holes_and_drops_late_packets() {
        let mut j = MicJitter::new();
        let mut t = 0.0;
        feed(&mut j, [0, 1], &mut t);
        assert_eq!(j.pull(0.0), MicAction::Decode(packet(0)));
        feed(&mut j, [3], &mut t); // 2 lost (so far)
        assert_eq!(j.pull(0.0), MicAction::Decode(packet(1)));
        assert_eq!(j.pull(0.0), MicAction::Conceal);
        j.insert(2, &packet(2), t); // too late now
        assert_eq!(j.late, 1);
        assert_eq!(j.pull(0.0), MicAction::Decode(packet(3)));
    }

    #[test]
    fn mic_underrun_conceals_briefly_then_goes_silent_and_refills() {
        let mut j = MicJitter::new();
        let mut t = 0.0;
        feed(&mut j, [0, 1], &mut t);
        assert_eq!(j.pull(0.0), MicAction::Decode(packet(0)));
        assert_eq!(j.pull(0.0), MicAction::Decode(packet(1)));
        for _ in 0..MicJitter::MAX_CONCEALED_RUN {
            assert_eq!(j.pull(0.0), MicAction::Conceal);
        }
        assert_eq!(j.pull(0.0), MicAction::Silence);
        // The stream comes back later; it refills before playing again.
        feed(&mut j, [10], &mut t);
        assert_eq!(j.pull(0.0), MicAction::Silence);
        feed(&mut j, [11], &mut t);
        assert_eq!(j.pull(0.0), MicAction::Decode(packet(10)));
    }

    #[test]
    fn mic_merges_when_far_too_deep() {
        let mut j = MicJitter::new();
        let mut t = 0.0;
        feed(&mut j, [0, 1], &mut t);
        assert_eq!(j.pull(0.0), MicAction::Decode(packet(0)));
        // A burst of 20 packets at once.
        for seq in 2..22 {
            j.insert(seq, &packet(seq as u8), t);
        }
        let mut merged = false;
        for _ in 0..40 {
            if let MicAction::Merge(..) = j.pull(0.0) {
                merged = true;
                break;
            }
        }
        assert!(merged);
    }

    #[test]
    fn mic_resyncs_after_a_large_jump() {
        let mut j = MicJitter::new();
        let mut t = 0.0;
        feed(&mut j, [0, 1], &mut t);
        assert_eq!(j.pull(0.0), MicAction::Decode(packet(0)));
        feed(&mut j, [5000, 5001], &mut t);
        assert_eq!(j.pull(0.0), MicAction::Decode(packet(5000u32 as u8)));
    }

    #[test]
    fn crossfade_starts_on_the_first_and_ends_near_the_second() {
        let mut a = vec![1000i16; 100];
        let b = vec![-1000i16; 100];
        crossfade(&mut a, &b);
        assert_eq!(a[0], 1000);
        assert!(a[99] < -900);
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
        opus.set_loss_percent(10);
        assert_eq!(opus.loss_percent, 10);
        for bitrate in [96_000, 128_000, 192_000, 256_000, 96_000] {
            opus.set_bitrate(bitrate).unwrap();
            assert_eq!(opus.bitrate_bps, bitrate);
            let packet = opus.encode(&chunk).unwrap().to_vec();
            // The primary plus both redundant copies fit one datagram at the top bitrate.
            assert!(3 * (packet.len() + 3) + 1 < 1400, "{} bytes", packet.len());
            let frames = decoder.decode(&packet, &mut pcm, false).unwrap();
            assert_eq!(frames, SAMPLES_PER_CHUNK);
        }
    }

    #[test]
    fn speaker_control_reacts_to_reports() {
        let mut control = SpeakerControl::default();
        let now = Instant::now();
        assert_eq!(control.redundancy(now), REDUNDANCY_NORMAL);
        assert_eq!(control.read_period(), Duration::from_millis(10));
        let report = AudioReport {
            generation: 1,
            depth_us: 30_000,
            target_us: 20_000,
            due: 20,
            primary_lost: 4,
            recovered: 3,
            concealed: 1,
            late: 0,
            max_burst: 2,
            jitter_us: 10_000,
            cap_us: 0,
        };
        control.on_report(&report, now);
        assert_eq!(control.redundancy(now), REDUNDANCY_BURSTY);
        assert_eq!(control.redundancy(now + BURST_HOLD), REDUNDANCY_NORMAL);
        // Too deep: read slower.
        assert!(control.read_period() > Duration::from_millis(10));
        assert!(control.read_period() <= scaled(Duration::from_millis(10), PaceController::MAX_PPM));
        assert_eq!(control.loss_percent(), 4); // 0.2 * 20%
    }
}
