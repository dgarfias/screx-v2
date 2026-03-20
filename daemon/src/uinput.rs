use std::fs::{File, OpenOptions};
use std::io::Write;
use std::mem;
use std::os::unix::io::AsRawFd;

use anyhow::{bail, Context, Result};

const UINPUT_PATH: &str = "/dev/uinput";

const UI_SET_EVBIT: libc::c_ulong = 0x40045564; // _IOW('U', 100, int)
const UI_SET_KEYBIT: libc::c_ulong = 0x40045565; // _IOW('U', 101, int)
const UI_SET_RELBIT: libc::c_ulong = 0x40045566; // _IOW('U', 102, int)
const UI_SET_ABSBIT: libc::c_ulong = 0x40045567; // _IOW('U', 103, int)
const UI_DEV_SETUP: libc::c_ulong = 0x405c5503; // _IOW('U', 3, struct uinput_setup)
const UI_DEV_CREATE: libc::c_ulong = 0x5501; // _IO('U', 1)
const UI_DEV_DESTROY: libc::c_ulong = 0x5502; // _IO('U', 2)

// input event types
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

// absolute axes
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

// relative axes
const REL_WHEEL: u16 = 0x08;
const REL_HWHEEL: u16 = 0x06;

// buttons
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_TOUCH: u16 = 0x14a;

// syn
const SYN_REPORT: u16 = 0x00;

// bus type
const BUS_VIRTUAL: u16 = 0x06;

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UinputAbsSetup {
    code: u16,
    _padding: u16,
    absinfo: InputAbsinfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct InputEvent {
    time_sec: u64,
    time_usec: u64,
    type_: u16,
    code: u16,
    value: i32,
}

pub struct VirtualInput {
    file: File,
}

impl VirtualInput {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open(UINPUT_PATH)
            .context("failed to open /dev/uinput (is the uinput module loaded?)")?;

        let fd = file.as_raw_fd();

        unsafe {
            ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_ulong)?;
            ioctl(fd, UI_SET_EVBIT, EV_ABS as libc::c_ulong)?;
            ioctl(fd, UI_SET_EVBIT, EV_REL as libc::c_ulong)?;
            ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_ulong)?;

            ioctl(fd, UI_SET_KEYBIT, BTN_LEFT as libc::c_ulong)?;
            ioctl(fd, UI_SET_KEYBIT, BTN_RIGHT as libc::c_ulong)?;
            ioctl(fd, UI_SET_KEYBIT, BTN_TOUCH as libc::c_ulong)?;

            ioctl(fd, UI_SET_ABSBIT, ABS_X as libc::c_ulong)?;
            ioctl(fd, UI_SET_ABSBIT, ABS_Y as libc::c_ulong)?;

            ioctl(fd, UI_SET_RELBIT, REL_WHEEL as libc::c_ulong)?;
            ioctl(fd, UI_SET_RELBIT, REL_HWHEEL as libc::c_ulong)?;

            // Configure absolute axes via ABS_SETUP ioctl
                let abs_x = UinputAbsSetup {
                code: ABS_X as u16,
                _padding: 0,
                absinfo: InputAbsinfo {
                    value: 0,
                    minimum: 0,
                    maximum: 65535,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
            };
            let abs_y = UinputAbsSetup {
                code: ABS_Y as u16,
                _padding: 0,
                absinfo: InputAbsinfo {
                    value: 0,
                    minimum: 0,
                    maximum: 65535,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
            };

            // UI_ABS_SETUP = _IOW('U', 4, struct uinput_abs_setup)
            const UI_ABS_SETUP: libc::c_ulong = 0x40185504;
            let ret = libc::ioctl(
                fd,
                UI_ABS_SETUP,
                &abs_x as *const _ as *const libc::c_void,
            );
            if ret < 0 {
                bail!("UI_ABS_SETUP(X) failed: {}", std::io::Error::last_os_error());
            }
            let ret = libc::ioctl(
                fd,
                UI_ABS_SETUP,
                &abs_y as *const _ as *const libc::c_void,
            );
            if ret < 0 {
                bail!("UI_ABS_SETUP(Y) failed: {}", std::io::Error::last_os_error());
            }

            // Device setup
            let mut setup: UinputSetup = mem::zeroed();
            setup.id = InputId {
                bustype: BUS_VIRTUAL,
                vendor: 0x1234,
                product: 0x5678,
                version: 1,
            };
            let name = b"Screx Virtual Touch";
            setup.name[..name.len()].copy_from_slice(name);

            let ret = libc::ioctl(
                fd,
                UI_DEV_SETUP,
                &setup as *const _ as *const libc::c_void,
            );
            if ret < 0 {
                bail!("UI_DEV_SETUP failed: {}", std::io::Error::last_os_error());
            }

            let ret = libc::ioctl(fd, UI_DEV_CREATE, 0);
            if ret < 0 {
                bail!("UI_DEV_CREATE failed: {}", std::io::Error::last_os_error());
            }
        }

        println!(
            "[uinput] virtual input device created: {}x{} (Screx Virtual Touch)",
            width, height
        );

        Ok(Self { file })
    }

    pub fn move_absolute(&mut self, x: u16, y: u16) {
        self.write_event(EV_ABS, ABS_X, x as i32);
        self.write_event(EV_ABS, ABS_Y, y as i32);
        self.write_event(EV_SYN, SYN_REPORT, 0);
    }

    pub fn click(&mut self, x: u16, y: u16) {
        self.write_event(EV_ABS, ABS_X, x as i32);
        self.write_event(EV_ABS, ABS_Y, y as i32);
        self.write_event(EV_KEY, BTN_LEFT, 1);
        self.write_event(EV_KEY, BTN_TOUCH, 1);
        self.write_event(EV_SYN, SYN_REPORT, 0);

        self.write_event(EV_KEY, BTN_LEFT, 0);
        self.write_event(EV_KEY, BTN_TOUCH, 0);
        self.write_event(EV_SYN, SYN_REPORT, 0);
    }

    pub fn button_down(&mut self, x: u16, y: u16, button: u16) {
        self.write_event(EV_ABS, ABS_X, x as i32);
        self.write_event(EV_ABS, ABS_Y, y as i32);
        self.write_event(EV_KEY, button, 1);
        self.write_event(EV_KEY, BTN_TOUCH, 1);
        self.write_event(EV_SYN, SYN_REPORT, 0);
    }

    pub fn button_up(&mut self, button: u16) {
        self.write_event(EV_KEY, button, 0);
        self.write_event(EV_KEY, BTN_TOUCH, 0);
        self.write_event(EV_SYN, SYN_REPORT, 0);
    }

    pub fn drag(&mut self, x: u16, y: u16) {
        self.write_event(EV_ABS, ABS_X, x as i32);
        self.write_event(EV_ABS, ABS_Y, y as i32);
        self.write_event(EV_SYN, SYN_REPORT, 0);
    }

    pub fn scroll(&mut self, x: u16, y: u16, dx: i32, dy: i32) {
        self.write_event(EV_ABS, ABS_X, x as i32);
        self.write_event(EV_ABS, ABS_Y, y as i32);
        if dy != 0 {
            self.write_event(EV_REL, REL_WHEEL, dy);
        }
        if dx != 0 {
            self.write_event(EV_REL, REL_HWHEEL, dx);
        }
        self.write_event(EV_SYN, SYN_REPORT, 0);
    }

    fn write_event(&mut self, type_: u16, code: u16, value: i32) {
        let ev = InputEvent {
            time_sec: 0,
            time_usec: 0,
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(&ev as *const InputEvent as *const u8, mem::size_of::<InputEvent>())
        };
        let _ = self.file.write_all(bytes);
    }
}

impl Drop for VirtualInput {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY, 0);
        }
        println!("[uinput] virtual input device destroyed");
    }
}

unsafe fn ioctl(fd: i32, request: libc::c_ulong, value: libc::c_ulong) -> Result<()> {
    let ret = libc::ioctl(fd, request, value);
    if ret < 0 {
        bail!(
            "ioctl 0x{:x} failed: {}",
            request,
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

// Touch event types matching the iPad protocol
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TouchEventType {
    Tap = 0,
    Down = 1,
    Move = 2,
    Up = 3,
    Scroll = 4,
    RightClick = 5,
}

impl TouchEventType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Tap),
            1 => Some(Self::Down),
            2 => Some(Self::Move),
            3 => Some(Self::Up),
            4 => Some(Self::Scroll),
            5 => Some(Self::RightClick),
            _ => None,
        }
    }
}

/// Parse a touch event packet: "TOUCH" + type(1) + x(u16 BE) + y(u16 BE) + dx(i16 BE) + dy(i16 BE) = 14 bytes
pub fn parse_touch_packet(buf: &[u8], len: usize) -> Option<(TouchEventType, u16, u16, i16, i16)> {
    const MAGIC: &[u8] = b"TOUCH";
    if len < 14 || &buf[..5] != MAGIC {
        return None;
    }
    let event_type = TouchEventType::from_u8(buf[5])?;
    let x = u16::from_be_bytes([buf[6], buf[7]]);
    let y = u16::from_be_bytes([buf[8], buf[9]]);
    let dx = i16::from_be_bytes([buf[10], buf[11]]);
    let dy = i16::from_be_bytes([buf[12], buf[13]]);
    Some((event_type, x, y, dx, dy))
}
