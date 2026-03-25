use qmetaobject::prelude::*;
use qmetaobject::{queued_callback, QPointer};

use crate::backend::{BackendHandle, UiEvent};

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
                self.set_connecting(false);
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
        fn send_key_event(&mut self, qt_key: i32, pressed: bool) {
            self.ensure_backend();
            if !self.keyboard_enabled || !self.connected {
                return;
            }
            if let Some(hid) = crate::input::qt_key_to_hid(qt_key) {
                if let Some(backend) = self.backend.clone() {
                    backend.send_key_event(hid, pressed);
                }
            }
        }
    ),

    pub send_mouse_move: qt_method!(
        fn send_mouse_move(&mut self, norm_x: f32, norm_y: f32) {
            self.ensure_backend();
            if !self.connected {
                return;
            }
            let x = (norm_x.clamp(0.0, 1.0) * 65535.0) as u16;
            let y = (norm_y.clamp(0.0, 1.0) * 65535.0) as u16;
            if let Some(backend) = self.backend.clone() {
                backend.send_mouse_move(x, y);
            }
        }
    ),

    pub send_mouse_button: qt_method!(
        fn send_mouse_button(&mut self, button: i32, pressed: bool) {
            self.ensure_backend();
            if !self.connected {
                return;
            }
            // Qt button constants: 1=Left, 2=Right, 4=Middle
            let btn = match button {
                1 => 1u8, // Left
                2 => 2u8, // Right
                4 => 3u8, // Middle
                _ => return,
            };
            if let Some(backend) = self.backend.clone() {
                backend.send_mouse_button(btn, pressed);
            }
        }
    ),

    pub send_mouse_scroll: qt_method!(
        fn send_mouse_scroll(&mut self, dy: f32) {
            self.ensure_backend();
            if !self.connected {
                return;
            }
            let delta = (dy / 120.0 * 3.0) as i16; // normalize wheel delta
            if delta != 0 {
                if let Some(backend) = self.backend.clone() {
                    backend.send_mouse_scroll(delta);
                }
            }
        }
    ),

    backend: Option<BackendHandle>,
}

impl AppState {
    fn ensure_backend(&mut self) {
        if self.backend.is_some() {
            return;
        }
        let qptr = QPointer::from(&*self);
        let apply_event = queued_callback(move |event| {
            qptr.as_pinned().map(|pinned| {
                pinned.borrow_mut().apply_ui_event(event);
            });
        });
        let backend = crate::backend::spawn_backend(
            move |event| {
                apply_event(event);
            },
            crate::video_surface::global_frame_slot_clone(),
        );
        self.backend = Some(backend);
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
            send_mouse_button: Default::default(),
            send_mouse_scroll: Default::default(),
            backend: None,
        }
    }
}
