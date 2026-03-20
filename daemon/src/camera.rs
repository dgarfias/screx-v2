use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::process::Command;

use anyhow::{Context, Result};

const VIDEO_DEVICE: &str = "/dev/video10";
const CARD_LABEL: &str = "Screx iPad Camera";
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

// v4l2 constants
const VIDIOC_S_FMT: libc::c_ulong = 0xc0d05605;
const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const V4L2_PIX_FMT_MJPEG: u32 = u32::from_le_bytes(*b"MJPG");
const V4L2_FIELD_NONE: u32 = 1;

#[repr(C)]
#[derive(Default)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    // union padding
    ycbcr_enc_or_quantization: [u32; 2],
}

#[repr(C)]
struct V4l2Format {
    type_: u32,
    pix: V4l2PixFormat,
    // pad to full struct size (v4l2_format is 208 bytes)
    _pad: [u8; 208 - 4 - std::mem::size_of::<V4l2PixFormat>()],
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
    // Check if already loaded with our device
    if std::path::Path::new(VIDEO_DEVICE).exists() {
        println!("[camera] {VIDEO_DEVICE} already exists");
        return Ok(());
    }

    let status = Command::new("modprobe")
        .args([
            "v4l2loopback",
            &format!("video_nr=10"),
            &format!("card_label={CARD_LABEL}"),
            "exclusive_caps=1",
            "max_buffers=2",
        ])
        .status()
        .context("failed to run modprobe v4l2loopback")?;

    if !status.success() {
        anyhow::bail!("modprobe v4l2loopback failed — is v4l2loopback-dkms installed?");
    }

    // Wait for device node
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
    let file = OpenOptions::new()
        .write(true)
        .open(VIDEO_DEVICE)
        .with_context(|| format!("failed to open {VIDEO_DEVICE}"))?;

    let fd = file.as_raw_fd();

    let mut fmt = V4l2Format {
        type_: V4L2_BUF_TYPE_VIDEO_OUTPUT,
        pix: V4l2PixFormat {
            width: WIDTH,
            height: HEIGHT,
            pixelformat: V4L2_PIX_FMT_MJPEG,
            field: V4L2_FIELD_NONE,
            sizeimage: WIDTH * HEIGHT * 2,
            ..Default::default()
        },
        _pad: [0u8; 208 - 4 - std::mem::size_of::<V4l2PixFormat>()],
    };

    let ret = unsafe { libc::ioctl(fd, VIDIOC_S_FMT, &mut fmt) };
    if ret < 0 {
        anyhow::bail!(
            "VIDIOC_S_FMT failed: {}",
            std::io::Error::last_os_error()
        );
    }

    println!("[camera] writer ready: {WIDTH}x{HEIGHT} MJPEG -> {VIDEO_DEVICE}");
    Ok(CamWriter { file })
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
