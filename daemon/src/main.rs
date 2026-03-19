mod capture;
mod discovery;
mod doctor;
mod encode;
mod transport;

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone)]
struct AppConfig {
    stream_target: SocketAddr,
    control_bind: SocketAddr,
    discovery_name: String,
    discovery_service: String,
    capture_backend: capture::CaptureBackend,
    capture_source: capture::CaptureSource,
    encoder_backend: encode::EncoderBackend,
    width: u32,
    height: u32,
    fps: u32,
    gop: u32,
    bitrate_bps: u32,
    mtu: usize,
}

impl AppConfig {
    fn from_env() -> Result<Self> {
        let stream_ip = read_env("SCREX_TARGET_IP", "127.0.0.1")
            .parse::<IpAddr>()
            .context("invalid SCREX_TARGET_IP")?;
        let stream_port = read_env("SCREX_TARGET_PORT", "5004")
            .parse::<u16>()
            .context("invalid SCREX_TARGET_PORT")?;
        let control_port = match env::var("SCREX_CONTROL_PORT") {
            Ok(raw) => raw.parse::<u16>().context("invalid SCREX_CONTROL_PORT")?,
            Err(_) => stream_port,
        };
        let capture_source =
            capture::CaptureSource::from_env(&read_env("SCREX_CAPTURE_SOURCE", "virtual"));
        let mut width = read_env("SCREX_WIDTH", "1920")
            .parse::<u32>()
            .context("invalid SCREX_WIDTH")?;
        let mut height = read_env("SCREX_HEIGHT", "1080")
            .parse::<u32>()
            .context("invalid SCREX_HEIGHT")?;
        let fps = read_env("SCREX_FPS", "60")
            .parse::<u32>()
            .context("invalid SCREX_FPS")?;
        let gop = read_env("SCREX_GOP", "120")
            .parse::<u32>()
            .context("invalid SCREX_GOP")?;
        let bitrate_bps = read_env("SCREX_BITRATE_BPS", "10000000")
            .parse::<u32>()
            .context("invalid SCREX_BITRATE_BPS")?;
        let mtu = read_env("SCREX_MTU", "1200")
            .parse::<usize>()
            .context("invalid SCREX_MTU")?;

        if capture_source == capture::CaptureSource::Virtual1080p {
            width = 1920;
            height = 1080;
        }

        Ok(Self {
            stream_target: SocketAddr::new(stream_ip, stream_port),
            control_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), control_port),
            discovery_name: read_env("SCREX_DISCOVERY_NAME", "screx-daemon"),
            discovery_service: read_env("SCREX_DISCOVERY_SERVICE", "_screenstream._udp"),
            capture_backend: capture::CaptureBackend::from_env(&read_env(
                "SCREX_CAPTURE_BACKEND",
                "auto",
            )),
            capture_source,
            encoder_backend: encode::EncoderBackend::from_env(&read_env(
                "SCREX_ENCODER_BACKEND",
                "auto",
            )),
            width,
            height,
            fps,
            gop: gop.max(60),
            bitrate_bps,
            mtu,
        })
    }
}

fn read_env(key: &str, fallback: &str) -> String {
    env::var(key).unwrap_or_else(|_| fallback.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    if read_env("SCREX_COMMAND", "").eq_ignore_ascii_case("doctor")
        || std::env::args().any(|arg| arg == "doctor" || arg == "--doctor")
    {
        return doctor::run_doctor();
    }

    let config = AppConfig::from_env()?;
    println!("screx-daemon boot with config: {config:?}");

    let (stop_tx, stop_rx) = watch::channel(false);
    let (frame_tx, frame_rx) = mpsc::channel(8);
    let (au_tx, au_rx) = mpsc::channel(16);
    let (control_tx, control_rx) = mpsc::channel(32);

    let mut discovery_handle = discovery::start_avahi_advertisement(
        &config.discovery_name,
        &config.discovery_service,
        config.stream_target.port(),
        config.control_bind.port(),
    )
    .await?;

    let capture_handle = capture::spawn_capture_thread(
        capture::CaptureConfig {
            width: config.width,
            height: config.height,
            fps: config.fps,
            prefer_dma_buf: true,
            backend: config.capture_backend,
            source: config.capture_source,
        },
        frame_tx,
        stop_rx.clone(),
    );

    let encode_task = tokio::spawn(encode::run_encoder_loop(
        encode::EncoderConfig {
            bitrate_bps: config.bitrate_bps,
            gop: config.gop,
            fps: config.fps,
            width: config.width,
            height: config.height,
            backend: config.encoder_backend,
        },
        frame_rx,
        au_tx,
        control_rx,
        stop_rx.clone(),
    ));

    let transport_task = tokio::spawn(transport::run_transport_loop(
        transport::TransportConfig {
            target: config.stream_target,
            mtu: config.mtu,
            payload_type: 96,
        },
        au_rx,
        stop_rx.clone(),
    ));

    let control_task = tokio::spawn(transport::run_control_channel(
        config.control_bind,
        control_tx,
        stop_rx.clone(),
    ));

    tokio::signal::ctrl_c().await?;
    println!("shutdown requested (ctrl-c), draining tasks...");
    let _ = stop_tx.send(true);

    let control_result = control_task.await?;
    if let Err(err) = control_result {
        eprintln!("control channel stopped with error: {err:#}");
    }

    let transport_result = transport_task.await?;
    if let Err(err) = transport_result {
        eprintln!("transport stopped with error: {err:#}");
    }

    let encode_result = encode_task.await?;
    if let Err(err) = encode_result {
        eprintln!("encoder stopped with error: {err:#}");
    }

    match capture_handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => eprintln!("capture thread stopped with error: {err:#}"),
        Err(_) => eprintln!("capture thread panicked"),
    }

    if let Some(handle) = discovery_handle.as_mut() {
        handle.stop().await;
    }

    println!("screx-daemon shutdown complete");
    Ok(())
}
