use anyhow::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Platform-specific virtual camera backend.
pub trait CameraBackend: Send {
    fn start(&mut self, w: u32, h: u32, fps: u32) -> Result<()>;
    fn write_jpeg(&mut self, jpeg: &[u8]) -> Result<()>;
    fn stop(&mut self);
}

/// Cheap capability probe: is virtual webcam forwarding likely to work right
/// now? Necessary but not sufficient for an actually-working camera (a
/// not-yet-approved OS permission, or a busy device, can still make the real
/// `CamWriter::new` fail later) — this is just "the underlying driver is
/// present," not a guarantee.
pub fn probe_camera_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        // Mirrors the modprobe check in `platform::linux::v4l2::ensure_v4l2loopback`
        // without actually loading the module: available if it's already
        // loaded, or if modprobe could load it on demand.
        if std::path::Path::new("/sys/module/v4l2loopback").exists() {
            return true;
        }
        std::process::Command::new("modinfo")
            .arg("v4l2loopback")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        crate::platform::windows::vcam::probe_camera_available()
    }
    #[cfg(target_os = "macos")]
    {
        // Virtual camera forwarding is deferred on macOS.
        false
    }
}

/// Create a platform camera backend.
pub fn create_camera_backend(exclusive_caps: bool) -> Box<dyn CameraBackend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(crate::platform::linux::v4l2::V4l2Camera::new(
            exclusive_caps,
        ))
    }
    #[cfg(target_os = "windows")]
    {
        let _ = exclusive_caps;
        Box::new(crate::platform::windows::vcam::WindowsCamera::new())
    }
    #[cfg(target_os = "macos")]
    {
        let _ = exclusive_caps;
        Box::new(MacCameraStub::new())
    }
}

/// Stub camera backend — real virtual-camera forwarding on macOS is
/// deferred (no design work has landed yet); `probe_camera_available()`
/// honestly reports `false` so this backend should never actually be
/// invoked in practice.
#[cfg(target_os = "macos")]
struct MacCameraStub;

#[cfg(target_os = "macos")]
impl MacCameraStub {
    fn new() -> Self {
        Self
    }
}

#[cfg(target_os = "macos")]
impl CameraBackend for MacCameraStub {
    fn start(&mut self, _w: u32, _h: u32, _fps: u32) -> Result<()> {
        anyhow::bail!("camera forwarding not implemented on macOS yet")
    }

    fn write_jpeg(&mut self, _jpeg: &[u8]) -> Result<()> {
        anyhow::bail!("camera forwarding not implemented on macOS yet")
    }

    fn stop(&mut self) {}
}

/// Convenience writer used by `stream_server`.
pub struct CamWriter {
    backend: Box<dyn CameraBackend>,
}

impl CamWriter {
    pub fn new(config: CameraConfig, exclusive_caps: bool) -> Result<Self> {
        let mut backend = create_camera_backend(exclusive_caps);
        backend.start(config.width, config.height, config.fps)?;
        Ok(Self { backend })
    }

    pub fn write_frame(&mut self, jpeg: &[u8]) {
        let _ = self.backend.write_jpeg(jpeg);
    }
}

impl Drop for CamWriter {
    fn drop(&mut self) {
        self.backend.stop();
    }
}

/// Reassembles chunked camera frames from UDP.
/// Format: frame_id(u32 BE) + chunk_idx(u16 BE) + total_chunks(u16 BE) + jpeg_data
pub struct CamReassembler {
    current_frame_id: u32,
    total_chunks: u16,
    received: Vec<Option<Vec<u8>>>,
    received_count: u16,
}

const MAX_CAM_CHUNKS: u16 = 4096;

impl CamReassembler {
    pub fn new() -> Self {
        Self {
            current_frame_id: 0,
            total_chunks: 0,
            received: Vec::new(),
            received_count: 0,
        }
    }

    /// Feed a chunk. Returns the complete JPEG frame when all chunks are received.
    pub fn feed(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < 8 {
            return None;
        }

        let frame_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let chunk_idx = u16::from_be_bytes([data[4], data[5]]) as usize;
        let total = u16::from_be_bytes([data[6], data[7]]);
        let payload = &data[8..];

        if total == 0 || total > MAX_CAM_CHUNKS {
            return None;
        }

        if frame_id != self.current_frame_id || total != self.total_chunks {
            self.current_frame_id = frame_id;
            self.total_chunks = total;
            self.received = vec![None; total as usize];
            self.received_count = 0;
        }

        if chunk_idx < self.received.len() && self.received[chunk_idx].is_none() {
            self.received[chunk_idx] = Some(payload.to_vec());
            self.received_count += 1;
        }

        if self.received_count == self.total_chunks {
            let mut jpeg = Vec::new();
            for chunk in &self.received {
                if let Some(ref c) = chunk {
                    jpeg.extend_from_slice(c);
                }
            }
            self.received = Vec::new();
            self.received_count = 0;
            Some(jpeg)
        } else {
            None
        }
    }
}
