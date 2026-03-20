use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::stream_server::SharedState;

const IPROXY_LOCAL_PORT: u16 = 9001;
const DEVICE_PORT: u16 = 9000;
const DETECT_INTERVAL: Duration = Duration::from_secs(2);
const CONNECT_RETRY: Duration = Duration::from_secs(1);

const MSG_VIDEO: u8 = 0x01;
const MSG_AUDIO: u8 = 0x02;
const MSG_CONTROL: u8 = 0x03;

fn detect_device() -> bool {
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

fn start_iproxy() -> Result<Child> {
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
    Ok(child)
}

fn stop_iproxy(child: &mut Child) {
    let pid = child.id();
    let _ = child.kill();
    let _ = child.wait();
    println!("[usb] iproxy stopped (pid {pid})");
}

/// Thread-safe TCP sender for framed messages over USB.
/// Stored in SharedState behind a Mutex.
pub struct TcpFramedSender {
    stream: TcpStream,
    write_buf: Vec<u8>,
}

impl TcpFramedSender {
    fn new(stream: TcpStream) -> Result<Self> {
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream,
            write_buf: Vec::with_capacity(256 * 1024),
        })
    }

    pub fn send_video(&mut self, annex_b: &[u8], is_idr: bool, timestamp_ms: u32) -> Result<()> {
        let payload_len = 1 + 1 + 4 + annex_b.len(); // type + is_idr + ts + data
        self.write_buf.clear();
        self.write_buf
            .extend_from_slice(&(payload_len as u32).to_be_bytes());
        self.write_buf.push(MSG_VIDEO);
        self.write_buf.push(if is_idr { 1 } else { 0 });
        self.write_buf
            .extend_from_slice(&timestamp_ms.to_be_bytes());
        self.write_buf.extend_from_slice(annex_b);
        self.stream
            .write_all(&self.write_buf)
            .context("USB TCP write (video)")
    }

    pub fn send_audio(&mut self, pcm: &[u8], timestamp_ms: u32) -> Result<()> {
        let payload_len = 1 + 4 + pcm.len(); // type + ts + data
        self.write_buf.clear();
        self.write_buf
            .extend_from_slice(&(payload_len as u32).to_be_bytes());
        self.write_buf.push(MSG_AUDIO);
        self.write_buf
            .extend_from_slice(&timestamp_ms.to_be_bytes());
        self.write_buf.extend_from_slice(pcm);
        self.stream
            .write_all(&self.write_buf)
            .context("USB TCP write (audio)")
    }
}

/// Main USB transport loop. Runs on its own thread.
/// Detects device, starts iproxy, connects TCP, reads control messages.
pub fn run_usb_transport(shared: Arc<SharedState>, stop: Arc<AtomicBool>) {
    println!("[usb] transport thread started (polling for device every {DETECT_INTERVAL:?})");

    while !stop.load(Ordering::Relaxed) {
        if !detect_device() {
            std::thread::sleep(DETECT_INTERVAL);
            continue;
        }

        println!("[usb] iOS device detected via USB");

        let mut iproxy_child = match start_iproxy() {
            Ok(child) => child,
            Err(e) => {
                eprintln!("[usb] failed to start iproxy: {e:#}");
                std::thread::sleep(DETECT_INTERVAL);
                continue;
            }
        };

        let addr = format!("127.0.0.1:{IPROXY_LOCAL_PORT}");

        // Retry TCP connect a few times (iproxy may need a moment)
        let mut tcp_stream = None;
        for attempt in 0..5 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match TcpStream::connect(&addr) {
                Ok(s) => {
                    println!("[usb] TCP connected to {addr} (attempt {attempt})");
                    tcp_stream = Some(s);
                    break;
                }
                Err(_) => {
                    std::thread::sleep(CONNECT_RETRY);
                }
            }
        }

        let stream = match tcp_stream {
            Some(s) => s,
            None => {
                eprintln!("[usb] failed to connect to iproxy at {addr}");
                stop_iproxy(&mut iproxy_child);
                std::thread::sleep(DETECT_INTERVAL);
                continue;
            }
        };

        let read_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[usb] failed to clone TCP stream: {e}");
                stop_iproxy(&mut iproxy_child);
                continue;
            }
        };

        let sender = match TcpFramedSender::new(stream) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[usb] failed to create TCP sender: {e}");
                stop_iproxy(&mut iproxy_child);
                continue;
            }
        };

        // Activate USB transport
        {
            let mut usb = shared.usb_sender.lock().unwrap();
            *usb = Some(sender);
        }
        shared.usb_active.store(true, Ordering::SeqCst);
        shared.force_idr.store(true, Ordering::Relaxed);
        println!("[usb] transport ACTIVE — video/audio will prefer USB");

        // Read control messages from iPad until disconnect
        read_control_loop(read_stream, &shared, &stop);

        // Deactivate USB transport
        shared.usb_active.store(false, Ordering::SeqCst);
        {
            let mut usb = shared.usb_sender.lock().unwrap();
            *usb = None;
        }
        println!("[usb] transport deactivated, falling back to WiFi");

        stop_iproxy(&mut iproxy_child);

        if !stop.load(Ordering::Relaxed) {
            std::thread::sleep(DETECT_INTERVAL);
        }
    }

    println!("[usb] transport thread stopped");
}

fn read_control_loop(
    mut stream: TcpStream,
    shared: &Arc<SharedState>,
    stop: &Arc<AtomicBool>,
) {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok();

    let mut len_buf = [0u8; 4];
    let mut msg_buf = vec![0u8; 256];

    while !stop.load(Ordering::Relaxed) {
        match stream.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => {
                println!("[usb] TCP read disconnected");
                break;
            }
        }

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len == 0 || msg_len > 65536 {
            continue;
        }
        if msg_buf.len() < msg_len {
            msg_buf.resize(msg_len, 0);
        }

        match stream.read_exact(&mut msg_buf[..msg_len]) {
            Ok(()) => {}
            Err(_) => {
                println!("[usb] TCP read disconnected (payload)");
                break;
            }
        }

        if msg_buf[0] == MSG_CONTROL && msg_len >= 2 {
            let ctrl = &msg_buf[1..msg_len];
            if ctrl.starts_with(b"PLI") {
                shared.force_idr.store(true, Ordering::Relaxed);
            } else if ctrl.starts_with(b"TOUCH") && ctrl.len() > 5 {
                let touch_data = &ctrl[5..];
                let mut touch = shared.virtual_touch.lock().unwrap();
                if let Some(ref mut ts) = *touch {
                    crate::uinput::handle_touch_packet(ts, touch_data);
                }
            } else if ctrl.starts_with(b"KEY") && ctrl.len() > 3 {
                let key_data = &ctrl[3..];
                if !crate::uinput::handle_key_packet_no_kb(key_data) {
                    let mut kb = shared.virtual_keyboard.lock().unwrap();
                    if let Some(ref mut keyboard) = *kb {
                        crate::uinput::handle_key_packet(keyboard, key_data);
                    }
                }
            } else if ctrl.starts_with(b"CAM") && ctrl.len() > 3 {
                let jpeg = &ctrl[3..];
                let mut cam = shared.cam_writer.lock().unwrap();
                if let Some(ref mut cw) = *cam {
                    cw.write_frame(jpeg);
                }
            } else if ctrl.starts_with(b"MIC") && ctrl.len() > 7 {
                // "MIC"(3) + seq(4) + opus_data
                let opus_data = &ctrl[7..];
                let mut mic = shared.mic_writer.lock().unwrap();
                if let Some(ref mut mw) = *mic {
                    if let Err(e) = mw.write_opus_packet(opus_data) {
                        eprintln!("[mic] USB write error: {e}");
                    }
                }
            }
        }
    }
}
