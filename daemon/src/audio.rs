use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::UdpSocket;
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::stream_server::{AudioSender, SharedState};

const SINK_NAME: &str = "screx_ipad";
const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const CHUNK_DURATION_MS: u32 = 10;
const SAMPLES_PER_CHUNK: usize = (SAMPLE_RATE / 1000 * CHUNK_DURATION_MS) as usize;
const BYTES_PER_CHUNK: usize = SAMPLES_PER_CHUNK * CHANNELS as usize * 2;

fn pulse_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Ok(uid) = std::env::var("SUDO_UID") {
        let runtime_dir = format!("/run/user/{uid}");
        env.push(("XDG_RUNTIME_DIR".into(), runtime_dir.clone()));
        env.push((
            "PULSE_SERVER".into(),
            format!("unix:{runtime_dir}/pulse/native"),
        ));
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

pub fn create_virtual_sink() -> Result<u32> {
    let existing = pactl_cmd()
        .args(["list", "short", "modules"])
        .output()
        .context("pactl not found")?;

    let output = String::from_utf8_lossy(&existing.stdout);
    for line in output.lines() {
        if line.contains(SINK_NAME) {
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
// Virtual mic source — receives PCM from iPad mic, exposes as PulseAudio source
// ---------------------------------------------------------------------------

const MIC_SOURCE_NAME: &str = "screx_ipad_mic";
const MIC_FIFO_PATH: &str = "/tmp/screx_mic";
const MIC_RATE: u32 = 48000;
const MIC_CHANNELS: u16 = 1;

pub struct MicWriter {
    file: std::fs::File,
}

impl MicWriter {
    pub fn write_pcm(&mut self, data: &[u8]) {
        let _ = self.file.write_all(data);
    }
}

pub fn create_virtual_mic_source() -> Result<(u32, MicWriter)> {
    // Check if already loaded
    let existing = pactl_cmd()
        .args(["list", "short", "modules"])
        .output()
        .context("pactl not found")?;

    let output = String::from_utf8_lossy(&existing.stdout);
    for line in output.lines() {
        if line.contains(MIC_SOURCE_NAME) {
            let module_id: u32 = line
                .split_whitespace()
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            pactl_cmd()
                .args(["unload-module", &module_id.to_string()])
                .output()
                .ok();
            println!("[mic] removed stale mic source (module {module_id})");
        }
    }

    // Remove stale FIFO
    let _ = fs::remove_file(MIC_FIFO_PATH);

    // Create FIFO
    let path = std::ffi::CString::new(MIC_FIFO_PATH).unwrap();
    let ret = unsafe { libc::mkfifo(path.as_ptr(), 0o666) };
    if ret != 0 {
        anyhow::bail!(
            "mkfifo({MIC_FIFO_PATH}) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    // Load module-pipe-source (reads from the FIFO)
    let result = pactl_cmd()
        .args([
            "load-module",
            "module-pipe-source",
            &format!("source_name={MIC_SOURCE_NAME}"),
            &format!("source_properties=device.description=\"Screx\\ iPad\\ Mic\""),
            &format!("file={MIC_FIFO_PATH}"),
            &format!("rate={MIC_RATE}"),
            &format!("channels={MIC_CHANNELS}"),
            "format=s16le",
        ])
        .output()
        .context("failed to load module-pipe-source")?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let _ = fs::remove_file(MIC_FIFO_PATH);
        anyhow::bail!("pactl load-module module-pipe-source failed: {stderr}");
    }

    let module_id: u32 = String::from_utf8_lossy(&result.stdout)
        .trim()
        .parse()
        .unwrap_or(0);

    // Open the FIFO for writing (non-blocking to avoid deadlock if nothing reads yet)
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(MIC_FIFO_PATH)
        .context("failed to open mic FIFO for writing")?;

    println!("[mic] virtual mic source created (module {module_id})");

    Ok((module_id, MicWriter { file }))
}

pub fn remove_virtual_mic_source(module_id: u32) {
    if module_id == 0 {
        return;
    }
    let _ = pactl_cmd()
        .args(["unload-module", &module_id.to_string()])
        .output();
    let _ = fs::remove_file(MIC_FIFO_PATH);
    println!("[mic] removed virtual mic source (module {module_id})");
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

    let mut parec = Command::new("parec");
    for (k, v) in pulse_env() {
        parec.env(&k, &v);
    }
    let mut child = parec
        .args([
            &format!("--device={monitor_source}"),
            "--format=s16le",
            &format!("--rate={SAMPLE_RATE}"),
            &format!("--channels={CHANNELS}"),
            "--latency-msec=10",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start parec — is pulseaudio-utils installed?")?;

    println!("[audio] capturing from {monitor_source} via parec (pid {})", child.id());

    let mut stdout = child.stdout.take().context("no stdout from parec")?;
    let mut sender = AudioSender::new(socket);
    let mut buf = vec![0u8; BYTES_PER_CHUNK];

    while !stop.load(Ordering::Relaxed) {
        match stdout.read_exact(&mut buf) {
            Ok(()) => {
                // Prefer USB if active
                if shared.usb_active.load(Ordering::Relaxed) {
                    let mut usb = shared.usb_sender.lock().unwrap();
                    if let Some(ref mut tcp) = *usb {
                        if let Err(e) = tcp.send_audio(&buf) {
                            eprintln!("[audio] USB send error: {e}");
                            drop(usb);
                            shared.usb_active.store(false, Ordering::SeqCst);
                        }
                        continue;
                    }
                }
                // Fall back to WiFi UDP
                let client_addr = *shared.client_addr.lock().unwrap();
                if let Some(addr) = client_addr {
                    if let Err(e) = sender.send_audio(&buf, addr) {
                        eprintln!("[audio] send error: {e}");
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                eprintln!("[audio] parec stream ended");
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
    println!("[audio] capture stopped");
    Ok(())
}
