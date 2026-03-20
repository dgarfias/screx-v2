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
// On-screen keyboard (OSK) via gsettings + GNOME Shell D-Bus
// ---------------------------------------------------------------------------

const OSK_SCHEMA: &str = "org.gnome.desktop.a11y.applications";
const OSK_KEY: &str = "screen-keyboard-enabled";

fn user_session_cmd(program: &str, args: &[&str]) -> Option<String> {
    let (sudo_user, sudo_uid) = (
        std::env::var("SUDO_USER").ok(),
        std::env::var("SUDO_UID").ok(),
    );

    let output = if let (Some(ref user), Some(ref uid)) = (sudo_user, sudo_uid) {
        let runtime_dir = format!("/run/user/{uid}");
        let dbus_addr = format!("unix:path={runtime_dir}/bus");
        let mut cmd = Command::new("runuser");
        cmd.args(["-u", user, "--"])
            .arg("env")
            .arg(format!("DBUS_SESSION_BUS_ADDRESS={dbus_addr}"))
            .arg(format!("XDG_RUNTIME_DIR={runtime_dir}"))
            .arg(program);
        for a in args {
            cmd.arg(a);
        }
        cmd.output()
    } else {
        let mut cmd = Command::new(program);
        for a in args {
            cmd.arg(a);
        }
        cmd.output()
    };

    match output {
        Ok(o) if o.status.success() => {
            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("[osk] {program} failed: {stderr}");
            None
        }
        Err(e) => {
            eprintln!("[osk] could not run {program}: {e}");
            None
        }
    }
}

fn gsettings_cmd(args: &[&str]) -> Option<String> {
    user_session_cmd("gsettings", args)
}

/// Read the current on-screen keyboard enabled state.
pub fn get_osk_enabled() -> Option<bool> {
    gsettings_cmd(&["get", OSK_SCHEMA, OSK_KEY])
        .map(|v| v == "true")
}

/// Set the on-screen keyboard enabled state.
pub fn set_osk_enabled(enabled: bool) {
    let val = if enabled { "true" } else { "false" };
    if gsettings_cmd(&["set", OSK_SCHEMA, OSK_KEY, val]).is_some() {
        println!("[osk] screen keyboard {}", if enabled { "enabled" } else { "disabled" });
    }
}

/// Toggle the on-screen keyboard visibility via GNOME Shell.
/// 1. Ensures the a11y keyboard is enabled (so GNOME Shell loads the keyboard actor)
/// 2. Calls Shell.Eval to open or close the keyboard directly
pub fn toggle_osk(osk_visible: &mut bool) {
    // Make sure the OSK infrastructure is enabled
    if get_osk_enabled() != Some(true) {
        set_osk_enabled(true);
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    *osk_visible = !*osk_visible;

    let js = if *osk_visible {
        concat!(
            "let _ki = -1; ",
            "for (let i = 0; i < global.display.get_n_monitors(); i++) { ",
            "  let c = global.display.get_monitor_connector(i) || ''; ",
            "  if (c.includes('EVDI')) { _ki = i; break; } ",
            "} ",
            "if (_ki >= 0) imports.ui.main.layoutManager.keyboardIndex = _ki; ",
            "imports.ui.main.keyboard.open(imports.gi.Clutter.get_current_event_time());",
        )
    } else {
        "imports.ui.main.keyboard.close();"
    };

    let result = user_session_cmd("gdbus", &[
        "call", "--session",
        "--dest", "org.gnome.Shell",
        "--object-path", "/org/gnome/Shell",
        "--method", "org.gnome.Shell.Eval",
        js,
    ]);

    match result {
        Some(ref out) if out.contains("true") => {
            println!("[osk] keyboard {}", if *osk_visible { "opened" } else { "closed" });
        }
        Some(ref out) => {
            eprintln!("[osk] Shell.Eval returned: {out}");
            eprintln!("[osk] keyboard will auto-show when you tap text fields instead");
        }
        None => {
            eprintln!("[osk] Shell.Eval not available — keyboard will auto-show on text field tap");
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
