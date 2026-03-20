use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::CaptureFrame;

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
pub struct EncodedAccessUnit {
    pub is_idr: bool,
    pub annex_b: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    Auto,
    Bootstrap,
    Vaapi,
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

pub struct Encoder {
    config: EncoderConfig,
    #[cfg(feature = "real-encode")]
    vaapi: Option<vaapi::DirectVaapiEncoder>,
    use_vaapi: bool,
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

    pub fn new(config: EncoderConfig) -> Result<Self> {
        let mut use_vaapi = false;

        #[cfg(feature = "real-encode")]
        let vaapi_enc = match config.backend {
            EncoderBackend::Vaapi => {
                let enc = vaapi::DirectVaapiEncoder::new(&config)?;
                use_vaapi = true;
                Some(enc)
            }
            EncoderBackend::Auto => match vaapi::DirectVaapiEncoder::new(&config) {
                Ok(enc) => {
                    use_vaapi = true;
                    println!("[encode] backend=vaapi(auto)");
                    Some(enc)
                }
                Err(e) => {
                    eprintln!("[encode] VA-API unavailable ({e:#}), falling back to bootstrap");
                    None
                }
            },
            EncoderBackend::Bootstrap => None,
        };

        if !use_vaapi {
            println!("[encode] backend=bootstrap (synthetic encoder)");
        }

        Ok(Self {
            config,
            #[cfg(feature = "real-encode")]
            vaapi: vaapi_enc,
            use_vaapi,
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
            #[cfg(feature = "real-encode")]
            if self.use_vaapi {
                self.vaapi = Some(vaapi::DirectVaapiEncoder::new(&self.config)?);
            }
        }

        let pli_idr = force_idr && self.last_idr_at.elapsed() >= Self::MIN_PLI_IDR_INTERVAL;
        let is_idr = pli_idr
            || self.frame_count % u64::from(self.config.gop.max(1)) == 0
            || self.last_idr_at.elapsed() >= self.max_idr_interval;

        let mut aus = Vec::new();

        #[cfg(feature = "real-encode")]
        if let Some(enc) = self.vaapi.as_mut() {
            aus = enc.push_frame(&self.config, frame, is_idr)?;
        }

        if aus.is_empty() && !self.use_vaapi {
            aus.push(encode_bootstrap(frame, is_idr));
        }

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

fn encode_bootstrap(frame: &CaptureFrame<'_>, is_idr: bool) -> EncodedAccessUnit {
    let mut annex_b = Vec::with_capacity(256);
    if is_idr {
        // Minimal H.264 SPS/PPS/IDR
        annex_b.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x0A, 0xF8, 0x41, 0xA2]);
        annex_b.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80]);
        annex_b.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        let payload_len = 96.min(frame.data.len());
        annex_b.extend_from_slice(&frame.data[..payload_len]);
    } else {
        annex_b.extend_from_slice(&[0, 0, 0, 1, 0x41]);
        let payload_len = 96.min(frame.data.len());
        annex_b.extend_from_slice(&frame.data[..payload_len]);
    }
    EncodedAccessUnit { is_idr, annex_b }
}

// ---------------------------------------------------------------------------
// VA-API encoder
// ---------------------------------------------------------------------------

#[cfg(feature = "real-encode")]
mod vaapi {
    use std::ptr;

    use anyhow::{bail, Context, Result};
    use ffmpeg_next as ffmpeg;
    use ffmpeg_sys_next as ffi;

    use super::{EncodedAccessUnit, EncoderConfig};
    use crate::capture::CaptureFrame;

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
            ffmpeg::init().context("ffmpeg init")?;
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
                (*ctx).framerate = ffi::AVRational {
                    num: config.fps.max(1) as i32,
                    den: 1,
                };
                (*ctx).bit_rate = config.bitrate_bps as i64;
                (*ctx).rc_buffer_size = (config.bitrate_bps / 2).max(1) as i32;
                (*ctx).gop_size = config.gop as i32;
                (*ctx).max_b_frames = 0;
                (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;

                // async_depth=1 for minimal pipeline delay (0 is rejected by VA-API)
                let opt_key = std::ffi::CString::new("async_depth").unwrap();
                ffi::av_opt_set_int((*ctx).priv_data, opt_key.as_ptr(), 1, 0);

                let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
                if ret < 0 {
                    ffi::av_buffer_unref(&mut (hw_frames_ref as *mut _));
                    ffi::av_buffer_unref(&mut hw_device_ctx);
                    ffi::avcodec_free_context(&mut (ctx as *mut _));
                    bail!("failed to open h264_vaapi encoder (error {ret})");
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
                    "[encode] VA-API H.264 encoder: {}x{}@{} bitrate={} gop={} async_depth=1",
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
            frame: &CaptureFrame<'_>,
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

                    let annex_b = if is_key && !self.extradata.is_empty() {
                        let mut buf = Vec::with_capacity(self.extradata.len() + encoded.len());
                        buf.extend_from_slice(&self.extradata);
                        buf.extend_from_slice(encoded);
                        buf
                    } else {
                        encoded.to_vec()
                    };

                    output.push(EncodedAccessUnit { is_idr: is_key, annex_b });
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
