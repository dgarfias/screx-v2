mod capture;
#[allow(dead_code)]
mod discovery;
mod doctor;
mod encode;
mod signaling;
#[allow(dead_code)]
mod transport;
mod webrtc_sender;

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

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
    signaling_port: u16,
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
        let signaling_port = read_env("SCREX_SIGNAL_PORT", "8080")
            .parse::<u16>()
            .context("invalid SCREX_SIGNAL_PORT")?;

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
            signaling_port,
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
    let (_control_tx, control_rx) = mpsc::channel(32);

    let sender = Arc::new(webrtc_sender::WebRtcSender::new().await?);

    let signal_addr: SocketAddr = ([0, 0, 0, 0], config.signaling_port).into();
    let router = signaling::build_router(sender.clone());
    let signal_listener = tokio::net::TcpListener::bind(signal_addr).await?;
    println!("[main] signaling server listening on http://0.0.0.0:{}", config.signaling_port);
    println!("[main] open http://<this-machine-ip>:{} on iPad Safari to receive", config.signaling_port);

    let signal_stop = stop_rx.clone();
    let signal_task = tokio::spawn(async move {
        axum::serve(signal_listener, router)
            .with_graceful_shutdown(async move {
                let mut rx = signal_stop;
                loop {
                    if *rx.borrow() {
                        break;
                    }
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
            .map_err(|e| anyhow::anyhow!("signaling server error: {e}"))
    });

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

    let webrtc_task = tokio::spawn(webrtc_sender::run_webrtc_feed_loop(
        sender.clone(),
        au_rx,
        stop_rx.clone(),
    ));

    tokio::signal::ctrl_c().await?;
    println!("shutdown requested (ctrl-c), draining tasks...");
    let _ = stop_tx.send(true);

    let signal_result = signal_task.await?;
    if let Err(err) = signal_result {
        eprintln!("signaling server stopped with error: {err:#}");
    }

    let webrtc_result = webrtc_task.await?;
    if let Err(err) = webrtc_result {
        eprintln!("webrtc sender stopped with error: {err:#}");
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

    println!("screx-daemon shutdown complete");
    Ok(())
}
