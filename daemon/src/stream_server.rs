use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::audio::MicWriter;
use crate::camera::{CamReassembler, CamWriter, CameraConfig};
use crate::capture::DisplayId;
use crate::encode::{EncodedAccessUnit, EncoderBackend, VideoCodec};
use crate::input::{
    parse_key_event, parse_mouse_packet, parse_rawkey_event, parse_touch_packet, GamepadState,
    InputBackend, KeyboardWorker,
};
use crate::platform::net::{
    build_ipv4_cmsg, build_pktinfo_cmsg, send_to_from, send_to_with_cmsg, sendmmsg_batch,
    udp_source_for_local_ip, PktinfoCmsg, UdpSource, TOS_EF,
};

const CHUNK_PAYLOAD: usize = 1400;
const HEADER_LEN: usize = 22;
use crate::protocol;

/// Validation/clamping bounds for client-proposed `SETTINGS`. The bitrate
/// floor is fixed, not operator-configurable; the ceilings come from the
/// daemon's `--max-*` CLI flags, stored on `SharedState`.
const SETTINGS_RESOLUTION_FLOOR_W: u32 = 640;
const SETTINGS_RESOLUTION_FLOOR_H: u32 = 360;
const SETTINGS_FPS_FLOOR: u32 = 15;
const SETTINGS_BITRATE_FLOOR_BPS: u32 = 500_000;

const PENDING_SESSION_TIMEOUT: Duration = Duration::from_secs(3);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT_MAGIC: &[u8] = b"SCREX_HB";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_FEC_SHARDS: usize = 127;
/// Reed-Solomon over GF(2^8) allows at most 255 data+parity shards per block.
const MAX_SHARDS_PER_FEC_BLOCK: usize = 255;
/// The iPad client rejects frames with more shards than this.
const MAX_SHARDS_PER_FRAME: usize = 4096;
/// Frames with more datagrams than this are paced across part of the frame
/// interval instead of being sent as one burst.
const PACING_THRESHOLD_SHARDS: usize = 48;
/// Datagrams sent per paced batch.
const PACING_BATCH_SIZE: usize = 8;
/// Fraction of the frame interval a large frame is spread across. Leaves the
/// rest of the interval for the next frame's capture/encode.
const PACING_SPREAD_NUM: u64 = 1;
const PACING_SPREAD_DEN: u64 = 2;
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

/// Extract only this session's messages. Never let a retiring consumer steal a new session's queue.
pub fn take_session_messages<T>(queue: &mut VecDeque<(u64, T)>, session: u64) -> Vec<T> {
    let mut messages = Vec::new();
    for _ in 0..queue.len() {
        let (id, message) = queue.pop_front().unwrap();
        if id == session {
            messages.push(message);
        } else if id > session {
            queue.push_back((id, message));
        }
    }
    messages
}

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
            crate::log_detail!(
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

pub struct DisplayStreamControl {
    pub force_idr: AtomicBool,
    pub force_refresh: Arc<AtomicBool>,
    pub generation: AtomicU32,
    adaptive_stream: Mutex<AdaptiveStreamState>,
}

impl DisplayStreamControl {
    pub fn new(base_bitrate_bps: u32) -> Self {
        Self {
            force_idr: AtomicBool::new(false),
            force_refresh: Arc::new(AtomicBool::new(false)),
            generation: AtomicU32::new(0),
            adaptive_stream: Mutex::new(AdaptiveStreamState::new(base_bitrate_bps)),
        }
    }

    pub fn tuning(&self) -> StreamTuning {
        self.adaptive_stream
            .lock()
            .unwrap()
            .current_tuning(Instant::now())
    }

    fn request_keyframe(&self, generation: u32) {
        if generation == 0 || self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        self.adaptive_stream
            .lock()
            .unwrap()
            .note_pli(Instant::now());
        self.force_idr.store(true, Ordering::Relaxed);
        self.force_refresh.store(true, Ordering::Relaxed);
    }
}

pub struct SharedState {
    pub client_addr: Mutex<Option<SocketAddr>>,
    display_streams: [Mutex<Arc<DisplayStreamControl>>; 2],
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
    pub audio_output_enabled: AtomicBool,
    pub audio_bitrate_bps: AtomicU32,
    /// `AUDIO_REPORT`s from the client, drained by the speaker thread.
    pub audio_reports: Mutex<VecDeque<crate::audio::AudioReport>>,
    /// Names the speaker and microphone cards: the iPad model the client sent in `SETTINGS`,
    /// like the monitor. Waited on with `audio_notify`.
    pub audio_device_name: Mutex<String>,
    pub network_session_busy: AtomicBool,
    pub network_session_pending: AtomicBool,
    pub network_session_id: AtomicU64,
    network_session_closing: AtomicBool,
    network_session_lock: Mutex<()>,
    pub session_key: Mutex<Option<[u8; 32]>>,
    /// IP expected from the TCP handshake; used to accept the first UDP packet
    pub expected_client_ip: Mutex<Option<std::net::IpAddr>>,
    /// Local source IP/interface pinned for daemon→client UDP (from TCP handshake)
    pub udp_source: Mutex<Option<UdpSource>>,
    /// Ceilings advertised in `CAPS` and enforced when clamping `SETTINGS`
    /// requests — 1:1 with the daemon operator's `--max-width`/
    /// `--max-height`/`--max-framerate`/`--max-bitrate` CLI flags. These
    /// double as the session defaults for any field `SETTINGS` leaves at "default".
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub bitrate_max_bps: u32,
    pub encoder_backend: EncoderBackend,
    /// Operator-disabled optional features.
    pub features: FeatureToggles,
    /// Latest topology request, tagged with the owning network session. Consumed by the
    /// display manager both at startup and while streaming. Superseded revisions coalesce.
    pub pending_settings: Arc<(Mutex<Option<(u64, RequestedSettings)>>, Condvar)>,
    pub presentation_acks: Mutex<VecDeque<(u64, crate::display_manager::PresentationAck)>>,
    pub display_status: Mutex<VecDeque<(u64, Vec<u8>)>>,
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
        features: FeatureToggles,
    ) -> Self {
        Self {
            client_addr: Mutex::new(None),
            display_streams: std::array::from_fn(|_| {
                Mutex::new(Arc::new(DisplayStreamControl::new(base_bitrate_bps)))
            }),
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
            audio_output_enabled: AtomicBool::new(false),
            audio_bitrate_bps: AtomicU32::new(128_000),
            audio_reports: Mutex::new(VecDeque::new()),
            audio_device_name: Mutex::new("iPad".into()),
            network_session_busy: AtomicBool::new(false),
            network_session_pending: AtomicBool::new(false),
            network_session_id: AtomicU64::new(0),
            network_session_closing: AtomicBool::new(false),
            network_session_lock: Mutex::new(()),
            session_key: Mutex::new(None),
            expected_client_ip: Mutex::new(None),
            udp_source: Mutex::new(None),
            max_width,
            max_height,
            max_fps,
            bitrate_max_bps,
            encoder_backend,
            features,
            pending_settings: Arc::new((Mutex::new(None), Condvar::new())),
            presentation_acks: Mutex::new(VecDeque::new()),
            display_status: Mutex::new(VecDeque::new()),
            start_time: Instant::now(),
        }
    }

    pub fn reserve_network_session(&self) -> Option<u64> {
        let _guard = self.network_session_lock.lock().unwrap();
        self.network_session_busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        self.network_session_pending.store(true, Ordering::SeqCst);
        self.network_session_closing.store(false, Ordering::SeqCst);
        Some(
            self.network_session_id
                .fetch_add(1, Ordering::SeqCst)
                .wrapping_add(1),
        )
    }

    pub fn is_current_network_session(&self, session_id: u64) -> bool {
        self.network_session_busy.load(Ordering::SeqCst)
            && !self.network_session_closing.load(Ordering::SeqCst)
            && self.network_session_id.load(Ordering::SeqCst) == session_id
    }

    pub fn mark_network_session_active(&self, session_id: u64) {
        if self.is_current_network_session(session_id) {
            self.network_session_pending.store(false, Ordering::SeqCst);
        }
    }

    pub fn display_stream(&self, display: DisplayId) -> Arc<DisplayStreamControl> {
        Arc::clone(&self.display_streams[display as usize].lock().unwrap())
    }

    pub fn install_display_stream(&self, display: DisplayId, control: Arc<DisplayStreamControl>) {
        *self.display_streams[display as usize].lock().unwrap() = control;
    }

    pub fn reset_stream_tuning(&self) {
        for stream in &self.display_streams {
            stream
                .lock()
                .unwrap()
                .adaptive_stream
                .lock()
                .unwrap()
                .reset();
        }
    }

    pub fn queue_display_status(
        &self,
        session: u64,
        status: crate::display_manager::DisplayStatus,
    ) {
        let mut queue = self.display_status.lock().unwrap();
        if queue.len() >= 128 {
            queue.pop_front();
        }
        queue.push_back((session, status.encode()));
    }
}

// ---------------------------------------------------------------------------
// Capability negotiation — `CAPS` (daemon -> client) and `SETTINGS` (client
// -> daemon). See docs/ARCHITECTURE.md "Capability Negotiation" for the
// fixed wire layouts.
// ---------------------------------------------------------------------------

/// Optional host features the operator can switch off from the command line
/// (`--no-speaker`, `--no-mic`, `--no-camera`, `--no-gamepad`). A disabled
/// feature is advertised as unavailable in `CAPS` and its enable requests are
/// ignored.
#[derive(Debug, Clone, Copy)]
pub struct FeatureToggles {
    pub speaker: bool,
    pub microphone: bool,
    pub camera: bool,
    pub gamepad: bool,
}

/// What this daemon can actually do right now, not just "compiled in."
/// Rebuilt fresh for every `CAPS` message (see `build_caps_message`) rather
/// than cached, since e.g. plugging in a driver after the daemon started
/// should show up in the next connection's advertised capabilities.
#[derive(Debug, Clone)]
pub struct DaemonCapabilities {
    pub camera: bool,
    pub microphone: bool,
    pub speaker: bool,
    /// `None` = unsupported, `Some(n)` = max simultaneous controllers.
    pub gamepad: Option<u8>,
    pub codecs: Vec<VideoCodec>,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub bitrate_min_bps: u32,
    pub bitrate_max_bps: u32,
}

/// Client-proposed stream settings parsed out of `SETTINGS`, already clamped
/// against `DaemonCapabilities`. `None` fields mean "use the daemon's own
/// default for that field" — either because the client sent the "default"
/// value (0, or `CODEC_DEFAULT`), or because what it asked for wasn't usable
/// (unsupported codec).
#[derive(Debug, Clone, Default)]
pub struct RequestedSettings {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub codec: Option<VideoCodec>,
    pub bitrate_bps: Option<u32>,
    pub enabled_mask: u8,
    pub external_width: Option<u32>,
    pub external_height: Option<u32>,
    pub external_fps: Option<u32>,
    pub internal_display_name: Option<[u8; 13]>,
    /// The iPad panel's width and height in mm; `None` when the client doesn't know them.
    pub internal_size_mm: Option<(u16, u16)>,
    pub audio_bitrate_bps: u32,
    pub revision: u32,
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
    features: FeatureToggles,
) -> DaemonCapabilities {
    DaemonCapabilities {
        camera: features.camera && crate::camera::probe_camera_available(),
        microphone: features.microphone && crate::audio::probe_mic_available(),
        speaker: features.speaker && crate::audio::probe_speaker_available(),
        gamepad: if features.gamepad {
            crate::input::probe_gamepad_max_controllers()
        } else {
            None
        },
        codecs: crate::encode::probe_available_codecs(encoder_backend),
        max_width,
        max_height,
        max_fps,
        bitrate_min_bps: SETTINGS_BITRATE_FLOOR_BPS,
        bitrate_max_bps,
    }
}

/// Build the `CAPS` control message advertising what this daemon can do
/// right now, plus its host name. Re-probes capabilities on every call (see
/// `probe_capabilities`) rather than reading a cached value — pairing.rs
/// calls this once per connection.
///
/// Layout after the type byte: `camera(u8) mic(u8) speaker(u8)
/// gamepad_max(u8, 0 = unsupported) codecs(u8 bitmask, bit n = codec id n)
/// max_width(u16) max_height(u16) max_fps(u8) bitrate_min(u32)
/// bitrate_max(u32) hostname(UTF-8, rest of message)`. Integers are big-endian.
pub fn build_caps_message(shared: &SharedState) -> Vec<u8> {
    let caps = probe_capabilities(
        shared.max_width,
        shared.max_height,
        shared.max_fps,
        shared.bitrate_max_bps,
        shared.encoder_backend,
        shared.features,
    );
    let hostname = crate::platform::shim::hostname().unwrap_or_default();

    let mut msg = Vec::with_capacity(22 + hostname.len());
    msg.push(protocol::daemon::CAPS);
    msg.push(caps.camera as u8);
    msg.push(caps.microphone as u8);
    msg.push(caps.speaker as u8);
    msg.push(caps.gamepad.unwrap_or(0));
    msg.push(
        caps.codecs
            .iter()
            .fold(0u8, |mask, codec| mask | (1 << codec.transport_id())),
    );
    msg.extend_from_slice(&(caps.max_width.min(u32::from(u16::MAX)) as u16).to_be_bytes());
    msg.extend_from_slice(&(caps.max_height.min(u32::from(u16::MAX)) as u16).to_be_bytes());
    msg.push(caps.max_fps.min(255) as u8);
    msg.extend_from_slice(&caps.bitrate_min_bps.to_be_bytes());
    msg.extend_from_slice(&caps.bitrate_max_bps.to_be_bytes());
    msg.extend_from_slice(hostname.as_bytes());
    msg
}

/// Clamp `value` into `[floor, ceiling]`, tolerating a misconfigured
/// operator ceiling below the fixed floor (e.g. `--max-width` set below 640)
/// rather than panicking — `u32::clamp` requires `floor <= ceiling`.
fn clamp_u32(value: u32, floor: u32, ceiling: u32) -> u32 {
    value.clamp(floor, ceiling.max(floor))
}

/// Length of a `SETTINGS` body (after the type byte).
const SETTINGS_BODY_LEN: usize = 41;
/// Panel sizes outside this range (mm) are treated as unknown.
const PANEL_MM_RANGE: std::ops::RangeInclusive<u16> = 50..=1000;

/// Parse a `SETTINGS` body: internal width/height/FPS, shared codec/bitrate, enabled mask,
/// external width/height, revision, external FPS, internal EDID name, internal panel
/// width/height in mm, speaker audio bitrate in bps, big-endian. A zero (or `CODEC_DEFAULT` for the codec) means "use the
/// daemon's default" for that field; a zero panel size means unknown.
fn parse_settings_body(body: &[u8]) -> Option<RequestedSettings> {
    if body.len() != SETTINGS_BODY_LEN || !(1..=3).contains(&body[10]) {
        return None;
    }
    let audio_bitrate_bps = match u32::from_be_bytes(body[37..41].try_into().unwrap()) {
        0 => 128_000,
        value @ (96_000 | 128_000 | 192_000 | 256_000) => value,
        _ => return None,
    };
    let revision = u32::from_be_bytes(body[15..19].try_into().unwrap());
    if revision == 0 {
        return None;
    }
    // Safe both for EDID ASCII descriptors and GNOME's single-quoted output identity.
    let name: [u8; 13] = body[20..33].try_into().ok()?;
    if name
        .iter()
        .any(|b| !(0x20..=0x7e).contains(b) || matches!(*b, b'\'' | b'\\'))
        || name.iter().all(|b| *b == b' ')
    {
        return None;
    }
    let ew = u32::from(u16::from_be_bytes(body[11..13].try_into().unwrap()));
    let eh = u32::from(u16::from_be_bytes(body[13..15].try_into().unwrap()));
    let width = u32::from(u16::from_be_bytes([body[0], body[1]]));
    let height = u32::from(u16::from_be_bytes([body[2], body[3]]));
    let fps = u32::from(body[4]);
    let bitrate = u32::from_be_bytes([body[6], body[7], body[8], body[9]]);
    let nonzero = |v: u32| (v != 0).then_some(v);
    let width_mm = u16::from_be_bytes(body[33..35].try_into().unwrap());
    let height_mm = u16::from_be_bytes(body[35..37].try_into().unwrap());
    let internal_size_mm = (PANEL_MM_RANGE.contains(&width_mm)
        && PANEL_MM_RANGE.contains(&height_mm))
    .then_some((width_mm, height_mm));
    let (width, height) = if width != 0 && height != 0 {
        (Some(width), Some(height))
    } else {
        (None, None)
    };
    Some(RequestedSettings {
        width,
        height,
        fps: nonzero(fps),
        codec: match body[5] {
            protocol::CODEC_H264 => Some(VideoCodec::H264),
            protocol::CODEC_H265 => Some(VideoCodec::H265),
            protocol::CODEC_DEFAULT => None,
            other => {
                eprintln!("[control] unknown codec id 0x{other:02x} in SETTINGS, using default");
                None
            }
        },
        bitrate_bps: nonzero(bitrate),
        enabled_mask: body[10],
        external_width: (ew > 0 && eh > 0).then_some(ew),
        external_height: (ew > 0 && eh > 0).then_some(eh),
        external_fps: nonzero(u32::from(body[19])),
        internal_display_name: Some(name),
        internal_size_mm,
        audio_bitrate_bps,
        revision,
    })
}

/// Clamp everything the client asked for against `DaemonCapabilities` —
/// never trust the client's numbers directly. Logs a warning whenever a
/// clamp changes what was requested, so operators can debug "why did my
/// requested 4K60 get downgraded."
fn clamp_requested_settings(shared: &SharedState, mut req: RequestedSettings) -> RequestedSettings {
    if let (Some(w), Some(h)) = (req.width, req.height) {
        let clamped_w = clamp_u32(w, SETTINGS_RESOLUTION_FLOOR_W, shared.max_width);
        let clamped_h = clamp_u32(h, SETTINGS_RESOLUTION_FLOOR_H, shared.max_height);
        if clamped_w != w || clamped_h != h {
            println!("[control] requested resolution {w}x{h} clamped to {clamped_w}x{clamped_h}");
        }
        req.width = Some(clamped_w);
        req.height = Some(clamped_h);
    }

    if let Some(fps) = req.fps {
        let clamped = clamp_u32(fps, SETTINGS_FPS_FLOOR, shared.max_fps);
        if clamped != fps {
            println!("[control] requested framerate {fps} clamped to {clamped}");
        }
        req.fps = Some(clamped);
    }
    if let (Some(w), Some(h)) = (req.external_width, req.external_height) {
        req.external_width = Some(clamp_u32(w, SETTINGS_RESOLUTION_FLOOR_W, shared.max_width));
        req.external_height = Some(clamp_u32(h, SETTINGS_RESOLUTION_FLOOR_H, shared.max_height));
    }
    if let Some(fps) = req.external_fps {
        req.external_fps = Some(clamp_u32(fps, SETTINGS_FPS_FLOOR, shared.max_fps));
    }

    if let Some(codec) = req.codec {
        let available = crate::encode::probe_available_codecs(shared.encoder_backend);
        if !available.contains(&codec) {
            println!(
                "[control] requested codec {codec:?} not available on this daemon, falling back to default"
            );
            req.codec = None;
        }
    }

    if let Some(bitrate) = req.bitrate_bps {
        let ceiling = shared.bitrate_max_bps;
        let clamped = clamp_u32(bitrate, SETTINGS_BITRATE_FLOOR_BPS, ceiling);
        if clamped != bitrate {
            println!(
                "[control] requested bitrate {bitrate} clamped to {clamped} (ceiling {ceiling})"
            );
        }
        req.bitrate_bps = Some(clamped);
    }

    req
}

/// Handle an inbound `SETTINGS` control message: parse, clamp, and hand off to
/// the display manager via the session-tagged `pending_settings` mailbox.
fn apply_requested_settings(shared: &Arc<SharedState>, session: u64, body: &[u8]) {
    let Some(parsed) = parse_settings_body(body) else {
        eprintln!(
            "[control] malformed SETTINGS message ({} bytes), ignoring",
            body.len()
        );
        return;
    };

    let clamped = clamp_requested_settings(shared, parsed);
    shared.audio_bitrate_bps.store(clamped.audio_bitrate_bps, Ordering::Relaxed);
    crate::log_detail!("[control] received display SETTINGS: {clamped:?}");

    if let Some(name) = clamped.internal_display_name {
        // Cards plugged in from now on are named like the monitor.
        *shared.audio_device_name.lock().unwrap() =
            String::from_utf8_lossy(&name).trim_end().to_string();
    }

    let (lock, cvar) = &*shared.pending_settings;
    let mut pending = lock.lock().unwrap();
    if pending
        .as_ref()
        .is_none_or(|(id, old)| *id != session || clamped.revision > old.revision)
    {
        *pending = Some((session, clamped));
    }
    cvar.notify_all();
}

pub fn enable_camera(shared: &Arc<SharedState>, config: CameraConfig) {
    let mut current = shared.camera_config.lock().unwrap();
    let config_changed = *current != config;
    *current = config;
    drop(current);

    crate::log_detail!(
        "[camera] client enabled virtual webcam: {}x{} @ {}fps",
        config.width,
        config.height,
        config.fps
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
                crate::log_detail!(
                    "[camera] virtual webcam ready ({}x{} @ {}fps)",
                    config.width,
                    config.height,
                    config.fps
                );
            }
            Err(e) => eprintln!("[camera] {e:#}"),
        }
    }
}

pub fn disable_camera(shared: &Arc<SharedState>) {
    let had_writer = shared.cam_writer.lock().unwrap().take().is_some();
    if had_writer {
        crate::log_detail!("[camera] client disabled virtual webcam — writer removed");
    }
}

pub const FLAG_IDR: u8 = 0x01;
pub const FLAG_AUDIO: u8 = 0x02;
pub const FLAG_EXTERNAL: u8 = 0x10;

/// 22-byte packet header, including the authenticated stream generation.
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
    generation: u32,
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
    h[18..22].copy_from_slice(&generation.to_be_bytes());
    h
}

// ---------------------------------------------------------------------------
// Peripheral attach/detach — creates/destroys virtual devices on demand
// ---------------------------------------------------------------------------

const PERIPH_MOUSE: u8 = 0x01;
const PERIPH_KEYBOARD: u8 = 0x02;
const PERIPH_ATTACHED: u8 = 0x01;
const PERIPH_DETACHED: u8 = 0x00;

pub fn handle_periph_packet_data(shared: &Arc<SharedState>, data: &[u8]) {
    if data.len() < 2 {
        return;
    }
    let device_type = data[0];
    let state = data[1];

    match (device_type, state) {
        (PERIPH_MOUSE, PERIPH_ATTACHED) => {
            crate::log_detail!("[periph] physical mouse attached");
        }
        (PERIPH_MOUSE, PERIPH_DETACHED) => {
            // A mouse unplugged mid-drag never sends its button-up; release
            // every button so nothing stays held on the host.
            if let Ok(mut backend) = shared.input_backend.lock() {
                backend.release_mouse_buttons();
            }
            crate::log_detail!("[periph] physical mouse detached, buttons released");
        }
        (PERIPH_KEYBOARD, PERIPH_ATTACHED) => {
            crate::log_detail!("[periph] physical keyboard attached");
        }
        (PERIPH_KEYBOARD, PERIPH_DETACHED) => {
            // Same for keys: go through the keyboard worker so the release
            // lands after any key events still queued ahead of it.
            if let Some(ref worker) = *shared.keyboard_worker.lock().unwrap() {
                worker.send(crate::input::KeyboardEvent::ReleaseAll);
            }
            crate::log_detail!("[periph] physical keyboard detached, keys released");
        }
        _ => {}
    }
}

/// Dispatch one decrypted client -> daemon control message (`type(u8) +
/// body`). `PING` and `DISCONNECT` are handled by the control loop in
/// pairing.rs before this is called.
pub fn handle_control_message_data(shared: &Arc<SharedState>, session: u64, msg: &[u8]) {
    if !shared.is_current_network_session(session) {
        return;
    }
    let Some((&msg_type, body)) = msg.split_first() else {
        return;
    };
    use protocol::client;

    match msg_type {
        client::PLI if body.len() == 5 => {
            if let Some(display) = DisplayId::from_wire(body[0]) {
                let generation = u32::from_be_bytes(body[1..5].try_into().unwrap());
                shared.display_stream(display).request_keyframe(generation);
            }
        }
        client::SETTINGS => apply_requested_settings(shared, session, body),
        client::DISPLAY_PRESENTED if body.len() == 9 => {
            if let Some(display) = DisplayId::from_wire(body[4]) {
                let ack = crate::display_manager::PresentationAck {
                    revision: u32::from_be_bytes(body[..4].try_into().unwrap()),
                    display,
                    generation: u32::from_be_bytes(body[5..9].try_into().unwrap()),
                };
                let mut queue = shared.presentation_acks.lock().unwrap();
                if queue.len() < 128 {
                    queue.push_back((session, ack));
                }
            }
        }
        client::SPEAKER if body.len() == 1 => {
            let enabled = body[0] != 0;
            if enabled && !shared.features.speaker {
                println!("[audio] ignoring speaker enable: disabled with --no-speaker");
            } else {
                crate::log_detail!(
                    "[audio] client turned speakers {}",
                    if enabled { "on" } else { "off" }
                );
                // The speaker thread plugs the card in or out.
                shared.audio_output_enabled.store(enabled, Ordering::SeqCst);
                shared.audio_notify.notify_all();
            }
        }
        client::MIC if body.len() == 1 => {
            let enabled = body[0] != 0;
            if enabled && !shared.features.microphone {
                println!("[mic] ignoring microphone enable: disabled with --no-mic");
            } else if enabled {
                enable_virtual_mic(shared);
            } else {
                disable_virtual_mic(shared);
            }
        }
        client::CAMERA if body.len() == 6 => {
            let width = u32::from(u16::from_be_bytes([body[0], body[1]]));
            let height = u32::from(u16::from_be_bytes([body[2], body[3]]));
            let fps = u32::from(u16::from_be_bytes([body[4], body[5]]));
            let enabled = width > 0 && height > 0 && fps > 0;
            if enabled && !shared.features.camera {
                println!("[camera] ignoring camera enable: disabled with --no-camera");
            } else if enabled {
                enable_camera(shared, CameraConfig { width, height, fps });
            } else {
                disable_camera(shared);
            }
        }
        client::TOUCH => {
            let contacts = parse_touch_packet(body);
            if let Ok(mut backend) = shared.input_backend.lock() {
                let _ = backend.touch(&contacts);
            }
        }
        client::KEY | client::RAWKEY => {
            let event = if msg_type == client::KEY {
                parse_key_event(body)
            } else {
                parse_rawkey_event(body)
            };
            if let Some(event) = event {
                if let Some(ref worker) = *shared.keyboard_worker.lock().unwrap() {
                    worker.send(event);
                } else {
                    eprintln!("[control] key event dropped: no input backend available");
                }
            }
        }
        client::MOUSE => {
            if let Some(ev) = parse_mouse_packet(body) {
                if let Ok(mut backend) = shared.input_backend.lock() {
                    let _ = backend.mouse(ev);
                }
            }
        }
        client::PERIPH => handle_periph_packet_data(shared, body),
        client::GAMEPAD => {
            if shared.features.gamepad {
                handle_gamepad_packet_data(shared, body);
            }
        }
        _ => {
            crate::log_trace!(
                "[control] ignoring unknown or malformed message type 0x{msg_type:02x} ({} bytes)",
                body.len()
            );
        }
    }
}

/// Turns speakers off; the speaker thread unplugs the card.
pub fn disable_speakers(shared: &SharedState) {
    shared.audio_output_enabled.store(false, Ordering::SeqCst);
    shared.audio_notify.notify_all();
}

pub fn enable_virtual_mic(shared: &Arc<SharedState>) {
    let mut mic = shared.mic_writer.lock().unwrap();
    if mic.is_some() {
        return; // already active
    }
    let name = shared.audio_device_name.lock().unwrap().clone();
    match crate::audio::create_virtual_mic(&name) {
        Ok(writer) => *mic = Some(writer),
        Err(e) => eprintln!("[mic] failed to create microphone: {e:#}"),
    }
}

/// Unplugs the microphone card, if any.
pub fn disable_virtual_mic(shared: &SharedState) {
    if shared.mic_writer.lock().unwrap().take().is_some() {
        crate::log_detail!("[mic] microphone card unplugged");
    }
}

const GPAD_DETACHED: u8 = 0x00;
const GPAD_ATTACHED: u8 = 0x01;
const GPAD_STATE: u8 = 0x02;

pub fn handle_gamepad_packet_data(shared: &Arc<SharedState>, data: &[u8]) {
    if data.len() < 2 {
        return;
    }

    let controller_id = data[0];
    let msg_type = data[1];

    match msg_type {
        GPAD_ATTACHED => {
            if let Ok(mut backend) = shared.input_backend.lock() {
                if let Err(e) = backend.gamepad_attach(controller_id) {
                    eprintln!(
                        "[gamepad] failed to create virtual gamepad {}: {e}",
                        controller_id + 1
                    );
                } else {
                    crate::log_detail!(
                        "[gamepad] controller {} attached — virtual gamepad created",
                        controller_id + 1
                    );
                }
            }
        }
        GPAD_DETACHED => {
            if let Ok(mut backend) = shared.input_backend.lock() {
                backend.gamepad_detach(controller_id);
                crate::log_detail!(
                    "[gamepad] controller {} detached — virtual gamepad destroyed",
                    controller_id + 1
                );
            }
        }
        GPAD_STATE if data.len() >= 18 => {
            let buttons_mask = u16::from_be_bytes([data[2], data[3]]);
            let lx = i16::from_be_bytes([data[4], data[5]]);
            let ly = i16::from_be_bytes([data[6], data[7]]);
            let rx = i16::from_be_bytes([data[8], data[9]]);
            let ry = i16::from_be_bytes([data[10], data[11]]);
            let lt = u16::from_be_bytes([data[12], data[13]]);
            let rt = u16::from_be_bytes([data[14], data[15]]);
            let hat_x = data[16] as i8;
            let hat_y = data[17] as i8;

            crate::log_trace!(
                "[gamepad] recv state controller={} buttons=0x{:x} lx={} ly={} rx={} ry={} lt={} rt={} hat=({}, {})",
                controller_id + 1,
                buttons_mask,
                lx,
                ly,
                rx,
                ry,
                lt,
                rt,
                hat_x,
                hat_y
            );

            if let Ok(mut backend) = shared.input_backend.lock() {
                let state = GamepadState {
                    buttons: buttons_mask,
                    lx,
                    ly,
                    rx,
                    ry,
                    lt,
                    rt,
                    hat_x,
                    hat_y,
                };
                let _ = backend.gamepad_state(controller_id, &state);
            }
        }
        _ => {}
    }
}

pub fn drop_network_client(shared: &Arc<SharedState>, session_id: u64) {
    {
        let _guard = shared.network_session_lock.lock().unwrap();
        if !shared.is_current_network_session(session_id) {
            return;
        }
        shared.network_session_closing.store(true, Ordering::SeqCst);
    }

    *shared.client_addr.lock().unwrap() = None;
    *shared.session_key.lock().unwrap() = None;
    *shared.expected_client_ip.lock().unwrap() = None;
    *shared.udp_source.lock().unwrap() = None;
    shared
        .network_session_pending
        .store(false, Ordering::SeqCst);

    if shared.has_active_client.swap(false, Ordering::SeqCst) {
        shared.audio_notify.notify_all();
        shared.reset_stream_tuning();
        // Keep the session reserved until cleanup completes. A delayed old callback must
        // never remove peripherals or settings belonging to the next connection.
        if let Some(ref cb) = *shared.on_client_disconnected.lock().unwrap() {
            cb();
        }
    }
    {
        let mut pending = shared.pending_settings.0.lock().unwrap();
        if pending.as_ref().is_some_and(|(id, _)| *id == session_id) {
            *pending = None;
        }
    }
    shared
        .presentation_acks
        .lock()
        .unwrap()
        .retain(|(id, _)| *id != session_id);
    shared
        .display_status
        .lock()
        .unwrap()
        .retain(|(id, _)| *id != session_id);
    shared.pending_settings.1.notify_all();
    shared.network_session_busy.store(false, Ordering::SeqCst);
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
    let mut heartbeat_sequence = crate::crypto::ServerPacketSequence::new();
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

    crate::log_detail!("[client] waiting for paired handshake...");

    while !stop.load(Ordering::Relaxed) {
        // Check for new session from the pairing TCP handshake
        {
            let mut rx = session_rx.lock().unwrap();
            if let Some(session) = rx.take() {
                if !shared.is_current_network_session(session.session_id) {
                    crate::log_trace!(
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
                crate::log_detail!(
                    "[client] session key established from TCP handshake ({})",
                    session.client_addr.ip(),
                );
                crate::log_trace!(
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
                    Some(src) => crate::log_detail!(
                        "[client] pinning UDP source address to {} (ifindex {})",
                        src.ip,
                        src.ifindex
                    ),
                    None => crate::log_trace!(
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
                recv_count = recv_count.wrapping_add(1);
                if should_log_debug(recv_count) {
                    let registered_client = *shared.client_addr.lock().unwrap();
                    let expected_ip = *shared.expected_client_ip.lock().unwrap();
                    crate::log_trace!(
                        "[client] udp recv #{recv_count}: from={addr} len={len} registered_client={registered_client:?} expected_ip={expected_ip:?} has_cipher={}",
                        local_cipher.is_some()
                    );
                }

                // All client→daemon UDP packets are encrypted:
                // [seq_num(4 BE)] [ciphertext] [tag(16)]
                if len < 4 + crate::crypto::TAG_LEN {
                    short_drop_count = short_drop_count.wrapping_add(1);
                    if should_log_debug(short_drop_count) {
                        crate::log_trace!(
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
                            crate::log_trace!(
                                "[client] drop udp packet #{ip_mismatch_drop_count}: from={addr} registered_client={registered_client:?} expected_ip={expected_ip:?} reason=ip_mismatch"
                            );
                        }
                        continue;
                    }
                    if let Some(ref mut r) = *client {
                        if *r != addr {
                            crate::log_trace!("[client] udp client address updated: {r} -> {addr}");
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
                            crate::log_trace!(
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
                            crate::log_trace!(
                                "[client] drop udp packet #{decrypt_fail_count}: from={addr} seq={seq_num} len={len} reason=decrypt_failed"
                            );
                        }
                        continue;
                    }
                };

                last_keepalive = Instant::now();
                if should_log_debug(recv_count) {
                    crate::log_trace!(
                        "[client] udp packet authenticated: from={addr} seq={seq_num} plaintext_len={}",
                        plaintext.len()
                    );
                }

                if input_seq_initialized && seq_is_stale(seq_num, input_seq_expected) {
                    crate::log_trace!(
                        "[client] drop udp packet: from={addr} seq={seq_num} expected_seq={input_seq_expected} reason=stale_out_of_order"
                    );
                    continue;
                }
                if input_seq_initialized && seq_num != input_seq_expected {
                    crate::log_trace!(
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
                        crate::log_detail!("[client] authenticated UDP client: {addr}");
                        *shared.expected_client_ip.lock().unwrap() = None;
                        shared.mark_network_session_active(current_session_id);
                        pending_started_at = None;
                        for stream in &shared.display_streams {
                            let stream = stream.lock().unwrap();
                            stream.force_idr.store(true, Ordering::Relaxed);
                            stream.force_refresh.store(true, Ordering::Relaxed);
                        }
                        shared.pending_settings.1.notify_all();
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
                            let hb_frame_id = heartbeat_sequence.take_frame()?;
                            let hb_flags: u8 = 0x80;
                            let nonce = crate::crypto::nonce_server(
                                hb_frame_id,
                                0,
                                hb_flags,
                                heartbeat_sequence.generation,
                            );
                            let header = build_header(
                                hb_frame_id,
                                0,
                                0,
                                0,
                                hb_flags,
                                0,
                                HEARTBEAT_MAGIC.len() as u16,
                                0,
                                heartbeat_sequence.generation,
                            );
                            let hb_magic_len = HEARTBEAT_MAGIC.len();
                            let mut hb_buf = [0u8; HEADER_LEN + 64];
                            hb_buf[..HEADER_LEN].copy_from_slice(&header);
                            hb_buf[HEADER_LEN..HEADER_LEN + hb_magic_len]
                                .copy_from_slice(HEARTBEAT_MAGIC);
                            let enc_len = c.encrypt_slice(
                                &nonce,
                                &header,
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
                match plaintext.split_first() {
                    Some((&protocol::udp::KEEPALIVE, _)) => {
                        crate::log_trace!("[client] keepalive received: from={addr} seq={seq_num} plaintext_len={pt_len}");
                    }
                    Some((&protocol::udp::CAMERA, cam_data)) => {
                        if let Some(jpeg) = cam_reassembler.feed(cam_data) {
                            let mut cam = shared.cam_writer.lock().unwrap();
                            if let Some(ref mut cw) = *cam {
                                cw.write_frame(&jpeg);
                            }
                        }
                    }
                    Some((&protocol::udp::AUDIO_REPORT, body)) => {
                        if let Some(report) = crate::audio::AudioReport::parse(body) {
                            let mut reports = shared.audio_reports.lock().unwrap();
                            if reports.len() >= 16 {
                                reports.pop_front();
                            }
                            reports.push_back(report);
                        }
                    }
                    Some((&protocol::udp::MIC, body)) if body.len() > 4 => {
                        let mic_seq = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                        let mut mic = shared.mic_writer.lock().unwrap();
                        if let Some(ref mut mw) = *mic {
                            if let Err(e) = mw.write_opus_packet(mic_seq, &body[4..]) {
                                eprintln!("[mic] write error: {e}");
                            }
                        }
                    }
                    _ => {
                        crate::log_trace!(
                            "[client] ignoring unknown udp payload from {addr} ({pt_len} bytes)"
                        );
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
                    let hb_frame_id = heartbeat_sequence.take_frame()?;
                    let hb_flags: u8 = 0x80;
                    let nonce = crate::crypto::nonce_server(
                        hb_frame_id,
                        0,
                        hb_flags,
                        heartbeat_sequence.generation,
                    );
                    let header = build_header(
                        hb_frame_id,
                        0,
                        0,
                        0,
                        hb_flags,
                        0,
                        HEARTBEAT_MAGIC.len() as u16,
                        0,
                        heartbeat_sequence.generation,
                    );
                    let hb_magic_len = HEARTBEAT_MAGIC.len();
                    let mut hb_buf = [0u8; HEADER_LEN + 64]; // enough for magic + tag
                    hb_buf[..HEADER_LEN].copy_from_slice(&header);
                    hb_buf[HEADER_LEN..HEADER_LEN + hb_magic_len].copy_from_slice(HEARTBEAT_MAGIC);
                    let enc_len =
                        c.encrypt_slice(&nonce, &header, &mut hb_buf[HEADER_LEN..], hb_magic_len);
                    match send_to_from(
                        &socket,
                        &hb_buf[..HEADER_LEN + enc_len],
                        addr,
                        session_udp_source,
                        None,
                    ) {
                        Ok(sent) => {
                            if should_log_debug(heartbeat_count) {
                                crate::log_trace!(
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

    crate::log_detail!("[client] manager stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// UDP sender — called synchronously from the capture thread
// ---------------------------------------------------------------------------

const MAX_RS_CACHE_ENTRIES: usize = 64;

#[cfg(test)]
mod display_stream_tests {
    use super::*;

    #[test]
    fn retiring_session_cannot_consume_new_session_messages() {
        let mut queue = VecDeque::from([
            (1, "stale"),
            (2, "current"),
            (3, "new"),
            (2, "also current"),
        ]);
        assert_eq!(
            take_session_messages(&mut queue, 2),
            vec!["current", "also current"]
        );
        assert_eq!(queue, VecDeque::from([(3, "new")]));
    }

    #[test]
    fn reconnect_waits_for_old_cleanup_and_stale_teardown_cannot_drop_it() {
        let shared = crate::test_support::shared_state();
        let session = shared.reserve_network_session().unwrap();
        shared.has_active_client.store(true, Ordering::Release);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (finish_tx, finish_rx) = std::sync::mpsc::sync_channel(1);
        let finish_rx = Mutex::new(finish_rx);
        *shared.on_client_disconnected.lock().unwrap() = Some(Box::new(move || {
            started_tx.send(()).unwrap();
            finish_rx.lock().unwrap().recv().unwrap();
        }));
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::spawn(move || drop_network_client(&worker_shared, session));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(shared.reserve_network_session().is_none());
        assert!(!shared.is_current_network_session(session));
        finish_tx.send(()).unwrap();
        worker.join().unwrap();
        let next = shared.reserve_network_session().unwrap();
        assert_ne!(session, next);
        drop_network_client(&shared, session);
        assert!(shared.is_current_network_session(next));
    }

    #[test]
    fn header_matches_swift_fixture() {
        assert_eq!(
            build_header(0x01020304, 2, 3, 1, 0x11, 1, 4, 0x05060708, 0x090a0b0c),
            [1, 2, 3, 4, 0, 2, 0, 3, 0, 1, 0x11, 1, 0, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
    }

    #[test]
    fn failed_video_send_does_not_reuse_the_frame_nonce() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut sender =
            UdpSender::new(UdpSocket::bind("127.0.0.1:0").unwrap(), DisplayId::Internal);
        sender.set_cipher(crate::crypto::SessionCipher::new(&[0x42; 32]));
        let au = EncodedAccessUnit {
            is_idr: true,
            annex_b: crate::encode::AnnexB::Vec(vec![1, 2, 3, 4]),
        };
        let tuning = StreamTuning {
            bitrate_bps: 20_000_000,
            idr_fec_percent: 0,
            pframe_fec_percent: 0,
        };
        assert!(sender
            .send_frame(&au, "127.0.0.1:0".parse().unwrap(), 0, 0, tuning)
            .is_err());
        sender
            .send_frame(&au, receiver.local_addr().unwrap(), 0, 0, tuning)
            .unwrap();
        let mut packet = [0u8; 2048];
        receiver.recv_from(&mut packet).unwrap();
        assert_eq!(u32::from_be_bytes(packet[..4].try_into().unwrap()), 1);
    }

    #[test]
    fn pli_targets_only_the_current_generation_of_one_display() {
        let streams = [
            DisplayStreamControl::new(20_000_000),
            DisplayStreamControl::new(20_000_000),
        ];
        streams[0].generation.store(10, Ordering::Release);
        streams[1].generation.store(11, Ordering::Release);
        streams[0].request_keyframe(9);
        streams[1].request_keyframe(10);
        assert!(!streams[0].force_idr.load(Ordering::Relaxed));
        assert!(!streams[1].force_idr.load(Ordering::Relaxed));
        streams[1].request_keyframe(11);
        assert!(!streams[0].force_idr.load(Ordering::Relaxed));
        assert!(!streams[0].force_refresh.load(Ordering::Relaxed));
        assert!(streams[1].force_idr.load(Ordering::Relaxed));
        assert!(streams[1].force_refresh.load(Ordering::Relaxed));
        assert!(streams[0]
            .adaptive_stream
            .lock()
            .unwrap()
            .pli_events
            .is_empty());
        assert_eq!(
            streams[1].adaptive_stream.lock().unwrap().pli_events.len(),
            1
        );
        assert_eq!(DisplayId::from_wire(2), None);
    }

    #[test]
    fn two_video_senders_route_and_authenticate_independently() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let target = receiver.local_addr().unwrap();
        let key = [0x42; 32];
        let cipher = crate::crypto::SessionCipher::new(&key);
        let mut generations = Vec::new();
        for display in [
            DisplayId::Internal,
            DisplayId::External,
            DisplayId::External,
        ] {
            // The last sender represents tearing down and recreating the external worker.
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut sender = UdpSender::new(socket, display);
            sender.set_cipher(crate::crypto::SessionCipher::new(&key));
            let au = EncodedAccessUnit {
                is_idr: true,
                annex_b: crate::encode::AnnexB::Vec(vec![1, 2, 3, 4]),
            };
            let tuning = StreamTuning {
                bitrate_bps: 20_000_000,
                idr_fec_percent: 0,
                pframe_fec_percent: 0,
            };
            sender.send_frame(&au, target, 123, 1, tuning).unwrap();
            let mut packet = [0u8; 2048];
            let (len, _) = receiver.recv_from(&mut packet).unwrap();
            assert_eq!(len, HEADER_LEN + 4 + crate::crypto::TAG_LEN);
            assert_eq!(u32::from_be_bytes(packet[..4].try_into().unwrap()), 0);
            assert_eq!(
                packet[10],
                FLAG_IDR
                    | if display == DisplayId::External {
                        FLAG_EXTERNAL
                    } else {
                        0
                    }
            );
            assert_eq!(packet[11], 1);
            assert_eq!(u32::from_be_bytes(packet[14..18].try_into().unwrap()), 123);
            let generation = u32::from_be_bytes(packet[18..22].try_into().unwrap());
            assert_eq!(generation, sender.generation());
            assert!(!generations.contains(&generation));
            generations.push(generation);
            let nonce = crate::crypto::nonce_server(0, 0, packet[10], generation);
            let header: [u8; HEADER_LEN] = packet[..HEADER_LEN].try_into().unwrap();
            assert_eq!(
                cipher
                    .decrypt(&nonce, &header, &mut packet[HEADER_LEN..len])
                    .unwrap(),
                &[1, 2, 3, 4]
            );
        }
    }
}

pub struct UdpSender {
    socket: UdpSocket,
    packet_pool: Vec<[u8; MAX_PACKET_LEN]>,
    packet_lens: Vec<usize>,
    sequence: crate::crypto::ServerPacketSequence,
    display_id: crate::capture::DisplayId,
    stats_start: Instant,
    stats_frames: u64,
    stats_bytes: u64,
    shard_pool: Vec<Vec<u8>>,
    rs_cache: HashMap<(usize, usize), ReedSolomon>,
    cipher: Option<crate::crypto::SessionCipher>,
    source: Option<UdpSource>,
    cmsg_buf: Option<PktinfoCmsg>,
    log_sent_aus: bool,
    /// Wire time one frame is allotted, used to pace large frames.
    frame_interval: Duration,
}

impl UdpSender {
    pub fn new(socket: UdpSocket, display_id: crate::capture::DisplayId) -> Self {
        Self {
            socket,
            packet_pool: Vec::new(),
            packet_lens: Vec::new(),
            sequence: crate::crypto::ServerPacketSequence::new(),
            display_id,
            stats_start: Instant::now(),
            stats_frames: 0,
            stats_bytes: 0,
            shard_pool: Vec::new(),
            rs_cache: HashMap::new(),
            cipher: None,
            source: None,
            cmsg_buf: None,
            log_sent_aus: std::env::var_os("SCREX_LOG_SENT_AUS").is_some(),
            frame_interval: Duration::from_micros(1_000_000 / 60),
        }
    }

    pub fn set_cipher(&mut self, cipher: crate::crypto::SessionCipher) {
        self.cipher = Some(cipher);
    }

    pub fn generation(&self) -> u32 {
        self.sequence.generation
    }

    pub fn replace_sequence(&mut self, sequence: crate::crypto::ServerPacketSequence) {
        self.sequence = sequence;
    }

    pub fn set_source(&mut self, source: Option<UdpSource>) {
        self.source = source;
        self.cmsg_buf = source.map(build_pktinfo_cmsg);
    }

    /// Per-session parameters: the negotiated framerate (for pacing).
    pub fn set_session(&mut self, fps: u32) {
        self.frame_interval = Duration::from_micros(1_000_000 / u64::from(fps.max(1)));
    }

    /// How many parity shards to actually send for a frame. Frames that fit
    /// in one Reed-Solomon block use a single block. Larger frames
    /// (typically keyframes) are split into several blocks.
    fn parity_shards_for(&self, data_count: usize, wanted_parity: usize) -> usize {
        if wanted_parity == 0 {
            return 0;
        }
        let single_block = wanted_parity.min(MAX_FEC_SHARDS);
        if data_count + single_block <= MAX_SHARDS_PER_FEC_BLOCK {
            return single_block;
        }
        // Every block needs at least one parity shard; raising the parity
        // count can raise the block count, so settle on a fixed point.
        let mut parity = wanted_parity.min(data_count);
        loop {
            let blocks = fec_block_count(data_count, parity);
            if parity >= blocks {
                break;
            }
            parity = blocks;
        }
        if data_count + parity > MAX_SHARDS_PER_FRAME {
            return 0;
        }
        parity
    }

    pub fn send_frame(
        &mut self,
        au: &EncodedAccessUnit,
        client_addr: SocketAddr,
        timestamp_ms: u32,
        codec_id: u8,
        tuning: StreamTuning,
    ) -> Result<u64> {
        let frame_id = self.sequence.take_frame()?;
        let payload: &[u8] = &*au.annex_b;
        let is_idr = au.is_idr;
        if self.log_sent_aus {
            let prefix_len = payload.len().min(24);
            println!(
                "[stream/send] frame_id={} codec={} idr={} bytes={} fnv=0x{:016x} prefix={:02x?}",
                frame_id,
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

        let wanted_parity = if data_count <= 1 || parity_percent == 0 {
            0
        } else {
            data_count
                .saturating_mul(parity_percent)
                .div_ceil(100)
                .max(1)
        };
        let actual_parity = self.parity_shards_for(data_count, wanted_parity);
        let fec_blocks = fec_block_count(data_count, actual_parity);
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

        // Generate parity shards, one Reed-Solomon block at a time. Frames
        // that fit in 255 shards are a single block, exactly as before.
        if actual_parity > 0 {
            let (data_shards, parity_shards) =
                self.shard_pool[..total_shards].split_at_mut(data_count);
            for block in 0..fec_blocks {
                let (data_start, block_data) = fec_block_range(data_count, fec_blocks, block);
                let (parity_start, block_parity) =
                    fec_block_range(actual_parity, fec_blocks, block);
                if self.rs_cache.len() >= MAX_RS_CACHE_ENTRIES {
                    self.rs_cache.clear();
                }
                let rs = match self.rs_cache.entry((block_data, block_parity)) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        let rs = ReedSolomon::new(block_data, block_parity)
                            .map_err(|e| anyhow::anyhow!("RS init: {e:?}"))?;
                        entry.insert(rs)
                    }
                };
                rs.encode_sep(
                    &data_shards[data_start..data_start + block_data],
                    &mut parity_shards[parity_start..parity_start + block_parity],
                )
                .map_err(|e| anyhow::anyhow!("RS encode: {e:?}"))?;
            }
        }

        let flags = if is_idr { FLAG_IDR } else { 0 }
            | if self.display_id == crate::capture::DisplayId::External {
                FLAG_EXTERNAL
            } else {
                0
            };

        for idx in 0..total_shards {
            // Data shards go out at their real length (only the last one is
            // short); the receiver zero-pads before FEC recovery. Parity
            // shards are always full-size.
            let actual_payload_len = if idx < data_count {
                let start = idx * CHUNK_PAYLOAD;
                let end = (start + CHUNK_PAYLOAD).min(payload.len());
                end - start
            } else {
                CHUNK_PAYLOAD
            };

            let header = build_header(
                frame_id,
                idx as u16,
                data_count as u16,
                actual_parity as u16,
                flags,
                codec_id,
                actual_payload_len as u16,
                timestamp_ms,
                self.sequence.generation,
            );

            self.packet_pool[idx][..HEADER_LEN].copy_from_slice(&header);
            self.packet_pool[idx][HEADER_LEN..HEADER_LEN + actual_payload_len]
                .copy_from_slice(&self.shard_pool[idx][..actual_payload_len]);

            self.packet_lens[idx] = if let Some(ref cipher) = self.cipher {
                let nonce = crate::crypto::nonce_server(
                    frame_id,
                    idx as u16,
                    flags,
                    self.sequence.generation,
                );
                let enc_len = cipher.encrypt_slice(
                    &nonce,
                    &header,
                    &mut self.packet_pool[idx][HEADER_LEN..],
                    actual_payload_len,
                );
                HEADER_LEN + enc_len
            } else {
                HEADER_LEN + actual_payload_len
            };
        }

        let frame_bytes = self.send_frame_batch(client_addr, total_shards)?;

        self.stats_frames += 1;
        self.stats_bytes += frame_bytes;

        if self.stats_start.elapsed() >= Duration::from_secs(1) {
            let elapsed = self.stats_start.elapsed().as_secs_f64();
            let fps = self.stats_frames as f64 / elapsed;
            let mbps = (self.stats_bytes as f64 * 8.0 / elapsed) / 1_000_000.0;
            crate::log_trace!(
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

        // Typical P-frames go out in one burst. Large frames (keyframes,
        // scene changes) are spread evenly across part of the frame interval
        // in small batches: a few hundred back-to-back datagrams overflow
        // Wi-Fi access point queues and get dropped, and they also hold up
        // the audio packets that share this socket.
        let paced = total_shards > PACING_THRESHOLD_SHARDS;
        let batch_size = if paced {
            PACING_BATCH_SIZE
        } else {
            SENDMMSG_BATCH_SIZE
        };
        let pacing_start = Instant::now();
        let pacing_window =
            self.frame_interval * PACING_SPREAD_NUM as u32 / PACING_SPREAD_DEN as u32;

        while sent < total_shards {
            if paced && sent > 0 {
                // Release batch k at k/n of the window. Falling behind (slow
                // send, long sleep) just sends the next batch immediately.
                let due = pacing_window.mul_f64(sent as f64 / total_shards as f64);
                if let Some(wait) = due.checked_sub(pacing_start.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
            let end = (sent + batch_size).min(total_shards);
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
                        crate::log_trace!(
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
        }

        Ok(frame_bytes)
    }
}

/// Number of Reed-Solomon blocks a frame's shards are split into. Must match
/// the iPad client's `FrameAssembly` layout exactly. A frame that fits in 255
/// shards is one block; otherwise blocks hold at most 254 shards on average,
/// so the uneven split in `fec_block_range` never exceeds 255 in any block.
pub fn fec_block_count(data_shards: usize, parity_shards: usize) -> usize {
    if parity_shards == 0 || data_shards + parity_shards <= MAX_SHARDS_PER_FEC_BLOCK {
        1
    } else {
        (data_shards + parity_shards).div_ceil(MAX_SHARDS_PER_FEC_BLOCK - 1)
    }
}

/// `(start, len)` of block `index` when `total` shards are split as evenly as
/// possible into `blocks` contiguous blocks (earlier blocks get the extras).
pub fn fec_block_range(total: usize, blocks: usize, index: usize) -> (usize, usize) {
    let base = total / blocks;
    let extra = total % blocks;
    (
        index * base + index.min(extra),
        base + usize::from(index < extra),
    )
}

// ---------------------------------------------------------------------------
// Audio sender — separate frame_id space; redundant copies instead of FEC
// ---------------------------------------------------------------------------

/// Copies of earlier packets reach back at most this many frame IDs.
pub const AUDIO_MAX_DISTANCE: u8 = 8;
const AUDIO_HISTORY: usize = AUDIO_MAX_DISTANCE as usize + 1;
/// Entries per datagram, the primary included.
const AUDIO_MAX_ENTRIES: usize = 4;

/// Builds a speaker datagram payload: `count(u8)` then entries of `distance(u8) + len(u16) +
/// opus`. The primary goes first with distance 0; copies follow in the order given and are
/// skipped once they no longer fit in `max_len`. Returns false if even the primary doesn't fit.
fn build_audio_payload<'a>(
    out: &mut Vec<u8>,
    primary: &[u8],
    copies: impl IntoIterator<Item = (u8, &'a [u8])>,
    max_len: usize,
) -> bool {
    out.clear();
    if 1 + 3 + primary.len() > max_len || primary.len() > usize::from(u16::MAX) {
        return false;
    }
    out.push(1);
    out.push(0);
    out.extend_from_slice(&(primary.len() as u16).to_be_bytes());
    out.extend_from_slice(primary);
    for (distance, opus) in copies {
        if usize::from(out[0]) == AUDIO_MAX_ENTRIES {
            break;
        }
        if out.len() + 3 + opus.len() > max_len {
            continue;
        }
        out.push(distance);
        out.extend_from_slice(&(opus.len() as u16).to_be_bytes());
        out.extend_from_slice(opus);
        out[0] += 1;
    }
    true
}

pub struct AudioSender {
    socket: UdpSocket,
    sequence: crate::crypto::ServerPacketSequence,
    send_buf: Vec<u8>,
    payload: Vec<u8>,
    cipher: Option<crate::crypto::SessionCipher>,
    source: Option<UdpSource>,
    /// Pinned source (if any) plus TOS EF, for IPv4 destinations.
    cmsg_buf: PktinfoCmsg,
    /// Recently sent packets by `frame_id % AUDIO_HISTORY`, for redundant copies.
    history: [(Option<u32>, Vec<u8>); AUDIO_HISTORY],
}

impl AudioSender {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            sequence: crate::crypto::ServerPacketSequence::new(),
            send_buf: vec![0u8; HEADER_LEN + CHUNK_PAYLOAD + crate::crypto::TAG_LEN],
            payload: Vec::with_capacity(CHUNK_PAYLOAD),
            cipher: None,
            source: None,
            cmsg_buf: build_ipv4_cmsg(None, TOS_EF),
            history: std::array::from_fn(|_| (None, Vec::new())),
        }
    }

    /// The generation the client's `AUDIO_REPORT`s must name.
    pub fn generation(&self) -> u32 {
        self.sequence.generation
    }

    /// Forgets sent packets, so a new speaker-card session never carries copies of audio from
    /// before it.
    pub fn reset_history(&mut self) {
        for entry in &mut self.history {
            entry.0 = None;
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
        self.cmsg_buf = build_ipv4_cmsg(source, TOS_EF);
    }

    /// Send one 10 ms Opus speaker packet plus copies of the packets `distances` frame IDs
    /// before it that were sent since the last `reset_history`.
    pub fn send_audio(
        &mut self,
        opus: &[u8],
        client_addr: SocketAddr,
        timestamp_ms: u32,
        distances: &[u8],
    ) -> Result<()> {
        let frame_id = self.sequence.take_frame()?;
        let history = &self.history;
        let copies = distances.iter().filter_map(|&distance| {
            if distance == 0 || distance > AUDIO_MAX_DISTANCE {
                return None;
            }
            let id = frame_id.checked_sub(u32::from(distance))?;
            let (stored, packet) = &history[id as usize % AUDIO_HISTORY];
            (*stored == Some(id)).then_some((distance, packet.as_slice()))
        });
        let fits = build_audio_payload(&mut self.payload, opus, copies, CHUNK_PAYLOAD);
        let slot = &mut self.history[frame_id as usize % AUDIO_HISTORY];
        slot.0 = Some(frame_id);
        slot.1.clear();
        slot.1.extend_from_slice(opus);
        anyhow::ensure!(fits, "{}-byte Opus packet does not fit in one datagram", opus.len());

        let payload_len = self.payload.len();
        let header = build_header(
            frame_id,
            0,
            1,
            0,
            FLAG_AUDIO,
            0, // codec byte is unused for audio (always Opus)
            payload_len as u16,
            timestamp_ms,
            self.sequence.generation,
        );
        self.send_buf[..HEADER_LEN].copy_from_slice(&header);
        self.send_buf[HEADER_LEN..HEADER_LEN + payload_len].copy_from_slice(&self.payload);

        let pkt_len = if let Some(ref cipher) = self.cipher {
            let nonce =
                crate::crypto::nonce_server(frame_id, 0, FLAG_AUDIO, self.sequence.generation);
            let enc_len =
                cipher.encrypt_slice(&nonce, &header, &mut self.send_buf[HEADER_LEN..], payload_len);
            HEADER_LEN + enc_len
        } else {
            HEADER_LEN + payload_len
        };

        let packet = &self.send_buf[..pkt_len];
        let sent = match client_addr {
            SocketAddr::V4(_) => send_to_with_cmsg(&self.socket, packet, client_addr, &self.cmsg_buf),
            SocketAddr::V6(_) => send_to_from(&self.socket, packet, client_addr, self.source, None),
        };
        sent.map(|_| ()).map_err(|e| anyhow::anyhow!("audio send: {e}"))
    }
}

#[cfg(test)]
mod audio_payload_tests {
    use super::*;

    fn entries(payload: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        let mut off = 1;
        for _ in 0..payload[0] {
            let distance = payload[off];
            let len = usize::from(u16::from_be_bytes([payload[off + 1], payload[off + 2]]));
            out.push((distance, payload[off + 3..off + 3 + len].to_vec()));
            off += 3 + len;
        }
        assert_eq!(off, payload.len());
        out
    }

    #[test]
    fn primary_first_then_copies_in_order() {
        let mut out = Vec::new();
        let copies: [(u8, &[u8]); 2] = [(1, &[7, 7]), (3, &[9])];
        assert!(build_audio_payload(&mut out, &[1, 2, 3], copies, 1400));
        assert_eq!(entries(&out), vec![(0, vec![1, 2, 3]), (1, vec![7, 7]), (3, vec![9])]);
    }

    #[test]
    fn copies_that_do_not_fit_are_skipped() {
        let mut out = Vec::new();
        let big = vec![0u8; 600];
        let copies: [(u8, &[u8]); 3] = [(1, &big), (2, &big), (3, &[5])];
        assert!(build_audio_payload(&mut out, &big, copies, 1400));
        assert_eq!(entries(&out).iter().map(|e| e.0).collect::<Vec<_>>(), vec![0, 1, 3]);
        assert!(out.len() <= 1400);
        assert!(!build_audio_payload(&mut out, &vec![0u8; 1400], [], 1400));
    }

    #[test]
    fn at_most_four_entries() {
        let mut out = Vec::new();
        let data: Vec<[u8; 1]> = (1..=8).map(|d| [d]).collect();
        let copies: Vec<(u8, &[u8])> = data.iter().map(|d| (d[0], &d[..])).collect();
        assert!(build_audio_payload(&mut out, &[0], copies, 1400));
        assert_eq!(out[0] as usize, AUDIO_MAX_ENTRIES);
    }

    #[test]
    fn sender_only_copies_packets_it_sent_this_session() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut sender = AudioSender::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let addr = rx.local_addr().unwrap();
        let mut buf = [0u8; 2048];
        let mut recv = || {
            let n = rx.recv(&mut buf).unwrap();
            entries(&buf[HEADER_LEN..n])
        };
        for i in 0..4u8 {
            sender.send_audio(&[i], addr, 0, &[1, 3]).unwrap();
        }
        let _ = (recv(), recv(), recv());
        // Frame 3 carries frame 2 (distance 1) and frame 0 (distance 3).
        assert_eq!(recv(), vec![(0, vec![3]), (1, vec![2]), (3, vec![0])]);
        sender.reset_history();
        sender.send_audio(&[4], addr, 0, &[1, 3]).unwrap();
        assert_eq!(recv(), vec![(0, vec![4])]);
    }
}

#[cfg(test)]
mod fec_layout_tests {
    use super::*;

    #[test]
    fn single_block_when_it_fits() {
        assert_eq!(fec_block_count(200, 20), 1);
        assert_eq!(fec_block_count(235, 20), 1);
        assert_eq!(fec_block_count(400, 0), 1);
    }

    #[test]
    fn every_block_fits_rs_limits() {
        for data in 2..2000 {
            for percent in [5usize, 10, 14, 18] {
                let parity = (data * percent).div_ceil(100).max(1).min(data);
                let blocks = fec_block_count(data, parity);
                if data + parity <= MAX_SHARDS_PER_FEC_BLOCK {
                    assert_eq!(blocks, 1);
                }
                let parity = parity.max(blocks);
                let blocks = fec_block_count(data, parity);
                let mut data_total = 0;
                let mut parity_total = 0;
                for b in 0..blocks {
                    let (ds, dl) = fec_block_range(data, blocks, b);
                    let (ps, pl) = fec_block_range(parity, blocks, b);
                    assert_eq!(ds, data_total);
                    assert_eq!(ps, parity_total);
                    assert!(dl >= 1 && pl >= 1, "data={data} parity={parity} block={b}");
                    assert!(
                        dl + pl <= MAX_SHARDS_PER_FEC_BLOCK,
                        "data={data} parity={parity}"
                    );
                    data_total += dl;
                    parity_total += pl;
                }
                assert_eq!((data_total, parity_total), (data, parity));
            }
        }
    }

    #[test]
    fn multiblock_encode_recovers_per_block() {
        let data = 400;
        let parity = 40;
        let blocks = fec_block_count(data, parity);
        assert!(blocks > 1);
        let mut shards: Vec<Vec<u8>> = (0..data + parity)
            .map(|i| {
                if i < data {
                    (0..64).map(|j| (i * 31 + j) as u8).collect()
                } else {
                    vec![0; 64]
                }
            })
            .collect();
        let original = shards.clone();
        let (data_shards, parity_shards) = shards.split_at_mut(data);
        for b in 0..blocks {
            let (ds, dl) = fec_block_range(data, blocks, b);
            let (ps, pl) = fec_block_range(parity, blocks, b);
            ReedSolomon::new(dl, pl)
                .unwrap()
                .encode_sep(&data_shards[ds..ds + dl], &mut parity_shards[ps..ps + pl])
                .unwrap();
        }
        // Lose one data shard per block, recover each block independently.
        for b in 0..blocks {
            let (ds, dl) = fec_block_range(data, blocks, b);
            let (ps, pl) = fec_block_range(parity, blocks, b);
            let mut block: Vec<Option<Vec<u8>>> = (ds..ds + dl)
                .chain(data + ps..data + ps + pl)
                .map(|i| Some(shards[i].clone()))
                .collect();
            block[dl / 2] = None;
            ReedSolomon::new(dl, pl)
                .unwrap()
                .reconstruct(&mut block)
                .unwrap();
            assert_eq!(block[dl / 2].as_ref().unwrap(), &original[ds + dl / 2]);
        }
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn accepts_only_supported_audio_bitrates_and_zero_default() {
        let mut body = vec![0u8; SETTINGS_BODY_LEN];
        body[20..33].copy_from_slice(b"Screx        ");
        body[10] = 1;
        body[18] = 1;
        for bitrate in [0u32, 96_000, 128_000, 192_000, 256_000] {
            body[37..41].copy_from_slice(&bitrate.to_be_bytes());
            let parsed = parse_settings_body(&body).unwrap();
            assert_eq!(parsed.audio_bitrate_bps, if bitrate == 0 { 128_000 } else { bitrate });
        }
        for bitrate in [1u32, 64_000, 500_000, u32::MAX] {
            body[37..41].copy_from_slice(&bitrate.to_be_bytes());
            assert!(parse_settings_body(&body).is_none());
        }
    }

    #[test]
    fn parses_explicit_settings() {
        let mut body = Vec::new();
        body.extend_from_slice(&1920u16.to_be_bytes());
        body.extend_from_slice(&1080u16.to_be_bytes());
        body.push(60);
        body.push(protocol::CODEC_H265);
        body.extend_from_slice(&15_000_000u32.to_be_bytes());
        body.push(3);
        body.extend_from_slice(&3840u16.to_be_bytes());
        body.extend_from_slice(&2160u16.to_be_bytes());
        body.extend_from_slice(&7u32.to_be_bytes());
        body.push(30);
        body.extend_from_slice(b"iPad A16 11\" ");
        body.extend_from_slice(&235u16.to_be_bytes());
        body.extend_from_slice(&163u16.to_be_bytes());
        body.extend_from_slice(&192_000u32.to_be_bytes());
        let s = parse_settings_body(&body).unwrap();
        assert_eq!((s.width, s.height), (Some(1920), Some(1080)));
        assert_eq!(s.fps, Some(60));
        assert_eq!(s.codec, Some(VideoCodec::H265));
        assert_eq!(s.bitrate_bps, Some(15_000_000));
        assert_eq!(s.enabled_mask, 3);
        assert_eq!(
            (s.external_width, s.external_height),
            (Some(3840), Some(2160))
        );
        assert_eq!(s.revision, 7);
        assert_eq!(s.external_fps, Some(30));
        assert_eq!(s.internal_display_name, Some(*b"iPad A16 11\" "));
        assert_eq!(s.internal_size_mm, Some((235, 163)));
        assert_eq!(s.audio_bitrate_bps, 192_000);
    }

    #[test]
    fn zero_or_implausible_panel_sizes_are_unknown() {
        let mut body = vec![0u8; SETTINGS_BODY_LEN];
        body[20..33].copy_from_slice(b"Screx        ");
        body[10] = 1;
        body[18] = 1;
        for (w, h) in [(0, 0), (235, 0), (10, 10), (5000, 3000)] {
            body[33..35].copy_from_slice(&u16::to_be_bytes(w));
            body[35..37].copy_from_slice(&u16::to_be_bytes(h));
            assert_eq!(parse_settings_body(&body).unwrap().internal_size_mm, None);
        }
    }

    #[test]
    fn zero_and_codec_default_mean_daemon_default() {
        let mut body = vec![0u8; SETTINGS_BODY_LEN];
        body[20..33].copy_from_slice(b"Screx        ");
        body[5] = protocol::CODEC_DEFAULT;
        body[10] = 1;
        body[18] = 1;
        let s = parse_settings_body(&body).unwrap();
        assert_eq!((s.width, s.height, s.fps), (None, None, None));
        assert_eq!(s.codec, None);
        assert_eq!(s.bitrate_bps, None);
        assert_eq!(s.external_fps, None);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(parse_settings_body(&[0u8; SETTINGS_BODY_LEN - 1]).is_none());
        assert!(parse_settings_body(&[0u8; SETTINGS_BODY_LEN + 1]).is_none());
    }

    #[test]
    fn rejects_invalid_masks_revisions_and_legacy_settings() {
        let mut body = vec![0; SETTINGS_BODY_LEN];
        body[20..33].copy_from_slice(b"Screx        ");
        body[18] = 1;
        for mask in [0, 4, 255] {
            body[10] = mask;
            assert!(parse_settings_body(&body).is_none());
        }
        body[10] = 1;
        body[18] = 0;
        assert!(parse_settings_body(&body).is_none());
        assert!(parse_settings_body(&[0; 10]).is_none());
        assert!(parse_settings_body(&[0; 19]).is_none());
        assert!(parse_settings_body(&[0; 20]).is_none());
    }

    #[test]
    fn rejects_unsafe_or_empty_edid_names() {
        let mut body = vec![0; SETTINGS_BODY_LEN];
        body[10] = 1;
        body[18] = 1;
        body[20..33].fill(b' ');
        assert!(parse_settings_body(&body).is_none());
        for byte in [b'\'', b'\\', b'\n', 0, 0xff] {
            body[20..33].copy_from_slice(b"Screx        ");
            body[20] = byte;
            assert!(parse_settings_body(&body).is_none());
        }
    }

    #[test]
    fn external_fps_is_clamped_independently() {
        let shared = crate::test_support::shared_state();
        let settings = clamp_requested_settings(
            &shared,
            RequestedSettings {
                fps: Some(30),
                external_fps: Some(120),
                ..Default::default()
            },
        );
        assert_eq!(settings.fps, Some(30));
        assert_eq!(settings.external_fps, Some(60));
    }
}
