use std::fs::{File, OpenOptions};
use std::io::Write;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::process::Command;

use anyhow::{Context, Result};

use crate::input::{
    GamepadState, InputBackend, KeyboardEvent, MouseEvent, TouchContact, GPAD_BTN_EAST,
    GPAD_BTN_MODE, GPAD_BTN_NORTH, GPAD_BTN_SELECT, GPAD_BTN_SOUTH, GPAD_BTN_START,
    GPAD_BTN_THUMBL, GPAD_BTN_THUMBR, GPAD_BTN_TL, GPAD_BTN_TR, GPAD_BTN_WEST, SPECIAL_BACKSPACE,
    TOUCH_DOWN, TOUCH_MOVE, TOUCH_UP,
};
use crate::input::{
    KEY_0, KEY_1, KEY_2, KEY_3, KEY_4, KEY_5, KEY_6, KEY_7, KEY_8, KEY_9, KEY_A, KEY_APOSTROPHE,
    KEY_B, KEY_BACKSLASH, KEY_BACKSPACE, KEY_C, KEY_CAPSLOCK, KEY_COMMA, KEY_D, KEY_DELETE,
    KEY_DOT, KEY_DOWN, KEY_E, KEY_END, KEY_ENTER, KEY_EQUAL, KEY_ESC, KEY_F, KEY_F1, KEY_F10,
    KEY_F11, KEY_F12, KEY_F2, KEY_F3, KEY_F4, KEY_F5, KEY_F6, KEY_F7, KEY_F8, KEY_F9, KEY_G,
    KEY_GRAVE, KEY_H, KEY_HOME, KEY_I, KEY_INSERT, KEY_J, KEY_K, KEY_L, KEY_LEFT, KEY_LEFTALT,
    KEY_LEFTBRACE, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_M, KEY_MINUS, KEY_N,
    KEY_NUMLOCK, KEY_O, KEY_P, KEY_PAGEDOWN, KEY_PAGEUP, KEY_Q, KEY_R, KEY_RIGHT, KEY_RIGHTALT,
    KEY_RIGHTBRACE, KEY_RIGHTCTRL, KEY_RIGHTMETA, KEY_RIGHTSHIFT, KEY_S, KEY_SCROLLLOCK,
    KEY_SEMICOLON, KEY_SLASH, KEY_SPACE, KEY_SYSRQ, KEY_T, KEY_TAB, KEY_U, KEY_UP, KEY_V, KEY_W,
    KEY_X, KEY_Y, KEY_Z,
};

// Linux input event constants
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0x00;
const BTN_TOUCH: u16 = 0x14a;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

const BTN_SOUTH: u16 = 0x130;
const BTN_EAST: u16 = 0x131;
const BTN_NORTH: u16 = 0x133;
const BTN_WEST: u16 = 0x134;
const BTN_TL: u16 = 0x136;
const BTN_TR: u16 = 0x137;
const BTN_SELECT: u16 = 0x13a;
const BTN_START: u16 = 0x13b;
const BTN_MODE: u16 = 0x13c;
const BTN_THUMBL: u16 = 0x13d;
const BTN_THUMBR: u16 = 0x13e;

const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_Z: u16 = 0x02;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

const MAX_SLOTS: i32 = 10;
const SCREX_VENDOR: u16 = 0x1234;
const SCREX_PRODUCT: u16 = 0x5678;

// uinput ioctl constants
const UINPUT_IOCTL_BASE: u8 = b'U';
const UI_SET_EVBIT: libc::c_ulong = 0x40045564;
const UI_SET_KEYBIT: libc::c_ulong = 0x40045565;
const UI_SET_ABSBIT: libc::c_ulong = 0x40045567;
const UI_SET_PROPBIT: libc::c_ulong = 0x4004556e;
const UI_SET_RELBIT: libc::c_ulong = 0x40045566;
const INPUT_PROP_DIRECT: libc::c_int = 0x01;

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
    let size = mem::size_of::<UinputSetup>() as libc::c_ulong;
    (1 << 30) | (size << 16) | ((UINPUT_IOCTL_BASE as libc::c_ulong) << 8) | 3
}

fn ioctl_ui_abs_setup() -> libc::c_ulong {
    let size = mem::size_of::<UinputAbsSetup>() as libc::c_ulong;
    (1 << 30) | (size << 16) | ((UINPUT_IOCTL_BASE as libc::c_ulong) << 8) | 4
}

fn ioctl_ui_dev_create() -> libc::c_ulong {
    ((UINPUT_IOCTL_BASE as libc::c_ulong) << 8) | 1
}

fn ioctl_ui_dev_destroy() -> libc::c_ulong {
    ((UINPUT_IOCTL_BASE as libc::c_ulong) << 8) | 2
}

fn input_event(type_: u16, code: u16, value: i32) -> InputEvent {
    InputEvent {
        time: current_timeval(),
        type_,
        code,
        value,
    }
}

fn emit_batch(file: &mut File, events: &[InputEvent]) {
    if events.is_empty() {
        return;
    }
    let bytes = unsafe {
        std::slice::from_raw_parts(
            events.as_ptr() as *const u8,
            events.len() * mem::size_of::<InputEvent>(),
        )
    };
    let _ = file.write_all(bytes);
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
        anyhow::bail!("ioctl failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

pub struct LinuxInput {
    touch: Option<VirtualTouchscreen>,
    keyboard: Option<VirtualKeyboard>,
    mouse: Option<VirtualMouse>,
    gamepads: std::collections::HashMap<u8, VirtualGamepad>,
    /// Negotiated per-session stream resolution, in wire coordinate space.
    /// `None` until the first `set_target_rect` call. The virtual
    /// touchscreen itself is created once at startup sized to the CLI/config
    /// ceiling (recreating it per session would re-trigger the ~800ms
    /// gsettings output-mapping sleep and device churn in GNOME), so when
    /// this differs from the touchscreen's own dimensions, incoming touch
    /// coordinates need to be rescaled onto the touchscreen's fixed ABS
    /// range before injection.
    session_size: Option<(u32, u32)>,
}

impl LinuxInput {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let touch = VirtualTouchscreen::new(width, height).ok();
        if let Some(ref ts) = touch {
            ts.map_to_output();
        }
        let keyboard = VirtualKeyboard::new().ok();
        Ok(Self {
            touch,
            keyboard,
            mouse: None,
            gamepads: std::collections::HashMap::new(),
            session_size: None,
        })
    }
}

impl InputBackend for LinuxInput {
    fn set_target_rect(&mut self, _left: i32, _top: i32, width: u32, height: u32) {
        // left/top are ignored here: the gsettings tablet->EVDI-output
        // mapping (see `map_to_output`) already confines the fixed-size
        // virtual touchscreen to the captured output, so there is no
        // desktop-absolute offset to apply. What changes per session is the
        // *resolution* touch coordinates are expressed in on the wire, which
        // we need in order to scale them onto the touchscreen's ABS range.
        if let Some(ref ts) = self.touch {
            println!(
                "[touch] session stream resolution {width}x{height}, tablet ABS range {}x{}",
                ts.width, ts.height
            );
        }
        self.session_size = Some((width, height));
    }

    fn touch(&mut self, contacts: &[TouchContact]) -> Result<()> {
        if let Some(ref mut ts) = self.touch {
            match self.session_size {
                Some((sw, sh)) if (sw, sh) != (ts.width as u32, ts.height as u32) => {
                    let scaled: Vec<TouchContact> = contacts
                        .iter()
                        .map(|c| ts.scale_contact(c, sw, sh))
                        .collect();
                    ts.handle_contacts(&scaled);
                }
                _ => ts.handle_contacts(contacts),
            }
        }
        Ok(())
    }

    fn key_text(&mut self, text: &str) -> Result<()> {
        if let Some(ref mut kb) = self.keyboard {
            kb.type_text(text);
        }
        Ok(())
    }

    fn key_special(&mut self, code: u8, modifiers: u8) -> Result<()> {
        if let Some(ref mut kb) = self.keyboard {
            if modifiers != 0 {
                kb.press_special_with_modifiers(modifiers, code);
            } else {
                kb.press_special(code);
            }
        }
        Ok(())
    }

    fn key_text_with_modifiers(&mut self, text: &str, mods: u8) -> Result<()> {
        if let Some(ref mut kb) = self.keyboard {
            kb.type_with_modifiers(mods, text);
        }
        Ok(())
    }

    fn key_raw_hid(&mut self, usage: u8, pressed: bool) -> Result<()> {
        if let Some(ref mut kb) = self.keyboard {
            if let Some(code) = crate::input::hid_to_evdev(u16::from(usage)) {
                kb.key_event(code, if pressed { 1 } else { 0 });
                kb.syn();
            }
        }
        Ok(())
    }

    fn mouse(&mut self, ev: MouseEvent) -> Result<()> {
        // The virtual mouse device is created lazily on first use rather than
        // eagerly in `LinuxInput::new`, mirroring the pre-refactor behavior
        // where it was only instantiated once a physical mouse was actually
        // reported as attached (or the first MOUSE packet arrived).
        if self.mouse.is_none() {
            self.mouse = Some(VirtualMouse::new()?);
        }
        let m = self.mouse.as_mut().unwrap();
        match ev {
            MouseEvent::MoveRelative { dx, dy } => {
                crate::vlog!("[mouse] recv move: dx={dx} dy={dy}");
                m.move_rel(dx as i32, dy as i32);
            }
            MouseEvent::MoveAbsolute { x, y } => {
                crate::vlog!("[mouse] recv abs move: x={x} y={y}");
                m.move_abs(x, y);
            }
            MouseEvent::Button { btn, state } => {
                crate::vlog!("[mouse] recv button: btn={btn} state={state}");
                m.button(btn, state as i32);
            }
            MouseEvent::Scroll { dy } => {
                crate::vlog!("[mouse] recv scroll: dy={dy}");
                m.scroll(dy as i32);
            }
        }
        Ok(())
    }

    fn gamepad_attach(&mut self, id: u8) -> Result<()> {
        if !self.gamepads.contains_key(&id) {
            self.gamepads.insert(id, VirtualGamepad::new(id)?);
        }
        Ok(())
    }

    fn gamepad_detach(&mut self, id: u8) {
        self.gamepads.remove(&id);
    }

    fn gamepad_state(&mut self, id: u8, st: &GamepadState) -> Result<()> {
        if let Some(pad) = self.gamepads.get_mut(&id) {
            pad.set_state(
                st.buttons, st.lx, st.ly, st.rx, st.ry, st.lt, st.rt, st.hat_x, st.hat_y,
            );
        }
        Ok(())
    }
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
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_ABS as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_KEYBIT, BTN_TOUCH as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_DIRECT))?;
            ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, ABS_X as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, ABS_Y as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, ABS_MT_SLOT as libc::c_int))?;
            ioctl_check(libc::ioctl(
                fd,
                UI_SET_ABSBIT,
                ABS_MT_TRACKING_ID as libc::c_int,
            ))?;
            ioctl_check(libc::ioctl(
                fd,
                UI_SET_ABSBIT,
                ABS_MT_POSITION_X as libc::c_int,
            ))?;
            ioctl_check(libc::ioctl(
                fd,
                UI_SET_ABSBIT,
                ABS_MT_POSITION_Y as libc::c_int,
            ))?;

            let w = width as i32;
            let h = height as i32;
            set_abs(fd, ABS_X, 0, w - 1)?;
            set_abs(fd, ABS_Y, 0, h - 1)?;
            set_abs(fd, ABS_MT_SLOT, 0, MAX_SLOTS - 1)?;
            set_abs(fd, ABS_MT_TRACKING_ID, 0, 0xffff)?;
            set_abs(fd, ABS_MT_POSITION_X, 0, w - 1)?;
            set_abs(fd, ABS_MT_POSITION_Y, 0, h - 1)?;

            let mut setup: UinputSetup = mem::zeroed();
            setup.id.bustype = 0x03;
            setup.id.vendor = SCREX_VENDOR;
            setup.id.product = SCREX_PRODUCT;
            setup.id.version = 1;
            let name = b"Screx Virtual Touchscreen";
            setup.name[..name.len()].copy_from_slice(name);

            ioctl_check(libc::ioctl(fd, ioctl_ui_dev_setup(), &setup))?;
            ioctl_check(libc::ioctl(fd, ioctl_ui_dev_create()))?;
        }

        std::thread::sleep(std::time::Duration::from_millis(200));
        println!("[touch] virtual touchscreen created: {width}x{height} ({MAX_SLOTS} slots)");

        Ok(Self {
            file,
            width: width as i32,
            height: height as i32,
            current_slot: 0,
        })
    }

    pub fn map_to_output(&self) {
        std::thread::sleep(std::time::Duration::from_millis(800));
        let vendor_product = format!("{:04x}:{:04x}", SCREX_VENDOR, SCREX_PRODUCT);
        let schema_path = format!(
            "org.gnome.desktop.peripherals.touchscreen:/org/gnome/desktop/peripherals/touchscreens/{vendor_product}/"
        );
        let output_value = "['SRX', 'Screx Virtual', '001']";
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
                println!(
                    "[touch] mapped touchscreen {vendor_product} to EVDI output via gsettings"
                );
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!("[touch] gsettings mapping failed: {stderr}");
            }
            Err(e) => {
                eprintln!("[touch] could not run gsettings/runuser: {e}");
            }
        }
    }

    fn send_touch(
        &mut self,
        events: &mut Vec<InputEvent>,
        slot: u8,
        event_type: u8,
        x: u16,
        y: u16,
    ) {
        let slot = (slot as i32).min(MAX_SLOTS - 1);
        let x = (x as i32).min(self.width - 1);
        let y = (y as i32).min(self.height - 1);

        if self.current_slot != slot {
            events.push(input_event(EV_ABS, ABS_MT_SLOT, slot));
            self.current_slot = slot;
        }

        match event_type {
            TOUCH_DOWN => {
                events.push(input_event(EV_ABS, ABS_MT_TRACKING_ID, slot));
                events.push(input_event(EV_ABS, ABS_MT_POSITION_X, x));
                events.push(input_event(EV_ABS, ABS_MT_POSITION_Y, y));
                events.push(input_event(EV_KEY, BTN_TOUCH, 1));
                events.push(input_event(EV_ABS, ABS_X, x));
                events.push(input_event(EV_ABS, ABS_Y, y));
            }
            TOUCH_MOVE => {
                events.push(input_event(EV_ABS, ABS_MT_POSITION_X, x));
                events.push(input_event(EV_ABS, ABS_MT_POSITION_Y, y));
                events.push(input_event(EV_ABS, ABS_X, x));
                events.push(input_event(EV_ABS, ABS_Y, y));
            }
            TOUCH_UP => {
                events.push(input_event(EV_ABS, ABS_MT_TRACKING_ID, -1));
            }
            _ => {}
        }
    }

    fn sync(&mut self, events: &mut Vec<InputEvent>) {
        events.push(input_event(EV_SYN, SYN_REPORT, 0));
        emit_batch(&mut self.file, events);
        events.clear();
    }

    /// Rescales a wire touch contact from session-stream coordinate space
    /// (0..session_w-1 / 0..session_h-1) onto this device's fixed ABS range
    /// (0..width-1 / 0..height-1). Uses u64 intermediate math with
    /// round-to-nearest, and clamps the result into range to guard against
    /// rounding overshoot at the extreme edge. Falls back to the raw,
    /// unscaled coordinate when the session dimension isn't usable as a
    /// scale denominator (<=1 pixel) — `send_touch` clamps into range
    /// regardless, so this degenerates safely.
    fn scale_contact(&self, c: &TouchContact, session_w: u32, session_h: u32) -> TouchContact {
        let scale = |v: u16, src: u32, dst: i32| -> u16 {
            if src <= 1 || dst <= 1 {
                return v;
            }
            let num = v as u64 * (dst as u64 - 1);
            let den = src as u64 - 1;
            let scaled = (num + den / 2) / den;
            scaled.min(dst as u64 - 1) as u16
        };
        TouchContact {
            slot: c.slot,
            event_type: c.event_type,
            x: scale(c.x, session_w, self.width),
            y: scale(c.y, session_h, self.height),
        }
    }

    pub fn handle_contacts(&mut self, contacts: &[TouchContact]) {
        let mut events = Vec::with_capacity(contacts.len() * 8 + 1);
        for c in contacts {
            self.send_touch(&mut events, c.slot, c.event_type, c.x, c.y);
        }
        self.sync(&mut events);
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
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_ABS as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_KEYBIT, BTN_LEFT as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_KEYBIT, BTN_RIGHT as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_KEYBIT, BTN_MIDDLE as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_RELBIT, REL_X as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_RELBIT, REL_Y as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_RELBIT, REL_WHEEL as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, ABS_X as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, ABS_Y as libc::c_int))?;
            set_abs(fd, ABS_X, 0, 65535)?;
            set_abs(fd, ABS_Y, 0, 65535)?;

            let mut setup: UinputSetup = mem::zeroed();
            setup.id.bustype = 0x03;
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
        emit_batch(
            &mut self.file,
            &[
                input_event(EV_REL, REL_X, dx),
                input_event(EV_REL, REL_Y, dy),
                input_event(EV_SYN, SYN_REPORT, 0),
            ],
        );
    }

    pub fn move_abs(&mut self, x: u16, y: u16) {
        emit_batch(
            &mut self.file,
            &[
                input_event(EV_ABS, ABS_X, x as i32),
                input_event(EV_ABS, ABS_Y, y as i32),
                input_event(EV_SYN, SYN_REPORT, 0),
            ],
        );
    }

    pub fn button(&mut self, btn: u8, state: i32) {
        if btn == 2 {
            if state != 0 {
                emit_batch(
                    &mut self.file,
                    &[
                        input_event(EV_KEY, BTN_MIDDLE, 1),
                        input_event(EV_SYN, SYN_REPORT, 0),
                        input_event(EV_KEY, BTN_MIDDLE, 0),
                        input_event(EV_SYN, SYN_REPORT, 0),
                    ],
                );
            }
            return;
        }
        let code = match btn {
            0 => BTN_LEFT,
            1 => BTN_RIGHT,
            _ => return,
        };
        emit_batch(
            &mut self.file,
            &[
                input_event(EV_KEY, code, state),
                input_event(EV_SYN, SYN_REPORT, 0),
            ],
        );
    }

    pub fn scroll(&mut self, dy: i32) {
        emit_batch(
            &mut self.file,
            &[
                input_event(EV_REL, REL_WHEEL, dy),
                input_event(EV_SYN, SYN_REPORT, 0),
            ],
        );
    }

    pub fn release_all_buttons(&mut self) {
        emit_batch(
            &mut self.file,
            &[
                input_event(EV_KEY, BTN_LEFT, 0),
                input_event(EV_KEY, BTN_RIGHT, 0),
                input_event(EV_KEY, BTN_MIDDLE, 0),
                input_event(EV_SYN, SYN_REPORT, 0),
            ],
        );
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

pub struct VirtualGamepad {
    file: File,
    buttons_mask: u16,
}

impl VirtualGamepad {
    pub fn new(slot: u8) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context("failed to open /dev/uinput for gamepad")?;

        let fd = file.as_raw_fd();

        unsafe {
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int))?;
            ioctl_check(libc::ioctl(fd, UI_SET_EVBIT, EV_ABS as libc::c_int))?;

            for &btn in &[
                BTN_SOUTH, BTN_EAST, BTN_NORTH, BTN_WEST, BTN_TL, BTN_TR, BTN_SELECT, BTN_START,
                BTN_MODE, BTN_THUMBL, BTN_THUMBR,
            ] {
                ioctl_check(libc::ioctl(fd, UI_SET_KEYBIT, btn as libc::c_int))?;
            }

            for &abs in &[
                ABS_X, ABS_Y, ABS_RX, ABS_RY, ABS_Z, ABS_RZ, ABS_HAT0X, ABS_HAT0Y,
            ] {
                ioctl_check(libc::ioctl(fd, UI_SET_ABSBIT, abs as libc::c_int))?;
            }

            set_abs(fd, ABS_X, -32768, 32767)?;
            set_abs(fd, ABS_Y, -32768, 32767)?;
            set_abs(fd, ABS_RX, -32768, 32767)?;
            set_abs(fd, ABS_RY, -32768, 32767)?;
            set_abs(fd, ABS_Z, 0, 1023)?;
            set_abs(fd, ABS_RZ, 0, 1023)?;
            set_abs(fd, ABS_HAT0X, -1, 1)?;
            set_abs(fd, ABS_HAT0Y, -1, 1)?;

            let mut setup: UinputSetup = mem::zeroed();
            setup.id.bustype = 0x03;
            setup.id.vendor = SCREX_VENDOR;
            setup.id.product = SCREX_PRODUCT + 10 + slot as u16;
            setup.id.version = 1;
            let name = format!("Screx Virtual Gamepad {}", slot + 1);
            let name_bytes = name.as_bytes();
            setup.name[..name_bytes.len()].copy_from_slice(name_bytes);

            ioctl_check(libc::ioctl(fd, ioctl_ui_dev_setup(), &setup))?;
            ioctl_check(libc::ioctl(fd, ioctl_ui_dev_create()))?;
        }

        std::thread::sleep(std::time::Duration::from_millis(200));
        println!("[gamepad] virtual gamepad {} created", slot + 1);

        Ok(Self {
            file,
            buttons_mask: 0,
        })
    }

    pub fn set_state(
        &mut self,
        buttons_mask: u16,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
        lt: u16,
        rt: u16,
        hat_x: i8,
        hat_y: i8,
    ) {
        let mut events = Vec::with_capacity(19);
        self.sync_button(&mut events, BTN_SOUTH, GPAD_BTN_SOUTH, buttons_mask);
        self.sync_button(&mut events, BTN_EAST, GPAD_BTN_EAST, buttons_mask);
        self.sync_button(&mut events, BTN_WEST, GPAD_BTN_WEST, buttons_mask);
        self.sync_button(&mut events, BTN_NORTH, GPAD_BTN_NORTH, buttons_mask);
        self.sync_button(&mut events, BTN_TL, GPAD_BTN_TL, buttons_mask);
        self.sync_button(&mut events, BTN_TR, GPAD_BTN_TR, buttons_mask);
        self.sync_button(&mut events, BTN_THUMBL, GPAD_BTN_THUMBL, buttons_mask);
        self.sync_button(&mut events, BTN_THUMBR, GPAD_BTN_THUMBR, buttons_mask);
        self.sync_button(&mut events, BTN_SELECT, GPAD_BTN_SELECT, buttons_mask);
        self.sync_button(&mut events, BTN_START, GPAD_BTN_START, buttons_mask);
        self.sync_button(&mut events, BTN_MODE, GPAD_BTN_MODE, buttons_mask);
        self.buttons_mask = buttons_mask;

        events.push(input_event(EV_ABS, ABS_X, lx as i32));
        events.push(input_event(EV_ABS, ABS_Y, ly as i32));
        events.push(input_event(EV_ABS, ABS_RX, rx as i32));
        events.push(input_event(EV_ABS, ABS_RY, ry as i32));
        events.push(input_event(EV_ABS, ABS_Z, lt as i32));
        events.push(input_event(EV_ABS, ABS_RZ, rt as i32));
        events.push(input_event(EV_ABS, ABS_HAT0X, hat_x as i32));
        events.push(input_event(EV_ABS, ABS_HAT0Y, hat_y as i32));
        events.push(input_event(EV_SYN, SYN_REPORT, 0));
        emit_batch(&mut self.file, &events);
    }

    pub fn release_all(&mut self) {
        self.set_state(0, 0, 0, 0, 0, 0, 0, 0, 0);
    }

    fn sync_button(
        &mut self,
        events: &mut Vec<InputEvent>,
        linux_code: u16,
        bit: u16,
        new_mask: u16,
    ) {
        let prev = (self.buttons_mask & bit) != 0;
        let next = (new_mask & bit) != 0;
        if prev != next {
            events.push(input_event(EV_KEY, linux_code, if next { 1 } else { 0 }));
        }
    }
}

impl Drop for VirtualGamepad {
    fn drop(&mut self) {
        self.release_all();
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), ioctl_ui_dev_destroy());
        }
        println!("[gamepad] virtual gamepad destroyed");
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
            setup.id.bustype = 0x03;
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
                let mut ev = Vec::with_capacity(6);
                if shift {
                    ev.push(input_event(EV_KEY, KEY_LEFTSHIFT, 1));
                }
                ev.push(input_event(EV_KEY, keycode, 1));
                ev.push(input_event(EV_SYN, SYN_REPORT, 0));
                ev.push(input_event(EV_KEY, keycode, 0));
                if shift {
                    ev.push(input_event(EV_KEY, KEY_LEFTSHIFT, 0));
                }
                ev.push(input_event(EV_SYN, SYN_REPORT, 0));
                emit_batch(&mut self.file, &ev);
            } else {
                self.type_unicode(c);
            }
        }
    }

    fn type_unicode(&mut self, c: char) {
        let hex = format!("{:x}", c as u32);
        emit_batch(
            &mut self.file,
            &[
                input_event(EV_KEY, KEY_LEFTCTRL, 1),
                input_event(EV_KEY, KEY_LEFTSHIFT, 1),
                input_event(EV_KEY, KEY_U, 1),
                input_event(EV_SYN, SYN_REPORT, 0),
                input_event(EV_KEY, KEY_U, 0),
                input_event(EV_KEY, KEY_LEFTSHIFT, 0),
                input_event(EV_KEY, KEY_LEFTCTRL, 0),
                input_event(EV_SYN, SYN_REPORT, 0),
            ],
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut ev = Vec::with_capacity(hex.len() * 4 + 4);
        for b in hex.bytes() {
            let key = match b {
                b'0' => KEY_0,
                b'1' => KEY_1,
                b'2' => KEY_2,
                b'3' => KEY_3,
                b'4' => KEY_4,
                b'5' => KEY_5,
                b'6' => KEY_6,
                b'7' => KEY_7,
                b'8' => KEY_8,
                b'9' => KEY_9,
                b'a' => KEY_A,
                b'b' => KEY_B,
                b'c' => KEY_C,
                b'd' => KEY_D,
                b'e' => KEY_E,
                b'f' => KEY_F,
                _ => continue,
            };
            ev.push(input_event(EV_KEY, key, 1));
            ev.push(input_event(EV_SYN, SYN_REPORT, 0));
            ev.push(input_event(EV_KEY, key, 0));
            ev.push(input_event(EV_SYN, SYN_REPORT, 0));
        }
        ev.push(input_event(EV_KEY, KEY_ENTER, 1));
        ev.push(input_event(EV_SYN, SYN_REPORT, 0));
        ev.push(input_event(EV_KEY, KEY_ENTER, 0));
        ev.push(input_event(EV_SYN, SYN_REPORT, 0));
        emit_batch(&mut self.file, &ev);
    }

    pub fn press_special(&mut self, code: u8) {
        if let Some(keycode) = special_to_keycode(code) {
            emit_batch(
                &mut self.file,
                &[
                    input_event(EV_KEY, keycode, 1),
                    input_event(EV_SYN, SYN_REPORT, 0),
                    input_event(EV_KEY, keycode, 0),
                    input_event(EV_SYN, SYN_REPORT, 0),
                ],
            );
        }
    }

    /// Types `text` while holding the given modifier combo (bit layout matches
    /// the iPad wire protocol: ctrl=0x01, alt=0x02, meta/cmd=0x04).
    pub fn type_with_modifiers(&mut self, mods: u8, text: &str) {
        let mut ev = Vec::with_capacity(2 + text.chars().count() * 6 + 4);
        self.push_mod_keys(&mut ev, mods, 1);
        ev.push(input_event(EV_SYN, SYN_REPORT, 0));
        for c in text.chars() {
            if let Some((keycode, shift)) = char_to_key(c) {
                if shift {
                    ev.push(input_event(EV_KEY, KEY_LEFTSHIFT, 1));
                }
                ev.push(input_event(EV_KEY, keycode, 1));
                ev.push(input_event(EV_SYN, SYN_REPORT, 0));
                ev.push(input_event(EV_KEY, keycode, 0));
                if shift {
                    ev.push(input_event(EV_KEY, KEY_LEFTSHIFT, 0));
                }
                ev.push(input_event(EV_SYN, SYN_REPORT, 0));
            }
        }
        self.push_mod_keys(&mut ev, mods, 0);
        ev.push(input_event(EV_SYN, SYN_REPORT, 0));
        emit_batch(&mut self.file, &ev);
    }

    /// Presses a special key while holding the given modifier combo (same
    /// bit layout as `type_with_modifiers`).
    pub fn press_special_with_modifiers(&mut self, mods: u8, code: u8) {
        if let Some(keycode) = special_to_keycode(code) {
            let mut ev = Vec::with_capacity(8);
            self.push_mod_keys(&mut ev, mods, 1);
            ev.push(input_event(EV_SYN, SYN_REPORT, 0));
            ev.push(input_event(EV_KEY, keycode, 1));
            ev.push(input_event(EV_SYN, SYN_REPORT, 0));
            ev.push(input_event(EV_KEY, keycode, 0));
            self.push_mod_keys(&mut ev, mods, 0);
            ev.push(input_event(EV_SYN, SYN_REPORT, 0));
            emit_batch(&mut self.file, &ev);
        }
    }

    fn push_mod_keys(&self, ev: &mut Vec<InputEvent>, mods: u8, value: i32) {
        if mods & 0x01 != 0 {
            ev.push(input_event(EV_KEY, KEY_LEFTCTRL, value));
        }
        if mods & 0x02 != 0 {
            ev.push(input_event(EV_KEY, KEY_LEFTALT, value));
        }
        if mods & 0x04 != 0 {
            ev.push(input_event(EV_KEY, KEY_LEFTMETA, value));
        }
    }

    pub fn key_event(&mut self, code: u16, value: i32) {
        emit_batch(&mut self.file, &[input_event(EV_KEY, code, value)]);
    }

    pub fn syn(&mut self) {
        emit_batch(&mut self.file, &[input_event(EV_SYN, SYN_REPORT, 0)]);
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

fn char_to_key(c: char) -> Option<(u16, bool)> {
    match c {
        'a' => Some((KEY_A, false)),
        'b' => Some((KEY_B, false)),
        'c' => Some((KEY_C, false)),
        'd' => Some((KEY_D, false)),
        'e' => Some((KEY_E, false)),
        'f' => Some((KEY_F, false)),
        'g' => Some((KEY_G, false)),
        'h' => Some((KEY_H, false)),
        'i' => Some((KEY_I, false)),
        'j' => Some((KEY_J, false)),
        'k' => Some((KEY_K, false)),
        'l' => Some((KEY_L, false)),
        'm' => Some((KEY_M, false)),
        'n' => Some((KEY_N, false)),
        'o' => Some((KEY_O, false)),
        'p' => Some((KEY_P, false)),
        'q' => Some((KEY_Q, false)),
        'r' => Some((KEY_R, false)),
        's' => Some((KEY_S, false)),
        't' => Some((KEY_T, false)),
        'u' => Some((KEY_U, false)),
        'v' => Some((KEY_V, false)),
        'w' => Some((KEY_W, false)),
        'x' => Some((KEY_X, false)),
        'y' => Some((KEY_Y, false)),
        'z' => Some((KEY_Z, false)),
        'A' => Some((KEY_A, true)),
        'B' => Some((KEY_B, true)),
        'C' => Some((KEY_C, true)),
        'D' => Some((KEY_D, true)),
        'E' => Some((KEY_E, true)),
        'F' => Some((KEY_F, true)),
        'G' => Some((KEY_G, true)),
        'H' => Some((KEY_H, true)),
        'I' => Some((KEY_I, true)),
        'J' => Some((KEY_J, true)),
        'K' => Some((KEY_K, true)),
        'L' => Some((KEY_L, true)),
        'M' => Some((KEY_M, true)),
        'N' => Some((KEY_N, true)),
        'O' => Some((KEY_O, true)),
        'P' => Some((KEY_P, true)),
        'Q' => Some((KEY_Q, true)),
        'R' => Some((KEY_R, true)),
        'S' => Some((KEY_S, true)),
        'T' => Some((KEY_T, true)),
        'U' => Some((KEY_U, true)),
        'V' => Some((KEY_V, true)),
        'W' => Some((KEY_W, true)),
        'X' => Some((KEY_X, true)),
        'Y' => Some((KEY_Y, true)),
        'Z' => Some((KEY_Z, true)),
        '1' => Some((KEY_1, false)),
        '2' => Some((KEY_2, false)),
        '3' => Some((KEY_3, false)),
        '4' => Some((KEY_4, false)),
        '5' => Some((KEY_5, false)),
        '6' => Some((KEY_6, false)),
        '7' => Some((KEY_7, false)),
        '8' => Some((KEY_8, false)),
        '9' => Some((KEY_9, false)),
        '0' => Some((KEY_0, false)),
        '!' => Some((KEY_1, true)),
        '@' => Some((KEY_2, true)),
        '#' => Some((KEY_3, true)),
        '$' => Some((KEY_4, true)),
        '%' => Some((KEY_5, true)),
        '^' => Some((KEY_6, true)),
        '&' => Some((KEY_7, true)),
        '*' => Some((KEY_8, true)),
        '(' => Some((KEY_9, true)),
        ')' => Some((KEY_0, true)),
        '-' => Some((KEY_MINUS, false)),
        '_' => Some((KEY_MINUS, true)),
        '=' => Some((KEY_EQUAL, false)),
        '+' => Some((KEY_EQUAL, true)),
        '[' => Some((KEY_LEFTBRACE, false)),
        '{' => Some((KEY_LEFTBRACE, true)),
        ']' => Some((KEY_RIGHTBRACE, false)),
        '}' => Some((KEY_RIGHTBRACE, true)),
        ';' => Some((KEY_SEMICOLON, false)),
        ':' => Some((KEY_SEMICOLON, true)),
        '\'' => Some((KEY_APOSTROPHE, false)),
        '"' => Some((KEY_APOSTROPHE, true)),
        '`' => Some((KEY_GRAVE, false)),
        '~' => Some((KEY_GRAVE, true)),
        '\\' => Some((KEY_BACKSLASH, false)),
        '|' => Some((KEY_BACKSLASH, true)),
        ',' => Some((KEY_COMMA, false)),
        '<' => Some((KEY_COMMA, true)),
        '.' => Some((KEY_DOT, false)),
        '>' => Some((KEY_DOT, true)),
        '/' => Some((KEY_SLASH, false)),
        '?' => Some((KEY_SLASH, true)),
        ' ' => Some((KEY_SPACE, false)),
        '\t' => Some((KEY_TAB, false)),
        '\n' => Some((KEY_ENTER, false)),
        _ => None,
    }
}

const ALL_KEYS: &[u16] = &[
    KEY_ESC,
    KEY_1,
    KEY_2,
    KEY_3,
    KEY_4,
    KEY_5,
    KEY_6,
    KEY_7,
    KEY_8,
    KEY_9,
    KEY_0,
    KEY_MINUS,
    KEY_EQUAL,
    KEY_BACKSPACE,
    KEY_TAB,
    KEY_Q,
    KEY_W,
    KEY_E,
    KEY_R,
    KEY_T,
    KEY_Y,
    KEY_U,
    KEY_I,
    KEY_O,
    KEY_P,
    KEY_LEFTBRACE,
    KEY_RIGHTBRACE,
    KEY_ENTER,
    KEY_LEFTCTRL,
    KEY_A,
    KEY_S,
    KEY_D,
    KEY_F,
    KEY_G,
    KEY_H,
    KEY_J,
    KEY_K,
    KEY_L,
    KEY_SEMICOLON,
    KEY_APOSTROPHE,
    KEY_GRAVE,
    KEY_LEFTSHIFT,
    KEY_BACKSLASH,
    KEY_Z,
    KEY_X,
    KEY_C,
    KEY_V,
    KEY_B,
    KEY_N,
    KEY_M,
    KEY_COMMA,
    KEY_DOT,
    KEY_SLASH,
    KEY_SPACE,
    KEY_LEFTALT,
    KEY_LEFTMETA,
    KEY_UP,
    KEY_LEFT,
    KEY_RIGHT,
    KEY_DOWN,
    KEY_DELETE,
    KEY_INSERT,
    KEY_HOME,
    KEY_END,
    KEY_CAPSLOCK,
    KEY_RIGHTSHIFT,
    KEY_RIGHTCTRL,
    KEY_RIGHTALT,
    KEY_RIGHTMETA,
    KEY_F1,
    KEY_F2,
    KEY_F3,
    KEY_F4,
    KEY_F5,
    KEY_F6,
    KEY_F7,
    KEY_F8,
    KEY_F9,
    KEY_F10,
    KEY_F11,
    KEY_F12,
    KEY_SCROLLLOCK,
    KEY_NUMLOCK,
    KEY_PAGEUP,
    KEY_PAGEDOWN,
    KEY_SYSRQ,
];
