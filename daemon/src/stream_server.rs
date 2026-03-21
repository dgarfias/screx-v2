use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::audio::MicWriter;
use crate::camera::{CamReassembler, CamWriter};
use crate::encode::EncodedAccessUnit;
use crate::uinput::{VirtualKeyboard, VirtualTouchscreen};
use crate::usb::TcpFramedSender;

const CHUNK_PAYLOAD: usize = 1400;
const HEADER_LEN: usize = 18;
const REGISTER_MAGIC: &[u8] = b"SCREX";
const PLI_MAGIC: &[u8] = b"PLI";
const TOUCH_MAGIC: &[u8] = b"TOUCH";
const KEY_MAGIC: &[u8] = b"KEY";
const CAM_MAGIC: &[u8] = b"CAM";
const MIC_MAGIC: &[u8] = b"MIC";
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_MAGIC: &[u8] = b"SCREX_HB";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_FEC_SHARDS: usize = 127;
const PACING_THRESHOLD: usize = 20;
const PACING_DELAY: Duration = Duration::from_micros(10);

pub struct SharedState {
    pub client_addr: Mutex<Option<SocketAddr>>,
    pub force_idr: AtomicBool,
    pub usb_sender: Mutex<Option<TcpFramedSender>>,
    pub usb_active: AtomicBool,
    pub virtual_touch: Mutex<Option<VirtualTouchscreen>>,
    pub virtual_keyboard: Mutex<Option<VirtualKeyboard>>,
    pub cam_writer: Mutex<Option<CamWriter>>,
    pub mic_writer: Mutex<Option<MicWriter>>,
    pub start_time: Instant,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            client_addr: Mutex::new(None),
            force_idr: AtomicBool::new(false),
            usb_sender: Mutex::new(None),
            usb_active: AtomicBool::new(false),
            virtual_touch: Mutex::new(None),
            virtual_keyboard: Mutex::new(None),
            cam_writer: Mutex::new(None),
            mic_writer: Mutex::new(None),
            start_time: Instant::now(),
        }
    }
}

pub const FLAG_IDR: u8 = 0x01;
pub const FLAG_AUDIO: u8 = 0x02;

/// 18-byte packet header (14 original + 4 byte timestamp_ms)
fn build_header(
    frame_id: u32,
    chunk_idx: u16,
    total_data: u16,
    total_parity: u16,
    flags: u8,
    payload_len: u16,
    timestamp_ms: u32,
) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(&frame_id.to_be_bytes());
    h[4..6].copy_from_slice(&chunk_idx.to_be_bytes());
    h[6..8].copy_from_slice(&total_data.to_be_bytes());
    h[8..10].copy_from_slice(&total_parity.to_be_bytes());
    h[10] = flags;
    h[11] = 0;
    h[12..14].copy_from_slice(&payload_len.to_be_bytes());
    h[14..18].copy_from_slice(&timestamp_ms.to_be_bytes());
    h
}

// ---------------------------------------------------------------------------
// Client manager — runs on its own thread, handles SCREX/PLI/keepalive
// ---------------------------------------------------------------------------

pub fn run_client_manager(
    socket: UdpSocket,
    shared: Arc<SharedState>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();

    let mut last_keepalive = Instant::now();
    let mut last_heartbeat = Instant::now();
    let mut recv_buf = vec![0u8; 4096];
    let mut cam_reassembler = CamReassembler::new();

    println!("[client] listening for iPad registration...");

    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut recv_buf) {
            Ok((len, addr)) => {
                if len >= REGISTER_MAGIC.len() && &recv_buf[..REGISTER_MAGIC.len()] == REGISTER_MAGIC
                {
                    let mut client = shared.client_addr.lock().unwrap();
                    let is_new = client.map_or(true, |prev| prev != addr);
                    *client = Some(addr);
                    last_keepalive = Instant::now();

                    if is_new {
                        println!("[client] registered: {addr}");
                        shared.force_idr.store(true, Ordering::Relaxed);
                    }
                }

                if len >= PLI_MAGIC.len() && &recv_buf[..PLI_MAGIC.len()] == PLI_MAGIC {
                    shared.force_idr.store(true, Ordering::Relaxed);
                }

                if len > TOUCH_MAGIC.len() && &recv_buf[..TOUCH_MAGIC.len()] == TOUCH_MAGIC {
                    let touch_data = &recv_buf[TOUCH_MAGIC.len()..len];
                    let mut touch = shared.virtual_touch.lock().unwrap();
                    if let Some(ref mut ts) = *touch {
                        crate::uinput::handle_touch_packet(ts, touch_data);
                    }
                }

                if len > KEY_MAGIC.len() && &recv_buf[..KEY_MAGIC.len()] == KEY_MAGIC {
                    let key_data = &recv_buf[KEY_MAGIC.len()..len];
                    let mut kb = shared.virtual_keyboard.lock().unwrap();
                    if let Some(ref mut keyboard) = *kb {
                        crate::uinput::handle_key_packet(keyboard, key_data);
                    }
                }

                if len > CAM_MAGIC.len() && &recv_buf[..CAM_MAGIC.len()] == CAM_MAGIC {
                    let cam_data = &recv_buf[CAM_MAGIC.len()..len];
                    if let Some(jpeg) = cam_reassembler.feed(cam_data) {
                        let mut cam = shared.cam_writer.lock().unwrap();
                        if let Some(ref mut cw) = *cam {
                            cw.write_frame(&jpeg);
                        }
                    }
                }

                // MIC packets: "MIC" + seq(4 BE) + opus_data
                if len > MIC_MAGIC.len() + 4 && &recv_buf[..MIC_MAGIC.len()] == MIC_MAGIC {
                    let opus_data = &recv_buf[MIC_MAGIC.len() + 4..len];
                    let mut mic = shared.mic_writer.lock().unwrap();
                    if let Some(ref mut mw) = *mic {
                        if let Err(e) = mw.write_opus_packet(opus_data) {
                            eprintln!("[mic] write error: {e}");
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                eprintln!("[client] recv error: {e}");
            }
        }

        // Send heartbeat to registered client so it knows we're alive
        // (EVDI may produce 0 fps for long stretches on static screens)
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            if let Some(addr) = *shared.client_addr.lock().unwrap() {
                let _ = socket.send_to(HEARTBEAT_MAGIC, addr);
            }
            last_heartbeat = Instant::now();
        }

        // Keepalive timeout check
        if shared.client_addr.lock().unwrap().is_some()
            && last_keepalive.elapsed() > KEEPALIVE_TIMEOUT
        {
            println!("[client] keepalive timeout, dropping client");
            *shared.client_addr.lock().unwrap() = None;
        }
    }

    println!("[client] manager stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// UDP sender — called synchronously from the capture thread
// ---------------------------------------------------------------------------

pub struct UdpSender {
    socket: UdpSocket,
    send_buf: Vec<u8>,
    frame_id: u32,
    stats_start: Instant,
    stats_frames: u64,
    stats_bytes: u64,
    shard_pool: Vec<Vec<u8>>,
}

impl UdpSender {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            send_buf: vec![0u8; HEADER_LEN + CHUNK_PAYLOAD],
            frame_id: 0,
            stats_start: Instant::now(),
            stats_frames: 0,
            stats_bytes: 0,
            shard_pool: Vec::new(),
        }
    }

    pub fn send_frame(&mut self, au: &EncodedAccessUnit, client_addr: SocketAddr, timestamp_ms: u32) -> Result<()> {
        let payload = &au.annex_b;
        let is_idr = au.is_idr;

        let data_count = (payload.len() + CHUNK_PAYLOAD - 1) / CHUNK_PAYLOAD;

        let parity_count = if data_count <= 1 {
            0
        } else if data_count > 50 {
            (data_count * 3 / 10).max(1).min(MAX_FEC_SHARDS)
        } else {
            (data_count / 5).max(1).min(MAX_FEC_SHARDS)
        };

        let use_fec = parity_count > 0 && (data_count + parity_count) <= 255;
        let actual_parity = if use_fec { parity_count } else { 0 };
        let total_shards = data_count + actual_parity;

        // Grow shard pool if needed (buffers persist across frames)
        while self.shard_pool.len() < total_shards {
            self.shard_pool.push(vec![0u8; CHUNK_PAYLOAD]);
        }

        // Fill data shards (zero-pad for RS alignment)
        for i in 0..data_count {
            let shard = &mut self.shard_pool[i];
            let start = i * CHUNK_PAYLOAD;
            let end = (start + CHUNK_PAYLOAD).min(payload.len());
            let actual = end - start;
            shard[..actual].copy_from_slice(&payload[start..end]);
            if actual < CHUNK_PAYLOAD {
                shard[actual..].fill(0);
            }
        }

        // Generate parity shards
        if actual_parity > 0 {
            for i in data_count..total_shards {
                self.shard_pool[i].fill(0);
            }
            let rs = ReedSolomon::new(data_count, actual_parity)
                .map_err(|e| anyhow::anyhow!("RS init: {e:?}"))?;
            rs.encode(&mut self.shard_pool[..total_shards])
                .map_err(|e| anyhow::anyhow!("RS encode: {e:?}"))?;
        }

        let needs_pacing = total_shards > PACING_THRESHOLD;
        let mut frame_bytes = 0_u64;
        let flags = if is_idr { FLAG_IDR } else { 0 };

        for idx in 0..total_shards {
            let actual_payload_len = if idx < data_count {
                let start = idx * CHUNK_PAYLOAD;
                let end = (start + CHUNK_PAYLOAD).min(payload.len());
                (end - start) as u16
            } else {
                CHUNK_PAYLOAD as u16
            };

            let header = build_header(
                self.frame_id,
                idx as u16,
                data_count as u16,
                actual_parity as u16,
                flags,
                actual_payload_len,
                timestamp_ms,
            );

            let pkt_len = HEADER_LEN + CHUNK_PAYLOAD;
            self.send_buf[..HEADER_LEN].copy_from_slice(&header);
            self.send_buf[HEADER_LEN..pkt_len]
                .copy_from_slice(&self.shard_pool[idx]);

            if let Err(e) = self.socket.send_to(&self.send_buf[..pkt_len], client_addr) {
                eprintln!("[stream] send error: {e}");
                break;
            }
            frame_bytes += pkt_len as u64;

            if needs_pacing {
                std::thread::sleep(PACING_DELAY);
            }
        }

        self.frame_id = self.frame_id.wrapping_add(1);
        self.stats_frames += 1;
        self.stats_bytes += frame_bytes;

        if self.stats_start.elapsed() >= Duration::from_secs(1) {
            let elapsed = self.stats_start.elapsed().as_secs_f64();
            let fps = self.stats_frames as f64 / elapsed;
            let mbps = (self.stats_bytes as f64 * 8.0 / elapsed) / 1_000_000.0;
            println!(
                "[stream] fps={fps:.1} throughput={mbps:.2}Mbps chunks={data_count}+{actual_parity}fec",
            );
            self.stats_frames = 0;
            self.stats_bytes = 0;
            self.stats_start = Instant::now();
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Audio sender — separate frame_id space, no FEC (audio chunks are small)
// ---------------------------------------------------------------------------

pub struct AudioSender {
    socket: UdpSocket,
    frame_id: u32,
    send_buf: Vec<u8>,
}

impl AudioSender {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            frame_id: 0,
            send_buf: vec![0u8; HEADER_LEN + CHUNK_PAYLOAD],
        }
    }

    pub fn send_audio(&mut self, pcm: &[u8], client_addr: SocketAddr, timestamp_ms: u32) -> Result<()> {
        let data_count = (pcm.len() + CHUNK_PAYLOAD - 1) / CHUNK_PAYLOAD;

        for i in 0..data_count {
            let start = i * CHUNK_PAYLOAD;
            let end = (start + CHUNK_PAYLOAD).min(pcm.len());
            let chunk = &pcm[start..end];
            let payload_len = (end - start) as u16;

            let header = build_header(
                self.frame_id,
                i as u16,
                data_count as u16,
                0,
                FLAG_AUDIO,
                payload_len,
                timestamp_ms,
            );

            let pkt_len = HEADER_LEN + chunk.len();
            self.send_buf[..HEADER_LEN].copy_from_slice(&header);
            self.send_buf[HEADER_LEN..pkt_len].copy_from_slice(chunk);

            if let Err(e) = self.socket.send_to(&self.send_buf[..pkt_len], client_addr) {
                eprintln!("[audio] send error: {e}");
                break;
            }
        }

        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }
}
