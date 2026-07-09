use std::sync::atomic::AtomicBool;

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Debug)]
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

    /// (left, top, width, height) of the captured output in absolute virtual-
    /// desktop coordinates, if the backend has one (Windows only — input
    /// injection needs it to translate wire touch/mouse coordinates, which
    /// are 0..width/0..height relative to the captured frame, into screen
    /// coordinates for SendInput/InjectTouchInput).
    fn output_rect(&self) -> Option<(i32, i32, u32, u32)> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBackend {
    Evdi,
    Vdd,
    #[cfg(target_os = "macos")]
    MacVirtualDisplay,
}

impl CaptureBackend {
    pub fn from_str(value: &str) -> Self {
        let v = value.trim().to_ascii_lowercase();
        match v.as_str() {
            "evdi" => Self::Evdi,
            "vdd" => Self::Vdd,
            "auto" | "" => Self::platform_default(),
            _ => Self::platform_default(),
        }
    }

    /// Platform-native default: EVDI on Linux, MTT VDD on Windows.
    pub fn platform_default() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::Evdi
        }
        #[cfg(target_os = "windows")]
        {
            Self::Vdd
        }
        #[cfg(target_os = "macos")]
        {
            Self::MacVirtualDisplay
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            Self::Evdi
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub backend: CaptureBackend,
}

/// Create a platform display backend for the requested capture backend.
pub fn create_display_backend(config: &CaptureConfig) -> Result<Box<dyn DisplayBackend + Send>> {
    #[cfg(target_os = "linux")]
    {
        match config.backend {
            CaptureBackend::Evdi => Ok(Box::new(crate::platform::linux::evdi::EvdiDisplay::new(
                config.width,
                config.height,
                config.fps,
            )?)),
            // Windows-only variant; on Linux this is treated as "use the
            // native backend" (EVDI) so an explicit --capture-backend=vdd
            // from a cross-platform config doesn't hard-fail.
            CaptureBackend::Vdd => Ok(Box::new(crate::platform::linux::evdi::EvdiDisplay::new(
                config.width,
                config.height,
                config.fps,
            )?)),
        }
    }
    #[cfg(target_os = "windows")]
    {
        // Windows always uses the MTT VDD based capture path regardless of
        // the requested variant (EVDI is Linux-only; map it to Vdd).
        Ok(Box::new(
            crate::platform::windows::display::WindowsDisplay::new(
                config.width,
                config.height,
                config.fps,
            )?,
        ))
    }
    #[cfg(target_os = "macos")]
    {
        // macOS always uses the ScreenCaptureKit/CGVirtualDisplay based
        // capture path regardless of the requested variant (Evdi/Vdd are
        // Linux/Windows-only; map any request to MacVirtualDisplay).
        let _ = config.backend;
        Ok(Box::new(crate::platform::macos::display::MacDisplay::new(
            config.width,
            config.height,
            config.fps,
        )?))
    }
}
