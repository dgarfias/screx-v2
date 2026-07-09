//! macOS audio backend — STUB.
//!
//! `probe_mic_available()` and `probe_speaker_available()` both report
//! `false` on macOS for now (mic forwarding needs a third-party virtual
//! audio driver; speaker/system-audio capture is a separate ScreenCaptureKit
//! milestone, M3), so in practice these methods should never be invoked.
//! They still need bodies for the crate to type-check.

use anyhow::{bail, Result};
use std::sync::atomic::AtomicBool;

use crate::audio::{AudioBackend, MicSink};

pub struct MacAudioBackend;

impl MacAudioBackend {
    pub fn new() -> Self {
        Self
    }
}

impl AudioBackend for MacAudioBackend {
    fn create_sink(&mut self) -> Result<()> {
        bail!("macOS speaker forwarding not yet implemented")
    }

    fn destroy_sink(&mut self) {}

    fn run_loopback(&mut self, _stop: &AtomicBool, _on_chunk: &mut dyn FnMut(&[u8])) -> Result<()> {
        bail!("macOS speaker forwarding not yet implemented")
    }

    fn create_mic(&mut self) -> Result<Box<dyn MicSink>> {
        bail!("macOS mic forwarding not yet implemented")
    }

    fn destroy_mic(&mut self) {}
}
