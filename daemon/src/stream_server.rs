use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
const PENDING_SESSION_TIMEOUT: Duration = Duration::from_secs(3);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT_MAGIC: &[u8] = b"SCREX_HB";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_FEC_SHARDS: usize = 127;
const PACING_THRESHOLD: usize = 20;
const PACING_DELAY: Duration = Duration::from_micros(10);

fn should_log_debug(counter: u64) -> bool {
    counter <= 12 || counter.is_power_of_two() || counter % 25 == 0
}

fn seq_is_stale(seq_num: u32, expected_seq: u32) -> bool {
    let diff = seq_num.wrapping_sub(expected_seq);
    diff > 0x8000_0000
}

pub type LifecycleCallback = Box<dyn Fn() + Send + Sync>;

pub struct SharedState {
    pub client_addr: Mutex<Option<SocketAddr>>,
    pub force_idr: AtomicBool,
    pub force_refresh_handle: Mutex<Option<Arc<AtomicBool>>>,
    pub capture_start: Arc<AtomicBool>,
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
    /// IP expected from the TCP handshake; used to accept the first UDP packet
    pub expected_client_ip: Mutex<Option<std::net::IpAddr>>,
}

impl SharedState {
    pub fn new(camera_exclusive_caps: bool) -> Self {
        Self {
            client_addr: Mutex::new(None),
            force_idr: AtomicBool::new(false),
            force_refresh_handle: Mutex::new(None),
            capture_start: Arc::new(AtomicBool::new(false)),
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
            expected_client_ip: Mutex::new(None),
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
        shared.force_idr.store(true, Ordering::Relaxed);
        return;
    }

    if ctrl.starts_with(SPEAKER_MAGIC) && ctrl.len() == SPEAKER_MAGIC.len() + 1 {
        let enabled = ctrl[SPEAKER_MAGIC.len()] != 0;
        shared.audio_output_enabled.store(enabled, Ordering::SeqCst);
        if enabled {
            println!("[audio] client enabled speaker passthrough");
            ensure_virtual_sink(shared);
        } else {
            println!("[audio] client disabled speaker passthrough");
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
        let mut kb = shared.virtual_keyboard.lock().unwrap();
        if let Some(ref mut keyboard) = *kb {
            crate::uinput::handle_key_packet(keyboard, key_data);
        }
        return;
    }

    if ctrl.starts_with(MOUSE_MAGIC) && ctrl.len() > MOUSE_MAGIC.len() {
        let mouse_data = &ctrl[MOUSE_MAGIC.len()..];
        let mut m = shared.virtual_mouse.lock().unwrap();
        if let Some(ref mut vm) = *m {
            crate::uinput::handle_mouse_packet(vm, mouse_data);
        }
        return;
    }

    if ctrl.starts_with(RAWKEY_MAGIC) && ctrl.len() > RAWKEY_MAGIC.len() {
        let rk_data = &ctrl[RAWKEY_MAGIC.len()..];
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
    if !shared.has_active_client.load(Ordering::Relaxed)
        || !shared.audio_output_enabled.load(Ordering::SeqCst)
    {
        return;
    }

    let current_id = *shared.audio_module_id.lock().unwrap();
    if current_id > 0 {
        return;
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
    shared
        .network_session_pending
        .store(false, Ordering::SeqCst);
    shared.network_session_busy.store(false, Ordering::SeqCst);

    if !shared.usb_active.load(Ordering::Relaxed)
        && shared.has_active_client.swap(false, Ordering::SeqCst)
    {
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
                            let _ = socket.send_to(&hb_buf[..HEADER_LEN + enc_len], addr);
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
                    match socket.send_to(&hb_buf[..HEADER_LEN + enc_len], addr) {
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
    send_buf: Vec<u8>,
    frame_id: u32,
    stats_start: Instant,
    stats_frames: u64,
    stats_bytes: u64,
    shard_pool: Vec<Vec<u8>>,
    cipher: Option<crate::crypto::SessionCipher>,
}

impl UdpSender {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            send_buf: vec![0u8; HEADER_LEN + CHUNK_PAYLOAD + crate::crypto::TAG_LEN],
            frame_id: 0,
            stats_start: Instant::now(),
            stats_frames: 0,
            stats_bytes: 0,
            shard_pool: Vec::new(),
            cipher: None,
        }
    }

    pub fn set_cipher(&mut self, cipher: crate::crypto::SessionCipher) {
        self.cipher = Some(cipher);
    }

    pub fn send_frame(
        &mut self,
        au: &EncodedAccessUnit,
        client_addr: SocketAddr,
        timestamp_ms: u32,
        codec_id: u8,
    ) -> Result<()> {
        let payload = &au.annex_b;
        let is_idr = au.is_idr;

        let data_count = (payload.len() + CHUNK_PAYLOAD - 1) / CHUNK_PAYLOAD;

        let parity_count = if data_count <= 1 {
            0
        } else if data_count > 50 {
            (data_count * 3 / 10).max(1).min(MAX_FEC_SHARDS)
        } else {
            (data_count / 5).max(1).min(MAX_FEC_SHARDS)
        };

        let use_fec = parity_count > 0 && (data_count + parity_count) <= 255;
        let actual_parity = if use_fec { parity_count } else { 0 };
        let total_shards = data_count + actual_parity;

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
            let rs = ReedSolomon::new(data_count, actual_parity)
                .map_err(|e| anyhow::anyhow!("RS init: {e:?}"))?;
            rs.encode(&mut self.shard_pool[..total_shards])
                .map_err(|e| anyhow::anyhow!("RS encode: {e:?}"))?;
        }

        let needs_pacing = total_shards > PACING_THRESHOLD;
        let mut frame_bytes = 0_u64;
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

            self.send_buf[..HEADER_LEN].copy_from_slice(&header);
            self.send_buf[HEADER_LEN..HEADER_LEN + CHUNK_PAYLOAD]
                .copy_from_slice(&self.shard_pool[idx]);

            let pkt_len = if let Some(ref cipher) = self.cipher {
                let nonce = crate::crypto::nonce_server(self.frame_id, idx as u16, flags);
                let enc_len = cipher.encrypt_slice(
                    &nonce,
                    &header,
                    &mut self.send_buf[HEADER_LEN..],
                    CHUNK_PAYLOAD,
                );
                HEADER_LEN + enc_len
            } else {
                HEADER_LEN + CHUNK_PAYLOAD
            };

            if let Err(e) = self.socket.send_to(&self.send_buf[..pkt_len], client_addr) {
                eprintln!("[stream] send error: {e}");
                break;
            }
            frame_bytes += pkt_len as u64;

            if needs_pacing {
                std::thread::sleep(PACING_DELAY);
            }
        }

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
}

// ---------------------------------------------------------------------------
// Audio sender — separate frame_id space, no FEC (audio chunks are small)
// ---------------------------------------------------------------------------

pub struct AudioSender {
    socket: UdpSocket,
    frame_id: u32,
    send_buf: Vec<u8>,
    cipher: Option<crate::crypto::SessionCipher>,
}

impl AudioSender {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            frame_id: 0,
            send_buf: vec![0u8; HEADER_LEN + CHUNK_PAYLOAD + crate::crypto::TAG_LEN],
            cipher: None,
        }
    }

    pub fn set_cipher(&mut self, cipher: crate::crypto::SessionCipher) {
        self.cipher = Some(cipher);
    }

    pub fn clear_cipher(&mut self) {
        self.cipher = None;
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

            if let Err(e) = self.socket.send_to(&self.send_buf[..pkt_len], client_addr) {
                eprintln!("[audio] send error: {e}");
                break;
            }
        }

        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }
}
