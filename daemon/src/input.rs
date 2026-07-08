use std::sync::atomic::AtomicBool;
use std::sync::Arc;

// Touch event types matching iPad protocol
pub const TOUCH_DOWN: u8 = 0;
pub const TOUCH_MOVE: u8 = 1;
pub const TOUCH_UP: u8 = 2;

pub const KEY_TYPE_TEXT: u8 = 0x01;
pub const KEY_TYPE_SPECIAL: u8 = 0x02;
pub const KEY_TYPE_COMBO: u8 = 0x04;

pub const SPECIAL_BACKSPACE: u8 = 0x01;

/// A single touch contact.
#[derive(Debug, Clone, Copy)]
pub struct TouchContact {
    pub slot: u8,
    pub event_type: u8,
    pub x: u16,
    pub y: u16,
}

/// Mouse event variants.
#[derive(Debug, Clone, Copy)]
pub enum MouseEvent {
    MoveRelative { dx: i16, dy: i16 },
    MoveAbsolute { x: u16, y: u16 },
    Button { btn: u8, state: u8 },
    Scroll { dy: i16 },
}

/// Gamepad button bits (same layout as Linux evdev mapping used by iPad).
pub const GPAD_BTN_SOUTH: u16 = 0x0001;
pub const GPAD_BTN_EAST: u16 = 0x0002;
pub const GPAD_BTN_WEST: u16 = 0x0004;
pub const GPAD_BTN_NORTH: u16 = 0x0008;
pub const GPAD_BTN_TL: u16 = 0x0010;
pub const GPAD_BTN_TR: u16 = 0x0020;
pub const GPAD_BTN_THUMBL: u16 = 0x0040;
pub const GPAD_BTN_THUMBR: u16 = 0x0080;
pub const GPAD_BTN_SELECT: u16 = 0x0100;
pub const GPAD_BTN_START: u16 = 0x0200;
pub const GPAD_BTN_MODE: u16 = 0x0400;

/// Normalized gamepad state.
#[derive(Debug, Clone, Copy)]
pub struct GamepadState {
    pub buttons: u16,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u16,
    pub rt: u16,
    pub hat_x: i8,
    pub hat_y: i8,
}

/// Cheap capability probe for virtual gamepad passthrough. Returns the max
/// number of simultaneous controllers supported, or `None` if this daemon
/// can't do gamepad passthrough at all right now.
pub fn probe_gamepad_max_controllers() -> Option<u8> {
    #[cfg(target_os = "linux")]
    {
        // uinput gamepad creation only fails if /dev/uinput isn't openable;
        // treat presence of the device node as the probe (mirrors
        // `VirtualGamepad::new` in platform::linux::uinput). The Linux
        // backend keys pads in a HashMap with no fixed cap; 4 matches the
        // controller_id range (0..4) the rest of the protocol assumes (see
        // the disconnect cleanup loop in main.rs).
        if std::path::Path::new("/dev/uinput").exists() {
            Some(4)
        } else {
            None
        }
    }
    #[cfg(target_os = "windows")]
    {
        if crate::platform::windows::vigem::probe_available() {
            Some(4)
        } else {
            None
        }
    }
}

/// Backend-specific input injection.
pub trait InputBackend: Send {
    /// Tells the backend the current session's stream resolution and, where
    /// meaningful, where the captured output sits on screen — (left, top,
    /// width, height). Wire touch/mouse coordinates are always 0..width/
    /// 0..height in *negotiated session stream* space, which can differ from
    /// the backend's underlying input device geometry (e.g. a client
    /// requesting 1920x1080 while the daemon's virtual devices are sized to
    /// a larger startup ceiling). Windows injects via absolute
    /// virtual-desktop coordinates, so it uses (left, top, width, height) to
    /// translate frame-local coordinates onto the desktop. Linux ignores
    /// left/top (its virtual touchscreen is a fixed-size device that
    /// gsettings maps directly onto the captured EVDI output, so there is no
    /// desktop-absolute translation to do) and instead scales incoming
    /// coordinates from session-resolution space onto its fixed-size virtual
    /// touchscreen's ABS range.
    fn set_target_rect(&mut self, _left: i32, _top: i32, _width: u32, _height: u32) {}

    fn touch(&mut self, contacts: &[TouchContact]) -> anyhow::Result<()>;
    fn key_text(&mut self, text: &str) -> anyhow::Result<()>;
    fn key_special(&mut self, code: u8, modifiers: u8) -> anyhow::Result<()>;
    fn key_text_with_modifiers(&mut self, text: &str, modifiers: u8) -> anyhow::Result<()>;
    fn key_raw_hid(&mut self, usage: u8, pressed: bool) -> anyhow::Result<()>;
    fn mouse(&mut self, ev: MouseEvent) -> anyhow::Result<()>;
    fn gamepad_attach(&mut self, id: u8) -> anyhow::Result<()>;
    fn gamepad_detach(&mut self, id: u8);
    fn gamepad_state(&mut self, id: u8, st: &GamepadState) -> anyhow::Result<()>;
}

/// Event types the keyboard worker can execute.
#[derive(Debug, Clone)]
pub enum KeyboardEvent {
    Text(String),
    Special(u8),
    Combo { mods: u8, text: String },
    SpecialCombo { mods: u8, code: u8 },
    Raw { usage: u16, state: i32 },
}

/// Owned handle to a background thread that executes keyboard events.
pub struct KeyboardWorker {
    sender: std::sync::mpsc::SyncSender<KeyboardEvent>,
}

impl KeyboardWorker {
    /// Spawn a thread that executes keyboard events sequentially against a
    /// shared backend.
    pub fn new(backend: Arc<std::sync::Mutex<dyn InputBackend>>) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<KeyboardEvent>(32);
        std::thread::Builder::new()
            .name("keyboard-input".into())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    if let Ok(mut backend) = backend.lock() {
                        dispatch_event(&mut *backend, &event);
                    }
                }
            })
            .ok();
        Self { sender }
    }

    /// Send an event to the keyboard worker. Non-blocking; drops on full/disconnect.
    pub fn send(&self, event: KeyboardEvent) {
        if self.sender.try_send(event).is_err() {
            crate::vlog!("[keyboard] worker event dropped (backpressure or stopped)");
        }
    }
}

fn dispatch_event<B: InputBackend + ?Sized>(backend: &mut B, event: &KeyboardEvent) {
    match event {
        KeyboardEvent::Text(text) => {
            let _ = backend.key_text(text);
        }
        KeyboardEvent::Special(code) => {
            let _ = backend.key_special(*code, 0);
        }
        KeyboardEvent::Combo { mods, text } => {
            let _ = backend.key_text_with_modifiers(text, *mods);
        }
        KeyboardEvent::SpecialCombo { mods, code } => {
            let _ = backend.key_special(*code, *mods);
        }
        KeyboardEvent::Raw { usage, state } => {
            let _ = backend.key_raw_hid(*usage as u8, *state != 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Touch packet parsing
// ---------------------------------------------------------------------------

/// Parse a batch of touch contacts from a raw buffer.
pub fn parse_touch_packet(data: &[u8]) -> Vec<TouchContact> {
    let mut out = Vec::new();
    if data.is_empty() {
        return out;
    }
    let count = data[0] as usize;
    let contacts = &data[1..];
    if contacts.len() < count * 8 {
        return out;
    }
    for i in 0..count {
        let off = i * 8;
        out.push(TouchContact {
            slot: contacts[off],
            event_type: contacts[off + 1],
            x: u16::from_be_bytes([contacts[off + 2], contacts[off + 3]]),
            y: u16::from_be_bytes([contacts[off + 4], contacts[off + 5]]),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Mouse packet parsing
// ---------------------------------------------------------------------------

const MOUSE_MOVE: u8 = 0x01;
const MOUSE_BUTTON: u8 = 0x02;
const MOUSE_SCROLL: u8 = 0x03;
const MOUSE_MOVE_ABS: u8 = 0x04;

/// Parse a mouse event packet.
pub fn parse_mouse_packet(data: &[u8]) -> Option<MouseEvent> {
    if data.is_empty() {
        return None;
    }
    match data[0] {
        MOUSE_MOVE if data.len() >= 5 => {
            let dx = i16::from_be_bytes([data[1], data[2]]);
            let dy = i16::from_be_bytes([data[3], data[4]]);
            Some(MouseEvent::MoveRelative { dx, dy })
        }
        MOUSE_MOVE_ABS if data.len() >= 5 => {
            let x = u16::from_be_bytes([data[1], data[2]]);
            let y = u16::from_be_bytes([data[3], data[4]]);
            Some(MouseEvent::MoveAbsolute { x, y })
        }
        MOUSE_BUTTON if data.len() >= 3 => Some(MouseEvent::Button {
            btn: data[1],
            state: data[2],
        }),
        MOUSE_SCROLL if data.len() >= 3 => {
            let dy = i16::from_be_bytes([data[1], data[2]]);
            Some(MouseEvent::Scroll { dy })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Keyboard packet parsing
// ---------------------------------------------------------------------------

/// Parse a KEY packet into a worker event.
pub fn parse_key_event(data: &[u8]) -> Option<KeyboardEvent> {
    if data.is_empty() {
        return None;
    }
    let key_type = data[0];
    let payload = &data[1..];
    match key_type {
        KEY_TYPE_TEXT => String::from_utf8(payload.to_vec())
            .ok()
            .map(KeyboardEvent::Text),
        KEY_TYPE_SPECIAL => payload.first().copied().map(KeyboardEvent::Special),
        KEY_TYPE_COMBO if payload.len() >= 2 => {
            let mods = payload[0];
            let inner_type = payload[1];
            let inner_payload = &payload[2..];
            match inner_type {
                KEY_TYPE_TEXT => String::from_utf8(inner_payload.to_vec())
                    .ok()
                    .map(|text| KeyboardEvent::Combo { mods, text }),
                KEY_TYPE_SPECIAL => inner_payload
                    .first()
                    .copied()
                    .map(|code| KeyboardEvent::SpecialCombo { mods, code }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Parse a RAWKEY packet into a worker event.
pub fn parse_rawkey_event(data: &[u8]) -> Option<KeyboardEvent> {
    if data.len() < 3 {
        return None;
    }
    let usage = u16::from_be_bytes([data[0], data[1]]);
    let state = data[2] as i32;
    Some(KeyboardEvent::Raw { usage, state })
}

// ---------------------------------------------------------------------------
// HID usage → evdev keycode (shared input-side mapping)
// ---------------------------------------------------------------------------

pub const KEY_A: u16 = 30;
pub const KEY_B: u16 = 48;
pub const KEY_C: u16 = 46;
pub const KEY_D: u16 = 32;
pub const KEY_E: u16 = 18;
pub const KEY_F: u16 = 33;
pub const KEY_G: u16 = 34;
pub const KEY_H: u16 = 35;
pub const KEY_I: u16 = 23;
pub const KEY_J: u16 = 36;
pub const KEY_K: u16 = 37;
pub const KEY_L: u16 = 38;
pub const KEY_M: u16 = 50;
pub const KEY_N: u16 = 49;
pub const KEY_O: u16 = 24;
pub const KEY_P: u16 = 25;
pub const KEY_Q: u16 = 16;
pub const KEY_R: u16 = 19;
pub const KEY_S: u16 = 31;
pub const KEY_T: u16 = 20;
pub const KEY_U: u16 = 22;
pub const KEY_V: u16 = 47;
pub const KEY_W: u16 = 17;
pub const KEY_X: u16 = 45;
pub const KEY_Y: u16 = 21;
pub const KEY_Z: u16 = 44;
pub const KEY_1: u16 = 2;
pub const KEY_2: u16 = 3;
pub const KEY_3: u16 = 4;
pub const KEY_4: u16 = 5;
pub const KEY_5: u16 = 6;
pub const KEY_6: u16 = 7;
pub const KEY_7: u16 = 8;
pub const KEY_8: u16 = 9;
pub const KEY_9: u16 = 10;
pub const KEY_0: u16 = 11;
pub const KEY_ENTER: u16 = 28;
pub const KEY_ESC: u16 = 1;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_SPACE: u16 = 57;
pub const KEY_MINUS: u16 = 12;
pub const KEY_EQUAL: u16 = 13;
pub const KEY_LEFTBRACE: u16 = 26;
pub const KEY_RIGHTBRACE: u16 = 27;
pub const KEY_BACKSLASH: u16 = 43;
pub const KEY_SEMICOLON: u16 = 39;
pub const KEY_APOSTROPHE: u16 = 40;
pub const KEY_GRAVE: u16 = 41;
pub const KEY_COMMA: u16 = 51;
pub const KEY_DOT: u16 = 52;
pub const KEY_SLASH: u16 = 53;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_F1: u16 = 59;
pub const KEY_F2: u16 = 60;
pub const KEY_F3: u16 = 61;
pub const KEY_F4: u16 = 62;
pub const KEY_F5: u16 = 63;
pub const KEY_F6: u16 = 64;
pub const KEY_F7: u16 = 65;
pub const KEY_F8: u16 = 66;
pub const KEY_F9: u16 = 67;
pub const KEY_F10: u16 = 68;
pub const KEY_F11: u16 = 87;
pub const KEY_F12: u16 = 88;
pub const KEY_SYSRQ: u16 = 99;
pub const KEY_SCROLLLOCK: u16 = 70;
pub const KEY_INSERT: u16 = 110;
pub const KEY_HOME: u16 = 102;
pub const KEY_PAGEUP: u16 = 104;
pub const KEY_DELETE: u16 = 111;
pub const KEY_END: u16 = 107;
pub const KEY_PAGEDOWN: u16 = 109;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_LEFT: u16 = 105;
pub const KEY_DOWN: u16 = 108;
pub const KEY_UP: u16 = 103;
pub const KEY_NUMLOCK: u16 = 69;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_RIGHTMETA: u16 = 126;

/// Convert HID keyboard usage code (USB HID spec, usage page 0x07) to Linux evdev keycode.
pub fn hid_to_evdev(hid: u16) -> Option<u16> {
    match hid {
        0x04 => Some(KEY_A),
        0x05 => Some(KEY_B),
        0x06 => Some(KEY_C),
        0x07 => Some(KEY_D),
        0x08 => Some(KEY_E),
        0x09 => Some(KEY_F),
        0x0A => Some(KEY_G),
        0x0B => Some(KEY_H),
        0x0C => Some(KEY_I),
        0x0D => Some(KEY_J),
        0x0E => Some(KEY_K),
        0x0F => Some(KEY_L),
        0x10 => Some(KEY_M),
        0x11 => Some(KEY_N),
        0x12 => Some(KEY_O),
        0x13 => Some(KEY_P),
        0x14 => Some(KEY_Q),
        0x15 => Some(KEY_R),
        0x16 => Some(KEY_S),
        0x17 => Some(KEY_T),
        0x18 => Some(KEY_U),
        0x19 => Some(KEY_V),
        0x1A => Some(KEY_W),
        0x1B => Some(KEY_X),
        0x1C => Some(KEY_Y),
        0x1D => Some(KEY_Z),
        0x1E => Some(KEY_1),
        0x1F => Some(KEY_2),
        0x20 => Some(KEY_3),
        0x21 => Some(KEY_4),
        0x22 => Some(KEY_5),
        0x23 => Some(KEY_6),
        0x24 => Some(KEY_7),
        0x25 => Some(KEY_8),
        0x26 => Some(KEY_9),
        0x27 => Some(KEY_0),
        0x28 => Some(KEY_ENTER),
        0x29 => Some(KEY_ESC),
        0x2A => Some(KEY_BACKSPACE),
        0x2B => Some(KEY_TAB),
        0x2C => Some(KEY_SPACE),
        0x2D => Some(KEY_MINUS),
        0x2E => Some(KEY_EQUAL),
        0x2F => Some(KEY_LEFTBRACE),
        0x30 => Some(KEY_RIGHTBRACE),
        0x31 => Some(KEY_BACKSLASH),
        0x33 => Some(KEY_SEMICOLON),
        0x34 => Some(KEY_APOSTROPHE),
        0x35 => Some(KEY_GRAVE),
        0x36 => Some(KEY_COMMA),
        0x37 => Some(KEY_DOT),
        0x38 => Some(KEY_SLASH),
        0x39 => Some(KEY_CAPSLOCK),
        0x3A => Some(KEY_F1),
        0x3B => Some(KEY_F2),
        0x3C => Some(KEY_F3),
        0x3D => Some(KEY_F4),
        0x3E => Some(KEY_F5),
        0x3F => Some(KEY_F6),
        0x40 => Some(KEY_F7),
        0x41 => Some(KEY_F8),
        0x42 => Some(KEY_F9),
        0x43 => Some(KEY_F10),
        0x44 => Some(KEY_F11),
        0x45 => Some(KEY_F12),
        0x46 => Some(KEY_SYSRQ),
        0x47 => Some(KEY_SCROLLLOCK),
        0x49 => Some(KEY_INSERT),
        0x4A => Some(KEY_HOME),
        0x4B => Some(KEY_PAGEUP),
        0x4C => Some(KEY_DELETE),
        0x4D => Some(KEY_END),
        0x4E => Some(KEY_PAGEDOWN),
        0x4F => Some(KEY_RIGHT),
        0x50 => Some(KEY_LEFT),
        0x51 => Some(KEY_DOWN),
        0x52 => Some(KEY_UP),
        0x53 => Some(KEY_NUMLOCK),
        0xE0 => Some(KEY_LEFTCTRL),
        0xE1 => Some(KEY_LEFTSHIFT),
        0xE2 => Some(KEY_LEFTALT),
        0xE3 => Some(KEY_LEFTMETA),
        0xE4 => Some(KEY_RIGHTCTRL),
        0xE5 => Some(KEY_RIGHTSHIFT),
        0xE6 => Some(KEY_RIGHTALT),
        0xE7 => Some(KEY_RIGHTMETA),
        _ => None,
    }
}

/// Global flag used by `stream_server` to decide whether direct mouse control is active.
pub static DIRECT_MOUSE_ENABLED: AtomicBool = AtomicBool::new(false);
