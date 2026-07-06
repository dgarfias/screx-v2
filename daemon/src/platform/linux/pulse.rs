use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::audio::{AudioBackend, MicSink};

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

fn pulse_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    if std::env::var("PULSE_SERVER").is_ok() {
        return env;
    }
    if let Ok(uid) = std::env::var("SCREX_PULSE_UID").or_else(|_| std::env::var("SUDO_UID")) {
        let runtime_dir = format!("/run/user/{uid}");
        env.push(("XDG_RUNTIME_DIR".into(), runtime_dir.clone()));
        env.push((
            "PULSE_SERVER".into(),
            format!("unix:{runtime_dir}/pulse/native"),
        ));
        return env;
    }
    if unsafe { libc::getuid() } == 0 {
        if let Ok(entries) = std::fs::read_dir("/run/user") {
            for entry in entries.flatten() {
                let pulse_path = entry.path().join("pulse/native");
                if pulse_path.exists() {
                    let runtime_dir = entry.path().to_string_lossy().to_string();
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

pub struct PulseAudioBackend {
    sink_module_id: u32,
}

impl PulseAudioBackend {
    pub fn new() -> Self {
        Self { sink_module_id: 0 }
    }
}

impl AudioBackend for PulseAudioBackend {
    fn create_sink(&mut self) -> Result<()> {
        if self.sink_module_id > 0 {
            return Ok(());
        }

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
                    self.sink_module_id = module_id;
                    return Ok(());
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

        self.sink_module_id = String::from_utf8_lossy(&result.stdout)
            .trim()
            .parse()
            .unwrap_or(0);
        println!(
            "[audio] created virtual sink '{SINK_NAME}' (module {})",
            self.sink_module_id
        );
        Ok(())
    }

    fn destroy_sink(&mut self) {
        if self.sink_module_id > 0 {
            let _ = pactl_cmd()
                .args(["unload-module", &self.sink_module_id.to_string()])
                .output();
            println!(
                "[audio] removed virtual sink (module {})",
                self.sink_module_id
            );
            self.sink_module_id = 0;
        }
    }

    fn run_loopback(&mut self, stop: &AtomicBool, on_chunk: &mut dyn FnMut(&[u8])) -> Result<()> {
        let monitor_source = format!("{SINK_NAME}.monitor");
        let mut buf = vec![0u8; BYTES_PER_CHUNK];

        while !stop.load(Ordering::Relaxed) {
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
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };

            let mut stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    continue;
                }
            };

            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match stdout.read_exact(&mut buf) {
                    Ok(()) => on_chunk(&buf),
                    Err(_) => break,
                }
            }

            let _ = child.kill();
            let _ = child.wait();
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    }

    fn create_mic(&mut self) -> Result<Box<dyn MicSink>> {
        let sink = try_pipe_source().or_else(|e| {
            println!("[mic] pipe-source failed ({e:#}), trying null-sink fallback...");
            try_null_sink_mic()
        })?;
        Ok(Box::new(sink))
    }

    fn destroy_mic(&mut self) {
        // Sinks are cleaned up when the Box<dyn MicSink> is dropped.
    }
}

struct PulseMicSink {
    output: MicOutput,
    module_ids: Vec<u32>,
    fifo_path: Option<String>,
    pacat_child: Option<Child>,
}

enum MicOutput {
    Fifo(File),
    Pacat(ChildStdin),
}

impl MicSink for PulseMicSink {
    fn write_pcm(&mut self, s16_mono_48k: &[i16]) {
        let mut bytes = Vec::with_capacity(s16_mono_48k.len() * 2);
        for &s in s16_mono_48k {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let _ = match &mut self.output {
            MicOutput::Fifo(f) => f.write(&bytes),
            MicOutput::Pacat(stdin) => stdin.write(&bytes),
        };
    }
}

impl Drop for PulseMicSink {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.pacat_child {
            let _ = child.kill();
            let _ = child.wait();
        }
        for &mid in &self.module_ids {
            if mid > 0 {
                let _ = pactl_cmd()
                    .args(["unload-module", &mid.to_string()])
                    .output();
            }
        }
        if let Some(ref path) = self.fifo_path {
            let _ = fs::remove_file(path);
        }
        println!("[mic] virtual mic removed");
    }
}

fn try_pipe_source() -> Result<PulseMicSink> {
    let _ = fs::remove_file(MIC_FIFO_PATH);
    let status = Command::new("mkfifo")
        .arg(MIC_FIFO_PATH)
        .status()
        .context("mkfifo not found")?;
    if !status.success() {
        anyhow::bail!("mkfifo failed");
    }
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

    println!("[mic] virtual mic ready via pipe-source (FIFO)");
    Ok(PulseMicSink {
        output: MicOutput::Fifo(fifo.unwrap()),
        module_ids: vec![module_id],
        fifo_path: Some(MIC_FIFO_PATH.to_string()),
        pacat_child: None,
    })
}

fn try_null_sink_mic() -> Result<PulseMicSink> {
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

    std::thread::sleep(Duration::from_millis(200));
    let mut link_cmd = Command::new("pw-link");
    for (k, v) in pulse_env() {
        link_cmd.env(&k, &v);
    }
    let _ = link_cmd
        .args([
            &format!("{MIC_SINK_NAME}:monitor_FL"),
            &format!("{MIC_SOURCE_NAME}:input_FL"),
        ])
        .output();

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
    unsafe {
        let flags = libc::fcntl(stdin.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(stdin.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    println!("[mic] pacat started (pid {})", child.id());

    Ok(PulseMicSink {
        output: MicOutput::Pacat(stdin),
        module_ids: vec![sink_mod, src_mod],
        fifo_path: None,
        pacat_child: Some(child),
    })
}
