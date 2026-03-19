use std::time::Duration;

use anyhow::Result;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use crate::encode::{ControlMessage, EncodedAccessUnit};

pub async fn run_stream_server(
    port: u16,
    mut au_rx: mpsc::Receiver<EncodedAccessUnit>,
    control_tx: mpsc::Sender<ControlMessage>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    println!("[stream] TCP server listening on port {port}");
    println!("[stream] waiting for iPad to connect...");

    loop {
        // While waiting for a client, keep draining frames so the encoder never blocks
        let stream = loop {
            tokio::select! {
                biased;
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        println!("[stream] server shutting down");
                        return Ok(());
                    }
                }
                _ = au_rx.recv() => {
                    // Discard frames while no client is connected
                    continue;
                }
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, addr)) => {
                            println!("[stream] client connected from {addr}");
                            break stream;
                        }
                        Err(e) => {
                            eprintln!("[stream] accept error: {e}");
                            continue;
                        }
                    }
                }
            }
        };

        // Drain stale frames so the first frame sent is fresh
        let mut drained = 0;
        while au_rx.try_recv().is_ok() {
            drained += 1;
        }
        if drained > 0 {
            println!("[stream] drained {drained} stale frames");
        }

        // Request a fresh IDR so the client gets SPS/PPS immediately
        if let Err(e) = control_tx.send(ControlMessage::RequestIdr).await {
            eprintln!("[stream] failed to request IDR: {e}");
        } else {
            println!("[stream] requested IDR for new client");
        }

        stream.set_nodelay(true)?;
        let (reader, mut writer) = stream.into_split();
        drop(reader);

        let mut frames = 0_u64;
        let mut bytes = 0_u64;
        let mut window_start = tokio::time::Instant::now();

        loop {
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
            let len = payload.len() as u32;
            let mut header = [0u8; 4];
            header.copy_from_slice(&len.to_be_bytes());

            if let Err(e) = writer.write_all(&header).await {
                eprintln!("[stream] write header error (client disconnected?): {e}");
                break;
            }
            if let Err(e) = writer.write_all(payload).await {
                eprintln!("[stream] write payload error: {e}");
                break;
            }

            frames += 1;
            bytes += 4 + payload.len() as u64;

            if window_start.elapsed() >= Duration::from_secs(1) {
                let elapsed = window_start.elapsed().as_secs_f64();
                let fps = frames as f64 / elapsed;
                let mbps = (bytes as f64 * 8.0 / elapsed) / 1_000_000.0;
                println!("[stream] fps={fps:.1} throughput={mbps:.2} Mbps");
                frames = 0;
                bytes = 0;
                window_start = tokio::time::Instant::now();
            }
        }

        println!("[stream] client disconnected, waiting for next connection...");
    }
}
