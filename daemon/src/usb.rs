use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::stream_server::SharedState;

const IPROXY_LOCAL_PORT: u16 = 9001;
const DEVICE_PORT: u16 = 9000;
const DETECT_INTERVAL: Duration = Duration::from_secs(2);
const CONNECT_RETRY: Duration = Duration::from_secs(1);
const READY_TIMEOUT: Duration = Duration::from_secs(3);

const MSG_VIDEO: u8 = 0x01;
const MSG_AUDIO: u8 = 0x02;
const MSG_CONTROL: u8 = 0x03;
const READY_MAGIC: &[u8] = b"READY";
const HOSTNAME_MAGIC: &[u8] = b"HOST";

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

    pub fn send_video(
        &mut self,
        annex_b: &[u8],
        is_idr: bool,
        timestamp_ms: u32,
        codec_id: u8,
    ) -> Result<()> {
        let payload_len = 1 + 1 + 1 + 4 + annex_b.len(); // type + is_idr + codec_id + ts + data
        self.write_buf.clear();
        self.write_buf
            .extend_from_slice(&(payload_len as u32).to_be_bytes());
        self.write_buf.push(MSG_VIDEO);
        self.write_buf.push(if is_idr { 1 } else { 0 });
        self.write_buf.push(codec_id);
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

    pub fn send_control(&mut self, payload: &[u8]) -> Result<()> {
        let payload_len = 1 + payload.len();
        self.write_buf.clear();
        self.write_buf
            .extend_from_slice(&(payload_len as u32).to_be_bytes());
        self.write_buf.push(MSG_CONTROL);
        self.write_buf.extend_from_slice(payload);
        self.stream
            .write_all(&self.write_buf)
            .context("USB TCP write (control)")
    }
}

/// Main USB transport loop. Runs on its own thread.
/// Detects device, starts iproxy, connects TCP, reads control messages.
pub fn run_usb_transport(shared: Arc<SharedState>, stop: Arc<AtomicBool>) {
    println!("[usb] transport thread started (polling for device every {DETECT_INTERVAL:?})");
    let mut device_present = false;

    while !stop.load(Ordering::Relaxed) {
        if !detect_device() {
            if device_present {
                println!("[usb] iOS device disconnected from USB");
                device_present = false;
            }
            std::thread::sleep(DETECT_INTERVAL);
            continue;
        }

        if !device_present {
            println!("[usb] iOS device detected via USB");
            device_present = true;
        }

        let mut iproxy_child = match start_iproxy() {
            Ok(child) => child,
            Err(e) => {
                eprintln!("[usb] failed to start iproxy: {e:#}");
                std::thread::sleep(DETECT_INTERVAL);
                continue;
            }
        };

        let addr = format!("127.0.0.1:{IPROXY_LOCAL_PORT}");
        println!("[usb] waiting for Screx app USB listener");
        while !stop.load(Ordering::Relaxed) && detect_device() {
            let stream = match connect_to_iproxy(&addr, &stop) {
                Some(stream) => stream,
                None => break,
            };

            let mut read_stream = match stream.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[usb] failed to clone TCP stream: {e}");
                    std::thread::sleep(CONNECT_RETRY);
                    continue;
                }
            };

            let sender = match TcpFramedSender::new(stream) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[usb] failed to create TCP sender: {e}");
                    std::thread::sleep(CONNECT_RETRY);
                    continue;
                }
            };

            if !wait_for_ready(&mut read_stream, &stop) {
                std::thread::sleep(CONNECT_RETRY);
                continue;
            }

            activate_usb_transport(&shared, sender);
            println!("[usb] transport ACTIVE — video/audio will prefer USB");

            read_control_loop(read_stream, &shared, &stop);

            deactivate_usb_transport(&shared);
            println!("[usb] transport deactivated");

            if !stop.load(Ordering::Relaxed) && detect_device() {
                std::thread::sleep(CONNECT_RETRY);
            }
        }

        deactivate_usb_transport(&shared);
        stop_iproxy(&mut iproxy_child);
    }

    println!("[usb] transport thread stopped");
}

fn connect_to_iproxy(addr: &str, stop: &Arc<AtomicBool>) -> Option<TcpStream> {
    while !stop.load(Ordering::Relaxed) && detect_device() {
        match TcpStream::connect(addr) {
            Ok(stream) => return Some(stream),
            Err(e) => {
                crate::vlog!("[usb] waiting for USB app listener at {addr}: {e}");
                std::thread::sleep(CONNECT_RETRY);
            }
        }
    }

    None
}

fn wait_for_ready(stream: &mut TcpStream, stop: &Arc<AtomicBool>) -> bool {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok();

    let started = Instant::now();
    let mut len_buf = [0u8; 4];
    let mut msg_buf = vec![0u8; 256];

    while !stop.load(Ordering::Relaxed) && detect_device() {
        if started.elapsed() > READY_TIMEOUT {
            println!("[usb] READY timeout, waiting for a fresh USB app connection");
            return false;
        }

        match stream.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                crate::vlog!("[usb] TCP disconnected before READY: {e}");
                return false;
            }
        }

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len == 0 || msg_len > 65536 {
            crate::vlog!("[usb] invalid READY frame length: {msg_len}");
            return false;
        }
        if msg_buf.len() < msg_len {
            msg_buf.resize(msg_len, 0);
        }

        match stream.read_exact(&mut msg_buf[..msg_len]) {
            Ok(()) => {}
            Err(e) => {
                crate::vlog!("[usb] TCP disconnected before READY payload: {e}");
                return false;
            }
        }

        if msg_buf[0] == MSG_CONTROL && msg_len >= 2 {
            let ctrl = &msg_buf[1..msg_len];
            if ctrl == READY_MAGIC {
                println!("[usb] app READY received");
                return true;
            }
        }
    }

    false
}

fn activate_usb_transport(shared: &Arc<SharedState>, mut sender: TcpFramedSender) {
    if let Some(hostname) = local_hostname() {
        let mut payload = Vec::with_capacity(HOSTNAME_MAGIC.len() + hostname.len());
        payload.extend_from_slice(HOSTNAME_MAGIC);
        payload.extend_from_slice(hostname.as_bytes());
        if let Err(e) = sender.send_control(&payload) {
            eprintln!("[usb] failed to send hostname: {e:#}");
        }
    }

    {
        let mut usb = shared.usb_sender.lock().unwrap();
        *usb = Some(sender);
    }
    shared.usb_active.store(true, Ordering::SeqCst);
    shared.force_idr.store(true, Ordering::Relaxed);
    shared.capture_start.store(true, Ordering::Release);
    if let Some(ref fr) = *shared.force_refresh_handle.lock().unwrap() {
        fr.store(true, Ordering::Relaxed);
    }
    if !shared.has_active_client.swap(true, Ordering::SeqCst) {
        if let Some(ref cb) = *shared.on_client_connected.lock().unwrap() {
            cb();
        }
    }
}

fn local_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return None;
    }

    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let hostname = String::from_utf8_lossy(&buf[..len]).trim().to_string();
    if hostname.is_empty() {
        None
    } else {
        Some(hostname)
    }
}

fn deactivate_usb_transport(shared: &Arc<SharedState>) {
    shared.usb_active.store(false, Ordering::SeqCst);
    {
        let mut usb = shared.usb_sender.lock().unwrap();
        *usb = None;
    }

    if shared.client_addr.lock().unwrap().is_none()
        && shared.has_active_client.swap(false, Ordering::SeqCst)
    {
        if let Some(ref cb) = *shared.on_client_disconnected.lock().unwrap() {
            cb();
        }
    }
}

fn read_control_loop(mut stream: TcpStream, shared: &Arc<SharedState>, stop: &Arc<AtomicBool>) {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok();

    let mut len_buf = [0u8; 4];
    let mut msg_buf = vec![0u8; 256];
    let mut cam_reassembler = crate::camera::CamReassembler::new();

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
            eprintln!("[usb] invalid USB control frame length: {msg_len}, closing transport");
            break;
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
            if ctrl == READY_MAGIC {
                crate::vlog!("[usb] ignoring duplicate READY on active transport");
            } else if ctrl.starts_with(b"CAM") && ctrl.len() > 3 {
                if let Some(jpeg) = cam_reassembler.feed(&ctrl[3..]) {
                    let mut cam = shared.cam_writer.lock().unwrap();
                    if let Some(ref mut cw) = *cam {
                        cw.write_frame(&jpeg);
                    }
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
            } else {
                crate::stream_server::handle_control_message_data(shared, ctrl);
            }
        }
    }
}
