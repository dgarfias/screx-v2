use std::net::SocketAddr;
use std::str;
use std::{io, io::ErrorKind};

use anyhow::{Context, Result};
use rand::Rng;
use serde::Deserialize;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::time::{Duration, Instant};

use crate::encode::{ControlMessage, EncodedAccessUnit};

#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub target: SocketAddr,
    pub mtu: usize,
    pub payload_type: u8,
}

#[derive(Debug, Clone)]
struct PayloadPacket {
    marker: bool,
    payload: Vec<u8>,
}

pub async fn run_transport_loop(
    config: TransportConfig,
    mut au_rx: mpsc::Receiver<EncodedAccessUnit>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("failed to bind UDP sender socket")?;
    socket
        .connect(config.target)
        .await
        .context("failed to connect UDP sender socket")?;
    println!("[transport] sending RTP/HEVC to {}", config.target);

    let mut seq: u16 = rand::thread_rng().gen();
    let ssrc: u32 = rand::thread_rng().gen();
    let mut packets_in_window = 0_u64;
    let mut send_errors_in_window = 0_u64;
    let mut window_start = Instant::now();

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    println!("[transport] stop signal received");
                    break;
                }
            }
            maybe_au = au_rx.recv() => {
                let Some(au) = maybe_au else {
                    println!("[transport] upstream channel closed");
                    break;
                };
                let payloads = packetize_hevc_annex_b(&au.annex_b, config.mtu);
                let packet_count = payloads.len();
                for payload in payloads {
                    let packet = build_rtp_packet(
                        seq,
                        au.timestamp_90k,
                        payload.marker,
                        config.payload_type,
                        ssrc,
                        &payload.payload,
                    );
                    seq = seq.wrapping_add(1);
                    if let Err(err) = socket.send(&packet).await {
                        if err.kind() == ErrorKind::ConnectionRefused {
                            // iOS may not be listening yet; keep pipeline alive and retry on next packet.
                            send_errors_in_window += 1;
                            continue;
                        }
                        return Err(io::Error::new(err.kind(), err.to_string()))
                            .context("failed to send RTP packet");
                    }
                }
                packets_in_window += packet_count as u64;
                if window_start.elapsed() >= Duration::from_secs(1) {
                    println!("[transport] rtp_packets_per_sec={packets_in_window}");
                    if send_errors_in_window > 0 {
                        println!(
                            "[transport] send_errors_per_sec={send_errors_in_window} (peer unavailable or not listening)"
                        );
                    }
                    packets_in_window = 0;
                    send_errors_in_window = 0;
                    window_start = Instant::now();
                }
                if au.is_idr {
                    println!("[transport] sent IDR access unit frame_index={}", au.frame_index);
                }
            }
        }
    }

    Ok(())
}

pub async fn run_control_channel(
    bind_addr: SocketAddr,
    control_tx: mpsc::Sender<ControlMessage>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    let socket = UdpSocket::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind control socket on {bind_addr}"))?;
    println!("[control] listening on {bind_addr}");
    let mut buf = [0_u8; 2048];

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    println!("[control] stop signal received");
                    break;
                }
            }
            recv = socket.recv_from(&mut buf) => {
                let (size, from) = recv.context("control recv failed")?;
                if let Some(msg) = parse_control_message(&buf[..size]) {
                    println!("[control] {from} -> {msg:?}");
                    let _ = control_tx.send(msg).await;
                } else {
                    if let Ok(text) = str::from_utf8(&buf[..size]) {
                        eprintln!("[control] ignored unknown command from {from}: {}", text.trim());
                    }
                }
            }
        }
    }

    Ok(())
}

fn packetize_hevc_annex_b(annex_b: &[u8], mtu: usize) -> Vec<PayloadPacket> {
    let nalus = split_annex_b_nalus(annex_b);
    let mut out = Vec::new();
    let payload_budget = mtu.saturating_sub(12);
    if payload_budget <= 5 {
        return out;
    }

    for (nalu_index, nalu) in nalus.iter().enumerate() {
        let is_last_nalu = nalu_index + 1 == nalus.len();
        if nalu.len() <= payload_budget {
            out.push(PayloadPacket {
                marker: is_last_nalu,
                payload: nalu.to_vec(),
            });
            continue;
        }

        if nalu.len() <= 2 {
            continue;
        }

        let original_type = (nalu[0] >> 1) & 0x3F;
        let fu_indicator0 = (nalu[0] & 0x81) | (49_u8 << 1);
        let fu_indicator1 = nalu[1];
        let mut remaining = &nalu[2..];
        let frag_budget = payload_budget.saturating_sub(3);
        if frag_budget == 0 {
            continue;
        }

        let mut first = true;
        while !remaining.is_empty() {
            let take = remaining.len().min(frag_budget);
            let chunk = &remaining[..take];
            remaining = &remaining[take..];
            let end = remaining.is_empty();

            let fu_header =
                (if first { 0x80 } else { 0x00 }) | (if end { 0x40 } else { 0x00 }) | original_type;
            first = false;

            let mut payload = Vec::with_capacity(chunk.len() + 3);
            payload.push(fu_indicator0);
            payload.push(fu_indicator1);
            payload.push(fu_header);
            payload.extend_from_slice(chunk);

            out.push(PayloadPacket {
                marker: is_last_nalu && end,
                payload,
            });
        }
    }

    out
}

fn split_annex_b_nalus(data: &[u8]) -> Vec<&[u8]> {
    let starts = find_start_codes(data);
    let mut nalus = Vec::new();
    for idx in 0..starts.len() {
        let (start, prefix_len) = starts[idx];
        let payload_start = start + prefix_len;
        let payload_end = if idx + 1 < starts.len() {
            starts[idx + 1].0
        } else {
            data.len()
        };
        if payload_start < payload_end {
            nalus.push(&data[payload_start..payload_end]);
        }
    }
    nalus
}

fn find_start_codes(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0_usize;
    while i + 3 < data.len() {
        if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            starts.push((i, 4));
            i += 4;
            continue;
        }
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, 3));
            i += 3;
            continue;
        }
        i += 1;
    }
    starts
}

fn build_rtp_packet(
    sequence: u16,
    timestamp: u32,
    marker: bool,
    payload_type: u8,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + payload.len());
    out.push(0x80); // V=2, P=0, X=0, CC=0
    out.push((if marker { 0x80 } else { 0x00 }) | (payload_type & 0x7F));
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn parse_control_message(raw: &[u8]) -> Option<ControlMessage> {
    let text = str::from_utf8(raw).ok()?.trim();
    if text.eq_ignore_ascii_case("IDR") {
        return Some(ControlMessage::RequestIdr);
    }
    if let Some(value) = text.strip_prefix("BITRATE:") {
        let bps = value.trim().parse::<u32>().ok()?;
        return Some(ControlMessage::SetBitrate(bps));
    }
    if let Some(value) = text.strip_prefix("RES:") {
        let (w, h) = value.split_once('x')?;
        let width = w.trim().parse::<u32>().ok()?;
        let height = h.trim().parse::<u32>().ok()?;
        return Some(ControlMessage::SetResolution(width, height));
    }

    // JSON control payload option:
    // {"type":"idr"} or {"type":"bitrate","value":12000000}
    let cmd = serde_json::from_slice::<JsonControl>(raw).ok()?;
    match cmd.kind.as_str() {
        "idr" => Some(ControlMessage::RequestIdr),
        "bitrate" => cmd.value.map(ControlMessage::SetBitrate),
        "resolution" => cmd
            .resolution
            .map(|res| ControlMessage::SetResolution(res.width(), res.height())),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct JsonControl {
    #[serde(rename = "type")]
    kind: String,
    value: Option<u32>,
    resolution: Option<JsonResolutionValue>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsonResolutionValue {
    Tuple((u32, u32)),
    Object(JsonResolution),
}

impl JsonResolutionValue {
    fn width(&self) -> u32 {
        match self {
            Self::Tuple((w, _)) => *w,
            Self::Object(obj) => obj.width,
        }
    }

    fn height(&self) -> u32 {
        match self {
            Self::Tuple((_, h)) => *h,
            Self::Object(obj) => obj.height,
        }
    }
}

#[derive(Debug, Deserialize)]
struct JsonResolution {
    width: u32,
    height: u32,
}

#[cfg(test)]
mod tests {
    use super::{
        build_rtp_packet, packetize_hevc_annex_b, parse_control_message, split_annex_b_nalus,
        ControlMessage,
    };

    #[test]
    fn annex_b_split_works() {
        let data = vec![0, 0, 0, 1, 0x26, 0x01, 0xAA, 0, 0, 1, 0x02, 0x01, 0xBB];
        let nalus = split_annex_b_nalus(&data);
        assert_eq!(nalus.len(), 2);
        assert_eq!(nalus[0], &[0x26, 0x01, 0xAA]);
        assert_eq!(nalus[1], &[0x02, 0x01, 0xBB]);
    }

    #[test]
    fn marker_on_last_packet_only() {
        let data = vec![0, 0, 0, 1, 0x02, 0x01, 0xAA, 0, 0, 0, 1, 0x02, 0x01, 0xBB];
        let packets = packetize_hevc_annex_b(&data, 1200);
        assert_eq!(packets.len(), 2);
        assert!(!packets[0].marker);
        assert!(packets[1].marker);
    }

    #[test]
    fn fu_fragmentation_splits_large_nalu() {
        let mut data = vec![0, 0, 0, 1, 0x26, 0x01];
        data.extend(std::iter::repeat(0xAA).take(3000));
        let packets = packetize_hevc_annex_b(&data, 200);
        assert!(packets.len() > 2);
        assert!(packets.last().expect("packet").marker);
    }

    #[test]
    fn rtp_packet_has_12_byte_header() {
        let pkt = build_rtp_packet(42, 1500, true, 96, 123, &[1, 2, 3]);
        assert_eq!(pkt.len(), 15);
        assert_eq!(pkt[0], 0x80);
        assert_eq!(pkt[1], 0xE0);
    }

    #[test]
    fn json_resolution_object_control_parses() {
        let msg = br#"{"type":"resolution","resolution":{"width":1280,"height":720}}"#;
        let parsed = parse_control_message(msg).expect("resolution control");
        match parsed {
            ControlMessage::SetResolution(w, h) => {
                assert_eq!(w, 1280);
                assert_eq!(h, 720);
            }
            _ => panic!("expected resolution control message"),
        }
    }
}
