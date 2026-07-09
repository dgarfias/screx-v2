//! macOS input backend — STUB.
//!
//! Real implementation (CGEvent injection for touch/mouse/keyboard) lands in
//! M2. This module exists only so the crate type-checks and runs end-to-end
//! on macOS with honest "not yet implemented" errors. There is no gamepad
//! passthrough backend planned for macOS (see
//! `input::probe_gamepad_max_controllers`), so `gamepad_attach`/
//! `gamepad_state` also just bail.

use anyhow::{bail, Result};

use crate::input::{GamepadState, InputBackend, MouseEvent, TouchContact};

#[allow(dead_code)]
pub struct MacInput {
    width: u32,
    height: u32,
}

impl MacInput {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        Ok(Self { width, height })
    }
}

impl InputBackend for MacInput {
    fn touch(&mut self, _contacts: &[TouchContact]) -> Result<()> {
        bail!("macOS input injection not yet implemented — lands in M2")
    }

    fn key_text(&mut self, _text: &str) -> Result<()> {
        bail!("macOS input injection not yet implemented — lands in M2")
    }

    fn key_special(&mut self, _code: u8, _modifiers: u8) -> Result<()> {
        bail!("macOS input injection not yet implemented — lands in M2")
    }

    fn key_text_with_modifiers(&mut self, _text: &str, _modifiers: u8) -> Result<()> {
        bail!("macOS input injection not yet implemented — lands in M2")
    }

    fn key_raw_hid(&mut self, _usage: u8, _pressed: bool) -> Result<()> {
        bail!("macOS input injection not yet implemented — lands in M2")
    }

    fn mouse(&mut self, _ev: MouseEvent) -> Result<()> {
        bail!("macOS input injection not yet implemented — lands in M2")
    }

    fn gamepad_attach(&mut self, _id: u8) -> Result<()> {
        bail!("macOS input injection not yet implemented — lands in M2")
    }

    fn gamepad_detach(&mut self, _id: u8) {}

    fn gamepad_state(&mut self, _id: u8, _st: &GamepadState) -> Result<()> {
        bail!("macOS input injection not yet implemented — lands in M2")
    }
}
