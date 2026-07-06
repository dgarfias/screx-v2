//! Compatibility re-exports — the shared input parsing and keyboard worker
//! types now live in `crate::input`. Platform-specific backends live in
//! `crate::platform::{linux,windows}::uinput`.

pub use crate::input::{
    parse_key_event, parse_mouse_packet, parse_rawkey_event, parse_touch_packet, GamepadState,
    KeyboardEvent, KeyboardWorker, MouseEvent, TouchContact, DIRECT_MOUSE_ENABLED, GPAD_BTN_EAST,
    GPAD_BTN_MODE, GPAD_BTN_NORTH, GPAD_BTN_SELECT, GPAD_BTN_SOUTH, GPAD_BTN_START,
    GPAD_BTN_THUMBL, GPAD_BTN_THUMBR, GPAD_BTN_TL, GPAD_BTN_TR, SPECIAL_BACKSPACE,
};
