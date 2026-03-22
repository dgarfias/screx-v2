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
const INPUT_PROP_POINTER: libc::c_int = 0x00;
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

fn current_timeval() -> libc::timeval {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    unsafe {
        libc::gettimeofday(&mut tv, std::ptr::null_mut());
    }
    tv
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
            time: current_timeval(),
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
// Virtual mouse (uinput) — receives mouse input from iPad's physical mouse
// ---------------------------------------------------------------------------

const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const UI_SET_RELBIT: libc::c_ulong = 0x40045566; // _IOW('U', 102, int)

pub struct VirtualMouse {
    file: File,
}

impl VirtualMouse {
    pub fn new() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context("failed to open /dev/uinput for mouse")?;

        let fd = file.as_raw_fd();

        unsafe {
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_REL as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_POINTER))?;

            ioctl_check(libc::ioctl(fd, UI_SET_KEYBIT, BTN_LEFT as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_KEYBIT, BTN_RIGHT as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_KEYBIT, BTN_MIDDLE as libc::c_int))?;

            ioctl_check(libc::ioctl(fd, UI_SET_RELBIT, REL_X as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_RELBIT, REL_Y as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_RELBIT, REL_WHEEL as libc::c_int))?;

            let mut setup: UinputSetup = mem::zeroed();
            setup.id.bustype = 0x03; // BUS_USB
            setup.id.vendor = SCREX_VENDOR;
            setup.id.product = SCREX_PRODUCT + 2;
            setup.id.version = 1;
            let name = b"Screx Virtual Mouse";
            setup.name[..name.len()].copy_from_slice(name);

            ioctl_check(libc::ioctl(fd, ioctl_ui_dev_setup(), &setup))?;
            ioctl_check(libc::ioctl(fd, ioctl_ui_dev_create()))?;
        }

        std::thread::sleep(std::time::Duration::from_millis(200));
        println!("[mouse] virtual mouse created");

        Ok(Self { file })
    }

    pub fn move_rel(&mut self, dx: i32, dy: i32) {
        self.emit(EV_REL, REL_X, dx);
        self.emit(EV_REL, REL_Y, dy);
        self.emit(EV_SYN, SYN_REPORT, 0);
    }

    pub fn button(&mut self, btn: u8, state: i32) {
        let code = match btn {
            0 => BTN_LEFT,
            1 => BTN_RIGHT,
            2 => BTN_MIDDLE,
            _ => return,
        };
        crate::vlog!("[mouse] emit button: btn={} code={} state={state}", btn, code);
        self.emit(EV_KEY, code, state);
        self.emit(EV_SYN, SYN_REPORT, 0);
    }

    pub fn scroll(&mut self, dy: i32) {
        self.emit(EV_REL, REL_WHEEL, dy);
        self.emit(EV_SYN, SYN_REPORT, 0);
    }

    pub fn release_all_buttons(&mut self) {
        self.emit(EV_KEY, BTN_LEFT, 0);
        self.emit(EV_KEY, BTN_RIGHT, 0);
        self.emit(EV_KEY, BTN_MIDDLE, 0);
        self.emit(EV_SYN, SYN_REPORT, 0);
    }

    fn emit(&mut self, type_: u16, code: u16, value: i32) {
        let ev = InputEvent {
            time: current_timeval(),
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

impl Drop for VirtualMouse {
    fn drop(&mut self) {
        self.release_all_buttons();
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), ioctl_ui_dev_destroy());
        }
        println!("[mouse] virtual mouse destroyed");
    }
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
const KEY_LEFTCTRL: u16 = 29;
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
const KEY_LEFTALT: u16 = 56;
const KEY_LEFTMETA: u16 = 125;
const KEY_UP: u16 = 103;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_DOWN: u16 = 108;
const KEY_DELETE: u16 = 111;
const KEY_INSERT: u16 = 110;
const KEY_HOME: u16 = 102;
const KEY_END: u16 = 107;

const ALL_KEYS: &[u16] = &[
    KEY_ESC, KEY_1, KEY_2, KEY_3, KEY_4, KEY_5, KEY_6, KEY_7, KEY_8, KEY_9, KEY_0,
    KEY_MINUS, KEY_EQUAL, KEY_BACKSPACE, KEY_TAB,
    KEY_Q, KEY_W, KEY_E, KEY_R, KEY_T_KEY, KEY_Y, KEY_U, KEY_I, KEY_O, KEY_P,
    KEY_LEFTBRACE, KEY_RIGHTBRACE, KEY_ENTER, KEY_LEFTCTRL,
    KEY_A, KEY_S, KEY_D, KEY_F, KEY_G, KEY_H, KEY_J, KEY_K, KEY_L,
    KEY_SEMICOLON, KEY_APOSTROPHE, KEY_GRAVE, KEY_LEFTSHIFT, KEY_BACKSLASH,
    KEY_Z, KEY_X, KEY_C, KEY_V, KEY_B_KEY, KEY_N, KEY_M,
    KEY_COMMA, KEY_DOT, KEY_SLASH, KEY_SPACE,
    KEY_LEFTALT, KEY_LEFTMETA,
    KEY_UP, KEY_LEFT, KEY_RIGHT, KEY_DOWN, KEY_DELETE, KEY_INSERT, KEY_HOME, KEY_END,
    KEY_CAPSLOCK, KEY_RIGHTSHIFT, KEY_RIGHTCTRL, KEY_RIGHTALT, KEY_RIGHTMETA,
    KEY_F1, KEY_F2, KEY_F3, KEY_F4, KEY_F5, KEY_F6,
    KEY_F7, KEY_F8, KEY_F9, KEY_F10, KEY_F11, KEY_F12,
    KEY_SCROLLLOCK, KEY_NUMLOCK, KEY_PAGEUP, KEY_PAGEDOWN, KEY_SYSRQ,
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
        0x0C => Some(KEY_LEFTCTRL),
        0x0D => Some(KEY_LEFTALT),
        0x0E => Some(KEY_LEFTMETA),
        0x0F => Some(KEY_INSERT),
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
            } else {
                self.type_unicode(c);
            }
        }
    }

    fn type_unicode(&mut self, c: char) {
        let hex = format!("{:x}", c as u32);

        self.key_event(KEY_LEFTCTRL, 1);
        self.key_event(KEY_LEFTSHIFT, 1);
        self.key_event(KEY_U, 1);
        self.syn();
        self.key_event(KEY_U, 0);
        self.key_event(KEY_LEFTSHIFT, 0);
        self.key_event(KEY_LEFTCTRL, 0);
        self.syn();

        std::thread::sleep(std::time::Duration::from_millis(10));

        for b in hex.bytes() {
            let key = match b {
                b'0' => KEY_0, b'1' => KEY_1, b'2' => KEY_2, b'3' => KEY_3,
                b'4' => KEY_4, b'5' => KEY_5, b'6' => KEY_6, b'7' => KEY_7,
                b'8' => KEY_8, b'9' => KEY_9, b'a' => KEY_A, b'b' => KEY_B_KEY,
                b'c' => KEY_C, b'd' => KEY_D, b'e' => KEY_E, b'f' => KEY_F,
                _ => continue,
            };
            self.key_event(key, 1);
            self.syn();
            self.key_event(key, 0);
            self.syn();
        }

        self.key_event(KEY_ENTER, 1);
        self.syn();
        self.key_event(KEY_ENTER, 0);
        self.syn();
    }

    pub fn press_special(&mut self, code: u8) {
        if let Some(keycode) = special_to_keycode(code) {
            self.key_event(keycode, 1);
            self.syn();
            self.key_event(keycode, 0);
            self.syn();
        }
    }

    pub fn type_with_modifiers(&mut self, mods: u8, text: &str) {
        self.press_mod_keys(mods, 1);
        self.syn();
        for c in text.chars() {
            if let Some((keycode, shift)) = char_to_key(c) {
                if shift { self.key_event(KEY_LEFTSHIFT, 1); }
                self.key_event(keycode, 1);
                self.syn();
                self.key_event(keycode, 0);
                if shift { self.key_event(KEY_LEFTSHIFT, 0); }
                self.syn();
            }
        }
        self.press_mod_keys(mods, 0);
        self.syn();
    }

    pub fn press_special_with_modifiers(&mut self, mods: u8, code: u8) {
        if let Some(keycode) = special_to_keycode(code) {
            self.press_mod_keys(mods, 1);
            self.syn();
            self.key_event(keycode, 1);
            self.syn();
            self.key_event(keycode, 0);
            self.press_mod_keys(mods, 0);
            self.syn();
        }
    }

    fn press_mod_keys(&mut self, mods: u8, value: i32) {
        if mods & 0x01 != 0 { self.key_event(KEY_LEFTCTRL, value); }
        if mods & 0x02 != 0 { self.key_event(KEY_LEFTALT, value); }
        if mods & 0x04 != 0 { self.key_event(KEY_LEFTMETA, value); }
    }

    fn key_event(&mut self, code: u16, value: i32) {
        let ev = InputEvent {
            time: current_timeval(),
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
            time: current_timeval(),
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
pub const KEY_TYPE_COMBO: u8 = 0x04;

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
        KEY_TYPE_COMBO => {
            if payload.len() >= 2 {
                let mods = payload[0];
                let inner_type = payload[1];
                let inner_payload = &payload[2..];
                match inner_type {
                    KEY_TYPE_TEXT => {
                        if let Ok(text) = std::str::from_utf8(inner_payload) {
                            kb.type_with_modifiers(mods, text);
                        }
                    }
                    KEY_TYPE_SPECIAL => {
                        if !inner_payload.is_empty() {
                            kb.press_special_with_modifiers(mods, inner_payload[0]);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
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

// ---------------------------------------------------------------------------
// Mouse packet parsing
// ---------------------------------------------------------------------------

const MOUSE_MOVE: u8 = 0x01;
const MOUSE_BUTTON: u8 = 0x02;
const MOUSE_SCROLL: u8 = 0x03;

/// Parse a mouse event packet.
/// Format: event_type(1) + payload
pub fn handle_mouse_packet(mouse: &mut VirtualMouse, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    match data[0] {
        MOUSE_MOVE if data.len() >= 5 => {
            let dx = i16::from_be_bytes([data[1], data[2]]) as i32;
            let dy = i16::from_be_bytes([data[3], data[4]]) as i32;
            crate::vlog!("[mouse] recv move: dx={dx} dy={dy}");
            mouse.move_rel(dx, dy);
        }
        MOUSE_BUTTON if data.len() >= 3 => {
            crate::vlog!("[mouse] recv button: btn={} state={}", data[1], data[2]);
            mouse.button(data[1], data[2] as i32);
        }
        MOUSE_SCROLL if data.len() >= 3 => {
            let dy = i16::from_be_bytes([data[1], data[2]]) as i32;
            crate::vlog!("[mouse] recv scroll: dy={dy}");
            mouse.scroll(dy);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Raw key packet parsing (physical keyboard HID usage → evdev)
// ---------------------------------------------------------------------------

/// Parse a raw key packet from a physical keyboard.
/// Format: hid_usage(u16 BE) + state(1)
pub fn handle_rawkey_packet(kb: &mut VirtualKeyboard, data: &[u8]) {
    if data.len() < 3 {
        return;
    }
    let hid = u16::from_be_bytes([data[0], data[1]]);
    let state = data[2] as i32;
    if let Some(evdev) = hid_to_evdev(hid) {
        kb.key_event(evdev, state);
        kb.syn();
    }
}

const KEY_RIGHTSHIFT: u16 = 54;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;
const KEY_RIGHTMETA: u16 = 126;
const KEY_CAPSLOCK: u16 = 58;
const KEY_F1: u16 = 59;
const KEY_F2: u16 = 60;
const KEY_F3: u16 = 61;
const KEY_F4: u16 = 62;
const KEY_F5: u16 = 63;
const KEY_F6: u16 = 64;
const KEY_F7: u16 = 65;
const KEY_F8: u16 = 66;
const KEY_F9: u16 = 67;
const KEY_F10: u16 = 68;
const KEY_F11: u16 = 87;
const KEY_F12: u16 = 88;
const KEY_SCROLLLOCK: u16 = 70;
const KEY_NUMLOCK: u16 = 69;
const KEY_PAGEUP: u16 = 104;
const KEY_PAGEDOWN: u16 = 109;
const KEY_SYSRQ: u16 = 99;

/// Convert HID keyboard usage code (USB HID spec, usage page 0x07) to Linux evdev keycode.
fn hid_to_evdev(hid: u16) -> Option<u16> {
    match hid {
        0x04 => Some(KEY_A),
        0x05 => Some(KEY_B_KEY),
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
        0x17 => Some(KEY_T_KEY),
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
