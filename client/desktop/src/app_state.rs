use std::cell::RefCell;
use std::sync::Arc;

use qmetaobject::prelude::*;
use qmetaobject::{queued_callback, QPointer};

use crate::backend::{BackendHandle, ControlSender, UiEvent};

const GLOBAL_KEY_SHORTCUT_OVERRIDE: i32 = 0;
const GLOBAL_KEY_PRESS: i32 = 1;
const GLOBAL_KEY_RELEASE: i32 = 2;

type GlobalKeyDispatcher = Box<dyn Fn(i32, QString, i32, i32) -> bool>;

thread_local! {
    static GLOBAL_KEY_DISPATCHER: RefCell<Option<GlobalKeyDispatcher>> = RefCell::new(None);
}

pub fn dispatch_global_key_event(qt_key: i32, text: QString, modifiers: i32, phase: i32) -> bool {
    GLOBAL_KEY_DISPATCHER.with(|dispatcher| {
        dispatcher
            .borrow()
            .as_ref()
            .map(|f| f(qt_key, text, modifiers, phase))
            .unwrap_or(false)
    })
}

fn should_log_key(qt_key: i32, text: &str) -> bool {
    matches!(
        qt_key,
        0x01000003
            | 0x01000004
            | 0x01000005
            | 0x01000001
            | 0x01000000
            | 0x01000012
            | 0x01000013
            | 0x01000014
            | 0x01000015
    ) || text.eq_ignore_ascii_case("s")
}

#[derive(QObject)]
pub struct AppState {
    pub base: qt_base_class!(trait QObject),

    pub connected: qt_property!(bool; NOTIFY connected_changed),
    pub connected_changed: qt_signal!(),

    pub connecting: qt_property!(bool; NOTIFY connecting_changed),
    pub connecting_changed: qt_signal!(),

    pub speaker_enabled: qt_property!(bool; NOTIFY speaker_enabled_changed),
    pub speaker_enabled_changed: qt_signal!(),

    pub mic_enabled: qt_property!(bool; NOTIFY mic_enabled_changed),
    pub mic_enabled_changed: qt_signal!(),

    pub camera_enabled: qt_property!(bool; NOTIFY camera_enabled_changed),
    pub camera_enabled_changed: qt_signal!(),

    pub keyboard_enabled: qt_property!(bool; NOTIFY keyboard_enabled_changed),
    pub keyboard_enabled_changed: qt_signal!(),
    pub mouse_grabbed: qt_property!(bool; NOTIFY mouse_grabbed_changed),
    pub mouse_grabbed_changed: qt_signal!(),

    pub info_visible: qt_property!(bool; NOTIFY info_visible_changed),
    pub info_visible_changed: qt_signal!(),

    pub session_title: qt_property!(QString; NOTIFY session_title_changed),
    pub session_title_changed: qt_signal!(),

    pub status_text: qt_property!(QString; NOTIFY status_text_changed),
    pub status_text_changed: qt_signal!(),

    pub transport_label: qt_property!(QString; NOTIFY transport_label_changed),
    pub transport_label_changed: qt_signal!(),

    pub codec_label: qt_property!(QString; NOTIFY codec_label_changed),
    pub codec_label_changed: qt_signal!(),

    pub resolution_label: qt_property!(QString; NOTIFY resolution_label_changed),
    pub resolution_label_changed: qt_signal!(),

    pub selected_camera_mode: qt_property!(QString; NOTIFY selected_camera_mode_changed),
    pub selected_camera_mode_changed: qt_signal!(),

    pub pin_prompt_text: qt_property!(QString; NOTIFY pin_prompt_text_changed),
    pub pin_prompt_text_changed: qt_signal!(),

    pub pin_prompt_visible: qt_property!(bool; NOTIFY pin_prompt_visible_changed),
    pub pin_prompt_visible_changed: qt_signal!(),

    pub connections_json: qt_property!(QString; NOTIFY connections_json_changed),
    pub connections_json_changed: qt_signal!(),

    pub fps: qt_property!(u32; NOTIFY fps_changed),
    pub fps_changed: qt_signal!(),

    pub latency_ms: qt_property!(u32; NOTIFY latency_ms_changed),
    pub latency_ms_changed: qt_signal!(),

    pub bitrate_mbps: qt_property!(f32; NOTIFY bitrate_mbps_changed),
    pub bitrate_mbps_changed: qt_signal!(),

    pub dropped_frames: qt_property!(u32; NOTIFY dropped_frames_changed),
    pub dropped_frames_changed: qt_signal!(),

    pub connect_to_host: qt_method!(
        fn connect_to_host(&mut self, host: QString) {
            self.ensure_backend();
            let trimmed = host.to_string().trim().to_owned();
            if trimmed.is_empty() {
                self.set_status("Enter a hostname or IP to start a Screx session.");
                self.set_connecting(false);
                return;
            }

            self.set_pin_prompt_text("");
            self.set_connecting(true);
            self.set_connected(false);
            self.set_status(&format!("Connecting to {trimmed}..."));

            if let Some(backend) = self.backend.clone() {
                backend.connect(trimmed, self.speaker_enabled);
            } else {
                self.set_status("Desktop backend is not ready yet.");
            }
        }
    ),

    pub connect_recent: qt_method!(
        fn connect_recent(&mut self, host: QString, port: i32) {
            let h = host.to_string();
            let endpoint = if port == 42069 {
                h.clone()
            } else {
                format!("{}:{}", h, port)
            };
            self.connect_to_host(endpoint.into());
        }
    ),

    pub toggle_pinned_connection: qt_method!(
        fn toggle_pinned_connection(&mut self, host: QString, port: i32) {
            self.ensure_backend();
            if let Some(backend) = self.backend.clone() {
                backend.toggle_pinned(host.to_string(), port as u16);
            }
        }
    ),

    pub delete_connection: qt_method!(
        fn delete_connection(&mut self, host: QString, port: i32) {
            self.ensure_backend();
            if let Some(backend) = self.backend.clone() {
                backend.delete_connection(host.to_string(), port as u16);
            }
        }
    ),

    pub clear_recent_connections: qt_method!(
        fn clear_recent_connections(&mut self) {
            self.ensure_backend();
            if let Some(backend) = self.backend.clone() {
                backend.clear_recent_connections();
            }
        }
    ),

    pub submit_pin: qt_method!(
        fn submit_pin(&mut self, pin: QString) {
            self.ensure_backend();
            let trimmed = pin.to_string().trim().to_owned();
            if trimmed.is_empty() {
                self.set_status("Enter the 6-digit PIN from the daemon terminal.");
                return;
            }

            self.set_connecting(true);
            self.set_status("Finishing pairing handshake...");

            if let Some(backend) = self.backend.clone() {
                backend.submit_pin(trimmed);
            }
        }
    ),

    pub disconnect_session: qt_method!(
        fn disconnect_session(&mut self) {
            self.ensure_backend();
            if let Some(backend) = self.backend.clone() {
                backend.disconnect();
            }
        }
    ),

    pub toggle_speaker: qt_method!(
        fn toggle_speaker(&mut self) {
            self.ensure_backend();
            self.speaker_enabled = !self.speaker_enabled;
            self.speaker_enabled_changed();
            if let Some(backend) = self.backend.clone() {
                backend.set_speaker(self.speaker_enabled);
            }
        }
    ),

    pub toggle_mic: qt_method!(
        fn toggle_mic(&mut self) {
            self.ensure_backend();
            self.mic_enabled = !self.mic_enabled;
            self.mic_enabled_changed();
            if let Some(backend) = self.backend.clone() {
                backend.set_mic(self.mic_enabled);
            }
        }
    ),

    pub toggle_camera: qt_method!(
        fn toggle_camera(&mut self) {
            self.ensure_backend();
            self.camera_enabled = !self.camera_enabled;
            self.camera_enabled_changed();
            if let Some(backend) = self.backend.clone() {
                backend.set_camera(self.camera_enabled);
            }
        }
    ),

    pub toggle_keyboard: qt_method!(
        fn toggle_keyboard(&mut self) {
            self.ensure_backend();
            self.keyboard_enabled = !self.keyboard_enabled;
            self.keyboard_enabled_changed();
            if let Some(backend) = self.backend.clone() {
                backend.set_keyboard(self.keyboard_enabled);
            }
        }
    ),

    pub toggle_info: qt_method!(
        fn toggle_info(&mut self) {
            self.info_visible = !self.info_visible;
            self.info_visible_changed();
        }
    ),

    pub select_camera_mode: qt_method!(
        fn select_camera_mode(&mut self, mode: QString) {
            self.ensure_backend();
            self.selected_camera_mode = mode;
            self.selected_camera_mode_changed();
            if let Some(backend) = self.backend.clone() {
                backend.set_camera_mode(self.selected_camera_mode.to_string());
            }
        }
    ),

    pub send_key_event: qt_method!(
        fn send_key_event(&mut self, qt_key: i32, text: QString, modifiers: i32, pressed: bool) {
            self.ensure_backend();
            if !self.keyboard_enabled || !self.connected {
                return;
            }
            if let Some(ref control) = self.direct_control {
                let _ = crate::input::send_qt_key_action(
                    control,
                    qt_key,
                    &text.to_string(),
                    modifiers,
                    pressed,
                );
            } else if let Some(backend) = self.backend.clone() {
                backend.send_key_action(qt_key, text.to_string(), modifiers, pressed);
            }
        }
    ),

    pub send_mouse_move: qt_method!(
        fn send_mouse_move(&mut self, norm_x: f32, norm_y: f32) {
            if !self.connected {
                return;
            }
            let x = (norm_x.clamp(0.0, 1.0) * 65535.0) as u16;
            let y = (norm_y.clamp(0.0, 1.0) * 65535.0) as u16;
            if let Some(ref control) = self.direct_control {
                let _ = crate::input::send_mouse_abs(control, x, y);
            } else if let Some(ref backend) = self.backend {
                backend.send_mouse_move(x, y);
            }
        }
    ),

    /// QML sends raw pixel coordinates plus the content rect — normalization
    /// happens here in Rust, avoiding the JS {x,y} object allocation per move.
    pub send_mouse_move_raw: qt_method!(
        fn send_mouse_move_raw(&mut self, px: f64, py: f64, cx: f64, cy: f64, cw: f64, ch: f64) {
            if !self.connected || cw <= 0.0 || ch <= 0.0 {
                return;
            }
            let norm_x = ((px - cx) / cw).clamp(0.0, 1.0);
            let norm_y = ((py - cy) / ch).clamp(0.0, 1.0);
            let x = (norm_x * 65535.0) as u16;
            let y = (norm_y * 65535.0) as u16;
            if let Some(ref control) = self.direct_control {
                let _ = crate::input::send_mouse_abs(control, x, y);
            } else if let Some(ref backend) = self.backend {
                backend.send_mouse_move(x, y);
            }
        }
    ),

    pub send_mouse_button: qt_method!(
        fn send_mouse_button(&mut self, button: i32, pressed: bool) {
            if !self.connected {
                return;
            }
            // Map Qt button constants to daemon mouse button codes:
            // daemon expects 0=left, 1=right, 2=middle.
            let btn = match button {
                1 => 0u8, // Left
                2 => 1u8, // Right
                4 => 2u8, // Middle
                _ => return,
            };
            if let Some(ref control) = self.direct_control {
                let _ = crate::input::send_mouse_button(control, btn, pressed);
            } else if let Some(ref backend) = self.backend {
                backend.send_mouse_button(btn, pressed);
            }
        }
    ),

    pub send_mouse_scroll: qt_method!(
        fn send_mouse_scroll(&mut self, dy: f32) {
            if !self.connected {
                return;
            }
            // Accumulate fractional scroll like iPad does
            self.scroll_accumulator += dy / 120.0;
            let whole_steps = self.scroll_accumulator as i32;
            if whole_steps == 0 {
                return;
            }
            self.scroll_accumulator -= whole_steps as f32;
            let delta = (whole_steps.max(-32).min(32)) as i16;
            if let Some(ref control) = self.direct_control {
                let _ = crate::input::send_mouse_scroll(control, delta);
            } else if let Some(ref backend) = self.backend {
                backend.send_mouse_scroll(delta);
            }
        }
    ),

    pub warp_cursor: qt_method!(
        fn warp_cursor(&mut self, x: f32, y: f32) {
            crate::warp_cursor_to(x as i32, y as i32);
        }
    ),

    backend: Option<BackendHandle>,
    direct_control: Option<Arc<ControlSender>>,
    scroll_accumulator: f32,
}

impl AppState {
    fn ensure_backend(&mut self) {
        if self.backend.is_some() {
            return;
        }
        let qptr = QPointer::from(&*self);
        let event_qptr = qptr.clone();
        let apply_event = queued_callback(move |event| {
            event_qptr.as_pinned().map(|pinned| {
                pinned.borrow_mut().apply_ui_event(event);
            });
        });
        let backend = crate::backend::spawn_backend(
            move |event| {
                apply_event(event);
            },
            crate::video_surface::global_frame_slot_clone(),
        );
        GLOBAL_KEY_DISPATCHER.with(|dispatcher| {
            let qptr = qptr.clone();
            *dispatcher.borrow_mut() = Some(Box::new(move |qt_key, text, modifiers, phase| {
                qptr.as_pinned()
                    .map(|pinned| {
                        pinned
                            .borrow_mut()
                            .handle_global_key_event(qt_key, text, modifiers, phase)
                    })
                    .unwrap_or(false)
            }));
        });
        self.backend = Some(backend.clone());
        backend.load_connections();
    }

    fn handle_global_key_event(
        &mut self,
        qt_key: i32,
        text: QString,
        modifiers: i32,
        phase: i32,
    ) -> bool {
        let text_string = text.to_string();
        if should_log_key(qt_key, &text_string) {
            println!(
                "[desktop/key/app] phase={} key=0x{:x} text={:?} mods=0x{:x} connected={} keyboard_enabled={} pin_prompt_visible={}",
                phase,
                qt_key,
                text_string,
                modifiers,
                self.connected,
                self.keyboard_enabled,
                self.pin_prompt_visible
            );
        }
        if !self.connected || !self.keyboard_enabled || self.pin_prompt_visible {
            return false;
        }

        match phase {
            GLOBAL_KEY_SHORTCUT_OVERRIDE => true,
            GLOBAL_KEY_PRESS | GLOBAL_KEY_RELEASE => {
                let pressed = phase == GLOBAL_KEY_PRESS;
                if let Some(ref control) = self.direct_control {
                    let _ = crate::input::send_qt_key_action(
                        control,
                        qt_key,
                        &text_string,
                        modifiers,
                        pressed,
                    );
                    true
                } else if let Some(backend) = self.backend.clone() {
                    backend.send_key_action(qt_key, text_string, modifiers, pressed);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    pub fn apply_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::SetConnecting(value) => self.set_connecting(value),
            UiEvent::SetConnected(value) => self.set_connected(value),
            UiEvent::SetSessionTitle(value) => self.set_session_title(&value),
            UiEvent::SetStatus(value) => self.set_status(&value),
            UiEvent::SetTransportLabel(value) => self.set_transport_label(&value),
            UiEvent::SetCodecLabel(value) => self.set_codec_label(&value),
            UiEvent::SetResolutionLabel(value) => self.set_resolution_label(&value),
            UiEvent::SetStats {
                fps,
                bitrate_mbps,
                latency_ms,
                dropped_frames,
            } => {
                self.fps = fps;
                self.bitrate_mbps = bitrate_mbps;
                self.latency_ms = latency_ms;
                self.dropped_frames = dropped_frames;
                self.fps_changed();
                self.bitrate_mbps_changed();
                self.latency_ms_changed();
                self.dropped_frames_changed();
            }
            UiEvent::PinRequired(value) => self.set_pin_prompt_text(&value),
            UiEvent::ClearPinPrompt => self.set_pin_prompt_text(""),
            UiEvent::SetCameraEnabled(value) => {
                self.camera_enabled = value;
                self.camera_enabled_changed();
            }
            UiEvent::SetConnections(conns) => {
                #[derive(serde::Serialize)]
                struct Entry {
                    host: String,
                    port: u16,
                    name: String,
                    pinned: bool,
                }
                let entries: Vec<Entry> = conns
                    .into_iter()
                    .map(|c| Entry {
                        host: c.host,
                        port: c.port,
                        name: c.name,
                        pinned: c.pinned,
                    })
                    .collect();
                let json = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".into());
                self.connections_json = QString::from(json.as_str());
                self.connections_json_changed();
            }
            UiEvent::SetDirectControl(ctrl) => {
                self.direct_control = ctrl;
            }
        }
    }

    fn set_connected(&mut self, value: bool) {
        self.connected = value;
        self.connected_changed();
    }

    fn set_connecting(&mut self, value: bool) {
        self.connecting = value;
        self.connecting_changed();
    }

    fn set_session_title(&mut self, value: &str) {
        self.session_title = QString::from(value);
        self.session_title_changed();
    }

    fn set_status(&mut self, value: &str) {
        self.status_text = QString::from(value);
        self.status_text_changed();
    }

    fn set_transport_label(&mut self, value: &str) {
        self.transport_label = QString::from(value);
        self.transport_label_changed();
    }

    fn set_codec_label(&mut self, value: &str) {
        self.codec_label = QString::from(value);
        self.codec_label_changed();
    }

    fn set_resolution_label(&mut self, value: &str) {
        self.resolution_label = QString::from(value);
        self.resolution_label_changed();
    }

    fn set_pin_prompt_text(&mut self, value: &str) {
        self.pin_prompt_text = QString::from(value);
        self.pin_prompt_text_changed();
        self.pin_prompt_visible = !value.is_empty();
        self.pin_prompt_visible_changed();
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            base: Default::default(),
            connected: false,
            connected_changed: Default::default(),
            connecting: false,
            connecting_changed: Default::default(),
            speaker_enabled: true,
            speaker_enabled_changed: Default::default(),
            mic_enabled: false,
            mic_enabled_changed: Default::default(),
            camera_enabled: false,
            camera_enabled_changed: Default::default(),
            keyboard_enabled: true,
            mouse_grabbed: false,
            mouse_grabbed_changed: Default::default(),
            keyboard_enabled_changed: Default::default(),
            info_visible: true,
            info_visible_changed: Default::default(),
            session_title: QString::from("No active session"),
            session_title_changed: Default::default(),
            status_text: QString::from("Enter a daemon host to connect."),
            status_text_changed: Default::default(),
            transport_label: QString::from("Network"),
            transport_label_changed: Default::default(),
            codec_label: QString::from("Waiting for stream"),
            codec_label_changed: Default::default(),
            resolution_label: QString::from("Pending"),
            resolution_label_changed: Default::default(),
            selected_camera_mode: QString::from("Auto · 1280 x 720 @ 30"),
            selected_camera_mode_changed: Default::default(),
            pin_prompt_text: QString::from(""),
            pin_prompt_text_changed: Default::default(),
            pin_prompt_visible: false,
            connections_json: QString::from(crate::backend::load_connections_json().as_str()),
            connections_json_changed: Default::default(),
            connect_recent: Default::default(),
            toggle_pinned_connection: Default::default(),
            delete_connection: Default::default(),
            clear_recent_connections: Default::default(),
            pin_prompt_visible_changed: Default::default(),
            fps: 0,
            fps_changed: Default::default(),
            latency_ms: 0,
            latency_ms_changed: Default::default(),
            bitrate_mbps: 0.0,
            bitrate_mbps_changed: Default::default(),
            dropped_frames: 0,
            dropped_frames_changed: Default::default(),
            connect_to_host: Default::default(),
            submit_pin: Default::default(),
            disconnect_session: Default::default(),
            toggle_speaker: Default::default(),
            toggle_mic: Default::default(),
            toggle_camera: Default::default(),
            toggle_keyboard: Default::default(),
            toggle_info: Default::default(),
            select_camera_mode: Default::default(),
            send_key_event: Default::default(),
            send_mouse_move: Default::default(),
            send_mouse_move_raw: Default::default(),
            send_mouse_button: Default::default(),
            send_mouse_scroll: Default::default(),
            warp_cursor: Default::default(),
            backend: None,
            direct_control: None,
            scroll_accumulator: 0.0,
        }
    }
}
