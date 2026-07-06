// ---------------------------------------------------------------------------
// Windows USB mux client.
//
// On Linux, the daemon shells out to idevice_id / iproxy (libimobiledevice)
// to detect an attached iPad and to get a local TCP proxy to its on-device
// port 9000. Windows has no such CLI tooling installed by default, but
// iTunes and the Apple Devices app both install "Apple Mobile Device
// Service" (AMDS) — a background service that speaks the exact same
// usbmuxd wire protocol Linux's usbmuxd speaks, just over a local TCP
// socket (127.0.0.1:27015) instead of the Unix domain socket
// /var/run/usbmuxd.
//
// This file speaks that protocol natively instead of shelling out:
//   - Every message is a 16-byte little-endian header — total length
//     (including the header), protocol version (1), message type (8 for a
//     plist payload), and a tag that is echoed back in the reply — followed
//     by an XML plist dictionary payload.
//   - "ListDevices" enumerates attached devices; we look for one whose
//     Properties.ConnectionType is "USB" and remember its DeviceID.
//   - "Connect" asks AMDS to turn the *same* socket into a raw byte pipe to
//     a port on the device (port given in network byte order). Once it
//     replies with Number == 0, the socket is a byte-identical stand-in for
//     what iproxy's local TCP port gives Linux, and gets handed to the same
//     shared usb.rs state machine unmodified.
// ---------------------------------------------------------------------------

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use plist::{Dictionary, Value};

use crate::usb::{MuxClient, ReadWriteStream};

const AMDS_ADDR: &str = "127.0.0.1:27015";
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PLIST_LEN: usize = 1024 * 1024;
const PROG_NAME: &str = "screx";
const CLIENT_VERSION: &str = "screx-1";

pub struct WindowsMuxClient {
    last_device_id: Option<u32>,
    warned_no_amds: bool,
    last_present: bool,
    tag: u32,
}

impl WindowsMuxClient {
    pub fn new() -> Self {
        Self {
            last_device_id: None,
            warned_no_amds: false,
            last_present: false,
            tag: 0,
        }
    }

    fn next_tag(&mut self) -> u32 {
        self.tag = self.tag.wrapping_add(1);
        self.tag
    }

    /// Sends ListDevices on `stream` and returns the DeviceID of the first
    /// USB-attached device, if any.
    fn list_devices(&mut self, stream: &mut TcpStream) -> Result<Option<u32>> {
        let mut req = Dictionary::new();
        req.insert(
            "MessageType".to_string(),
            Value::String("ListDevices".to_string()),
        );
        req.insert("ProgName".to_string(), Value::String(PROG_NAME.to_string()));
        req.insert(
            "ClientVersionString".to_string(),
            Value::String(CLIENT_VERSION.to_string()),
        );

        let tag = self.next_tag();
        send_plist(stream, tag, &Value::Dictionary(req))?;
        let reply = recv_plist(stream)?;

        let dict = reply
            .as_dictionary()
            .context("usbmux: ListDevices reply was not a dictionary")?;
        let devices = dict
            .get("DeviceList")
            .and_then(Value::as_array)
            .context("usbmux: ListDevices reply missing DeviceList")?;

        for dev in devices {
            let dev_dict = match dev.as_dictionary() {
                Some(d) => d,
                None => continue,
            };
            let props = match dev_dict.get("Properties").and_then(Value::as_dictionary) {
                Some(p) => p,
                None => continue,
            };
            let conn_type = props
                .get("ConnectionType")
                .and_then(Value::as_string)
                .unwrap_or("");
            if conn_type.eq_ignore_ascii_case("USB") {
                if let Some(id) = dev_dict.get("DeviceID").and_then(Value::as_unsigned_integer) {
                    return Ok(Some(id as u32));
                }
            }
        }
        Ok(None)
    }
}

/// Opens a fresh connection to Apple Mobile Device Service.
fn mux_connect() -> std::io::Result<TcpStream> {
    let addr = AMDS_ADDR.parse().expect("AMDS_ADDR is a valid socket addr");
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

fn send_plist(stream: &mut TcpStream, tag: u32, value: &Value) -> Result<()> {
    let mut payload = Vec::new();
    value
        .to_writer_xml(&mut payload)
        .context("usbmux: failed to serialize plist request")?;

    let total_len = (16 + payload.len()) as u32;
    let mut header = [0u8; 16];
    header[0..4].copy_from_slice(&total_len.to_le_bytes());
    header[4..8].copy_from_slice(&1u32.to_le_bytes()); // protocol version
    header[8..12].copy_from_slice(&8u32.to_le_bytes()); // message type: plist
    header[12..16].copy_from_slice(&tag.to_le_bytes());

    stream
        .write_all(&header)
        .context("usbmux: failed to write request header")?;
    stream
        .write_all(&payload)
        .context("usbmux: failed to write request payload")?;
    Ok(())
}

fn recv_plist(stream: &mut TcpStream) -> Result<Value> {
    let mut header = [0u8; 16];
    stream
        .read_exact(&mut header)
        .context("usbmux: failed to read reply header")?;

    let total_len = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
    if total_len < 16 || total_len > MAX_PLIST_LEN {
        bail!("usbmux: reply length {total_len} out of bounds");
    }

    let mut payload = vec![0u8; total_len - 16];
    stream
        .read_exact(&mut payload)
        .context("usbmux: failed to read reply payload")?;

    Value::from_reader_xml(&payload[..]).context("usbmux: failed to parse reply plist")
}

impl MuxClient for WindowsMuxClient {
    fn device_present(&mut self) -> bool {
        let mut stream = match mux_connect() {
            Ok(s) => s,
            Err(_) => {
                if !self.warned_no_amds {
                    eprintln!(
                        "[usb] Apple Mobile Device Service not reachable on 127.0.0.1:27015 — install iTunes or the Apple Devices app for USB mode"
                    );
                    self.warned_no_amds = true;
                }
                if self.last_present {
                    println!("[usb] lost connection to Apple Mobile Device Service");
                    self.last_present = false;
                }
                self.last_device_id = None;
                return false;
            }
        };

        match self.list_devices(&mut stream) {
            Ok(Some(id)) => {
                self.last_device_id = Some(id);
                if !self.last_present {
                    println!("[usb] iOS device detected via usbmuxd (device id {id})");
                    self.last_present = true;
                }
                true
            }
            Ok(None) => {
                self.last_device_id = None;
                if self.last_present {
                    println!("[usb] iOS device no longer reported by usbmuxd");
                    self.last_present = false;
                }
                false
            }
            Err(_) => {
                self.last_device_id = None;
                if self.last_present {
                    println!("[usb] usbmuxd ListDevices failed, treating device as absent");
                    self.last_present = false;
                }
                false
            }
        }
    }

    fn connect(&mut self, device_port: u16) -> Result<Box<dyn ReadWriteStream>> {
        let device_id = match self.last_device_id {
            Some(id) => id,
            None => {
                let mut probe = mux_connect()
                    .context("usbmux: failed to connect to Apple Mobile Device Service")?;
                let id = self
                    .list_devices(&mut probe)?
                    .context("usbmux: no USB device present")?;
                self.last_device_id = Some(id);
                id
            }
        };

        let mut stream = mux_connect()
            .context("usbmux: failed to connect to Apple Mobile Device Service")?;

        let mut req = Dictionary::new();
        req.insert(
            "MessageType".to_string(),
            Value::String("Connect".to_string()),
        );
        req.insert("ProgName".to_string(), Value::String(PROG_NAME.to_string()));
        req.insert(
            "ClientVersionString".to_string(),
            Value::String(CLIENT_VERSION.to_string()),
        );
        req.insert("DeviceID".to_string(), Value::from(device_id));
        // usbmuxd wants the device port in network (big-endian) byte order,
        // stored as a plain integer.
        req.insert(
            "PortNumber".to_string(),
            Value::from(device_port.to_be() as u32),
        );

        let tag = self.next_tag();
        send_plist(&mut stream, tag, &Value::Dictionary(req))?;
        let reply = recv_plist(&mut stream)?;

        let dict = reply
            .as_dictionary()
            .context("usbmux: Connect reply was not a dictionary")?;
        let result = dict
            .get("Number")
            .and_then(Value::as_unsigned_integer)
            .context("usbmux: Connect reply missing Number")?;

        if result != 0 {
            let meaning = match result {
                2 => "device gone",
                3 => "iPad app not in USB mode yet",
                5 => "malformed request",
                _ => "unknown error",
            };
            bail!("usbmux: Connect failed, code {result} ({meaning})");
        }

        stream
            .set_read_timeout(None)
            .context("usbmux: failed to clear read timeout")?;
        stream
            .set_write_timeout(None)
            .context("usbmux: failed to clear write timeout")?;
        stream.set_nodelay(true).ok();

        Ok(Box::new(stream))
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
