use std::io::{Read, Write};
use std::net::UdpSocket;
use std::process::{Child, Command, Stdio};
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
    let needle = format!("sink_name={SINK_NAME}");
    for line in output.lines() {
        if let Some(pos) = line.find(&needle) {
            let after = pos + needle.len();
            let next_char = line[after..].chars().next();
            // Only match if the sink name is followed by a space, tab, or end-of-line
            // (avoids "screx_ipad" matching "screx_ipad_mic")
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
// Virtual mic source — receives s16le/48kHz/mono PCM from iPad, exposes as
// a PipeWire source (microphone) via pw-loopback.
//
// pw-loopback creates two nodes:
//   capture side  → media.class=Audio/Sink   (pacat writes PCM here)
//   playback side → media.class=Audio/Source  (apps record from here)
// ---------------------------------------------------------------------------

const MIC_SINK_NODE: &str = "screx_mic_sink";
const MIC_SOURCE_NODE: &str = "screx_ipad_mic";
const MIC_RATE: u32 = 48000;
const MIC_CHANNELS: u16 = 1;

pub struct MicWriter {
    loopback: Child,
    pacat: Child,
    stdin: std::process::ChildStdin,
}

impl MicWriter {
    pub fn write_pcm(&mut self, data: &[u8]) {
        let _ = self.stdin.write_all(data);
    }
}

impl Drop for MicWriter {
    fn drop(&mut self) {
        let _ = self.pacat.kill();
        let _ = self.pacat.wait();
        let _ = self.loopback.kill();
        let _ = self.loopback.wait();
        println!("[mic] pw-loopback + pacat stopped");
    }
}

pub fn create_virtual_mic_source() -> Result<MicWriter> {
    let mut loopback = Command::new("pw-loopback");
    for (k, v) in pulse_env() {
        loopback.env(&k, &v);
    }
    let loopback_child = loopback
        .args([
            "--channel-map", "[MONO]",
            "--capture-props",
            &format!(
                "media.class=Audio/Sink node.name={MIC_SINK_NODE} node.description=\"Screx Mic Internal\""
            ),
            "--playback-props",
            &format!(
                "media.class=Audio/Source node.name={MIC_SOURCE_NODE} node.description=\"Screx iPad Mic\""
            ),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start pw-loopback (is pipewire-utils installed?)")?;

    println!("[mic] pw-loopback started (pid {}), waiting for sink to appear...", loopback_child.id());

    // Give PipeWire a moment to register the nodes
    std::thread::sleep(std::time::Duration::from_millis(500));

    let mut pacat = Command::new("pacat");
    for (k, v) in pulse_env() {
        pacat.env(&k, &v);
    }
    let mut pacat_child = pacat
        .args([
            "--playback",
            &format!("--device={MIC_SINK_NODE}"),
            "--format=s16le",
            &format!("--rate={MIC_RATE}"),
            &format!("--channels={MIC_CHANNELS}"),
            "--latency-msec=10",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start pacat for mic feed")?;

    let stdin = pacat_child.stdin.take().context("no stdin from pacat")?;

    println!(
        "[mic] virtual mic ready: pw-loopback pid={}, pacat pid={}, sink={MIC_SINK_NODE}, source={MIC_SOURCE_NODE}",
        loopback_child.id(),
        pacat_child.id(),
    );

    Ok(MicWriter {
        loopback: loopback_child,
        pacat: pacat_child,
        stdin,
    })
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
