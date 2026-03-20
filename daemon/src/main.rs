mod audio;
mod capture;
mod discovery;
mod doctor;
mod encode;
mod stream_server;

use std::env;
use std::net::UdpSocket;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
struct AppConfig {
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
        let width = read_env("SCREX_WIDTH", "2160")
            .parse::<u32>()
            .context("invalid SCREX_WIDTH")?;
        let height = read_env("SCREX_HEIGHT", "1620")
            .parse::<u32>()
            .context("invalid SCREX_HEIGHT")?;
        let fps = read_env("SCREX_FPS", "30")
            .parse::<u32>()
            .context("invalid SCREX_FPS")?;
        let gop = read_env("SCREX_GOP", "30")
            .parse::<u32>()
            .context("invalid SCREX_GOP")?;
        let bitrate_bps = read_env("SCREX_BITRATE_BPS", "8000000")
            .parse::<u32>()
            .context("invalid SCREX_BITRATE_BPS")?;
        let stream_port = read_env("SCREX_STREAM_PORT", "9000")
            .parse::<u16>()
            .context("invalid SCREX_STREAM_PORT")?;

        Ok(Self {
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
    println!("screx-daemon v2 (EVDI) config: {config:?}");

    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(stream_server::SharedState::new());

    // UDP socket for streaming
    let socket = UdpSocket::bind(("0.0.0.0", config.stream_port))
        .with_context(|| format!("failed to bind UDP port {}", config.stream_port))?;

    // Enlarge kernel send buffer to absorb IDR frame bursts
    unsafe {
        let sndbuf: libc::c_int = 2 * 1024 * 1024;
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &sndbuf as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }

    println!("[main] UDP socket bound on port {}", config.stream_port);

    // mDNS advertisement
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

    // Client manager thread (handles SCREX register + PLI packets)
    let client_socket = socket.try_clone().context("clone socket for client mgr")?;
    let client_shared = Arc::clone(&shared);
    let client_stop = Arc::clone(&stop);
    let client_thread = thread::Builder::new()
        .name("client-mgr".into())
        .spawn(move || {
            if let Err(e) =
                stream_server::run_client_manager(client_socket, client_shared, client_stop)
            {
                eprintln!("[client] manager error: {e:#}");
            }
        })
        .context("failed to spawn client manager thread")?;

    // Capture + encode + send thread (the synchronous hot path)
    let send_socket = socket.try_clone().context("clone socket for sender")?;
    let capture_shared = Arc::clone(&shared);
    let capture_stop = Arc::clone(&stop);
    let capture_config = capture::CaptureConfig {
        width: config.width,
        height: config.height,
        fps: config.fps,
    };

    let mut encoder = encode::Encoder::new(encode::EncoderConfig {
        bitrate_bps: config.bitrate_bps,
        gop: config.gop,
        fps: config.fps,
        width: config.width,
        height: config.height,
        backend: config.encoder_backend,
    })?;

    let mut sender = stream_server::UdpSender::new(send_socket);

    let capture_thread = thread::Builder::new()
        .name("capture".into())
        .spawn(move || -> Result<()> {
            capture::run_capture_loop(capture_config, Arc::clone(&capture_stop), |frame| {
                let force_idr = capture_shared.force_idr.swap(false, Ordering::Relaxed);

                match encoder.encode_frame(&frame, force_idr) {
                    Ok(aus) => {
                        let client_addr = *capture_shared.client_addr.lock().unwrap();
                        if let Some(addr) = client_addr {
                            for au in &aus {
                                if let Err(e) = sender.send_frame(au, addr) {
                                    eprintln!("[pipeline] send error: {e:#}");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[pipeline] encode error: {e:#}");
                    }
                }
            })
        })
        .context("failed to spawn capture thread")?;

    // Audio capture thread
    let audio_socket = socket.try_clone().context("clone socket for audio")?;
    let audio_shared = Arc::clone(&shared);
    let audio_stop = Arc::clone(&stop);
    let audio_module_id = match audio::create_virtual_sink() {
        Ok(id) => {
            println!("[main] audio: virtual sink ready (module {id})");
            id
        }
        Err(e) => {
            eprintln!("[main] audio: failed to create virtual sink ({e:#}), audio disabled");
            0
        }
    };

    let audio_thread = if audio_module_id > 0 {
        Some(
            thread::Builder::new()
                .name("audio".into())
                .spawn(move || {
                    if let Err(e) =
                        audio::run_audio_capture(audio_socket, audio_shared, audio_stop)
                    {
                        eprintln!("[audio] capture error: {e:#}");
                    }
                })
                .context("failed to spawn audio thread")?,
        )
    } else {
        None
    };

    // Wait for Ctrl-C
    tokio::signal::ctrl_c().await?;
    println!("\nshutdown requested (ctrl-c)");
    stop.store(true, Ordering::SeqCst);

    if let Err(e) = capture_thread.join() {
        eprintln!("[main] capture thread panicked: {e:?}");
    }
    if let Some(t) = audio_thread {
        if let Err(e) = t.join() {
            eprintln!("[main] audio thread panicked: {e:?}");
        }
    }
    if let Err(e) = client_thread.join() {
        eprintln!("[main] client manager thread panicked: {e:?}");
    }

    audio::remove_virtual_sink(audio_module_id);

    if let Some(handle) = mdns_handle {
        handle.shutdown();
    }

    println!("screx-daemon shutdown complete");
    Ok(())
}
