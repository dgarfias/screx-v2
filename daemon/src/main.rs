mod audio;
mod camera;
mod capture;
mod crypto;
mod encode;
mod input;
mod logging;
mod pairing;
mod platform;
mod stream_server;

use std::fs;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    let num: f64 = num_part
        .parse()
        .map_err(|e| format!("invalid number: {e}"))?;
    let val = (num * multiplier as f64) as u64;
    u32::try_from(val).map_err(|_| format!("bitrate {val} exceeds u32 max"))
}

#[derive(Parser, Debug)]
#[command(
    name = "screx",
    about = "Low-latency screen streaming daemon for Screx V2"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Maximum virtual display width a session may negotiate (ceiling, not a
    /// fixed value — connecting clients may request anything at or below it
    /// via `STNG`; old clients that never negotiate get this as their
    /// session default)
    #[arg(short = 'w', long = "max-width", default_value_t = 3840)]
    max_width: u32,

    /// Maximum virtual display height a session may negotiate (uses -H to
    /// avoid conflict with --help; ceiling, not a fixed value — see
    /// --max-width)
    #[arg(short = 'H', long = "max-height", default_value_t = 2160)]
    max_height: u32,

    /// Maximum target framerate a session may negotiate (ceiling, not a
    /// fixed value — see --max-width)
    #[arg(short = 'f', long = "max-framerate", default_value_t = 60)]
    max_framerate: u32,

    /// Keyframe interval (in frames)
    #[arg(short, long, default_value_t = 90)]
    keyframe: u32,

    /// Replace periodic IDR keyframes with rolling intra-refresh where the
    /// encoder supports it (software H.264 and NVENC); full IDRs are then
    /// emitted only when a client requests one. Pass --no-intra-refresh to
    /// keep the legacy periodic keyframes.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    intra_refresh: bool,

    /// Maximum encoder bitrate a session may negotiate (e.g. 20000000, 20M,
    /// 500K; ceiling, not a fixed value — see --max-width)
    #[arg(short = 'b', long = "max-bitrate", default_value = "20M", value_parser = parse_bitrate)]
    max_bitrate: u32,

    /// UDP/TCP streaming port
    #[arg(short, long, default_value_t = 9000)]
    port: u16,

    /// Encoder backend: auto, vaapi, nvenc, software
    #[arg(short = 'e', long, default_value = "auto")]
    backend: String,

    /// Video codec: h264, h265
    #[arg(short, long, default_value = "h264")]
    codec: String,

    /// Enable detailed diagnostic logs
    #[arg(short, long, default_value_t = false)]
    verbose: bool,

    /// Disable v4l2loopback exclusive capture caps for better app compatibility
    #[arg(long, default_value_t = false)]
    no_camera_exclusive_caps: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// List or remove paired devices
    Unpair {
        /// Device ID to unpair, or --all to remove all
        device_id: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct AppConfig {
    encoder_backend: encode::EncoderBackend,
    capture_backend: capture::CaptureBackend,
    codec: encode::VideoCodec,
    width: u32,
    height: u32,
    fps: u32,
    gop: u32,
    bitrate_bps: u32,
    intra_refresh: bool,
    stream_port: u16,
    camera_exclusive_caps: bool,
}

impl AppConfig {
    // `width`/`height`/`fps`/`bitrate_bps` here are sourced 1:1 from the
    // renamed `--max-*` CLI flags. They now mean "session ceiling" (per
    // `DaemonCapabilities`) *and* "session default when a client never
    // negotiates via STNG" — the same field does both jobs, since an
    // un-negotiated session is defined as "get the daemon's max."
    fn from_cli(cli: &Cli) -> Self {
        Self {
            encoder_backend: encode::EncoderBackend::from_str(&cli.backend),
            capture_backend: capture::CaptureBackend::platform_default(),
            codec: encode::VideoCodec::from_str(&cli.codec),
            width: cli.max_width,
            height: cli.max_height,
            fps: cli.max_framerate,
            gop: cli.keyframe.max(10),
            bitrate_bps: cli.max_bitrate,
            intra_refresh: cli.intra_refresh,
            stream_port: cli.port,
            camera_exclusive_caps: !cli.no_camera_exclusive_caps,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    logging::set_verbose(cli.verbose);

    match &cli.command {
        Some(Commands::Unpair { device_id }) => {
            return pairing::run_unpair(device_id.as_deref());
        }
        None => {}
    }

    let config = AppConfig::from_cli(&cli);
    println!("screx v2 config: {config:?}");
    if cli.verbose {
        println!("[main] verbose logging enabled");
    }

    // One-time startup probe, just for an operator-visible log line — the
    // real per-connection `CAPS` message re-probes fresh every time (see
    // `stream_server::build_caps_message`), since a driver installed after
    // the daemon started should be reflected in the next connection.
    let startup_capabilities = stream_server::probe_capabilities(
        config.width,
        config.height,
        config.fps,
        config.bitrate_bps,
        config.encoder_backend,
    );
    println!("[main] daemon capabilities: {startup_capabilities:?}");

    let stop = Arc::new(AtomicBool::new(false));

    let input_backend: Arc<Mutex<dyn crate::input::InputBackend>> = {
        let backend = crate::platform::linux::uinput::LinuxInput::new(config.width, config.height)
            .map_err(|e| {
                eprintln!("[main] input backend failed (input disabled): {e:#}");
                e
            })?;
        Arc::new(Mutex::new(backend))
    };

    let shared = Arc::new(stream_server::SharedState::new(
        config.camera_exclusive_caps,
        config.bitrate_bps,
        Arc::clone(&input_backend),
        config.width,
        config.height,
        config.fps,
        config.bitrate_bps,
        config.encoder_backend,
    ));

    *shared.keyboard_worker.lock().unwrap() = Some(crate::input::KeyboardWorker::new(Arc::clone(
        &input_backend,
    )));

    // UDP socket for streaming
    let socket = UdpSocket::bind(("0.0.0.0", config.stream_port))
        .with_context(|| format!("failed to bind UDP port {}", config.stream_port))?;

    if let Err(e) = crate::platform::net::tune_udp_socket(&socket) {
        eprintln!("[main] failed to tune UDP socket: {e:#}");
    }

    println!("[main] UDP socket bound on port {}", config.stream_port);

    // Pairing state + TCP handshake server
    let pairing_state = Arc::new(std::sync::Mutex::new(pairing::PairingState::load()));
    let session_rx: Arc<std::sync::Mutex<Option<pairing::SessionInfo>>> =
        Arc::new(std::sync::Mutex::new(None));

    let pairing_thread = {
        let ps = Arc::clone(&pairing_state);
        let sr = Arc::clone(&session_rx);
        let pairing_shared = Arc::clone(&shared);
        let pairing_stop = Arc::clone(&stop);
        let port = config.stream_port;
        thread::Builder::new()
            .name("pairing".into())
            .spawn(move || {
                if let Err(e) =
                    pairing::run_pairing_server(port, ps, sr, pairing_shared, pairing_stop)
                {
                    eprintln!("[pairing] server error: {e:#}");
                }
            })
            .context("failed to spawn pairing thread")?
    };

    // Clean up stale audio modules from a previous crash
    audio::cleanup_stale_modules();

    // -----------------------------------------------------------------------
    // Lifecycle callbacks — create peripherals on connect, remove on disconnect
    // -----------------------------------------------------------------------

    {
        let shared_c = Arc::clone(&shared);
        *shared.on_client_connected.lock().unwrap() = Some(Box::new(move || {
            println!("[lifecycle] client connected");
            // Peripherals (camera, mic, speaker) are now created on-demand
            // when the client sends the corresponding enable signal (CAMCFG,
            // MICCFG, SPKR). Nothing to create eagerly here.
            let _ = &shared_c; // keep the Arc alive for future use
        }));
    }

    {
        let shared_d = Arc::clone(&shared);
        *shared.on_client_disconnected.lock().unwrap() = Some(Box::new(move || {
            println!("[lifecycle] client disconnected — removing peripherals");

            // Camera
            *shared_d.cam_writer.lock().unwrap() = None;

            // Mic
            if let Some(ref mut mic) = *shared_d.mic_writer.lock().unwrap() {
                audio::remove_virtual_mic(mic);
            }
            *shared_d.mic_writer.lock().unwrap() = None;

            // Reset audio output flag first so the WASAPI loopback thread stops
            // before the Steam Streaming Speakers devnode is disabled.
            shared_d.audio_output_enabled.store(false, Ordering::SeqCst);

            // Audio sink
            crate::stream_server::disable_virtual_sink(&shared_d);

            // Discard any STNG left over from this connection so it can't
            // leak into the next session (e.g. a client that sent STNG then
            // aborted before ever starting to stream).
            {
                let (lock, _) = &*shared_d.pending_settings;
                *lock.lock().unwrap() = None;
            }

            // Signal capture thread to stop (EVDI will be torn down)
            shared_d.capture_stop_flag.store(true, Ordering::SeqCst);
            shared_d.capture_start.store(false, Ordering::Release);
            let (_, cvar) = &*shared_d.capture_start_signal;
            cvar.notify_all();
        }));
    }

    // -----------------------------------------------------------------------
    // Client manager thread
    // -----------------------------------------------------------------------

    let client_thread = {
        let client_socket = socket.try_clone().context("clone socket for client mgr")?;
        let client_shared = Arc::clone(&shared);
        let client_stop = Arc::clone(&stop);
        let client_session_rx = Arc::clone(&session_rx);
        thread::Builder::new()
            .name("client-mgr".into())
            .spawn(move || {
                if let Err(e) = stream_server::run_client_manager(
                    client_socket,
                    client_shared,
                    client_stop,
                    client_session_rx,
                ) {
                    eprintln!("[client] manager error: {e:#}");
                }
            })
            .context("failed to spawn client manager thread")?
    };

    // -----------------------------------------------------------------------
    // Capture + encode + send thread
    // -----------------------------------------------------------------------

    let send_socket = socket.try_clone().context("clone socket for sender")?;
    let capture_shared = Arc::clone(&shared);
    let capture_stop = Arc::clone(&stop);
    // Used once, below, to construct the display backend before the
    // per-session loop starts (see the "do NOT reconstruct it per session"
    // note on `capture_config` inside the loop). `config` itself is moved
    // into the capture thread closure so each session can merge in
    // client-requested (STNG) settings over these CLI/max defaults.
    let capture_config = capture::CaptureConfig {
        width: config.width,
        height: config.height,
        fps: config.fps,
        backend: config.capture_backend,
    };

    let force_refresh = Arc::new(AtomicBool::new(false));
    let capture_force_refresh = Arc::clone(&force_refresh);
    shared
        .force_refresh_handle
        .lock()
        .unwrap()
        .replace(Arc::clone(&force_refresh));

    let capture_start = Arc::clone(&shared.capture_start);
    let capture_start_signal = Arc::clone(&shared.capture_start_signal);
    let capture_stop_flag = Arc::clone(&shared.capture_stop_flag);

    let capture_thread = thread::Builder::new()
        .name("capture".into())
        .spawn(move || -> Result<()> {
            let mut sender = stream_server::UdpSender::new(send_socket);
            // Backend is constructed ONCE for the life of the process, from
            // the CLI/max-derived `capture_config` above — NOT per session.
            // Per-session resolution/fps changes are applied below via
            // `display.attach(mode)`, which this backend already supports
            // being called with fresh each session.
            let mut display = match capture::create_display_backend(&capture_config) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[capture] display backend init failed: {e:#}");
                    return Err(e);
                }
            };

            loop {
                if capture_stop.load(Ordering::Relaxed) {
                    break;
                }

                // Wait for capture_start to be set (client connected)
                while !capture_start.load(Ordering::Acquire) {
                    if capture_stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let (lock, cvar) = &*capture_start_signal;
                    let guard = lock.lock().unwrap();
                    if !capture_start.load(Ordering::Acquire)
                        && !capture_stop.load(Ordering::Relaxed)
                    {
                        let _ = cvar
                            .wait_timeout(guard, std::time::Duration::from_millis(20))
                            .unwrap();
                    }
                }

                // Reset the stop flag for this capture session
                capture_stop_flag.store(false, Ordering::SeqCst);

                // Give a client that just received CAPS a short window to
                // send STNG before this session's config is locked in. On
                // the network path STNG normally arrives over the TCP
                // control channel before the first authenticated UDP packet
                // flips capture_start (stream_server.rs), so this usually
                // returns instantly; the timeout is the compatibility path
                // for old clients that never send STNG at all.
                let requested = {
                    let (lock, cvar) = &*capture_shared.pending_settings;
                    let guard = lock.lock().unwrap();
                    let (mut guard, _timeout) = cvar
                        .wait_timeout_while(
                            guard,
                            std::time::Duration::from_millis(2000),
                            |settings| settings.is_none(),
                        )
                        .unwrap();
                    guard.take()
                };

                // Merge the (already-clamped) requested settings over the
                // CLI/max defaults to get this session's actual config.
                // Bitrate/resolution/fps/codec are the "full rebuild" case,
                // but that's fine — Encoder::new and display.attach() are
                // already fresh per session below.
                let session_width = requested.as_ref().and_then(|r| r.width).unwrap_or(config.width);
                let session_height = requested
                    .as_ref()
                    .and_then(|r| r.height)
                    .unwrap_or(config.height);
                let session_fps = requested.as_ref().and_then(|r| r.fps).unwrap_or(config.fps);
                let session_codec = requested
                    .as_ref()
                    .and_then(|r| r.codec)
                    .unwrap_or(config.codec);
                let session_bitrate_bps = requested
                    .as_ref()
                    .and_then(|r| r.bitrate_bps)
                    .unwrap_or(config.bitrate_bps);

                if let Some(ref r) = requested {
                    println!(
                        "[capture] starting session with negotiated settings: {r:?} -> {session_width}x{session_height}@{session_fps} codec={session_codec:?} bitrate={session_bitrate_bps}"
                    );
                }

                // Rebaseline the adaptive bitrate loop around this session's
                // bitrate so it throttles up/down from the negotiated value
                // instead of the CLI default. Restored on session teardown
                // below.
                capture_shared.set_base_bitrate(session_bitrate_bps);

                let capture_config = capture::CaptureConfig {
                    width: session_width,
                    height: session_height,
                    fps: session_fps,
                    backend: config.capture_backend,
                };
                let enc_config = encode::EncoderConfig {
                    bitrate_bps: session_bitrate_bps,
                    gop: config.gop,
                    fps: session_fps,
                    width: session_width,
                    height: session_height,
                    backend: config.encoder_backend,
                    codec: session_codec,
                    intra_refresh: config.intra_refresh,
                };

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
                // Pin the UDP source address to the IP the client dialed
                // (multi-homed hosts may otherwise reply from another IP).
                sender.set_source(*capture_shared.udp_source.lock().unwrap());

                println!("[capture] attaching display backend");
                if let Err(e) = display.attach(capture::DisplayMode {
                    width: capture_config.width,
                    height: capture_config.height,
                    fps: capture_config.fps,
                }) {
                    eprintln!("[capture] display attach failed: {e:#}");
                    capture_shared.set_base_bitrate(config.bitrate_bps);
                    capture_start.store(false, Ordering::Release);
                    let (_, cvar) = &*capture_start_signal;
                    cvar.notify_all();
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }

                // Backends that inject via absolute screen coordinates (Windows,
                // macOS) need to know where the captured output actually sits on
                // the desktop; wire touch/mouse coordinates only carry
                // frame-local 0..width/0..height values in the negotiated
                // session resolution — NOT necessarily the output's real
                // on-screen size (macOS: WindowServer can silently substitute a
                // persisted display-mode preference for the virtual display's
                // requested mode, so the live rect can come back smaller/larger
                // than the session resolution). Prefer the backend's own notion
                // of output placement (Windows/macOS); fall back to the session
                // resolution at the origin otherwise — Linux ignores left/top
                // and instead scales incoming coordinates onto its fixed-size
                // virtual touchscreen, but still needs the session dims to do
                // that.
                //
                // `set_output_size` reports the live rect's actual w/h (which
                // can differ from the session dims on macOS); `set_target_rect`
                // always carries the SESSION dims per the trait's documented
                // contract (Linux needs those, Windows/macOS rect size ==
                // session dims in the common case). MacInput is the only
                // backend that keeps both around, to scale wire coordinates
                // from session space onto the real output rect.
                let (left, top, rect_width, rect_height) = display
                    .output_rect()
                    .unwrap_or((0, 0, capture_config.width, capture_config.height));
                println!(
                    "[capture] input target rect: left={left} top={top} width={rect_width} height={rect_height} \
                     (session resolution {}x{})",
                    capture_config.width, capture_config.height
                );
                if let Ok(mut backend) = capture_shared.input_backend.lock() {
                    backend.set_output_size(rect_width, rect_height);
                    backend.set_target_rect(left, top, capture_config.width, capture_config.height);
                }

                println!("[capture] starting display capture session");

                let session_shared = Arc::clone(&capture_shared);
                let session_stop = Arc::clone(&capture_stop);
                let session_stop_flag = Arc::clone(&capture_stop_flag);
                let session_refresh = Arc::clone(&capture_force_refresh);
                let mut dump_capture_remaining = std::env::var("SCREX_DUMP_CAPTURE_FRAMES")
                    .ok()
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(0);
                let mut dump_au_remaining = std::env::var("SCREX_DUMP_SENT_AUS")
                    .ok()
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(0);

                // Combined stop: global stop OR per-session stop (client disconnected)
                let combined_stop = Arc::new(AtomicBool::new(false));
                let cs1 = Arc::clone(&combined_stop);
                let cs2 = Arc::clone(&combined_stop);
                let ss = Arc::clone(&session_stop);
                let sf = Arc::clone(&session_stop_flag);

                // Watchdog thread: sets combined_stop when either flag fires
                let watchdog =
                    match thread::Builder::new()
                        .name("capture-wd".into())
                        .spawn(move || {
                            while !cs1.load(Ordering::Relaxed) {
                                if ss.load(Ordering::Relaxed) || sf.load(Ordering::Relaxed) {
                                    cs1.store(true, Ordering::SeqCst);
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                        }) {
                        Ok(handle) => handle,
                        Err(e) => {
                            eprintln!("[capture] failed to spawn watchdog thread: {e}");
                            capture_stop_flag.store(true, Ordering::SeqCst);
                            capture_start.store(false, Ordering::Release);
                            let (_, cvar) = &*capture_start_signal;
                            cvar.notify_all();
                            std::thread::sleep(std::time::Duration::from_secs(1));
                            continue;
                        }
                    };

                if let Err(e) = display.run_capture_loop(
                    &*cs2,
                    &*session_refresh,
                    &mut |frame: capture::CaptureFrame<'_>| {
                        if dump_capture_remaining > 0 {
                            dump_capture_frame_ppm(dump_capture_remaining, &frame);
                            dump_capture_remaining -= 1;
                        }
                        let force_idr = session_shared.force_idr.swap(false, Ordering::Relaxed);
                        let ts = session_shared.start_time.elapsed().as_millis() as u32;
                        let tuning = session_shared.current_stream_tuning();

                        if tuning.bitrate_bps != encoder.bitrate_bps() {
                            if let Err(e) = encoder.reconfigure_bitrate(tuning.bitrate_bps) {
                                eprintln!("[pipeline] encoder retune failed: {e:#}");
                            }
                        }

                        match encoder.encode_frame(&frame, force_idr) {
                            Ok(aus) => {
                                if dump_au_remaining > 0 {
                                    for au in &aus {
                                        if dump_au_remaining == 0 {
                                            break;
                                        }
                                        dump_sent_access_unit(
                                            dump_au_remaining,
                                            codec_id,
                                            au.is_idr,
                                            &*au.annex_b,
                                        );
                                        dump_au_remaining -= 1;
                                    }
                                }
                                let udp_addr = *session_shared.client_addr.lock().unwrap();

                                for au in &aus {
                                    if let Some(addr) = udp_addr {
                                        match sender.send_frame(au, addr, ts, codec_id, tuning) {
                                            Ok(bytes) => {
                                                session_shared
                                                    .bytes_tx
                                                    .fetch_add(bytes, Ordering::Relaxed);
                                            }
                                            Err(e) => {
                                                eprintln!("[pipeline] send error: {e:#}");
                                            }
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

                display.detach();

                let _ = watchdog.join();

                // Restore the CLI/max default bitrate baseline so the next
                // session — which may be an old client that never sends
                // STNG — isn't stuck ramping from this session's negotiated
                // bitrate.
                capture_shared.set_base_bitrate(config.bitrate_bps);

                println!("[capture] display session ended, waiting for next client...");

                // Reset capture_start so we wait for the next client
                capture_start.store(false, Ordering::Release);
                let (_, cvar) = &*capture_start_signal;
                cvar.notify_all();
            }

            Ok(())
        })
        .context("failed to spawn capture thread")?;

    // -----------------------------------------------------------------------
    // Audio capture thread (runs continuously, but only captures when sink exists)
    // -----------------------------------------------------------------------

    let audio_socket = socket.try_clone().context("clone socket for audio")?;
    let audio_shared = Arc::clone(&shared);
    let audio_stop = Arc::clone(&stop);
    let audio_thread = thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            if let Err(e) = audio::run_audio_capture(audio_socket, audio_shared, audio_stop) {
                eprintln!("[audio] capture error: {e:#}");
            }
        })
        .context("failed to spawn audio thread")?;

    // -----------------------------------------------------------------------
    // Wait for shutdown, then clean up
    // -----------------------------------------------------------------------

    let do_shutdown = move || {
        stop.store(true, Ordering::SeqCst);
        shared.capture_stop_flag.store(true, Ordering::SeqCst);
        {
            let (_, cvar) = &*shared.capture_start_signal;
            cvar.notify_all();
        }

        // Cleanup remaining resources
        *shared.keyboard_worker.lock().unwrap() = None;
        *shared.cam_writer.lock().unwrap() = None;
        if let Some(ref mut mic) = *shared.mic_writer.lock().unwrap() {
            audio::remove_virtual_mic(mic);
        }
        *shared.mic_writer.lock().unwrap() = None;
        crate::stream_server::disable_virtual_sink(&shared);

        let _ = capture_thread.join();
        if let Err(err) = audio_thread.join() {
            eprintln!("[audio] thread join failed: {err:?}");
        }
        let _ = client_thread.join();
        let _ = pairing_thread.join();

        println!("screx cleanup complete, exiting");
    };

    tokio::signal::ctrl_c().await?;
    println!("\nshutdown requested (ctrl-c)");
    do_shutdown();

    Ok(())
}

fn dump_capture_frame_ppm(index: u32, frame: &capture::CaptureFrame<'_>) {
    if frame.format != capture::CapturePixelFormat::Bgra {
        eprintln!("[capture] raw PPM dump only supports BGRA capture frames");
        return;
    }
    let path = std::env::temp_dir()
        .join(format!("screx-daemon-capture-{index:03}.ppm"))
        .to_string_lossy()
        .to_string();
    let mut ppm = Vec::with_capacity(32 + frame.data.len() / 4 * 3);
    ppm.extend_from_slice(format!("P6\n{} {}\n255\n", frame.width, frame.height).as_bytes());
    for px in frame.data.chunks_exact(4) {
        ppm.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    match fs::write(&path, ppm) {
        Ok(()) => println!("[capture] dumped raw capture frame to {path}"),
        Err(error) => eprintln!("[capture] failed to dump raw capture frame to {path}: {error}"),
    }
}

fn dump_sent_access_unit(index: u32, codec_id: u8, is_idr: bool, annex_b: &[u8]) {
    let ext = if codec_id == 0x01 { "h265" } else { "h264" };
    let path = std::env::temp_dir()
        .join(format!("screx-daemon-au-{index:03}.{ext}"))
        .to_string_lossy()
        .to_string();
    match fs::write(&path, annex_b) {
        Ok(()) => println!(
            "[capture] dumped encoded access unit to {} codec={} idr={} bytes={}",
            path,
            codec_id,
            is_idr,
            annex_b.len()
        ),
        Err(error) => eprintln!("[capture] failed to dump encoded access unit to {path}: {error}"),
    }
}
