mod audio;
mod camera;
mod capture;
mod crypto;
mod doctor;
mod encode;
mod logging;
mod pairing;
mod stream_server;
mod uinput;
mod usb;

use std::net::UdpSocket;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

fn parse_bitrate(s: &str) -> std::result::Result<u32, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty value".into());
    }
    let (num_part, multiplier) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1_000u64),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1_000_000u64),
        _ => (s, 1u64),
    };
    let num: f64 = num_part.parse().map_err(|e| format!("invalid number: {e}"))?;
    let val = (num * multiplier as f64) as u64;
    u32::try_from(val).map_err(|_| format!("bitrate {val} exceeds u32 max"))
}

#[derive(Parser, Debug)]
#[command(name = "screx", about = "Low-latency Linux-to-iPad screen streaming daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Virtual display width
    #[arg(short, long, default_value_t = 2160)]
    width: u32,

    /// Virtual display height (uses -H to avoid conflict with --help)
    #[arg(short = 'H', long, default_value_t = 1620)]
    height: u32,

    /// Target framerate
    #[arg(short, long, default_value_t = 30)]
    framerate: u32,

    /// Keyframe interval (in frames)
    #[arg(short, long, default_value_t = 30)]
    keyframe: u32,

    /// Encoder bitrate (e.g. 8000000, 8M, 500K)
    #[arg(short = 'r', long, default_value = "8M", value_parser = parse_bitrate)]
    bitrate: u32,

    /// UDP/TCP streaming port
    #[arg(short, long, default_value_t = 9000)]
    port: u16,

    /// Encoder backend: auto, vaapi, nvenc, software
    #[arg(short, long, default_value = "auto")]
    backend: String,

    /// Video codec: h264, h265
    #[arg(short, long, default_value = "h264")]
    codec: String,

    /// Enable detailed diagnostic logs
    #[arg(short, long, default_value_t = false)]
    verbose: bool,

    /// Network only — disable USB transport
    #[arg(long, default_value_t = false, conflicts_with = "usb_only")]
    network_only: bool,

    /// USB only — disable network pairing and UDP streaming
    #[arg(long, default_value_t = false, conflicts_with = "network_only")]
    usb_only: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run host readiness checks
    Doctor,
    /// List or remove paired devices
    Unpair {
        /// Device ID to unpair, or --all to remove all
        device_id: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct AppConfig {
    encoder_backend: encode::EncoderBackend,
    codec: encode::VideoCodec,
    width: u32,
    height: u32,
    fps: u32,
    gop: u32,
    bitrate_bps: u32,
    stream_port: u16,
}

impl AppConfig {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            encoder_backend: encode::EncoderBackend::from_str(&cli.backend),
            codec: encode::VideoCodec::from_str(&cli.codec),
            width: cli.width,
            height: cli.height,
            fps: cli.framerate,
            gop: cli.keyframe.max(10),
            bitrate_bps: cli.bitrate,
            stream_port: cli.port,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    logging::set_verbose(cli.verbose);

    match &cli.command {
        Some(Commands::Doctor) => return doctor::run_doctor(),
        Some(Commands::Unpair { device_id }) => {
            return pairing::run_unpair(device_id.as_deref());
        }
        None => {}
    }

    let config = AppConfig::from_cli(&cli);
    let transport_mode = if cli.network_only {
        "network-only"
    } else if cli.usb_only {
        "usb-only"
    } else {
        "network + usb"
    };
    println!("screx v2 config: {config:?}");
    println!("[main] transport mode: {transport_mode}");
    if cli.verbose {
        println!("[main] verbose logging enabled");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(stream_server::SharedState::new());

    // UDP socket for streaming
    let socket = UdpSocket::bind(("0.0.0.0", config.stream_port))
        .with_context(|| format!("failed to bind UDP port {}", config.stream_port))?;

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

    // Pairing state + TCP handshake server (not needed in USB-only mode)
    let pairing_state = Arc::new(std::sync::Mutex::new(pairing::PairingState::load()));
    let session_rx: Arc<std::sync::Mutex<Option<pairing::SessionInfo>>> =
        Arc::new(std::sync::Mutex::new(None));

    if !cli.usb_only {
        let ps = Arc::clone(&pairing_state);
        let sr = Arc::clone(&session_rx);
        let pairing_shared = Arc::clone(&shared);
        let pairing_stop = Arc::clone(&stop);
        let port = config.stream_port;
        thread::Builder::new()
            .name("pairing".into())
            .spawn(move || {
                if let Err(e) = pairing::run_pairing_server(port, ps, sr, pairing_shared, pairing_stop) {
                    eprintln!("[pairing] server error: {e:#}");
                }
            })
            .context("failed to spawn pairing thread")?;
    } else {
        println!("[main] USB-only mode — network pairing disabled");
    }

    // Virtual touchscreen + keyboard (always running — needed for input even
    // before video starts, and uinput devices are lightweight)
    match uinput::VirtualTouchscreen::new(config.width, config.height) {
        Ok(ts) => {
            ts.map_to_output();
            *shared.virtual_touch.lock().unwrap() = Some(ts);
            println!("[main] virtual touchscreen ready");
        }
        Err(e) => {
            eprintln!("[main] virtual touchscreen failed (touch disabled): {e:#}");
        }
    }
    match uinput::VirtualKeyboard::new() {
        Ok(kb) => {
            *shared.virtual_keyboard.lock().unwrap() = Some(kb);
            println!("[main] virtual keyboard ready");
        }
        Err(e) => {
            eprintln!("[main] virtual keyboard failed (keyboard disabled): {e:#}");
        }
    }

    // Clean up stale audio modules from a previous crash
    audio::cleanup_stale_modules();

    // -----------------------------------------------------------------------
    // Lifecycle callbacks — create peripherals on connect, remove on disconnect
    // -----------------------------------------------------------------------

    // State shared with the lifecycle callbacks
    let audio_module_id = Arc::new(std::sync::Mutex::new(0u32));

    {
        let shared_c = Arc::clone(&shared);
        let audio_id_c = Arc::clone(&audio_module_id);
        *shared.on_client_connected.lock().unwrap() = Some(Box::new(move || {
            println!("[lifecycle] client connected — creating peripherals");

            // Virtual camera
            match camera::load_v4l2loopback() {
                Ok(()) => match camera::create_cam_writer() {
                    Ok(writer) => {
                        *shared_c.cam_writer.lock().unwrap() = Some(writer);
                        println!("[lifecycle] camera: virtual webcam ready");
                    }
                    Err(e) => eprintln!("[lifecycle] camera: {e:#}"),
                },
                Err(e) => eprintln!("[lifecycle] camera: v4l2loopback not available ({e:#})"),
            }

            // Virtual microphone
            match audio::create_virtual_mic() {
                Ok(writer) => {
                    *shared_c.mic_writer.lock().unwrap() = Some(writer);
                    println!("[lifecycle] mic: virtual microphone ready");
                }
                Err(e) => eprintln!("[lifecycle] mic: {e:#}"),
            }

            // Virtual audio sink + capture
            match audio::create_virtual_sink() {
                Ok(id) => {
                    *audio_id_c.lock().unwrap() = id;
                    println!("[lifecycle] audio: virtual sink ready (module {id})");
                }
                Err(e) => eprintln!("[lifecycle] audio: {e:#}"),
            }
        }));
    }

    {
        let shared_d = Arc::clone(&shared);
        let audio_id_d = Arc::clone(&audio_module_id);
        *shared.on_client_disconnected.lock().unwrap() = Some(Box::new(move || {
            println!("[lifecycle] client disconnected — removing peripherals");

            // Camera
            *shared_d.cam_writer.lock().unwrap() = None;

            // Mic
            if let Some(ref mut mic) = *shared_d.mic_writer.lock().unwrap() {
                audio::remove_virtual_mic(mic);
            }
            *shared_d.mic_writer.lock().unwrap() = None;

            // Virtual mouse (physical peripheral)
            *shared_d.virtual_mouse.lock().unwrap() = None;

            // Virtual gamepads
            shared_d.virtual_gamepads.lock().unwrap().clear();

            // Audio sink
            let mid = *audio_id_d.lock().unwrap();
            if mid > 0 {
                audio::remove_virtual_sink(mid);
                *audio_id_d.lock().unwrap() = 0;
            }

            // Signal capture thread to stop (EVDI will be torn down)
            shared_d.capture_stop_flag.store(true, Ordering::SeqCst);
            shared_d.capture_start.store(false, Ordering::Release);
        }));
    }

    // -----------------------------------------------------------------------
    // Client manager thread (not needed in USB-only mode)
    // -----------------------------------------------------------------------

    if !cli.usb_only {
        let client_socket = socket.try_clone().context("clone socket for client mgr")?;
        let client_shared = Arc::clone(&shared);
        let client_stop = Arc::clone(&stop);
        let client_session_rx = Arc::clone(&session_rx);
        let _client_thread = thread::Builder::new()
            .name("client-mgr".into())
            .spawn(move || {
                if let Err(e) =
                    stream_server::run_client_manager(client_socket, client_shared, client_stop, client_session_rx)
                {
                    eprintln!("[client] manager error: {e:#}");
                }
            })
            .context("failed to spawn client manager thread")?;
    }

    // -----------------------------------------------------------------------
    // Capture + encode + send thread
    // -----------------------------------------------------------------------

    let send_socket = socket.try_clone().context("clone socket for sender")?;
    let capture_shared = Arc::clone(&shared);
    let capture_stop = Arc::clone(&stop);
    let capture_config = capture::CaptureConfig {
        width: config.width,
        height: config.height,
        fps: config.fps,
    };
    let enc_config = encode::EncoderConfig {
        bitrate_bps: config.bitrate_bps,
        gop: config.gop,
        fps: config.fps,
        width: config.width,
        height: config.height,
        backend: config.encoder_backend,
        codec: config.codec,
    };

    let force_refresh = Arc::new(AtomicBool::new(false));
    let capture_force_refresh = Arc::clone(&force_refresh);
    shared
        .force_refresh_handle
        .lock()
        .unwrap()
        .replace(Arc::clone(&force_refresh));

    let capture_start = Arc::clone(&shared.capture_start);
    let capture_stop_flag = Arc::clone(&shared.capture_stop_flag);

    let capture_thread = thread::Builder::new()
        .name("capture".into())
        .spawn(move || -> Result<()> {
            let mut sender = stream_server::UdpSender::new(send_socket);

            loop {
                if capture_stop.load(Ordering::Relaxed) {
                    break;
                }

                // Wait for capture_start to be set (client connected)
                while !capture_start.load(Ordering::Acquire) {
                    if capture_stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }

                // Reset the stop flag for this capture session
                capture_stop_flag.store(false, Ordering::SeqCst);

                // Create encoder fresh for each session
                let mut encoder = match encode::Encoder::new(enc_config.clone()) {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("[capture] encoder init failed: {e:#}");
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        continue;
                    }
                };
                let codec_id = encoder.codec().transport_id();

                // Set up encryption on the sender using the current session key
                if let Some(key) = *capture_shared.session_key.lock().unwrap() {
                    sender.set_cipher(crypto::SessionCipher::new(&key));
                }

                println!("[capture] starting EVDI capture session");

                let session_shared = Arc::clone(&capture_shared);
                let session_stop = Arc::clone(&capture_stop);
                let session_stop_flag = Arc::clone(&capture_stop_flag);
                let session_refresh = Arc::clone(&capture_force_refresh);

                // Combined stop: global stop OR per-session stop (client disconnected)
                let combined_stop = Arc::new(AtomicBool::new(false));
                let cs1 = Arc::clone(&combined_stop);
                let cs2 = Arc::clone(&combined_stop);
                let ss = Arc::clone(&session_stop);
                let sf = Arc::clone(&session_stop_flag);

                // Watchdog thread: sets combined_stop when either flag fires
                let watchdog = thread::Builder::new()
                    .name("capture-wd".into())
                    .spawn(move || {
                        while !cs1.load(Ordering::Relaxed) {
                            if ss.load(Ordering::Relaxed) || sf.load(Ordering::Relaxed) {
                                cs1.store(true, Ordering::SeqCst);
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    })
                    .ok();

                if let Err(e) = capture::run_capture_loop(
                    capture_config.clone(),
                    cs2,
                    Arc::clone(&session_refresh),
                    Arc::new(AtomicBool::new(true)), // already started
                    |frame| {
                        if session_shared.client_backgrounded.load(Ordering::Relaxed) {
                            return;
                        }

                        let force_idr =
                            session_shared.force_idr.swap(false, Ordering::Relaxed);
                        let ts = session_shared.start_time.elapsed().as_millis() as u32;

                        match encoder.encode_frame(&frame, force_idr) {
                            Ok(aus) => {
                                let use_usb = session_shared.usb_active.load(Ordering::Relaxed);
                                let udp_addr = if !use_usb {
                                    *session_shared.client_addr.lock().unwrap()
                                } else {
                                    None
                                };

                                for au in &aus {
                                    if use_usb {
                                        let mut usb =
                                            session_shared.usb_sender.lock().unwrap();
                                        if let Some(ref mut tcp) = *usb {
                                            if let Err(e) = tcp.send_video(
                                                &au.annex_b,
                                                au.is_idr,
                                                ts,
                                                codec_id,
                                            ) {
                                                eprintln!(
                                                    "[pipeline] USB send error: {e:#}"
                                                );
                                                drop(usb);
                                                session_shared
                                                    .usb_active
                                                    .store(false, Ordering::SeqCst);
                                            }
                                            continue;
                                        }
                                    }
                                    if let Some(addr) = udp_addr {
                                        if let Err(e) =
                                            sender.send_frame(au, addr, ts, codec_id)
                                        {
                                            eprintln!("[pipeline] send error: {e:#}");
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("[pipeline] encode error: {e:#}");
                            }
                        }
                    },
                ) {
                    eprintln!("[capture] session error: {e:#}");
                }

                if let Some(wd) = watchdog {
                    let _ = wd.join();
                }

                println!("[capture] EVDI session ended, waiting for next client...");

                // Reset capture_start so we wait for the next client
                capture_start.store(false, Ordering::Release);
            }

            Ok(())
        })
        .context("failed to spawn capture thread")?;

    // -----------------------------------------------------------------------
    // USB transport thread (not needed in network-only mode)
    // -----------------------------------------------------------------------

    if !cli.network_only {
        let usb_shared = Arc::clone(&shared);
        let usb_stop = Arc::clone(&stop);
        let _usb_thread = thread::Builder::new()
            .name("usb".into())
            .spawn(move || {
                usb::run_usb_transport(usb_shared, usb_stop);
            })
            .context("failed to spawn USB transport thread")?;
    } else {
        println!("[main] network-only mode — USB transport disabled");
    }

    // -----------------------------------------------------------------------
    // Audio capture thread (runs continuously, but only captures when sink exists)
    // -----------------------------------------------------------------------

    let audio_socket = socket.try_clone().context("clone socket for audio")?;
    let audio_shared = Arc::clone(&shared);
    let audio_stop = Arc::clone(&stop);
    let _audio_thread = thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            if let Err(e) = audio::run_audio_capture(audio_socket, audio_shared, audio_stop) {
                eprintln!("[audio] capture error: {e:#}");
            }
        })
        .context("failed to spawn audio thread")?;

    // -----------------------------------------------------------------------
    // Wait for Ctrl-C
    // -----------------------------------------------------------------------

    tokio::signal::ctrl_c().await?;
    println!("\nshutdown requested (ctrl-c)");
    stop.store(true, Ordering::SeqCst);
    shared.capture_stop_flag.store(true, Ordering::SeqCst);

    // Cleanup remaining resources
    *shared.virtual_keyboard.lock().unwrap() = None;
    *shared.virtual_touch.lock().unwrap() = None;
    *shared.virtual_mouse.lock().unwrap() = None;
    *shared.cam_writer.lock().unwrap() = None;
    if let Some(ref mut mic) = *shared.mic_writer.lock().unwrap() {
        audio::remove_virtual_mic(mic);
    }
    *shared.mic_writer.lock().unwrap() = None;
    let mid = *audio_module_id.lock().unwrap();
    if mid > 0 {
        audio::remove_virtual_sink(mid);
    }

    let _ = capture_thread.join();

    println!("screx cleanup complete, exiting");
    std::process::exit(0);
}
