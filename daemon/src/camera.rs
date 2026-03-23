use std::fs::File;
use std::io::Write;
use std::os::unix::io::FromRawFd;
use std::process::Command;

use anyhow::{Context, Result};

const VIDEO_DEVICE: &str = "/dev/video10";
const CARD_LABEL: &str = "Screx iPad Camera";
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

extern "C" {
    fn screx_v4l2_open_output(
        device: *const libc::c_char,
        width: libc::c_int,
        height: libc::c_int,
    ) -> libc::c_int;
}


pub struct CamWriter {
    file: File,
}

impl CamWriter {
    pub fn write_frame(&mut self, jpeg: &[u8]) {
        let _ = self.file.write_all(jpeg);
    }
}

pub fn load_v4l2loopback() -> Result<()> {
    // Always reload cleanly to avoid stale device state
    if std::path::Path::new(VIDEO_DEVICE).exists() {
        println!("[camera] removing stale v4l2loopback...");
        let _ = Command::new("rmmod").arg("v4l2loopback").status();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let status = Command::new("modprobe")
        .args([
            "v4l2loopback",
            "video_nr=10",
            &format!("card_label={CARD_LABEL}"),
            "exclusive_caps=1",
            "max_buffers=2",
        ])
        .status()
        .context("failed to run modprobe v4l2loopback")?;

    if !status.success() {
        anyhow::bail!("modprobe v4l2loopback failed — is v4l2loopback-dkms installed?");
    }

    for _ in 0..20 {
        if std::path::Path::new(VIDEO_DEVICE).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    if !std::path::Path::new(VIDEO_DEVICE).exists() {
        anyhow::bail!("{VIDEO_DEVICE} did not appear after modprobe");
    }

    println!("[camera] v4l2loopback loaded, {VIDEO_DEVICE} ready");
    Ok(())
}

pub fn create_cam_writer() -> Result<CamWriter> {
    std::thread::sleep(std::time::Duration::from_millis(500));

    let device = std::ffi::CString::new(VIDEO_DEVICE).unwrap();
    let fd = unsafe {
        screx_v4l2_open_output(device.as_ptr(), WIDTH as libc::c_int, HEIGHT as libc::c_int)
    };

    match fd {
        -1 => anyhow::bail!("failed to open {VIDEO_DEVICE}: {}", std::io::Error::last_os_error()),
        -2 => anyhow::bail!("VIDIOC_S_FMT failed on {VIDEO_DEVICE}: {}", std::io::Error::last_os_error()),
        fd if fd < 0 => anyhow::bail!("v4l2 setup failed (code {fd})"),
        fd => {
            let file = unsafe { File::from_raw_fd(fd) };
            println!("[camera] writer ready: {WIDTH}x{HEIGHT} MJPEG -> {VIDEO_DEVICE}");
            Ok(CamWriter { file })
        }
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
