mod audio;
mod camera;
mod capture;
mod crypto;
mod display_manager;
mod encode;
mod input;
mod logging;
mod pairing;
mod pipeline;
mod platform;
mod protocol;
mod stream_server;
#[cfg(test)]
mod test_support;

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
    /// via `SETTINGS`; also the default when `SETTINGS` omits the resolution)
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

    /// Disable forwarding host audio to the iPad speaker
    #[arg(long, default_value_t = false)]
    no_speaker: bool,

    /// Disable the virtual microphone fed by the iPad mic
    #[arg(long, default_value_t = false)]
    no_mic: bool,

    /// Disable the virtual webcam fed by the iPad camera
    #[arg(long, default_value_t = false)]
    no_camera: bool,

    /// Disable game controller passthrough
    #[arg(long, default_value_t = false)]
    no_gamepad: bool,
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
    codec: encode::VideoCodec,
    width: u32,
    height: u32,
    fps: u32,
    gop: u32,
    bitrate_bps: u32,
    intra_refresh: bool,
    stream_port: u16,
    camera_exclusive_caps: bool,
    features: stream_server::FeatureToggles,
}

impl AppConfig {
    // `width`/`height`/`fps`/`bitrate_bps` here are sourced 1:1 from the
    // renamed `--max-*` CLI flags. They now mean "session ceiling" (per
    // `DaemonCapabilities`) *and* "session default when a client never
    // negotiates via SETTINGS" — the same field does both jobs, since an
    // un-negotiated session is defined as "get the daemon's max."
    fn from_cli(cli: &Cli) -> Self {
        Self {
            encoder_backend: encode::EncoderBackend::from_str(&cli.backend),
            codec: encode::VideoCodec::from_str(&cli.codec),
            width: cli.max_width,
            height: cli.max_height,
            fps: cli.max_framerate,
            gop: cli.keyframe.max(10),
            bitrate_bps: cli.max_bitrate,
            intra_refresh: cli.intra_refresh,
            stream_port: cli.port,
            camera_exclusive_caps: !cli.no_camera_exclusive_caps,
            features: stream_server::FeatureToggles {
                speaker: !cli.no_speaker,
                microphone: !cli.no_mic,
                camera: !cli.no_camera,
                gamepad: !cli.no_gamepad,
            },
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
        config.features,
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
        config.features,
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

            // Input backend cleanup: release anything still held so a
            // dropped session can't leave a button or key stuck on the host,
            // and remove virtual gamepads.
            if let Ok(mut backend) = shared_d.input_backend.lock() {
                backend.release_mouse_buttons();
                backend.release_keys();
                for id in 0..4 {
                    backend.gamepad_detach(id);
                }
            }

            // Reset audio output flag first so the loopback capture stops
            // before the virtual sink is removed.
            shared_d.audio_output_enabled.store(false, Ordering::SeqCst);

            // Audio sink
            crate::stream_server::disable_virtual_sink(&shared_d);
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
    let capture_thread = thread::Builder::new()
        .name("display-manager".into())
        .spawn(move || pipeline::run(send_socket, capture_shared, capture_stop, config))
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
        shared.pending_settings.1.notify_all();

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
