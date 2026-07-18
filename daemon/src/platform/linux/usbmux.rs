use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::usb::{MuxClient, ReadWriteStream};

const IPROXY_LOCAL_PORT: u16 = 9001;
const DEVICE_PORT: u16 = 9000;

pub struct LinuxMuxClient {
    iproxy_child: Option<Child>,
    /// Set once we've warned that `idevice_id` is missing, so the 2-second
    /// poll loop doesn't spam the log every tick.
    warned_missing: bool,
}

impl LinuxMuxClient {
    pub fn new() -> Self {
        Self {
            iproxy_child: None,
            warned_missing: false,
        }
    }
}

impl MuxClient for LinuxMuxClient {
    fn device_present(&mut self) -> bool {
        match Command::new("idevice_id")
            .arg("-l")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        {
            Ok(o) => {
                let out = String::from_utf8_lossy(&o.stdout);
                o.status.success() && out.lines().any(|l| !l.trim().is_empty())
            }
            // Without this, a missing `idevice_id` silently and permanently
            // disables USB — the daemon just polls dormantly forever with
            // no indication anything is wrong. Surface it once.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !self.warned_missing => {
                eprintln!(
                    "[usb] idevice_id not found — USB transport disabled. \
                     Install libimobiledevice-utils (e.g. apt install \
                     libimobiledevice-utils usbmuxd) and restart the daemon."
                );
                self.warned_missing = true;
                false
            }
            Err(_) => false,
        }
    }

    fn connect(&mut self, _device_port: u16) -> Result<Box<dyn ReadWriteStream>> {
        if self.iproxy_child.is_none() {
            let child = Command::new("iproxy")
                .arg(format!("{IPROXY_LOCAL_PORT}:{DEVICE_PORT}"))
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .context("failed to start iproxy — is libimobiledevice-utils installed?")?;
            println!(
                "[usb] iproxy started (pid {}): localhost:{} -> device:{}",
                child.id(),
                IPROXY_LOCAL_PORT,
                DEVICE_PORT
            );
            std::thread::sleep(Duration::from_millis(500));
            self.iproxy_child = Some(child);
        }

        let addr = format!("127.0.0.1:{IPROXY_LOCAL_PORT}");
        let stream = TcpStream::connect(&addr)
            .with_context(|| format!("failed to connect to iproxy at {addr}"))?;
        stream.set_nodelay(true).ok();
        Ok(Box::new(stream))
    }
}

impl Drop for LinuxMuxClient {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.iproxy_child {
            let pid = child.id();
            let _ = child.kill();
            let _ = child.wait();
            println!("[usb] iproxy stopped (pid {pid})");
        }
    }
}

impl ReadWriteStream for TcpStream {
    fn try_clone_box(&self) -> Option<Box<dyn ReadWriteStream>> {
        self.try_clone()
            .ok()
            .map(|s| Box::new(s) as Box<dyn ReadWriteStream>)
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        std::net::TcpStream::set_read_timeout(self, timeout)
    }
}
