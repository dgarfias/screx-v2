use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use reed_solomon_erasure::galois_8::ReedSolomon;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519};
use ring::hkdf;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::audio_player::AudioPlayer;
use crate::decoder::{CodecId, DecodedOutput, FrameBufferPool, VideoDecoder};
use crate::mic_capture::MicCapture;
use crate::video_surface::{DisplayFrame, FrameSlotRef, RawFrame};
use crate::webcam_capture::WebcamCapture;

const DEFAULT_PORT: u16 = 9000;
const CONTROL_MAX_FRAME: usize = 65536;
const UDP_HEADER_LEN: usize = 18;
const UDP_CHUNK_PAYLOAD: usize = 1400;
const UDP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
const UDP_DATA_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_READ_TIMEOUT: Duration = Duration::from_millis(500);
const FRAME_TIMEOUT: Duration = Duration::from_millis(100);
const PLI_MIN_INTERVAL: Duration = Duration::from_secs(1);
/// Message sent from the UDP receiver thread to the decoder thread.
struct DecodeJob {
    annex_b: Vec<u8>,
    frame_id: u32,
    codec_id: u8,
    flags: u8,
    timestamp_ms: u32,
}
const MAGIC_PAIR: &[u8] = b"SCREX_PAIR";
const MAGIC_HELLO: &[u8] = b"SCREX_HELLO";
const MAGIC_PIN: &[u8] = b"SCREX_PIN\0";
const MAGIC_ANSWER: &[u8] = b"SCREX_ANSWER";
const MAGIC_BUSY: &[u8] = b"SCREX_BUSY\0\0";
const MAGIC_OK: &[u8] = b"SCREX_OK\0\0";
const MAGIC_REJECT: &[u8] = b"SCREX_REJECT";
const MAGIC_REGISTER: &[u8] = b"SCREX";
const MAGIC_PLI: &[u8] = b"PLI";
const MAGIC_HOST: &[u8] = b"HOST";
const MAGIC_DISCONNECT: &[u8] = b"DISCONNECT";
const MAGIC_SPEAKER: &[u8] = b"SPKR";
const MAGIC_MICCFG: &[u8] = b"MICCFG";
const MAGIC_CAMCFG: &[u8] = b"CAMCFG";
const MAGIC_PERIPH: &[u8] = b"PERIPH";
const FLAG_IDR: u8 = 0x01;
const FLAG_AUDIO: u8 = 0x02;
const TAG_LEN: usize = 16;

fn should_send_pli(last_pli_at: &mut Option<Instant>) -> bool {
    let allowed = last_pli_at
        .map(|instant| instant.elapsed() >= PLI_MIN_INTERVAL)
        .unwrap_or(true);
    if allowed {
        *last_pli_at = Some(Instant::now());
    }
    allowed
}

#[derive(Clone)]
pub struct BackendHandle {
    tx: Sender<BackendCommand>,
}

impl BackendHandle {
    pub fn connect(&self, host: String, speaker_enabled: bool) {
        let _ = self.tx.send(BackendCommand::Connect {
            host,
            speaker_enabled,
        });
    }

    pub fn submit_pin(&self, pin: String) {
        let _ = self.tx.send(BackendCommand::SubmitPin { pin });
    }

    pub fn disconnect(&self) {
        let _ = self.tx.send(BackendCommand::Disconnect);
    }

    pub fn set_speaker(&self, enabled: bool) {
        let _ = self.tx.send(BackendCommand::SetSpeaker { enabled });
    }

    pub fn set_camera_mode(&self, mode: String) {
        let _ = self.tx.send(BackendCommand::SetCameraMode { mode });
    }

    pub fn set_mic(&self, enabled: bool) {
        let _ = self.tx.send(BackendCommand::SetMic { enabled });
    }

    pub fn set_camera(&self, enabled: bool) {
        let _ = self.tx.send(BackendCommand::SetCamera { enabled });
    }

    pub fn set_keyboard(&self, enabled: bool) {
        let _ = self.tx.send(BackendCommand::SetKeyboard { enabled });
    }

    pub fn load_connections(&self) {
        let _ = self.tx.send(BackendCommand::LoadConnections);
    }

    pub fn toggle_pinned(&self, host: String, port: u16) {
        let _ = self.tx.send(BackendCommand::TogglePinned { host, port });
    }

    pub fn delete_connection(&self, host: String, port: u16) {
        let _ = self
            .tx
            .send(BackendCommand::DeleteConnection { host, port });
    }

    pub fn clear_recent_connections(&self) {
        let _ = self.tx.send(BackendCommand::ClearRecentConnections);
    }

    pub fn send_key_action(&self, qt_key: i32, text: String, modifiers: i32, pressed: bool) {
        let _ = self.tx.send(BackendCommand::SendKeyAction {
            qt_key,
            text,
            modifiers,
            pressed,
        });
    }

    pub fn send_mouse_move(&self, x: u16, y: u16) {
        let _ = self.tx.send(BackendCommand::SendMouseMove { x, y });
    }

    pub fn send_mouse_button(&self, button: u8, pressed: bool) {
        let _ = self
            .tx
            .send(BackendCommand::SendMouseButton { button, pressed });
    }

    pub fn send_mouse_scroll(&self, dy: i16) {
        let _ = self.tx.send(BackendCommand::SendMouseScroll { dy });
    }
}

enum BackendCommand {
    Connect {
        host: String,
        speaker_enabled: bool,
    },
    RetryBlankStream {
        session_id: u64,
    },
    SubmitPin {
        pin: String,
    },
    Disconnect,
    SetSpeaker {
        enabled: bool,
    },
    SetCameraMode {
        mode: String,
    },
    SetMic {
        enabled: bool,
    },
    SetCamera {
        enabled: bool,
    },
    SetKeyboard {
        enabled: bool,
    },
    SendKeyAction {
        qt_key: i32,
        text: String,
        modifiers: i32,
        pressed: bool,
    },
    SendMouseMove {
        x: u16,
        y: u16,
    },
    SendMouseButton {
        button: u8,
        pressed: bool,
    },
    SendMouseScroll {
        dy: i16,
    },
    SessionClosed {
        session_id: u64,
        reason: String,
    },
    LoadConnections,
    TogglePinned {
        host: String,
        port: u16,
    },
    DeleteConnection {
        host: String,
        port: u16,
    },
    ClearRecentConnections,
    UpdateDaemonHostname {
        session_id: u64,
        hostname: String,
    },
}

#[derive(Clone)]
pub enum UiEvent {
    SetConnecting(bool),
    SetConnected(bool),
    SetSessionTitle(String),
    SetStatus(String),
    SetTransportLabel(String),
    SetCodecLabel(String),
    SetResolutionLabel(String),
    SetStats {
        fps: u32,
        bitrate_mbps: f32,
        latency_ms: u32,
        dropped_frames: u32,
    },
    PinRequired(String),
    ClearPinPrompt,
    SetConnections(Vec<RecentConnection>),
    SetCameraEnabled(bool),
    /// Give the UI thread a direct handle to the TCP control sender
    /// so mouse/key events bypass the mpsc channel entirely.
    SetDirectControl(Option<Arc<ControlSender>>),
}

/// Load connections from disk as JSON string for eager UI initialization.
pub fn load_connections_json() -> String {
    match AppStorage::load() {
        Ok(storage) => {
            let conns = storage.get_connections();
            #[derive(serde::Serialize)]
            struct Entry {
                host: String,
                port: u16,
                name: String,
                pinned: bool,
            }
            let entries: Vec<Entry> = conns
                .iter()
                .map(|c| Entry {
                    host: c.host.clone(),
                    port: c.port,
                    name: c.name.clone(),
                    pinned: c.pinned,
                })
                .collect();
            serde_json::to_string(&entries).unwrap_or_else(|_| "[]".into())
        }
        Err(_) => "[]".into(),
    }
}

pub fn spawn_backend<F>(ui: F, frame_slot: FrameSlotRef) -> BackendHandle
where
    F: Fn(UiEvent) + Send + Sync + 'static,
{
    let (tx, rx) = mpsc::channel();
    let ui = Arc::new(ui);
    let worker_tx = tx.clone();

    thread::spawn(move || {
        let mut worker = BackendWorker::new(rx, worker_tx, ui, frame_slot);
        worker.run();
    });

    BackendHandle { tx }
}

struct BackendWorker {
    rx: Receiver<BackendCommand>,
    tx: Sender<BackendCommand>,
    ui: Arc<dyn Fn(UiEvent) + Send + Sync>,
    storage: AppStorage,
    active: Option<ActiveSession>,
    pending_pairing: Option<PendingPairing>,
    next_session_id: u64,
    frame_slot: FrameSlotRef,
    blank_stream_retry_attempted: bool,
}

impl BackendWorker {
    fn new(
        rx: Receiver<BackendCommand>,
        tx: Sender<BackendCommand>,
        ui: Arc<dyn Fn(UiEvent) + Send + Sync>,
        frame_slot: FrameSlotRef,
    ) -> Self {
        Self {
            rx,
            tx,
            ui,
            storage: AppStorage::load().unwrap_or_default(),
            active: None,
            pending_pairing: None,
            next_session_id: 1,
            frame_slot,
            blank_stream_retry_attempted: false,
        }
    }

    fn run(&mut self) {
        while let Ok(command) = self.rx.recv() {
            // Drain and coalesce: if multiple mouse moves queued, only send the latest
            let command = self.coalesce_mouse(command);
            self.dispatch(command);
        }
    }

    /// Drain queued commands and keep only the latest mouse move,
    /// dispatching any non-mouse commands encountered along the way.
    fn coalesce_mouse(&mut self, initial: BackendCommand) -> BackendCommand {
        let mut latest = initial;
        loop {
            match self.rx.try_recv() {
                Ok(BackendCommand::SendMouseMove { x, y }) => {
                    // Replace with newer position, skip the old one
                    latest = BackendCommand::SendMouseMove { x, y };
                }
                Ok(other) => {
                    // Process non-mouse command immediately, keep draining
                    self.dispatch(other);
                }
                Err(_) => break,
            }
        }
        latest
    }

    fn dispatch(&mut self, command: BackendCommand) {
        match command {
            BackendCommand::Connect {
                host,
                speaker_enabled,
            } => {
                self.blank_stream_retry_attempted = false;
                self.handle_connect(host, speaker_enabled)
            }
            BackendCommand::RetryBlankStream { session_id } => {
                self.handle_retry_blank_stream(session_id)
            }
            BackendCommand::SubmitPin { pin } => self.handle_submit_pin(pin),
            BackendCommand::Disconnect => self.handle_disconnect(false),
            BackendCommand::SetSpeaker { enabled } => self.handle_set_speaker(enabled),
            BackendCommand::SetCameraMode { mode } => self.handle_set_camera_mode(mode),
            BackendCommand::SetMic { enabled } => self.handle_set_mic(enabled),
            BackendCommand::SetCamera { enabled } => self.handle_set_camera(enabled),
            BackendCommand::SetKeyboard { enabled } => {
                // Keyboard is always-on when connected; toggle is cosmetic state.
                (self.ui)(UiEvent::SetStatus(format!(
                    "Keyboard forwarding {}.",
                    if enabled { "active" } else { "paused" }
                )));
            }
            BackendCommand::SendKeyAction {
                qt_key,
                text,
                modifiers,
                pressed,
            } => {
                if let Some(ref active) = self.active {
                    let _ = crate::input::send_qt_key_action(
                        &active.control,
                        qt_key,
                        &text,
                        modifiers,
                        pressed,
                    );
                }
            }
            BackendCommand::SendMouseMove { x, y } => {
                if let Some(ref active) = self.active {
                    let _ = crate::input::send_mouse_abs(&active.control, x, y);
                }
            }
            BackendCommand::SendMouseButton { button, pressed } => {
                if let Some(ref active) = self.active {
                    let _ = crate::input::send_mouse_button(&active.control, button, pressed);
                }
            }
            BackendCommand::SendMouseScroll { dy } => {
                if let Some(ref active) = self.active {
                    let _ = crate::input::send_mouse_scroll(&active.control, dy);
                }
            }
            BackendCommand::SessionClosed { session_id, reason } => {
                if self
                    .active
                    .as_ref()
                    .map(|session| session.session_id == session_id)
                    .unwrap_or(false)
                {
                    self.active = None;
                    self.pending_pairing = None;
                    (self.ui)(UiEvent::SetConnecting(false));
                    (self.ui)(UiEvent::ClearPinPrompt);
                    (self.ui)(UiEvent::SetDirectControl(None));
                    (self.ui)(UiEvent::SetConnected(false));
                    (self.ui)(UiEvent::SetSessionTitle("No active session".into()));
                    (self.ui)(UiEvent::SetStatus(reason));
                    (self.ui)(UiEvent::SetCodecLabel("Waiting for stream".into()));
                    (self.ui)(UiEvent::SetResolutionLabel("Pending".into()));
                    (self.ui)(UiEvent::SetStats {
                        fps: 0,
                        bitrate_mbps: 0.0,
                        latency_ms: 0,
                        dropped_frames: 0,
                    });
                }
            }
            BackendCommand::LoadConnections => {
                self.push_connections_to_ui();
            }
            BackendCommand::TogglePinned { host, port } => {
                self.storage.toggle_pinned(&host, port);
                self.push_connections_to_ui();
            }
            BackendCommand::DeleteConnection { host, port } => {
                self.storage.delete_connection(&host, port);
                self.push_connections_to_ui();
            }
            BackendCommand::ClearRecentConnections => {
                self.storage.clear_recent_connections();
                self.push_connections_to_ui();
            }
            BackendCommand::UpdateDaemonHostname {
                session_id,
                hostname,
            } => {
                if let Some(ref active) = self.active {
                    if active.session_id == session_id {
                        let port = active.server_port;
                        let host = &active.reconnect_host;
                        self.storage.update_connection_name(host, port, &hostname);
                        self.push_connections_to_ui();
                    }
                }
            }
        }
    }

    fn handle_connect(&mut self, host_input: String, speaker_enabled: bool) {
        (self.ui)(UiEvent::ClearPinPrompt);
        (self.ui)(UiEvent::SetConnecting(true));
        (self.ui)(UiEvent::SetDirectControl(None));
        (self.ui)(UiEvent::SetConnected(false));
        (self.ui)(UiEvent::SetStatus(format!("Connecting to {host_input}...")));

        match establish_session(&mut self.storage, &host_input) {
            Ok(ConnectResult::Established(bootstrap)) => {
                self.activate_session(bootstrap, host_input, speaker_enabled);
            }
            Ok(ConnectResult::PinRequired(pending)) => {
                self.pending_pairing = Some(pending);
                (self.ui)(UiEvent::SetConnecting(false));
                (self.ui)(UiEvent::PinRequired(
                    "Enter the 6-digit PIN shown in the daemon terminal.".into(),
                ));
                (self.ui)(UiEvent::SetStatus(
                    "Pairing requested. Enter the PIN from the daemon.".into(),
                ));
            }
            Err(error) => {
                (self.ui)(UiEvent::SetConnecting(false));
                (self.ui)(UiEvent::SetStatus(format!("Connection failed: {error:#}")));
            }
        }
    }

    fn handle_submit_pin(&mut self, pin: String) {
        let Some(pending) = self.pending_pairing.take() else {
            (self.ui)(UiEvent::SetStatus(
                "No pairing request is waiting for a PIN.".into(),
            ));
            return;
        };

        let pin = pin.trim().to_owned();
        if pin.len() != 6 || !pin.as_bytes().iter().all(|b| b.is_ascii_digit()) {
            self.pending_pairing = Some(pending);
            (self.ui)(UiEvent::SetStatus("PIN must be exactly 6 digits.".into()));
            return;
        }

        (self.ui)(UiEvent::SetConnecting(true));
        (self.ui)(UiEvent::SetStatus("Finishing pairing handshake...".into()));

        match complete_pairing(&mut self.storage, pending, &pin) {
            Ok(bootstrap) => {
                // Speaker is enabled by default on fresh connections.
                let reconnect_host = bootstrap.display_host.clone();
                self.activate_session(bootstrap, reconnect_host, true);
            }
            Err(error) => {
                (self.ui)(UiEvent::SetConnecting(false));
                (self.ui)(UiEvent::PinRequired(
                    "PIN rejected or pairing failed. Try again with the current daemon PIN.".into(),
                ));
                (self.ui)(UiEvent::SetStatus(format!("Pairing failed: {error:#}")));
            }
        }
    }

    fn activate_session(
        &mut self,
        bootstrap: SessionBootstrap,
        reconnect_host: String,
        speaker_enabled: bool,
    ) {
        let session_id = self.next_session_id;
        self.next_session_id = self.next_session_id.wrapping_add(1);

        let stop = Arc::new(AtomicBool::new(false));
        let session_key = bootstrap.session_key;
        let control_stream = match bootstrap
            .control_stream
            .try_clone()
            .context("clone tcp control stream")
        {
            Ok(stream) => stream,
            Err(error) => {
                (self.ui)(UiEvent::SetConnecting(false));
                (self.ui)(UiEvent::SetStatus(format!(
                    "Control setup failed: {error:#}"
                )));
                return;
            }
        };

        let control = match ControlSender::new(control_stream, session_key) {
            Ok(sender) => Arc::new(sender),
            Err(error) => {
                (self.ui)(UiEvent::SetConnecting(false));
                (self.ui)(UiEvent::SetStatus(format!(
                    "Control setup failed: {error:#}"
                )));
                return;
            }
        };

        let udp_sender = match UdpSender::new(bootstrap.server_addr, session_key) {
            Ok(sender) => Arc::new(sender),
            Err(error) => {
                (self.ui)(UiEvent::SetConnecting(false));
                (self.ui)(UiEvent::SetStatus(format!("UDP setup failed: {error:#}")));
                return;
            }
        };

        let title = bootstrap.display_host.clone();
        (self.ui)(UiEvent::ClearPinPrompt);
        (self.ui)(UiEvent::SetSessionTitle(title.clone()));

        // Remember this connection
        let port = bootstrap.server_addr.port();
        self.storage
            .remember_connection(&title, &bootstrap.display_host, port);
        self.push_connections_to_ui();
        (self.ui)(UiEvent::SetTransportLabel("Network".into()));
        (self.ui)(UiEvent::SetCodecLabel("Negotiating stream".into()));
        (self.ui)(UiEvent::SetResolutionLabel("Receiving stream".into()));
        (self.ui)(UiEvent::SetConnecting(false));
        (self.ui)(UiEvent::SetDirectControl(Some(Arc::clone(&control))));
        (self.ui)(UiEvent::SetConnected(true));
        (self.ui)(UiEvent::SetStatus(format!(
            "Session established with {title}. Waiting for UDP media..."
        )));

        let _ = control.send_speaker_state(speaker_enabled);
        let _ = control.send_peripheral_state(PERIPH_MOUSE, true);
        let _ = control.send_peripheral_state(PERIPH_KEYBOARD, true);

        // Create shared AV sync state — passed to both audio player and UDP runtime.
        let av_sync = Arc::new(AvSyncState::new());

        // Start audio playback before spawning UDP so the player is ready for PCM.
        let audio_player: Option<Arc<AudioPlayer>> = if speaker_enabled {
            match AudioPlayer::start(Arc::clone(&av_sync)) {
                Ok(player) => Some(Arc::new(player)),
                Err(e) => {
                    (self.ui)(UiEvent::SetStatus(format!(
                        "Audio playback init failed: {e:#}"
                    )));
                    None
                }
            }
        } else {
            None
        };

        spawn_control_receiver(
            session_id,
            bootstrap.control_stream,
            session_key,
            Arc::clone(&self.ui),
            self.tx.clone(),
            Arc::clone(&stop),
        );
        spawn_udp_runtime(
            session_id,
            Arc::clone(&udp_sender),
            Arc::clone(&control),
            Arc::clone(&self.ui),
            self.tx.clone(),
            Arc::clone(&stop),
            Arc::clone(&self.frame_slot),
            audio_player.as_ref().map(Arc::clone),
            Arc::clone(&av_sync),
        );

        self.active = Some(ActiveSession {
            session_id,
            control,
            udp: udp_sender,
            stop,
            audio_player,
            av_sync,
            mic_capture: None,
            webcam_capture: None,
            reconnect_host,
            server_port: bootstrap.server_addr.port(),
            speaker_enabled,
        });
    }

    fn handle_retry_blank_stream(&mut self, session_id: u64) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if active.session_id != session_id || self.blank_stream_retry_attempted {
            return;
        }

        self.blank_stream_retry_attempted = true;
        let host = active.reconnect_host.clone();
        let speaker_enabled = active.speaker_enabled;
        (self.ui)(UiEvent::SetStatus(
            "The initial desktop video looked blank. Retrying the session once.".into(),
        ));
        self.handle_connect(host, speaker_enabled);
    }

    fn push_connections_to_ui(&self) {
        let conns = self.storage.get_connections().to_vec();
        (self.ui)(UiEvent::SetConnections(conns));
    }

    fn handle_disconnect(&mut self, quiet: bool) {
        self.pending_pairing = None;
        if let Some(active) = self.active.take() {
            // Tell the daemon to tear down virtual devices if active
            let _ = active.control.send_peripheral_state(PERIPH_MOUSE, false);
            let _ = active.control.send_peripheral_state(PERIPH_KEYBOARD, false);
            if active.webcam_capture.is_some() {
                let _ = active.control.send_camera_disable();
            }
            if active.mic_capture.is_some() {
                let _ = active.control.send_mic_state(false);
            }
            if active.audio_player.is_some() {
                let _ = active.control.send_speaker_state(false);
            }
            active.stop.store(true, Ordering::SeqCst);
            let _ = active.control.disconnect_gracefully();
            if !quiet {
                (self.ui)(UiEvent::SetConnecting(false));
                (self.ui)(UiEvent::ClearPinPrompt);
                (self.ui)(UiEvent::SetDirectControl(None));
                (self.ui)(UiEvent::SetConnected(false));
                (self.ui)(UiEvent::SetSessionTitle("No active session".into()));
                (self.ui)(UiEvent::SetStatus(
                    "Disconnected. Transport/core stays active for the next session.".into(),
                ));
                (self.ui)(UiEvent::SetCodecLabel("Waiting for stream".into()));
                (self.ui)(UiEvent::SetResolutionLabel("Pending".into()));
                (self.ui)(UiEvent::SetStats {
                    fps: 0,
                    bitrate_mbps: 0.0,
                    latency_ms: 0,
                    dropped_frames: 0,
                });
            }
        } else if !quiet {
            (self.ui)(UiEvent::SetConnecting(false));
            (self.ui)(UiEvent::ClearPinPrompt);
            (self.ui)(UiEvent::SetDirectControl(None));
            (self.ui)(UiEvent::SetConnected(false));
            (self.ui)(UiEvent::SetStatus("No active session.".into()));
        }
    }

    fn handle_set_speaker(&mut self, enabled: bool) {
        if let Some(active) = &mut self.active {
            if let Err(error) = active.control.send_speaker_state(enabled) {
                (self.ui)(UiEvent::SetStatus(format!(
                    "Speaker update failed: {error:#}"
                )));
                return;
            }
            if enabled {
                if active.audio_player.is_none() {
                    match AudioPlayer::start(Arc::clone(&active.av_sync)) {
                        Ok(player) => {
                            active.audio_player = Some(Arc::new(player));
                            (self.ui)(UiEvent::SetStatus("Speaker enabled.".into()));
                        }
                        Err(e) => {
                            (self.ui)(UiEvent::SetStatus(format!(
                                "Audio playback init failed: {e:#}"
                            )));
                        }
                    }
                }
            } else {
                active.audio_player = None; // Drop stops playback
                (self.ui)(UiEvent::SetStatus("Speaker disabled.".into()));
            }
        }
    }

    fn handle_set_camera_mode(&mut self, mode: String) {
        if let Some(active) = &self.active {
            match parse_camera_mode(&mode) {
                Some(config) => {
                    // Only send CAMCFG if camera is currently active.
                    // If camera is off, the new mode will be used when camera is next enabled.
                    if active.webcam_capture.is_some() {
                        if let Err(error) = active.control.send_camera_config(config) {
                            (self.ui)(UiEvent::SetStatus(format!(
                                "Camera mode update failed: {error:#}"
                            )));
                        } else {
                            (self.ui)(UiEvent::SetStatus(format!(
                                "Requested webcam mode {}x{} @ {}fps.",
                                config.width, config.height, config.fps
                            )));
                        }
                    } else {
                        (self.ui)(UiEvent::SetStatus(format!(
                            "Camera mode set to {}x{} @ {}fps (will apply when camera is enabled).",
                            config.width, config.height, config.fps
                        )));
                    }
                }
                None => {
                    (self.ui)(UiEvent::SetStatus(
                        "Could not parse the selected camera mode.".into(),
                    ));
                }
            }
        }
    }

    fn handle_set_mic(&mut self, enabled: bool) {
        if let Some(active) = &mut self.active {
            if enabled {
                if active.mic_capture.is_none() {
                    // Tell the daemon to create the virtual microphone
                    if let Err(e) = active.control.send_mic_state(true) {
                        (self.ui)(UiEvent::SetStatus(format!(
                            "Failed to signal mic enable: {e:#}"
                        )));
                    }
                    match MicCapture::start(Arc::clone(&active.udp)) {
                        Ok(mic) => {
                            active.mic_capture = Some(mic);
                            (self.ui)(UiEvent::SetStatus("Mic capture started.".into()));
                        }
                        Err(e) => {
                            // Mic open failed — tell daemon to tear down
                            let _ = active.control.send_mic_state(false);
                            (self.ui)(UiEvent::SetStatus(format!("Mic start failed: {e:#}")));
                        }
                    }
                }
            } else {
                active.mic_capture = None; // Drop stops the capture
                                           // Tell the daemon to destroy the virtual microphone
                let _ = active.control.send_mic_state(false);
                (self.ui)(UiEvent::SetStatus("Mic capture stopped.".into()));
            }
        }
    }

    fn handle_set_camera(&mut self, enabled: bool) {
        if let Some(active) = &mut self.active {
            if enabled {
                if active.webcam_capture.is_none() {
                    // Parse the current camera mode for resolution/fps.
                    // We'll use 1280x720@30 as a safe default if parsing fails.
                    let config = CameraConfig {
                        width: 1280,
                        height: 720,
                        fps: 30,
                    };
                    // Tell the daemon to create the virtual webcam with our config
                    if let Err(e) = active.control.send_camera_config(config) {
                        (self.ui)(UiEvent::SetStatus(format!(
                            "Failed to signal camera config: {e:#}"
                        )));
                    }
                    match WebcamCapture::start(
                        Arc::clone(&active.udp),
                        config.width,
                        config.height,
                        config.fps,
                    ) {
                        Ok(cam) => {
                            active.webcam_capture = Some(cam);
                            (self.ui)(UiEvent::SetStatus("Webcam capture started.".into()));
                        }
                        Err(e) => {
                            // Camera open failed — tell daemon to tear down
                            let _ = active.control.send_camera_disable();
                            (self.ui)(UiEvent::SetCameraEnabled(false));
                            (self.ui)(UiEvent::SetStatus(format!("Webcam start failed: {e:#}")));
                        }
                    }
                }
            } else {
                active.webcam_capture = None; // Drop stops capture
                                              // Tell the daemon to destroy the virtual webcam
                let _ = active.control.send_camera_disable();
                (self.ui)(UiEvent::SetStatus("Webcam capture stopped.".into()));
            }
        }
    }
}

struct ActiveSession {
    session_id: u64,
    control: Arc<ControlSender>,
    udp: Arc<UdpSender>,
    stop: Arc<AtomicBool>,
    audio_player: Option<Arc<AudioPlayer>>,
    av_sync: Arc<AvSyncState>,
    mic_capture: Option<MicCapture>,
    webcam_capture: Option<WebcamCapture>,
    reconnect_host: String,
    server_port: u16,
    speaker_enabled: bool,
}

const PERIPH_MOUSE: u8 = 0x01;
const PERIPH_KEYBOARD: u8 = 0x02;
const PERIPH_ATTACHED: u8 = 0x01;
const PERIPH_DETACHED: u8 = 0x00;

struct PendingPairing {
    tcp: TcpStream,
    server_addr: SocketAddr,
    endpoint_key: String,
    display_host: String,
    ecdh_secret: Vec<u8>,
}

struct SessionBootstrap {
    control_stream: TcpStream,
    server_addr: SocketAddr,
    display_host: String,
    session_key: [u8; 32],
}

enum ConnectResult {
    Established(SessionBootstrap),
    PinRequired(PendingPairing),
}

fn establish_session(storage: &mut AppStorage, host_input: &str) -> Result<ConnectResult> {
    let endpoint = resolve_endpoint(host_input)?;
    let device_id = storage.get_or_create_device_id()?;

    if let Some(pairing_key) = storage.get_pairing_key(&endpoint.endpoint_key) {
        hello_flow(endpoint, device_id, pairing_key)
    } else {
        pair_flow(endpoint, device_id)
    }
}

fn pair_flow(endpoint: EndpointInfo, device_id: [u8; 16]) -> Result<ConnectResult> {
    let rng = SystemRandom::new();
    let mut tcp = TcpStream::connect_timeout(&endpoint.server_addr, Duration::from_secs(5))
        .with_context(|| format!("connect {}", endpoint.server_addr))?;
    tcp.set_nodelay(true).ok();

    let private = EphemeralPrivateKey::generate(&X25519, &rng)
        .map_err(|_| anyhow!("failed to generate X25519 keypair"))?;
    let public = private
        .compute_public_key()
        .map_err(|_| anyhow!("failed to compute X25519 public key"))?;

    let mut packet = Vec::with_capacity(MAGIC_PAIR.len() + device_id.len() + public.as_ref().len());
    packet.extend_from_slice(MAGIC_PAIR);
    packet.extend_from_slice(&device_id);
    packet.extend_from_slice(public.as_ref());
    tcp.write_all(&packet).context("send pair request")?;
    tcp.flush().ok();

    let (kind, body) = read_handshake_response(&mut tcp)?;
    match kind {
        HandshakeKind::Busy => bail!("daemon is busy with another client"),
        HandshakeKind::Reject => bail!("daemon rejected the pairing request"),
        HandshakeKind::Pin => {
            if body.len() != 32 {
                bail!("invalid PIN challenge payload");
            }
            let server_pub = UnparsedPublicKey::new(&X25519, &body);
            let ecdh_secret =
                agreement::agree_ephemeral(private, &server_pub, |shared| shared.to_vec())
                    .map_err(|_| anyhow!("ECDH failed during pairing"))?;
            Ok(ConnectResult::PinRequired(PendingPairing {
                tcp,
                server_addr: endpoint.server_addr,
                endpoint_key: endpoint.endpoint_key,
                display_host: endpoint.display_host,
                ecdh_secret,
            }))
        }
        HandshakeKind::Ok => {
            if body.len() != 64 {
                bail!("invalid reconnect payload from pair flow");
            }
            bail!("daemon recognized this device as already paired, but no local pairing key was available")
        }
    }
}

fn complete_pairing(
    storage: &mut AppStorage,
    pending: PendingPairing,
    pin: &str,
) -> Result<SessionBootstrap> {
    let rng = SystemRandom::new();
    let mut tcp = pending.tcp;

    let pin_key = hkdf_sha256(&pending.ecdh_secret, b"screx-pin-exchange", b"pin-encrypt");
    let cipher = SessionCipher::new(&pin_key)?;

    let mut nonce = [0u8; 12];
    rng.fill(&mut nonce)
        .map_err(|_| anyhow!("failed to generate PIN nonce"))?;
    let encrypted = cipher.encrypt_vec(&nonce, b"screx-pin-verify", pin.as_bytes())?;

    let mut packet = Vec::with_capacity(MAGIC_ANSWER.len() + nonce.len() + encrypted.len());
    packet.extend_from_slice(MAGIC_ANSWER);
    packet.extend_from_slice(&nonce);
    packet.extend_from_slice(&encrypted);
    tcp.write_all(&packet).context("send pin answer")?;
    tcp.flush().ok();

    let (kind, body) = read_handshake_response(&mut tcp)?;
    match kind {
        HandshakeKind::Reject => bail!("wrong PIN"),
        HandshakeKind::Busy => bail!("daemon became busy during pairing"),
        HandshakeKind::Pin => bail!("daemon requested another PIN unexpectedly"),
        HandshakeKind::Ok => {
            if body.len() != 64 {
                bail!("invalid final pairing response");
            }
            let session_salt = &body[..32];
            let server_hmac = &body[32..64];

            let mut ikm = pending.ecdh_secret.clone();
            ikm.extend_from_slice(pin.as_bytes());
            let pairing_key = hkdf_sha256(&ikm, b"screx-pairing-salt", b"screx-pairing");
            storage.set_pairing_key(&pending.endpoint_key, pairing_key)?;
            let session_key = hkdf_sha256(&pairing_key, session_salt, b"screx-session");

            let expected = hmac_sha256(&session_key, b"server-verify");
            if expected.as_slice() != server_hmac {
                bail!("server verification failed after pairing");
            }

            Ok(SessionBootstrap {
                control_stream: tcp,
                server_addr: pending.server_addr,
                display_host: pending.display_host,
                session_key,
            })
        }
    }
}

fn hello_flow(
    endpoint: EndpointInfo,
    device_id: [u8; 16],
    pairing_key: [u8; 32],
) -> Result<ConnectResult> {
    let rng = SystemRandom::new();
    let mut tcp = TcpStream::connect_timeout(&endpoint.server_addr, Duration::from_secs(5))
        .with_context(|| format!("connect {}", endpoint.server_addr))?;
    tcp.set_nodelay(true).ok();

    let mut client_nonce = [0u8; 32];
    rng.fill(&mut client_nonce)
        .map_err(|_| anyhow!("failed to generate reconnect nonce"))?;

    let mut packet = Vec::with_capacity(MAGIC_HELLO.len() + device_id.len() + client_nonce.len());
    packet.extend_from_slice(MAGIC_HELLO);
    packet.extend_from_slice(&device_id);
    packet.extend_from_slice(&client_nonce);
    tcp.write_all(&packet).context("send hello request")?;
    tcp.flush().ok();

    let (kind, body) = read_handshake_response(&mut tcp)?;
    match kind {
        HandshakeKind::Busy => bail!("daemon is busy with another client"),
        HandshakeKind::Reject => bail!("daemon no longer recognizes this device; pair again"),
        HandshakeKind::Pin => bail!("daemon requested PIN during HELLO unexpectedly"),
        HandshakeKind::Ok => {
            if body.len() != 64 {
                bail!("invalid HELLO response payload");
            }
            let server_nonce = &body[..32];
            let server_hmac = &body[32..64];
            let mut salt = Vec::with_capacity(64);
            salt.extend_from_slice(&client_nonce);
            salt.extend_from_slice(server_nonce);
            let session_key = hkdf_sha256(&pairing_key, &salt, b"screx-session");

            let expected = hmac_sha256(&session_key, b"server-verify");
            if expected.as_slice() != server_hmac {
                bail!("server verification failed");
            }

            Ok(ConnectResult::Established(SessionBootstrap {
                control_stream: tcp,
                server_addr: endpoint.server_addr,
                display_host: endpoint.display_host,
                session_key,
            }))
        }
    }
}

enum HandshakeKind {
    Pin,
    Ok,
    Busy,
    Reject,
}

fn read_handshake_response(stream: &mut TcpStream) -> Result<(HandshakeKind, Vec<u8>)> {
    let mut header = [0u8; 12];
    stream
        .read_exact(&mut header)
        .context("read handshake header")?;

    if header == MAGIC_BUSY {
        return Ok((HandshakeKind::Busy, Vec::new()));
    }
    if header == MAGIC_REJECT {
        return Ok((HandshakeKind::Reject, Vec::new()));
    }

    if header[..10] == *MAGIC_PIN {
        let mut body = vec![0u8; 32];
        body[..2].copy_from_slice(&header[10..12]);
        stream
            .read_exact(&mut body[2..])
            .context("read PIN handshake body")?;
        return Ok((HandshakeKind::Pin, body));
    }

    if header[..10] == *MAGIC_OK {
        let mut body = vec![0u8; 64];
        body[..2].copy_from_slice(&header[10..12]);
        stream
            .read_exact(&mut body[2..])
            .context("read OK handshake body")?;
        return Ok((HandshakeKind::Ok, body));
    }

    bail!("unexpected handshake response")
}

fn spawn_control_receiver(
    session_id: u64,
    mut stream: TcpStream,
    session_key: [u8; 32],
    ui: Arc<dyn Fn(UiEvent) + Send + Sync>,
    tx: Sender<BackendCommand>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let cipher = match SessionCipher::new(&session_key) {
            Ok(cipher) => cipher,
            Err(error) => {
                let _ = tx.send(BackendCommand::SessionClosed {
                    session_id,
                    reason: format!("Control cipher setup failed: {error:#}"),
                });
                return;
            }
        };

        stream.set_read_timeout(Some(CONTROL_READ_TIMEOUT)).ok();
        let mut len_buf = [0u8; 4];
        let mut frame = vec![0u8; 256];

        while !stop.load(Ordering::Relaxed) {
            match stream.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(_) => {
                    let _ = tx.send(BackendCommand::SessionClosed {
                        session_id,
                        reason: "TCP control channel closed.".into(),
                    });
                    return;
                }
            }

            let body_len = u32::from_be_bytes(len_buf) as usize;
            if body_len < 4 + TAG_LEN || body_len > CONTROL_MAX_FRAME {
                let _ = tx.send(BackendCommand::SessionClosed {
                    session_id,
                    reason: "TCP control channel sent an invalid frame.".into(),
                });
                return;
            }

            if frame.len() < body_len {
                frame.resize(body_len, 0);
            }
            if let Err(_) = stream.read_exact(&mut frame[..body_len]) {
                let _ = tx.send(BackendCommand::SessionClosed {
                    session_id,
                    reason: "TCP control channel ended while reading payload.".into(),
                });
                return;
            }

            let seq = [frame[0], frame[1], frame[2], frame[3]];
            let nonce = nonce_control_server(u32::from_be_bytes(seq));
            let Some(payload) = cipher.decrypt(&nonce, &seq, &mut frame[4..body_len]) else {
                continue;
            };

            if payload.starts_with(MAGIC_HOST) {
                let hostname = String::from_utf8_lossy(&payload[MAGIC_HOST.len()..])
                    .trim()
                    .to_string();
                if !hostname.is_empty() {
                    ui(UiEvent::SetSessionTitle(hostname.clone()));
                    let _ = tx.send(BackendCommand::UpdateDaemonHostname {
                        session_id,
                        hostname,
                    });
                }
            }
        }
    });
}

fn spawn_udp_runtime(
    session_id: u64,
    udp: Arc<UdpSender>,
    control: Arc<ControlSender>,
    ui: Arc<dyn Fn(UiEvent) + Send + Sync>,
    tx: Sender<BackendCommand>,
    stop: Arc<AtomicBool>,
    frame_slot: FrameSlotRef,
    audio_player: Option<Arc<AudioPlayer>>,
    av_sync: Arc<AvSyncState>,
) {
    // Keepalive thread — unchanged
    let keepalive_udp = Arc::clone(&udp);
    let keepalive_stop = Arc::clone(&stop);
    thread::spawn(move || {
        let _ = keepalive_udp.send_encrypted(MAGIC_REGISTER);
        while !keepalive_stop.load(Ordering::Relaxed) {
            thread::sleep(UDP_KEEPALIVE_INTERVAL);
            if keepalive_stop.load(Ordering::Relaxed) {
                break;
            }
            let _ = keepalive_udp.send_encrypted(MAGIC_REGISTER);
        }
    });

    // Bounded decode channel: UDP thread → decoder thread.
    // Capacity of 4 gives enough buffering without unbounded growth.
    let (decode_tx, decode_rx) = mpsc::channel::<DecodeJob>();

    // --- Decoder thread ---
    let decode_stop = Arc::clone(&stop);
    let _decode_ui = Arc::clone(&ui);
    let decode_tx_cmd = tx.clone();
    let decode_control = Arc::clone(&control);
    thread::spawn(move || {
        let mut decoder: Option<VideoDecoder> = None;
        let mut current_codec_id: Option<u8> = None;
        let mut pool = FrameBufferPool::new();
        let mut decoded_frame_count: u64 = 0;
        let mut mostly_black_frame_count: u32 = 0;
        let mut blank_stream_retry_queued = false;
        let mut last_pli_at: Option<Instant> = None;

        let mut debug_dump_au_remaining = std::env::var("SCREX_DUMP_ACCESS_UNITS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let mut debug_dump_remaining = std::env::var("SCREX_DUMP_DECODED_FRAMES")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);

        while !decode_stop.load(Ordering::Relaxed) {
            let job = match decode_rx.recv_timeout(Duration::from_millis(250)) {
                Ok(job) => job,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };

            if debug_dump_au_remaining > 0 {
                dump_access_unit(
                    job.frame_id,
                    job.codec_id,
                    (job.flags & FLAG_IDR) != 0,
                    &job.annex_b,
                );
                debug_dump_au_remaining -= 1;
            }

            // Ensure decoder matches codec
            let need_new_decoder = current_codec_id.map_or(true, |c| c != job.codec_id);
            if need_new_decoder {
                let codec = CodecId::from_transport_id(job.codec_id);
                match VideoDecoder::new(codec) {
                    Ok(dec) => {
                        decoder = Some(dec);
                        current_codec_id = Some(job.codec_id);
                    }
                    Err(e) => {
                        eprintln!("[decoder-thread] decoder init failed: {e:#}");
                        decoder = None;
                        current_codec_id = None;
                    }
                }
            }

            if let Some(dec) = &mut decoder {
                let t0 = Instant::now();
                match dec.decode(&job.annex_b, &mut pool) {
                    Ok(decoded_frames) => {
                        let decode_us = t0.elapsed().as_micros();
                        for output in decoded_frames {
                            decoded_frame_count = decoded_frame_count.wrapping_add(1);
                            let au_len = job.annex_b.len();

                            match output {
                                DecodedOutput::HwFrame(hw) => {
                                    let w = hw.width;
                                    let h = hw.height;

                                    // Drop the old frame without recycling (it's a HwFrame, no RGBA buf)
                                    let _ = frame_slot.take_latest();

                                    let t1 = Instant::now();
                                    frame_slot.publish(Arc::new(DisplayFrame::Hw(hw)));
                                    crate::video_surface::request_video_surface_update();
                                    let present_us = t1.elapsed().as_micros();
                                    if decoded_frame_count == 1 || decoded_frame_count % 60 == 0 {
                                        println!(
                                            "[decoder-thread] frame={} decode={}us present={}us size={}x{} au={}B path=zero-copy",
                                            decoded_frame_count, decode_us, present_us, w, h, au_len,
                                        );
                                    }
                                }
                                DecodedOutput::Rgba(df) => {
                                    let w = df.width;
                                    let h = df.height;

                                    // Black-frame detection for first 90 frames (sampled)
                                    if decoded_frame_count <= 90 {
                                        let pct = non_black_pixel_percent_sampled(&df.rgba);
                                        if pct < 0.5 {
                                            mostly_black_frame_count =
                                                mostly_black_frame_count.saturating_add(1);
                                            if mostly_black_frame_count >= 30
                                                && !blank_stream_retry_queued
                                            {
                                                blank_stream_retry_queued = true;
                                                let _ = decode_tx_cmd.send(
                                                    BackendCommand::RetryBlankStream { session_id },
                                                );
                                            }
                                        } else {
                                            mostly_black_frame_count = 0;
                                        }
                                    }
                                    if debug_dump_remaining > 0 {
                                        dump_decoded_frame_ppm(
                                            decoded_frame_count,
                                            df.width,
                                            df.height,
                                            &df.rgba,
                                        );
                                        debug_dump_remaining -= 1;
                                    }

                                    // Recycle the previous frame's RGBA buffer
                                    if let Some(old_frame) = frame_slot.take_latest() {
                                        if let Ok(old_display) = Arc::try_unwrap(old_frame) {
                                            if let DisplayFrame::Rgba(old_raw) = old_display {
                                                pool.recycle(old_raw.rgba);
                                            }
                                        }
                                    }

                                    let t1 = Instant::now();
                                    frame_slot.publish(Arc::new(DisplayFrame::Rgba(RawFrame {
                                        width: df.width,
                                        height: df.height,
                                        rgba: df.rgba,
                                    })));
                                    crate::video_surface::request_video_surface_update();
                                    let present_us = t1.elapsed().as_micros();
                                    if decoded_frame_count == 1 || decoded_frame_count % 60 == 0 {
                                        println!(
                                            "[decoder-thread] frame={} decode={}us present={}us size={}x{} au={}B path=readback",
                                            decoded_frame_count, decode_us, present_us, w, h, au_len,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[decoder-thread] decode error: {e:#}");
                        if should_send_pli(&mut last_pli_at) {
                            let _ = decode_control.send_pli();
                        }
                    }
                }
            }
        }
    });

    // --- UDP receiver thread ---
    thread::spawn(move || {
        let receiver = match udp.socket.try_clone() {
            Ok(socket) => socket,
            Err(error) => {
                let _ = tx.send(BackendCommand::SessionClosed {
                    session_id,
                    reason: format!("UDP clone failed: {error}"),
                });
                return;
            }
        };
        receiver.set_nonblocking(true).ok();

        let cipher = match SessionCipher::new(&udp.session_key) {
            Ok(cipher) => cipher,
            Err(error) => {
                let _ = tx.send(BackendCommand::SessionClosed {
                    session_id,
                    reason: format!("UDP cipher setup failed: {error:#}"),
                });
                return;
            }
        };

        // Pre-allocate a batch of packet buffers.  We drain the kernel
        // socket buffer as fast as possible into these, then process them
        // in a second pass so that the socket never stalls waiting on our
        // per-packet bookkeeping.
        const BATCH_SIZE: usize = 256;
        const PKT_BUF_SIZE: usize = UDP_HEADER_LEN + UDP_CHUNK_PAYLOAD + TAG_LEN;
        let mut pkt_ring: Vec<Vec<u8>> = (0..BATCH_SIZE).map(|_| vec![0u8; PKT_BUF_SIZE]).collect();
        let mut pkt_sizes: Vec<usize> = vec![0; BATCH_SIZE];

        let mut video_frames: HashMap<u32, FrameAssembly> = HashMap::new();
        let mut audio_frames: HashMap<u32, AudioAssembly> = HashMap::new();
        let mut stats = StreamStats::default();
        let mut last_packet = Instant::now();
        let mut has_received_first_frame = false;
        let mut last_completed_frame_id: u32 = 0;
        let mut last_pli_at: Option<Instant> = None;
        let mut video_packet_count: u64 = 0;
        let mut audio_packet_count: u64 = 0;
        let mut decrypt_fail_count: u64 = 0;
        let mut stale_drop_count: u64 = 0;
        let mut total_recv_count: u64 = 0;
        let mut completed_frame_count: u64 = 0;
        let mut dropped_frame_count: u64 = 0;
        let mut last_recv_log = Instant::now();
        let mut last_prune = Instant::now();

        // Use poll(2) (Unix) or recv_timeout (Windows) to wait for data
        // with a timeout, so we can check the stop flag periodically.
        #[cfg(unix)]
        {
            receiver.set_nonblocking(true).ok();
        }
        #[cfg(windows)]
        {
            receiver
                .set_read_timeout(Some(Duration::from_millis(50)))
                .ok();
        }

        #[cfg(unix)]
        let poll_fd = libc::pollfd {
            fd: receiver.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };

        while !stop.load(Ordering::Relaxed) {
            // --- Phase 1: Wait for at least one packet ---
            #[cfg(unix)]
            {
                let mut pfd = poll_fd;
                let poll_rc = unsafe {
                    libc::poll(&mut pfd, 1, 50 /* ms */)
                };
                if poll_rc <= 0 {
                    if last_packet.elapsed() >= UDP_DATA_TIMEOUT {
                        let _ = tx.send(BackendCommand::SessionClosed {
                            session_id,
                            reason: "UDP media timed out. The daemon stopped sending data.".into(),
                        });
                        return;
                    }
                    continue;
                }
            }

            #[cfg(windows)]
            {
                // On Windows, try a blocking recv with timeout as our "poll"
                match receiver.recv(&mut pkt_ring[0]) {
                    Ok(size) => {
                        pkt_sizes[0] = size;
                        // We got one packet; we'll drain more below starting from index 1
                    }
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        if last_packet.elapsed() >= UDP_DATA_TIMEOUT {
                            let _ = tx.send(BackendCommand::SessionClosed {
                                session_id,
                                reason: "UDP media timed out. The daemon stopped sending data."
                                    .into(),
                            });
                            return;
                        }
                        continue;
                    }
                    Err(_) => continue,
                }
            }

            // --- Phase 2: Drain the socket as fast as possible ---
            #[cfg(unix)]
            let drain_start = 0;
            #[cfg(windows)]
            let drain_start = 1; // already got one packet in Phase 1

            // Switch to nonblocking for drain (Windows needs this temporarily)
            #[cfg(windows)]
            receiver.set_nonblocking(true).ok();

            let mut batch_count = drain_start;
            loop {
                if batch_count >= BATCH_SIZE {
                    break;
                }
                match receiver.recv(&mut pkt_ring[batch_count]) {
                    Ok(size) => {
                        pkt_sizes[batch_count] = size;
                        batch_count += 1;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }

            // Restore blocking+timeout for Windows
            #[cfg(windows)]
            {
                receiver.set_nonblocking(false).ok();
                receiver
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .ok();
            }

            if batch_count == 0 {
                continue;
            }
            last_packet = Instant::now();
            total_recv_count += batch_count as u64;

            if last_recv_log.elapsed() >= Duration::from_secs(2) {
                let elapsed = last_recv_log.elapsed().as_secs_f64();
                println!(
                    "[desktop/udp] recv rate: {:.0} pkt/s (total={} batch={} completed={} dropped={} decrypt_fail={} stale_drop={})",
                    total_recv_count as f64 / elapsed,
                    total_recv_count,
                    batch_count,
                    completed_frame_count,
                    dropped_frame_count,
                    decrypt_fail_count,
                    stale_drop_count,
                );
                total_recv_count = 0;
                completed_frame_count = 0;
                dropped_frame_count = 0;
                decrypt_fail_count = 0;
                stale_drop_count = 0;
                last_recv_log = Instant::now();
            }

            // --- Phase 3: Process the batch ---
            for pkt_i in 0..batch_count {
                let size = pkt_sizes[pkt_i];
                let buffer = &mut pkt_ring[pkt_i];
                if size < UDP_HEADER_LEN {
                    continue;
                }

                let (header, payload_all) = buffer[..size].split_at_mut(UDP_HEADER_LEN);
                let frame_id = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
                let chunk_idx = u16::from_be_bytes([header[4], header[5]]);
                let total_data = u16::from_be_bytes([header[6], header[7]]) as usize;
                let total_parity = u16::from_be_bytes([header[8], header[9]]) as usize;
                let flags = header[10];
                let codec_id = header[11];
                let payload_len = u16::from_be_bytes([header[12], header[13]]) as usize;
                let timestamp_ms =
                    u32::from_be_bytes([header[14], header[15], header[16], header[17]]);

                if frame_id == u32::MAX && total_data == 0 {
                    continue;
                }

                let payload = payload_all;
                let nonce = nonce_server(frame_id, chunk_idx, flags);
                let Some(plaintext) = cipher.decrypt(&nonce, header, payload) else {
                    decrypt_fail_count += 1;
                    if decrypt_fail_count <= 5 || decrypt_fail_count % 200 == 0 {
                        println!(
                            "[desktop/video] decrypt FAILED #{} frame={} chunk={}/{}",
                            decrypt_fail_count,
                            frame_id,
                            chunk_idx + 1,
                            total_data
                        );
                    }
                    continue;
                };

                stats.note_packet(size);

                // --- Audio path ---
                if (flags & FLAG_AUDIO) != 0 {
                    audio_packet_count = audio_packet_count.wrapping_add(1);
                    if audio_packet_count == 1 || audio_packet_count % 200 == 0 {
                        println!(
                            "[desktop/audio] packets={} frame_id={} chunk={}/{} payload={}B",
                            audio_packet_count,
                            frame_id,
                            chunk_idx + 1,
                            total_data,
                            payload_len
                        );
                    }
                    let assembly = audio_frames
                        .entry(frame_id)
                        .or_insert_with(|| AudioAssembly::new(total_data));
                    assembly.add_chunk(chunk_idx as usize, plaintext, payload_len);
                    if assembly.is_complete() {
                        if let Some(pcm) = assembly.reassemble() {
                            if let Some(ref player) = audio_player {
                                player.enqueue(&pcm, timestamp_ms);
                            }
                        }
                        audio_frames.remove(&frame_id);
                    }
                    continue;
                }

                // --- Video path ---
                video_packet_count = video_packet_count.wrapping_add(1);
                if video_packet_count == 1 || video_packet_count % 200 == 0 {
                    println!(
                            "[desktop/video] packets={} codec={} frame_id={} chunk={}/{} payload={}B flags=0x{:02x}",
                            video_packet_count, codec_id, frame_id, chunk_idx + 1, total_data, payload_len, flags
                        );
                }

                // Drop stale packets for already-completed frames
                if has_received_first_frame
                    && frame_id < last_completed_frame_id
                    && (last_completed_frame_id - frame_id) < 0x8000_0000
                {
                    stale_drop_count += 1;
                    if stale_drop_count <= 5 || stale_drop_count % 200 == 0 {
                        println!(
                            "[desktop/video] STALE drop #{} frame={} (last_completed={})",
                            stale_drop_count, frame_id, last_completed_frame_id
                        );
                    }
                    continue;
                }

                let assembly = video_frames.entry(frame_id).or_insert_with(|| {
                    FrameAssembly::new(total_data, total_parity, codec_id, flags, timestamp_ms)
                });
                assembly.add_chunk(chunk_idx as usize, plaintext, payload_len);

                if assembly.is_complete() {
                    if let Some(annex_b) = assembly.reassemble() {
                        video_frames.remove(&frame_id);

                        // Gap detection -> request PLI (throttled)
                        if has_received_first_frame && frame_id > last_completed_frame_id + 1 {
                            let gap = frame_id - last_completed_frame_id - 1;
                            if gap > 0 && gap < 0x8000_0000 {
                                if should_send_pli(&mut last_pli_at) {
                                    println!(
                                            "[desktop/video] frame gap: last={} current={} gap={} -> PLI",
                                            last_completed_frame_id, frame_id, gap,
                                        );
                                    let _ = control.send_pli();
                                }
                            }
                        }

                        last_completed_frame_id = frame_id;
                        has_received_first_frame = true;
                        av_sync.update_video(timestamp_ms);
                        stats.note_frame();
                        completed_frame_count += 1;

                        // Send to decoder thread (unbounded — never drops frames)
                        let job = DecodeJob {
                            annex_b,
                            frame_id,
                            codec_id,
                            flags,
                            timestamp_ms,
                        };
                        let _ = decode_tx.send(job);
                    } else {
                        video_frames.remove(&frame_id);
                    }
                }
            } // end for pkt_i in 0..batch_count

            // Prune expired incomplete assemblies — every 100ms
            // Like the iPad client: try FEC recovery before dropping,
            // and send PLI if frames had to be discarded.
            if last_prune.elapsed() >= Duration::from_millis(100) {
                let now = Instant::now();
                let mut expired_ids = Vec::new();
                let mut had_unrecoverable = false;

                for (&fid, assembly) in video_frames.iter() {
                    if now.duration_since(assembly.created_at) > FRAME_TIMEOUT {
                        expired_ids.push(fid);
                    }
                }

                for fid in expired_ids {
                    if let Some(assembly) = video_frames.remove(&fid) {
                        if assembly.can_recover() {
                            if let Some(annex_b) = assembly.reassemble() {
                                let job = DecodeJob {
                                    annex_b,
                                    frame_id: fid,
                                    codec_id: assembly.codec_id,
                                    flags: assembly.flags,
                                    timestamp_ms: assembly.timestamp_ms,
                                };
                                let _ = decode_tx.send(job);
                            } else {
                                had_unrecoverable = true;
                            }
                        } else {
                            let needed = assembly.total_data;
                            let got = assembly.received_count;
                            let total = assembly.total_data + assembly.total_parity;
                            let is_idr = assembly.flags & 0x01 != 0;
                            let age_ms = now.duration_since(assembly.created_at).as_millis();
                            println!(
                                    "[desktop/video] dropped frame {} ({}) got {}/{} shards (need {}) age={}ms",
                                    fid,
                                    if is_idr { "IDR" } else { "P" },
                                    got,
                                    total,
                                    needed,
                                    age_ms,
                                );
                            had_unrecoverable = true;
                            dropped_frame_count += 1;
                        }
                    }
                }

                if had_unrecoverable {
                    if should_send_pli(&mut last_pli_at) {
                        println!("[desktop/video] unrecoverable frame(s) expired -> PLI");
                        let _ = control.send_pli();
                    }
                }

                audio_frames.retain(|_, a| now.duration_since(a.created_at) <= FRAME_TIMEOUT);
                last_prune = now;
            }

            if let Some(update) = stats.sample(av_sync.estimate_transport_latency_ms()) {
                ui(UiEvent::SetStats {
                    fps: update.fps,
                    bitrate_mbps: update.bitrate_mbps,
                    latency_ms: update.latency_ms,
                    dropped_frames: stats.dropped_frames,
                });
            }
        }
    });
}

pub struct ControlSender {
    inner: Mutex<ControlWriter>,
}

struct ControlWriter {
    stream: TcpStream,
    cipher: SessionCipher,
    seq: u32,
    send_buf: Vec<u8>,
}

impl ControlSender {
    fn new(stream: TcpStream, session_key: [u8; 32]) -> Result<Self> {
        stream.set_nodelay(true).ok();
        Ok(Self {
            inner: Mutex::new(ControlWriter {
                stream,
                cipher: SessionCipher::new(&session_key)?,
                seq: 0,
                send_buf: Vec::with_capacity(512),
            }),
        })
    }

    pub fn send_payload(&self, payload: &[u8]) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let seq = inner.seq;
        inner.seq = inner.seq.wrapping_add(1);

        let aad = seq.to_be_bytes();
        let nonce = nonce_control_client(seq);

        // Encrypt in-place within send_buf to avoid separate allocation.
        // Layout: [body_len:4] [aad:4] [encrypted_payload] [tag:16]
        let ControlWriter {
            stream,
            send_buf,
            cipher,
            ..
        } = &mut *inner;
        send_buf.clear();
        // Reserve space for: 4 (len) + 4 (aad) + payload.len() + 16 (tag)
        let total = 4 + aad.len() + payload.len() + 16;
        send_buf.reserve(total);
        // Placeholder for body_len (filled after encryption)
        send_buf.extend_from_slice(&[0u8; 4]);
        send_buf.extend_from_slice(&aad);
        let encrypt_start = send_buf.len();
        send_buf.extend_from_slice(payload);
        let nonce = Nonce::assume_unique_for_key(nonce);
        let tag = cipher
            .key
            .seal_in_place_separate_tag(nonce, Aad::from(&aad[..]), &mut send_buf[encrypt_start..])
            .map_err(|_| anyhow!("AES-GCM encrypt failed"))?;
        send_buf.extend_from_slice(tag.as_ref());
        // Fill in body_len = everything after the 4-byte length prefix
        let body_len = (send_buf.len() - 4) as u32;
        send_buf[..4].copy_from_slice(&body_len.to_be_bytes());
        stream.write_all(send_buf)?;
        Ok(())
    }

    fn send_pli(&self) -> Result<()> {
        self.send_payload(MAGIC_PLI)
    }

    fn send_speaker_state(&self, enabled: bool) -> Result<()> {
        let mut payload = Vec::with_capacity(MAGIC_SPEAKER.len() + 1);
        payload.extend_from_slice(MAGIC_SPEAKER);
        payload.push(u8::from(enabled));
        self.send_payload(&payload)
    }

    fn send_mic_state(&self, enabled: bool) -> Result<()> {
        let mut payload = Vec::with_capacity(MAGIC_MICCFG.len() + 1);
        payload.extend_from_slice(MAGIC_MICCFG);
        payload.push(u8::from(enabled));
        self.send_payload(&payload)
    }

    fn send_peripheral_state(&self, periph: u8, attached: bool) -> Result<()> {
        let mut payload = Vec::with_capacity(MAGIC_PERIPH.len() + 2);
        payload.extend_from_slice(MAGIC_PERIPH);
        payload.push(periph);
        payload.push(if attached {
            PERIPH_ATTACHED
        } else {
            PERIPH_DETACHED
        });
        self.send_payload(&payload)
    }

    fn send_camera_config(&self, config: CameraConfig) -> Result<()> {
        let mut payload = Vec::with_capacity(MAGIC_CAMCFG.len() + 6);
        payload.extend_from_slice(MAGIC_CAMCFG);
        payload.extend_from_slice(&(config.width as u16).to_be_bytes());
        payload.extend_from_slice(&(config.height as u16).to_be_bytes());
        payload.extend_from_slice(&(config.fps as u16).to_be_bytes());
        self.send_payload(&payload)
    }

    fn send_camera_disable(&self) -> Result<()> {
        let mut payload = Vec::with_capacity(MAGIC_CAMCFG.len() + 6);
        payload.extend_from_slice(MAGIC_CAMCFG);
        payload.extend_from_slice(&[0u8; 6]);
        self.send_payload(&payload)
    }

    fn disconnect_gracefully(&self) -> Result<()> {
        self.send_payload(MAGIC_DISCONNECT)
    }
}

pub struct UdpSender {
    pub socket: UdpSocket,
    pub session_key: [u8; 32],
    pub server_addr: SocketAddr,
    inner: Mutex<UdpSenderInner>,
}

struct UdpSenderInner {
    cipher: SessionCipher,
    seq: u32,
    send_buf: Vec<u8>,
}

impl UdpSender {
    pub fn new(server_addr: SocketAddr, session_key: [u8; 32]) -> Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", 0)).context("bind UDP client socket")?;
        tune_udp_socket(&socket).ok();
        socket
            .connect(server_addr)
            .with_context(|| format!("connect UDP socket to {server_addr}"))?;
        Ok(Self {
            socket,
            session_key,
            server_addr,
            inner: Mutex::new(UdpSenderInner {
                cipher: SessionCipher::new(&session_key)?,
                seq: 0,
                send_buf: Vec::with_capacity(2048),
            }),
        })
    }

    pub fn send_encrypted(&self, payload: &[u8]) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let seq = inner.seq;
        inner.seq = inner.seq.wrapping_add(1);

        let aad = seq.to_be_bytes();
        let nonce_bytes = nonce_client(seq);

        // Encrypt in-place within send_buf to avoid per-call allocation.
        // Layout: [aad:4] [encrypted_payload] [tag:16]
        let UdpSenderInner {
            cipher, send_buf, ..
        } = &mut *inner;
        send_buf.clear();
        let total = aad.len() + payload.len() + TAG_LEN;
        send_buf.reserve(total);
        send_buf.extend_from_slice(&aad);
        let encrypt_start = send_buf.len();
        send_buf.extend_from_slice(payload);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let tag = cipher
            .key
            .seal_in_place_separate_tag(nonce, Aad::from(&aad[..]), &mut send_buf[encrypt_start..])
            .map_err(|_| anyhow!("AES-GCM encrypt failed"))?;
        send_buf.extend_from_slice(tag.as_ref());
        self.socket
            .send(send_buf)
            .context("send encrypted UDP packet")?;
        Ok(())
    }
}

fn tune_udp_socket(socket: &UdpSocket) -> Result<()> {
    // Platform-specific socket tuning for large UDP buffers.
    #[cfg(unix)]
    {
        let fd = socket.as_raw_fd();
        let mut value: libc::c_int = 8 * 1024 * 1024;
        let value_ptr = &value as *const _ as *const libc::c_void;
        let value_len = std::mem::size_of_val(&value) as libc::socklen_t;

        let recv_rc = unsafe {
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, value_ptr, value_len)
        };
        if recv_rc != 0 {
            value = 4 * 1024 * 1024;
            let value_ptr = &value as *const _ as *const libc::c_void;
            let recv_rc = unsafe {
                libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, value_ptr, value_len)
            };
            if recv_rc != 0 {
                return Err(anyhow!(
                    "setsockopt SO_RCVBUF failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }

        let mut actual: libc::c_int = 0;
        let mut actual_len: libc::socklen_t = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &mut actual as *mut _ as *mut libc::c_void,
                &mut actual_len,
            );
        }
        println!("[udp] SO_RCVBUF requested={} actual={}", value, actual);

        let send_value: libc::c_int = 4 * 1024 * 1024;
        let send_ptr = &send_value as *const _ as *const libc::c_void;
        let send_rc =
            unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, send_ptr, value_len) };
        if send_rc != 0 {
            return Err(anyhow!(
                "setsockopt SO_SNDBUF failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        // On Windows, libc::setsockopt uses *const i8 instead of *const c_void.
        // Use the ws2_32 API directly via libc's Windows bindings.
        let sock = socket.as_raw_socket() as usize;

        // Windows SOL_SOCKET = 0xffff, SO_RCVBUF = 0x1002, SO_SNDBUF = 0x1001
        const SOL_SOCKET: i32 = 0xffff_u16 as i32;
        const SO_RCVBUF: i32 = 0x1002;
        const SO_SNDBUF: i32 = 0x1001;

        let mut value: i32 = 8 * 1024 * 1024;
        let value_len = std::mem::size_of::<i32>() as i32;

        let recv_rc = unsafe {
            libc::setsockopt(
                sock as _,
                SOL_SOCKET,
                SO_RCVBUF,
                &value as *const _ as *const i8,
                value_len,
            )
        };
        if recv_rc != 0 {
            value = 4 * 1024 * 1024;
            let recv_rc = unsafe {
                libc::setsockopt(
                    sock as _,
                    SOL_SOCKET,
                    SO_RCVBUF,
                    &value as *const _ as *const i8,
                    value_len,
                )
            };
            if recv_rc != 0 {
                return Err(anyhow!(
                    "setsockopt SO_RCVBUF failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }

        let mut actual: i32 = 0;
        let mut actual_len: i32 = std::mem::size_of::<i32>() as i32;
        unsafe {
            libc::getsockopt(
                sock as _,
                SOL_SOCKET,
                SO_RCVBUF,
                &mut actual as *mut _ as *mut i8,
                &mut actual_len,
            );
        }
        println!("[udp] SO_RCVBUF requested={} actual={}", value, actual);

        let send_value: i32 = 4 * 1024 * 1024;
        let send_rc = unsafe {
            libc::setsockopt(
                sock as _,
                SOL_SOCKET,
                SO_SNDBUF,
                &send_value as *const _ as *const i8,
                value_len,
            )
        };
        if send_rc != 0 {
            return Err(anyhow!(
                "setsockopt SO_SNDBUF failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }

    Ok(())
}

#[derive(Clone, Copy)]
struct CameraConfig {
    width: u32,
    height: u32,
    fps: u32,
}

fn parse_camera_mode(mode: &str) -> Option<CameraConfig> {
    let nums: Vec<u32> = mode
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok())
        .collect();
    if nums.len() < 3 {
        return None;
    }
    Some(CameraConfig {
        width: nums[0],
        height: nums[1],
        fps: nums[2],
    })
}

/// Thread-safe AV sync state — shared between the UDP receiver thread and the
/// audio player.  Mirrors the iPad's `AVSyncState` class.
pub struct AvSyncState {
    inner: Mutex<AvSyncInner>,
}

struct AvSyncInner {
    last_video_ts: u32,
    last_video_wall: Option<Instant>,
    gap_resume: bool,
}

/// How long without a video frame before we consider it a gap (clear audio buf).
const AV_GAP_THRESHOLD: Duration = Duration::from_secs(2);

impl AvSyncState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(AvSyncInner {
                last_video_ts: 0,
                last_video_wall: None,
                gap_resume: false,
            }),
        }
    }

    /// Called by the UDP receiver when a video frame completes assembly.
    pub fn update_video(&self, timestamp_ms: u32) {
        if let Ok(mut inner) = self.inner.lock() {
            let now = Instant::now();
            if let Some(prev_wall) = inner.last_video_wall {
                if now.duration_since(prev_wall) >= AV_GAP_THRESHOLD {
                    inner.gap_resume = true;
                }
            }
            inner.last_video_ts = timestamp_ms;
            inner.last_video_wall = Some(now);
        }
    }

    /// Returns `true` once after video resumes from a gap. Consuming — resets
    /// after read.  The audio player uses this to flush its ring buffer.
    pub fn consume_gap_resume(&self) -> bool {
        if let Ok(mut inner) = self.inner.lock() {
            let val = inner.gap_resume;
            inner.gap_resume = false;
            val
        } else {
            false
        }
    }

    /// Extrapolate what daemon timestamp should be "right now" based on the last
    /// video frame and local wall-clock elapsed time.
    pub fn expected_daemon_time_now(&self) -> Option<u32> {
        if let Ok(inner) = self.inner.lock() {
            let wall = inner.last_video_wall?;
            if wall.elapsed() > Duration::from_secs(1) {
                return None; // stale
            }
            let elapsed_ms = wall.elapsed().as_millis() as u32;
            Some(inner.last_video_ts.wrapping_add(elapsed_ms))
        } else {
            None
        }
    }

    pub fn estimate_transport_latency_ms(&self) -> u32 {
        if let Ok(inner) = self.inner.lock() {
            let Some(wall) = inner.last_video_wall else {
                return 0;
            };
            let elapsed = wall.elapsed().as_millis() as u32;
            elapsed.min(999)
        } else {
            0
        }
    }
}

#[derive(Default)]
struct StreamStats {
    window_start: Option<Instant>,
    window_bytes: u64,
    window_frames: u64,
    dropped_frames: u32,
}

struct StatsUpdate {
    fps: u32,
    bitrate_mbps: f32,
    latency_ms: u32,
}

impl StreamStats {
    fn note_packet(&mut self, size: usize) {
        self.window_start.get_or_insert_with(Instant::now);
        self.window_bytes += size as u64;
    }

    fn note_frame(&mut self) {
        self.window_start.get_or_insert_with(Instant::now);
        self.window_frames += 1;
    }

    fn sample(&mut self, latency_ms: u32) -> Option<StatsUpdate> {
        let start = self.window_start?;
        let elapsed = start.elapsed();
        if elapsed < Duration::from_secs(1) {
            return None;
        }

        let seconds = elapsed.as_secs_f32().max(0.001);
        let fps = (self.window_frames as f32 / seconds).round() as u32;
        let bitrate_mbps = ((self.window_bytes as f32 * 8.0) / seconds) / 1_000_000.0;

        self.window_start = Some(Instant::now());
        self.window_bytes = 0;
        self.window_frames = 0;

        Some(StatsUpdate {
            fps,
            bitrate_mbps,
            latency_ms,
        })
    }
}

struct FrameAssembly {
    total_data: usize,
    total_parity: usize,
    created_at: Instant,
    /// Contiguous buffer: `total_shards * UDP_CHUNK_PAYLOAD` bytes.
    /// Each shard occupies a fixed-size slot to avoid per-chunk Vec allocation.
    shard_buf: Vec<u8>,
    /// Tracks which shard slots have been filled.
    present: Vec<bool>,
    /// Actual (pre-padding) payload length for each shard.
    actual_lengths: Vec<usize>,
    received_count: usize,
    codec_id: u8,
    flags: u8,
    timestamp_ms: u32,
}

impl FrameAssembly {
    fn new(
        total_data: usize,
        total_parity: usize,
        codec_id: u8,
        flags: u8,
        timestamp_ms: u32,
    ) -> Self {
        let total_shards = total_data + total_parity;
        Self {
            total_data,
            total_parity,
            created_at: Instant::now(),
            shard_buf: vec![0u8; total_shards * UDP_CHUNK_PAYLOAD],
            present: vec![false; total_shards],
            actual_lengths: vec![0usize; total_shards],
            received_count: 0,
            codec_id,
            flags,
            timestamp_ms,
        }
    }

    fn add_chunk(&mut self, index: usize, payload: &[u8], actual_len: usize) {
        let total_shards = self.total_data + self.total_parity;
        if index >= total_shards {
            return;
        }
        if self.present[index] {
            return; // duplicate
        }
        // Copy payload into the fixed-size slot (zero-padded by initialization).
        let offset = index * UDP_CHUNK_PAYLOAD;
        let copy_len = payload.len().min(UDP_CHUNK_PAYLOAD);
        self.shard_buf[offset..offset + copy_len].copy_from_slice(&payload[..copy_len]);
        self.present[index] = true;
        self.actual_lengths[index] = actual_len;
        self.received_count += 1;
    }

    fn is_complete(&self) -> bool {
        if self.total_parity == 0 {
            return (0..self.total_data).all(|i| self.present[i]);
        }
        self.received_count >= self.total_data
    }

    /// Whether enough shards are present to reconstruct via FEC.
    fn can_recover(&self) -> bool {
        self.received_count >= self.total_data
    }

    fn reassemble(&self) -> Option<Vec<u8>> {
        let all_data_present = (0..self.total_data).all(|i| self.present[i]);
        if all_data_present {
            return Some(self.assemble_present_data());
        }
        if self.total_parity == 0 || self.received_count < self.total_data {
            return None;
        }

        // FEC recovery path: build shard array from the contiguous buffer.
        let total_shards = self.total_data + self.total_parity;
        let mut shards: Vec<Option<Vec<u8>>> = vec![None; total_shards];
        for i in 0..total_shards {
            if self.present[i] {
                let offset = i * UDP_CHUNK_PAYLOAD;
                shards[i] = Some(self.shard_buf[offset..offset + UDP_CHUNK_PAYLOAD].to_vec());
            }
        }

        let rs = ReedSolomon::new(self.total_data, self.total_parity).ok()?;
        rs.reconstruct(&mut shards).ok()?;

        let mut out = Vec::new();
        for i in 0..self.total_data {
            let shard = shards[i].as_ref()?;
            let actual_len = if self.actual_lengths[i] > 0 {
                self.actual_lengths[i]
            } else {
                shard.len()
            };
            out.extend_from_slice(&shard[..actual_len.min(shard.len())]);
        }
        Some(out)
    }

    fn assemble_present_data(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..self.total_data {
            if self.present[i] {
                let offset = i * UDP_CHUNK_PAYLOAD;
                let actual_len = if self.actual_lengths[i] > 0 {
                    self.actual_lengths[i]
                } else {
                    UDP_CHUNK_PAYLOAD
                };
                let end = offset + actual_len.min(UDP_CHUNK_PAYLOAD);
                out.extend_from_slice(&self.shard_buf[offset..end]);
            }
        }
        out
    }
}

struct AudioAssembly {
    total_data: usize,
    created_at: Instant,
    /// Contiguous buffer: `total_data * UDP_CHUNK_PAYLOAD` bytes.
    shard_buf: Vec<u8>,
    present: Vec<bool>,
    actual_lengths: Vec<usize>,
    received_count: usize,
}

impl AudioAssembly {
    fn new(total_data: usize) -> Self {
        Self {
            total_data,
            created_at: Instant::now(),
            shard_buf: vec![0u8; total_data * UDP_CHUNK_PAYLOAD],
            present: vec![false; total_data],
            actual_lengths: vec![0usize; total_data],
            received_count: 0,
        }
    }

    fn add_chunk(&mut self, index: usize, payload: &[u8], actual_len: usize) {
        if index >= self.total_data {
            return;
        }
        if self.present[index] {
            return; // duplicate
        }
        let offset = index * UDP_CHUNK_PAYLOAD;
        let copy_len = payload.len().min(UDP_CHUNK_PAYLOAD);
        self.shard_buf[offset..offset + copy_len].copy_from_slice(&payload[..copy_len]);
        self.present[index] = true;
        self.actual_lengths[index] = actual_len;
        self.received_count += 1;
    }

    fn is_complete(&self) -> bool {
        self.received_count >= self.total_data
    }

    fn reassemble(&self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut out = Vec::new();
        for i in 0..self.total_data {
            if !self.present[i] {
                return None;
            }
            let offset = i * UDP_CHUNK_PAYLOAD;
            let actual_len = if self.actual_lengths[i] > 0 {
                self.actual_lengths[i]
            } else {
                UDP_CHUNK_PAYLOAD
            };
            let end = offset + actual_len.min(UDP_CHUNK_PAYLOAD);
            out.extend_from_slice(&self.shard_buf[offset..end]);
        }
        Some(out)
    }
}

struct EndpointInfo {
    server_addr: SocketAddr,
    display_host: String,
    endpoint_key: String,
}

fn dump_decoded_frame_ppm(frame_index: u64, width: u32, height: u32, rgba: &[u8]) {
    let path = format!("/tmp/screx-decoded-frame-{frame_index:06}.ppm");
    let pixel_count = rgba.len() / 4;
    let non_black = rgba
        .chunks_exact(4)
        .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
        .count();

    let mut ppm = Vec::with_capacity(32 + pixel_count * 3);
    ppm.extend_from_slice(format!("P6\n{} {}\n255\n", width, height).as_bytes());
    for px in rgba.chunks_exact(4) {
        ppm.extend_from_slice(&px[..3]);
    }

    match fs::write(&path, ppm) {
        Ok(()) => {
            let pct = if pixel_count == 0 {
                0.0
            } else {
                (non_black as f64 * 100.0) / (pixel_count as f64)
            };
            println!(
                "[desktop/video] dumped decoded frame {} to {} (non_black={:.1}%)",
                frame_index, path, pct
            );
        }
        Err(error) => {
            eprintln!(
                "[desktop/video] failed to dump decoded frame {} to {}: {}",
                frame_index, path, error
            );
        }
    }
}

fn non_black_pixel_percent(rgba: &[u8]) -> f64 {
    let pixel_count = rgba.len() / 4;
    if pixel_count == 0 {
        return 0.0;
    }
    let non_black = rgba
        .chunks_exact(4)
        .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
        .count();
    (non_black as f64 * 100.0) / (pixel_count as f64)
}

/// Sampled version: checks every 16th pixel for performance.
fn non_black_pixel_percent_sampled(rgba: &[u8]) -> f64 {
    let pixel_count = rgba.len() / 4;
    if pixel_count == 0 {
        return 0.0;
    }
    let step = 16;
    let mut sampled = 0u64;
    let mut non_black = 0u64;
    for px in rgba.chunks_exact(4).step_by(step) {
        sampled += 1;
        if px[0] != 0 || px[1] != 0 || px[2] != 0 {
            non_black += 1;
        }
    }
    if sampled == 0 {
        return 0.0;
    }
    (non_black as f64 * 100.0) / (sampled as f64)
}

fn dump_access_unit(frame_id: u32, codec_id: u8, is_idr: bool, annex_b: &[u8]) {
    let ext = if codec_id == 0x01 { "h265" } else { "h264" };
    let path = format!("/tmp/screx-au-{frame_id:06}.{ext}");
    match fs::write(&path, annex_b) {
        Ok(()) => {
            let prefix_len = annex_b.len().min(24);
            println!(
                "[desktop/video] dumped access unit frame_id={} codec={} idr={} bytes={} path={} prefix={:02x?}",
                frame_id,
                codec_id,
                is_idr,
                annex_b.len(),
                path,
                &annex_b[..prefix_len]
            );
        }
        Err(error) => {
            eprintln!(
                "[desktop/video] failed to dump access unit frame_id={} to {}: {}",
                frame_id, path, error
            );
        }
    }
}

fn resolve_endpoint(host_input: &str) -> Result<EndpointInfo> {
    let trimmed = host_input.trim();
    if trimmed.is_empty() {
        bail!("enter a hostname or IP address");
    }

    let (host, port) = parse_host_port(trimmed);
    let server_addr = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("resolve {host}:{port}"))?
        .next()
        .ok_or_else(|| anyhow!("no address resolved for {host}:{port}"))?;

    Ok(EndpointInfo {
        server_addr,
        display_host: host.clone(),
        endpoint_key: format!("{host}:{port}"),
    })
}

fn parse_host_port(input: &str) -> (String, u16) {
    if let Some(host) = input
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
    {
        let port = host
            .1
            .strip_prefix(':')
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);
        return (host.0.to_string(), port);
    }

    if let Some((host, port)) = input.rsplit_once(':') {
        if !host.contains(':') {
            if let Ok(port) = port.parse::<u16>() {
                return (host.to_string(), port);
            }
        }
    }

    (input.to_string(), DEFAULT_PORT)
}

#[derive(Default, Serialize, Deserialize)]
struct StoredState {
    device_id: Option<[u8; 16]>,
    pairings: HashMap<String, String>,
    #[serde(default)]
    recent_connections: Vec<RecentConnection>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RecentConnection {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub last_connected_at: u64, // unix timestamp seconds
    pub pinned: bool,
}

const MAX_PINNED: usize = 10;
const MAX_RECENT: usize = 5;

#[derive(Default)]
struct AppStorage {
    path: PathBuf,
    state: StoredState,
}

impl AppStorage {
    fn load() -> Result<Self> {
        let path = storage_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }

        let state = match fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => StoredState::default(),
        };

        Ok(Self { path, state })
    }

    fn get_or_create_device_id(&mut self) -> Result<[u8; 16]> {
        if let Some(device_id) = self.state.device_id {
            return Ok(device_id);
        }

        let rng = SystemRandom::new();
        let mut device_id = [0u8; 16];
        rng.fill(&mut device_id)
            .map_err(|_| anyhow!("failed to generate device id"))?;
        self.state.device_id = Some(device_id);
        self.save()?;
        Ok(device_id)
    }

    fn get_pairing_key(&self, endpoint_key: &str) -> Option<[u8; 32]> {
        let value = self.state.pairings.get(endpoint_key)?;
        decode_hex_32(value)
    }

    fn set_pairing_key(&mut self, endpoint_key: &str, key: [u8; 32]) -> Result<()> {
        self.state
            .pairings
            .insert(endpoint_key.to_string(), encode_hex(&key));
        self.save()
    }

    fn remember_connection(&mut self, name: &str, host: &str, port: u16) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let existing_pinned = self
            .state
            .recent_connections
            .iter()
            .find(|c| c.host == host && c.port == port)
            .map(|c| c.pinned)
            .unwrap_or(false);

        self.state
            .recent_connections
            .retain(|c| c.host != host || c.port != port);

        self.state.recent_connections.insert(
            0,
            RecentConnection {
                host: host.to_string(),
                port,
                name: name.to_string(),
                last_connected_at: now,
                pinned: existing_pinned,
            },
        );

        self.normalize_connections();
        self.save().ok();
    }

    fn toggle_pinned(&mut self, host: &str, port: u16) {
        for c in &mut self.state.recent_connections {
            if c.host == host && c.port == port {
                c.pinned = !c.pinned;
                break;
            }
        }
        self.normalize_connections();
        self.save().ok();
    }

    fn delete_connection(&mut self, host: &str, port: u16) {
        self.state
            .recent_connections
            .retain(|c| c.host != host || c.port != port);
        self.save().ok();
    }

    fn update_connection_name(&mut self, host: &str, port: u16, name: &str) {
        for c in &mut self.state.recent_connections {
            if c.host == host && c.port == port {
                c.name = name.to_string();
            }
        }
        self.save().ok();
    }

    fn clear_recent_connections(&mut self) {
        self.state.recent_connections.retain(|c| c.pinned);
        self.save().ok();
    }

    fn normalize_connections(&mut self) {
        let mut pinned: Vec<_> = self
            .state
            .recent_connections
            .iter()
            .filter(|c| c.pinned)
            .cloned()
            .collect();
        let mut recent: Vec<_> = self
            .state
            .recent_connections
            .iter()
            .filter(|c| !c.pinned)
            .cloned()
            .collect();
        pinned.truncate(MAX_PINNED);
        recent.truncate(MAX_RECENT);
        pinned.append(&mut recent);
        self.state.recent_connections = pinned;
    }

    fn get_connections(&self) -> &[RecentConnection] {
        &self.state.recent_connections
    }

    fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.state)?;
        fs::write(&self.path, bytes)?;
        Ok(())
    }
}

fn storage_path() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("could not locate config directory"))?;
    Ok(base.join("screx-desktop").join("state.json"))
}

struct SessionCipher {
    key: LessSafeKey,
}

impl SessionCipher {
    fn new(key_bytes: &[u8; 32]) -> Result<Self> {
        let key =
            UnboundKey::new(&AES_256_GCM, key_bytes).map_err(|_| anyhow!("invalid AES key"))?;
        Ok(Self {
            key: LessSafeKey::new(key),
        })
    }

    fn encrypt_vec(&self, nonce_bytes: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce = Nonce::assume_unique_for_key(*nonce_bytes);
        let mut buffer = plaintext.to_vec();
        let tag = self
            .key
            .seal_in_place_separate_tag(nonce, Aad::from(aad), &mut buffer)
            .map_err(|_| anyhow!("AES-GCM encrypt failed"))?;
        buffer.extend_from_slice(tag.as_ref());
        Ok(buffer)
    }

    fn decrypt<'a>(
        &self,
        nonce_bytes: &[u8; 12],
        aad: &[u8],
        ciphertext_with_tag: &'a mut [u8],
    ) -> Option<&'a [u8]> {
        let nonce = Nonce::assume_unique_for_key(*nonce_bytes);
        self.key
            .open_in_place(nonce, Aad::from(aad), ciphertext_with_tag)
            .ok()
            .map(|slice| &*slice)
    }
}

fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, salt);
    let prk = salt.extract(ikm);
    let info_parts = [info];
    let okm = prk
        .expand(&info_parts, hkdf::HKDF_SHA256)
        .expect("hkdf expand");
    let mut out = [0u8; 32];
    okm.fill(&mut out).expect("hkdf fill");
    out
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let tag = hmac::sign(&key, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

fn nonce_server(frame_id: u32, chunk_idx: u16, flags: u8) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0] = 0x00;
    nonce[1..5].copy_from_slice(&frame_id.to_be_bytes());
    nonce[5..7].copy_from_slice(&chunk_idx.to_be_bytes());
    nonce[7] = flags;
    nonce
}

fn nonce_client(seq_num: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0] = 0xFF;
    nonce[1..5].copy_from_slice(&seq_num.to_be_bytes());
    nonce
}

fn nonce_control_client(seq_num: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0] = 0x7F;
    nonce[1..5].copy_from_slice(&seq_num.to_be_bytes());
    nonce
}

fn nonce_control_server(seq_num: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0] = 0x80;
    nonce[1..5].copy_from_slice(&seq_num.to_be_bytes());
    nonce
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex_32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, slot) in out.iter_mut().enumerate() {
        let pos = index * 2;
        *slot = u8::from_str_radix(&hex[pos..pos + 2], 16).ok()?;
    }
    Some(out)
}
