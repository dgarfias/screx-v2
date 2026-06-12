use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::audio::MicWriter;
use crate::camera::{CamReassembler, CamWriter, CameraConfig};
use crate::encode::EncodedAccessUnit;
use crate::uinput::{VirtualKeyboard, VirtualTouchscreen};
use crate::usb::TcpFramedSender;

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
const GPAD_MAGIC: &[u8] = b"GPAD";
const SPEAKER_MAGIC: &[u8] = b"SPKR";
const MICCFG_MAGIC: &[u8] = b"MICCFG";
const CAMCFG_MAGIC: &[u8] = b"CAMCFG";

static MOUSE_CTRL_COUNT: AtomicU64 = AtomicU64::new(0);
static RAWKEY_CTRL_COUNT: AtomicU64 = AtomicU64::new(0);
static KEY_CTRL_COUNT: AtomicU64 = AtomicU64::new(0);
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

/// Source address pinning for daemon→client UDP packets.
///
/// On multi-homed hosts (e.g. ethernet + wifi on the same subnet) the kernel
/// may route UDP replies out a different interface — and with a different
/// source IP — than the one the client connected to. iOS connected UDP
/// sockets silently drop datagrams whose source doesn't match the address
/// they connected to, so every video/audio/heartbeat packet is lost.
/// We pin the source IP (and interface) to the local address of the TCP
/// handshake using an IP_PKTINFO control message on each send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpSource {
    pub ip: std::net::Ipv4Addr,
    pub ifindex: u32,
}

/// Find the interface index owning a local IPv4 address (0 if unknown).
pub fn ifindex_for_ipv4(ip: std::net::Ipv4Addr) -> u32 {
    unsafe {
        let mut ifap: *mut libc::ifaddrs = ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return 0;
        }
        let mut found = 0u32;
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null()
                && (*ifa.ifa_addr).sa_family == libc::AF_INET as libc::sa_family_t
            {
                let sin = ifa.ifa_addr as *const libc::sockaddr_in;
                let addr = std::net::Ipv4Addr::from(u32::from_be((*sin).sin_addr.s_addr));
                if addr == ip {
                    found = libc::if_nametoindex(ifa.ifa_name);
                    break;
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        found
    }
}

/// Resolve a session's UDP source pinning from the TCP handshake local IP.
pub fn udp_source_for_local_ip(local_ip: Option<std::net::IpAddr>) -> Option<UdpSource> {
    match local_ip {
        Some(std::net::IpAddr::V4(ip)) if !ip.is_unspecified() => Some(UdpSource {
            ip,
            ifindex: ifindex_for_ipv4(ip),
        }),
        _ => None,
    }
}

/// Build an IP_PKTINFO cmsg buffer that pins the IPv4 source address/interface.
fn build_pktinfo_cmsg(source: UdpSource) -> Vec<u8> {
    unsafe {
        let data_len = std::mem::size_of::<libc::in_pktinfo>() as libc::c_uint;
        let space = libc::CMSG_SPACE(data_len) as usize;
        let mut buf = vec![0u8; space];
        let cmsg = buf.as_mut_ptr() as *mut libc::cmsghdr;
        (*cmsg).cmsg_len = libc::CMSG_LEN(data_len) as _;
        (*cmsg).cmsg_level = libc::IPPROTO_IP;
        (*cmsg).cmsg_type = libc::IP_PKTINFO;
        let pi = libc::CMSG_DATA(cmsg) as *mut libc::in_pktinfo;
        std::ptr::write_unaligned(
            pi,
            libc::in_pktinfo {
                ipi_ifindex: source.ifindex as libc::c_int,
                ipi_spec_dst: libc::in_addr {
                    s_addr: u32::from(source.ip).to_be(),
                },
                ipi_addr: libc::in_addr { s_addr: 0 },
            },
        );
        buf
    }
}

/// `send_to` with an optional pinned IPv4 source address (IP_PKTINFO).
/// Falls back to a plain `send_to` when no source is pinned or dst is IPv6.
pub fn send_to_from(
    socket: &UdpSocket,
    buf: &[u8],
    dst: SocketAddr,
    source: Option<UdpSource>,
) -> std::io::Result<usize> {
    let source = match (source, dst) {
        (Some(s), SocketAddr::V4(_)) => s,
        _ => return socket.send_to(buf, dst),
    };

    let (mut addr_storage, addr_len) = socket_addr_to_raw(dst);
    let mut cmsg_buf = build_pktinfo_cmsg(source);
    let mut iov = libc::iovec {
        iov_base: buf.as_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let msg = libc::msghdr {
        msg_name: (&mut addr_storage as *mut libc::sockaddr_storage).cast(),
        msg_namelen: addr_len,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: cmsg_buf.as_mut_ptr().cast(),
        msg_controllen: cmsg_buf.len() as _,
        msg_flags: 0,
    };
    let ret = unsafe { libc::sendmsg(socket.as_raw_fd(), &msg, 0) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
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
    pub usb_sender: Mutex<Option<TcpFramedSender>>,
    pub usb_active: AtomicBool,
    pub virtual_touch: Mutex<Option<VirtualTouchscreen>>,
    pub virtual_keyboard: Mutex<Option<VirtualKeyboard>>,
    pub virtual_mouse: Mutex<Option<crate::uinput::VirtualMouse>>,
    pub virtual_gamepads: Mutex<HashMap<u8, crate::uinput::VirtualGamepad>>,
    pub cam_writer: Mutex<Option<CamWriter>>,
    pub camera_config: Mutex<CameraConfig>,
    pub camera_exclusive_caps: bool,
    pub mic_writer: Mutex<Option<MicWriter>>,
    pub start_time: Instant,
    pub on_client_connected: Mutex<Option<LifecycleCallback>>,
    pub on_client_disconnected: Mutex<Option<LifecycleCallback>>,
    pub has_active_client: AtomicBool,
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
}

impl SharedState {
    pub fn new(camera_exclusive_caps: bool, base_bitrate_bps: u32) -> Self {
        Self {
            client_addr: Mutex::new(None),
            force_idr: AtomicBool::new(false),
            force_refresh_handle: Mutex::new(None),
            capture_start: Arc::new(AtomicBool::new(false)),
            capture_start_signal: Arc::new((Mutex::new(false), Condvar::new())),
            capture_stop_flag: Arc::new(AtomicBool::new(false)),
            usb_sender: Mutex::new(None),
            usb_active: AtomicBool::new(false),
            virtual_touch: Mutex::new(None),
            virtual_keyboard: Mutex::new(None),
            virtual_mouse: Mutex::new(None),
            virtual_gamepads: Mutex::new(HashMap::new()),
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
            audio_module_id: Mutex::new(0),
            network_session_busy: AtomicBool::new(false),
            network_session_pending: AtomicBool::new(false),
            network_session_id: AtomicU64::new(0),
            session_key: Mutex::new(None),
            adaptive_stream: Mutex::new(AdaptiveStreamState::new(base_bitrate_bps)),
            expected_client_ip: Mutex::new(None),
            udp_source: Mutex::new(None),
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
        match crate::camera::ensure_v4l2loopback(shared.camera_exclusive_caps) {
            Ok(()) => match crate::camera::create_cam_writer(config) {
                Ok(writer) => {
                    *shared.cam_writer.lock().unwrap() = Some(writer);
                    println!(
                        "[camera] virtual webcam ready ({}x{} @ {}fps)",
                        config.width, config.height, config.fps
                    );
                }
                Err(e) => eprintln!("[camera] {e:#}"),
            },
            Err(e) => eprintln!("[camera] v4l2loopback not available ({e:#})"),
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

pub fn handle_periph_packet_data(shared: &Arc<SharedState>, data: &[u8]) {
    if data.len() < 2 {
        return;
    }
    let device_type = data[0];
    let state = data[1];

    match (device_type, state) {
        (PERIPH_MOUSE, PERIPH_ATTACHED) => {
            let mut m = shared.virtual_mouse.lock().unwrap();
            if m.is_none() {
                match crate::uinput::VirtualMouse::new() {
                    Ok(vm) => {
                        *m = Some(vm);
                        println!("[periph] physical mouse attached — virtual mouse created");
                    }
                    Err(e) => eprintln!("[periph] failed to create virtual mouse: {e}"),
                }
            }
        }
        (PERIPH_MOUSE, PERIPH_DETACHED) => {
            let mut m = shared.virtual_mouse.lock().unwrap();
            if let Some(mut vm) = m.take() {
                vm.release_all_buttons();
                println!("[periph] physical mouse detached — virtual mouse destroyed");
            }
        }
        (PERIPH_KEYBOARD, PERIPH_ATTACHED) => {
            println!("[periph] physical keyboard attached — using existing virtual keyboard");
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
        let mut touch = shared.virtual_touch.lock().unwrap();
        if let Some(ref mut ts) = *touch {
            crate::uinput::handle_touch_packet(ts, touch_data);
        }
        return;
    }

    if ctrl.starts_with(KEY_MAGIC) && ctrl.len() > KEY_MAGIC.len() {
        let key_data = &ctrl[KEY_MAGIC.len()..];
        let count = KEY_CTRL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        let key_type = key_data.first().copied().unwrap_or(0);
        if count == 1 || count % 50 == 0 || matches!(key_type, 0x02 | 0x04) {
            println!(
                "[control] parsed KEY packets={count} len={} type=0x{key_type:02x} bytes={:02x?}",
                key_data.len(),
                key_data
            );
        }
        let mut kb = shared.virtual_keyboard.lock().unwrap();
        if let Some(ref mut keyboard) = *kb {
            crate::uinput::handle_key_packet(keyboard, key_data);
        } else {
            eprintln!("[control] KEY packet dropped: no virtual keyboard available");
        }
        return;
    }

    if ctrl.starts_with(MOUSE_MAGIC) && ctrl.len() > MOUSE_MAGIC.len() {
        let mouse_data = &ctrl[MOUSE_MAGIC.len()..];
        let count = MOUSE_CTRL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count % 100 == 0 {
            println!(
                "[control] parsed MOUSE packets={count} len={}",
                mouse_data.len()
            );
        }
        let mut m = shared.virtual_mouse.lock().unwrap();
        if m.is_none() {
            match crate::uinput::VirtualMouse::new() {
                Ok(vm) => {
                    println!("[mouse] direct control active - virtual mouse created");
                    *m = Some(vm);
                }
                Err(e) => {
                    eprintln!("[mouse] failed to create virtual mouse for direct control: {e}");
                    return;
                }
            }
        }
        if let Some(ref mut vm) = *m {
            crate::uinput::handle_mouse_packet(vm, mouse_data);
        }
        return;
    }

    if ctrl.starts_with(RAWKEY_MAGIC) && ctrl.len() > RAWKEY_MAGIC.len() {
        let rk_data = &ctrl[RAWKEY_MAGIC.len()..];
        let count = RAWKEY_CTRL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count % 50 == 0 {
            println!(
                "[control] parsed RAWKEY packets={count} len={}",
                rk_data.len()
            );
        }
        let mut kb = shared.virtual_keyboard.lock().unwrap();
        if let Some(ref mut keyboard) = *kb {
            crate::uinput::handle_rawkey_packet(keyboard, rk_data);
        }
        return;
    }

    if ctrl.starts_with(PERIPH_MAGIC) && ctrl.len() > PERIPH_MAGIC.len() {
        let periph_data = &ctrl[PERIPH_MAGIC.len()..];
        handle_periph_packet_data(shared, periph_data);
        return;
    }

    if ctrl.starts_with(GPAD_MAGIC) && ctrl.len() > GPAD_MAGIC.len() {
        let gamepad_data = &ctrl[GPAD_MAGIC.len()..];
        handle_gamepad_packet_data(shared, gamepad_data);
    }
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
    let mut module_id = shared.audio_module_id.lock().unwrap();
    if *module_id > 0 {
        crate::audio::remove_virtual_sink(*module_id);
        *module_id = 0;
    }
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
            let mut pads = shared.virtual_gamepads.lock().unwrap();
            if pads.contains_key(&controller_id) {
                return;
            }
            match crate::uinput::VirtualGamepad::new(controller_id) {
                Ok(pad) => {
                    pads.insert(controller_id, pad);
                    println!(
                        "[gamepad] controller {} attached — virtual gamepad created",
                        controller_id + 1
                    );
                }
                Err(e) => eprintln!(
                    "[gamepad] failed to create virtual gamepad {}: {e}",
                    controller_id + 1
                ),
            }
        }
        GPAD_DETACHED => {
            let mut pads = shared.virtual_gamepads.lock().unwrap();
            if let Some(mut pad) = pads.remove(&controller_id) {
                pad.release_all();
                println!(
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

            crate::vlog!(
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

            let mut pads = shared.virtual_gamepads.lock().unwrap();
            if let Some(ref mut pad) = pads.get_mut(&controller_id) {
                pad.set_state(buttons_mask, lx, ly, rx, ry, lt, rt, hat_x, hat_y);
            }
        }
        _ => {}
    }
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

    if !shared.usb_active.load(Ordering::Relaxed)
        && shared.has_active_client.swap(false, Ordering::SeqCst)
    {
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

pub struct UdpSender {
    socket: UdpSocket,
    packet_pool: Vec<[u8; MAX_PACKET_LEN]>,
    packet_lens: Vec<usize>,
    frame_id: u32,
    stats_start: Instant,
    stats_frames: u64,
    stats_bytes: u64,
    shard_pool: Vec<Vec<u8>>,
    rs_cache: HashMap<(usize, usize), ReedSolomon>,
    cipher: Option<crate::crypto::SessionCipher>,
    source: Option<UdpSource>,
}

impl UdpSender {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            packet_pool: Vec::new(),
            packet_lens: Vec::new(),
            frame_id: 0,
            stats_start: Instant::now(),
            stats_frames: 0,
            stats_bytes: 0,
            shard_pool: Vec::new(),
            rs_cache: HashMap::new(),
            cipher: None,
            source: None,
        }
    }

    pub fn set_cipher(&mut self, cipher: crate::crypto::SessionCipher) {
        self.cipher = Some(cipher);
    }

    pub fn set_source(&mut self, source: Option<UdpSource>) {
        self.source = source;
    }

    pub fn send_frame(
        &mut self,
        au: &EncodedAccessUnit,
        client_addr: SocketAddr,
        timestamp_ms: u32,
        codec_id: u8,
        tuning: StreamTuning,
    ) -> Result<()> {
        let payload = &au.annex_b;
        let is_idr = au.is_idr;
        if std::env::var_os("SCREX_LOG_SENT_AUS").is_some() {
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
            println!(
                "[stream] fps={fps:.1} throughput={mbps:.2}Mbps chunks={data_count}+{actual_parity}fec",
            );
            self.stats_frames = 0;
            self.stats_bytes = 0;
            self.stats_start = Instant::now();
        }

        Ok(())
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

        while sent < total_shards {
            let end = (sent + SENDMMSG_BATCH_SIZE).min(total_shards);
            match self.sendmmsg_batch(client_addr, sent, end) {
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

    fn sendmmsg_batch(
        &self,
        client_addr: SocketAddr,
        start: usize,
        end: usize,
    ) -> std::io::Result<usize> {
        let batch_len = end - start;
        let (mut addr_storage, addr_len) = socket_addr_to_raw(client_addr);
        let mut iovecs = Vec::with_capacity(batch_len);
        let mut msgs = Vec::with_capacity(batch_len);

        // Optional IP_PKTINFO cmsg shared by all messages in the batch
        // (sendmmsg only reads it). Pins the source IP/interface so
        // multi-homed hosts reply from the address the client dialed.
        let mut cmsg_buf = match (self.source, client_addr) {
            (Some(src), SocketAddr::V4(_)) => build_pktinfo_cmsg(src),
            _ => Vec::new(),
        };
        let (cmsg_ptr, cmsg_len) = if cmsg_buf.is_empty() {
            (ptr::null_mut(), 0)
        } else {
            (
                cmsg_buf.as_mut_ptr() as *mut libc::c_void,
                cmsg_buf.len() as _,
            )
        };

        for idx in start..end {
            iovecs.push(libc::iovec {
                iov_base: self.packet_pool[idx].as_ptr() as *mut libc::c_void,
                iov_len: self.packet_lens[idx],
            });
        }

        for i in 0..batch_len {
            msgs.push(libc::mmsghdr {
                msg_hdr: libc::msghdr {
                    msg_name: (&mut addr_storage as *mut libc::sockaddr_storage).cast(),
                    msg_namelen: addr_len,
                    msg_iov: &mut iovecs[i] as *mut libc::iovec,
                    msg_iovlen: 1,
                    msg_control: cmsg_ptr,
                    msg_controllen: cmsg_len,
                    msg_flags: 0,
                },
                msg_len: 0,
            });
        }

        let ret = unsafe {
            libc::sendmmsg(
                self.socket.as_raw_fd(),
                msgs.as_mut_ptr(),
                batch_len as u32,
                0,
            )
        };

        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }
}

fn socket_addr_to_raw(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    match addr {
        SocketAddr::V4(addr) => {
            let sockaddr = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: addr.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_be_bytes(addr.ip().octets()).to_be(),
                },
                sin_zero: [0; 8],
            };
            let mut storage = unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
            unsafe {
                ptr::write(
                    (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>(),
                    sockaddr,
                );
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(addr) => {
            let sockaddr = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: addr.port().to_be(),
                sin6_flowinfo: addr.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: addr.ip().octets(),
                },
                sin6_scope_id: addr.scope_id(),
            };
            let mut storage = unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
            unsafe {
                ptr::write(
                    (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>(),
                    sockaddr,
                );
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
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
}

impl AudioSender {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            frame_id: 0,
            send_buf: vec![0u8; HEADER_LEN + CHUNK_PAYLOAD + crate::crypto::TAG_LEN],
            cipher: None,
            source: None,
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
    }

    pub fn send_audio(
        &mut self,
        pcm: &[u8],
        client_addr: SocketAddr,
        timestamp_ms: u32,
    ) -> Result<()> {
        let data_count = (pcm.len() + CHUNK_PAYLOAD - 1) / CHUNK_PAYLOAD;

        for i in 0..data_count {
            let start = i * CHUNK_PAYLOAD;
            let end = (start + CHUNK_PAYLOAD).min(pcm.len());
            let chunk = &pcm[start..end];
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
            ) {
                eprintln!("[audio] send error: {e}");
                break;
            }
        }

        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }
}
