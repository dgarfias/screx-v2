use std::os::fd::OwnedFd;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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

#[derive(Debug, Clone, Copy)]
pub struct DmaBufPlane {
    pub object_index: usize,
    pub offset: u32,
    pub pitch: u32,
}

#[derive(Debug)]
pub struct DmaBufObject {
    pub fd: OwnedFd,
    pub size: u64,
    pub modifier: u64,
}

#[derive(Debug)]
pub struct DmaBufCursor {
    pub fb_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub pixel_format: u32,
    pub fd: OwnedFd,
    pub size: u64,
    pub modifier: u64,
    pub pitch: u32,
    pub offset: u32,
}

#[derive(Debug)]
pub struct DmaBufFrame {
    pub fb_id: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub objects: Vec<DmaBufObject>,
    pub planes: Vec<DmaBufPlane>,
    pub cursor: Option<DmaBufCursor>,
}

#[derive(Debug)]
pub enum FrameData<'a> {
    Bgra(&'a [u8]),
    DmaBuf(&'a DmaBufFrame),
}

#[derive(Debug)]
/// One captured frame, either CPU BGRA pixels or a zero-copy GPU DMA-BUF.
pub struct CaptureFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub data: FrameData<'a>,
}

impl<'a> CaptureFrame<'a> {
    pub fn bgra(width: u32, height: u32, data: &'a [u8]) -> Self {
        Self {
            width,
            height,
            data: FrameData::Bgra(data),
        }
    }

    pub fn dmabuf(width: u32, height: u32, dmabuf: &'a DmaBufFrame) -> Self {
        Self {
            width,
            height,
            data: FrameData::DmaBuf(dmabuf),
        }
    }

    pub fn bgra_bytes(&self) -> Option<&'a [u8]> {
        match self.data {
            FrameData::Bgra(bytes) => Some(bytes),
            FrameData::DmaBuf(_) => None,
        }
    }

    pub fn is_dmabuf(&self) -> bool {
        matches!(self.data, FrameData::DmaBuf(_))
    }
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
    /// Shared toggle enabling DRM PRIME zero-copy capture; the pipeline may
    /// clear it to fall back to CPU capture if VA-API rejects the DMA-BUF.
    pub zero_copy: Arc<AtomicBool>,
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
        Arc::clone(&config.zero_copy),
    )?))
}
