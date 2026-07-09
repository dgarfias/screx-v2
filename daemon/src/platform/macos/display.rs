//! macOS display backend — STUB.
//!
//! Real implementation (CGVirtualDisplay for the virtual monitor,
//! ScreenCaptureKit for frame capture, VideoToolbox-friendly pixel buffer
//! plumbing) lands in M1. This module exists only so the crate type-checks
//! and runs end-to-end on macOS with honest "not yet implemented" errors.

use anyhow::{bail, Result};
use std::sync::atomic::AtomicBool;

use crate::capture::{CaptureFrame, DisplayBackend, DisplayMode};

#[allow(dead_code)]
pub struct MacDisplay {
    width: u32,
    height: u32,
    fps: u32,
}

impl MacDisplay {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        Ok(Self {
            width,
            height,
            fps,
        })
    }
}

impl DisplayBackend for MacDisplay {
    fn attach(&mut self, _mode: DisplayMode) -> Result<()> {
        bail!("macOS virtual display not yet implemented — lands in M1")
    }

    fn run_capture_loop(
        &mut self,
        _stop: &AtomicBool,
        _force_refresh: &AtomicBool,
        _on_frame: &mut dyn FnMut(CaptureFrame<'_>),
    ) -> Result<()> {
        bail!("macOS capture loop not yet implemented — lands in M1")
    }

    fn detach(&mut self) {}
}
