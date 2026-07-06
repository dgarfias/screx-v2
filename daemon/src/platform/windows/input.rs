use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
    VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_INSERT, VK_LCONTROL,
    VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT,
    VK_RWIN, VK_SHIFT, VK_TAB, VK_UP,
};
use windows::Win32::UI::Input::Pointer::{
    InitializeTouchInjection, InjectTouchInput, POINTER_FLAGS, POINTER_FLAG_DOWN,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
    POINTER_TOUCH_INFO, TOUCH_FEEDBACK_NONE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PT_TOUCH, SM_CXSCREEN, SM_CYSCREEN, TOUCH_FLAG_NONE, TOUCH_MASK_CONTACTAREA,
};

use crate::input::{
    GamepadState, InputBackend, MouseEvent, TouchContact, SPECIAL_BACKSPACE, TOUCH_DOWN,
    TOUCH_MOVE, TOUCH_UP,
};
use crate::platform::windows::vigem::VirtualGamepads;

/// InitializeTouchInjection's contact-count limit; matches the plan (10 slots).
const MAX_TOUCH_SLOTS: usize = 10;

/// Per-slot state the touch-keepalive thread needs to keep re-injecting
/// contacts that are held down without new wire packets arriving for them —
/// Windows cancels a contact that doesn't get periodic UPDATEs.
struct TouchState {
    /// (left, top, width, height) of the captured output on screen.
    rect: (i32, i32, u32, u32),
    /// Screen-coordinate position of each currently-down contact, by slot.
    slots: [Option<(i32, i32)>; MAX_TOUCH_SLOTS],
}

pub struct WindowsInput {
    left_button_down: AtomicBool,
    right_button_down: AtomicBool,
    middle_button_down: AtomicBool,
    touch_initialized: AtomicBool,
    touch_state: Arc<Mutex<TouchState>>,
    keepalive_stop: Arc<AtomicBool>,
    keepalive_thread: Option<thread::JoinHandle<()>>,
    gamepads: VirtualGamepads,
}

impl WindowsInput {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let touch_state = Arc::new(Mutex::new(TouchState {
            rect: (0, 0, width, height),
            slots: [None; MAX_TOUCH_SLOTS],
        }));
        let keepalive_stop = Arc::new(AtomicBool::new(false));

        let thread_state = Arc::clone(&touch_state);
        let thread_stop = Arc::clone(&keepalive_stop);
        let keepalive_thread = thread::Builder::new()
            .name("touch-keepalive".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                    if let Ok(state) = thread_state.lock() {
                        let no_overrides = [None; MAX_TOUCH_SLOTS];
                        if let Err(e) = inject_slots(&state.slots, &no_overrides) {
                            crate::vlog!("[input] touch keepalive inject failed: {e:#}");
                        }
                    }
                }
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn touch keepalive thread: {e}"))?;

        Ok(Self {
            left_button_down: AtomicBool::new(false),
            right_button_down: AtomicBool::new(false),
            middle_button_down: AtomicBool::new(false),
            touch_initialized: AtomicBool::new(false),
            touch_state,
            keepalive_stop,
            keepalive_thread: Some(keepalive_thread),
            gamepads: VirtualGamepads::new(),
        })
    }

    fn screen_size(&self) -> (i32, i32) {
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
    }

    fn ensure_touch_initialized(&self) -> Result<()> {
        if self.touch_initialized.load(Ordering::SeqCst) {
            return Ok(());
        }
        unsafe {
            InitializeTouchInjection(MAX_TOUCH_SLOTS as u32, TOUCH_FEEDBACK_NONE)
                .map_err(|e| anyhow::anyhow!("InitializeTouchInjection failed: {e}"))?;
        }
        self.touch_initialized.store(true, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for WindowsInput {
    fn drop(&mut self) {
        self.keepalive_stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.keepalive_thread.take() {
            let _ = handle.join();
        }
    }
}

fn make_touch_info(pointer_id: u32, flags: u32, x: i32, y: i32) -> POINTER_TOUCH_INFO {
    let mut info: POINTER_TOUCH_INFO = unsafe { std::mem::zeroed() };
    info.pointerInfo.pointerType = PT_TOUCH;
    info.pointerInfo.pointerId = pointer_id;
    info.pointerInfo.pointerFlags = POINTER_FLAGS(flags);
    info.pointerInfo.ptPixelLocation = POINT { x, y };
    info.touchFlags = TOUCH_FLAG_NONE;
    info.touchMask = TOUCH_MASK_CONTACTAREA;
    const RADIUS: i32 = 4;
    info.rcContact = RECT {
        left: x - RADIUS,
        top: y - RADIUS,
        right: x + RADIUS,
        bottom: y + RADIUS,
    };
    info
}

/// Builds the *complete* set of currently-active contacts and injects them in
/// one InjectTouchInput call — Windows requires the full active set every
/// time, not just the contacts that changed. `overrides` carries the one-shot
/// DOWN/UP flags for slots that transitioned this call; everything else that's
/// still down gets a plain UPDATE so Windows doesn't time it out.
fn inject_slots(
    slots: &[Option<(i32, i32)>; MAX_TOUCH_SLOTS],
    overrides: &[Option<(u32, i32, i32)>; MAX_TOUCH_SLOTS],
) -> Result<()> {
    let mut infos = Vec::new();
    for slot in 0..MAX_TOUCH_SLOTS {
        if let Some((flags, x, y)) = overrides[slot] {
            infos.push(make_touch_info(slot as u32, flags, x, y));
        } else if let Some((x, y)) = slots[slot] {
            let flags = POINTER_FLAG_UPDATE.0 | POINTER_FLAG_INRANGE.0 | POINTER_FLAG_INCONTACT.0;
            infos.push(make_touch_info(slot as u32, flags, x, y));
        }
    }
    if infos.is_empty() {
        return Ok(());
    }
    unsafe {
        InjectTouchInput(&infos).map_err(|e| anyhow::anyhow!("InjectTouchInput failed: {e}"))?;
    }
    Ok(())
}

impl InputBackend for WindowsInput {
    fn set_target_rect(&mut self, left: i32, top: i32, width: u32, height: u32) {
        let mut state = self.touch_state.lock().unwrap();
        state.rect = (left, top, width, height);
    }

    fn touch(&mut self, contacts: &[TouchContact]) -> Result<()> {
        if contacts.is_empty() {
            return Ok(());
        }
        self.ensure_touch_initialized()?;

        let mut state = self.touch_state.lock().unwrap();
        let (left, top, _, _) = state.rect;

        // One-shot DOWN/UP overrides for slots that transitioned this call;
        // slots left at None here just get a plain UPDATE in inject_slots()
        // if they're still down, matching Windows' "resend the whole active
        // set every time" requirement.
        let mut overrides: [Option<(u32, i32, i32)>; MAX_TOUCH_SLOTS] = [None; MAX_TOUCH_SLOTS];

        for c in contacts {
            let slot = c.slot as usize;
            if slot >= MAX_TOUCH_SLOTS {
                crate::vlog!("[input] touch slot {slot} out of range, ignoring");
                continue;
            }
            // Wire coordinates are already in the daemon's target width/height
            // space (the iPad scales to that before sending) — just offset by
            // where the captured output actually sits on screen.
            let x = left + c.x as i32;
            let y = top + c.y as i32;

            if c.event_type == TOUCH_DOWN {
                overrides[slot] = Some((
                    POINTER_FLAG_DOWN.0 | POINTER_FLAG_INRANGE.0 | POINTER_FLAG_INCONTACT.0,
                    x,
                    y,
                ));
                state.slots[slot] = Some((x, y));
            } else if c.event_type == TOUCH_MOVE {
                state.slots[slot] = Some((x, y));
            } else if c.event_type == TOUCH_UP {
                let (last_x, last_y) = state.slots[slot].unwrap_or((x, y));
                overrides[slot] = Some((POINTER_FLAG_UP.0, last_x, last_y));
                state.slots[slot] = None;
            } else {
                crate::vlog!("[input] unknown touch event_type {}", c.event_type);
            }
        }

        inject_slots(&state.slots, &overrides)
    }

    fn key_text(&mut self, text: &str) -> Result<()> {
        let mut inputs: Vec<INPUT> = Vec::with_capacity(text.encode_utf16().count() * 2);
        for code_unit in text.encode_utf16() {
            inputs.push(keyboard_input(KEYEVENTF_UNICODE, VIRTUAL_KEY(0), code_unit));
            inputs.push(keyboard_input(
                KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                VIRTUAL_KEY(0),
                code_unit,
            ));
        }
        unsafe { send_inputs(&inputs) };
        Ok(())
    }

    fn key_special(&mut self, code: u8, modifiers: u8) -> Result<()> {
        // Mirrors special_to_keycode() in platform/linux/uinput.rs 1:1 — that
        // table is the behavioral spec for this wire format.
        let vk = match code {
            SPECIAL_BACKSPACE => VK_BACK,
            0x02 => VK_RETURN,
            0x03 => VK_TAB,
            0x04 => VK_ESCAPE,
            0x05 => VK_LEFT,
            0x06 => VK_RIGHT,
            0x07 => VK_UP,
            0x08 => VK_DOWN,
            0x09 => VK_DELETE,
            0x0A => VK_HOME,
            0x0B => VK_END,
            0x0C => VK_LCONTROL,
            0x0D => VK_LMENU,
            0x0E => VK_LWIN,
            0x0F => VK_INSERT,
            _ => {
                crate::vlog!("[input] unknown special key code 0x{code:02x}");
                return Ok(());
            }
        };
        press_key(vk, modifiers);
        Ok(())
    }

    fn key_text_with_modifiers(&mut self, text: &str, modifiers: u8) -> Result<()> {
        let mods = modifier_vks(modifiers);
        press_modifiers(&mods);
        let _ = self.key_text(text);
        release_modifiers(&mods);
        Ok(())
    }

    fn key_raw_hid(&mut self, usage: u8, pressed: bool) -> Result<()> {
        if let Some(vk) = hid_usage_to_vk(usage) {
            let flag = if pressed {
                KEYBD_EVENT_FLAGS(0)
            } else {
                KEYEVENTF_KEYUP
            };
            let input = keyboard_input(flag, vk, 0);
            unsafe { send_inputs(&[input]) };
        }
        Ok(())
    }

    fn mouse(&mut self, ev: MouseEvent) -> Result<()> {
        let mut input = unsafe { std::mem::zeroed::<INPUT>() };
        input.r#type = INPUT_MOUSE;
        let mut mi = unsafe { std::mem::zeroed::<MOUSEINPUT>() };

        match ev {
            MouseEvent::MoveRelative { dx, dy } => {
                mi.dwFlags = MOUSEEVENTF_MOVE;
                mi.dx = dx as i32;
                mi.dy = dy as i32;
            }
            MouseEvent::MoveAbsolute { x, y } => {
                let (sw, sh) = self.screen_size();
                if sw > 0 && sh > 0 {
                    mi.dwFlags = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE;
                    mi.dx = ((x as i32 * 65535) / sw).clamp(0, 65535);
                    mi.dy = ((y as i32 * 65535) / sh).clamp(0, 65535);
                }
            }
            MouseEvent::Button { btn, state } => {
                let down = state != 0;
                match btn {
                    0 => {
                        mi.dwFlags = if down {
                            MOUSEEVENTF_LEFTDOWN
                        } else {
                            MOUSEEVENTF_LEFTUP
                        };
                        self.left_button_down.store(down, Ordering::SeqCst);
                    }
                    1 => {
                        mi.dwFlags = if down {
                            MOUSEEVENTF_RIGHTDOWN
                        } else {
                            MOUSEEVENTF_RIGHTUP
                        };
                        self.right_button_down.store(down, Ordering::SeqCst);
                    }
                    2 => {
                        mi.dwFlags = if down {
                            MOUSEEVENTF_MIDDLEDOWN
                        } else {
                            MOUSEEVENTF_MIDDLEUP
                        };
                        self.middle_button_down.store(down, Ordering::SeqCst);
                    }
                    _ => return Ok(()),
                }
            }
            MouseEvent::Scroll { dy } => {
                mi.dwFlags = MOUSEEVENTF_WHEEL;
                mi.mouseData = (dy * 120) as u32;
            }
        }

        unsafe {
            input.Anonymous = INPUT_0 { mi };
            send_inputs(&[input]);
        }
        Ok(())
    }

    fn gamepad_attach(&mut self, id: u8) -> Result<()> {
        self.gamepads.attach(id)
    }

    fn gamepad_detach(&mut self, id: u8) {
        self.gamepads.detach(id);
    }

    fn gamepad_state(&mut self, id: u8, st: &GamepadState) -> Result<()> {
        self.gamepads.state(id, st)
    }
}

fn keyboard_input(flags: KEYBD_EVENT_FLAGS, vk: VIRTUAL_KEY, scan: u16) -> INPUT {
    let mut input = unsafe { std::mem::zeroed::<INPUT>() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous = INPUT_0 {
        ki: KEYBDINPUT {
            wVk: vk,
            wScan: scan,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: 0,
        },
    };
    input
}

unsafe fn send_inputs(inputs: &[INPUT]) {
    if !inputs.is_empty() {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn press_key(vk: VIRTUAL_KEY, modifiers: u8) {
    let mods = modifier_vks(modifiers);
    press_modifiers(&mods);
    unsafe { send_inputs(&[keyboard_input(KEYBD_EVENT_FLAGS(0), vk, 0)]) };
    unsafe { send_inputs(&[keyboard_input(KEYEVENTF_KEYUP, vk, 0)]) };
    release_modifiers(&mods);
}

fn modifier_vks(modifiers: u8) -> Vec<VIRTUAL_KEY> {
    let mut vks = Vec::new();
    // Bit layout matches the iPad protocol: shift=1, ctrl=2, alt=4, cmd/meta=8.
    if modifiers & 0x01 != 0 {
        vks.push(VK_SHIFT);
    }
    if modifiers & 0x02 != 0 {
        vks.push(VK_CONTROL);
    }
    if modifiers & 0x04 != 0 {
        vks.push(VK_LMENU);
    }
    if modifiers & 0x08 != 0 {
        // Windows key; map to VK_LWIN when available. For now use VK_MENU as a placeholder.
        vks.push(VK_LMENU);
    }
    vks
}

fn press_modifiers(mods: &[VIRTUAL_KEY]) {
    let inputs: Vec<_> = mods
        .iter()
        .map(|vk| keyboard_input(KEYBD_EVENT_FLAGS(0), *vk, 0))
        .collect();
    unsafe { send_inputs(&inputs) };
}

fn release_modifiers(mods: &[VIRTUAL_KEY]) {
    let inputs: Vec<_> = mods
        .iter()
        .map(|vk| keyboard_input(KEYEVENTF_KEYUP, *vk, 0))
        .collect();
    unsafe { send_inputs(&inputs) };
}

/// Map HID keyboard usage page 0x07 values to Windows virtual-key codes.
fn hid_usage_to_vk(usage: u8) -> Option<VIRTUAL_KEY> {
    let vk: u16 = match usage {
        0x00 => 0x00,
        0x01 => 0x01,                                     // ErrorRollOver
        0x02 => 0x02,                                     // POSTFail
        0x03 => 0x03,                                     // ErrorUndefined
        0x04..=0x1D => 0x41u16 + ((usage - 0x04) as u16), // A-Z
        // HID usage 0x1E-0x26 is "1".."9" (VK '1'-'9' = 0x31-0x39), and 0x27
        // is "0" (VK '0' = 0x30) — the row wraps around at the end, it isn't
        // a flat offset from 0x1E. The old `0x30 + (usage - 0x1E)` formula
        // shifted the whole row down by one (physical 1 sent VK '0', physical
        // 0 sent VK '9').
        0x1E..=0x26 => 0x31u16 + ((usage - 0x1E) as u16), // 1-9
        0x27 => 0x30,                                     // 0
        0x28 => 0x0D,                                     // Return
        0x29 => 0x1B,                                     // Escape
        0x2A => 0x08,                                     // Backspace
        0x2B => 0x09,                                     // Tab
        0x2C => 0x20,                                     // Space
        0x2D => 0xBD,                                     // Minus
        0x2E => 0xBB,                                     // Equal
        0x2F => 0xDB,                                     // LeftBrace
        0x30 => 0xDD,                                     // RightBrace
        0x31 => 0xDC,                                     // Backslash
        0x33 => 0xBA,                                     // Semicolon
        0x34 => 0xDE,                                     // Quote
        0x35 => 0xC0,                                     // Grave
        0x36 => 0xBC,                                     // Comma
        0x37 => 0xBE,                                     // Dot
        0x38 => 0xBF,                                     // Slash
        0x39 => 0x14,                                     // CapsLock
        0x3A..=0x45 => 0x70u16 + ((usage - 0x3A) as u16), // F1-F12
        0x46 => 0x2C,                                     // PrintScreen
        0x47 => 0x91,                                     // ScrollLock
        0x48 => 0x13,                                     // Pause
        0x49 => 0x2D,                                     // Insert
        0x4A => 0x24,                                     // Home
        0x4B => 0x21,                                     // PageUp
        0x4C => 0x2E,                                     // Delete
        0x4D => 0x23,                                     // End
        0x4E => 0x22,                                     // PageDown
        0x4F => 0x27,                                     // Right
        0x50 => 0x25,                                     // Left
        0x51 => 0x28,                                     // Down
        0x52 => 0x26,                                     // Up
        0x54 => 0x6F,                                     // Keypad /
        0x55 => 0x6A,                                     // Keypad *
        0x56 => 0x6D,                                     // Keypad -
        0x57 => 0x6B,                                     // Keypad +
        0x58 => 0x0D,                                     // Keypad Enter
        0x59 => 0x61,                                     // Keypad 1
        0x5A => 0x62,
        0x5B => 0x63,
        0x5C => 0x64,
        0x5D => 0x65,
        0x5E => 0x66,
        0x5F => 0x67,
        0x60 => 0x68,
        0x61 => 0x69,
        0x62 => 0x60, // Keypad 0
        0x63 => 0x6E, // Keypad .
        0x64 => 0x5C, // Non-US backslash
        0x75 => 0xAE, // Help
        0x7F => 0x12, // Mute (APP2 placeholder)
        0x80 => 0xAD, // VolumeUp
        0x81 => 0xAE, // VolumeDown
        0x88 => 0x7B, // Keyboard Int'l 1
        // Named constants here, not magic hex — this exact block previously had
        // Left Control/Shift/Alt and Right Shift shuffled to the wrong VK codes
        // (e.g. Right Shift pointed at VK_LMENU), which is why right shift
        // didn't work on an external keyboard.
        0xE0 => VK_LCONTROL.0, // Left Control
        0xE1 => VK_LSHIFT.0,   // Left Shift
        0xE2 => VK_LMENU.0,    // Left Alt
        0xE3 => VK_LWIN.0,     // Left GUI
        0xE4 => VK_RCONTROL.0, // Right Control
        0xE5 => VK_RSHIFT.0,   // Right Shift
        0xE6 => VK_RMENU.0,    // Right Alt
        0xE7 => VK_RWIN.0,     // Right GUI
        _ => return None,
    };
    Some(VIRTUAL_KEY(vk))
}
