use std::fs::{File, OpenOptions};
use std::io::Write;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::process::Command;

use anyhow::{Context, Result};

// Linux input event constants
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const BTN_TOUCH: u16 = 0x14a;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;

// uinput ioctl constants
const UINPUT_IOCTL_BASE: u8 = b'U';
const UI_SET_EVBIT: libc::c_ulong = 0x40045564;   // _IOW('U', 100, int)
const UI_SET_KEYBIT: libc::c_ulong = 0x40045565;  // _IOW('U', 101, int)
const UI_SET_ABSBIT: libc::c_ulong = 0x40045567;  // _IOW('U', 103, int)
const UI_SET_PROPBIT: libc::c_ulong = 0x4004556e;  // _IOW('U', 110, int)
const INPUT_PROP_DIRECT: libc::c_int = 0x01;

const MAX_SLOTS: i32 = 10;
const SCREX_VENDOR: u16 = 0x1234;
const SCREX_PRODUCT: u16 = 0x5678;

// Touch event types matching iPad protocol
pub const TOUCH_DOWN: u8 = 0;
pub const TOUCH_MOVE: u8 = 1;
pub const TOUCH_UP: u8 = 2;

#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
}

#[repr(C)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

fn ioctl_ui_dev_setup() -> libc::c_ulong {
    // _IOW('U', 3, struct uinput_setup) = direction(1) | size(sizeof) | type('U') | nr(3)
    let size = mem::size_of::<UinputSetup>() as libc::c_ulong;
    (1 << 30) | (size << 16) | ((UINPUT_IOCTL_BASE as libc::c_ulong) << 8) | 3
}

fn ioctl_ui_abs_setup() -> libc::c_ulong {
    // _IOW('U', 4, struct uinput_abs_setup)
    let size = mem::size_of::<UinputAbsSetup>() as libc::c_ulong;
    (1 << 30) | (size << 16) | ((UINPUT_IOCTL_BASE as libc::c_ulong) << 8) | 4
}

fn ioctl_ui_dev_create() -> libc::c_ulong {
    // _IO('U', 1)
    ((UINPUT_IOCTL_BASE as libc::c_ulong) << 8) | 1
}

fn ioctl_ui_dev_destroy() -> libc::c_ulong {
    // _IO('U', 2)
    ((UINPUT_IOCTL_BASE as libc::c_ulong) << 8) | 2
}

pub struct VirtualTouchscreen {
    file: File,
    width: i32,
    height: i32,
    current_slot: i32,
}

impl VirtualTouchscreen {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context("failed to open /dev/uinput — is the uinput module loaded?")?;

        let fd = file.as_raw_fd();

        unsafe {
            // Enable event types
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_ABS as libc::c_int))?;

            // BTN_TOUCH
            ioctl_check(libc::ioctl(fd, UI_SET_KEYBIT, BTN_TOUCH as libc::c_int))?;

            // INPUT_PROP_DIRECT — marks this as a touchscreen, not a trackpad
            ioctl_check(libc::ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_DIRECT))?;

            // Enable absolute axes
            ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, ABS_X as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, ABS_Y as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, ABS_MT_SLOT as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, ABS_MT_TRACKING_ID as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, ABS_MT_POSITION_X as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, ABS_MT_POSITION_Y as libc::c_int))?;

            let w = width as i32;
            let h = height as i32;

            // Configure absolute axes via UI_ABS_SETUP
            set_abs(fd, ABS_X, 0, w - 1)?;
            set_abs(fd, ABS_Y, 0, h - 1)?;
            set_abs(fd, ABS_MT_SLOT, 0, MAX_SLOTS - 1)?;
            set_abs(fd, ABS_MT_TRACKING_ID, 0, 0xffff)?;
            set_abs(fd, ABS_MT_POSITION_X, 0, w - 1)?;
            set_abs(fd, ABS_MT_POSITION_Y, 0, h - 1)?;

            // Device setup
            let mut setup: UinputSetup = mem::zeroed();
            setup.id.bustype = 0x03; // BUS_USB
            setup.id.vendor = SCREX_VENDOR;
            setup.id.product = SCREX_PRODUCT;
            setup.id.version = 1;
            let name = b"Screx Virtual Touchscreen";
            setup.name[..name.len()].copy_from_slice(name);

            ioctl_check(libc::ioctl(fd, ioctl_ui_dev_setup(), &setup))?;
            ioctl_check(libc::ioctl(fd, ioctl_ui_dev_create()))?;
        }

        // Give udev time to process the new device
        std::thread::sleep(std::time::Duration::from_millis(200));

        println!("[touch] virtual touchscreen created: {width}x{height} ({MAX_SLOTS} slots)");

        Ok(Self {
            file,
            width: width as i32,
            height: height as i32,
            current_slot: 0,
        })
    }

    /// Maps this virtual touchscreen to the EVDI "Screx Virtual" output via gsettings.
    /// Must be called after device creation. Needs ~1s for mutter to detect the device.
    pub fn map_to_output(&self) {
        // Wait for mutter to detect the new uinput device
        std::thread::sleep(std::time::Duration::from_millis(800));

        let vendor_product = format!(
            "{:04x}:{:04x}",
            SCREX_VENDOR, SCREX_PRODUCT
        );
        let schema_path = format!(
            "org.gnome.desktop.peripherals.touchscreen:/org/gnome/desktop/peripherals/touchscreens/{vendor_product}/"
        );
        let output_value = "['SRX', 'Screx Virtual', '001']";

        // When running under sudo, we must run gsettings as the real user
        // with access to their DBUS session, otherwise dconf can't talk to
        // the GNOME session and the mapping silently fails.
        let (sudo_user, sudo_uid) = (
            std::env::var("SUDO_USER").ok(),
            std::env::var("SUDO_UID").ok(),
        );

        let result = if let (Some(ref user), Some(ref uid)) = (sudo_user, sudo_uid) {
            let runtime_dir = format!("/run/user/{uid}");
            let dbus_addr = format!("unix:path={runtime_dir}/bus");

            println!(
                "[touch] running gsettings as user '{user}' (uid={uid}) to map {vendor_product} -> EVDI output"
            );

            Command::new("runuser")
                .args(["-u", user, "--"])
                .arg("env")
                .arg(format!("DBUS_SESSION_BUS_ADDRESS={dbus_addr}"))
                .arg(format!("XDG_RUNTIME_DIR={runtime_dir}"))
                .arg("gsettings")
                .arg("set")
                .arg(&schema_path)
                .arg("output")
                .arg(output_value)
                .output()
        } else {
            println!("[touch] running gsettings directly (not under sudo)");
            Command::new("gsettings")
                .args(["set", &schema_path, "output", output_value])
                .output()
        };

        match result {
            Ok(output) if output.status.success() => {
                println!("[touch] mapped touchscreen {vendor_product} to EVDI output via gsettings");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                eprintln!("[touch] gsettings mapping failed: stderr={stderr} stdout={stdout}");
                eprintln!("[touch] you can map manually: gsettings set {schema_path} output \"{output_value}\"");
            }
            Err(e) => {
                eprintln!("[touch] could not run gsettings/runuser: {e}");
                eprintln!(
                    "[touch] map manually: gsettings set {schema_path} output \"{output_value}\""
                );
            }
        }
    }

    /// Process a touch contact from the iPad.
    pub fn send_touch(&mut self, slot: u8, event_type: u8, x: u16, y: u16) {
        let slot = (slot as i32).min(MAX_SLOTS - 1);
        let x = (x as i32).min(self.width - 1);
        let y = (y as i32).min(self.height - 1);

        // Switch slot if needed
        if self.current_slot != slot {
            self.emit(EV_ABS, ABS_MT_SLOT, slot);
            self.current_slot = slot;
        }

        match event_type {
            TOUCH_DOWN => {
                self.emit(EV_ABS, ABS_MT_TRACKING_ID, slot);
                self.emit(EV_ABS, ABS_MT_POSITION_X, x);
                self.emit(EV_ABS, ABS_MT_POSITION_Y, y);
                self.emit(EV_KEY, BTN_TOUCH, 1);
                // Also update single-touch axes
                self.emit(EV_ABS, ABS_X, x);
                self.emit(EV_ABS, ABS_Y, y);
            }
            TOUCH_MOVE => {
                self.emit(EV_ABS, ABS_MT_POSITION_X, x);
                self.emit(EV_ABS, ABS_MT_POSITION_Y, y);
                self.emit(EV_ABS, ABS_X, x);
                self.emit(EV_ABS, ABS_Y, y);
            }
            TOUCH_UP => {
                self.emit(EV_ABS, ABS_MT_TRACKING_ID, -1);
            }
            _ => {}
        }
    }

    /// Send a SYN_REPORT after all contacts in a batch have been processed.
    pub fn sync(&mut self) {
        self.emit(EV_SYN, SYN_REPORT, 0);
    }

    fn emit(&mut self, type_: u16, code: u16, value: i32) {
        let ev = InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &ev as *const InputEvent as *const u8,
                mem::size_of::<InputEvent>(),
            )
        };
        let _ = self.file.write_all(bytes);
    }
}

impl Drop for VirtualTouchscreen {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), ioctl_ui_dev_destroy());
        }
        println!("[touch] virtual touchscreen destroyed");
    }
}

unsafe fn set_abs(fd: i32, code: u16, min: i32, max: i32) -> Result<()> {
    let setup = UinputAbsSetup {
        code,
        absinfo: InputAbsinfo {
            value: 0,
            minimum: min,
            maximum: max,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        },
    };
    ioctl_check(libc::ioctl(fd, ioctl_ui_abs_setup(), &setup))
}

unsafe fn ioctl_check(ret: libc::c_int) -> Result<()> {
    if ret < 0 {
        anyhow::bail!(
            "ioctl failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Virtual keyboard (uinput) — receives keystrokes from iPad's native keyboard
// ---------------------------------------------------------------------------

const KEY_ESC: u16 = 1;
const KEY_1: u16 = 2;
const KEY_2: u16 = 3;
const KEY_3: u16 = 4;
const KEY_4: u16 = 5;
const KEY_5: u16 = 6;
const KEY_6: u16 = 7;
const KEY_7: u16 = 8;
const KEY_8: u16 = 9;
const KEY_9: u16 = 10;
const KEY_0: u16 = 11;
const KEY_MINUS: u16 = 12;
const KEY_EQUAL: u16 = 13;
const KEY_BACKSPACE: u16 = 14;
const KEY_TAB: u16 = 15;
const KEY_Q: u16 = 16;
const KEY_W: u16 = 17;
const KEY_E: u16 = 18;
const KEY_R: u16 = 19;
const KEY_T_KEY: u16 = 20;
const KEY_Y: u16 = 21;
const KEY_U: u16 = 22;
const KEY_I: u16 = 23;
const KEY_O: u16 = 24;
const KEY_P: u16 = 25;
const KEY_LEFTBRACE: u16 = 26;
const KEY_RIGHTBRACE: u16 = 27;
const KEY_ENTER: u16 = 28;
const KEY_A: u16 = 30;
const KEY_S: u16 = 31;
const KEY_D: u16 = 32;
const KEY_F: u16 = 33;
const KEY_G: u16 = 34;
const KEY_H: u16 = 35;
const KEY_J: u16 = 36;
const KEY_K: u16 = 37;
const KEY_L: u16 = 38;
const KEY_SEMICOLON: u16 = 39;
const KEY_APOSTROPHE: u16 = 40;
const KEY_GRAVE: u16 = 41;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_BACKSLASH: u16 = 43;
const KEY_Z: u16 = 44;
const KEY_X: u16 = 45;
const KEY_C: u16 = 46;
const KEY_V: u16 = 47;
const KEY_B_KEY: u16 = 48;
const KEY_N: u16 = 49;
const KEY_M: u16 = 50;
const KEY_COMMA: u16 = 51;
const KEY_DOT: u16 = 52;
const KEY_SLASH: u16 = 53;
const KEY_SPACE: u16 = 57;
const KEY_UP: u16 = 103;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_DOWN: u16 = 108;
const KEY_DELETE: u16 = 111;
const KEY_HOME: u16 = 102;
const KEY_END: u16 = 107;

const ALL_KEYS: &[u16] = &[
    KEY_ESC, KEY_1, KEY_2, KEY_3, KEY_4, KEY_5, KEY_6, KEY_7, KEY_8, KEY_9, KEY_0,
    KEY_MINUS, KEY_EQUAL, KEY_BACKSPACE, KEY_TAB,
    KEY_Q, KEY_W, KEY_E, KEY_R, KEY_T_KEY, KEY_Y, KEY_U, KEY_I, KEY_O, KEY_P,
    KEY_LEFTBRACE, KEY_RIGHTBRACE, KEY_ENTER,
    KEY_A, KEY_S, KEY_D, KEY_F, KEY_G, KEY_H, KEY_J, KEY_K, KEY_L,
    KEY_SEMICOLON, KEY_APOSTROPHE, KEY_GRAVE, KEY_LEFTSHIFT, KEY_BACKSLASH,
    KEY_Z, KEY_X, KEY_C, KEY_V, KEY_B_KEY, KEY_N, KEY_M,
    KEY_COMMA, KEY_DOT, KEY_SLASH, KEY_SPACE,
    KEY_UP, KEY_LEFT, KEY_RIGHT, KEY_DOWN, KEY_DELETE, KEY_HOME, KEY_END,
];

fn char_to_key(c: char) -> Option<(u16, bool)> {
    match c {
        'a' => Some((KEY_A, false)),       'b' => Some((KEY_B_KEY, false)),
        'c' => Some((KEY_C, false)),       'd' => Some((KEY_D, false)),
        'e' => Some((KEY_E, false)),       'f' => Some((KEY_F, false)),
        'g' => Some((KEY_G, false)),       'h' => Some((KEY_H, false)),
        'i' => Some((KEY_I, false)),       'j' => Some((KEY_J, false)),
        'k' => Some((KEY_K, false)),       'l' => Some((KEY_L, false)),
        'm' => Some((KEY_M, false)),       'n' => Some((KEY_N, false)),
        'o' => Some((KEY_O, false)),       'p' => Some((KEY_P, false)),
        'q' => Some((KEY_Q, false)),       'r' => Some((KEY_R, false)),
        's' => Some((KEY_S, false)),       't' => Some((KEY_T_KEY, false)),
        'u' => Some((KEY_U, false)),       'v' => Some((KEY_V, false)),
        'w' => Some((KEY_W, false)),       'x' => Some((KEY_X, false)),
        'y' => Some((KEY_Y, false)),       'z' => Some((KEY_Z, false)),
        'A' => Some((KEY_A, true)),        'B' => Some((KEY_B_KEY, true)),
        'C' => Some((KEY_C, true)),        'D' => Some((KEY_D, true)),
        'E' => Some((KEY_E, true)),        'F' => Some((KEY_F, true)),
        'G' => Some((KEY_G, true)),        'H' => Some((KEY_H, true)),
        'I' => Some((KEY_I, true)),        'J' => Some((KEY_J, true)),
        'K' => Some((KEY_K, true)),        'L' => Some((KEY_L, true)),
        'M' => Some((KEY_M, true)),        'N' => Some((KEY_N, true)),
        'O' => Some((KEY_O, true)),        'P' => Some((KEY_P, true)),
        'Q' => Some((KEY_Q, true)),        'R' => Some((KEY_R, true)),
        'S' => Some((KEY_S, true)),        'T' => Some((KEY_T_KEY, true)),
        'U' => Some((KEY_U, true)),        'V' => Some((KEY_V, true)),
        'W' => Some((KEY_W, true)),        'X' => Some((KEY_X, true)),
        'Y' => Some((KEY_Y, true)),        'Z' => Some((KEY_Z, true)),
        '1' => Some((KEY_1, false)),       '2' => Some((KEY_2, false)),
        '3' => Some((KEY_3, false)),       '4' => Some((KEY_4, false)),
        '5' => Some((KEY_5, false)),       '6' => Some((KEY_6, false)),
        '7' => Some((KEY_7, false)),       '8' => Some((KEY_8, false)),
        '9' => Some((KEY_9, false)),       '0' => Some((KEY_0, false)),
        '!' => Some((KEY_1, true)),        '@' => Some((KEY_2, true)),
        '#' => Some((KEY_3, true)),        '$' => Some((KEY_4, true)),
        '%' => Some((KEY_5, true)),        '^' => Some((KEY_6, true)),
        '&' => Some((KEY_7, true)),        '*' => Some((KEY_8, true)),
        '(' => Some((KEY_9, true)),        ')' => Some((KEY_0, true)),
        '-' => Some((KEY_MINUS, false)),   '_' => Some((KEY_MINUS, true)),
        '=' => Some((KEY_EQUAL, false)),   '+' => Some((KEY_EQUAL, true)),
        '[' => Some((KEY_LEFTBRACE, false)), '{' => Some((KEY_LEFTBRACE, true)),
        ']' => Some((KEY_RIGHTBRACE, false)), '}' => Some((KEY_RIGHTBRACE, true)),
        ';' => Some((KEY_SEMICOLON, false)), ':' => Some((KEY_SEMICOLON, true)),
        '\'' => Some((KEY_APOSTROPHE, false)), '"' => Some((KEY_APOSTROPHE, true)),
        '`' => Some((KEY_GRAVE, false)),   '~' => Some((KEY_GRAVE, true)),
        '\\' => Some((KEY_BACKSLASH, false)), '|' => Some((KEY_BACKSLASH, true)),
        ',' => Some((KEY_COMMA, false)),   '<' => Some((KEY_COMMA, true)),
        '.' => Some((KEY_DOT, false)),     '>' => Some((KEY_DOT, true)),
        '/' => Some((KEY_SLASH, false)),   '?' => Some((KEY_SLASH, true)),
        ' ' => Some((KEY_SPACE, false)),
        '\t' => Some((KEY_TAB, false)),
        '\n' => Some((KEY_ENTER, false)),
        _ => None,
    }
}

pub const SPECIAL_BACKSPACE: u8 = 0x01;

fn special_to_keycode(code: u8) -> Option<u16> {
    match code {
        SPECIAL_BACKSPACE => Some(KEY_BACKSPACE),
        0x02 => Some(KEY_ENTER),
        0x03 => Some(KEY_TAB),
        0x04 => Some(KEY_ESC),
        0x05 => Some(KEY_LEFT),
        0x06 => Some(KEY_RIGHT),
        0x07 => Some(KEY_UP),
        0x08 => Some(KEY_DOWN),
        0x09 => Some(KEY_DELETE),
        0x0A => Some(KEY_HOME),
        0x0B => Some(KEY_END),
        _ => None,
    }
}

pub struct VirtualKeyboard {
    file: File,
}

impl VirtualKeyboard {
    pub fn new() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context("failed to open /dev/uinput for keyboard")?;

        let fd = file.as_raw_fd();

        unsafe {
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int))?;

            for &key in ALL_KEYS {
                ioctl_check(libc::ioctl(fd, UI_SET_KEYBIT, key as libc::c_int))?;
            }

            let mut setup: UinputSetup = mem::zeroed();
            setup.id.bustype = 0x03; // BUS_USB
            setup.id.vendor = SCREX_VENDOR;
            setup.id.product = SCREX_PRODUCT + 1;
            setup.id.version = 1;
            let name = b"Screx Virtual Keyboard";
            setup.name[..name.len()].copy_from_slice(name);

            ioctl_check(libc::ioctl(fd, ioctl_ui_dev_setup(), &setup))?;
            ioctl_check(libc::ioctl(fd, ioctl_ui_dev_create()))?;
        }

        std::thread::sleep(std::time::Duration::from_millis(200));
        println!("[keyboard] virtual keyboard created");

        Ok(Self { file })
    }

    pub fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            if let Some((keycode, shift)) = char_to_key(c) {
                if shift {
                    self.key_event(KEY_LEFTSHIFT, 1);
                }
                self.key_event(keycode, 1);
                self.syn();
                self.key_event(keycode, 0);
                if shift {
                    self.key_event(KEY_LEFTSHIFT, 0);
                }
                self.syn();
            }
        }
    }

    pub fn press_special(&mut self, code: u8) {
        if let Some(keycode) = special_to_keycode(code) {
            self.key_event(keycode, 1);
            self.syn();
            self.key_event(keycode, 0);
            self.syn();
        }
    }

    fn key_event(&mut self, code: u16, value: i32) {
        let ev = InputEvent {
            time: libc::timeval { tv_sec: 0, tv_usec: 0 },
            type_: EV_KEY,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &ev as *const InputEvent as *const u8,
                mem::size_of::<InputEvent>(),
            )
        };
        let _ = self.file.write_all(bytes);
    }

    fn syn(&mut self) {
        let ev = InputEvent {
            time: libc::timeval { tv_sec: 0, tv_usec: 0 },
            type_: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &ev as *const InputEvent as *const u8,
                mem::size_of::<InputEvent>(),
            )
        };
        let _ = self.file.write_all(bytes);
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), ioctl_ui_dev_destroy());
        }
        println!("[keyboard] virtual keyboard destroyed");
    }
}

pub const KEY_TYPE_TEXT: u8 = 0x01;
pub const KEY_TYPE_SPECIAL: u8 = 0x02;
pub const KEY_TYPE_OSK_TOGGLE: u8 = 0x03;

/// Handles a key packet, dispatching OSK toggle separately since it doesn't need a keyboard.
/// Returns true if the packet was fully handled (no keyboard needed).
pub fn handle_key_packet_no_kb(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    if data[0] == KEY_TYPE_OSK_TOGGLE {
        toggle_gnome_osk();
        return true;
    }
    false
}

/// Parse and handle a key packet from the iPad.
/// Format: type(1) + payload(variable)
pub fn handle_key_packet(kb: &mut VirtualKeyboard, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let key_type = data[0];
    let payload = &data[1..];

    match key_type {
        KEY_TYPE_TEXT => {
            if let Ok(text) = std::str::from_utf8(payload) {
                kb.type_text(text);
            }
        }
        KEY_TYPE_SPECIAL => {
            if !payload.is_empty() {
                kb.press_special(payload[0]);
            }
        }
        KEY_TYPE_OSK_TOGGLE => {
            toggle_gnome_osk();
        }
        _ => {}
    }
}

/// Toggles GNOME's on-screen keyboard via gsettings.
fn toggle_gnome_osk() {
    println!("[osk] toggle requested");

    let (sudo_user, sudo_uid) = (
        std::env::var("SUDO_USER").ok(),
        std::env::var("SUDO_UID").ok(),
    );

    println!("[osk] SUDO_USER={:?} SUDO_UID={:?}", sudo_user, sudo_uid);

    let schema = "org.gnome.desktop.a11y.applications";
    let key = "screen-keyboard-enabled";

    let current = run_gsettings(&sudo_user, &sudo_uid, &["get", schema, key]);
    let is_enabled = current.trim().contains("true");
    let new_val = if is_enabled { "false" } else { "true" };

    println!("[osk] current={:?} -> setting to {new_val}", current.trim());

    let result = run_gsettings(&sudo_user, &sudo_uid, &["set", schema, key, new_val]);
    if result.is_empty() {
        println!("[osk] GNOME on-screen keyboard: {new_val}");
    } else {
        eprintln!("[osk] gsettings failed: {result}");
    }
}

fn run_gsettings(user: &Option<String>, uid: &Option<String>, args: &[&str]) -> String {
    let output = if let (Some(ref u), Some(ref id)) = (user, uid) {
        let rt = format!("/run/user/{id}");
        let dbus = format!("unix:path={rt}/bus");
        Command::new("runuser")
            .args(["-u", u, "--"])
            .arg("env")
            .arg(format!("DBUS_SESSION_BUS_ADDRESS={dbus}"))
            .arg(format!("XDG_RUNTIME_DIR={rt}"))
            .arg("gsettings")
            .args(args)
            .output()
    } else {
        Command::new("gsettings").args(args).output()
    };
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let stdout = String::from_utf8_lossy(&o.stdout);
            eprintln!("[osk] gsettings {:?} failed: stderr={stderr} stdout={stdout}", args);
            format!("ERROR: {stderr}")
        }
        Err(e) => {
            eprintln!("[osk] could not run gsettings: {e}");
            format!("ERROR: {e}")
        }
    }
}

// ---------------------------------------------------------------------------
// Touch packet parsing
// ---------------------------------------------------------------------------

/// Parse a batch of touch contacts from a raw buffer.
/// Format: count(1) + N * [slot(1) + event_type(1) + x(u16 BE) + y(u16 BE) + padding(2)]
pub fn handle_touch_packet(touch: &mut VirtualTouchscreen, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let count = data[0] as usize;
    let contacts = &data[1..];
    if contacts.len() < count * 8 {
        return;
    }

    for i in 0..count {
        let off = i * 8;
        let slot = contacts[off];
        let event_type = contacts[off + 1];
        let x = u16::from_be_bytes([contacts[off + 2], contacts[off + 3]]);
        let y = u16::from_be_bytes([contacts[off + 4], contacts[off + 5]]);
        touch.send_touch(slot, event_type, x, y);
    }
    touch.sync();
}
