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
}

impl LinuxMuxClient {
    pub fn new() -> Self {
        Self { iproxy_child: None }
    }
}

impl MuxClient for LinuxMuxClient {
    fn device_present(&mut self) -> bool {
        Command::new("idevice_id")
            .arg("-l")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                o.status.success() && out.lines().any(|l| !l.trim().is_empty())
            })
            .unwrap_or(false)
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
