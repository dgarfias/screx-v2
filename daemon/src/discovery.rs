use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

const BEACON_PORT: u16 = 9999;
const BEACON_INTERVAL: Duration = Duration::from_secs(2);
const BEACON_MAGIC: &[u8] = b"SCREX_BEACON";

/// Spawns a thread that broadcasts a UDP beacon every 2 seconds.
/// The beacon packet is: "SCREX_BEACON" (12 bytes) + stream_port (u16 BE).
/// iPad listens on port 9999 and extracts the daemon's IP from the source address.
///
/// When `pause` is true, beacon broadcasting is suppressed (e.g. during an active session).
pub fn start_beacon(
    stream_port: u16,
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    let sock = UdpSocket::bind("0.0.0.0:0").context("failed to bind beacon socket")?;
    sock.set_broadcast(true).context("failed to set SO_BROADCAST")?;

    let mut packet = Vec::with_capacity(BEACON_MAGIC.len() + 2);
    packet.extend_from_slice(BEACON_MAGIC);
    packet.extend_from_slice(&stream_port.to_be_bytes());

    let handle = std::thread::Builder::new()
        .name("beacon".into())
        .spawn(move || {
            let dest = format!("255.255.255.255:{BEACON_PORT}");
            println!("[beacon] broadcasting on port {BEACON_PORT} every {BEACON_INTERVAL:?} (stream_port={stream_port})");

            while !stop.load(Ordering::Relaxed) {
                if !pause.load(Ordering::Relaxed) {
                    if let Err(e) = sock.send_to(&packet, &dest) {
                        eprintln!("[beacon] send error: {e}");
                    }
                }
                std::thread::sleep(BEACON_INTERVAL);
            }

            println!("[beacon] stopped");
        })
        .context("failed to spawn beacon thread")?;

    Ok(handle)
}
