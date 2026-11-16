use std::sync::{Arc, Mutex};

use crate::input::{GamepadState, InputBackend, MouseEvent, TouchContact};
use crate::stream_server::{FeatureToggles, SharedState};

struct NoopInput;
impl InputBackend for NoopInput {
    fn set_stream_size(&mut self, _: u32, _: u32) {}
    fn touch(&mut self, _: &[TouchContact]) -> anyhow::Result<()> {
        Ok(())
    }
    fn key_text(&mut self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn key_special(&mut self, _: u8, _: u8) -> anyhow::Result<()> {
        Ok(())
    }
    fn key_text_with_modifiers(&mut self, _: &str, _: u8) -> anyhow::Result<()> {
        Ok(())
    }
    fn key_raw_hid(&mut self, _: u8, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn mouse(&mut self, _: MouseEvent) -> anyhow::Result<()> {
        Ok(())
    }
    fn release_mouse_buttons(&mut self) {}
    fn release_keys(&mut self) {}
    fn gamepad_attach(&mut self, _: u8) -> anyhow::Result<()> {
        Ok(())
    }
    fn gamepad_detach(&mut self, _: u8) {}
    fn gamepad_state(&mut self, _: u8, _: &GamepadState) -> anyhow::Result<()> {
        Ok(())
    }
}

pub fn shared_state() -> Arc<SharedState> {
    Arc::new(SharedState::new(
        false,
        20_000_000,
        Arc::new(Mutex::new(NoopInput)),
        3840,
        2160,
        60,
        20_000_000,
        crate::encode::EncoderBackend::Software,
        FeatureToggles {
            speaker: false,
            microphone: false,
            camera: false,
            gamepad: false,
        },
    ))
}
