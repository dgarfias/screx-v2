//! Compatibility re-exports — the shared input parsing and keyboard worker
//! types now live in `crate::input`. Platform-specific backends live in
//! `crate::platform::{linux,windows}::uinput`.

pub use crate::input::{
    parse_key_event, parse_mouse_packet, parse_rawkey_event, parse_touch_packet, KeyboardEvent,
    KeyboardWorker, MouseEvent, TouchContact, DIRECT_MOUSE_ENABLED, SPECIAL_BACKSPACE,
};
