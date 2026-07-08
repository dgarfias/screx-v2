use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::platform::shim::hostname;
use crate::stream_server::SharedState;

const DETECT_INTERVAL: Duration = Duration::from_secs(2);
const CONNECT_RETRY: Duration = Duration::from_secs(1);
const READY_TIMEOUT: Duration = Duration::from_secs(3);
/// Read poll interval for the USB read loop. Keeps `read_exact` from
/// blocking forever so the global `stop` flag (Ctrl-C shutdown) is honored
/// promptly and the mux socket is closed, letting the iPad detect loss of
/// the daemon end of the pipe.
const USB_READ_POLL: Duration = Duration::from_millis(250);

const MSG_VIDEO: u8 = 0x01;
const MSG_AUDIO: u8 = 0x02;
const MSG_CONTROL: u8 = 0x03;
const READY_MAGIC: &[u8] = b"READY";
const HOSTNAME_MAGIC: &[u8] = b"HOST";

/// A bidirectional byte stream to the iPad (TCP over USB mux).
pub trait ReadWriteStream: Read + Write + Send {
    fn try_clone_box(&self) -> Option<Box<dyn ReadWriteStream>>;
    /// Forwarded to the underlying socket so the read loop can poll the
    /// global `stop` flag instead of blocking forever (and so Ctrl-C on the
    /// daemon unblocks the USB read loop, closing the mux socket so the iPad
    /// detects the disconnect).
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()>;
}

impl ReadWriteStream for Box<dyn ReadWriteStream> {
    fn try_clone_box(&self) -> Option<Box<dyn ReadWriteStream>> {
        self.as_ref().try_clone_box()
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.as_ref().set_read_timeout(timeout)
    }
}

/// Platform-specific USB multiplexer client.
pub trait MuxClient {
    fn device_present(&mut self) -> bool;
    fn connect(&mut self, device_port: u16) -> Result<Box<dyn ReadWriteStream>>;
}

fn create_mux_client() -> Box<dyn MuxClient> {
    #[cfg(target_os = "linux")]
    {
        Box::new(crate::platform::linux::usbmux::LinuxMuxClient::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(crate::platform::windows::usbmux::WindowsMuxClient::new())
    }
}

/// Thread-safe TCP sender for framed messages over USB.
pub struct TcpFramedSender {
    stream: Box<dyn ReadWriteStream>,
    write_buf: Vec<u8>,
}

impl TcpFramedSender {
    fn new(stream: Box<dyn ReadWriteStream>) -> Result<Self> {
        Ok(Self {
            stream,
            write_buf: Vec::with_capacity(256 * 1024),
        })
    }

    fn write_all_vectored(&mut self, header: &[u8], payload: &[u8]) -> std::io::Result<()> {
        self.write_buf.clear();
        self.write_buf.extend_from_slice(header);
        self.write_buf.extend_from_slice(payload);
        self.stream.write_all(&self.write_buf)
    }

    pub fn send_video(
        &mut self,
        annex_b: &[u8],
        is_idr: bool,
        timestamp_ms: u32,
        codec_id: u8,
    ) -> Result<()> {
        let payload_len = 1 + 1 + 1 + 4 + annex_b.len();
        let mut header = [0u8; 4 + 1 + 1 + 1 + 4];
        header[..4].copy_from_slice(&(payload_len as u32).to_be_bytes());
        header[4] = MSG_VIDEO;
        header[5] = if is_idr { 1 } else { 0 };
        header[6] = codec_id;
        header[7..11].copy_from_slice(&timestamp_ms.to_be_bytes());
        self.write_all_vectored(&header, annex_b)
            .context("USB TCP write (video)")
    }

    pub fn send_audio(&mut self, pcm: &[u8], timestamp_ms: u32) -> Result<()> {
        let payload_len = 1 + 4 + pcm.len();
        let mut header = [0u8; 4 + 1 + 4];
        header[..4].copy_from_slice(&(payload_len as u32).to_be_bytes());
        header[4] = MSG_AUDIO;
        header[5..9].copy_from_slice(&timestamp_ms.to_be_bytes());
        self.write_all_vectored(&header, pcm)
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

pub fn run_usb_transport(shared: Arc<SharedState>, stop: Arc<AtomicBool>) {
    println!("[usb] transport thread started (polling for device every {DETECT_INTERVAL:?})");
    let mut mux = create_mux_client();
    let mut device_present = false;

    while !stop.load(Ordering::Relaxed) {
        // Stay dormant while a network (non-USB) client session is alive:
        // the iPad is reachable over Wi-Fi/Ethernet, so probing the USB mux
        // every second is wasted work and spams the log. USB polling resumes
        // automatically once the network session ends.
        if shared.has_active_client.load(Ordering::SeqCst)
            && !shared.usb_active.load(Ordering::SeqCst)
        {
            std::thread::sleep(DETECT_INTERVAL);
            continue;
        }

        if !mux.device_present() {
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

        let stream = match mux.connect(9000) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[usb] failed to connect USB mux: {e:#}");
                std::thread::sleep(CONNECT_RETRY);
                continue;
            }
        };

        let mut read_stream = match try_clone_stream(&*stream) {
            Some(s) => s,
            None => {
                eprintln!("[usb] failed to clone USB stream");
                std::thread::sleep(CONNECT_RETRY);
                continue;
            }
        };
        // Poll the socket instead of blocking forever so Ctrl-C shutdown is
        // honored promptly and the socket is dropped, letting the iPad see the
        // disconnect. The WouldBlock/TimedOut branches in the loops below keep
        // the read loops spinning until real data arrives or `stop` is set.
        let _ = read_stream.set_read_timeout(Some(USB_READ_POLL));

        let sender = match TcpFramedSender::new(stream) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[usb] failed to create TCP sender: {e}");
                std::thread::sleep(CONNECT_RETRY);
                continue;
            }
        };

        if !wait_for_ready(&mut read_stream, &stop, &mut *mux) {
            std::thread::sleep(CONNECT_RETRY);
            continue;
        }

        activate_usb_transport(&shared, sender);
        println!("[usb] transport ACTIVE — video/audio will prefer USB");

        read_control_loop(read_stream, &shared, &stop);

        deactivate_usb_transport(&shared);
        println!("[usb] transport deactivated");

        if !stop.load(Ordering::Relaxed) && mux.device_present() {
            std::thread::sleep(CONNECT_RETRY);
        }
    }

    println!("[usb] transport thread stopped");
}

fn try_clone_stream(stream: &dyn ReadWriteStream) -> Option<Box<dyn ReadWriteStream>> {
    stream.try_clone_box()
}

fn wait_for_ready(
    stream: &mut dyn ReadWriteStream,
    stop: &Arc<AtomicBool>,
    mux: &mut dyn MuxClient,
) -> bool {
    let started = Instant::now();
    let mut len_buf = [0u8; 4];
    let mut msg_buf = vec![0u8; 256];

    while !stop.load(Ordering::Relaxed) && mux.device_present() {
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
    if let Some(hostname) = hostname() {
        let mut payload = Vec::with_capacity(HOSTNAME_MAGIC.len() + hostname.len());
        payload.extend_from_slice(HOSTNAME_MAGIC);
        payload.extend_from_slice(hostname.as_bytes());
        if let Err(e) = sender.send_control(&payload) {
            eprintln!("[usb] failed to send hostname: {e:#}");
        }
    }

    let caps_payload = crate::stream_server::build_caps_message(shared);
    if let Err(e) = sender.send_control(&caps_payload) {
        eprintln!("[usb] failed to send capabilities: {e:#}");
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
    shared.audio_notify.notify_all();
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
    shared.audio_notify.notify_all();
}

fn read_control_loop(
    mut stream: Box<dyn ReadWriteStream>,
    shared: &Arc<SharedState>,
    stop: &Arc<AtomicBool>,
) {
    let mut len_buf = [0u8; 4];
    let mut msg_buf = vec![0u8; 256];
    let mut cam_reassembler = crate::camera::CamReassembler::new();
    let mut cam_chunks: u64 = 0;
    let mut cam_frames: u64 = 0;
    let mut cam_dropped_no_writer: u64 = 0;
    let mut cam_last_report = std::time::Instant::now();

    crate::vlog!("[usb] read loop started");
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
            } else if ctrl.starts_with(b"CAMCFG") || ctrl.starts_with(b"MICCFG") {
                // CAMCFG/MICCFG config magics share the "CAM"/"MIC" prefix with the
                // CAM/MIC data magics below, so they must be routed to the control
                // handler here before the data branches would otherwise swallow them.
                crate::stream_server::handle_control_message_data(shared, ctrl);
            } else if ctrl.starts_with(b"CAM") && ctrl.len() > 3 {
                cam_chunks += 1;
                if let Some(jpeg) = cam_reassembler.feed(&ctrl[3..]) {
                    cam_frames += 1;
                    let mut cam = shared.cam_writer.lock().unwrap();
                    if let Some(ref mut cw) = *cam {
                        cw.write_frame(&jpeg);
                    } else {
                        cam_dropped_no_writer += 1;
                    }
                }
                if cam_chunks > 0 && cam_last_report.elapsed() >= std::time::Duration::from_secs(1)
                {
                    crate::vlog!(
                        "[usb] cam: chunks={cam_chunks} frames={cam_frames} dropped_no_writer={cam_dropped_no_writer}"
                    );
                    cam_last_report = std::time::Instant::now();
                }
            } else if ctrl.starts_with(b"MIC") && ctrl.len() > 7 {
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
