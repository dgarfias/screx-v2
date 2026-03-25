//! Input adapter — raw keyboard passthrough and absolute mouse injection.
//!
//! Keyboard: converts platform key events to HID usage codes and sends RAWKEY
//! control payloads over encrypted TCP.
//!
//! Mouse: converts QML mouse position to absolute coordinates (0-65535 range)
//! and sends MOUSE control payloads over encrypted TCP.

use anyhow::Result;

use crate::backend::ControlSender;

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
    let mut payload = Vec::with_capacity(RAWKEY_MAGIC.len() + 3);
    payload.extend_from_slice(RAWKEY_MAGIC);
    payload.extend_from_slice(&hid_usage.to_be_bytes());
    payload.push(if pressed { 1 } else { 0 });
    control.send_payload(&payload)
}

/// Sends an absolute mouse move to the daemon.
/// `x`, `y` are in the range 0-65535 (full virtual display area).
pub fn send_mouse_abs(control: &ControlSender, x: u16, y: u16) -> Result<()> {
    let mut payload = Vec::with_capacity(MOUSE_MAGIC.len() + 5);
    payload.extend_from_slice(MOUSE_MAGIC);
    payload.push(MOUSE_MOVE_ABS);
    payload.extend_from_slice(&x.to_be_bytes());
    payload.extend_from_slice(&y.to_be_bytes());
    control.send_payload(&payload)
}

/// Sends a mouse button event to the daemon.
/// `button`: 1=left, 2=right, 3=middle.
/// `pressed`: true for button-down, false for button-up.
pub fn send_mouse_button(control: &ControlSender, button: u8, pressed: bool) -> Result<()> {
    let mut payload = Vec::with_capacity(MOUSE_MAGIC.len() + 3);
    payload.extend_from_slice(MOUSE_MAGIC);
    payload.push(MOUSE_BUTTON);
    payload.push(button);
    payload.push(if pressed { 1 } else { 0 });
    control.send_payload(&payload)
}

/// Sends a mouse scroll event to the daemon.
/// `dy`: signed scroll delta (positive = up, negative = down).
pub fn send_mouse_scroll(control: &ControlSender, dy: i16) -> Result<()> {
    let mut payload = Vec::with_capacity(MOUSE_MAGIC.len() + 3);
    payload.extend_from_slice(MOUSE_MAGIC);
    payload.push(MOUSE_SCROLL);
    payload.extend_from_slice(&dy.to_be_bytes());
    control.send_payload(&payload)
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
        // Equal =
        0x3d => Some(0x2e),
        // Left bracket [
        0x5b => Some(0x2f),
        // Right bracket ]
        0x5d => Some(0x30),
        // Backslash
        0x5c => Some(0x31),
        // Semicolon ;
        0x3b => Some(0x33),
        // Apostrophe '
        0x27 => Some(0x34),
        // Grave `
        0x60 => Some(0x35),
        // Comma ,
        0x2c => Some(0x36),
        // Period .
        0x2e => Some(0x37),
        // Slash /
        0x2f => Some(0x38),
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
