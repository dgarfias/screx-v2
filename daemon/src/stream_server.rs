use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use reed_solomon_erasure::galois_8::ReedSolomon;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

use crate::encode::{ControlMessage, EncodedAccessUnit};

const CHUNK_PAYLOAD: usize = 1400;
const HEADER_LEN: usize = 10;
const REGISTER_MAGIC: &[u8] = b"SCREX";
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);

/// 10-byte packet header:
///   frame_id:     u32 BE  (bytes 0..4)
///   chunk_idx:    u8      (byte 4)
///   total_data:   u8      (byte 5)
///   total_parity: u8      (byte 6)
///   flags:        u8      (byte 7)   bit 0 = is_idr
///   payload_len:  u16 BE  (bytes 8..10)
fn build_header(
    frame_id: u32,
    chunk_idx: u8,
    total_data: u8,
    total_parity: u8,
    is_idr: bool,
    payload_len: u16,
) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(&frame_id.to_be_bytes());
    h[4] = chunk_idx;
    h[5] = total_data;
    h[6] = total_parity;
    h[7] = if is_idr { 1 } else { 0 };
    h[8..10].copy_from_slice(&payload_len.to_be_bytes());
    h
}

pub async fn run_stream_server(
    port: u16,
    mut au_rx: mpsc::Receiver<EncodedAccessUnit>,
    control_tx: mpsc::Sender<ControlMessage>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", port)).await?;
    println!("[stream] UDP server listening on port {port}");
    println!("[stream] waiting for iPad to register...");

    let mut send_buf = vec![0u8; HEADER_LEN + CHUNK_PAYLOAD];
    let mut frame_id: u32 = 0;

    loop {
        // Outer loop: wait for a client registration while draining frames
        let client_addr = loop {
            let mut reg_buf = [0u8; 64];
            tokio::select! {
                biased;
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        println!("[stream] server shutting down");
                        return Ok(());
                    }
                }
                _ = au_rx.recv() => {
                    continue;
                }
                result = socket.recv_from(&mut reg_buf) => {
                    match result {
                        Ok((len, addr)) => {
                            if len >= REGISTER_MAGIC.len() && &reg_buf[..REGISTER_MAGIC.len()] == REGISTER_MAGIC {
                                println!("[stream] client registered from {addr}");
                                break addr;
                            }
                        }
                        Err(e) => {
                            eprintln!("[stream] recv error: {e}");
                        }
                    }
                }
            }
        };

        // Drain stale frames
        let mut drained = 0;
        while au_rx.try_recv().is_ok() {
            drained += 1;
        }
        if drained > 0 {
            println!("[stream] drained {drained} stale frames");
        }

        // Request IDR for new client
        if let Err(e) = control_tx.send(ControlMessage::RequestIdr).await {
            eprintln!("[stream] failed to request IDR: {e}");
        } else {
            println!("[stream] requested IDR for new client");
        }

        let mut frames_sent = 0_u64;
        let mut bytes_sent = 0_u64;
        let mut window_start = tokio::time::Instant::now();
        let mut last_keepalive = tokio::time::Instant::now();

        // Inner loop: send frames to the registered client
        loop {
            // Check for keepalive / re-registration between frame sends
            let mut check_buf = [0u8; 64];
            while let Ok(result) = socket.try_recv_from(&mut check_buf) {
                let (len, addr) = result;
                if len >= REGISTER_MAGIC.len() && &check_buf[..REGISTER_MAGIC.len()] == REGISTER_MAGIC {
                    if addr == client_addr {
                        last_keepalive = tokio::time::Instant::now();
                    }
                }
            }

            if last_keepalive.elapsed() > KEEPALIVE_TIMEOUT {
                println!("[stream] client keepalive timeout, waiting for new client...");
                break;
            }

            let au = tokio::select! {
                biased;
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() { break; }
                    continue;
                }
                maybe_au = au_rx.recv() => {
                    match maybe_au {
                        Some(au) => au,
                        None => {
                            println!("[stream] encoder channel closed");
                            break;
                        }
                    }
                }
            };

            let payload = &au.annex_b;
            let is_idr = au.is_idr;

            // Chunk the access unit
            let data_count = (payload.len() + CHUNK_PAYLOAD - 1) / CHUNK_PAYLOAD;
            if data_count > 200 {
                eprintln!("[stream] frame too large ({} bytes, {data_count} chunks), skipping", payload.len());
                continue;
            }
            let data_count_u8 = data_count as u8;

            // FEC: ~20% overhead, minimum 1 parity shard, skip if only 1 chunk
            let parity_count = if data_count <= 1 { 0 } else { (data_count / 5).max(1).min(50) };
            let parity_count_u8 = parity_count as u8;

            let total_shards = data_count + parity_count;

            // Build data shards (all padded to CHUNK_PAYLOAD for RS)
            let mut shards: Vec<Vec<u8>> = Vec::with_capacity(total_shards);
            for i in 0..data_count {
                let start = i * CHUNK_PAYLOAD;
                let end = (start + CHUNK_PAYLOAD).min(payload.len());
                let mut shard = vec![0u8; CHUNK_PAYLOAD];
                shard[..(end - start)].copy_from_slice(&payload[start..end]);
                shards.push(shard);
            }

            // Generate parity shards
            if parity_count > 0 {
                for _ in 0..parity_count {
                    shards.push(vec![0u8; CHUNK_PAYLOAD]);
                }
                let rs = ReedSolomon::new(data_count, parity_count)
                    .map_err(|e| anyhow::anyhow!("RS init failed: {e:?}"))?;
                rs.encode(&mut shards)
                    .map_err(|e| anyhow::anyhow!("RS encode failed: {e:?}"))?;
            }

            // Send all shards (data + parity)
            let mut frame_bytes = 0_u64;
            for (idx, shard) in shards.iter().enumerate() {
                let actual_payload_len = if idx < data_count {
                    let start = idx * CHUNK_PAYLOAD;
                    let end = (start + CHUNK_PAYLOAD).min(payload.len());
                    (end - start) as u16
                } else {
                    CHUNK_PAYLOAD as u16
                };

                let header = build_header(
                    frame_id,
                    idx as u8,
                    data_count_u8,
                    parity_count_u8,
                    is_idr,
                    actual_payload_len,
                );

                let pkt_len = HEADER_LEN + shard.len();
                send_buf[..HEADER_LEN].copy_from_slice(&header);
                send_buf[HEADER_LEN..HEADER_LEN + shard.len()].copy_from_slice(shard);

                if let Err(e) = socket.send_to(&send_buf[..pkt_len], client_addr).await {
                    eprintln!("[stream] send error: {e}");
                    break;
                }
                frame_bytes += pkt_len as u64;
            }

            frame_id = frame_id.wrapping_add(1);
            frames_sent += 1;
            bytes_sent += frame_bytes;

            if window_start.elapsed() >= Duration::from_secs(1) {
                let elapsed = window_start.elapsed().as_secs_f64();
                let fps = frames_sent as f64 / elapsed;
                let mbps = (bytes_sent as f64 * 8.0 / elapsed) / 1_000_000.0;
                println!("[stream] fps={fps:.1} throughput={mbps:.2} Mbps fec={parity_count_u8}/{data_count_u8}");
                frames_sent = 0;
                bytes_sent = 0;
                window_start = tokio::time::Instant::now();
            }
        }

        println!("[stream] client disconnected, waiting for next registration...");
    }
}
