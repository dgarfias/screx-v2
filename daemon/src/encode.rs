use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg_sys_next as ffi;

use crate::capture::CaptureFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

impl VideoCodec {
    pub fn from_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "h265" | "hevc" | "h.265" => Self::H265,
            _ => Self::H264,
        }
    }

    pub fn transport_id(self) -> u8 {
        match self {
            Self::H264 => 0x00,
            Self::H265 => 0x01,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub bitrate_bps: u32,
    pub gop: u32,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub backend: EncoderBackend,
    pub codec: VideoCodec,
}

#[derive(Debug, Clone)]
pub struct EncodedAccessUnit {
    pub is_idr: bool,
    pub annex_b: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    Auto,
    Vaapi,
    Nvenc,
    Software,
}

impl EncoderBackend {
    pub fn from_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "vaapi" => Self::Vaapi,
            "nvenc" | "nvidia" | "cuda" => Self::Nvenc,
            "software" | "sw" | "x264" | "libx264" | "x265" | "libx265" => Self::Software,
            _ => Self::Auto,
        }
    }
}

enum ActiveEncoder {
    HwAccel(HwEncoder),
    Software(SwEncoder),
}

pub struct Encoder {
    config: EncoderConfig,
    inner: ActiveEncoder,
    frame_count: u64,
    last_idr_at: Instant,
    max_idr_interval: Duration,
    stats_start: Instant,
    stats_encoded: u64,
    stats_bytes: u64,
    stats_idr_count: u64,
}

impl Encoder {
    const MIN_PLI_IDR_INTERVAL: Duration = Duration::from_secs(3);

    pub fn codec(&self) -> VideoCodec {
        self.config.codec
    }

    pub fn new(config: EncoderConfig) -> Result<Self> {
        ffmpeg::init().context("ffmpeg init")?;

        let inner = match config.backend {
            EncoderBackend::Vaapi => {
                let enc = HwEncoder::new_vaapi(&config)?;
                ActiveEncoder::HwAccel(enc)
            }
            EncoderBackend::Nvenc => {
                let enc = HwEncoder::new_nvenc(&config)?;
                ActiveEncoder::HwAccel(enc)
            }
            EncoderBackend::Software => {
                let enc = SwEncoder::new(&config)?;
                ActiveEncoder::Software(enc)
            }
            EncoderBackend::Auto => {
                let codec_name = match config.codec {
                    VideoCodec::H264 => "H.264",
                    VideoCodec::H265 => "H.265",
                };
                if let Ok(enc) = HwEncoder::new_vaapi(&config) {
                    println!("[encode] auto-selected: vaapi ({codec_name})");
                    ActiveEncoder::HwAccel(enc)
                } else if let Ok(enc) = HwEncoder::new_nvenc(&config) {
                    println!("[encode] auto-selected: nvenc ({codec_name})");
                    ActiveEncoder::HwAccel(enc)
                } else {
                    println!("[encode] no hw encoder available, using software ({codec_name})");
                    let enc = SwEncoder::new(&config)?;
                    ActiveEncoder::Software(enc)
                }
            }
        };

        Ok(Self {
            config,
            inner,
            frame_count: 0,
            last_idr_at: Instant::now(),
            max_idr_interval: Duration::from_secs(5),
            stats_start: Instant::now(),
            stats_encoded: 0,
            stats_bytes: 0,
            stats_idr_count: 0,
        })
    }

    pub fn encode_frame(
        &mut self,
        frame: &CaptureFrame<'_>,
        force_idr: bool,
    ) -> Result<Vec<EncodedAccessUnit>> {
        if frame.width != self.config.width || frame.height != self.config.height {
            println!(
                "[encode] resolution change: {}x{} -> {}x{}",
                self.config.width, self.config.height, frame.width, frame.height
            );
            self.config.width = frame.width.max(1);
            self.config.height = frame.height.max(1);
            self.inner = match &self.inner {
                ActiveEncoder::HwAccel(hw) => match hw.kind {
                    HwKind::Vaapi => ActiveEncoder::HwAccel(HwEncoder::new_vaapi(&self.config)?),
                    HwKind::Nvenc => ActiveEncoder::HwAccel(HwEncoder::new_nvenc(&self.config)?),
                },
                ActiveEncoder::Software(_) => ActiveEncoder::Software(SwEncoder::new(&self.config)?),
            };
        }

        let pli_idr = force_idr && self.last_idr_at.elapsed() >= Self::MIN_PLI_IDR_INTERVAL;
        let is_idr = pli_idr
            || self.frame_count % u64::from(self.config.gop.max(1)) == 0
            || self.last_idr_at.elapsed() >= self.max_idr_interval;

        let aus = match &mut self.inner {
            ActiveEncoder::HwAccel(enc) => enc.push_frame(frame, is_idr)?,
            ActiveEncoder::Software(enc) => enc.push_frame(frame, is_idr)?,
        };

        if is_idr && !aus.is_empty() {
            self.last_idr_at = Instant::now();
        }

        self.frame_count += 1;

        for au in &aus {
            self.stats_encoded += 1;
            self.stats_bytes += au.annex_b.len() as u64;
            if au.is_idr {
                self.stats_idr_count += 1;
            }
        }
        if self.stats_start.elapsed() >= Duration::from_secs(1) {
            let elapsed = self.stats_start.elapsed().as_secs_f64();
            let fps = self.stats_encoded as f64 / elapsed;
            let mbps = (self.stats_bytes as f64 * 8.0 / elapsed) / 1_000_000.0;
            println!(
                "[encode] fps={fps:.1} stream_mbps={mbps:.2} idr={}/{} bitrate={}",
                self.stats_idr_count, self.stats_encoded, self.config.bitrate_bps
            );
            self.stats_encoded = 0;
            self.stats_bytes = 0;
            self.stats_idr_count = 0;
            self.stats_start = Instant::now();
        }

        Ok(aus)
    }
}

// ---------------------------------------------------------------------------
// Hardware-accelerated encoder (VA-API or NVENC)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum HwKind {
    Vaapi,
    Nvenc,
}

struct HwEncoder {
    kind: HwKind,
    ctx: *mut ffi::AVCodecContext,
    hw_device_ctx: *mut ffi::AVBufferRef,
    sws: *mut ffi::SwsContext,
    sw_bgra: *mut ffi::AVFrame,
    sw_nv12: *mut ffi::AVFrame,
    hw_frame: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    width: i32,
    height: i32,
    frame_index: u64,
    fps: u32,
    extradata: Vec<u8>,
}

unsafe impl Send for HwEncoder {}

impl HwEncoder {
    fn new_vaapi(config: &EncoderConfig) -> Result<Self> {
        Self::new(config, HwKind::Vaapi)
    }

    fn new_nvenc(config: &EncoderConfig) -> Result<Self> {
        Self::new(config, HwKind::Nvenc)
    }

    fn new(config: &EncoderConfig, kind: HwKind) -> Result<Self> {
        let width = config.width as i32;
        let height = config.height as i32;

        let (codec_name, hw_type, hw_pix_fmt, device_path) = match (kind, config.codec) {
            (HwKind::Vaapi, VideoCodec::H264) => (
                "h264_vaapi",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
                "/dev/dri/renderD128",
            ),
            (HwKind::Vaapi, VideoCodec::H265) => (
                "hevc_vaapi",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
                "/dev/dri/renderD128",
            ),
            (HwKind::Nvenc, VideoCodec::H264) => (
                "h264_nvenc",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
                "0",
            ),
            (HwKind::Nvenc, VideoCodec::H265) => (
                "hevc_nvenc",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
                "0",
            ),
        };

        let kind_name = match kind {
            HwKind::Vaapi => "VA-API",
            HwKind::Nvenc => "NVENC",
        };

        unsafe {
            let codec_cstr = std::ffi::CString::new(codec_name).unwrap();
            let codec = ffi::avcodec_find_encoder_by_name(codec_cstr.as_ptr());
            if codec.is_null() {
                bail!("{codec_name} encoder not found");
            }

            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                bail!("failed to allocate codec context");
            }

            let mut hw_device_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
            let device_cstr = std::ffi::CString::new(device_path).unwrap();
            let ret = ffi::av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                hw_type,
                device_cstr.as_ptr(),
                ptr::null_mut(),
                0,
            );
            if ret < 0 {
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to create {kind_name} device context (error {ret})");
            }

            let hw_frames_ref = ffi::av_hwframe_ctx_alloc(hw_device_ctx);
            if hw_frames_ref.is_null() {
                ffi::av_buffer_unref(&mut hw_device_ctx);
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to allocate hw frames context");
            }
            let frames_ctx = (*hw_frames_ref).data as *mut ffi::AVHWFramesContext;
            (*frames_ctx).format = hw_pix_fmt;
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
            (*ctx).framerate = ffi::AVRational {
                num: config.fps.max(1) as i32,
                den: 1,
            };
            (*ctx).bit_rate = config.bitrate_bps as i64;
            (*ctx).rc_buffer_size = (config.bitrate_bps / 2).max(1) as i32;
            (*ctx).gop_size = config.gop as i32;
            (*ctx).max_b_frames = 0;
            (*ctx).pix_fmt = hw_pix_fmt;

            match kind {
                HwKind::Vaapi => {
                    let key = std::ffi::CString::new("async_depth").unwrap();
                    ffi::av_opt_set_int((*ctx).priv_data, key.as_ptr(), 1, 0);
                }
                HwKind::Nvenc => {
                    let preset = std::ffi::CString::new("preset").unwrap();
                    let p4 = std::ffi::CString::new("p4").unwrap();
                    ffi::av_opt_set((*ctx).priv_data, preset.as_ptr(), p4.as_ptr(), 0);
                    let tune = std::ffi::CString::new("tune").unwrap();
                    let ull = std::ffi::CString::new("ull").unwrap();
                    ffi::av_opt_set((*ctx).priv_data, tune.as_ptr(), ull.as_ptr(), 0);
                    let delay = std::ffi::CString::new("delay").unwrap();
                    ffi::av_opt_set_int((*ctx).priv_data, delay.as_ptr(), 0, 0);
                    let zerolatency = std::ffi::CString::new("zerolatency").unwrap();
                    let one = std::ffi::CString::new("1").unwrap();
                    ffi::av_opt_set((*ctx).priv_data, zerolatency.as_ptr(), one.as_ptr(), 0);
                }
            }

            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                ffi::av_buffer_unref(&mut (hw_frames_ref as *mut _));
                ffi::av_buffer_unref(&mut hw_device_ctx);
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to open {codec_name} encoder (error {ret})");
            }

            let extradata = if !(*ctx).extradata.is_null() && (*ctx).extradata_size > 0 {
                std::slice::from_raw_parts((*ctx).extradata, (*ctx).extradata_size as usize)
                    .to_vec()
            } else {
                Vec::new()
            };

            ffi::av_buffer_unref(&mut (hw_frames_ref as *mut _));

            let sws = ffi::sws_getContext(
                width,
                height,
                ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                width,
                height,
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                1, // SWS_FAST_BILINEAR
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if sws.is_null() {
                ffi::av_buffer_unref(&mut hw_device_ctx);
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to create swscale context");
            }

            let sw_bgra = ffi::av_frame_alloc();
            if sw_bgra.is_null() {
                bail!("failed to allocate BGRA frame");
            }
            (*sw_bgra).format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA as i32;
            (*sw_bgra).width = width;
            (*sw_bgra).height = height;
            (*sw_bgra).linesize[0] = width * 4;

            let sw_nv12 = ffi::av_frame_alloc();
            if sw_nv12.is_null() {
                bail!("failed to allocate NV12 frame");
            }
            (*sw_nv12).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
            (*sw_nv12).width = width;
            (*sw_nv12).height = height;
            let ret = ffi::av_frame_get_buffer(sw_nv12, 0);
            if ret < 0 {
                bail!("failed to allocate NV12 buffer (error {ret})");
            }

            let hw_frame = ffi::av_frame_alloc();
            if hw_frame.is_null() {
                bail!("failed to allocate HW frame");
            }

            let pkt = ffi::av_packet_alloc();
            if pkt.is_null() {
                bail!("failed to allocate packet");
            }

            println!(
                "[encode] {kind_name} H.264 encoder: {}x{}@{} bitrate={} gop={}",
                width, height, config.fps, config.bitrate_bps, config.gop
            );

            Ok(Self {
                kind,
                ctx,
                hw_device_ctx,
                sws,
                sw_bgra,
                sw_nv12,
                hw_frame,
                pkt,
                width,
                height,
                frame_index: 0,
                fps: config.fps.max(1),
                extradata,
            })
        }
    }

    fn push_frame(
        &mut self,
        frame: &CaptureFrame<'_>,
        is_idr: bool,
    ) -> Result<Vec<EncodedAccessUnit>> {
        unsafe {
            (*self.sw_bgra).data[0] = frame.data.as_ptr() as *mut u8;

            ffi::sws_scale(
                self.sws,
                (*self.sw_bgra).data.as_ptr() as *const *const u8,
                (*self.sw_bgra).linesize.as_ptr(),
                0,
                self.height,
                (*self.sw_nv12).data.as_mut_ptr(),
                (*self.sw_nv12).linesize.as_mut_ptr(),
            );

            let ret = ffi::av_hwframe_get_buffer(
                (*self.ctx).hw_frames_ctx,
                self.hw_frame,
                0,
            );
            if ret < 0 {
                bail!("failed to get hw frame buffer (error {ret})");
            }

            let ret = ffi::av_hwframe_transfer_data(self.hw_frame, self.sw_nv12, 0);
            if ret < 0 {
                ffi::av_frame_unref(self.hw_frame);
                bail!("failed to upload frame to hw surface (error {ret})");
            }

            let pts = (self.frame_index as i64 * 90_000) / self.fps as i64;
            (*self.hw_frame).pts = pts;
            (*self.hw_frame).pict_type = if is_idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };

            let ret = ffi::avcodec_send_frame(self.ctx, self.hw_frame);
            ffi::av_frame_unref(self.hw_frame);
            if ret < 0 {
                bail!("avcodec_send_frame failed (error {ret})");
            }

            let output = self.drain_packets()?;
            self.frame_index += 1;
            Ok(output)
        }
    }

    fn drain_packets(&mut self) -> Result<Vec<EncodedAccessUnit>> {
        let mut output = Vec::new();
        unsafe {
            loop {
                let ret = ffi::avcodec_receive_packet(self.ctx, self.pkt);
                if ret == ffi::AVERROR(ffi::EAGAIN) || ret == ffi::AVERROR_EOF {
                    break;
                }
                if ret < 0 {
                    bail!("avcodec_receive_packet failed (error {ret})");
                }

                let encoded =
                    std::slice::from_raw_parts((*self.pkt).data, (*self.pkt).size as usize);
                let is_key = ((*self.pkt).flags & ffi::AV_PKT_FLAG_KEY) != 0;

                let annex_b = if is_key && !self.extradata.is_empty() {
                    let mut buf = Vec::with_capacity(self.extradata.len() + encoded.len());
                    buf.extend_from_slice(&self.extradata);
                    buf.extend_from_slice(encoded);
                    buf
                } else {
                    encoded.to_vec()
                };

                output.push(EncodedAccessUnit { is_idr: is_key, annex_b });
                ffi::av_packet_unref(self.pkt);
            }
        }
        Ok(output)
    }
}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.pkt.is_null() {
                ffi::av_packet_free(&mut self.pkt);
            }
            if !self.hw_frame.is_null() {
                ffi::av_frame_free(&mut self.hw_frame);
            }
            if !self.sw_nv12.is_null() {
                ffi::av_frame_free(&mut self.sw_nv12);
            }
            if !self.sw_bgra.is_null() {
                ffi::av_frame_free(&mut self.sw_bgra);
            }
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

// ---------------------------------------------------------------------------
// Software encoder (libx264)
// ---------------------------------------------------------------------------

struct SwEncoder {
    ctx: *mut ffi::AVCodecContext,
    sws: *mut ffi::SwsContext,
    sw_bgra: *mut ffi::AVFrame,
    sw_yuv: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    height: i32,
    frame_index: u64,
    fps: u32,
    extradata: Vec<u8>,
}

unsafe impl Send for SwEncoder {}

impl SwEncoder {
    fn new(config: &EncoderConfig) -> Result<Self> {
        let width = config.width as i32;
        let height = config.height as i32;

        let (enc_name, opts): (&str, Vec<(&str, &str)>) = match config.codec {
            VideoCodec::H264 => ("libx264", vec![
                ("preset", "ultrafast"),
                ("tune", "zerolatency"),
                ("forced-idr", "1"),
                ("profile", "baseline"),
            ]),
            VideoCodec::H265 => ("libx265", vec![
                ("preset", "ultrafast"),
                ("tune", "zerolatency"),
                ("forced-idr", "1"),
            ]),
        };

        unsafe {
            let codec_name = std::ffi::CString::new(enc_name).unwrap();
            let codec = ffi::avcodec_find_encoder_by_name(codec_name.as_ptr());
            if codec.is_null() {
                bail!("{enc_name} encoder not found");
            }

            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                bail!("failed to allocate codec context");
            }

            (*ctx).width = width;
            (*ctx).height = height;
            (*ctx).time_base = ffi::AVRational { num: 1, den: 90_000 };
            (*ctx).framerate = ffi::AVRational {
                num: config.fps.max(1) as i32,
                den: 1,
            };
            (*ctx).bit_rate = config.bitrate_bps as i64;
            (*ctx).rc_buffer_size = (config.bitrate_bps / 2).max(1) as i32;
            (*ctx).gop_size = config.gop as i32;
            (*ctx).max_b_frames = 0;
            (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
            (*ctx).thread_count = 1;
            (*ctx).flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32;

            for (k, v) in &opts {
                let key = std::ffi::CString::new(*k).unwrap();
                let val = std::ffi::CString::new(*v).unwrap();
                ffi::av_opt_set((*ctx).priv_data, key.as_ptr(), val.as_ptr(), 0);
            }

            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to open {enc_name} encoder (error {ret})");
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

            let sws = ffi::sws_getContext(
                width,
                height,
                ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                width,
                height,
                ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
                1, // SWS_FAST_BILINEAR
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if sws.is_null() {
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to create swscale context");
            }

            let sw_bgra = ffi::av_frame_alloc();
            if sw_bgra.is_null() {
                bail!("failed to allocate BGRA frame");
            }
            (*sw_bgra).format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA as i32;
            (*sw_bgra).width = width;
            (*sw_bgra).height = height;
            (*sw_bgra).linesize[0] = width * 4;

            let sw_yuv = ffi::av_frame_alloc();
            if sw_yuv.is_null() {
                bail!("failed to allocate YUV420P frame");
            }
            (*sw_yuv).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
            (*sw_yuv).width = width;
            (*sw_yuv).height = height;
            let ret = ffi::av_frame_get_buffer(sw_yuv, 0);
            if ret < 0 {
                bail!("failed to allocate YUV420P buffer (error {ret})");
            }

            let pkt = ffi::av_packet_alloc();
            if pkt.is_null() {
                bail!("failed to allocate packet");
            }

            println!(
                "[encode] software encoder ({enc_name}): {}x{}@{} bitrate={} gop={}",
                width, height, config.fps, config.bitrate_bps, config.gop
            );

            Ok(Self {
                ctx,
                sws,
                sw_bgra,
                sw_yuv,
                pkt,
                height,
                frame_index: 0,
                fps: config.fps.max(1),
                extradata,
            })
        }
    }

    fn push_frame(
        &mut self,
        frame: &CaptureFrame<'_>,
        is_idr: bool,
    ) -> Result<Vec<EncodedAccessUnit>> {
        unsafe {
            // Ensure we have exclusive ownership of the YUV buffer
            // (libx264 may still hold a ref from the previous frame)
            let ret = ffi::av_frame_make_writable(self.sw_yuv);
            if ret < 0 {
                bail!("av_frame_make_writable failed (error {ret})");
            }

            (*self.sw_bgra).data[0] = frame.data.as_ptr() as *mut u8;

            ffi::sws_scale(
                self.sws,
                (*self.sw_bgra).data.as_ptr() as *const *const u8,
                (*self.sw_bgra).linesize.as_ptr(),
                0,
                self.height,
                (*self.sw_yuv).data.as_mut_ptr(),
                (*self.sw_yuv).linesize.as_mut_ptr(),
            );

            let pts = (self.frame_index as i64 * 90_000) / self.fps as i64;
            (*self.sw_yuv).pts = pts;
            (*self.sw_yuv).pict_type = if is_idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };

            let ret = ffi::avcodec_send_frame(self.ctx, self.sw_yuv);
            if ret < 0 {
                bail!("avcodec_send_frame failed (error {ret})");
            }

            let mut output = Vec::new();
            loop {
                let ret = ffi::avcodec_receive_packet(self.ctx, self.pkt);
                if ret == ffi::AVERROR(ffi::EAGAIN) || ret == ffi::AVERROR_EOF {
                    break;
                }
                if ret < 0 {
                    bail!("avcodec_receive_packet failed (error {ret})");
                }

                let encoded =
                    std::slice::from_raw_parts((*self.pkt).data, (*self.pkt).size as usize);
                let is_key = ((*self.pkt).flags & ffi::AV_PKT_FLAG_KEY) != 0;

                let annex_b = if is_key && !self.extradata.is_empty() {
                    let mut buf = Vec::with_capacity(self.extradata.len() + encoded.len());
                    buf.extend_from_slice(&self.extradata);
                    buf.extend_from_slice(encoded);
                    buf
                } else {
                    encoded.to_vec()
                };

                output.push(EncodedAccessUnit { is_idr: is_key, annex_b });
                ffi::av_packet_unref(self.pkt);
            }

            self.frame_index += 1;
            Ok(output)
        }
    }
}

impl Drop for SwEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.pkt.is_null() {
                ffi::av_packet_free(&mut self.pkt);
            }
            if !self.sw_yuv.is_null() {
                ffi::av_frame_free(&mut self.sw_yuv);
            }
            if !self.sw_bgra.is_null() {
                ffi::av_frame_free(&mut self.sw_bgra);
            }
            if !self.sws.is_null() {
                ffi::sws_freeContext(self.sws);
            }
            if !self.ctx.is_null() {
                ffi::avcodec_free_context(&mut self.ctx);
            }
        }
    }
}
