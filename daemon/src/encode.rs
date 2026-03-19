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

pub fn run_encoder_loop(
    mut config: EncoderConfig,
    mut frame_rx: mpsc::Receiver<CaptureFrame>,
    au_tx: mpsc::Sender<EncodedAccessUnit>,
    mut control_rx: mpsc::Receiver<ControlMessage>,
    stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    let active_backend = initialize_encoder_backend(config.backend, &config)?;
    #[cfg(feature = "real-encode")]
    let mut vaapi_encoder = match active_backend {
        ActiveEncoderBackend::VaapiBootstrapPayload => {
            Some(vaapi::DirectVaapiEncoder::new(&config)?)
        }
        ActiveEncoderBackend::Bootstrap => None,
    };

    let mut force_next_idr = true;
    let mut encoded_in_window = 0_u64;
    let mut bytes_in_window = 0_u64;
    let mut dropped_capture_frames_in_window = 0_u64;
    let mut window_start = Instant::now();

    loop {
        if *stop_rx.borrow() {
            println!("[encode] stop signal received");
            break;
        }

        while let Ok(msg) = control_rx.try_recv() {
            match msg {
                ControlMessage::RequestIdr => {
                    println!("[encode] control: forcing IDR on next frame");
                    force_next_idr = true;
                }
                ControlMessage::SetBitrate(bps) => {
                    println!("[encode] control: updating bitrate {} -> {}", config.bitrate_bps, bps);
                    config.bitrate_bps = bps.max(500_000);
                    #[cfg(feature = "real-encode")]
                    if let Some(worker) = vaapi_encoder.as_mut() {
                        *worker = vaapi::DirectVaapiEncoder::new(&config)?;
                    }
                }
                ControlMessage::SetResolution(w, h) => {
                    println!("[encode] control: target resolution {}x{} -> {}x{}", config.width, config.height, w, h);
                    config.width = w.max(320);
                    config.height = h.max(180);
                    force_next_idr = true;
                    #[cfg(feature = "real-encode")]
                    if let Some(worker) = vaapi_encoder.as_mut() {
                        *worker = vaapi::DirectVaapiEncoder::new(&config)?;
                    }
                }
            }
        }

        let mut frame = match frame_rx.blocking_recv() {
            Some(f) => f,
            None => {
                println!("[encode] upstream channel closed");
                return Ok(());
            }
        };
        if *stop_rx.borrow() {
            println!("[encode] stop signal received");
            return Ok(());
        }
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
                *worker = vaapi::DirectVaapiEncoder::new(&config)?;
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
        force_next_idr = false;

        for au in produced_aus {
            bytes_in_window += au.annex_b.len() as u64;
            encoded_in_window += 1;
            if window_start.elapsed() >= Duration::from_secs(1) {
                let elapsed = window_start.elapsed().as_secs_f64();
                let fps = encoded_in_window as f64 / elapsed;
                let mbps = (bytes_in_window as f64 * 8.0 / elapsed) / 1_000_000.0;
                println!(
                    "[encode] fps={fps:.1} stream_mbps={mbps:.2} target_bitrate_bps={} dropped_capture_frames_per_sec={}",
                    config.bitrate_bps, dropped_capture_frames_in_window
                );
                encoded_in_window = 0;
                bytes_in_window = 0;
                dropped_capture_frames_in_window = 0;
                window_start = Instant::now();
            }

            let mut pending = au;
            loop {
                match au_tx.try_send(pending) {
                    Ok(()) => break,
                    Err(tokio::sync::mpsc::error::TrySendError::Full(returned)) => {
                        if *stop_rx.borrow() {
                            return Ok(());
                        }
                        pending = returned;
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        println!("[encode] downstream channel closed");
                        return Ok(());
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
    use std::fs;
    use std::ptr;

    use anyhow::{bail, Context, Result};
    use ffmpeg_next as ffmpeg;
    use ffmpeg_sys_next as ffi;

    use super::{EncodedAccessUnit, EncoderConfig};
    use crate::capture::CaptureFrame;

    pub(super) fn initialize(config: &EncoderConfig) -> Result<()> {
        ffmpeg::init().context("ffmpeg init failed")?;
        let encoder = ffmpeg::codec::encoder::find_by_name("h264_vaapi")
            .context("h264_vaapi encoder unavailable")?;
        if !encoder.is_encoder() {
            bail!("h264_vaapi codec is not flagged as encoder");
        }

        let render_node = "/dev/dri/renderD128";
        let meta = fs::metadata(render_node)
            .with_context(|| format!("VA-API render node missing: {render_node}"))?;
        if meta.permissions().readonly() {
            bail!("VA-API render node is not writable: {render_node}");
        }

        println!(
            "[encode] vaapi init target={}x{}@{} bitrate={}",
            config.width, config.height, config.fps, config.bitrate_bps
        );
        Ok(())
    }

    pub(super) struct DirectVaapiEncoder {
        ctx: *mut ffi::AVCodecContext,
        hw_device_ctx: *mut ffi::AVBufferRef,
        sws: *mut ffi::SwsContext,
        width: i32,
        height: i32,
        frame_index: u64,
        fps: u32,
        extradata: Vec<u8>,
    }

    unsafe impl Send for DirectVaapiEncoder {}

    impl DirectVaapiEncoder {
        pub(super) fn new(config: &EncoderConfig) -> Result<Self> {
            let width = config.width as i32;
            let height = config.height as i32;

            unsafe {
                let codec_name = std::ffi::CString::new("h264_vaapi").unwrap();
                let codec = ffi::avcodec_find_encoder_by_name(codec_name.as_ptr());
                if codec.is_null() {
                    bail!("h264_vaapi encoder not found");
                }

                let ctx = ffi::avcodec_alloc_context3(codec);
                if ctx.is_null() {
                    bail!("failed to allocate codec context");
                }

                let mut hw_device_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
                let device_path = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
                let ret = ffi::av_hwdevice_ctx_create(
                    &mut hw_device_ctx,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    device_path.as_ptr(),
                    ptr::null_mut(),
                    0,
                );
                if ret < 0 {
                    ffi::avcodec_free_context(&mut (ctx as *mut _));
                    bail!("failed to create VA-API device context (error {ret})");
                }

                let hw_frames_ref = ffi::av_hwframe_ctx_alloc(hw_device_ctx);
                if hw_frames_ref.is_null() {
                    ffi::av_buffer_unref(&mut hw_device_ctx);
                    ffi::avcodec_free_context(&mut (ctx as *mut _));
                    bail!("failed to allocate hw frames context");
                }
                let frames_ctx = (*hw_frames_ref).data as *mut ffi::AVHWFramesContext;
                (*frames_ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
                (*frames_ctx).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
                (*frames_ctx).width = width;
                (*frames_ctx).height = height;
                (*frames_ctx).initial_pool_size = 20;

                let ret = ffi::av_hwframe_ctx_init(hw_frames_ref);
                if ret < 0 {
                    ffi::av_buffer_unref(&mut (hw_frames_ref as *mut _));
                    ffi::av_buffer_unref(&mut hw_device_ctx);
                    ffi::avcodec_free_context(&mut (ctx as *mut _));
                    bail!("failed to init hw frames context (error {ret})");
                }

                (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device_ctx);
                (*ctx).hw_frames_ctx = ffi::av_buffer_ref(hw_frames_ref);
                (*ctx).width = width;
                (*ctx).height = height;
                (*ctx).time_base = ffi::AVRational { num: 1, den: 90_000 };
                (*ctx).framerate = ffi::AVRational { num: config.fps.max(1) as i32, den: 1 };
                (*ctx).bit_rate = config.bitrate_bps as i64;
                (*ctx).rc_buffer_size = (config.bitrate_bps / 2).max(1) as i32;
                (*ctx).gop_size = config.gop as i32;
                (*ctx).max_b_frames = 0;
                (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;

                let opt_key = std::ffi::CString::new("async_depth").unwrap();
                ffi::av_opt_set_int((*ctx).priv_data, opt_key.as_ptr(), 0, 0);

                let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
                if ret < 0 {
                    ffi::av_buffer_unref(&mut (hw_frames_ref as *mut _));
                    ffi::av_buffer_unref(&mut hw_device_ctx);
                    ffi::avcodec_free_context(&mut (ctx as *mut _));
                    bail!("failed to open hevc_vaapi encoder (error {ret})");
                }

                let extradata = if !(*ctx).extradata.is_null() && (*ctx).extradata_size > 0 {
                    std::slice::from_raw_parts(
                        (*ctx).extradata,
                        (*ctx).extradata_size as usize,
                    )
                    .to_vec()
                } else {
                    Vec::new()
                };

                // hw_frames_ref ownership transferred to ctx, release our ref
                ffi::av_buffer_unref(&mut (hw_frames_ref as *mut _));

                let sws = ffi::sws_getContext(
                    width,
                    height,
                    ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                    width,
                    height,
                    ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                    ffi::SwsFlags::SWS_FAST_BILINEAR as i32,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                );
                if sws.is_null() {
                    ffi::av_buffer_unref(&mut hw_device_ctx);
                    ffi::avcodec_free_context(&mut (ctx as *mut _));
                    bail!("failed to create swscale context");
                }

                println!(
                    "[encode] direct VA-API H.264 encoder opened: {}x{}@{} bitrate={} gop={} async_depth=0",
                    width, height, config.fps, config.bitrate_bps, config.gop
                );

                Ok(Self {
                    ctx,
                    hw_device_ctx,
                    sws,
                    width,
                    height,
                    frame_index: 0,
                    fps: config.fps.max(1),
                    extradata,
                })
            }
        }

        pub(super) fn push_frame(
            &mut self,
            _config: &EncoderConfig,
            frame: &CaptureFrame,
            is_idr: bool,
        ) -> Result<Vec<EncodedAccessUnit>> {
            unsafe {
                let sw_bgra = ffi::av_frame_alloc();
                if sw_bgra.is_null() {
                    bail!("failed to allocate BGRA frame");
                }
                (*sw_bgra).format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA as i32;
                (*sw_bgra).width = self.width;
                (*sw_bgra).height = self.height;
                (*sw_bgra).data[0] = frame.data.as_ptr() as *mut u8;
                (*sw_bgra).linesize[0] = self.width * 4;

                let sw_nv12 = ffi::av_frame_alloc();
                if sw_nv12.is_null() {
                    ffi::av_frame_free(&mut (sw_bgra as *mut _));
                    bail!("failed to allocate NV12 frame");
                }
                (*sw_nv12).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
                (*sw_nv12).width = self.width;
                (*sw_nv12).height = self.height;
                let ret = ffi::av_frame_get_buffer(sw_nv12, 0);
                if ret < 0 {
                    ffi::av_frame_free(&mut (sw_bgra as *mut _));
                    ffi::av_frame_free(&mut (sw_nv12 as *mut _));
                    bail!("failed to allocate NV12 buffer (error {ret})");
                }

                ffi::sws_scale(
                    self.sws,
                    (*sw_bgra).data.as_ptr() as *const *const u8,
                    (*sw_bgra).linesize.as_ptr(),
                    0,
                    self.height,
                    (*sw_nv12).data.as_mut_ptr(),
                    (*sw_nv12).linesize.as_mut_ptr(),
                );

                let hw_frame = ffi::av_frame_alloc();
                if hw_frame.is_null() {
                    ffi::av_frame_free(&mut (sw_bgra as *mut _));
                    ffi::av_frame_free(&mut (sw_nv12 as *mut _));
                    bail!("failed to allocate hw frame");
                }
                let ret = ffi::av_hwframe_get_buffer((*self.ctx).hw_frames_ctx, hw_frame, 0);
                if ret < 0 {
                    ffi::av_frame_free(&mut (sw_bgra as *mut _));
                    ffi::av_frame_free(&mut (sw_nv12 as *mut _));
                    ffi::av_frame_free(&mut (hw_frame as *mut _));
                    bail!("failed to get hw frame buffer (error {ret})");
                }

                let ret = ffi::av_hwframe_transfer_data(hw_frame, sw_nv12, 0);
                ffi::av_frame_free(&mut (sw_bgra as *mut _));
                ffi::av_frame_free(&mut (sw_nv12 as *mut _));
                if ret < 0 {
                    ffi::av_frame_free(&mut (hw_frame as *mut _));
                    bail!("failed to upload frame to VA-API surface (error {ret})");
                }

                let pts = (self.frame_index as i64 * 90_000) / self.fps as i64;
                (*hw_frame).pts = pts;

                if is_idr {
                    (*hw_frame).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
                }

                let ret = ffi::avcodec_send_frame(self.ctx, hw_frame);
                ffi::av_frame_free(&mut (hw_frame as *mut _));
                if ret < 0 {
                    bail!("avcodec_send_frame failed (error {ret})");
                }

                let mut output = Vec::new();
                let pkt = ffi::av_packet_alloc();
                loop {
                    let ret = ffi::avcodec_receive_packet(self.ctx, pkt);
                    if ret == ffi::AVERROR(ffi::EAGAIN) || ret == ffi::AVERROR_EOF {
                        break;
                    }
                    if ret < 0 {
                        ffi::av_packet_free(&mut (pkt as *mut _));
                        bail!("avcodec_receive_packet failed (error {ret})");
                    }

                    let encoded =
                        std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
                    let is_key = ((*pkt).flags & ffi::AV_PKT_FLAG_KEY) != 0;
                    let timestamp_90k = (*pkt).pts as u32;

                    let annex_b = if is_key && !self.extradata.is_empty() {
                        let mut buf =
                            Vec::with_capacity(self.extradata.len() + encoded.len());
                        buf.extend_from_slice(&self.extradata);
                        buf.extend_from_slice(encoded);
                        buf
                    } else {
                        encoded.to_vec()
                    };

                    output.push(EncodedAccessUnit {
                        frame_index: self.frame_index,
                        timestamp_90k,
                        is_idr: is_key,
                        annex_b,
                    });

                    ffi::av_packet_unref(pkt);
                }
                ffi::av_packet_free(&mut (pkt as *mut _));

                self.frame_index += 1;
                Ok(output)
            }
        }
    }

    impl Drop for DirectVaapiEncoder {
        fn drop(&mut self) {
            unsafe {
                if !self.sws.is_null() {
                    ffi::sws_freeContext(self.sws);
                }
                if !self.ctx.is_null() {
                    ffi::avcodec_free_context(&mut self.ctx);
                }
                if !self.hw_device_ctx.is_null() {
                    ffi::av_buffer_unref(&mut self.hw_device_ctx);
                }
            }
        }
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
