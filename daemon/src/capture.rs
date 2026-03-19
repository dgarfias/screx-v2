use std::thread;
use std::time::{Duration, Instant};

#[cfg(not(feature = "real-capture"))]
use anyhow::bail;
use anyhow::Result;
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone, Copy)]
pub enum BufferType {
    DmaBuf,
    Shm,
}

#[derive(Debug, Clone, Copy)]
pub enum PixelFormat {
    Bgra8888,
}

#[derive(Debug, Clone)]
pub struct CaptureFrame {
    pub frame_index: u64,
    pub timestamp_90k: u32,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub buffer_type: BufferType,
    pub data: Vec<u8>,
    pub captured_at: Instant,
}

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub prefer_dma_buf: bool,
    pub backend: CaptureBackend,
    pub source: CaptureSource,
}

#[derive(Debug, Clone)]
struct PortalSession {
    pub session_id: String,
    pub source_type: &'static str,
    pub stream_node_id: Option<u32>,
    pub stream_size: Option<(i32, i32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBackend {
    Auto,
    Synthetic,
    PortalPipewire,
}

impl CaptureBackend {
    pub fn from_env(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "synthetic" | "mock" => Self::Synthetic,
            "portal" | "pipewire" | "portal-pipewire" | "real" => Self::PortalPipewire,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    Virtual1080p,
    Monitor,
}

impl CaptureSource {
    pub fn from_env(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "monitor" => Self::Monitor,
            _ => Self::Virtual1080p,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Virtual1080p => "Virtual",
            Self::Monitor => "Monitor",
        }
    }
}

pub fn spawn_capture_thread(
    config: CaptureConfig,
    tx: mpsc::Sender<CaptureFrame>,
    stop_rx: watch::Receiver<bool>,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || capture_loop(config, tx, stop_rx))
}

fn capture_loop(
    config: CaptureConfig,
    tx: mpsc::Sender<CaptureFrame>,
    stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    if config.backend != CaptureBackend::Synthetic {
        match run_real_capture_if_available(&config, &tx, &stop_rx) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if config.backend == CaptureBackend::PortalPipewire
                    || config.source == CaptureSource::Virtual1080p
                {
                    return Err(err.context("real capture backend requested explicitly"));
                }
                eprintln!(
                    "[capture] real capture unavailable, falling back to synthetic source: {err:#}"
                );
            }
        }
    }

    let portal = run_portal_flow_mock(config.source);
    println!(
        "[capture] using synthetic source (session id={}, source={}, node_id={:?}, size={:?})",
        portal.session_id, portal.source_type, portal.stream_node_id, portal.stream_size
    );
    run_synthetic_capture_loop(
        &config,
        tx,
        stop_rx,
        if config.prefer_dma_buf {
            BufferType::DmaBuf
        } else {
            BufferType::Shm
        },
    )
}

fn run_synthetic_capture_loop(
    config: &CaptureConfig,
    tx: mpsc::Sender<CaptureFrame>,
    stop_rx: watch::Receiver<bool>,
    buffer_type: BufferType,
) -> Result<()> {
    let frame_interval = Duration::from_micros(1_000_000 / config.fps.max(1) as u64);
    let mut frame_index = 0_u64;
    let mut stats_window_start = Instant::now();
    let mut stats_frames = 0_u64;
    let start = Instant::now();

    loop {
        if *stop_rx.borrow() {
            println!("[capture] stop signal received");
            break;
        }

        let timestamp_90k = ((frame_index * 90_000) / config.fps.max(1) as u64) as u32;
        let captured_at = Instant::now();
        let frame =
            build_synthetic_frame(config, frame_index, timestamp_90k, captured_at, buffer_type);

        if tx.blocking_send(frame).is_err() {
            println!("[capture] downstream channel closed");
            break;
        }

        frame_index += 1;
        stats_frames += 1;
        if stats_window_start.elapsed() >= Duration::from_secs(1) {
            let fps = stats_frames as f64 / stats_window_start.elapsed().as_secs_f64();
            let uptime = start.elapsed().as_secs();
            let buffer = match buffer_type {
                BufferType::DmaBuf => "DMA-BUF",
                BufferType::Shm => "SHM",
            };
            println!(
                "[capture] fps={fps:.1} resolution={}x{} format=BGRA buffer={buffer} uptime={}s",
                config.width, config.height, uptime
            );
            stats_window_start = Instant::now();
            stats_frames = 0;
        }

        thread::sleep(frame_interval);
    }

    Ok(())
}

fn run_portal_flow_mock(source: CaptureSource) -> PortalSession {
    // This mirrors the intended xdg-desktop-portal sequence:
    // CreateSession -> SelectSources(SourceType::Monitor) -> Start -> OpenPipeWireRemote
    // In this bootstrap implementation we keep the structure and logging while using
    // a synthetic frame source so integration can proceed before hardware capture wiring.
    println!("[capture] portal: CreateSession");
    thread::sleep(Duration::from_millis(20));
    println!(
        "[capture] portal: SelectSources(SourceType::{})",
        source.label()
    );
    thread::sleep(Duration::from_millis(20));
    println!("[capture] portal: Start");
    thread::sleep(Duration::from_millis(20));
    println!("[capture] portal: OpenPipeWireRemote");

    PortalSession {
        session_id: "mock-session-001".to_string(),
        source_type: source.label(),
        stream_node_id: None,
        stream_size: Some((1920, 1080)),
    }
}

fn build_synthetic_frame(
    config: &CaptureConfig,
    frame_index: u64,
    timestamp_90k: u32,
    captured_at: Instant,
    buffer_type: BufferType,
) -> CaptureFrame {
    let pixel_count = (config.width as usize) * (config.height as usize);
    let mut data = vec![0_u8; pixel_count * 4];

    // A lightweight gradient gives deterministic motion to exercise the pipeline.
    let t = (frame_index % 255) as u8;
    for px in data.chunks_exact_mut(4).step_by(97) {
        px[0] = t; // B
        px[1] = t.wrapping_add(80); // G
        px[2] = t.wrapping_add(160); // R
        px[3] = 255; // A
    }

    CaptureFrame {
        frame_index,
        timestamp_90k,
        width: config.width,
        height: config.height,
        format: PixelFormat::Bgra8888,
        buffer_type,
        data,
        captured_at,
    }
}

#[cfg(feature = "real-capture")]
fn run_real_capture_if_available(
    config: &CaptureConfig,
    tx: &mpsc::Sender<CaptureFrame>,
    stop_rx: &watch::Receiver<bool>,
) -> Result<()> {
    real_capture::run_capture(config, tx, stop_rx)
}

#[cfg(not(feature = "real-capture"))]
fn run_real_capture_if_available(
    _config: &CaptureConfig,
    _tx: &mpsc::Sender<CaptureFrame>,
    _stop_rx: &watch::Receiver<bool>,
) -> Result<()> {
    bail!("daemon built without `real-capture` feature")
}

#[cfg(feature = "real-capture")]
mod real_capture {
    use std::os::fd::OwnedFd;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use ashpd::desktop::{
        screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
        PersistMode,
    };
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::pod::Pod;
    use tokio::runtime::Builder;
    use tokio::sync::{mpsc, watch};

    use super::{
        BufferType, CaptureConfig, CaptureFrame, CaptureSource, PixelFormat, PortalSession,
    };

    struct ActivePortalSession {
        meta: PortalSession,
        node_id: u32,
        remote_fd: OwnedFd,
    }

    struct CaptureUserData {
        format: spa::param::video::VideoInfoRaw,
        sender: mpsc::Sender<CaptureFrame>,
        frame_index: u64,
        fps: u32,
        requested_width: u32,
        requested_height: u32,
        fallback_width: u32,
        fallback_height: u32,
        warned_size_mismatch: bool,
        last_stats: Instant,
        stats_frames: u64,
        stats_dropped_pw_buffers: u64,
    }

    pub(super) fn run_capture(
        config: &CaptureConfig,
        tx: &mpsc::Sender<CaptureFrame>,
        stop_rx: &watch::Receiver<bool>,
    ) -> Result<()> {
        let session = open_portal_session(config.source, config.width, config.height)?;
        println!(
            "[capture] portal session started: id={}, source={}, node_id={:?}, size={:?}",
            session.meta.session_id,
            session.meta.source_type,
            session.meta.stream_node_id,
            session.meta.stream_size
        );

        let mainloop =
            pw::main_loop::MainLoopRc::new(None).context("failed to create PipeWire main loop")?;
        let context = pw::context::ContextRc::new(&mainloop, None)
            .context("failed to create PipeWire context")?;
        let core = context
            .connect_fd_rc(session.remote_fd, None)
            .context("failed to connect to PipeWire remote fd")?;

        let stream = pw::stream::StreamBox::new(
            &core,
            "screx-capture",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )
        .context("failed to create PipeWire capture stream")?;

        let mut effective_config = config.clone();
        if let Some((w, h)) = session.meta.stream_size {
            if w > 0 && h > 0 {
                effective_config.width = w as u32;
                effective_config.height = h as u32;
            }
        }

        let data = CaptureUserData {
            format: Default::default(),
            sender: tx.clone(),
            frame_index: 0,
            fps: config.fps.max(1),
            requested_width: config.width,
            requested_height: config.height,
            fallback_width: effective_config.width,
            fallback_height: effective_config.height,
            warned_size_mismatch: false,
            last_stats: Instant::now(),
            stats_frames: 0,
            stats_dropped_pw_buffers: 0,
        };

        let _listener = stream
            .add_local_listener_with_user_data(data)
            .state_changed(|_, _, old, new| {
                println!("[capture] pipewire stream state changed: {old:?} -> {new:?}");
            })
            .param_changed(|_, user_data, id, param| {
                let Some(param) = param else {
                    return;
                };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let (media_type, media_subtype) =
                    match pw::spa::param::format_utils::parse_format(param) {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                if media_type != pw::spa::param::format::MediaType::Video
                    || media_subtype != pw::spa::param::format::MediaSubtype::Raw
                {
                    return;
                }
                if user_data.format.parse(param).is_ok() {
                    let size = user_data.format.size();
                    if size.width > 0 && size.height > 0 {
                        user_data.fallback_width = size.width;
                        user_data.fallback_height = size.height;
                        if !user_data.warned_size_mismatch
                            && (size.width != user_data.requested_width
                                || size.height != user_data.requested_height)
                        {
                            eprintln!(
                                "[capture] negotiated size {}x{} differs from requested {}x{}; adapting encoder geometry",
                                size.width,
                                size.height,
                                user_data.requested_width,
                                user_data.requested_height
                            );
                            user_data.warned_size_mismatch = true;
                        }
                    }
                    println!(
                        "[capture] negotiated video format={:?} size={}x{} framerate={}/{}",
                        user_data.format.format(),
                        user_data.fallback_width,
                        user_data.fallback_height,
                        user_data.format.framerate().num,
                        user_data.format.framerate().denom
                    );
                }
            })
            .process(|stream, user_data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                while let Some(next) = stream.dequeue_buffer() {
                    buffer = next;
                    user_data.stats_dropped_pw_buffers += 1;
                }
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let data_type = data.type_();
                let chunk = data.chunk();
                let chunk_size = chunk.size() as usize;
                let chunk_offset = chunk.offset() as usize;
                let chunk_stride = chunk.stride();

                let Some(raw) = data.data() else {
                    // If this is a non-mapped DMA-BUF path, keep loop alive and fall back in higher layer.
                    return;
                };
                if chunk_offset >= raw.len() {
                    return;
                }
                let max_len = raw.len() - chunk_offset;
                let use_len = if chunk_size == 0 {
                    max_len
                } else {
                    chunk_size.min(max_len)
                };
                let payload = &raw[chunk_offset..(chunk_offset + use_len)];

                let width = user_data.fallback_width.max(1);
                let height = user_data.fallback_height.max(1);
                let expected = (width as usize) * (height as usize) * 4;
                let mut frame_data = vec![0_u8; expected];
                let row_bytes = (width as usize) * 4;
                let stride_abs = if chunk_stride == 0 {
                    row_bytes
                } else {
                    chunk_stride.unsigned_abs() as usize
                };
                let mut copied_rows = 0_usize;
                if stride_abs >= row_bytes {
                    let h = height as usize;
                    for row in 0..h {
                        let src_row = if chunk_stride < 0 { h - 1 - row } else { row };
                        let src_start = src_row.saturating_mul(stride_abs);
                        let src_end = src_start.saturating_add(row_bytes);
                        let dst_start = row.saturating_mul(row_bytes);
                        let dst_end = dst_start.saturating_add(row_bytes);
                        if src_end > payload.len() || dst_end > frame_data.len() {
                            break;
                        }
                        frame_data[dst_start..dst_end].copy_from_slice(&payload[src_start..src_end]);
                        copied_rows += 1;
                    }
                }
                if copied_rows == 0 {
                    let copy_len = expected.min(payload.len());
                    frame_data[..copy_len].copy_from_slice(&payload[..copy_len]);
                }
                // Ensure opaque alpha channel for BGRx payloads.
                for px in frame_data.chunks_exact_mut(4) {
                    px[3] = 255;
                }

                let timestamp_90k =
                    ((user_data.frame_index * 90_000) / user_data.fps as u64) as u32;
                let frame = CaptureFrame {
                    frame_index: user_data.frame_index,
                    timestamp_90k,
                    width,
                    height,
                    format: PixelFormat::Bgra8888,
                    buffer_type: if data_type == spa::buffer::DataType::DmaBuf {
                        BufferType::DmaBuf
                    } else {
                        BufferType::Shm
                    },
                    data: frame_data,
                    captured_at: Instant::now(),
                };
                let _ = user_data.sender.try_send(frame);

                user_data.frame_index = user_data.frame_index.wrapping_add(1);
                user_data.stats_frames += 1;
                if user_data.last_stats.elapsed() >= Duration::from_secs(1) {
                    let fps = user_data.stats_frames as f64
                        / user_data.last_stats.elapsed().as_secs_f64();
                    let buffer_name = if data_type == spa::buffer::DataType::DmaBuf {
                        "DMA-BUF"
                    } else {
                        "SHM"
                    };
                    println!(
                        "[capture] fps={fps:.1} resolution={}x{} format=BGRA buffer={buffer_name} dropped_pw_buffers_per_sec={}",
                        width, height, user_data.stats_dropped_pw_buffers
                    );
                    user_data.last_stats = Instant::now();
                    user_data.stats_frames = 0;
                    user_data.stats_dropped_pw_buffers = 0;
                }
            })
            .register()
            .context("failed to register PipeWire stream listener")?;

        let obj = pw::spa::pod::object!(
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::EnumFormat,
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaType,
                Id,
                pw::spa::param::format::MediaType::Video
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaSubtype,
                Id,
                pw::spa::param::format::MediaSubtype::Raw
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                pw::spa::param::video::VideoFormat::BGRA,
                pw::spa::param::video::VideoFormat::BGRA,
                pw::spa::param::video::VideoFormat::BGRx
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                pw::spa::utils::Rectangle {
                    width: config.width,
                    height: config.height
                },
                pw::spa::utils::Rectangle {
                    width: 1,
                    height: 1
                },
                pw::spa::utils::Rectangle {
                    width: 8192,
                    height: 8192
                }
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                pw::spa::utils::Fraction {
                    num: config.fps.max(1),
                    denom: 1
                },
                pw::spa::utils::Fraction { num: 0, denom: 1 },
                pw::spa::utils::Fraction {
                    num: 240,
                    denom: 1
                }
            ),
        );
        let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(obj),
        )
        .context("failed to serialize PipeWire pod format")?
        .0
        .into_inner();
        let mut params = [Pod::from_bytes(&values).context("failed to decode PipeWire pod bytes")?];

        stream
            .connect(
                spa::utils::Direction::Input,
                Some(session.node_id),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .context("failed to connect PipeWire stream")?;

        println!(
            "[capture] PipeWire stream connected to node {}",
            session.node_id
        );
        while !*stop_rx.borrow() {
            let _ = mainloop.loop_().iterate(Duration::from_millis(50));
        }
        let _ = stream.disconnect();
        println!("[capture] PipeWire stream disconnected");
        Ok(())
    }

    fn open_portal_session(
        source: CaptureSource,
        expected_width: u32,
        expected_height: u32,
    ) -> Result<ActivePortalSession> {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create capture runtime")?;

        runtime.block_on(async {
            let proxy = Screencast::new().await.context("Screencast::new failed")?;

            let session = proxy
                .create_session(Default::default())
                .await
                .context("CreateSession failed")?;

            let options = SelectSourcesOptions::default()
                .set_multiple(false)
                .set_sources(Some(
                    match source {
                        CaptureSource::Virtual1080p => SourceType::Virtual,
                        CaptureSource::Monitor => SourceType::Monitor,
                    }
                    .into(),
                ))
                .set_cursor_mode(CursorMode::Embedded)
                .set_persist_mode(PersistMode::DoNot);

            proxy
                .select_sources(&session, options)
                .await
                .context("SelectSources request failed")?
                .response()
                .context("SelectSources failed")?;

            let start = proxy
                .start(&session, None, Default::default())
                .await
                .context("Start request failed")?
                .response()
                .context("Start failed")?;
            let stream = start.streams().first();
            let node_id = stream.map(|s| s.pipe_wire_node_id());
            let size = stream.and_then(|s| s.size());
            if node_id.is_none() {
                anyhow::bail!("portal returned no screencast streams");
            }

            let pw_fd = proxy
                .open_pipe_wire_remote(&session, Default::default())
                .await
                .context("OpenPipeWireRemote failed")?;
            let node_id = node_id.expect("checked above");
            if source == CaptureSource::Virtual1080p {
                if let Some((width, height)) = size {
                    if width as u32 != expected_width || height as u32 != expected_height {
                        anyhow::bail!(
                            "virtual stream size mismatch: expected {}x{}, got {}x{}",
                            expected_width,
                            expected_height,
                            width,
                            height
                        );
                    }
                }
            }

            Ok(ActivePortalSession {
                meta: PortalSession {
                    session_id: format!("node-{node_id}"),
                    source_type: source.label(),
                    stream_node_id: Some(node_id),
                    stream_size: size,
                },
                node_id,
                remote_fd: pw_fd,
            })
        })
    }
}
