use std::fs::File;
use std::io::Write;
use std::os::unix::io::FromRawFd;
use std::process::Command;

use anyhow::{Context, Result};

use crate::camera::CameraBackend;

const VIDEO_DEVICE: &str = "/dev/video10";
const CARD_LABEL: &str = "Screx Camera";

extern "C" {
    fn screx_v4l2_open_output(
        device: *const libc::c_char,
        width: libc::c_int,
        height: libc::c_int,
        fps: libc::c_int,
    ) -> libc::c_int;
}

pub struct V4l2Camera {
    file: Option<File>,
    exclusive_caps: bool,
}

impl V4l2Camera {
    pub fn new(exclusive_caps: bool) -> Self {
        Self {
            file: None,
            exclusive_caps,
        }
    }
}

impl CameraBackend for V4l2Camera {
    fn start(&mut self, w: u32, h: u32, fps: u32) -> Result<()> {
        ensure_v4l2loopback(self.exclusive_caps)?;
        std::thread::sleep(std::time::Duration::from_millis(500));

        let device = std::ffi::CString::new(VIDEO_DEVICE).unwrap();
        let fd = unsafe {
            screx_v4l2_open_output(
                device.as_ptr(),
                w as libc::c_int,
                h as libc::c_int,
                fps as libc::c_int,
            )
        };

        match fd {
            -1 => anyhow::bail!(
                "failed to open {VIDEO_DEVICE}: {}",
                std::io::Error::last_os_error()
            ),
            -2 => anyhow::bail!(
                "VIDIOC_S_FMT failed on {VIDEO_DEVICE}: {}",
                std::io::Error::last_os_error()
            ),
            -3 => anyhow::bail!(
                "VIDIOC_S_PARM failed on {VIDEO_DEVICE}: {}",
                std::io::Error::last_os_error()
            ),
            fd if fd < 0 => anyhow::bail!("v4l2 setup failed (code {fd})"),
            fd => {
                let file = unsafe { File::from_raw_fd(fd) };
                println!("[camera] writer ready: {w}x{h} @ {fps}fps MJPEG -> {VIDEO_DEVICE}");
                self.file = Some(file);
                Ok(())
            }
        }
    }

    fn write_jpeg(&mut self, jpeg: &[u8]) -> Result<()> {
        if let Some(ref mut f) = self.file {
            f.write_all(jpeg).context("v4l2 write failed")?;
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.file = None;
    }
}

fn ensure_v4l2loopback(exclusive_caps: bool) -> Result<()> {
    if std::path::Path::new(VIDEO_DEVICE).exists() {
        return Ok(());
    }

    let exclusive_caps_arg = if exclusive_caps {
        "exclusive_caps=1"
    } else {
        "exclusive_caps=0"
    };
    println!("[camera] loading v4l2loopback with {exclusive_caps_arg}");

    let status = Command::new("modprobe")
        .args([
            "v4l2loopback",
            "video_nr=10",
            &format!("card_label={CARD_LABEL}"),
            exclusive_caps_arg,
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
