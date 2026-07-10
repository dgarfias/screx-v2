//! macOS USB mux client — shells out to libimobiledevice's `idevice_id`/
//! `iproxy` CLI tools, same as `platform::linux::usbmux`. macOS ships Apple's
//! usbmuxd natively; `idevice_id`/`iproxy` come from Homebrew
//! (`brew install libimobiledevice`).
//!
//! NOTE: this file is a near-verbatim copy of `platform::linux::usbmux`
//! (same ports, same child-process lifecycle) — only the error-message
//! wording differs (brew vs. distro package hints). Hoisting the shared
//! logic into a `#[cfg(any(target_os = "linux", target_os = "macos"))]`
//! module is a reasonable future cleanup, but is not worth blocking this
//! milestone on.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::usb::{MuxClient, ReadWriteStream};

const IPROXY_LOCAL_PORT: u16 = 9001;
const DEVICE_PORT: u16 = 9000;

pub struct MacMuxClient {
    iproxy_child: Option<Child>,
}

impl MacMuxClient {
    pub fn new() -> Self {
        Self { iproxy_child: None }
    }
}

impl MuxClient for MacMuxClient {
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
                .context(
                    "failed to start iproxy — is libimobiledevice installed? \
                     (brew install libimobiledevice)",
                )?;
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

impl Drop for MacMuxClient {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual smoke test exercising the real code paths against whatever is
    /// (or isn't) installed on this machine. Ignored by default since it
    /// shells out to external CLI tools and its outcome depends on whether
    /// libimobiledevice is installed and a device is attached.
    ///
    /// On a machine with libimobiledevice missing and no iPad attached (the
    /// expected state in CI / most dev machines), this confirms:
    ///   - `device_present()` returns `false` gracefully (no panic) when
    ///     `idevice_id` isn't found.
    ///   - `connect()` fails with the actionable brew-install hint rather
    ///     than a raw io::Error or a panic.
    ///
    /// Run manually with:
    ///   cargo test --release -- --ignored macos_usbmux_smoke --nocapture
    #[test]
    #[ignore]
    fn macos_usbmux_smoke() {
        let mut mux = MacMuxClient::new();

        let present = mux.device_present();
        println!("[test] device_present() -> {present}");
        // Without libimobiledevice installed (or without a device attached),
        // this must be false — and, critically, must not panic even though
        // `idevice_id` likely isn't on PATH.
        assert!(!present, "expected no device present in this environment");

        match mux.connect(DEVICE_PORT) {
            Ok(_) => {
                // Only plausible if libimobiledevice happens to be installed
                // and iproxy could bind/connect. Not an error either way.
                println!("[test] connect() unexpectedly succeeded (is libimobiledevice installed?)");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                println!("[test] connect() failed as expected: {msg}");
                assert!(
                    msg.contains("brew install libimobiledevice"),
                    "error message should mention the brew install hint, got: {msg}"
                );
            }
        }
    }
}
