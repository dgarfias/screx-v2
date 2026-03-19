use std::time::Duration;

use anyhow::Result;
use reed_solomon_erasure::galois_8::ReedSolomon;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

use crate::encode::{ControlMessage, EncodedAccessUnit};

const CHUNK_PAYLOAD: usize = 1400;
const HEADER_LEN: usize = 14;
const REGISTER_MAGIC: &[u8] = b"SCREX";
const PLI_MAGIC: &[u8] = b"PLI";
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FEC_SHARDS: usize = 127;
const PACING_THRESHOLD: usize = 10;
const PACING_DELAY: Duration = Duration::from_micros(50);

/// 14-byte packet header:
///   frame_id:     u32 BE  (bytes 0..4)
///   chunk_idx:    u16 BE  (bytes 4..6)
///   total_data:   u16 BE  (bytes 6..8)
///   total_parity: u16 BE  (bytes 8..10)
///   flags:        u8      (byte 10)   bit 0 = is_idr
///   reserved:     u8      (byte 11)
///   payload_len:  u16 BE  (bytes 12..14)
fn build_header(
    frame_id: u32,
    chunk_idx: u16,
    total_data: u16,
    total_parity: u16,
    is_idr: bool,
    payload_len: u16,
) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(&frame_id.to_be_bytes());
    h[4..6].copy_from_slice(&chunk_idx.to_be_bytes());
    h[6..8].copy_from_slice(&total_data.to_be_bytes());
    h[8..10].copy_from_slice(&total_parity.to_be_bytes());
    h[10] = if is_idr { 1 } else { 0 };
    h[11] = 0;
    h[12..14].copy_from_slice(&payload_len.to_be_bytes());
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
            let mut check_buf = [0u8; 64];
            while let Ok(result) = socket.try_recv_from(&mut check_buf) {
                let (len, addr) = result;
                if addr != client_addr {
                    continue;
                }
                if len >= REGISTER_MAGIC.len()
                    && &check_buf[..REGISTER_MAGIC.len()] == REGISTER_MAGIC
                {
                    last_keepalive = tokio::time::Instant::now();
                }
                if len >= PLI_MAGIC.len() && &check_buf[..PLI_MAGIC.len()] == PLI_MAGIC {
                    let _ = control_tx.try_send(ControlMessage::RequestIdr);
                    println!("[stream] PLI received from client, requesting IDR");
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

            let data_count = (payload.len() + CHUNK_PAYLOAD - 1) / CHUNK_PAYLOAD;

            // FEC: ~20% overhead, min 1 parity for multi-chunk, capped for RS performance
            let parity_count = if data_count <= 1 {
                0
            } else {
                (data_count / 5).max(1).min(MAX_FEC_SHARDS)
            };

            // RS requires data+parity <= 256 for GF(2^8). If frame is very large,
            // skip FEC rather than failing.
            let use_fec = parity_count > 0 && (data_count + parity_count) <= 255;
            let actual_parity = if use_fec { parity_count } else { 0 };

            let total_shards = data_count + actual_parity;

            // Build data shards (padded to CHUNK_PAYLOAD for RS)
            let mut shards: Vec<Vec<u8>> = Vec::with_capacity(total_shards);
            for i in 0..data_count {
                let start = i * CHUNK_PAYLOAD;
                let end = (start + CHUNK_PAYLOAD).min(payload.len());
                let mut shard = vec![0u8; CHUNK_PAYLOAD];
                shard[..(end - start)].copy_from_slice(&payload[start..end]);
                shards.push(shard);
            }

            // Generate parity shards
            if actual_parity > 0 {
                for _ in 0..actual_parity {
                    shards.push(vec![0u8; CHUNK_PAYLOAD]);
                }
                let rs = ReedSolomon::new(data_count, actual_parity)
                    .map_err(|e| anyhow::anyhow!("RS init failed: {e:?}"))?;
                rs.encode(&mut shards)
                    .map_err(|e| anyhow::anyhow!("RS encode failed: {e:?}"))?;
            }

            // Send all shards with pacing for large frames
            let needs_pacing = shards.len() > PACING_THRESHOLD;
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
                    idx as u16,
                    data_count as u16,
                    actual_parity as u16,
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

                if needs_pacing {
                    tokio::time::sleep(PACING_DELAY).await;
                }
            }

            frame_id = frame_id.wrapping_add(1);
            frames_sent += 1;
            bytes_sent += frame_bytes;

            if window_start.elapsed() >= Duration::from_secs(1) {
                let elapsed = window_start.elapsed().as_secs_f64();
                let fps = frames_sent as f64 / elapsed;
                let mbps = (bytes_sent as f64 * 8.0 / elapsed) / 1_000_000.0;
                println!(
                    "[stream] fps={fps:.1} throughput={mbps:.2} Mbps chunks={data_count}+{actual_parity}fec frame_bytes={}",
                    payload.len()
                );
                frames_sent = 0;
                bytes_sent = 0;
                window_start = tokio::time::Instant::now();
            }
        }

        println!("[stream] client disconnected, waiting for next registration...");
    }
}
