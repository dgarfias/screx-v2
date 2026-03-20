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

pub const MIC_MAGIC: &[u8] = b"MIC";
pub const MIC_HEADER_LEN: usize = 13;
pub const MIC_PROTOCOL_VERSION: u8 = 1;
pub const MIC_CODEC_PCM16LE: u8 = 1;
pub const MIC_SAMPLE_RATE: u16 = 48_000;
pub const MIC_CHANNELS: u8 = 1;
pub const MIC_SAMPLES_PER_FRAME: u16 = 480;
pub const MIC_BYTES_PER_FRAME: usize = MIC_SAMPLES_PER_FRAME as usize * 2;

const MIC_SINK_NODE: &str = "screx_mic_sink";
const MIC_SOURCE_NODE: &str = "screx_ipad_mic";

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

pub struct ParsedMicPacket<'a> {
    pub seq: u32,
    pub pcm: &'a [u8],
}

pub fn parse_mic_packet(packet: &[u8]) -> Result<ParsedMicPacket<'_>> {
    let min_len = MIC_MAGIC.len() + MIC_HEADER_LEN;
    if packet.len() < min_len {
        anyhow::bail!("short packet: {} bytes", packet.len());
    }
    if &packet[..MIC_MAGIC.len()] != MIC_MAGIC {
        anyhow::bail!("invalid packet magic");
    }

    let h = &packet[MIC_MAGIC.len()..(MIC_MAGIC.len() + MIC_HEADER_LEN)];
    let version = h[0];
    let codec = h[1];
    let sample_rate = u16::from_be_bytes([h[2], h[3]]);
    let channels = h[4];
    let samples_per_frame = u16::from_be_bytes([h[5], h[6]]);
    let payload_len = u16::from_be_bytes([h[7], h[8]]) as usize;
    let seq = u32::from_be_bytes([h[9], h[10], h[11], h[12]]);
    let payload = &packet[min_len..];

    if version != MIC_PROTOCOL_VERSION {
        anyhow::bail!("unsupported protocol version: {version}");
    }
    if codec != MIC_CODEC_PCM16LE {
        anyhow::bail!("unsupported mic codec: {codec}");
    }
    if sample_rate != MIC_SAMPLE_RATE {
        anyhow::bail!("unexpected sample rate: {sample_rate} (expected {MIC_SAMPLE_RATE})");
    }
    if channels != MIC_CHANNELS {
        anyhow::bail!("unexpected channels: {channels} (expected {MIC_CHANNELS})");
    }
    if samples_per_frame != MIC_SAMPLES_PER_FRAME {
        anyhow::bail!(
            "unexpected samples/frame: {samples_per_frame} (expected {MIC_SAMPLES_PER_FRAME})"
        );
    }
    if payload_len != payload.len() {
        anyhow::bail!("payload length mismatch: header={payload_len}, actual={}", payload.len());
    }
    if payload_len != MIC_BYTES_PER_FRAME {
        anyhow::bail!(
            "unexpected payload bytes/frame: {payload_len} (expected {MIC_BYTES_PER_FRAME})"
        );
    }
    if payload_len % 2 != 0 {
        anyhow::bail!("invalid PCM payload length (must be multiple of 2): {payload_len}");
    }

    Ok(ParsedMicPacket { seq, pcm: payload })
}

pub struct MicWriter {
    loopback: Child,
    feeder: Child,
    stdin: std::process::ChildStdin,
}

impl MicWriter {
    pub fn write_pcm(&mut self, data: &[u8]) {
        let _ = self.stdin.write_all(data);
    }
}

impl Drop for MicWriter {
    fn drop(&mut self) {
        let _ = self.feeder.kill();
        let _ = self.feeder.wait();
        let _ = self.loopback.kill();
        let _ = self.loopback.wait();
        println!("[mic] loopback and feeder stopped");
    }
}

pub fn create_virtual_mic_source() -> Result<MicWriter> {
    let mut loopback = Command::new("pw-loopback");
    for (k, v) in pulse_env() {
        loopback.env(&k, &v);
    }
    let mut loopback_child = loopback
        .args([
            "--capture-props",
            &format!(
                "node.name={MIC_SINK_NODE} media.class=Audio/Sink audio.position=[MONO] audio.rate={MIC_SAMPLE_RATE}"
            ),
            "--playback-props",
            &format!(
                "node.name={MIC_SOURCE_NODE} node.description=Screx_iPad_Mic media.class=Audio/Source audio.position=[MONO] audio.rate={MIC_SAMPLE_RATE}"
            ),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start pw-loopback (is pipewire-utils installed?)")?;

    // Give PipeWire a moment to register nodes before feeder starts.
    std::thread::sleep(std::time::Duration::from_millis(400));

    let mut pacat = Command::new("pacat");
    for (k, v) in pulse_env() {
        pacat.env(&k, &v);
    }
    let mut feeder = match pacat
        .args([
            "--playback",
            &format!("--device={MIC_SINK_NODE}"),
            "--format=s16le",
            &format!("--rate={MIC_SAMPLE_RATE}"),
            &format!("--channels={MIC_CHANNELS}"),
            "--latency-msec=10",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let _ = loopback_child.kill();
            let _ = loopback_child.wait();
            return Err(anyhow::Error::new(e).context("failed to start pacat mic feeder"));
        }
    };

    let stdin = feeder
        .stdin
        .take()
        .context("no stdin from mic feeder process")?;

    println!(
        "[mic] virtual mic ready: source={MIC_SOURCE_NODE}, sink={MIC_SINK_NODE}, loopback pid={}, feeder pid={}",
        loopback_child.id(),
        feeder.id()
    );

    Ok(MicWriter {
        loopback: loopback_child,
        feeder,
        stdin,
    })
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
