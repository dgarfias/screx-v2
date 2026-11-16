use std::sync::atomic::AtomicBool;

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Debug)]
/// One captured BGRA frame.
pub struct CaptureFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
}

/// Backend-specific display control and frame capture.
pub trait DisplayBackend {
    /// Plug the virtual monitor at exactly this mode. Blocks until the OS sees it.
    fn attach(&mut self, mode: DisplayMode) -> Result<()>;

    /// Pump frames until `stop` is true. Must honor force_refresh (resend last/black frame)
    /// so the iPad never starves.
    fn run_capture_loop(
        &mut self,
        stop: &AtomicBool,
        force_refresh: &AtomicBool,
        on_frame: &mut dyn FnMut(CaptureFrame<'_>),
    ) -> Result<()>;

    /// Unplug the monitor.
    fn detach(&mut self);
}

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Create the EVDI virtual-display backend.
pub fn create_display_backend(config: &CaptureConfig) -> Result<Box<dyn DisplayBackend + Send>> {
    Ok(Box::new(crate::platform::linux::evdi::EvdiDisplay::new(
        config.width,
        config.height,
        config.fps,
    )?))
}
