//! Input adapter — raw keyboard passthrough and absolute mouse injection.
//!
//! Keyboard: converts platform key events to HID usage codes and sends RAWKEY
//! control payloads over encrypted TCP.
//!
//! Mouse: converts QML mouse position to absolute coordinates (0-65535 range)
//! and sends MOUSE control payloads over encrypted TCP.

use anyhow::Result;

use crate::backend::ControlSender;

const KEY_MAGIC: &[u8] = b"KEY";
const RAWKEY_MAGIC: &[u8] = b"RAWKEY";
const MOUSE_MAGIC: &[u8] = b"MOUSE";

// Mouse subtypes matching daemon/src/uinput.rs
const MOUSE_MOVE_ABS: u8 = 0x04;
const MOUSE_BUTTON: u8 = 0x02;
const MOUSE_SCROLL: u8 = 0x03;

/// Sends a raw key event to the daemon.
/// `hid_usage`: USB HID usage code (e.g. 0x04 = 'a', 0x28 = Enter).
/// `pressed`: true for key-down, false for key-up.
pub fn send_raw_key(control: &ControlSender, hid_usage: u16, pressed: bool) -> Result<()> {
    let mut buf = [0u8; 32];
    let len = RAWKEY_MAGIC.len() + 3;
    buf[..RAWKEY_MAGIC.len()].copy_from_slice(RAWKEY_MAGIC);
    buf[RAWKEY_MAGIC.len()..RAWKEY_MAGIC.len() + 2].copy_from_slice(&hid_usage.to_be_bytes());
    buf[RAWKEY_MAGIC.len() + 2] = if pressed { 1 } else { 0 };
    control.send_payload(&buf[..len])
}

pub fn send_key_text(control: &ControlSender, text: &str) -> Result<()> {
    let mut payload = Vec::with_capacity(KEY_MAGIC.len() + 1 + text.len());
    payload.extend_from_slice(KEY_MAGIC);
    payload.push(0x01);
    payload.extend_from_slice(text.as_bytes());
    control.send_payload(&payload)
}

pub fn send_key_special(control: &ControlSender, code: u8) -> Result<()> {
    let mut buf = [0u8; 16];
    let len = KEY_MAGIC.len() + 2;
    buf[..KEY_MAGIC.len()].copy_from_slice(KEY_MAGIC);
    buf[KEY_MAGIC.len()] = 0x02;
    buf[KEY_MAGIC.len() + 1] = code;
    control.send_payload(&buf[..len])
}

pub fn send_key_combo_text(control: &ControlSender, mods: u8, text: &str) -> Result<()> {
    let mut payload = Vec::with_capacity(KEY_MAGIC.len() + 3 + text.len());
    payload.extend_from_slice(KEY_MAGIC);
    payload.push(0x04);
    payload.push(mods);
    payload.push(0x01);
    payload.extend_from_slice(text.as_bytes());
    control.send_payload(&payload)
}

pub fn send_key_combo_special(control: &ControlSender, mods: u8, code: u8) -> Result<()> {
    let mut payload = Vec::with_capacity(KEY_MAGIC.len() + 4);
    payload.extend_from_slice(KEY_MAGIC);
    payload.push(0x04);
    payload.push(mods);
    payload.push(0x02);
    payload.push(code);
    control.send_payload(&payload)
}

pub fn send_qt_key_action(
    control: &ControlSender,
    qt_key: i32,
    text: &str,
    modifiers: i32,
    pressed: bool,
) -> Result<()> {
    // Modifier keys (Shift, Ctrl, Alt, Meta) should NOT be sent as RAWKEY
    // because our KEY combo packets already encode modifier state.
    // Sending RAWKEY for modifiers causes them to get "stuck" on the remote side.
    if qt_key_is_modifier(qt_key) {
        return Ok(());
    }

    let combo_mods = qt_modifiers_to_combo(modifiers, qt_key);
    let special = qt_key_to_special(qt_key);
    let text = sanitize_key_text(text);

    if pressed {
        if combo_mods != 0 {
            if let Some(code) = special {
                return send_key_combo_special(control, combo_mods, code);
            }
            if let Some(text) = text.as_deref() {
                return send_key_combo_text(control, combo_mods, text);
            }
        } else {
            if let Some(code) = special {
                return send_key_special(control, code);
            }
            if let Some(text) = text.as_deref() {
                return send_key_text(control, text);
            }
        }
    }

    // KEY text/special packets are press-only (like iPad soft keyboard).
    // No release event needed for those.
    if special.is_some() || text.is_some() {
        return Ok(());
    }

    // Fallback: keys with no text and no special mapping use RAWKEY
    if let Some(hid_usage) = qt_key_to_hid(qt_key) {
        return send_raw_key(control, hid_usage, pressed);
    }

    Ok(())
}

/// Sends an absolute mouse move to the daemon.
/// `x`, `y` are in the range 0-65535 (full virtual display area).
pub fn send_mouse_abs(control: &ControlSender, x: u16, y: u16) -> Result<()> {
    let mut buf = [0u8; 16];
    let m = MOUSE_MAGIC.len();
    buf[..m].copy_from_slice(MOUSE_MAGIC);
    buf[m] = MOUSE_MOVE_ABS;
    buf[m + 1..m + 3].copy_from_slice(&x.to_be_bytes());
    buf[m + 3..m + 5].copy_from_slice(&y.to_be_bytes());
    control.send_payload(&buf[..m + 5])
}

pub fn send_mouse_button(control: &ControlSender, button: u8, pressed: bool) -> Result<()> {
    let mut buf = [0u8; 16];
    let m = MOUSE_MAGIC.len();
    buf[..m].copy_from_slice(MOUSE_MAGIC);
    buf[m] = MOUSE_BUTTON;
    buf[m + 1] = button;
    buf[m + 2] = if pressed { 1 } else { 0 };
    control.send_payload(&buf[..m + 3])
}

pub fn send_mouse_scroll(control: &ControlSender, dy: i16) -> Result<()> {
    let mut buf = [0u8; 16];
    let m = MOUSE_MAGIC.len();
    buf[..m].copy_from_slice(MOUSE_MAGIC);
    buf[m] = MOUSE_SCROLL;
    buf[m + 1..m + 3].copy_from_slice(&dy.to_be_bytes());
    control.send_payload(&buf[..m + 3])
}

/// Maps a Qt key code to a USB HID usage code.
/// This covers the most common keys; extend as needed.
pub fn qt_key_to_hid(qt_key: i32) -> Option<u16> {
    // Qt::Key constants (subset)
    // Full mapping table: USB HID Usage Tables, Section 10
    match qt_key {
        // Letters A-Z: Qt::Key_A = 0x41 .. Key_Z = 0x5a → HID 0x04..0x1d
        0x41..=0x5a => Some((qt_key - 0x41 + 0x04) as u16),
        // Digits 1-9: Qt::Key_1 = 0x31 .. Key_9 = 0x39 → HID 0x1e..0x26
        0x31..=0x39 => Some((qt_key - 0x31 + 0x1e) as u16),
        // 0: Qt::Key_0 = 0x30 → HID 0x27
        0x30 => Some(0x27),
        // Enter
        0x01000004 => Some(0x28), // Qt::Key_Return
        0x01000005 => Some(0x58), // Qt::Key_Enter (keypad)
        // Escape
        0x01000000 => Some(0x29),
        // Backspace
        0x01000003 => Some(0x2a),
        // Tab
        0x01000001 => Some(0x2b),
        // Space
        0x20 => Some(0x2c),
        // Minus -
        0x2d => Some(0x2d),
        // Underscore _ (Shift+-)
        0x5f => Some(0x2d),
        // Equal =
        0x3d => Some(0x2e),
        // Plus + (Shift+=)
        0x2b => Some(0x2e),
        // Left bracket [
        0x5b => Some(0x2f),
        // Left brace { (Shift+[)
        0x7b => Some(0x2f),
        // Right bracket ]
        0x5d => Some(0x30),
        // Right brace } (Shift+])
        0x7d => Some(0x30),
        // Backslash
        0x5c => Some(0x31),
        // Pipe | (Shift+\)
        0x7c => Some(0x31),
        // Semicolon ;
        0x3b => Some(0x33),
        // Colon : (Shift+;)
        0x3a => Some(0x33),
        // Apostrophe '
        0x27 => Some(0x34),
        // Double quote " (Shift+')
        0x22 => Some(0x34),
        // Grave `
        0x60 => Some(0x35),
        // Tilde ~ (Shift+`)
        0x7e => Some(0x35),
        // Comma ,
        0x2c => Some(0x36),
        // Less-than < (Shift+,)
        0x3c => Some(0x36),
        // Period .
        0x2e => Some(0x37),
        // Greater-than > (Shift+.)
        0x3e => Some(0x37),
        // Slash /
        0x2f => Some(0x38),
        // Question mark ? (Shift+/)
        0x3f => Some(0x38),
        // Exclamation ! (Shift+1)
        0x21 => Some(0x1e),
        // At @ (Shift+2)
        0x40 => Some(0x1f),
        // Hash # (Shift+3)
        0x23 => Some(0x20),
        // Dollar $ (Shift+4)
        0x24 => Some(0x21),
        // Percent % (Shift+5)
        0x25 => Some(0x22),
        // Caret ^ (Shift+6)
        0x5e => Some(0x23),
        // Ampersand & (Shift+7)
        0x26 => Some(0x24),
        // Asterisk * (Shift+8)
        0x2a => Some(0x25),
        // Left paren (Shift+9)
        0x28 => Some(0x26),
        // Right paren (Shift+0)
        0x29 => Some(0x27),
        // Caps Lock
        0x01000024 => Some(0x39),
        // F1-F12: Qt::Key_F1 = 0x01000030 → HID 0x3a
        0x01000030..=0x0100003b => Some((qt_key - 0x01000030 + 0x3a) as u16),
        // Print Screen
        0x01000009 => Some(0x46),
        // Scroll Lock
        0x01000026 => Some(0x47),
        // Pause
        0x01000008 => Some(0x48),
        // Insert
        0x01000006 => Some(0x49),
        // Home
        0x01000010 => Some(0x4a),
        // Page Up
        0x01000016 => Some(0x4b),
        // Delete
        0x01000007 => Some(0x4c),
        // End
        0x01000011 => Some(0x4d),
        // Page Down
        0x01000017 => Some(0x4e),
        // Arrow Right
        0x01000014 => Some(0x4f),
        // Arrow Left
        0x01000012 => Some(0x50),
        // Arrow Down
        0x01000015 => Some(0x51),
        // Arrow Up
        0x01000013 => Some(0x52),
        // Num Lock
        0x01000025 => Some(0x53),
        // Left Control
        0x01000021 => Some(0xe0),
        // Left Shift
        0x01000020 => Some(0xe1),
        // Left Alt
        0x01000023 => Some(0xe2),
        // Left Super/Meta
        0x01000022 => Some(0xe3),
        // Note: Qt does not distinguish left/right for Ctrl/Shift/Alt/Meta by default.
        // Only the left-side HID codes are sent.
        _ => None,
    }
}

pub fn qt_modifiers_to_combo(mods: i32, qt_key: i32) -> u8 {
    let mut out = 0u8;
    if mods & 0x04000000 != 0 && qt_key != 0x01000021 {
        out |= 0x01; // Ctrl
    }
    if mods & 0x08000000 != 0 && qt_key != 0x01000023 {
        out |= 0x02; // Alt
    }
    if mods & 0x10000000 != 0 && qt_key != 0x01000022 {
        out |= 0x04; // Meta
    }
    out
}

fn sanitize_key_text(text: &str) -> Option<String> {
    let filtered: String = text.chars().filter(|c| !c.is_control()).collect();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

fn qt_key_is_modifier(qt_key: i32) -> bool {
    matches!(
        qt_key,
        0x01000020 | // Shift
        0x01000021 | // Ctrl
        0x01000022 | // Meta
        0x01000023 // Alt
    )
}

pub fn qt_key_to_special(qt_key: i32) -> Option<u8> {
    match qt_key {
        0x01000003 => Some(0x01),              // Backspace
        0x01000004 | 0x01000005 => Some(0x02), // Return / Enter
        0x01000001 => Some(0x03),              // Tab
        0x01000000 => Some(0x04),              // Escape
        0x01000012 => Some(0x05),              // Left
        0x01000014 => Some(0x06),              // Right
        0x01000013 => Some(0x07),              // Up
        0x01000015 => Some(0x08),              // Down
        0x01000007 => Some(0x09),              // Delete
        0x01000010 => Some(0x0A),              // Home
        0x01000011 => Some(0x0B),              // End
        0x01000021 => Some(0x0C),              // Ctrl
        0x01000023 => Some(0x0D),              // Alt
        0x01000022 => Some(0x0E),              // Meta
        0x01000006 => Some(0x0F),              // Insert
        _ => None,
    }
}
