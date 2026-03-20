use std::net::UdpSocket;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

use crate::stream_server::{AudioSender, SharedState};

const SINK_NAME: &str = "screx_ipad";
const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const CHUNK_DURATION_MS: u32 = 10;
const SAMPLES_PER_CHUNK: usize = (SAMPLE_RATE / 1000 * CHUNK_DURATION_MS) as usize;
const BYTES_PER_CHUNK: usize = SAMPLES_PER_CHUNK * CHANNELS as usize * 2; // i16 stereo

pub fn create_virtual_sink() -> Result<u32> {
    let existing = Command::new("pactl")
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

    let result = Command::new("pactl")
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
    let _ = Command::new("pactl")
        .args(["unload-module", &module_id.to_string()])
        .output();
    println!("[audio] removed virtual sink (module {module_id})");
}

#[allow(deprecated)]
fn find_monitor_device() -> Result<cpal::Device> {
    let host = cpal::default_host();
    let monitor_name = format!("{SINK_NAME}.monitor");

    for device in host.input_devices().context("no input devices")? {
        if let Ok(name) = device.name() {
            if name == monitor_name {
                println!("[audio] found monitor source: {name}");
                return Ok(device);
            }
        }
    }

    anyhow::bail!(
        "monitor source '{monitor_name}' not found. \
         Make sure PulseAudio/PipeWire is running and the virtual sink was created."
    )
}

pub fn run_audio_capture(
    socket: UdpSocket,
    shared: Arc<SharedState>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let device = find_monitor_device()?;

    let config = cpal::StreamConfig {
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE,
        buffer_size: cpal::BufferSize::Default,
    };

    let supported = device
        .supported_input_configs()
        .context("failed to query input configs")?;
    let mut found_i16 = false;
    for cfg in supported {
        if cfg.sample_format() == SampleFormat::I16
            && cfg.channels() == CHANNELS
            && cfg.min_sample_rate() <= SAMPLE_RATE
            && cfg.max_sample_rate() >= SAMPLE_RATE
        {
            found_i16 = true;
            break;
        }
    }
    if !found_i16 {
        eprintln!("[audio] warning: i16 format not directly supported, will attempt anyway");
    }

    let mut sender = AudioSender::new(socket);
    let mut pcm_buf: Vec<u8> = Vec::with_capacity(BYTES_PER_CHUNK * 2);
    let shared_clone = Arc::clone(&shared);

    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);

    let stream = device
        .build_input_stream(
            &config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let bytes: &[u8] = unsafe {
                    std::slice::from_raw_parts(
                        data.as_ptr() as *const u8,
                        data.len() * std::mem::size_of::<i16>(),
                    )
                };
                let _ = tx.try_send(bytes.to_vec());
            },
            |err| {
                eprintln!("[audio] capture error: {err}");
            },
            None,
        )
        .context("failed to build audio input stream")?;

    stream.play().context("failed to start audio capture")?;
    println!("[audio] capturing from {SINK_NAME}.monitor at {SAMPLE_RATE}Hz stereo");

    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(data) => {
                pcm_buf.extend_from_slice(&data);

                while pcm_buf.len() >= BYTES_PER_CHUNK {
                    let chunk: Vec<u8> = pcm_buf.drain(..BYTES_PER_CHUNK).collect();
                    let client_addr = *shared_clone.client_addr.lock().unwrap();
                    if let Some(addr) = client_addr {
                        if let Err(e) = sender.send_audio(&chunk, addr) {
                            eprintln!("[audio] send error: {e}");
                        }
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }

    drop(stream);
    println!("[audio] capture stopped");
    Ok(())
}
