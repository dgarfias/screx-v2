use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::UdpSocket;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use opus_decoder::OpusDecoder;

use crate::stream_server::{AudioSender, SharedState};

const SINK_NAME: &str = "screx_ipad";
const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const CHUNK_DURATION_MS: u32 = 10;
const SAMPLES_PER_CHUNK: usize = (SAMPLE_RATE / 1000 * CHUNK_DURATION_MS) as usize;
const BYTES_PER_CHUNK: usize = SAMPLES_PER_CHUNK * CHANNELS as usize * 2;

const MIC_FIFO_PATH: &str = "/tmp/screx_mic";
const MIC_SOURCE_NAME: &str = "screx_mic";
const MIC_SINK_NAME: &str = "screx_mic_sink";
const MIC_RATE: u32 = 48000;
const MIC_MAX_FRAME: usize = 5760; // 120ms at 48kHz (max Opus frame)

fn pulse_env() -> Vec<(String, String)> {
    let mut env = Vec::new();

    // If PULSE_SERVER is already set, use it as-is
    if std::env::var("PULSE_SERVER").is_ok() {
        return env;
    }

    // SCREX_PULSE_UID > SUDO_UID > auto-detect
    if let Ok(uid) = std::env::var("SCREX_PULSE_UID").or_else(|_| std::env::var("SUDO_UID")) {
        let runtime_dir = format!("/run/user/{uid}");
        env.push(("XDG_RUNTIME_DIR".into(), runtime_dir.clone()));
        env.push((
            "PULSE_SERVER".into(),
            format!("unix:{runtime_dir}/pulse/native"),
        ));
        return env;
    }

    // Running as root directly (no sudo) -- try to find a user PulseAudio socket
    if unsafe { libc::getuid() } == 0 {
        if let Ok(entries) = std::fs::read_dir("/run/user") {
            for entry in entries.flatten() {
                let pulse_path = entry.path().join("pulse/native");
                if pulse_path.exists() {
                    let runtime_dir = entry.path().to_string_lossy().to_string();
                    println!(
                        "[audio] auto-detected PulseAudio socket at {}",
                        pulse_path.display()
                    );
                    env.push(("XDG_RUNTIME_DIR".into(), runtime_dir.clone()));
                    env.push((
                        "PULSE_SERVER".into(),
                        format!("unix:{}", pulse_path.display()),
                    ));
                    return env;
                }
            }
        }
    }

    env
}

fn pactl_cmd() -> Command {
    let mut cmd = Command::new("pactl");
    for (k, v) in pulse_env() {
        cmd.env(&k, &v);
    }
    cmd
}

/// Remove any leftover Screx PulseAudio/PipeWire modules from a previous crash.
pub fn cleanup_stale_modules() {
    let output = match pactl_cmd().args(["list", "short", "modules"]).output() {
        Ok(o) => o,
        Err(_) => return,
    };
    let listing = String::from_utf8_lossy(&output.stdout);
    let screx_names = [
        &format!("sink_name={SINK_NAME}"),
        &format!("source_name={MIC_SOURCE_NAME}"),
        &format!("sink_name={MIC_SINK_NAME}"),
    ];

    for line in listing.lines() {
        if screx_names.iter().any(|n| line.contains(n.as_str())) {
            if let Some(id_str) = line.split_whitespace().next() {
                if let Ok(id) = id_str.parse::<u32>() {
                    let _ = pactl_cmd()
                        .args(["unload-module", &id.to_string()])
                        .output();
                    println!("[audio] cleaned up stale module {id}: {}", line.trim());
                }
            }
        }
    }

    let _ = fs::remove_file(MIC_FIFO_PATH);
}

pub fn create_virtual_sink() -> Result<u32> {
    let existing = pactl_cmd()
        .args(["list", "short", "modules"])
        .output()
        .context("pactl not found")?;

    let output = String::from_utf8_lossy(&existing.stdout);
    let needle = format!("sink_name={SINK_NAME}");
    for line in output.lines() {
        if let Some(pos) = line.find(&needle) {
            let after = pos + needle.len();
            let next_char = line[after..].chars().next();
            // Only match if the sink name is followed by a space, tab, or end-of-line.
            if next_char.is_none() || next_char == Some(' ') || next_char == Some('\t') {
                let module_id: u32 = line
                    .split_whitespace()
                    .next()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0);
                println!("[audio] virtual sink '{SINK_NAME}' already exists (module {module_id})");
                return Ok(module_id);
            }
        }
    }

    let result = pactl_cmd()
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={SINK_NAME}"),
            &format!("sink_properties=device.description=\"Screx iPad\""),
            &format!("rate={SAMPLE_RATE}"),
            "channels=2",
            "format=s16le",
        ])
        .output()
        .context("failed to create virtual sink")?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        anyhow::bail!("pactl load-module failed: {stderr}");
    }

    let module_id: u32 = String::from_utf8_lossy(&result.stdout)
        .trim()
        .parse()
        .unwrap_or(0);

    println!("[audio] created virtual sink '{SINK_NAME}' (module {module_id})");
    Ok(module_id)
}

pub fn remove_virtual_sink(module_id: u32) {
    if module_id == 0 {
        return;
    }
    let _ = pactl_cmd()
        .args(["unload-module", &module_id.to_string()])
        .output();
    println!("[audio] removed virtual sink (module {module_id})");
}

// ---------------------------------------------------------------------------
// Audio capture — reads from virtual sink monitor, sends to iPad
// ---------------------------------------------------------------------------

pub fn run_audio_capture(
    socket: UdpSocket,
    shared: Arc<SharedState>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let monitor_source = format!("{SINK_NAME}.monitor");
    let mut sender = AudioSender::new(socket);
    let mut buf = vec![0u8; BYTES_PER_CHUNK];
    let start_time = shared.start_time;

    while !stop.load(Ordering::Relaxed) {
        // Wait for a client to be active AND the virtual sink to actually exist.
        // The sink is created by the SPKR control handler; we must not spawn
        // parec until the PulseAudio module is loaded (audio_module_id > 0).
        if !shared.has_active_client.load(Ordering::Relaxed)
            || !shared.audio_output_enabled.load(Ordering::SeqCst)
            || *shared.audio_module_id.lock().unwrap() == 0
        {
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }

        let mut parec = Command::new("parec");
        for (k, v) in pulse_env() {
            parec.env(&k, &v);
        }
        let child = parec
            .args([
                &format!("--device={monitor_source}"),
                "--format=s16le",
                &format!("--rate={SAMPLE_RATE}"),
                &format!("--channels={CHANNELS}"),
                "--latency-msec=10",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[audio] failed to start parec: {e}");
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };

        // Set up encryption cipher if session key is available
        let mut active_session_key = *shared.session_key.lock().unwrap();
        if let Some(key) = active_session_key {
            sender.set_cipher(crate::crypto::SessionCipher::new(&key));
        } else {
            sender.clear_cipher();
        }

        println!(
            "[audio] capturing from {monitor_source} via parec (pid {})",
            child.id()
        );

        let mut stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                eprintln!("[audio] no stdout from parec");
                let _ = child.kill();
                let _ = child.wait();
                continue;
            }
        };

        loop {
            if stop.load(Ordering::Relaxed) || !shared.audio_output_enabled.load(Ordering::SeqCst) {
                break;
            }
            match stdout.read_exact(&mut buf) {
                Ok(()) => {
                    let ts = start_time.elapsed().as_millis() as u32;

                    if shared.usb_active.load(Ordering::Relaxed) {
                        let mut usb = shared.usb_sender.lock().unwrap();
                        if let Some(ref mut tcp) = *usb {
                            if let Err(e) = tcp.send_audio(&buf, ts) {
                                eprintln!("[audio] USB send error: {e}");
                                drop(usb);
                                shared.usb_active.store(false, Ordering::SeqCst);
                            }
                            continue;
                        }
                    }

                    let current_session_key = *shared.session_key.lock().unwrap();
                    if current_session_key != active_session_key {
                        active_session_key = current_session_key;
                        if let Some(key) = active_session_key {
                            sender.set_cipher(crate::crypto::SessionCipher::new(&key));
                            crate::vlog!("[audio] refreshed UDP audio cipher for new session");
                        } else {
                            sender.clear_cipher();
                            crate::vlog!(
                                "[audio] cleared UDP audio cipher (no active session key)"
                            );
                        }
                    }

                    let client_addr = *shared.client_addr.lock().unwrap();
                    if let Some(addr) = client_addr {
                        if let Err(e) = sender.send_audio(&buf, addr, ts) {
                            eprintln!("[audio] send error: {e}");
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    println!("[audio] parec stream ended (sink removed?)");
                    break;
                }
                Err(e) => {
                    eprintln!("[audio] read error: {e}");
                    break;
                }
            }
        }

        let _ = child.kill();
        let _ = child.wait();
        println!("[audio] capture session stopped, waiting for next client...");
        // Brief backoff so we don't tight-loop if parec exits immediately
        // (e.g. sink was removed between our check and parec connecting).
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    println!("[audio] capture thread exiting");
    Ok(())
}

// ---------------------------------------------------------------------------
// Virtual microphone — receives Opus from iPad, decodes, feeds to PipeWire
// ---------------------------------------------------------------------------

enum MicOutput {
    Fifo(File),
    Pacat(ChildStdin),
}

pub struct MicWriter {
    decoder: OpusDecoder,
    output: MicOutput,
    pub module_ids: Vec<u32>,
    pub fifo_path: Option<String>,
    pacat_child: Option<Child>,
    pcm_buf: Vec<i16>,
    byte_buf: Vec<u8>,
}

// OpusDecoder is Send (pure Rust, no thread-local state)
unsafe impl Send for MicWriter {}

impl MicWriter {
    /// Decode an Opus packet and write PCM to the virtual mic output.
    /// Uses non-blocking I/O — silently drops the frame if the output
    /// buffer is momentarily full (< PIPE_BUF writes are atomic on Linux).
    pub fn write_opus_packet(&mut self, opus_data: &[u8]) -> Result<()> {
        let samples = self
            .decoder
            .decode(opus_data, &mut self.pcm_buf, false)
            .map_err(|e| anyhow::anyhow!("opus decode: {e:?}"))?;

        self.byte_buf.clear();
        for &sample in &self.pcm_buf[..samples] {
            self.byte_buf.extend_from_slice(&sample.to_le_bytes());
        }

        let result = match &mut self.output {
            MicOutput::Fifo(f) => f.write(&self.byte_buf),
            MicOutput::Pacat(stdin) => stdin.write(&self.byte_buf),
        };

        match result {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn create_virtual_mic() -> Result<MicWriter> {
    match try_pipe_source() {
        Ok(w) => return Ok(w),
        Err(e) => {
            println!("[mic] pipe-source failed ({e:#}), trying null-sink fallback...");
        }
    }
    try_null_sink_mic()
}

fn try_pipe_source() -> Result<MicWriter> {
    let _ = fs::remove_file(MIC_FIFO_PATH);

    // Create FIFO ourselves so the module can open it for reading immediately
    let status = Command::new("mkfifo")
        .arg(MIC_FIFO_PATH)
        .status()
        .context("mkfifo not found")?;
    if !status.success() {
        anyhow::bail!("mkfifo failed");
    }

    // Make world-readable so PipeWire (user process) can read it
    let _ = Command::new("chmod").args(["666", MIC_FIFO_PATH]).status();

    let result = pactl_cmd()
        .args([
            "load-module",
            "module-pipe-source",
            &format!("source_name={MIC_SOURCE_NAME}"),
            &format!("file={MIC_FIFO_PATH}"),
            "format=s16le",
            &format!("rate={MIC_RATE}"),
            "channels=1",
            &format!("source_properties=device.description=\"Screx\\ Microphone\""),
        ])
        .output()
        .context("pactl not found")?;

    if !result.status.success() {
        let _ = fs::remove_file(MIC_FIFO_PATH);
        let stderr = String::from_utf8_lossy(&result.stderr);
        anyhow::bail!("module-pipe-source: {}", stderr.trim());
    }

    let module_id: u32 = String::from_utf8_lossy(&result.stdout)
        .trim()
        .parse()
        .unwrap_or(0);

    println!("[mic] module-pipe-source loaded (module {module_id})");

    // Open FIFO for writing with O_NONBLOCK to avoid hanging if module
    // hasn't started reading yet. Retry for up to 2 seconds.
    let mut fifo = None;
    for attempt in 0..20 {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(MIC_FIFO_PATH)
        {
            Ok(f) => {
                fifo = Some(f);
                break;
            }
            Err(e) => {
                if attempt == 19 {
                    let _ = pactl_cmd()
                        .args(["unload-module", &module_id.to_string()])
                        .output();
                    let _ = fs::remove_file(MIC_FIFO_PATH);
                    anyhow::bail!("FIFO open failed after 2s: {e}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    let fifo = fifo.unwrap();
    // Keep O_NONBLOCK — writes < PIPE_BUF (4096) are atomic on Linux,
    // and our frames are ~1920 bytes. Non-blocking prevents stalling
    // the control thread that also handles touch/key events.

    let decoder =
        OpusDecoder::new(MIC_RATE, 1).map_err(|e| anyhow::anyhow!("opus decoder init: {e:?}"))?;

    println!("[mic] virtual mic ready via pipe-source (FIFO)");

    Ok(MicWriter {
        decoder,
        output: MicOutput::Fifo(fifo),
        module_ids: vec![module_id],
        fifo_path: Some(MIC_FIFO_PATH.to_string()),
        pacat_child: None,
        pcm_buf: vec![0i16; MIC_MAX_FRAME],
        byte_buf: Vec::with_capacity(MIC_MAX_FRAME * 2),
    })
}

fn try_null_sink_mic() -> Result<MicWriter> {
    // 1. Internal null-sink (receives decoded PCM via pacat)
    let sink_out = pactl_cmd()
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={MIC_SINK_NAME}"),
            &format!("rate={MIC_RATE}"),
            "channels=1",
            "channel_map=front-left",
            "format=s16le",
            "sink_properties=device.description=\"Screx\\ Mic\\ (internal)\"",
        ])
        .output()
        .context("pactl not found")?;

    if !sink_out.status.success() {
        let stderr = String::from_utf8_lossy(&sink_out.stderr);
        anyhow::bail!("null-sink for mic failed: {}", stderr.trim());
    }

    let sink_mod: u32 = String::from_utf8_lossy(&sink_out.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    println!("[mic] internal sink loaded (module {sink_mod})");

    // 2. Virtual source (visible as microphone in GNOME/pavucontrol)
    let src_out = pactl_cmd()
        .args([
            "load-module",
            "module-null-sink",
            "media.class=Audio/Source/Virtual",
            &format!("sink_name={MIC_SOURCE_NAME}"),
            &format!("rate={MIC_RATE}"),
            "channels=1",
            "channel_map=front-left",
            "sink_properties=device.description=\"Screx\\ Microphone\"",
        ])
        .output()
        .context("pactl not found")?;

    if !src_out.status.success() {
        let _ = pactl_cmd()
            .args(["unload-module", &sink_mod.to_string()])
            .output();
        let stderr = String::from_utf8_lossy(&src_out.stderr);
        anyhow::bail!("virtual source for mic failed: {}", stderr.trim());
    }

    let src_mod: u32 = String::from_utf8_lossy(&src_out.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    println!("[mic] virtual source loaded (module {src_mod})");

    // 3. Link sink monitor → virtual source input
    std::thread::sleep(Duration::from_millis(200));
    let mut link_cmd = Command::new("pw-link");
    for (k, v) in pulse_env() {
        link_cmd.env(&k, &v);
    }
    let link_out = link_cmd
        .args([
            &format!("{MIC_SINK_NAME}:monitor_FL"),
            &format!("{MIC_SOURCE_NAME}:input_FL"),
        ])
        .output();
    if let Ok(ref r) = link_out {
        if !r.status.success() {
            let stderr = String::from_utf8_lossy(&r.stderr);
            eprintln!("[mic] pw-link warning (may still work): {}", stderr.trim());
        } else {
            println!("[mic] linked monitor → source");
        }
    }

    // 4. Spawn pacat to feed decoded PCM to the internal sink
    let mut pacat = Command::new("pacat");
    for (k, v) in pulse_env() {
        pacat.env(&k, &v);
    }
    let mut child = pacat
        .args([
            "--playback",
            &format!("--device={MIC_SINK_NAME}"),
            "--format=s16le",
            &format!("--rate={MIC_RATE}"),
            "--channels=1",
            "--latency-msec=10",
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start pacat for mic")?;

    let stdin = child.stdin.take().context("no stdin from pacat")?;
    // Non-blocking so mic writes never stall the control thread
    unsafe {
        let flags = libc::fcntl(stdin.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(stdin.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    println!("[mic] pacat started (pid {})", child.id());

    let decoder =
        OpusDecoder::new(MIC_RATE, 1).map_err(|e| anyhow::anyhow!("opus decoder init: {e:?}"))?;

    println!("[mic] virtual mic ready via null-sink + virtual-source");

    Ok(MicWriter {
        decoder,
        output: MicOutput::Pacat(stdin),
        module_ids: vec![sink_mod, src_mod],
        fifo_path: None,
        pacat_child: Some(child),
        pcm_buf: vec![0i16; MIC_MAX_FRAME],
        byte_buf: Vec::with_capacity(MIC_MAX_FRAME * 2),
    })
}

pub fn remove_virtual_mic(mic: &mut MicWriter) {
    // Close the output first
    match &mut mic.output {
        MicOutput::Fifo(_) => {} // dropped when MicWriter is dropped
        MicOutput::Pacat(_) => {}
    }

    if let Some(ref mut child) = mic.pacat_child {
        let _ = child.kill();
        let _ = child.wait();
    }

    for &mid in &mic.module_ids {
        if mid > 0 {
            let _ = pactl_cmd()
                .args(["unload-module", &mid.to_string()])
                .output();
        }
    }

    if let Some(ref path) = mic.fifo_path {
        let _ = fs::remove_file(path);
    }

    println!("[mic] virtual mic removed");
}
