use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{mpsc, watch};

use crate::capture::{BufferType, CaptureFrame, PixelFormat};

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub bitrate_bps: u32,
    pub gop: u32,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub backend: EncoderBackend,
}

#[derive(Debug, Clone)]
pub enum ControlMessage {
    RequestIdr,
    SetBitrate(u32),
    SetResolution(u32, u32),
}

#[derive(Debug, Clone)]
pub struct EncodedAccessUnit {
    pub frame_index: u64,
    pub timestamp_90k: u32,
    pub is_idr: bool,
    pub annex_b: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    Auto,
    Bootstrap,
    Vaapi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveEncoderBackend {
    Bootstrap,
    #[cfg(feature = "real-encode")]
    VaapiBootstrapPayload,
}

impl EncoderBackend {
    pub fn from_env(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "bootstrap" | "mock" | "synthetic" => Self::Bootstrap,
            "vaapi" | "real" => Self::Vaapi,
            _ => Self::Auto,
        }
    }
}

pub async fn run_encoder_loop(
    mut config: EncoderConfig,
    mut frame_rx: mpsc::Receiver<CaptureFrame>,
    au_tx: mpsc::Sender<EncodedAccessUnit>,
    mut control_rx: mpsc::Receiver<ControlMessage>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    let active_backend = initialize_encoder_backend(config.backend, &config)?;
    #[cfg(feature = "real-encode")]
    let mut vaapi_encoder = match active_backend {
        ActiveEncoderBackend::VaapiBootstrapPayload => {
            Some(vaapi::VaapiEncoderProcess::new(&config)?)
        }
        ActiveEncoderBackend::Bootstrap => None,
    };

    let mut force_next_idr = true;
    let mut encoded_in_window = 0_u64;
    let mut bytes_in_window = 0_u64;
    let mut dropped_capture_frames_in_window = 0_u64;
    let mut dropped_au_in_window = 0_u64;
    let mut window_start = Instant::now();

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    println!("[encode] stop signal received");
                    break;
                }
            }
            maybe_cmd = control_rx.recv() => {
                match maybe_cmd {
                    Some(ControlMessage::RequestIdr) => {
                        println!("[encode] control: forcing IDR on next frame");
                        force_next_idr = true;
                        #[cfg(feature = "real-encode")]
                        if let Some(worker) = vaapi_encoder.as_mut() {
                            // Restarting encoder guarantees a keyframe boundary and helps
                            // recover the receiver if parameter sets were missed.
                            *worker = vaapi::VaapiEncoderProcess::new(&config)?;
                        }
                    }
                    Some(ControlMessage::SetBitrate(bps)) => {
                        println!("[encode] control: updating bitrate {} -> {}", config.bitrate_bps, bps);
                        config.bitrate_bps = bps.max(500_000);
                        #[cfg(feature = "real-encode")]
                        if let Some(worker) = vaapi_encoder.as_mut() {
                            // ffmpeg subprocess bitrate is configured at process start.
                            *worker = vaapi::VaapiEncoderProcess::new(&config)?;
                        }
                    }
                    Some(ControlMessage::SetResolution(w, h)) => {
                        println!("[encode] control: target resolution {}x{} -> {}x{}", config.width, config.height, w, h);
                        config.width = w.max(320);
                        config.height = h.max(180);
                        force_next_idr = true;
                        #[cfg(feature = "real-encode")]
                        if let Some(worker) = vaapi_encoder.as_mut() {
                            // Reinitialize ffmpeg process so rawvideo geometry matches configured dimensions.
                            *worker = vaapi::VaapiEncoderProcess::new(&config)?;
                        }
                    }
                    None => {}
                }
            }
            maybe_frame = frame_rx.recv() => {
                let Some(mut frame) = maybe_frame else {
                    println!("[encode] upstream channel closed");
                    break;
                };
                while let Ok(newer_frame) = frame_rx.try_recv() {
                    frame = newer_frame;
                    dropped_capture_frames_in_window += 1;
                }

                if frame.width != config.width || frame.height != config.height {
                    println!(
                        "[encode] capture geometry change detected: {}x{} -> {}x{}; reconfiguring encoder",
                        config.width, config.height, frame.width, frame.height
                    );
                    config.width = frame.width.max(1);
                    config.height = frame.height.max(1);
                    force_next_idr = true;
                    #[cfg(feature = "real-encode")]
                    if let Some(worker) = vaapi_encoder.as_mut() {
                        *worker = vaapi::VaapiEncoderProcess::new(&config)?;
                    }
                }

                let is_idr = force_next_idr || frame.frame_index % u64::from(config.gop.max(1)) == 0;
                let mut produced_aus = Vec::new();
                #[cfg(feature = "real-encode")]
                {
                    if let Some(worker) = vaapi_encoder.as_mut() {
                        produced_aus = worker.push_frame(&config, &frame, is_idr)?;
                    }
                }
                #[cfg(feature = "real-encode")]
                let has_worker = vaapi_encoder.is_some();
                #[cfg(not(feature = "real-encode"))]
                let has_worker = false;
                if produced_aus.is_empty() && !has_worker {
                    produced_aus.push(encode_frame(active_backend, &frame, is_idr));
                }
                if produced_aus.is_empty() {
                    continue;
                }
                if produced_aus.len() > 1 {
                    dropped_au_in_window += (produced_aus.len() - 1) as u64;
                    let newest = produced_aus.pop().expect("len checked");
                    produced_aus.clear();
                    produced_aus.push(newest);
                }
                force_next_idr = false;

                for au in produced_aus {
                    bytes_in_window += au.annex_b.len() as u64;
                    encoded_in_window += 1;
                    if window_start.elapsed() >= Duration::from_secs(1) {
                        let elapsed = window_start.elapsed().as_secs_f64();
                        let fps = encoded_in_window as f64 / elapsed;
                        let mbps = (bytes_in_window as f64 * 8.0 / elapsed) / 1_000_000.0;
                        println!(
                            "[encode] fps={fps:.1} stream_mbps={mbps:.2} target_bitrate_bps={} dropped_capture_frames_per_sec={} dropped_au_per_sec={}",
                            config.bitrate_bps, dropped_capture_frames_in_window, dropped_au_in_window
                        );
                        encoded_in_window = 0;
                        bytes_in_window = 0;
                        dropped_capture_frames_in_window = 0;
                        dropped_au_in_window = 0;
                        window_start = Instant::now();
                    }

                    match au_tx.try_send(au) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            // Low-latency policy: drop stale AU instead of waiting and building delay.
                            dropped_au_in_window += 1;
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            println!("[encode] downstream channel closed");
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn encode_frame(
    active_backend: ActiveEncoderBackend,
    frame: &CaptureFrame,
    is_idr: bool,
) -> EncodedAccessUnit {
    match active_backend {
        ActiveEncoderBackend::Bootstrap => encode_frame_bootstrap(frame, is_idr),
        #[cfg(feature = "real-encode")]
        ActiveEncoderBackend::VaapiBootstrapPayload => encode_frame_bootstrap(frame, is_idr),
    }
}

fn encode_frame_bootstrap(frame: &CaptureFrame, is_idr: bool) -> EncodedAccessUnit {
    let mut annex_b = Vec::with_capacity(256);
    if is_idr {
        annex_b.extend_from_slice(&nal_with_start_code(32, &[0x01, 0x02, 0x03, 0x04])); // VPS
        annex_b.extend_from_slice(&nal_with_start_code(33, &[0x11, 0x22, 0x33, 0x44])); // SPS
        annex_b.extend_from_slice(&nal_with_start_code(34, &[0x55, 0x66, 0x77])); // PPS
        annex_b.extend_from_slice(&nal_with_start_code(
            19,
            build_slice_payload(frame, 96).as_slice(),
        )); // IDR
    } else {
        annex_b.extend_from_slice(&nal_with_start_code(
            1,
            build_slice_payload(frame, 96).as_slice(),
        )); // non-IDR
    }

    EncodedAccessUnit {
        frame_index: frame.frame_index,
        timestamp_90k: frame.timestamp_90k,
        is_idr,
        annex_b,
    }
}

fn nal_with_start_code(nal_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 6);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(&hevc_nal_header(nal_type));
    out.extend_from_slice(payload);
    out
}

fn initialize_encoder_backend(
    backend: EncoderBackend,
    config: &EncoderConfig,
) -> Result<ActiveEncoderBackend> {
    match backend {
        EncoderBackend::Bootstrap => {
            println!(
                "[encode] backend=bootstrap codec=hevc_vaapi profile=low-latency-cbr bitrate={} gop={} fps={} bf=0",
                config.bitrate_bps, config.gop, config.fps
            );
            Ok(ActiveEncoderBackend::Bootstrap)
        }
        EncoderBackend::Auto => {
            #[cfg(feature = "real-encode")]
            {
                if vaapi::initialize(config).is_ok() {
                    println!(
                        "[encode] backend=vaapi(auto) device=/dev/dri/renderD128 codec=hevc_vaapi"
                    );
                    return Ok(ActiveEncoderBackend::VaapiBootstrapPayload);
                }
            }
            println!("[encode] backend=bootstrap(auto-fallback)");
            Ok(ActiveEncoderBackend::Bootstrap)
        }
        EncoderBackend::Vaapi => {
            #[cfg(feature = "real-encode")]
            {
                vaapi::initialize(config)?;
                println!("[encode] backend=vaapi device=/dev/dri/renderD128 codec=hevc_vaapi");
                return Ok(ActiveEncoderBackend::VaapiBootstrapPayload);
            }
            #[cfg(not(feature = "real-encode"))]
            {
                anyhow::bail!("requested vaapi backend but daemon was built without `real-encode`");
            }
        }
    }
}

#[cfg(feature = "real-encode")]
mod vaapi {
    use std::collections::VecDeque;
    use std::fs;
    use std::io::{Read, Write};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc as std_mpsc;
    use std::thread;

    use anyhow::{bail, Context, Result};
    use ffmpeg_next as ffmpeg;

    use super::{EncodedAccessUnit, EncoderConfig};
    use crate::capture::CaptureFrame;

    pub(super) fn initialize(config: &EncoderConfig) -> Result<()> {
        ffmpeg::init().context("ffmpeg init failed")?;
        let encoder = ffmpeg::codec::encoder::find_by_name("hevc_vaapi")
            .context("hevc_vaapi encoder unavailable")?;
        if !encoder.is_encoder() {
            bail!("hevc_vaapi codec is not flagged as encoder");
        }

        let render_node = "/dev/dri/renderD128";
        let meta = fs::metadata(render_node)
            .with_context(|| format!("VA-API render node missing: {render_node}"))?;
        if meta.permissions().readonly() {
            bail!("VA-API render node is not writable: {render_node}");
        }

        let _ = ffmpeg::format::Pixel::NV12;
        println!(
            "[encode] vaapi init target={}x{}@{} bitrate={}",
            config.width, config.height, config.fps, config.bitrate_bps
        );
        Ok(())
    }

    pub(super) struct VaapiEncoderProcess {
        child: Child,
        stdin: ChildStdin,
        chunk_rx: std_mpsc::Receiver<Vec<u8>>,
        parser: AnnexBAccessUnitParser,
        pending_meta: VecDeque<FrameMeta>,
    }

    #[derive(Debug, Clone, Copy)]
    struct FrameMeta {
        frame_index: u64,
        timestamp_90k: u32,
        is_idr: bool,
    }

    impl VaapiEncoderProcess {
        pub(super) fn new(config: &EncoderConfig) -> Result<Self> {
            let mut cmd = Command::new("ffmpeg");
            cmd.args(["-hide_banner", "-loglevel", "error"])
                .args(["-vaapi_device", "/dev/dri/renderD128"])
                .args(["-f", "rawvideo"])
                .args(["-pix_fmt", "bgr0"])
                .args([
                    "-video_size",
                    &format!("{}x{}", config.width, config.height),
                ])
                .args(["-framerate", &config.fps.to_string()])
                .args(["-i", "-"])
                .args(["-vf", "format=nv12,hwupload"])
                .args(["-c:v", "hevc_vaapi"])
                .args(["-async_depth", "8"])
                .args(["-rc_mode", "CBR"])
                .args(["-b:v", &config.bitrate_bps.to_string()])
                .args(["-maxrate", &config.bitrate_bps.to_string()])
                .args(["-bufsize", &(config.bitrate_bps / 2).max(1).to_string()])
                .args(["-g", &config.gop.to_string()])
                .args(["-idr_interval", &config.gop.to_string()])
                .args(["-bf", "0"])
                .args(["-aud", "1"])
                .args(["-bsf:v", "hevc_mp4toannexb,dump_extra=freq=keyframe"])
                .args(["-f", "hevc"])
                .arg("-")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());

            let mut child = cmd
                .spawn()
                .context("failed to spawn ffmpeg vaapi encoder")?;
            let stdin = child.stdin.take().context("failed to open ffmpeg stdin")?;
            let mut stdout = child
                .stdout
                .take()
                .context("failed to open ffmpeg stdout")?;
            let (tx, rx) = std_mpsc::channel::<Vec<u8>>();
            thread::spawn(move || {
                let mut buf = [0_u8; 16 * 1024];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) => break,
                        Ok(size) => {
                            if tx.send(buf[..size].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });

            Ok(Self {
                child,
                stdin,
                chunk_rx: rx,
                parser: AnnexBAccessUnitParser::default(),
                pending_meta: VecDeque::new(),
            })
        }

        pub(super) fn push_frame(
            &mut self,
            config: &EncoderConfig,
            frame: &CaptureFrame,
            is_idr: bool,
        ) -> Result<Vec<EncodedAccessUnit>> {
            let expected_size = (config.width as usize) * (config.height as usize) * 4;
            if frame.data.len() >= expected_size {
                self.stdin
                    .write_all(&frame.data[..expected_size])
                    .context("failed to feed frame into ffmpeg encoder")?;
            } else {
                let mut padded = vec![0_u8; expected_size];
                padded[..frame.data.len()].copy_from_slice(&frame.data);
                self.stdin
                    .write_all(&padded)
                    .context("failed to feed padded frame into ffmpeg encoder")?;
            }
            self.pending_meta.push_back(FrameMeta {
                frame_index: frame.frame_index,
                timestamp_90k: frame.timestamp_90k,
                is_idr,
            });

            for chunk in self.chunk_rx.try_iter() {
                self.parser.push(&chunk);
            }

            let mut output = Vec::new();
            for au_data in self.parser.take_access_units() {
                let meta = self.pending_meta.pop_front().unwrap_or(FrameMeta {
                    frame_index: frame.frame_index,
                    timestamp_90k: frame.timestamp_90k,
                    is_idr,
                });
                output.push(EncodedAccessUnit {
                    frame_index: meta.frame_index,
                    timestamp_90k: meta.timestamp_90k,
                    is_idr: meta.is_idr,
                    annex_b: au_data,
                });
            }
            Ok(output)
        }
    }

    impl Drop for VaapiEncoderProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
        }
    }

    #[derive(Default)]
    struct AnnexBAccessUnitParser {
        buffer: Vec<u8>,
        current_au: Vec<Vec<u8>>,
        cached_vps: Option<Vec<u8>>,
        cached_sps: Option<Vec<u8>>,
        cached_pps: Option<Vec<u8>>,
    }

    impl AnnexBAccessUnitParser {
        fn push(&mut self, chunk: &[u8]) {
            self.buffer.extend_from_slice(chunk);
        }

        fn take_access_units(&mut self) -> Vec<Vec<u8>> {
            let mut completed = Vec::new();
            for nalu in self.take_complete_nalus() {
                let nal_type = hevc_nalu_type(&nalu);
                match nal_type {
                    32 => self.cached_vps = Some(nalu.clone()),
                    33 => self.cached_sps = Some(nalu.clone()),
                    34 => self.cached_pps = Some(nalu.clone()),
                    _ => {}
                }

                if nal_type == 35 && !self.current_au.is_empty() {
                    completed.push(self.finish_current_au());
                }
                self.current_au.push(nalu);
            }
            completed
        }

        fn finish_current_au(&mut self) -> Vec<u8> {
            let mut contains_irap = false;
            let mut contains_vps = false;
            let mut contains_sps = false;
            let mut contains_pps = false;
            for nalu in &self.current_au {
                match hevc_nalu_type(nalu) {
                    16..=23 => contains_irap = true,
                    32 => contains_vps = true,
                    33 => contains_sps = true,
                    34 => contains_pps = true,
                    _ => {}
                }
            }

            let mut output = Vec::new();
            if contains_irap {
                if !contains_vps {
                    if let Some(vps) = &self.cached_vps {
                        output.extend_from_slice(vps);
                    }
                }
                if !contains_sps {
                    if let Some(sps) = &self.cached_sps {
                        output.extend_from_slice(sps);
                    }
                }
                if !contains_pps {
                    if let Some(pps) = &self.cached_pps {
                        output.extend_from_slice(pps);
                    }
                }
            }
            for nalu in self.current_au.drain(..) {
                output.extend_from_slice(&nalu);
            }
            output
        }

        fn take_complete_nalus(&mut self) -> Vec<Vec<u8>> {
            let starts = find_start_codes(&self.buffer);
            if starts.len() < 2 {
                return Vec::new();
            }

            let mut out = Vec::new();
            for index in 0..starts.len() - 1 {
                let start = starts[index];
                let end = starts[index + 1];
                out.push(self.buffer[start..end].to_vec());
            }

            let keep_from = *starts.last().expect("len >= 2");
            self.buffer.drain(0..keep_from);
            out
        }
    }

    fn find_start_codes(data: &[u8]) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut i = 0_usize;
        while i + 3 < data.len() {
            if i + 4 <= data.len()
                && data[i] == 0
                && data[i + 1] == 0
                && data[i + 2] == 0
                && data[i + 3] == 1
            {
                starts.push(i);
                i += 4;
                continue;
            }
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                starts.push(i);
                i += 3;
                continue;
            }
            i += 1;
        }
        starts
    }

    fn hevc_nalu_type(nalu: &[u8]) -> u8 {
        let header_idx = if nalu.starts_with(&[0, 0, 0, 1]) {
            4
        } else if nalu.starts_with(&[0, 0, 1]) {
            3
        } else {
            0
        };
        if nalu.len() <= header_idx {
            return 0;
        }
        (nalu[header_idx] >> 1) & 0x3F
    }
}

fn hevc_nal_header(nal_type: u8) -> [u8; 2] {
    // F=0, LayerId=0, TID=1
    let b0 = (nal_type & 0x3F) << 1;
    let b1 = 0x01;
    [b0, b1]
}

fn build_slice_payload(frame: &CaptureFrame, desired_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(desired_len);
    out.extend_from_slice(&(frame.width as u16).to_be_bytes());
    out.extend_from_slice(&(frame.height as u16).to_be_bytes());
    out.extend_from_slice(&(frame.frame_index as u32).to_be_bytes());
    let format_id = match frame.format {
        PixelFormat::Bgra8888 => 1_u8,
    };
    let buffer_id = match frame.buffer_type {
        BufferType::DmaBuf => 1_u8,
        BufferType::Shm => 2_u8,
    };
    out.push(format_id);
    out.push(buffer_id);
    out.extend_from_slice(&frame.captured_at.elapsed().as_micros().to_be_bytes());

    let body_len = desired_len.saturating_sub(out.len());
    if body_len > 0 {
        let src = &frame.data;
        for idx in 0..body_len {
            out.push(src[idx % src.len()]);
        }
    }
    out
}
