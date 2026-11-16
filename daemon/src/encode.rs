use std::ops::Deref;
use std::ptr;
use std::time::{Duration, Instant};

use libc;

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
    /// Replace periodic full IDR keyframes with rolling intra-refresh where the
    /// encoder supports it (software H.264, NVENC). Spreads the keyframe cost
    /// across frames instead of spiking the pipeline every GOP; full IDRs are
    /// then emitted only when the client explicitly requests one (PLI).
    pub intra_refresh: bool,
}

/// Owned reference to an encoded packet's underlying AVBuffer.
/// Avoids copying the encoded data when no extradata prefix is required.
pub struct OwnedPacketBuf {
    buf: *mut ffi::AVBufferRef,
    data: *const u8,
    len: usize,
}

unsafe impl Send for OwnedPacketBuf {}

impl OwnedPacketBuf {
    /// Create an owned buffer ref from a received AVPacket.
    /// Returns `None` if the packet does not own a buffer.
    unsafe fn from_packet(pkt: *const ffi::AVPacket) -> Option<Self> {
        if pkt.is_null() || (*pkt).buf.is_null() {
            return None;
        }
        let buf = ffi::av_buffer_ref((*pkt).buf);
        if buf.is_null() {
            return None;
        }
        Some(Self {
            buf,
            data: (*pkt).data,
            len: (*pkt).size as usize,
        })
    }
}

impl Deref for OwnedPacketBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }
}

impl Drop for OwnedPacketBuf {
    fn drop(&mut self) {
        unsafe {
            if !self.buf.is_null() {
                ffi::av_buffer_unref(&mut self.buf);
            }
        }
    }
}

/// Holds an Annex-B access unit, either as a zero-copy packet buffer or as a
/// freshly allocated Vec when an IDR extradata prefix is prepended.
pub enum AnnexB {
    Packet(OwnedPacketBuf),
    Vec(Vec<u8>),
}

impl Deref for AnnexB {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            AnnexB::Packet(p) => p,
            AnnexB::Vec(v) => v,
        }
    }
}

pub struct EncodedAccessUnit {
    pub is_idr: bool,
    pub annex_b: AnnexB,
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

/// Resolve the ffmpeg encoder name that would be used for a concrete
/// (non-`Auto`) backend + codec pair. Mirrors the name tables in
/// `HwEncoder::new_with_vaapi_hevc_mode` and `SwEncoder::new` below — kept
/// separate so `probe_available_codecs` can check availability without
/// opening any hardware device.
fn resolved_codec_name(backend: EncoderBackend, codec: VideoCodec) -> Option<&'static str> {
    Some(match (backend, codec) {
        (EncoderBackend::Vaapi, VideoCodec::H264) => "h264_vaapi",
        (EncoderBackend::Vaapi, VideoCodec::H265) => "hevc_vaapi",
        (EncoderBackend::Nvenc, VideoCodec::H264) => "h264_nvenc",
        (EncoderBackend::Nvenc, VideoCodec::H265) => "hevc_nvenc",
        (EncoderBackend::Software, VideoCodec::H264) => "libx264",
        (EncoderBackend::Software, VideoCodec::H265) => "libx265",
        (EncoderBackend::Auto, _) => return None,
    })
}

/// True if ffmpeg has an encoder registered under this name. Cheap — a
/// symbol table lookup, no device is opened (same call used at encoder
/// construction time, `avcodec_find_encoder_by_name`).
fn encoder_name_resolves(name: &str) -> bool {
    match std::ffi::CString::new(name) {
        Ok(cstr) => unsafe { !ffi::avcodec_find_encoder_by_name(cstr.as_ptr()).is_null() },
        Err(_) => false,
    }
}

/// Which codecs have a matching FFmpeg encoder registered for `backend`
/// (which may be `Auto`). Does not just parrot the operator's `--codec`
/// flag; hardware and session initialization may still fail later.
/// `Auto` reports the union of what any backend it would consider at
/// encoder-construction time supports, since the one that actually wins is
/// only known once real hardware probing happens in `Encoder::new`.
pub fn probe_available_codecs(backend: EncoderBackend) -> Vec<VideoCodec> {
    let _ = ffmpeg::init();

    let candidates: Vec<EncoderBackend> = match backend {
        EncoderBackend::Auto => {
            vec![
                EncoderBackend::Vaapi,
                EncoderBackend::Nvenc,
                EncoderBackend::Software,
            ]
        }
        other => vec![other],
    };

    [VideoCodec::H264, VideoCodec::H265]
        .into_iter()
        .filter(|codec| {
            candidates
                .iter()
                .any(|b| resolved_codec_name(*b, *codec).is_some_and(encoder_name_resolves))
        })
        .collect()
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
    /// A client keyframe request (PLI) not yet answered. Requests that arrive
    /// inside `MIN_PLI_IDR_INTERVAL` wait here instead of being dropped.
    pending_pli: bool,
    stats_start: Instant,
    stats_encoded: u64,
    stats_bytes: u64,
    stats_idr_count: u64,
}

impl Encoder {
    /// Minimum spacing between PLI-triggered IDRs. The client already limits
    /// PLIs to one per second; this only guards against PLI storms.
    const MIN_PLI_IDR_INTERVAL: Duration = Duration::from_secs(1);

    pub fn codec(&self) -> VideoCodec {
        self.config.codec
    }

    pub fn bitrate_bps(&self) -> u32 {
        self.config.bitrate_bps
    }

    pub fn reconfigure_bitrate(&mut self, bps: u32) -> Result<()> {
        if bps == self.config.bitrate_bps {
            return Ok(());
        }

        match &mut self.inner {
            ActiveEncoder::HwAccel(enc) => enc.reconfigure_bitrate(bps)?,
            ActiveEncoder::Software(enc) => enc.reconfigure_bitrate(bps)?,
        }
        self.config.bitrate_bps = bps;
        Ok(())
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
                {
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
            }
        };

        Ok(Self {
            config,
            inner,
            frame_count: 0,
            last_idr_at: Instant::now(),
            max_idr_interval: Duration::from_secs(5),
            pending_pli: false,
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
                ActiveEncoder::Software(_) => {
                    ActiveEncoder::Software(SwEncoder::new(&self.config)?)
                }
            };
        }

        self.pending_pli |= force_idr;
        let pli_idr = self.pending_pli && self.last_idr_at.elapsed() >= Self::MIN_PLI_IDR_INTERVAL;
        let intra_idr_supported = match &mut self.inner {
            ActiveEncoder::HwAccel(enc) => enc.supports_intra_refresh(),
            ActiveEncoder::Software(enc) => enc.supports_intra_refresh(),
        };
        let intra_refresh_active = self.config.intra_refresh && intra_idr_supported;
        let is_idr = if intra_refresh_active {
            // Rolling intra-refresh replaces periodic IDRs; only an explicit
            // request (PLI / reconnect) forces a full IDR, so the pipeline
            // never takes a recurring keyframe spike.
            pli_idr
        } else {
            pli_idr
                || self.frame_count % u64::from(self.config.gop.max(1)) == 0
                || self.last_idr_at.elapsed() >= self.max_idr_interval
        };

        let aus = match &mut self.inner {
            ActiveEncoder::HwAccel(enc) => enc.push_frame(frame, is_idr)?,
            ActiveEncoder::Software(enc) => enc.push_frame(frame, is_idr)?,
        };

        if is_idr && !aus.is_empty() {
            self.last_idr_at = Instant::now();
            self.pending_pli = false;
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
            crate::vlog!(
                "[encode] fps={fps:.1} stream_mbps={mbps:.2} idr={}/{} bitrate={}",
                self.stats_idr_count,
                self.stats_encoded,
                self.config.bitrate_bps
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

/// BGRA -> NV12/YUV420P conversion split into horizontal bands, each with
/// its own swscale context, converted in parallel. A single swscale context
/// runs on one core (~9 ms per 4K frame on a Ryzen 7 5850U, over half the
/// 60 fps budget); `sws_scale_frame`'s own threading would first copy the
/// non-refcounted capture buffer. There is no vertical scaling, so bands
/// with even row counts convert independently: NV12 output is identical to
/// a single context; YUV420P chroma may differ by 1 on each band's last row.
struct BandedScaler {
    /// (first row, row count, context) per band, top to bottom.
    bands: Vec<(i32, i32, *mut ffi::SwsContext)>,
    /// Destination planes after the first are vertically subsampled by 2.
    dst_planes: usize,
}

/// Raw pointers handed to a band worker thread. Each band touches a
/// disjoint range of rows in every plane.
struct BandJob {
    sws: *mut ffi::SwsContext,
    src: *const u8,
    src_stride: i32,
    dst: [*mut u8; 3],
    dst_stride: [i32; 3],
    rows: i32,
}

unsafe impl Send for BandJob {}

impl BandJob {
    unsafe fn run(&self) {
        unsafe {
            let src = [self.src, ptr::null(), ptr::null(), ptr::null()];
            let src_stride = [self.src_stride, 0, 0, 0];
            let dst = [self.dst[0], self.dst[1], self.dst[2], ptr::null_mut()];
            let dst_stride = [
                self.dst_stride[0],
                self.dst_stride[1],
                self.dst_stride[2],
                0,
            ];
            ffi::sws_scale(
                self.sws,
                src.as_ptr(),
                src_stride.as_ptr(),
                0,
                self.rows,
                dst.as_ptr(),
                dst_stride.as_ptr(),
            );
        }
    }
}

impl BandedScaler {
    /// Smallest band worth a thread of its own.
    const MIN_BAND_ROWS: i32 = 256;
    const MAX_BANDS: i32 = 4;

    fn new(width: i32, height: i32, dst_format: ffi::AVPixelFormat) -> Option<Self> {
        let dst_planes = match dst_format {
            ffi::AVPixelFormat::AV_PIX_FMT_NV12 => 2,
            ffi::AVPixelFormat::AV_PIX_FMT_YUV420P => 3,
            _ => return None,
        };
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as i32;
        let band_count = (height / Self::MIN_BAND_ROWS)
            .min(cores / 2)
            .clamp(1, Self::MAX_BANDS);
        // Even band heights keep each band's chroma rows self-contained.
        let band_rows = (height / band_count) & !1;

        let mut scaler = Self {
            bands: Vec::with_capacity(band_count as usize),
            dst_planes,
        };
        for band in 0..band_count {
            let first_row = band * band_rows;
            let rows = if band == band_count - 1 {
                height - first_row
            } else {
                band_rows
            };
            let sws = unsafe {
                ffi::sws_getContext(
                    width,
                    rows,
                    ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                    width,
                    rows,
                    dst_format,
                    1, // SWS_FAST_BILINEAR
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            if sws.is_null() {
                return None; // Drop frees the bands created so far
            }
            scaler.bands.push((first_row, rows, sws));
        }
        Some(scaler)
    }

    /// Same contract as `sws_scale` over the whole frame: one packed BGRA
    /// source plane, `dst_planes` destination planes.
    unsafe fn scale(
        &self,
        src: *const *const u8,
        src_stride: *const i32,
        dst: *const *mut u8,
        dst_stride: *const i32,
    ) {
        unsafe {
            let jobs: Vec<BandJob> = self
                .bands
                .iter()
                .map(|&(first_row, rows, sws)| {
                    let src_plane_stride = *src_stride;
                    let mut job = BandJob {
                        sws,
                        src: (*src).offset(first_row as isize * src_plane_stride as isize),
                        src_stride: src_plane_stride,
                        dst: [ptr::null_mut(); 3],
                        dst_stride: [0; 3],
                        rows,
                    };
                    for plane in 0..self.dst_planes {
                        let stride = *dst_stride.add(plane);
                        let plane_row = if plane == 0 { first_row } else { first_row / 2 };
                        job.dst[plane] =
                            (*dst.add(plane)).offset(plane_row as isize * stride as isize);
                        job.dst_stride[plane] = stride;
                    }
                    job
                })
                .collect();

            let mut jobs = jobs;
            let Some(last) = jobs.pop() else {
                return;
            };
            std::thread::scope(|scope| {
                for job in jobs {
                    scope.spawn(move || unsafe { job.run() });
                }
                last.run();
            });
        }
    }
}

impl Drop for BandedScaler {
    fn drop(&mut self) {
        for &(_, _, sws) in &self.bands {
            unsafe { ffi::sws_freeContext(sws) };
        }
    }
}

struct HwEncoder {
    kind: HwKind,
    ctx: *mut ffi::AVCodecContext,
    hw_device_ctx: *mut ffi::AVBufferRef,
    sws: BandedScaler,
    sw_bgra: *mut ffi::AVFrame,
    sw_nv12: *mut ffi::AVFrame,
    hw_frame: *mut ffi::AVFrame,
    mapped_frame: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    height: i32,
    frame_index: u64,
    fps: u32,
    extradata: Vec<u8>,
    is_cqp: bool,
}

unsafe impl Send for HwEncoder {}

#[derive(Clone, Copy)]
enum VaapiHevcMode {
    Bitrate,
    Cqp(i64),
}

/// Whether the given encoder exposes FFmpeg's `intra-refresh` private option.
/// Verified against FFmpeg 9: NVENC (H.264/H.265) exposes it; VA-API does
/// not, so it keeps periodic full IDRs.
fn hw_supports_intra_refresh(kind: HwKind) -> bool {
    matches!(kind, HwKind::Nvenc)
}

fn vaapi_hevc_qp(config: &EncoderConfig) -> i64 {
    // Some Intel VA-API HEVC drivers only expose CQP, so approximate the
    // user-provided bitrate target with a fixed QP chosen from bits/pixel.
    let pixels_per_second =
        (config.width.max(1) as f64) * (config.height.max(1) as f64) * (config.fps.max(1) as f64);
    let bits_per_pixel = config.bitrate_bps as f64 / pixels_per_second;

    if bits_per_pixel >= 0.18 {
        20
    } else if bits_per_pixel >= 0.12 {
        22
    } else if bits_per_pixel >= 0.08 {
        24
    } else if bits_per_pixel >= 0.05 {
        27
    } else if bits_per_pixel >= 0.03 {
        30
    } else {
        33
    }
}

impl HwEncoder {
    fn reconfigure_bitrate(&mut self, bps: u32) -> Result<()> {
        unsafe {
            let ctx = self.ctx;
            if ctx.is_null() {
                bail!("encoder context is null");
            }
            (*ctx).bit_rate = bps as i64;
            (*ctx).rc_max_rate = bps as i64;
            (*ctx).rc_buffer_size = (bps / self.fps.max(1) * 2).max(1) as i32;

            if matches!(self.kind, HwKind::Vaapi) && !self.is_cqp {
                let key = std::ffi::CString::new("b").unwrap();
                ffi::av_opt_set_int((*ctx).priv_data, key.as_ptr(), bps as i64, 0);
            }
        }
        Ok(())
    }

    fn supports_intra_refresh(&self) -> bool {
        hw_supports_intra_refresh(self.kind)
    }

    fn new_vaapi(config: &EncoderConfig) -> Result<Self> {
        if config.codec == VideoCodec::H265 {
            match Self::new_with_vaapi_hevc_mode(config, HwKind::Vaapi, VaapiHevcMode::Bitrate) {
                Ok(enc) => {
                    println!("[encode] VA-API HEVC using bitrate-based rate control");
                    Ok(enc)
                }
                Err(primary_err) => {
                    let qp = vaapi_hevc_qp(config);
                    eprintln!(
                        "[encode] VA-API HEVC bitrate mode unavailable, retrying with CQP (qp={qp}): {primary_err:#}"
                    );
                    Self::new_with_vaapi_hevc_mode(config, HwKind::Vaapi, VaapiHevcMode::Cqp(qp))
                        .with_context(|| {
                            format!(
                                "VA-API HEVC init failed in bitrate mode and CQP fallback; bitrate mode error: {primary_err:#}"
                            )
                        })
                }
            }
        } else {
            Self::new_with_vaapi_hevc_mode(config, HwKind::Vaapi, VaapiHevcMode::Bitrate)
        }
    }

    fn new_nvenc(config: &EncoderConfig) -> Result<Self> {
        Self::new_with_vaapi_hevc_mode(config, HwKind::Nvenc, VaapiHevcMode::Bitrate)
    }

    fn new_with_vaapi_hevc_mode(
        config: &EncoderConfig,
        kind: HwKind,
        vaapi_hevc_mode: VaapiHevcMode,
    ) -> Result<Self> {
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

        let use_intra_refresh = config.intra_refresh && hw_supports_intra_refresh(kind);

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
            let mut hw_frames_ref: *mut ffi::AVBufferRef = ptr::null_mut();

            {
                let device_cstr = std::ffi::CString::new(device_path).unwrap();
                let device = if device_path.is_empty() {
                    ptr::null()
                } else {
                    device_cstr.as_ptr()
                };
                let ret = ffi::av_hwdevice_ctx_create(
                    &mut hw_device_ctx,
                    hw_type,
                    device,
                    ptr::null_mut(),
                    0,
                );
                if ret < 0 {
                    ffi::avcodec_free_context(&mut (ctx as *mut _));
                    bail!("failed to create {kind_name} device context (error {ret})");
                }

                hw_frames_ref = ffi::av_hwframe_ctx_alloc(hw_device_ctx);
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
                (*ctx).pix_fmt = hw_pix_fmt;
                (*ctx).sw_pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            }

            (*ctx).width = width;
            (*ctx).height = height;
            (*ctx).time_base = ffi::AVRational {
                num: 1,
                den: 90_000,
            };
            (*ctx).framerate = ffi::AVRational {
                num: config.fps.max(1) as i32,
                den: 1,
            };
            (*ctx).gop_size = if use_intra_refresh {
                10_000
            } else {
                config.gop as i32
            };
            (*ctx).max_b_frames = 0;

            let is_vaapi_cqp = matches!(
                (kind, config.codec, vaapi_hevc_mode),
                (HwKind::Vaapi, VideoCodec::H265, VaapiHevcMode::Cqp(_))
            );

            if is_vaapi_cqp {
                (*ctx).bit_rate = 0;
                (*ctx).rc_max_rate = 0;
                (*ctx).rc_buffer_size = 0;
            } else {
                (*ctx).bit_rate = config.bitrate_bps as i64;
                (*ctx).rc_max_rate = config.bitrate_bps as i64;
                (*ctx).rc_buffer_size = (config.bitrate_bps / config.fps.max(1) * 2).max(1) as i32;
            }

            match kind {
                HwKind::Vaapi => {
                    let key = std::ffi::CString::new("async_depth").unwrap();
                    ffi::av_opt_set_int((*ctx).priv_data, key.as_ptr(), 1, 0);
                    if let VaapiHevcMode::Cqp(qp_value) = vaapi_hevc_mode {
                        let rc_mode = std::ffi::CString::new("rc_mode").unwrap();
                        let cqp = std::ffi::CString::new("CQP").unwrap();
                        ffi::av_opt_set((*ctx).priv_data, rc_mode.as_ptr(), cqp.as_ptr(), 0);

                        let qp = std::ffi::CString::new("qp").unwrap();
                        ffi::av_opt_set_int((*ctx).priv_data, qp.as_ptr(), qp_value, 0);
                        println!(
                            "[encode] VA-API HEVC using CQP mode (qp={qp_value}) to support Intel drivers"
                        );
                    }
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
                    if use_intra_refresh {
                        let intra_refresh = std::ffi::CString::new("intra-refresh").unwrap();
                        ffi::av_opt_set((*ctx).priv_data, intra_refresh.as_ptr(), one.as_ptr(), 0);
                    }
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

            let Some(sws) = BandedScaler::new(width, height, ffi::AVPixelFormat::AV_PIX_FMT_NV12)
            else {
                ffi::av_buffer_unref(&mut hw_device_ctx);
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to create swscale context");
            };

            let sw_bgra = ffi::av_frame_alloc();
            if sw_bgra.is_null() {
                bail!("failed to allocate BGRA frame");
            }
            (*sw_bgra).format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA as i32;
            (*sw_bgra).width = width;
            (*sw_bgra).height = height;
            (*sw_bgra).linesize[0] = width * 4;

            // sw_nv12 is only allocated if av_hwframe_map fails on the first
            // frame. Most drivers support mapping, so skipping the upfront
            // allocation saves memory and startup cost on the common path.
            let sw_nv12: *mut ffi::AVFrame = ptr::null_mut();

            let hw_frame = ffi::av_frame_alloc();
            if hw_frame.is_null() {
                bail!("failed to allocate HW frame");
            }

            let mapped_frame = ffi::av_frame_alloc();
            if mapped_frame.is_null() {
                bail!("failed to allocate mapped frame");
            }

            let pkt = ffi::av_packet_alloc();
            if pkt.is_null() {
                bail!("failed to allocate packet");
            }

            let is_cqp = matches!(
                (kind, config.codec, vaapi_hevc_mode),
                (HwKind::Vaapi, VideoCodec::H265, VaapiHevcMode::Cqp(_))
            );

            let codec_label = match config.codec {
                VideoCodec::H264 => "H.264",
                VideoCodec::H265 => "H.265",
            };

            println!(
                "[encode] {kind_name} {codec_label} encoder: {}x{}@{} bitrate={} gop={} intra_refresh={}",
                width, height, config.fps, config.bitrate_bps, config.gop, use_intra_refresh
            );

            Ok(Self {
                kind,
                ctx,
                hw_device_ctx,
                sws,
                sw_bgra,
                sw_nv12,
                hw_frame,
                mapped_frame,
                pkt,
                height,
                frame_index: 0,
                fps: config.fps.max(1),
                extradata,
                is_cqp,
            })
        }
    }

    fn push_frame(
        &mut self,
        frame: &CaptureFrame<'_>,
        is_idr: bool,
    ) -> Result<Vec<EncodedAccessUnit>> {
        unsafe {
            let ret = ffi::av_hwframe_get_buffer((*self.ctx).hw_frames_ctx, self.hw_frame, 0);
            if ret < 0 {
                bail!("failed to get hw frame buffer (error {ret})");
            }

            let map_flags = (ffi::AV_HWFRAME_MAP_WRITE as libc::c_int)
                | (ffi::AV_HWFRAME_MAP_OVERWRITE as libc::c_int);
            let mapped = ffi::av_hwframe_map(self.mapped_frame, self.hw_frame, map_flags);

            (*self.sw_bgra).data[0] = frame.data.as_ptr() as *mut u8;

            if mapped >= 0 {
                self.sws.scale(
                    (*self.sw_bgra).data.as_ptr() as *const *const u8,
                    (*self.sw_bgra).linesize.as_ptr(),
                    (*self.mapped_frame).data.as_mut_ptr(),
                    (*self.mapped_frame).linesize.as_mut_ptr(),
                );
                ffi::av_frame_unref(self.mapped_frame);
            } else {
                // Fallback for drivers that do not support direct mapping.
                // Allocate the NV12 system buffer lazily on first use.
                if self.sw_nv12.is_null() {
                    eprintln!("[encode] hwframe mapping unavailable, allocating sw_nv12 fallback");
                    self.sw_nv12 = ffi::av_frame_alloc();
                    if self.sw_nv12.is_null() {
                        ffi::av_frame_unref(self.mapped_frame);
                        ffi::av_frame_unref(self.hw_frame);
                        bail!("failed to allocate NV12 fallback frame");
                    }
                    (*self.sw_nv12).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
                    (*self.sw_nv12).width = (*self.ctx).width;
                    (*self.sw_nv12).height = (*self.ctx).height;
                    let ret = ffi::av_frame_get_buffer(self.sw_nv12, 0);
                    if ret < 0 {
                        ffi::av_frame_free(&mut self.sw_nv12);
                        ffi::av_frame_unref(self.mapped_frame);
                        ffi::av_frame_unref(self.hw_frame);
                        bail!("failed to allocate NV12 fallback buffer (error {ret})");
                    }
                }

                ffi::av_frame_unref(self.mapped_frame);
                ffi::av_frame_unref(self.hw_frame);

                self.sws.scale(
                    (*self.sw_bgra).data.as_ptr() as *const *const u8,
                    (*self.sw_bgra).linesize.as_ptr(),
                    (*self.sw_nv12).data.as_mut_ptr(),
                    (*self.sw_nv12).linesize.as_mut_ptr(),
                );

                let ret = ffi::av_hwframe_get_buffer((*self.ctx).hw_frames_ctx, self.hw_frame, 0);
                if ret < 0 {
                    bail!("failed to get hw frame buffer (error {ret})");
                }

                let ret = ffi::av_hwframe_transfer_data(self.hw_frame, self.sw_nv12, 0);
                if ret < 0 {
                    ffi::av_frame_unref(self.hw_frame);
                    bail!("failed to upload frame to hw surface (error {ret})");
                }
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

                let is_key = ((*self.pkt).flags & ffi::AV_PKT_FLAG_KEY) != 0;

                let annex_b = if is_key && !self.extradata.is_empty() {
                    let encoded =
                        std::slice::from_raw_parts((*self.pkt).data, (*self.pkt).size as usize);
                    let mut buf = Vec::with_capacity(self.extradata.len() + encoded.len());
                    buf.extend_from_slice(&self.extradata);
                    buf.extend_from_slice(encoded);
                    AnnexB::Vec(buf)
                } else {
                    match OwnedPacketBuf::from_packet(self.pkt) {
                        Some(pkt_buf) => AnnexB::Packet(pkt_buf),
                        None => {
                            let encoded = std::slice::from_raw_parts(
                                (*self.pkt).data,
                                (*self.pkt).size as usize,
                            );
                            AnnexB::Vec(encoded.to_vec())
                        }
                    }
                };

                output.push(EncodedAccessUnit {
                    is_idr: is_key,
                    annex_b,
                });
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
            if !self.mapped_frame.is_null() {
                ffi::av_frame_free(&mut self.mapped_frame);
            }
            if !self.sw_nv12.is_null() {
                ffi::av_frame_free(&mut self.sw_nv12);
            }
            if !self.sw_bgra.is_null() {
                ffi::av_frame_free(&mut self.sw_bgra);
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
    sws: BandedScaler,
    sw_bgra: *mut ffi::AVFrame,
    sw_yuv: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    height: i32,
    frame_index: u64,
    fps: u32,
    codec: VideoCodec,
    extradata: Vec<u8>,
}

unsafe impl Send for SwEncoder {}

impl SwEncoder {
    fn reconfigure_bitrate(&mut self, bps: u32) -> Result<()> {
        unsafe {
            let ctx = self.ctx;
            if ctx.is_null() {
                bail!("encoder context is null");
            }
            (*ctx).bit_rate = bps as i64;
            (*ctx).rc_max_rate = bps as i64;
            (*ctx).rc_buffer_size = (bps / self.fps.max(1) * 2).max(1) as i32;
        }
        Ok(())
    }

    fn supports_intra_refresh(&self) -> bool {
        self.codec == VideoCodec::H264
    }

    fn new(config: &EncoderConfig) -> Result<Self> {
        let width = config.width as i32;
        let height = config.height as i32;

        let use_intra_refresh = config.intra_refresh && config.codec == VideoCodec::H264;

        let (enc_name, opts): (&str, Vec<(&str, &str)>) = match config.codec {
            VideoCodec::H264 => {
                let mut opts = vec![
                    ("preset", "ultrafast"),
                    ("tune", "zerolatency"),
                    ("forced-idr", "1"),
                    ("profile", "baseline"),
                ];
                if use_intra_refresh {
                    opts.push(("intra-refresh", "1"));
                }
                ("libx264", opts)
            }
            VideoCodec::H265 => (
                "libx265",
                vec![
                    ("preset", "ultrafast"),
                    ("tune", "zerolatency"),
                    ("forced-idr", "1"),
                ],
            ),
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
            (*ctx).time_base = ffi::AVRational {
                num: 1,
                den: 90_000,
            };
            (*ctx).framerate = ffi::AVRational {
                num: config.fps.max(1) as i32,
                den: 1,
            };
            (*ctx).bit_rate = config.bitrate_bps as i64;
            (*ctx).rc_max_rate = config.bitrate_bps as i64;
            (*ctx).rc_buffer_size = (config.bitrate_bps / config.fps.max(1) * 2).max(1) as i32;
            // Intra-refresh replaces periodic IDRs with a rolling intra column,
            // so a near-infinite GOP is safe (full IDRs are only PLI-triggered).
            (*ctx).gop_size = if use_intra_refresh {
                10_000
            } else {
                config.gop as i32
            };
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
                std::slice::from_raw_parts((*ctx).extradata, (*ctx).extradata_size as usize)
                    .to_vec()
            } else {
                Vec::new()
            };

            let Some(sws) =
                BandedScaler::new(width, height, ffi::AVPixelFormat::AV_PIX_FMT_YUV420P)
            else {
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to create swscale context");
            };

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
                "[encode] software encoder ({enc_name}): {}x{}@{} bitrate={} gop={} intra_refresh={}",
                width, height, config.fps, config.bitrate_bps, config.gop, use_intra_refresh
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
                codec: config.codec,
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
            self.sws.scale(
                (*self.sw_bgra).data.as_ptr() as *const *const u8,
                (*self.sw_bgra).linesize.as_ptr(),
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

                let is_key = ((*self.pkt).flags & ffi::AV_PKT_FLAG_KEY) != 0;

                let annex_b = if is_key && !self.extradata.is_empty() {
                    let encoded =
                        std::slice::from_raw_parts((*self.pkt).data, (*self.pkt).size as usize);
                    let mut buf = Vec::with_capacity(self.extradata.len() + encoded.len());
                    buf.extend_from_slice(&self.extradata);
                    buf.extend_from_slice(encoded);
                    AnnexB::Vec(buf)
                } else {
                    match OwnedPacketBuf::from_packet(self.pkt) {
                        Some(pkt_buf) => AnnexB::Packet(pkt_buf),
                        None => {
                            let encoded = std::slice::from_raw_parts(
                                (*self.pkt).data,
                                (*self.pkt).size as usize,
                            );
                            AnnexB::Vec(encoded.to_vec())
                        }
                    }
                };

                output.push(EncodedAccessUnit {
                    is_idr: is_key,
                    annex_b,
                });
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
            if !self.ctx.is_null() {
                ffi::avcodec_free_context(&mut self.ctx);
            }
        }
    }
}

#[cfg(test)]
mod banded_scaler_tests {
    use super::*;

    fn test_pattern(width: usize, height: usize) -> Vec<u8> {
        let mut bgra = vec![0u8; width * height * 4];
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % width, i / width);
            px[0] = (x * 7 + y * 3) as u8;
            px[1] = (x ^ y) as u8;
            px[2] = (y * 5 + (x >> 2)) as u8;
            px[3] = 255;
        }
        bgra
    }

    /// Converts with one full-frame context and with `BandedScaler`, and
    /// returns the per-plane outputs of both.
    fn convert_both(
        width: i32,
        height: i32,
        format: ffi::AVPixelFormat,
    ) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let bgra = test_pattern(width as usize, height as usize);
        let planes = if format == ffi::AVPixelFormat::AV_PIX_FMT_NV12 {
            2
        } else {
            3
        };
        let strides: Vec<i32> = (0..planes)
            .map(|p| match (planes, p) {
                (_, 0) => width,
                (2, _) => width,
                _ => width / 2,
            })
            .collect();
        let plane_rows = |p: usize| if p == 0 { height } else { height / 2 };
        let alloc = || -> Vec<Vec<u8>> {
            (0..planes)
                .map(|p| vec![0u8; (strides[p] * plane_rows(p)) as usize])
                .collect()
        };
        let src = [bgra.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
        let src_stride = [width * 4, 0, 0, 0];
        let mut dst_stride = [0i32; 4];
        dst_stride[..planes].copy_from_slice(&strides);

        let mut reference = alloc();
        let mut banded = alloc();
        unsafe {
            let sws = ffi::sws_getContext(
                width,
                height,
                ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                width,
                height,
                format,
                1,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            assert!(!sws.is_null());
            let mut dst = [ptr::null_mut::<u8>(); 4];
            for p in 0..planes {
                dst[p] = reference[p].as_mut_ptr();
            }
            ffi::sws_scale(
                sws,
                src.as_ptr(),
                src_stride.as_ptr(),
                0,
                height,
                dst.as_ptr(),
                dst_stride.as_ptr(),
            );
            ffi::sws_freeContext(sws);

            let scaler = BandedScaler::new(width, height, format).expect("banded scaler");
            for p in 0..planes {
                dst[p] = banded[p].as_mut_ptr();
            }
            scaler.scale(
                src.as_ptr(),
                src_stride.as_ptr(),
                dst.as_ptr(),
                dst_stride.as_ptr(),
            );
        }
        (reference, banded)
    }

    #[test]
    fn banded_nv12_matches_single_context() {
        for (w, h) in [(1920, 1080), (1280, 720), (2732, 2048), (640, 360)] {
            let (reference, banded) = convert_both(w, h, ffi::AVPixelFormat::AV_PIX_FMT_NV12);
            assert!(reference == banded, "NV12 mismatch at {w}x{h}");
        }
    }

    /// swscale's YUV420P path clamps its vertical chroma filter at a band's
    /// last row, so the chroma row at each band boundary may differ by 1.
    #[test]
    fn banded_yuv420p_within_one_at_band_edges() {
        let (w, h) = (1920, 1080);
        let (reference, banded) = convert_both(w, h, ffi::AVPixelFormat::AV_PIX_FMT_YUV420P);
        assert!(reference[0] == banded[0], "luma must match exactly");
        let scaler = BandedScaler::new(w, h, ffi::AVPixelFormat::AV_PIX_FMT_YUV420P).unwrap();
        let edge_rows: Vec<usize> = scaler.bands[..scaler.bands.len() - 1]
            .iter()
            .map(|&(first, rows, _)| ((first + rows) / 2 - 1) as usize)
            .collect();
        for p in 1..3 {
            for (i, (&a, &b)) in reference[p].iter().zip(&banded[p]).enumerate() {
                if a != b {
                    assert!(
                        edge_rows.contains(&(i / (w as usize / 2))),
                        "plane {p} differs off a band edge"
                    );
                    assert!(
                        (a as i32 - b as i32).abs() <= 1,
                        "plane {p} differs by more than 1"
                    );
                }
            }
        }
    }

    /// Timing only; run with `cargo test --release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn banded_scaler_speed_4k() {
        let (w, h) = (3840, 2160);
        let bgra = test_pattern(w as usize, h as usize);
        let mut y = vec![0u8; (w * h) as usize];
        let mut uv = vec![0u8; (w * h / 2) as usize];
        let src = [bgra.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
        let src_stride = [w * 4, 0, 0, 0];
        let dst = [
            y.as_mut_ptr(),
            uv.as_mut_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
        ];
        let dst_stride = [w, w, 0, 0];
        unsafe {
            let single = ffi::sws_getContext(
                w,
                h,
                ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                w,
                h,
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                1,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            let banded = BandedScaler::new(w, h, ffi::AVPixelFormat::AV_PIX_FMT_NV12).unwrap();
            let iterations = 60;
            let start = Instant::now();
            for _ in 0..iterations {
                ffi::sws_scale(
                    single,
                    src.as_ptr(),
                    src_stride.as_ptr(),
                    0,
                    h,
                    dst.as_ptr(),
                    dst_stride.as_ptr(),
                );
            }
            let single_ms = start.elapsed().as_secs_f64() * 1000.0 / iterations as f64;
            let start = Instant::now();
            for _ in 0..iterations {
                banded.scale(
                    src.as_ptr(),
                    src_stride.as_ptr(),
                    dst.as_ptr(),
                    dst_stride.as_ptr(),
                );
            }
            let banded_ms = start.elapsed().as_secs_f64() * 1000.0 / iterations as f64;
            ffi::sws_freeContext(single);
            println!(
                "4K BGRA->NV12: single={single_ms:.2} ms banded({} bands)={banded_ms:.2} ms",
                banded.bands.len()
            );
        }
    }
}
