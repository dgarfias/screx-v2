use std::sync::atomic::AtomicBool;

use anyhow::Result;

/// Stable output identities, independent of EVDI card numbers and worker lifetimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DisplayId {
    Internal = 0,
    External = 1,
}

impl DisplayId {
    pub fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Internal),
            1 => Some(Self::External),
            _ => None,
        }
    }
}

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
    pub display_id: DisplayId,
    pub monitor_name: [u8; 13],
    /// Panel size the client reported; `None` when unknown (the external display).
    pub physical_size_mm: Option<(u16, u16)>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// The monitor's size as EDID stores it: whole centimetres, 1..=255. Compositors read the
/// size back from the kernel as these centimetres times ten, so anything that must match the
/// monitor's size (the touchscreen) has to use `cm * 10` mm, not the client's exact mm.
pub fn edid_size_cm((width_mm, height_mm): (u16, u16)) -> (u8, u8) {
    let cm = |mm: u16| ((mm + 5) / 10).clamp(1, 255) as u8;
    (cm(width_mm), cm(height_mm))
}

/// Create the EVDI virtual-display backend.
pub fn create_display_backend(config: &CaptureConfig) -> Result<Box<dyn DisplayBackend + Send>> {
    Ok(Box::new(crate::platform::linux::evdi::EvdiDisplay::new(
        config.display_id,
        config.monitor_name,
        config.physical_size_mm,
        config.width,
        config.height,
        config.fps,
    )?))
}
