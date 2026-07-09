//! macOS USB mux client — STUB.
//!
//! USB transport (shelling out to `iproxy`/`idevice_id`, mirroring
//! `platform::linux::usbmux`) is a deferred milestone, not in scope for this
//! pass. `device_present()` always reports `false` so `usb::run_usb_transport`
//! just polls harmlessly forever without ever calling `connect()`.

use anyhow::{bail, Result};

use crate::usb::{MuxClient, ReadWriteStream};

pub struct MacMuxClient;

impl MacMuxClient {
    pub fn new() -> Self {
        Self
    }
}

impl MuxClient for MacMuxClient {
    fn device_present(&mut self) -> bool {
        false
    }

    fn connect(&mut self, _device_port: u16) -> Result<Box<dyn ReadWriteStream>> {
        bail!("USB transport not yet implemented on macOS")
    }
}
