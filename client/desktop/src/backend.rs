use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
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
use crate::decoder::{CodecId, VideoDecoder};
use crate::mic_capture::MicCapture;
use crate::video_surface::{FrameSlot, RawFrame};
use crate::webcam_capture::WebcamCapture;

const DEFAULT_PORT: u16 = 9000;
const CONTROL_MAX_FRAME: usize = 65536;
const UDP_HEADER_LEN: usize = 18;
const UDP_CHUNK_PAYLOAD: usize = 1400;
const UDP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
const UDP_DATA_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_READ_TIMEOUT: Duration = Duration::from_millis(500);
const UDP_READ_TIMEOUT: Duration = Duration::from_millis(250);
const FRAME_TIMEOUT: Duration = Duration::from_millis(50);
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
const FLAG_IDR: u8 = 0x01;
const FLAG_AUDIO: u8 = 0x02;
const TAG_LEN: usize = 16;

#[derive(Clone)]
pub struct BackendHandle {
    tx: Sender<BackendCommand>,
}

impl BackendHandle {
    pub fn connect(&self, host: String, camera_mode: String, speaker_enabled: bool) {
        let _ = self.tx.send(BackendCommand::Connect {
            host,
            camera_mode,
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

    pub fn note_unimplemented_toggle(&self, label: &'static str, enabled: bool) {
        let _ = self
            .tx
            .send(BackendCommand::NoteUnimplementedToggle { label, enabled });
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

    pub fn send_key_event(&self, hid_usage: u16, pressed: bool) {
        let _ = self
            .tx
            .send(BackendCommand::SendKeyEvent { hid_usage, pressed });
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
        camera_mode: String,
        speaker_enabled: bool,
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
    NoteUnimplementedToggle {
        label: &'static str,
        enabled: bool,
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
    SendKeyEvent {
        hid_usage: u16,
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
}

pub fn spawn_backend<F>(ui: F, frame_slot: FrameSlot) -> BackendHandle
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
    frame_slot: FrameSlot,
}

impl BackendWorker {
    fn new(
        rx: Receiver<BackendCommand>,
        tx: Sender<BackendCommand>,
        ui: Arc<dyn Fn(UiEvent) + Send + Sync>,
        frame_slot: FrameSlot,
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
        }
    }

    fn run(&mut self) {
        while let Ok(command) = self.rx.recv() {
            match command {
                BackendCommand::Connect {
                    host,
                    camera_mode,
                    speaker_enabled,
                } => self.handle_connect(host, camera_mode, speaker_enabled),
                BackendCommand::SubmitPin { pin } => self.handle_submit_pin(pin),
                BackendCommand::Disconnect => self.handle_disconnect(false),
                BackendCommand::SetSpeaker { enabled } => self.handle_set_speaker(enabled),
                BackendCommand::SetCameraMode { mode } => self.handle_set_camera_mode(mode),
                BackendCommand::NoteUnimplementedToggle { label, enabled } => {
                    (self.ui)(UiEvent::SetStatus(format!(
                        "{label} {} UI state only for now. Platform adapter wiring is next.",
                        if enabled { "enabled" } else { "disabled" }
                    )));
                }
                BackendCommand::SetMic { enabled } => self.handle_set_mic(enabled),
                BackendCommand::SetCamera { enabled } => self.handle_set_camera(enabled),
                BackendCommand::SetKeyboard { enabled } => {
                    // Keyboard is always-on when connected; toggle is cosmetic state.
                    (self.ui)(UiEvent::SetStatus(format!(
                        "Keyboard forwarding {}.",
                        if enabled { "active" } else { "paused" }
                    )));
                }
                BackendCommand::SendKeyEvent { hid_usage, pressed } => {
                    if let Some(ref active) = self.active {
                        let _ = crate::input::send_raw_key(&active.control, hid_usage, pressed);
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
            }
        }
    }

    fn handle_connect(&mut self, host_input: String, camera_mode: String, speaker_enabled: bool) {
        self.handle_disconnect(true);
        self.pending_pairing = None;

        (self.ui)(UiEvent::ClearPinPrompt);
        (self.ui)(UiEvent::SetConnecting(true));
        (self.ui)(UiEvent::SetConnected(false));
        (self.ui)(UiEvent::SetStatus(format!("Connecting to {host_input}...")));

        match establish_session(&mut self.storage, &host_input) {
            Ok(ConnectResult::Established(bootstrap)) => {
                self.activate_session(bootstrap, camera_mode, speaker_enabled);
            }
            Ok(ConnectResult::PinRequired(pending)) => {
                self.pending_pairing = Some(pending);
                (self.ui)(UiEvent::SetConnecting(false));
                (self.ui)(UiEvent::PinRequired(
                    "Enter the 6-digit PIN shown in the daemon terminal.".into(),
                ));
                (self.ui)(UiEvent::SetStatus(
                    "Pairing requested. Waiting for the 6-digit PIN.".into(),
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
                let camera_mode = bootstrap.initial_camera_mode.clone();
                let speaker_enabled = bootstrap.initial_speaker_enabled;
                self.activate_session(bootstrap, camera_mode, speaker_enabled);
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
        mut bootstrap: SessionBootstrap,
        camera_mode: String,
        speaker_enabled: bool,
    ) {
        bootstrap.initial_camera_mode = camera_mode.clone();
        bootstrap.initial_speaker_enabled = speaker_enabled;

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
        (self.ui)(UiEvent::SetTransportLabel("Network".into()));
        (self.ui)(UiEvent::SetCodecLabel("Negotiating stream".into()));
        (self.ui)(UiEvent::SetResolutionLabel("Receiving stream".into()));
        (self.ui)(UiEvent::SetConnecting(false));
        (self.ui)(UiEvent::SetConnected(true));
        (self.ui)(UiEvent::SetStatus(format!(
            "Session established with {title}. Waiting for UDP media..."
        )));

        if let Some(config) = parse_camera_mode(&camera_mode) {
            // Store the camera mode but don't send CAMCFG yet — the daemon only
            // creates the virtual webcam when the user explicitly enables the camera.
            let _ = config;
        }
        let _ = control.send_speaker_state(speaker_enabled);

        // Start audio playback before spawning UDP so the player is ready for PCM.
        let audio_player: Option<Arc<AudioPlayer>> = if speaker_enabled {
            match AudioPlayer::start() {
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
        );

        self.active = Some(ActiveSession {
            session_id,
            control,
            udp: udp_sender,
            stop,
            audio_player,
            mic_capture: None,
            webcam_capture: None,
        });
    }

    fn handle_disconnect(&mut self, quiet: bool) {
        self.pending_pairing = None;
        if let Some(active) = self.active.take() {
            // Tell the daemon to tear down virtual devices if active
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
                    match AudioPlayer::start() {
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
    mic_capture: Option<MicCapture>,
    webcam_capture: Option<WebcamCapture>,
}

struct PendingPairing {
    tcp: TcpStream,
    server_addr: SocketAddr,
    endpoint_key: String,
    display_host: String,
    ecdh_secret: Vec<u8>,
    initial_camera_mode: String,
    initial_speaker_enabled: bool,
}

struct SessionBootstrap {
    control_stream: TcpStream,
    server_addr: SocketAddr,
    display_host: String,
    session_key: [u8; 32],
    initial_camera_mode: String,
    initial_speaker_enabled: bool,
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
                initial_camera_mode: String::new(),
                initial_speaker_enabled: true,
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
                initial_camera_mode: pending.initial_camera_mode,
                initial_speaker_enabled: pending.initial_speaker_enabled,
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
                initial_camera_mode: String::new(),
                initial_speaker_enabled: true,
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
                    ui(UiEvent::SetSessionTitle(hostname));
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
    frame_slot: FrameSlot,
    audio_player: Option<Arc<AudioPlayer>>,
) {
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
        receiver.set_read_timeout(Some(UDP_READ_TIMEOUT)).ok();

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

        let mut buffer = vec![0u8; UDP_HEADER_LEN + UDP_CHUNK_PAYLOAD + TAG_LEN];
        let mut video_frames: HashMap<u32, FrameAssembly> = HashMap::new();
        let mut audio_frames: HashMap<u32, AudioAssembly> = HashMap::new();
        let mut av_sync = AvSyncState::default();
        let mut stats = StreamStats::default();
        let mut last_packet = Instant::now();
        let mut decoder: Option<VideoDecoder> = None;
        let mut current_codec_id: Option<u8> = None;

        while !stop.load(Ordering::Relaxed) {
            match receiver.recv(&mut buffer) {
                Ok(size) => {
                    last_packet = Instant::now();
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
                        continue;
                    };

                    stats.note_packet(size);
                    if (flags & FLAG_AUDIO) != 0 {
                        let assembly = audio_frames
                            .entry(frame_id)
                            .or_insert_with(|| AudioAssembly::new(total_data));
                        assembly.add_chunk(chunk_idx as usize, plaintext, payload_len);
                        if assembly.is_complete() {
                            if let Some(pcm) = assembly.reassemble() {
                                if let Some(ref player) = audio_player {
                                    player.enqueue(&pcm);
                                }
                            }
                            audio_frames.remove(&frame_id);
                        }
                    } else {
                        let codec_label = if codec_id == 0x01 { "H.265" } else { "H.264" };
                        ui(UiEvent::SetCodecLabel(format!("{codec_label} · UDP live")));
                        let assembly = video_frames.entry(frame_id).or_insert_with(|| {
                            FrameAssembly::new(
                                total_data,
                                total_parity,
                                (flags & FLAG_IDR) != 0,
                                timestamp_ms,
                            )
                        });
                        assembly.add_chunk(chunk_idx as usize, plaintext, payload_len);

                        if assembly.is_complete() {
                            if let Some(annex_b) = assembly.reassemble() {
                                // Ensure decoder matches the current codec
                                let need_new_decoder =
                                    current_codec_id.map_or(true, |c| c != codec_id);
                                if need_new_decoder {
                                    let codec = CodecId::from_transport_id(codec_id);
                                    match VideoDecoder::new(codec) {
                                        Ok(dec) => {
                                            let hw_label = match codec {
                                                CodecId::H264 => "H.264",
                                                CodecId::H265 => "H.265",
                                            };
                                            ui(UiEvent::SetCodecLabel(format!(
                                                "{hw_label} · HW decode · UDP live"
                                            )));
                                            decoder = Some(dec);
                                            current_codec_id = Some(codec_id);
                                        }
                                        Err(e) => {
                                            eprintln!("[backend] decoder init failed: {e:#}");
                                            decoder = None;
                                            current_codec_id = None;
                                        }
                                    }
                                }

                                if let Some(dec) = &mut decoder {
                                    match dec.decode(&annex_b) {
                                        Ok(decoded_frames) => {
                                            for df in decoded_frames {
                                                // Push to shared frame slot for paint()
                                                if let Ok(mut slot) = frame_slot.lock() {
                                                    ui(UiEvent::SetResolutionLabel(format!(
                                                        "{} x {}",
                                                        df.width, df.height
                                                    )));
                                                    *slot = Some(RawFrame {
                                                        width: df.width,
                                                        height: df.height,
                                                        rgba: df.rgba,
                                                    });
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            eprintln!("[backend] decode error: {e:#}");
                                            stats.dropped_frames =
                                                stats.dropped_frames.saturating_add(1);
                                            let _ = control.send_pli();
                                        }
                                    }
                                }

                                av_sync.update_video(timestamp_ms);
                                stats.note_frame();
                            } else {
                                stats.dropped_frames = stats.dropped_frames.saturating_add(1);
                                let _ = control.send_pli();
                            }
                            video_frames.remove(&frame_id);
                        }
                    }

                    prune_expired_frames(&mut video_frames, &mut stats, &control);
                    if let Some(update) = stats.sample(av_sync.estimate_transport_latency_ms()) {
                        ui(UiEvent::SetStats {
                            fps: update.fps,
                            bitrate_mbps: update.bitrate_mbps,
                            latency_ms: update.latency_ms,
                            dropped_frames: stats.dropped_frames,
                        });
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if last_packet.elapsed() >= UDP_DATA_TIMEOUT {
                        let _ = tx.send(BackendCommand::SessionClosed {
                            session_id,
                            reason: "UDP media timed out. The daemon stopped sending data.".into(),
                        });
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(BackendCommand::SessionClosed {
                        session_id,
                        reason: format!("UDP receive failed: {error}"),
                    });
                    return;
                }
            }
        }
    });
}

fn prune_expired_frames(
    frames: &mut HashMap<u32, FrameAssembly>,
    stats: &mut StreamStats,
    control: &ControlSender,
) {
    let mut expired = Vec::new();
    for (frame_id, assembly) in frames.iter() {
        if assembly.created_at.elapsed() > FRAME_TIMEOUT {
            expired.push(*frame_id);
        }
    }

    if !expired.is_empty() {
        for frame_id in expired {
            frames.remove(&frame_id);
            stats.dropped_frames = stats.dropped_frames.saturating_add(1);
        }
        let _ = control.send_pli();
    }
}

pub struct ControlSender {
    inner: Mutex<ControlWriter>,
}

struct ControlWriter {
    stream: TcpStream,
    cipher: SessionCipher,
    seq: u32,
}

impl ControlSender {
    fn new(stream: TcpStream, session_key: [u8; 32]) -> Result<Self> {
        Ok(Self {
            inner: Mutex::new(ControlWriter {
                stream,
                cipher: SessionCipher::new(&session_key)?,
                seq: 0,
            }),
        })
    }

    pub fn send_payload(&self, payload: &[u8]) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let seq = inner.seq;
        inner.seq = inner.seq.wrapping_add(1);

        let aad = seq.to_be_bytes();
        let nonce = nonce_control_client(seq);
        let encrypted = inner.cipher.encrypt_vec(&nonce, &aad, payload)?;
        let body_len = (aad.len() + encrypted.len()) as u32;

        inner.stream.write_all(&body_len.to_be_bytes())?;
        inner.stream.write_all(&aad)?;
        inner.stream.write_all(&encrypted)?;
        inner.stream.flush().ok();
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
    seq: Mutex<u32>,
}

impl UdpSender {
    pub fn new(server_addr: SocketAddr, session_key: [u8; 32]) -> Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", 0)).context("bind UDP client socket")?;
        socket
            .connect(server_addr)
            .with_context(|| format!("connect UDP socket to {server_addr}"))?;
        Ok(Self {
            socket,
            session_key,
            seq: Mutex::new(0),
        })
    }

    pub fn send_encrypted(&self, payload: &[u8]) -> Result<()> {
        let cipher = SessionCipher::new(&self.session_key)?;
        let mut seq = self.seq.lock().unwrap();
        let aad = seq.to_be_bytes();
        let nonce = nonce_client(*seq);
        let encrypted = cipher.encrypt_vec(&nonce, &aad, payload)?;
        *seq = seq.wrapping_add(1);

        let mut packet = Vec::with_capacity(4 + encrypted.len());
        packet.extend_from_slice(&aad);
        packet.extend_from_slice(&encrypted);
        self.socket
            .send(&packet)
            .context("send encrypted UDP packet")?;
        Ok(())
    }
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

#[derive(Default)]
struct AvSyncState {
    last_video_ts: u32,
    last_video_wall: Option<Instant>,
}

impl AvSyncState {
    fn update_video(&mut self, timestamp_ms: u32) {
        self.last_video_ts = timestamp_ms;
        self.last_video_wall = Some(Instant::now());
    }

    fn estimate_transport_latency_ms(&self) -> u32 {
        let Some(wall) = self.last_video_wall else {
            return 0;
        };
        let elapsed = wall.elapsed().as_millis() as u32;
        elapsed.min(999)
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
    shards: HashMap<usize, Vec<u8>>,
    actual_lengths: HashMap<usize, usize>,
}

impl FrameAssembly {
    fn new(total_data: usize, total_parity: usize, _is_idr: bool, timestamp_ms: u32) -> Self {
        let _ = timestamp_ms;
        Self {
            total_data,
            total_parity,
            created_at: Instant::now(),
            shards: HashMap::new(),
            actual_lengths: HashMap::new(),
        }
    }

    fn add_chunk(&mut self, index: usize, payload: &[u8], actual_len: usize) {
        self.shards.entry(index).or_insert_with(|| payload.to_vec());
        self.actual_lengths.entry(index).or_insert(actual_len);
    }

    fn is_complete(&self) -> bool {
        if self.total_parity == 0 {
            return (0..self.total_data).all(|index| self.shards.contains_key(&index));
        }
        self.shards.len() >= self.total_data
    }

    fn reassemble(&self) -> Option<Vec<u8>> {
        if (0..self.total_data).all(|index| self.shards.contains_key(&index)) {
            return Some(self.assemble_present_data());
        }
        if self.total_parity == 0 || self.shards.len() < self.total_data {
            return None;
        }

        let total_shards = self.total_data + self.total_parity;
        let mut shards = vec![None; total_shards];
        for (index, shard) in &self.shards {
            let mut data = shard.clone();
            if data.len() < UDP_CHUNK_PAYLOAD {
                data.resize(UDP_CHUNK_PAYLOAD, 0);
            }
            shards[*index] = Some(data);
        }

        let rs = ReedSolomon::new(self.total_data, self.total_parity).ok()?;
        rs.reconstruct(&mut shards).ok()?;

        let mut out = Vec::new();
        for index in 0..self.total_data {
            let shard = shards[index].as_ref()?;
            let actual_len = self
                .actual_lengths
                .get(&index)
                .copied()
                .unwrap_or(shard.len());
            out.extend_from_slice(&shard[..actual_len.min(shard.len())]);
        }
        Some(out)
    }

    fn assemble_present_data(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for index in 0..self.total_data {
            if let Some(shard) = self.shards.get(&index) {
                let actual_len = self
                    .actual_lengths
                    .get(&index)
                    .copied()
                    .unwrap_or(shard.len());
                out.extend_from_slice(&shard[..actual_len.min(shard.len())]);
            }
        }
        out
    }
}

struct AudioAssembly {
    total_data: usize,
    shards: HashMap<usize, Vec<u8>>,
}

impl AudioAssembly {
    fn new(total_data: usize) -> Self {
        Self {
            total_data,
            shards: HashMap::new(),
        }
    }

    fn add_chunk(&mut self, index: usize, payload: &[u8], _actual_len: usize) {
        self.shards.entry(index).or_insert_with(|| payload.to_vec());
    }

    fn is_complete(&self) -> bool {
        self.shards.len() >= self.total_data
    }

    fn reassemble(&self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut out = Vec::new();
        for i in 0..self.total_data {
            if let Some(shard) = self.shards.get(&i) {
                out.extend_from_slice(shard);
            } else {
                return None;
            }
        }
        Some(out)
    }
}

struct EndpointInfo {
    server_addr: SocketAddr,
    display_host: String,
    endpoint_key: String,
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
}

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
