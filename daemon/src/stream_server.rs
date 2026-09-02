use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::audio::MicWriter;
use crate::camera::{CamReassembler, CamWriter, CameraConfig};
use crate::encode::{EncodedAccessUnit, EncoderBackend, VideoCodec};
use crate::input::{
    parse_key_event, parse_mouse_packet, parse_rawkey_event, parse_touch_packet, InputBackend,
    KeyboardWorker,
};
use crate::platform::net::{
    build_pktinfo_cmsg, send_to_from, sendmmsg_batch, udp_source_for_local_ip, PktinfoCmsg,
    UdpSource,
};

const CHUNK_PAYLOAD: usize = 1400;
const HEADER_LEN: usize = 18;
const REGISTER_MAGIC: &[u8] = b"SCREX";
const PLI_MAGIC: &[u8] = b"PLI";
const TOUCH_MAGIC: &[u8] = b"TOUCH";
const KEY_MAGIC: &[u8] = b"KEY";
const CAM_MAGIC: &[u8] = b"CAM";
const MIC_MAGIC: &[u8] = b"MIC";
const MOUSE_MAGIC: &[u8] = b"MOUSE";
const RAWKEY_MAGIC: &[u8] = b"RAWKEY";
const PERIPH_MAGIC: &[u8] = b"PERIPH";
const SPEAKER_MAGIC: &[u8] = b"SPKR";
const MICCFG_MAGIC: &[u8] = b"MICCFG";
const CAMCFG_MAGIC: &[u8] = b"CAMCFG";
const STNG_MAGIC: &[u8] = b"STNG";

// Capability-negotiation wire format (daemon -> client `CAPS`, client ->
// daemon `STNG`). See docs/ARCHITECTURE.md "Capability Negotiation" for the
// canonical spec both clients implement independently.
const CAPS_MAGIC: &[u8] = b"CAPS";
const CAPS_VERSION: u8 = 1;
const CAPS_TAG_CAMERA: u8 = 0x01;
const CAPS_TAG_MICROPHONE: u8 = 0x02;
const CAPS_TAG_SPEAKER: u8 = 0x03;
// 0x04 (GAMEPAD) is retired: this daemon does not do controller passthrough.
// The tag value stays reserved — clients skip tags they don't recognize, so
// reusing it for something else would silently misparse on older clients.
const CAPS_TAG_CODECS: u8 = 0x05;
const CAPS_TAG_MAX_RESOLUTION: u8 = 0x06;
const CAPS_TAG_MAX_FRAMERATE: u8 = 0x07;
const CAPS_TAG_BITRATE_RANGE: u8 = 0x08;
const CAPS_ENTRY_COUNT: u8 = 7;

const STNG_TAG_RESOLUTION: u8 = 0x01;
const STNG_TAG_FRAMERATE: u8 = 0x02;
const STNG_TAG_CODEC: u8 = 0x03;
const STNG_TAG_BITRATE: u8 = 0x04;

/// Validation/clamping bounds for client-proposed `STNG` settings (see the
/// plan's "Validation/clamping bounds" section). The bitrate floor is fixed,
/// not operator-configurable; the ceilings come from the daemon's
/// `--max-*` CLI flags, stored on `SharedState`.
const STNG_RESOLUTION_FLOOR_W: u32 = 640;
const STNG_RESOLUTION_FLOOR_H: u32 = 360;
const STNG_FPS_FLOOR: u32 = 15;
const STNG_BITRATE_FLOOR_BPS: u32 = 500_000;

const PENDING_SESSION_TIMEOUT: Duration = Duration::from_secs(3);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT_MAGIC: &[u8] = b"SCREX_HB";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_FEC_SHARDS: usize = 127;
const MAX_PACKET_LEN: usize = HEADER_LEN + CHUNK_PAYLOAD + crate::crypto::TAG_LEN;
const IDR_FEC_PERCENT: usize = 10;
const PFRAME_FEC_PERCENT: usize = 5;
const MODERATE_IDR_FEC_PERCENT: usize = 14;
const MODERATE_PFRAME_FEC_PERCENT: usize = 7;
const SEVERE_IDR_FEC_PERCENT: usize = 18;
const SEVERE_PFRAME_FEC_PERCENT: usize = 9;
const ADAPTIVE_ADJUST_INTERVAL: Duration = Duration::from_secs(1);
const ADAPTIVE_SHORT_WINDOW: Duration = Duration::from_secs(2);
const ADAPTIVE_LONG_WINDOW: Duration = Duration::from_secs(6);
const SENDMMSG_BATCH_SIZE: usize = 32;

fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn should_log_debug(counter: u64) -> bool {
    counter <= 12 || counter.is_power_of_two() || counter % 25 == 0
}

fn seq_is_stale(seq_num: u32, expected_seq: u32) -> bool {
    let diff = seq_num.wrapping_sub(expected_seq);
    diff > 0x8000_0000
}

pub type LifecycleCallback = Box<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct StreamTuning {
    pub bitrate_bps: u32,
    pub idr_fec_percent: usize,
    pub pframe_fec_percent: usize,
}

#[derive(Debug)]
struct AdaptiveStreamState {
    base_bitrate_bps: u32,
    current_bitrate_bps: u32,
    idr_fec_percent: usize,
    pframe_fec_percent: usize,
    pli_events: VecDeque<Instant>,
    last_adjust_at: Instant,
}

impl AdaptiveStreamState {
    fn new(base_bitrate_bps: u32) -> Self {
        Self {
            base_bitrate_bps,
            current_bitrate_bps: base_bitrate_bps,
            idr_fec_percent: IDR_FEC_PERCENT,
            pframe_fec_percent: PFRAME_FEC_PERCENT,
            pli_events: VecDeque::new(),
            last_adjust_at: Instant::now(),
        }
    }

    fn reset(&mut self) {
        self.current_bitrate_bps = self.base_bitrate_bps;
        self.idr_fec_percent = IDR_FEC_PERCENT;
        self.pframe_fec_percent = PFRAME_FEC_PERCENT;
        self.pli_events.clear();
        self.last_adjust_at = Instant::now();
    }

    /// Rebaseline the adaptive bitrate loop around a new base (e.g. a
    /// client-negotiated `STNG` bitrate, or the CLI default restored when a
    /// session ends). The adaptive loop in `adjust()` throttles up/down from
    /// `base_bitrate_bps`, so changing it without also resetting the current
    /// value would leave the stream ramping from the old client's bitrate.
    fn set_base(&mut self, base_bitrate_bps: u32) {
        self.base_bitrate_bps = base_bitrate_bps;
        self.reset();
    }

    fn note_pli(&mut self, now: Instant) {
        self.pli_events.push_back(now);
        self.prune(now);
    }

    fn current_tuning(&mut self, now: Instant) -> StreamTuning {
        self.prune(now);

        if now.duration_since(self.last_adjust_at) >= ADAPTIVE_ADJUST_INTERVAL {
            self.adjust(now);
        }

        StreamTuning {
            bitrate_bps: self.current_bitrate_bps,
            idr_fec_percent: self.idr_fec_percent,
            pframe_fec_percent: self.pframe_fec_percent,
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .pli_events
            .front()
            .is_some_and(|ts| now.duration_since(*ts) > ADAPTIVE_LONG_WINDOW)
        {
            self.pli_events.pop_front();
        }
    }

    fn adjust(&mut self, now: Instant) {
        let recent_pli = self
            .pli_events
            .iter()
            .rev()
            .take_while(|ts| now.duration_since(**ts) <= ADAPTIVE_SHORT_WINDOW)
            .count();
        let long_pli = self.pli_events.len();
        let min_bitrate_bps = (self.base_bitrate_bps / 3).max(2_000_000);
        let old_bitrate = self.current_bitrate_bps;
        let old_idr_fec = self.idr_fec_percent;
        let old_pframe_fec = self.pframe_fec_percent;

        if recent_pli >= 2 || long_pli >= 4 {
            self.current_bitrate_bps = (self.current_bitrate_bps.saturating_mul(80))
                .div_ceil(100)
                .max(min_bitrate_bps);
            self.idr_fec_percent = SEVERE_IDR_FEC_PERCENT;
            self.pframe_fec_percent = SEVERE_PFRAME_FEC_PERCENT;
        } else if recent_pli >= 1 || long_pli >= 2 {
            self.current_bitrate_bps = (self.current_bitrate_bps.saturating_mul(90))
                .div_ceil(100)
                .max(min_bitrate_bps);
            self.idr_fec_percent = MODERATE_IDR_FEC_PERCENT;
            self.pframe_fec_percent = MODERATE_PFRAME_FEC_PERCENT;
        } else {
            self.current_bitrate_bps = ((u64::from(self.current_bitrate_bps) * 110) / 100)
                .min(u64::from(self.base_bitrate_bps))
                as u32;
            self.idr_fec_percent = IDR_FEC_PERCENT;
            self.pframe_fec_percent = PFRAME_FEC_PERCENT;
        }

        self.last_adjust_at = now;

        if self.current_bitrate_bps != old_bitrate
            || self.idr_fec_percent != old_idr_fec
            || self.pframe_fec_percent != old_pframe_fec
        {
            println!(
                "[stream/adapt] pli_short={} pli_long={} bitrate={} idr_fec={} p_fec={}",
                recent_pli,
                long_pli,
                self.current_bitrate_bps,
                self.idr_fec_percent,
                self.pframe_fec_percent
            );
        }
    }
}

pub struct SharedState {
    pub client_addr: Mutex<Option<SocketAddr>>,
    pub force_idr: AtomicBool,
    pub force_refresh_handle: Mutex<Option<Arc<AtomicBool>>>,
    pub capture_start: Arc<AtomicBool>,
    pub capture_start_signal: Arc<(Mutex<bool>, Condvar)>,
    pub capture_stop_flag: Arc<AtomicBool>,
    pub input_backend: Arc<Mutex<dyn InputBackend>>,
    pub keyboard_worker: Mutex<Option<KeyboardWorker>>,
    pub audio_notify: Arc<Condvar>,
    pub cam_writer: Mutex<Option<CamWriter>>,
    pub camera_config: Mutex<CameraConfig>,
    pub camera_exclusive_caps: bool,
    pub mic_writer: Mutex<Option<MicWriter>>,
    pub start_time: Instant,
    pub on_client_connected: Mutex<Option<LifecycleCallback>>,
    pub on_client_disconnected: Mutex<Option<LifecycleCallback>>,
    pub has_active_client: AtomicBool,
    /// Cumulative wire bytes sent to / received from the client. Used only
    /// for the macOS menu bar's live throughput display (see
    /// `platform::macos::statusbar`); harmless no-op bookkeeping elsewhere.
    pub bytes_tx: AtomicU64,
    pub bytes_rx: AtomicU64,
    pub audio_output_enabled: AtomicBool,
    pub audio_module_id: Mutex<u32>,
    pub network_session_busy: AtomicBool,
    pub network_session_pending: AtomicBool,
    pub network_session_id: AtomicU64,
    pub session_key: Mutex<Option<[u8; 32]>>,
    adaptive_stream: Mutex<AdaptiveStreamState>,
    /// IP expected from the TCP handshake; used to accept the first UDP packet
    pub expected_client_ip: Mutex<Option<std::net::IpAddr>>,
    /// Local source IP/interface pinned for daemon→client UDP (from TCP handshake)
    pub udp_source: Mutex<Option<UdpSource>>,
    /// Ceilings advertised in `CAPS` and enforced when clamping `STNG`
    /// requests — 1:1 with the daemon operator's `--max-width`/
    /// `--max-height`/`--max-framerate`/`--max-bitrate` CLI flags. These
    /// double as the session defaults used when a client never sends
    /// `STNG` (or a tag within it), matching today's "one fixed config for
    /// every session" behavior for old clients.
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub bitrate_max_bps: u32,
    pub encoder_backend: EncoderBackend,
    /// Settings parsed out of a client's `STNG` control message, consumed
    /// once by the capture thread at the start of the next session (see
    /// `main.rs`'s capture loop). `None` means "no STNG received yet for
    /// the pending session" — the capture thread waits briefly on the
    /// condvar before falling back to the CLI/max defaults.
    pub pending_settings: Arc<(Mutex<Option<RequestedSettings>>, Condvar)>,
}

impl SharedState {
    pub fn new(
        camera_exclusive_caps: bool,
        base_bitrate_bps: u32,
        input_backend: Arc<Mutex<dyn InputBackend>>,
        max_width: u32,
        max_height: u32,
        max_fps: u32,
        bitrate_max_bps: u32,
        encoder_backend: EncoderBackend,
    ) -> Self {
        Self {
            client_addr: Mutex::new(None),
            force_idr: AtomicBool::new(false),
            force_refresh_handle: Mutex::new(None),
            capture_start: Arc::new(AtomicBool::new(false)),
            capture_start_signal: Arc::new((Mutex::new(false), Condvar::new())),
            capture_stop_flag: Arc::new(AtomicBool::new(false)),
            input_backend,
            keyboard_worker: Mutex::new(None),
            audio_notify: Arc::new(Condvar::new()),
            cam_writer: Mutex::new(None),
            camera_config: Mutex::new(CameraConfig {
                width: 0,
                height: 0,
                fps: 0,
            }),
            camera_exclusive_caps,
            mic_writer: Mutex::new(None),
            on_client_connected: Mutex::new(None),
            on_client_disconnected: Mutex::new(None),
            has_active_client: AtomicBool::new(false),
            bytes_tx: AtomicU64::new(0),
            bytes_rx: AtomicU64::new(0),
            audio_output_enabled: AtomicBool::new(false),
            audio_module_id: Mutex::new(0),
            network_session_busy: AtomicBool::new(false),
            network_session_pending: AtomicBool::new(false),
            network_session_id: AtomicU64::new(0),
            session_key: Mutex::new(None),
            adaptive_stream: Mutex::new(AdaptiveStreamState::new(base_bitrate_bps)),
            expected_client_ip: Mutex::new(None),
            udp_source: Mutex::new(None),
            max_width,
            max_height,
            max_fps,
            bitrate_max_bps,
            encoder_backend,
            pending_settings: Arc::new((Mutex::new(None), Condvar::new())),
            start_time: Instant::now(),
        }
    }

    pub fn reserve_network_session(&self) -> Option<u64> {
        self.network_session_busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        self.network_session_pending.store(true, Ordering::SeqCst);
        Some(
            self.network_session_id
                .fetch_add(1, Ordering::SeqCst)
                .wrapping_add(1),
        )
    }

    pub fn is_current_network_session(&self, session_id: u64) -> bool {
        self.network_session_busy.load(Ordering::SeqCst)
            && self.network_session_id.load(Ordering::SeqCst) == session_id
    }

    pub fn mark_network_session_active(&self, session_id: u64) {
        if self.is_current_network_session(session_id) {
            self.network_session_pending.store(false, Ordering::SeqCst);
        }
    }

    pub fn note_pli(&self) {
        self.adaptive_stream
            .lock()
            .unwrap()
            .note_pli(Instant::now());
    }

    pub fn current_stream_tuning(&self) -> StreamTuning {
        self.adaptive_stream
            .lock()
            .unwrap()
            .current_tuning(Instant::now())
    }

    pub fn reset_stream_tuning(&self) {
        self.adaptive_stream.lock().unwrap().reset();
    }

    /// Rebaseline the adaptive bitrate loop, e.g. to a client-negotiated
    /// `STNG` bitrate at session start, or back to the CLI default when a
    /// session ends so the next (possibly old-client) session isn't stuck
    /// on the previous client's negotiated bitrate.
    pub fn set_base_bitrate(&self, base_bitrate_bps: u32) {
        self.adaptive_stream
            .lock()
            .unwrap()
            .set_base(base_bitrate_bps);
    }
}

// ---------------------------------------------------------------------------
// Capability negotiation — `CAPS` (daemon -> client) and `STNG` (client ->
// daemon). See docs/ARCHITECTURE.md "Capability Negotiation" for the wire
// format; both clients implement it independently from scratch.
// ---------------------------------------------------------------------------

/// What this daemon can actually do right now, not just "compiled in."
/// Rebuilt fresh for every `CAPS` message (see `build_caps_message`) rather
/// than cached, since e.g. plugging in a driver after the daemon started
/// should show up in the next connection's advertised capabilities.
#[derive(Debug, Clone)]
pub struct DaemonCapabilities {
    pub camera: bool,
    pub microphone: bool,
    pub speaker: bool,
    pub codecs: Vec<VideoCodec>,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub bitrate_min_bps: u32,
    pub bitrate_max_bps: u32,
}

/// Client-proposed stream settings parsed out of `STNG`, already clamped
/// against `DaemonCapabilities`. `None` fields mean "use the daemon's own
/// default for that field" — either because the client omitted the tag, or
/// because what it asked for wasn't usable (unsupported codec, etc).
#[derive(Debug, Clone, Default)]
pub struct RequestedSettings {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub codec: Option<VideoCodec>,
    pub bitrate_bps: Option<u32>,
}

/// Cheap, real probes (not just "compiled in") for what this daemon can
/// actually do right now. Called both at startup (for an operator-visible
/// log line) and fresh on every `CAPS` message — see `build_caps_message`.
pub fn probe_capabilities(
    max_width: u32,
    max_height: u32,
    max_fps: u32,
    bitrate_max_bps: u32,
    encoder_backend: EncoderBackend,
) -> DaemonCapabilities {
    DaemonCapabilities {
        camera: crate::camera::probe_camera_available(),
        microphone: crate::audio::probe_mic_available(),
        speaker: crate::audio::probe_speaker_available(),
        codecs: crate::encode::probe_available_codecs(encoder_backend),
        max_width,
        max_height,
        max_fps,
        bitrate_min_bps: STNG_BITRATE_FLOOR_BPS,
        bitrate_max_bps,
    }
}

fn push_tlv_entry(msg: &mut Vec<u8>, tag: u8, value: &[u8]) {
    msg.push(tag);
    msg.extend_from_slice(&(value.len() as u16).to_be_bytes());
    msg.extend_from_slice(value);
}

/// Build a `CAPS` control message frame body advertising what this daemon
/// can do right now. Re-probes capabilities on every call (see
/// `probe_capabilities`) rather than reading a cached value, so it's cheap
/// by design — the caller (pairing.rs) calls this once per connection.
pub fn build_caps_message(shared: &SharedState) -> Vec<u8> {
    let caps = probe_capabilities(
        shared.max_width,
        shared.max_height,
        shared.max_fps,
        shared.bitrate_max_bps,
        shared.encoder_backend,
    );

    let mut msg = Vec::with_capacity(64);
    msg.extend_from_slice(CAPS_MAGIC);
    msg.push(CAPS_VERSION);
    msg.push(CAPS_ENTRY_COUNT);

    push_tlv_entry(&mut msg, CAPS_TAG_CAMERA, &[caps.camera as u8]);
    push_tlv_entry(&mut msg, CAPS_TAG_MICROPHONE, &[caps.microphone as u8]);
    push_tlv_entry(&mut msg, CAPS_TAG_SPEAKER, &[caps.speaker as u8]);

    let mut codecs_value = Vec::with_capacity(1 + caps.codecs.len());
    codecs_value.push(caps.codecs.len() as u8);
    codecs_value.extend(caps.codecs.iter().map(|c| c.transport_id()));
    push_tlv_entry(&mut msg, CAPS_TAG_CODECS, &codecs_value);

    let mut resolution_value = Vec::with_capacity(4);
    resolution_value
        .extend_from_slice(&(caps.max_width.min(u32::from(u16::MAX)) as u16).to_be_bytes());
    resolution_value
        .extend_from_slice(&(caps.max_height.min(u32::from(u16::MAX)) as u16).to_be_bytes());
    push_tlv_entry(&mut msg, CAPS_TAG_MAX_RESOLUTION, &resolution_value);

    push_tlv_entry(
        &mut msg,
        CAPS_TAG_MAX_FRAMERATE,
        &[caps.max_fps.min(255) as u8],
    );

    let mut bitrate_value = Vec::with_capacity(8);
    bitrate_value.extend_from_slice(&caps.bitrate_min_bps.to_be_bytes());
    bitrate_value.extend_from_slice(&caps.bitrate_max_bps.to_be_bytes());
    push_tlv_entry(&mut msg, CAPS_TAG_BITRATE_RANGE, &bitrate_value);

    msg
}

/// Clamp `value` into `[floor, ceiling]`, tolerating a misconfigured
/// operator ceiling below the fixed floor (e.g. `--max-width` set below 640)
/// rather than panicking — `u32::clamp` requires `floor <= ceiling`.
fn clamp_u32(value: u32, floor: u32, ceiling: u32) -> u32 {
    value.clamp(floor, ceiling.max(floor))
}

/// Parse an `STNG` message body (everything after the `"STNG"` magic:
/// version + entry_count + entries) into a `RequestedSettings`. Unknown tags
/// — or a known tag with an unexpected length — are skipped via their
/// declared length rather than treated as a parse failure, so a future v2
/// tag doesn't break this parser. Returns `None` only if the body is too
/// short to contain a header at all (malformed message, not just unknown
/// tags).
fn parse_stng_body(body: &[u8]) -> Option<RequestedSettings> {
    if body.len() < 2 {
        return None;
    }
    let _version = body[0];
    let entry_count = body[1] as usize;
    let mut offset = 2usize;
    let mut settings = RequestedSettings::default();

    for _ in 0..entry_count {
        if offset + 3 > body.len() {
            break; // truncated entry header; stop parsing what we have
        }
        let tag = body[offset];
        let len = u16::from_be_bytes([body[offset + 1], body[offset + 2]]) as usize;
        offset += 3;
        if offset + len > body.len() {
            break; // truncated value; stop parsing what we have
        }
        let value = &body[offset..offset + len];
        offset += len;

        match tag {
            STNG_TAG_RESOLUTION if len == 4 => {
                settings.width = Some(u16::from_be_bytes([value[0], value[1]]) as u32);
                settings.height = Some(u16::from_be_bytes([value[2], value[3]]) as u32);
            }
            STNG_TAG_FRAMERATE if len == 1 => {
                settings.fps = Some(value[0] as u32);
            }
            STNG_TAG_CODEC if len == 1 => {
                settings.codec = match value[0] {
                    0x00 => Some(VideoCodec::H264),
                    0x01 => Some(VideoCodec::H265),
                    _ => None, // invalid codec id -> daemon default
                };
            }
            STNG_TAG_BITRATE if len == 4 => {
                settings.bitrate_bps =
                    Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]));
            }
            // Unknown tag, or a known tag with an unexpected length: `offset`
            // has already advanced past the value, so parsing just continues
            // with the next entry.
            _ => {}
        }
    }

    Some(settings)
}

/// Clamp everything the client asked for against `DaemonCapabilities` —
/// never trust the client's numbers directly. Logs a warning whenever a
/// clamp changes what was requested, so operators can debug "why did my
/// requested 4K60 get downgraded."
fn clamp_requested_settings(shared: &SharedState, mut req: RequestedSettings) -> RequestedSettings {
    if let (Some(w), Some(h)) = (req.width, req.height) {
        let clamped_w = clamp_u32(w, STNG_RESOLUTION_FLOOR_W, shared.max_width);
        let clamped_h = clamp_u32(h, STNG_RESOLUTION_FLOOR_H, shared.max_height);
        if clamped_w != w || clamped_h != h {
            println!(
                "[control] STNG requested resolution {w}x{h} clamped to {clamped_w}x{clamped_h}"
            );
        }
        req.width = Some(clamped_w);
        req.height = Some(clamped_h);
    }

    if let Some(fps) = req.fps {
        let clamped = clamp_u32(fps, STNG_FPS_FLOOR, shared.max_fps);
        if clamped != fps {
            println!("[control] STNG requested framerate {fps} clamped to {clamped}");
        }
        req.fps = Some(clamped);
    }

    if let Some(codec) = req.codec {
        let available = crate::encode::probe_available_codecs(shared.encoder_backend);
        if !available.contains(&codec) {
            println!(
                "[control] STNG requested codec {codec:?} not available on this daemon, falling back to default"
            );
            req.codec = None;
        }
    }

    if let Some(bitrate) = req.bitrate_bps {
        let ceiling = shared.bitrate_max_bps;
        let clamped = clamp_u32(bitrate, STNG_BITRATE_FLOOR_BPS, ceiling);
        if clamped != bitrate {
            println!(
                "[control] STNG requested bitrate {bitrate} clamped to {clamped} (ceiling {ceiling})"
            );
        }
        req.bitrate_bps = Some(clamped);
    }

    req
}

/// Handle an inbound `STNG` control message: parse, clamp, and hand off to
/// the capture thread via the `pending_settings` condvar for the *next*
/// session it starts (see main.rs's capture loop).
fn apply_requested_settings(shared: &Arc<SharedState>, body: &[u8]) {
    let Some(parsed) = parse_stng_body(body) else {
        eprintln!(
            "[control] malformed STNG message ({} bytes), ignoring",
            body.len()
        );
        return;
    };

    let clamped = clamp_requested_settings(shared, parsed);
    println!("[control] received STNG, will apply to next session: {clamped:?}");

    let (lock, cvar) = &*shared.pending_settings;
    *lock.lock().unwrap() = Some(clamped);
    cvar.notify_all();
}

pub fn enable_camera(shared: &Arc<SharedState>, config: CameraConfig) {
    let mut current = shared.camera_config.lock().unwrap();
    let config_changed = *current != config;
    *current = config;
    drop(current);

    println!(
        "[camera] client enabled virtual webcam: {}x{} @ {}fps",
        config.width, config.height, config.fps
    );

    // Tear down existing writer if config changed or there is none
    let needs_create = {
        let mut writer = shared.cam_writer.lock().unwrap();
        if config_changed && writer.is_some() {
            *writer = None;
            true
        } else {
            writer.is_none()
        }
    };

    if needs_create {
        match crate::camera::CamWriter::new(config, shared.camera_exclusive_caps) {
            Ok(writer) => {
                *shared.cam_writer.lock().unwrap() = Some(writer);
                println!(
                    "[camera] virtual webcam ready ({}x{} @ {}fps)",
                    config.width, config.height, config.fps
                );
            }
            Err(e) => eprintln!("[camera] {e:#}"),
        }
    }
}

pub fn disable_camera(shared: &Arc<SharedState>) {
    let had_writer = shared.cam_writer.lock().unwrap().take().is_some();
    if had_writer {
        println!("[camera] client disabled virtual webcam — writer removed");
    }
}

pub const FLAG_IDR: u8 = 0x01;
pub const FLAG_AUDIO: u8 = 0x02;

/// 18-byte packet header (14 original + 4 byte timestamp_ms)
/// Byte 11 = codec_id: 0x00=H.264, 0x01=H.265
fn build_header(
    frame_id: u32,
    chunk_idx: u16,
    total_data: u16,
    total_parity: u16,
    flags: u8,
    codec_id: u8,
    payload_len: u16,
    timestamp_ms: u32,
) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(&frame_id.to_be_bytes());
    h[4..6].copy_from_slice(&chunk_idx.to_be_bytes());
    h[6..8].copy_from_slice(&total_data.to_be_bytes());
    h[8..10].copy_from_slice(&total_parity.to_be_bytes());
    h[10] = flags;
    h[11] = codec_id;
    h[12..14].copy_from_slice(&payload_len.to_be_bytes());
    h[14..18].copy_from_slice(&timestamp_ms.to_be_bytes());
    h
}

// ---------------------------------------------------------------------------
// Peripheral attach/detach — creates/destroys virtual devices on demand
// ---------------------------------------------------------------------------

const PERIPH_MOUSE: u8 = 0x01;
const PERIPH_KEYBOARD: u8 = 0x02;
const PERIPH_ATTACHED: u8 = 0x01;
const PERIPH_DETACHED: u8 = 0x00;

pub fn handle_periph_packet_data(_shared: &Arc<SharedState>, data: &[u8]) {
    if data.len() < 2 {
        return;
    }
    let device_type = data[0];
    let state = data[1];

    match (device_type, state) {
        (PERIPH_MOUSE, PERIPH_ATTACHED) => {
            crate::input::DIRECT_MOUSE_ENABLED.store(true, Ordering::SeqCst);
            println!("[periph] physical mouse attached");
        }
        (PERIPH_MOUSE, PERIPH_DETACHED) => {
            crate::input::DIRECT_MOUSE_ENABLED.store(false, Ordering::SeqCst);
            println!("[periph] physical mouse detached");
        }
        (PERIPH_KEYBOARD, PERIPH_ATTACHED) => {
            println!("[periph] physical keyboard attached");
        }
        (PERIPH_KEYBOARD, PERIPH_DETACHED) => {
            println!("[periph] physical keyboard detached");
        }
        _ => {}
    }
}

pub fn handle_control_message_data(shared: &Arc<SharedState>, ctrl: &[u8]) {
    if ctrl.starts_with(PLI_MAGIC) {
        shared.note_pli();
        shared.force_idr.store(true, Ordering::Relaxed);
        if let Some(ref refresh) = *shared.force_refresh_handle.lock().unwrap() {
            refresh.store(true, Ordering::Relaxed);
        }
        return;
    }

    if ctrl.starts_with(STNG_MAGIC) {
        apply_requested_settings(shared, &ctrl[STNG_MAGIC.len()..]);
        return;
    }

    if ctrl.starts_with(SPEAKER_MAGIC) && ctrl.len() == SPEAKER_MAGIC.len() + 1 {
        let enabled = ctrl[SPEAKER_MAGIC.len()] != 0;
        if enabled {
            println!("[audio] client enabled speaker passthrough");
            // Create the sink BEFORE setting the flag so the audio capture
            // thread never sees "enabled" without a ready PulseAudio sink.
            ensure_virtual_sink(shared);
            shared.audio_output_enabled.store(true, Ordering::SeqCst);
        } else {
            println!("[audio] client disabled speaker passthrough");
            // Clear the flag BEFORE removing the sink so the capture thread
            // stops trying to read from the monitor source.
            shared.audio_output_enabled.store(false, Ordering::SeqCst);
            disable_virtual_sink(shared);
        }
        shared.audio_notify.notify_all();
        return;
    }

    if ctrl.starts_with(MICCFG_MAGIC) && ctrl.len() == MICCFG_MAGIC.len() + 1 {
        let enabled = ctrl[MICCFG_MAGIC.len()] != 0;
        if enabled {
            enable_virtual_mic(shared);
        } else {
            disable_virtual_mic(shared);
        }
        return;
    }

    if ctrl.starts_with(CAMCFG_MAGIC) && ctrl.len() == CAMCFG_MAGIC.len() + 6 {
        let off = CAMCFG_MAGIC.len();
        let width = u16::from_be_bytes([ctrl[off], ctrl[off + 1]]) as u32;
        let height = u16::from_be_bytes([ctrl[off + 2], ctrl[off + 3]]) as u32;
        let fps = u16::from_be_bytes([ctrl[off + 4], ctrl[off + 5]]) as u32;
        if width > 0 && height > 0 && fps > 0 {
            enable_camera(shared, CameraConfig { width, height, fps });
        } else {
            disable_camera(shared);
        }
        return;
    }

    if ctrl.starts_with(TOUCH_MAGIC) && ctrl.len() > TOUCH_MAGIC.len() {
        let touch_data = &ctrl[TOUCH_MAGIC.len()..];
        let contacts = parse_touch_packet(touch_data);
        if let Ok(mut backend) = shared.input_backend.lock() {
            let _ = backend.touch(&contacts);
        }
        return;
    }

    if ctrl.starts_with(KEY_MAGIC) && ctrl.len() > KEY_MAGIC.len() {
        let key_data = &ctrl[KEY_MAGIC.len()..];
        if let Some(event) = parse_key_event(key_data) {
            let worker = shared.keyboard_worker.lock().unwrap();
            if let Some(ref w) = *worker {
                w.send(event);
            } else {
                eprintln!("[control] KEY packet dropped: no input backend available");
            }
        }
        return;
    }

    if ctrl.starts_with(MOUSE_MAGIC) && ctrl.len() > MOUSE_MAGIC.len() {
        let mouse_data = &ctrl[MOUSE_MAGIC.len()..];
        if let Some(ev) = parse_mouse_packet(mouse_data) {
            crate::input::DIRECT_MOUSE_ENABLED.store(true, Ordering::SeqCst);
            if let Ok(mut backend) = shared.input_backend.lock() {
                let _ = backend.mouse(ev);
            }
        }
        return;
    }

    if ctrl.starts_with(RAWKEY_MAGIC) && ctrl.len() > RAWKEY_MAGIC.len() {
        let rk_data = &ctrl[RAWKEY_MAGIC.len()..];
        if let Some(event) = parse_rawkey_event(rk_data) {
            let worker = shared.keyboard_worker.lock().unwrap();
            if let Some(ref w) = *worker {
                w.send(event);
            } else {
                eprintln!("[control] RAWKEY packet dropped: no input backend available");
            }
        }
        return;
    }

    if ctrl.starts_with(PERIPH_MAGIC) && ctrl.len() > PERIPH_MAGIC.len() {
        let periph_data = &ctrl[PERIPH_MAGIC.len()..];
        handle_periph_packet_data(shared, periph_data);
    }

    // Anything else — including a `GPAD` packet from a client that still
    // does controller passthrough — falls through here and is ignored.
}

pub fn ensure_virtual_sink(shared: &Arc<SharedState>) {
    let current_id = *shared.audio_module_id.lock().unwrap();
    if current_id > 0 {
        return; // already loaded
    }

    match crate::audio::create_virtual_sink() {
        Ok(id) => {
            *shared.audio_module_id.lock().unwrap() = id;
            println!("[lifecycle] audio: virtual sink ready (module {id})");
        }
        Err(e) => eprintln!("[lifecycle] audio: {e:#}"),
    }
}

pub fn disable_virtual_sink(shared: &Arc<SharedState>) {
    shared.audio_output_enabled.store(false, Ordering::SeqCst);
    shared.audio_notify.notify_all();
    std::thread::sleep(Duration::from_millis(200));

    let mut module_id = shared.audio_module_id.lock().unwrap();
    if *module_id > 0 {
        crate::audio::remove_virtual_sink(*module_id);
        *module_id = 0;
    }
    shared.audio_notify.notify_all();
}

pub fn enable_virtual_mic(shared: &Arc<SharedState>) {
    {
        let existing = shared.mic_writer.lock().unwrap();
        if existing.is_some() {
            return; // already active
        }
    }
    match crate::audio::create_virtual_mic() {
        Ok(writer) => {
            *shared.mic_writer.lock().unwrap() = Some(writer);
            println!("[mic] client enabled virtual microphone");
        }
        Err(e) => eprintln!("[mic] failed to create virtual microphone: {e:#}"),
    }
}

pub fn disable_virtual_mic(shared: &Arc<SharedState>) {
    let mut mic = shared.mic_writer.lock().unwrap();
    if let Some(ref mut writer) = *mic {
        crate::audio::remove_virtual_mic(writer);
        println!("[mic] client disabled virtual microphone");
    }
    *mic = None;
}

pub fn drop_network_client(shared: &Arc<SharedState>, session_id: u64) {
    if !shared.is_current_network_session(session_id) {
        return;
    }

    *shared.client_addr.lock().unwrap() = None;
    *shared.session_key.lock().unwrap() = None;
    *shared.expected_client_ip.lock().unwrap() = None;
    *shared.udp_source.lock().unwrap() = None;
    shared
        .network_session_pending
        .store(false, Ordering::SeqCst);
    shared.network_session_busy.store(false, Ordering::SeqCst);

    if shared.has_active_client.swap(false, Ordering::SeqCst) {
        shared.audio_notify.notify_all();
        shared.reset_stream_tuning();
        let shared_lc = Arc::clone(shared);
        std::thread::Builder::new()
            .name("lifecycle-disconnect".into())
            .spawn(move || {
                if let Some(ref cb) = *shared_lc.on_client_disconnected.lock().unwrap() {
                    cb();
                }
            })
            .ok();
    }
}

// ---------------------------------------------------------------------------
// Client manager — runs on its own thread, handles SCREX/PLI/keepalive
// ---------------------------------------------------------------------------

pub fn run_client_manager(
    socket: UdpSocket,
    shared: Arc<SharedState>,
    stop: Arc<AtomicBool>,
    session_rx: Arc<Mutex<Option<crate::pairing::SessionInfo>>>,
) -> Result<()> {
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();

    let mut last_keepalive = Instant::now();
    let mut last_heartbeat = Instant::now();
    let mut recv_buf = vec![0u8; 4096];
    let mut cam_reassembler = CamReassembler::new();
    let mut input_seq_expected: u32 = 0;
    let mut input_seq_initialized = false;
    let mut local_cipher: Option<crate::crypto::SessionCipher> = None;
    let mut session_serial: u64 = 0;
    let mut recv_count: u64 = 0;
    let mut short_drop_count: u64 = 0;
    let mut ip_mismatch_drop_count: u64 = 0;
    let mut no_cipher_drop_count: u64 = 0;
    let mut decrypt_fail_count: u64 = 0;
    let mut heartbeat_count: u64 = 0;
    let mut current_session_id: u64 = 0;
    let mut pending_started_at: Option<Instant> = None;
    let mut session_udp_source: Option<UdpSource> = None;

    println!("[client] waiting for paired handshake...");

    while !stop.load(Ordering::Relaxed) {
        // Check for new session from the pairing TCP handshake
        {
            let mut rx = session_rx.lock().unwrap();
            if let Some(session) = rx.take() {
                if !shared.is_current_network_session(session.session_id) {
                    crate::vlog!(
                        "[client] ignoring stale session info: session_id={} current_id={} busy={}",
                        session.session_id,
                        shared.network_session_id.load(Ordering::SeqCst),
                        shared.network_session_busy.load(Ordering::SeqCst)
                    );
                    continue;
                }
                session_serial = session_serial.wrapping_add(1);
                let previous_expected = *shared.expected_client_ip.lock().unwrap();
                let previous_client = *shared.client_addr.lock().unwrap();
                println!(
                    "[client] session key established from TCP handshake ({})",
                    session.client_addr.ip(),
                );
                crate::vlog!(
                    "[client] session #{session_serial} key established from TCP handshake: tcp_addr={} expected_udp_ip={} previous_expected_ip={previous_expected:?} previous_udp_client={previous_client:?}",
                    session.client_addr,
                    session.client_addr.ip(),
                );

                *shared.session_key.lock().unwrap() = Some(session.session_key);
                *shared.expected_client_ip.lock().unwrap() = Some(session.client_addr.ip());

                // Pin daemon→client UDP source to the local IP the client
                // dialed for the TCP handshake. On multi-homed hosts the
                // default route may otherwise pick another interface/IP and
                // the iPad will silently drop every packet we send.
                session_udp_source = udp_source_for_local_ip(session.local_ip);
                *shared.udp_source.lock().unwrap() = session_udp_source;
                match session_udp_source {
                    Some(src) => println!(
                        "[client] pinning UDP source address to {} (ifindex {})",
                        src.ip, src.ifindex
                    ),
                    None => crate::vlog!(
                        "[client] no UDP source pinning (handshake local ip: {:?})",
                        session.local_ip
                    ),
                }

                local_cipher = Some(crate::crypto::SessionCipher::new(&session.session_key));
                input_seq_expected = 0;
                input_seq_initialized = false;
                current_session_id = session.session_id;
                pending_started_at = Some(Instant::now());
                last_keepalive = Instant::now();
            }
        }

        match socket.recv_from(&mut recv_buf) {
            Ok((len, addr)) => {
                shared.bytes_rx.fetch_add(len as u64, Ordering::Relaxed);
                recv_count = recv_count.wrapping_add(1);
                if should_log_debug(recv_count) {
                    let registered_client = *shared.client_addr.lock().unwrap();
                    let expected_ip = *shared.expected_client_ip.lock().unwrap();
                    crate::vlog!(
                        "[client] udp recv #{recv_count}: from={addr} len={len} registered_client={registered_client:?} expected_ip={expected_ip:?} has_cipher={}",
                        local_cipher.is_some()
                    );
                }

                // All client→daemon UDP packets are encrypted:
                // [seq_num(4 BE)] [ciphertext] [tag(16)]
                if len < 4 + crate::crypto::TAG_LEN {
                    short_drop_count = short_drop_count.wrapping_add(1);
                    if should_log_debug(short_drop_count) {
                        crate::vlog!(
                            "[client] drop short udp packet #{short_drop_count}: from={addr} len={len} min_len={}",
                            4 + crate::crypto::TAG_LEN
                        );
                    }
                    continue; // too small to be valid
                }

                // Check IP and update port in one lock acquisition
                {
                    let mut client = shared.client_addr.lock().unwrap();
                    let registered_client = *client;
                    let expected_ip = *shared.expected_client_ip.lock().unwrap();
                    let registered_ok = client.map_or(false, |r| r.ip() == addr.ip());
                    let expected_ok = if !registered_ok {
                        expected_ip.map_or(false, |ip| ip == addr.ip())
                    } else {
                        false
                    };
                    if !registered_ok && !expected_ok {
                        ip_mismatch_drop_count = ip_mismatch_drop_count.wrapping_add(1);
                        if should_log_debug(ip_mismatch_drop_count) {
                            crate::vlog!(
                                "[client] drop udp packet #{ip_mismatch_drop_count}: from={addr} registered_client={registered_client:?} expected_ip={expected_ip:?} reason=ip_mismatch"
                            );
                        }
                        continue;
                    }
                    if let Some(ref mut r) = *client {
                        if *r != addr {
                            crate::vlog!("[client] udp client address updated: {r} -> {addr}");
                            *r = addr;
                        }
                    }
                }

                let seq_num =
                    u32::from_be_bytes([recv_buf[0], recv_buf[1], recv_buf[2], recv_buf[3]]);
                let nonce = crate::crypto::nonce_client(seq_num);

                let cipher = match local_cipher.as_ref() {
                    Some(c) => c,
                    None => {
                        no_cipher_drop_count = no_cipher_drop_count.wrapping_add(1);
                        if should_log_debug(no_cipher_drop_count) {
                            crate::vlog!(
                                "[client] drop udp packet #{no_cipher_drop_count}: from={addr} seq={seq_num} reason=no_session_cipher"
                            );
                        }
                        continue;
                    }
                };

                let aad: [u8; 4] = [recv_buf[0], recv_buf[1], recv_buf[2], recv_buf[3]];
                let plaintext = match cipher.decrypt(&nonce, &aad, &mut recv_buf[4..len]) {
                    Some(pt) => pt,
                    None => {
                        decrypt_fail_count = decrypt_fail_count.wrapping_add(1);
                        if should_log_debug(decrypt_fail_count) {
                            crate::vlog!(
                                "[client] drop udp packet #{decrypt_fail_count}: from={addr} seq={seq_num} len={len} reason=decrypt_failed"
                            );
                        }
                        continue;
                    }
                };

                last_keepalive = Instant::now();
                if should_log_debug(recv_count) {
                    crate::vlog!(
                        "[client] udp packet authenticated: from={addr} seq={seq_num} plaintext_len={}",
                        plaintext.len()
                    );
                }

                if input_seq_initialized && seq_is_stale(seq_num, input_seq_expected) {
                    crate::vlog!(
                        "[client] drop udp packet: from={addr} seq={seq_num} expected_seq={input_seq_expected} reason=stale_out_of_order"
                    );
                    continue;
                }
                if input_seq_initialized && seq_num != input_seq_expected {
                    crate::vlog!(
                        "[client] udp sequence gap: from={addr} seq={seq_num} expected_seq={input_seq_expected}"
                    );
                }
                input_seq_expected = seq_num.wrapping_add(1);
                input_seq_initialized = true;

                // First authenticated packet: register the full client address
                {
                    let mut client = shared.client_addr.lock().unwrap();
                    let is_new = client.is_none() || client.map_or(false, |prev| prev != addr);
                    *client = Some(addr);

                    if is_new {
                        println!("[client] authenticated UDP client: {addr}");
                        *shared.expected_client_ip.lock().unwrap() = None;
                        shared.mark_network_session_active(current_session_id);
                        pending_started_at = None;
                        shared.force_idr.store(true, Ordering::Relaxed);
                        shared.capture_start.store(true, Ordering::Release);
                        {
                            let (_, cvar) = &*shared.capture_start_signal;
                            cvar.notify_all();
                        }
                        if let Some(ref fr) = *shared.force_refresh_handle.lock().unwrap() {
                            fr.store(true, Ordering::Relaxed);
                        }
                        if !shared.has_active_client.swap(true, Ordering::SeqCst) {
                            shared.audio_notify.notify_all();
                            drop(client);
                            let shared_lc = Arc::clone(&shared);
                            std::thread::Builder::new()
                                .name("lifecycle-connect".into())
                                .spawn(move || {
                                    if let Some(ref cb) =
                                        *shared_lc.on_client_connected.lock().unwrap()
                                    {
                                        cb();
                                    }
                                })
                                .ok();
                        }

                        // Send an immediate heartbeat so the iPad's data timeout
                        // doesn't fire while peripherals are being created.
                        if let Some(ref c) = local_cipher {
                            let hb_frame_id = 0xFFFF_FFFFu32;
                            let hb_flags: u8 = 0x80;
                            let nonce = crate::crypto::nonce_server(hb_frame_id, 0, hb_flags);
                            let header = build_header(
                                hb_frame_id,
                                0,
                                0,
                                0,
                                hb_flags,
                                0,
                                HEARTBEAT_MAGIC.len() as u16,
                                0,
                            );
                            let hb_magic_len = HEARTBEAT_MAGIC.len();
                            let mut hb_buf = [0u8; HEADER_LEN + 64];
                            hb_buf[..HEADER_LEN].copy_from_slice(&header);
                            hb_buf[HEADER_LEN..HEADER_LEN + hb_magic_len]
                                .copy_from_slice(HEARTBEAT_MAGIC);
                            let enc_len = c.encrypt_slice(
                                &nonce,
                                &[],
                                &mut hb_buf[HEADER_LEN..],
                                hb_magic_len,
                            );
                            let _ = send_to_from(
                                &socket,
                                &hb_buf[..HEADER_LEN + enc_len],
                                addr,
                                session_udp_source,
                                None,
                            );
                        }
                        last_heartbeat = Instant::now();
                    }
                }

                let pt_len = plaintext.len();

                if pt_len >= REGISTER_MAGIC.len()
                    && &plaintext[..REGISTER_MAGIC.len()] == REGISTER_MAGIC
                {
                    crate::vlog!("[client] keepalive/register received: from={addr} seq={seq_num} plaintext_len={pt_len}");
                }

                if pt_len > CAM_MAGIC.len() && &plaintext[..CAM_MAGIC.len()] == CAM_MAGIC {
                    let cam_data = &plaintext[CAM_MAGIC.len()..];
                    if let Some(jpeg) = cam_reassembler.feed(cam_data) {
                        let mut cam = shared.cam_writer.lock().unwrap();
                        if let Some(ref mut cw) = *cam {
                            cw.write_frame(&jpeg);
                        }
                    }
                }

                if pt_len > MIC_MAGIC.len() + 4 && &plaintext[..MIC_MAGIC.len()] == MIC_MAGIC {
                    let opus_data = &plaintext[MIC_MAGIC.len() + 4..];
                    let mut mic = shared.mic_writer.lock().unwrap();
                    if let Some(ref mut mw) = *mic {
                        if let Err(e) = mw.write_opus_packet(opus_data) {
                            eprintln!("[mic] write error: {e}");
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                eprintln!("[client] recv error: {e}");
            }
        }

        if current_session_id != 0
            && shared.network_session_pending.load(Ordering::SeqCst)
            && pending_started_at
                .map_or(false, |started| started.elapsed() > PENDING_SESSION_TIMEOUT)
        {
            println!(
                "[client] pending session timed out after {:?}, dropping session",
                PENDING_SESSION_TIMEOUT
            );
            local_cipher = None;
            session_udp_source = None;
            drop_network_client(&shared, current_session_id);
            current_session_id = 0;
            pending_started_at = None;
        }

        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            if let Some(addr) = *shared.client_addr.lock().unwrap() {
                if let Some(ref c) = local_cipher {
                    heartbeat_count = heartbeat_count.wrapping_add(1);
                    let hb_frame_id = 0xFFFF_FFFFu32;
                    let hb_flags: u8 = 0x80;
                    let nonce = crate::crypto::nonce_server(hb_frame_id, 0, hb_flags);
                    let header = build_header(
                        hb_frame_id,
                        0,
                        0,
                        0,
                        hb_flags,
                        0,
                        HEARTBEAT_MAGIC.len() as u16,
                        0,
                    );
                    let hb_magic_len = HEARTBEAT_MAGIC.len();
                    let mut hb_buf = [0u8; HEADER_LEN + 64]; // enough for magic + tag
                    hb_buf[..HEADER_LEN].copy_from_slice(&header);
                    hb_buf[HEADER_LEN..HEADER_LEN + hb_magic_len].copy_from_slice(HEARTBEAT_MAGIC);
                    let enc_len =
                        c.encrypt_slice(&nonce, &[], &mut hb_buf[HEADER_LEN..], hb_magic_len);
                    match send_to_from(
                        &socket,
                        &hb_buf[..HEADER_LEN + enc_len],
                        addr,
                        session_udp_source,
                        None,
                    ) {
                        Ok(sent) => {
                            if should_log_debug(heartbeat_count) {
                                crate::vlog!(
                                    "[client] heartbeat sent #{heartbeat_count}: to={addr} bytes={sent} payload_len={hb_magic_len}"
                                );
                            }
                        }
                        Err(e) => eprintln!("[client] heartbeat send failed to {addr}: {e}"),
                    }
                }
            }
            last_heartbeat = Instant::now();
        }

        // Keepalive timeout check
        if shared.client_addr.lock().unwrap().is_some()
            && last_keepalive.elapsed() > KEEPALIVE_TIMEOUT
        {
            println!("[client] keepalive timeout, dropping client");
            local_cipher = None;
            session_udp_source = None;
            drop_network_client(&shared, current_session_id);
            current_session_id = 0;
            pending_started_at = None;
        }
    }

    println!("[client] manager stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// UDP sender — called synchronously from the capture thread
// ---------------------------------------------------------------------------

const MAX_RS_CACHE_ENTRIES: usize = 64;

pub struct UdpSender {
    socket: UdpSocket,
    packet_pool: Vec<[u8; MAX_PACKET_LEN]>,
    packet_lens: Vec<usize>,
    frame_id: u32,
    /// Time one frame is allotted on the wire (microseconds). Used to pace
    /// shard batching so video never floods the shared audio socket.
    frame_interval: u64,
    stats_start: Instant,
    stats_frames: u64,
    stats_bytes: u64,
    shard_pool: Vec<Vec<u8>>,
    rs_cache: HashMap<(usize, usize), ReedSolomon>,
    cipher: Option<crate::crypto::SessionCipher>,
    source: Option<UdpSource>,
    cmsg_buf: Option<PktinfoCmsg>,
    log_sent_aus: bool,
}

impl UdpSender {
    pub fn new(socket: UdpSocket, fps: u32) -> Self {
        Self {
            socket,
            packet_pool: Vec::new(),
            packet_lens: Vec::new(),
            frame_id: 0,
            // One frame's worth of wire time, in microseconds. Used to pace
            // shard batching so video never floods the shared audio socket.
            frame_interval: 1_000_000 / u64::from(fps.max(1)),
            stats_start: Instant::now(),
            stats_frames: 0,
            stats_bytes: 0,
            shard_pool: Vec::new(),
            rs_cache: HashMap::new(),
            cipher: None,
            source: None,
            cmsg_buf: None,
            log_sent_aus: std::env::var_os("SCREX_LOG_SENT_AUS").is_some(),
        }
    }

    pub fn set_cipher(&mut self, cipher: crate::crypto::SessionCipher) {
        self.cipher = Some(cipher);
    }

    pub fn set_source(&mut self, source: Option<UdpSource>) {
        self.source = source;
        self.cmsg_buf = source.map(build_pktinfo_cmsg);
    }

    pub fn send_frame(
        &mut self,
        au: &EncodedAccessUnit,
        client_addr: SocketAddr,
        timestamp_ms: u32,
        codec_id: u8,
        tuning: StreamTuning,
    ) -> Result<u64> {
        let payload: &[u8] = &*au.annex_b;
        let is_idr = au.is_idr;
        if self.log_sent_aus {
            let prefix_len = payload.len().min(24);
            println!(
                "[stream/send] frame_id={} codec={} idr={} bytes={} fnv=0x{:016x} prefix={:02x?}",
                self.frame_id,
                codec_id,
                is_idr,
                payload.len(),
                fnv1a64(payload),
                &payload[..prefix_len]
            );
        }

        let data_count = (payload.len() + CHUNK_PAYLOAD - 1) / CHUNK_PAYLOAD;

        let parity_percent = if is_idr {
            tuning.idr_fec_percent
        } else {
            tuning.pframe_fec_percent
        };

        let parity_count = if data_count <= 1 || parity_percent == 0 {
            0
        } else {
            data_count
                .saturating_mul(parity_percent)
                .div_ceil(100)
                .max(1)
                .min(MAX_FEC_SHARDS)
        };

        let use_fec = parity_count > 0 && (data_count + parity_count) <= 255;
        let actual_parity = if use_fec { parity_count } else { 0 };
        let total_shards = data_count + actual_parity;
        self.ensure_packet_capacity(total_shards);

        // Grow shard pool if needed (buffers persist across frames)
        while self.shard_pool.len() < total_shards {
            self.shard_pool.push(vec![0u8; CHUNK_PAYLOAD]);
        }

        // Fill data shards (zero-pad for RS alignment)
        for i in 0..data_count {
            let shard = &mut self.shard_pool[i];
            let start = i * CHUNK_PAYLOAD;
            let end = (start + CHUNK_PAYLOAD).min(payload.len());
            let actual = end - start;
            shard[..actual].copy_from_slice(&payload[start..end]);
            if actual < CHUNK_PAYLOAD {
                shard[actual..].fill(0);
            }
        }

        // Generate parity shards
        if actual_parity > 0 {
            for i in data_count..total_shards {
                self.shard_pool[i].fill(0);
            }
            if self.rs_cache.len() >= MAX_RS_CACHE_ENTRIES {
                self.rs_cache.clear();
            }
            let rs = match self.rs_cache.entry((data_count, actual_parity)) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let rs = ReedSolomon::new(data_count, actual_parity)
                        .map_err(|e| anyhow::anyhow!("RS init: {e:?}"))?;
                    entry.insert(rs)
                }
            };
            rs.encode(&mut self.shard_pool[..total_shards])
                .map_err(|e| anyhow::anyhow!("RS encode: {e:?}"))?;
        }

        let flags = if is_idr { FLAG_IDR } else { 0 };

        for idx in 0..total_shards {
            let actual_payload_len = if idx < data_count {
                let start = idx * CHUNK_PAYLOAD;
                let end = (start + CHUNK_PAYLOAD).min(payload.len());
                (end - start) as u16
            } else {
                CHUNK_PAYLOAD as u16
            };

            let header = build_header(
                self.frame_id,
                idx as u16,
                data_count as u16,
                actual_parity as u16,
                flags,
                codec_id,
                actual_payload_len,
                timestamp_ms,
            );

            self.packet_pool[idx][..HEADER_LEN].copy_from_slice(&header);
            self.packet_pool[idx][HEADER_LEN..HEADER_LEN + CHUNK_PAYLOAD]
                .copy_from_slice(&self.shard_pool[idx]);

            self.packet_lens[idx] = if let Some(ref cipher) = self.cipher {
                let nonce = crate::crypto::nonce_server(self.frame_id, idx as u16, flags);
                let enc_len = cipher.encrypt_slice(
                    &nonce,
                    &header,
                    &mut self.packet_pool[idx][HEADER_LEN..],
                    CHUNK_PAYLOAD,
                );
                HEADER_LEN + enc_len
            } else {
                HEADER_LEN + CHUNK_PAYLOAD
            };
        }

        let frame_bytes = self.send_frame_batch(client_addr, total_shards)?;

        self.frame_id = self.frame_id.wrapping_add(1);
        self.stats_frames += 1;
        self.stats_bytes += frame_bytes;

        if self.stats_start.elapsed() >= Duration::from_secs(1) {
            let elapsed = self.stats_start.elapsed().as_secs_f64();
            let fps = self.stats_frames as f64 / elapsed;
            let mbps = (self.stats_bytes as f64 * 8.0 / elapsed) / 1_000_000.0;
            crate::vlog!(
                "[stream] fps={fps:.1} throughput={mbps:.2}Mbps chunks={data_count}+{actual_parity}fec",
            );
            self.stats_frames = 0;
            self.stats_bytes = 0;
            self.stats_start = Instant::now();
        }

        Ok(frame_bytes)
    }

    fn ensure_packet_capacity(&mut self, total_shards: usize) {
        while self.packet_pool.len() < total_shards {
            self.packet_pool.push([0u8; MAX_PACKET_LEN]);
            self.packet_lens.push(0);
        }
    }

    fn send_frame_batch(&mut self, client_addr: SocketAddr, total_shards: usize) -> Result<u64> {
        let mut sent = 0;
        let mut frame_bytes = 0_u64;

        // Moonlight-style pacing for large bursts only. A keyframe can be
        // ~300 datagrams; blasting them into the shared socket in one go
        // delays the audio thread's 10ms Opus packets behind the entire burst
        // (the audible crackle at IDR time). Small frames (<1 sendmmsg batch)
        // send instantly, so normal latency is untouched. Only when the frame
        // is large do we cap the per-batch rate: spread it across roughly one
        // frame interval so video and audio interleave on the socket.
        let pacing_needed = total_shards > SENDMMSG_BATCH_SIZE;
        let shard_interval = if pacing_needed {
            (self.frame_interval / (total_shards as u64).max(1)).min(2000) as u64
        } else {
            0
        };
        while sent < total_shards {
            let end = (sent + SENDMMSG_BATCH_SIZE).min(total_shards);
            let batch: Vec<&[u8]> = (sent..end)
                .map(|idx| &self.packet_pool[idx][..self.packet_lens[idx]])
                .collect();
            match sendmmsg_batch(&self.socket, &batch, client_addr, self.source) {
                Ok(sent_now) if sent_now > 0 => {
                    frame_bytes += self.packet_lens[sent..sent + sent_now]
                        .iter()
                        .map(|len| *len as u64)
                        .sum::<u64>();
                    sent += sent_now;
                }
                Ok(_) => anyhow::bail!("sendmmsg returned 0 packets sent"),
                Err(error) => {
                    if sent == 0 {
                        crate::vlog!(
                            "[stream] sendmmsg unavailable, falling back to send_to: {error}"
                        );
                    }
                    for idx in sent..end {
                        send_to_from(
                            &self.socket,
                            &self.packet_pool[idx][..self.packet_lens[idx]],
                            client_addr,
                            self.source,
                            self.cmsg_buf.as_ref().map(|b| b.as_slice()),
                        )
                        .map_err(|e| anyhow::anyhow!("send error: {e}"))?;
                        frame_bytes += self.packet_lens[idx] as u64;
                    }
                    sent = end;
                }
            }
            if shard_interval > 0 {
                std::thread::sleep(Duration::from_micros(shard_interval));
            }
        }

        Ok(frame_bytes)
    }
}

// ---------------------------------------------------------------------------
// Audio sender — separate frame_id space, no FEC (audio chunks are small)
// ---------------------------------------------------------------------------

pub struct AudioSender {
    socket: UdpSocket,
    frame_id: u32,
    send_buf: Vec<u8>,
    cipher: Option<crate::crypto::SessionCipher>,
    source: Option<UdpSource>,
    cmsg_buf: Option<PktinfoCmsg>,
}

impl AudioSender {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            frame_id: 0,
            send_buf: vec![0u8; HEADER_LEN + CHUNK_PAYLOAD + crate::crypto::TAG_LEN],
            cipher: None,
            source: None,
            cmsg_buf: None,
        }
    }

    pub fn set_cipher(&mut self, cipher: crate::crypto::SessionCipher) {
        self.cipher = Some(cipher);
    }

    pub fn clear_cipher(&mut self) {
        self.cipher = None;
    }

    pub fn set_source(&mut self, source: Option<UdpSource>) {
        self.source = source;
        self.cmsg_buf = source.map(build_pktinfo_cmsg);
    }

    /// `payload` is one Opus packet (see `audio::run_audio_capture`). Chunking
    /// is kept size-agnostic even though an Opus frame always fits in one
    /// datagram, so the client's reassembly path stays valid either way.
    pub fn send_audio(
        &mut self,
        payload: &[u8],
        client_addr: SocketAddr,
        timestamp_ms: u32,
    ) -> Result<()> {
        let data_count = payload.len().div_ceil(CHUNK_PAYLOAD);

        for i in 0..data_count {
            let start = i * CHUNK_PAYLOAD;
            let end = (start + CHUNK_PAYLOAD).min(payload.len());
            let chunk = &payload[start..end];
            let payload_len = (end - start) as u16;

            let header = build_header(
                self.frame_id,
                i as u16,
                data_count as u16,
                0,
                FLAG_AUDIO,
                0,
                payload_len,
                timestamp_ms,
            );

            self.send_buf[..HEADER_LEN].copy_from_slice(&header);
            let chunk_len = chunk.len();
            self.send_buf[HEADER_LEN..HEADER_LEN + chunk_len].copy_from_slice(chunk);

            let pkt_len = if let Some(ref cipher) = self.cipher {
                let nonce = crate::crypto::nonce_server(self.frame_id, i as u16, FLAG_AUDIO);
                let enc_len = cipher.encrypt_slice(
                    &nonce,
                    &header,
                    &mut self.send_buf[HEADER_LEN..],
                    chunk_len,
                );
                HEADER_LEN + enc_len
            } else {
                HEADER_LEN + chunk_len
            };

            if let Err(e) = send_to_from(
                &self.socket,
                &self.send_buf[..pkt_len],
                client_addr,
                self.source,
                self.cmsg_buf.as_ref().map(|b| b.as_slice()),
            ) {
                eprintln!("[audio] send error: {e}");
                break;
            }
        }

        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }
}
