mod capture;
mod discovery;
mod doctor;
mod encode;
mod stream_server;

use std::env;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone)]
struct AppConfig {
    capture_backend: capture::CaptureBackend,
    capture_source: capture::CaptureSource,
    encoder_backend: encode::EncoderBackend,
    width: u32,
    height: u32,
    fps: u32,
    gop: u32,
    bitrate_bps: u32,
    stream_port: u16,
}

impl AppConfig {
    fn from_env() -> Result<Self> {
        let capture_source =
            capture::CaptureSource::from_env(&read_env("SCREX_CAPTURE_SOURCE", "virtual"));
        let width = read_env("SCREX_WIDTH", "1920")
            .parse::<u32>()
            .context("invalid SCREX_WIDTH")?;
        let height = read_env("SCREX_HEIGHT", "1080")
            .parse::<u32>()
            .context("invalid SCREX_HEIGHT")?;
        let fps = read_env("SCREX_FPS", "60")
            .parse::<u32>()
            .context("invalid SCREX_FPS")?;
        let gop = read_env("SCREX_GOP", "60")
            .parse::<u32>()
            .context("invalid SCREX_GOP")?;
        let bitrate_bps = read_env("SCREX_BITRATE_BPS", "10000000")
            .parse::<u32>()
            .context("invalid SCREX_BITRATE_BPS")?;
        let stream_port = read_env("SCREX_STREAM_PORT", "9000")
            .parse::<u16>()
            .context("invalid SCREX_STREAM_PORT")?;

        Ok(Self {
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
            gop: gop.max(10),
            bitrate_bps,
            stream_port,
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
    let (frame_tx, frame_rx) = mpsc::channel(2);
    let (au_tx, au_rx) = mpsc::channel(2);
    let (control_tx, control_rx) = mpsc::channel(32);

    let mdns_handle = match discovery::start_sender_advertisement(
        "_screx._udp",
        "screx-daemon",
        config.stream_port,
    ) {
        Ok(handle) => {
            println!("[main] mDNS: advertising _screx._udp on port {}", config.stream_port);
            Some(handle)
        }
        Err(err) => {
            eprintln!("[main] mDNS advertisement failed (continuing): {err:#}");
            None
        }
    };

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

    let encode_stop_rx = stop_rx.clone();
    let encode_task = tokio::task::spawn_blocking(move || {
        encode::run_encoder_loop(
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
            encode_stop_rx,
        )
    });

    let stream_task = tokio::spawn(stream_server::run_stream_server(
        config.stream_port,
        au_rx,
        control_tx,
        stop_rx.clone(),
    ));

    tokio::signal::ctrl_c().await?;
    println!("\nshutdown requested (ctrl-c)");
    let _ = stop_tx.send(true);

    let _ = stream_task.await;
    let _ = encode_task.await;

    match capture_handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => eprintln!("capture stopped with error: {err:#}"),
        Err(_) => eprintln!("capture thread panicked"),
    }

    if let Some(handle) = mdns_handle {
        handle.shutdown();
    }

    println!("screx-daemon shutdown complete");
    Ok(())
}
