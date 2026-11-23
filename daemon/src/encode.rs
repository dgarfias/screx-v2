use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::ptr;
use std::time::{Duration, Instant};


use anyhow::{bail, Context, Result};
use ffmpeg_sys_next as ffi;

use crate::capture::{CaptureFrame, DmaBufFrame, FrameData};

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
    /// VA-API frames the encoder may pipeline at once.
    pub vaapi_async_depth: u32,
    /// Convert captured BGRA frames to NV12 on the GPU (VA-API VPP). False
    /// keeps the CPU scaler, as on a driver without video post-processing.
    pub gpu_convert: bool,
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

    pub fn supports_dmabuf(&self) -> bool {
        self.config.gpu_convert
            && matches!(
                &self.inner,
                ActiveEncoder::HwAccel(enc) if matches!(enc.kind, HwKind::Vaapi)
            )
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
                        crate::log_detail!("[encode] auto-selected: vaapi ({codec_name})");
                        ActiveEncoder::HwAccel(enc)
                    } else if let Ok(enc) = HwEncoder::new_nvenc(&config) {
                        crate::log_detail!("[encode] auto-selected: nvenc ({codec_name})");
                        ActiveEncoder::HwAccel(enc)
                    } else {
                        crate::log_detail!(
                            "[encode] no hw encoder available, using software ({codec_name})"
                        );
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
            crate::log_detail!(
                "[encode] resolution change: {}x{} -> {}x{}",
                self.config.width,
                self.config.height,
                frame.width,
                frame.height
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
        let is_idr = if intra_refresh_active || self.config.gop == 0 {
            // Rolling intra-refresh or infinite-GOP (PLI-only) mode: emit an IDR
            // on the very first frame and on explicit client PLIs, avoiding
            // periodic keyframe spikes on constrained links.
            self.frame_count == 0 || pli_idr
        } else {
            self.frame_count == 0
                || pli_idr
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
            crate::log_trace!(
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
            ffi::AVPixelFormat::AV_PIX_FMT_NV12 | ffi::AVPixelFormat::AV_PIX_FMT_P010LE => 2,
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
            // Match the encoder's BT.709 limited-range (MPEG/TV) signaling.
            unsafe {
                let coeffs = ffi::sws_getCoefficients(ffi::SWS_CS_ITU709 as libc::c_int);
                ffi::sws_setColorspaceDetails(
                    sws,
                    coeffs,
                    1, // full-range RGB source
                    coeffs,
                    0, // limited-range (MPEG/TV) YUV destination
                    0,
                    1 << 16,
                    1 << 16,
                );
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
                    scope.spawn(move || job.run());
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
    sw_pix_fmt: ffi::AVPixelFormat,
    config: EncoderConfig,
    vaapi_rc_mode: VaapiHevcMode,
    last_reopen_at: Instant,
    ctx: *mut ffi::AVCodecContext,
    hw_device_ctx: *mut ffi::AVBufferRef,
    hw_frames_ctx: *mut ffi::AVBufferRef,
    sws: BandedScaler,
    /// GPU colour conversion: CPU BGRA -> VA-API NV12 via hwupload + scale_vaapi.
    vpp: Option<VppScaler>,
    /// True zero-copy GPU conversion: DRM_PRIME DMA-BUF -> VA-API NV12 via
    /// av_hwframe_map + scale_vaapi (no CPU readback or hwupload).
    dmabuf_vpp: Option<DmaBufVppScaler>,
    sw_bgra: *mut ffi::AVFrame,
    sw_nv12: *mut ffi::AVFrame,
    hw_frame: *mut ffi::AVFrame,
    mapped_frame: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    frame_index: u64,
    fps: u32,
    extradata: Vec<u8>,
    is_cqp: bool,
}

unsafe impl Send for HwEncoder {}

/// Converts captured BGRA frames straight into VA-API NV12 surfaces with the
/// video processor, so the capture thread never touches pixel data. Pulled
/// frames go to the encoder as they are; the graph uses the same VA-API device
/// as the encoder, so its surfaces are valid there.
struct VppScaler {
    graph: *mut ffi::AVFilterGraph,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
}

unsafe impl Send for VppScaler {}

impl VppScaler {
    fn new(
        width: i32,
        height: i32,
        dst_format: ffi::AVPixelFormat,
        device: *mut ffi::AVBufferRef,
    ) -> Option<Self> {
        unsafe {
            let graph = ffi::avfilter_graph_alloc();
            if graph.is_null() {
                return None;
            }
            let mut scaler = Self { graph, src: ptr::null_mut(), sink: ptr::null_mut() };
            let create = |name: &str, args: Option<&str>, label: &str| {
                let filter = std::ffi::CString::new(name).ok()?;
                let label = std::ffi::CString::new(label).ok()?;
                let args = match args {
                    Some(args) => Some(std::ffi::CString::new(args).ok()?),
                    None => None,
                };
                let mut context = ptr::null_mut();
                let ret = ffi::avfilter_graph_create_filter(
                    &mut context,
                    ffi::avfilter_get_by_name(filter.as_ptr()),
                    label.as_ptr(),
                    args.as_ref().map_or(ptr::null(), |args| args.as_ptr()),
                    ptr::null_mut(),
                    scaler.graph,
                );
                (ret >= 0).then_some(context)
            };
            let source = format!(
                "video_size={width}x{height}:pix_fmt=bgra:time_base=1/90000:pixel_aspect=1/1"
            );
            let fmt_name = match dst_format {
                ffi::AVPixelFormat::AV_PIX_FMT_P010LE => "p010le",
                _ => "nv12",
            };
            let convert = format!("format={fmt_name}:out_color_matrix=bt709:out_range=tv");
            let (Some(src), Some(convert), Some(sink)) = (
                create("buffer", Some(&source), "in"),
                create("scale_vaapi", Some(&convert), "convert"),
                create("buffersink", None, "out"),
            ) else {
                scaler.free();
                return None;
            };
            // Upload into the encoder's own VA-API device: surface IDs are only
            // valid on the device context that created them. The filter must be
            // allocated, given the device and only then initialized.
            let upload_filter = ffi::avfilter_get_by_name(c"hwupload".as_ptr());
            let upload_label = c"upload".as_ptr();
            let upload = if upload_filter.is_null() {
                ptr::null_mut()
            } else {
                ffi::avfilter_graph_alloc_filter(scaler.graph, upload_filter, upload_label)
            };
            if upload.is_null() {
                scaler.free();
                return None;
            }
            (*upload).hw_device_ctx = ffi::av_buffer_ref(device);
            if (*upload).hw_device_ctx.is_null()
                || ffi::avfilter_init_str(upload, ptr::null()) < 0
            {
                scaler.free();
                return None;
            }
            scaler.src = src;
            scaler.sink = sink;
            let linked = ffi::avfilter_link(src, 0, upload, 0) >= 0
                && ffi::avfilter_link(upload, 0, convert, 0) >= 0
                && ffi::avfilter_link(convert, 0, sink, 0) >= 0
                && ffi::avfilter_graph_config(scaler.graph, ptr::null_mut()) >= 0;
            if !linked {
                scaler.free();
                return None;
            }
            Some(scaler)
        }
    }

    /// Converts one BGRA frame and moves the NV12 surface into `out`.
    fn convert(&mut self, bgra: *mut ffi::AVFrame, out: *mut ffi::AVFrame) -> Result<()> {
        unsafe {
            let ret = ffi::av_buffersrc_add_frame_flags(
                self.src,
                bgra,
                ffi::AV_BUFFERSRC_FLAG_KEEP_REF as libc::c_int,
            );
            if ret < 0 {
                bail!("VPP input rejected (error {ret})");
            }
            let ret = ffi::av_buffersink_get_frame(self.sink, out);
            if ret < 0 {
                bail!("VPP produced no frame (error {ret})");
            }
            Ok(())
        }
    }

    fn free(&mut self) {
        unsafe {
            if !self.graph.is_null() {
                ffi::avfilter_graph_free(&mut self.graph);
            }
        }
    }
}

impl Drop for VppScaler {
    fn drop(&mut self) {
        self.free();
    }
}

fn drm_fourcc_to_sw_pix_fmt(fourcc: u32) -> Option<ffi::AVPixelFormat> {
    const fn fourcc_code(a: u8, b: u8, c: u8, d: u8) -> u32 {
        (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
    }
    const DRM_FORMAT_XRGB8888: u32 = fourcc_code(b'X', b'R', b'2', b'4');
    const DRM_FORMAT_ARGB8888: u32 = fourcc_code(b'A', b'R', b'2', b'4');
    const DRM_FORMAT_XBGR8888: u32 = fourcc_code(b'X', b'B', b'2', b'4');
    const DRM_FORMAT_ABGR8888: u32 = fourcc_code(b'A', b'B', b'2', b'4');

    match fourcc {
        DRM_FORMAT_XRGB8888 => Some(ffi::AVPixelFormat::AV_PIX_FMT_BGR0),
        DRM_FORMAT_ARGB8888 => Some(ffi::AVPixelFormat::AV_PIX_FMT_BGRA),
        DRM_FORMAT_XBGR8888 => Some(ffi::AVPixelFormat::AV_PIX_FMT_RGB0),
        DRM_FORMAT_ABGR8888 => Some(ffi::AVPixelFormat::AV_PIX_FMT_RGBA),
        _ => None,
    }
}

unsafe extern "C" fn free_drm_frame_descriptor(_opaque: *mut libc::c_void, data: *mut u8) {
    if data.is_null() {
        return;
    }
    let desc = Box::from_raw(data as *mut ffi::AVDRMFrameDescriptor);
    let n = (desc.nb_objects as usize).min(desc.objects.len());
    for obj in &desc.objects[..n] {
        if obj.fd >= 0 {
            libc::close(obj.fd);
        }
    }
}

#[repr(C)]
struct ScrexVaVpp {
    _private: [u8; 0],
}

extern "C" {
    fn screx_va_vpp_create(
        va_display: *mut libc::c_void,
        width: libc::c_int,
        height: libc::c_int,
    ) -> *mut ScrexVaVpp;
    fn screx_va_vpp_destroy(vpp: *mut ScrexVaVpp);
    fn screx_va_vpp_compose(
        vpp: *mut ScrexVaVpp,
        dst_surface: u32,
        primary_surface: u32,
        frame_w: libc::c_int,
        frame_h: libc::c_int,
        has_cursor: libc::c_int,
        cursor_surface: u32,
        cursor_x: libc::c_int,
        cursor_y: libc::c_int,
        cursor_src_w: libc::c_int,
        cursor_src_h: libc::c_int,
        cursor_dst_w: libc::c_int,
        cursor_dst_h: libc::c_int,
    ) -> libc::c_int;
}

/// Imports a DRM PRIME DMA-BUF directly into a VA-API surface via
/// `av_hwframe_map` and runs single-pass VA-API VPP (`VAEntrypointVideoProc`)
/// directly into the encoder's NV12 surface pool, alpha-blending the DRM
/// hardware cursor plane on the GPU when present.
struct DmaBufVppScaler {
    width: i32,
    height: i32,
    sw_format: ffi::AVPixelFormat,
    vaapi_device_ctx: *mut ffi::AVBufferRef,
    drm_device_ctx: *mut ffi::AVBufferRef,
    drm_frames_ctx: *mut ffi::AVBufferRef,
    vaapi_in_frames_ctx: *mut ffi::AVBufferRef,
    va_vpp: *mut ScrexVaVpp,
    drm_frame: *mut ffi::AVFrame,
    vaapi_in_frame: *mut ffi::AVFrame,
    cursor_key: Option<(i32, i32, ffi::AVPixelFormat)>,
    cursor_drm_frames_ctx: *mut ffi::AVBufferRef,
    cursor_vaapi_frames_ctx: *mut ffi::AVBufferRef,
    cursor_drm_frame: *mut ffi::AVFrame,
    cursor_vaapi_frame: *mut ffi::AVFrame,
}

unsafe impl Send for DmaBufVppScaler {}

impl DmaBufVppScaler {
    fn new(
        width: i32,
        height: i32,
        sw_format: ffi::AVPixelFormat,
        vaapi_device_ctx: *mut ffi::AVBufferRef,
    ) -> Result<Self> {
        unsafe {
            let mut scaler = Self {
                width,
                height,
                sw_format,
                vaapi_device_ctx: ffi::av_buffer_ref(vaapi_device_ctx),
                drm_device_ctx: ptr::null_mut(),
                drm_frames_ctx: ptr::null_mut(),
                vaapi_in_frames_ctx: ptr::null_mut(),
                va_vpp: ptr::null_mut(),
                drm_frame: ptr::null_mut(),
                vaapi_in_frame: ptr::null_mut(),
                cursor_key: None,
                cursor_drm_frames_ctx: ptr::null_mut(),
                cursor_vaapi_frames_ctx: ptr::null_mut(),
                cursor_drm_frame: ptr::null_mut(),
                cursor_vaapi_frame: ptr::null_mut(),
            };
            if scaler.vaapi_device_ctx.is_null() {
                bail!("failed to ref VA-API device context");
            }

            let va_dev_ctx = (*vaapi_device_ctx).data as *mut ffi::AVHWDeviceContext;
            let va_hwctx = (*va_dev_ctx).hwctx as *mut ffi::AVVAAPIDeviceContext;
            let va_vpp = screx_va_vpp_create((*va_hwctx).display, width, height);
            if va_vpp.is_null() {
                bail!("screx_va_vpp_create failed for {width}x{height}");
            }
            scaler.va_vpp = va_vpp;

            let drm_dev = ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM);
            if drm_dev.is_null() {
                bail!("failed to allocate DRM hwdevice context");
            }
            scaler.drm_device_ctx = drm_dev;
            let dev_ctx = (*drm_dev).data as *mut ffi::AVHWDeviceContext;
            let drm_hwctx = (*dev_ctx).hwctx as *mut ffi::AVDRMDeviceContext;
            (*drm_hwctx).fd = -1;
            let ret = ffi::av_hwdevice_ctx_init(drm_dev);
            if ret < 0 {
                bail!("av_hwdevice_ctx_init(DRM) failed (error {ret})");
            }

            let drm_frames = ffi::av_hwframe_ctx_alloc(drm_dev);
            if drm_frames.is_null() {
                bail!("failed to allocate DRM hwframes context");
            }
            scaler.drm_frames_ctx = drm_frames;
            let fc = (*drm_frames).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
            (*fc).sw_format = sw_format;
            (*fc).width = width;
            (*fc).height = height;
            (*fc).initial_pool_size = 0;
            let ret = ffi::av_hwframe_ctx_init(drm_frames);
            if ret < 0 {
                bail!("av_hwframe_ctx_init(DRM_PRIME) failed (error {ret})");
            }

            let ret = ffi::av_hwframe_ctx_create_derived(
                &mut scaler.vaapi_in_frames_ctx,
                ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
                vaapi_device_ctx,
                drm_frames,
                0,
            );
            if ret < 0 || scaler.vaapi_in_frames_ctx.is_null() {
                bail!("av_hwframe_ctx_create_derived(VAAPI <- DRM_PRIME) failed (error {ret})");
            }

            scaler.drm_frame = ffi::av_frame_alloc();
            scaler.vaapi_in_frame = ffi::av_frame_alloc();
            scaler.cursor_drm_frame = ffi::av_frame_alloc();
            scaler.cursor_vaapi_frame = ffi::av_frame_alloc();
            if scaler.drm_frame.is_null()
                || scaler.vaapi_in_frame.is_null()
                || scaler.cursor_drm_frame.is_null()
                || scaler.cursor_vaapi_frame.is_null()
            {
                bail!("failed to allocate zero-copy VPP scratch frames");
            }

            crate::log_detail!(
                "[encode] VA-API zero-copy DMA-BUF VPP initialized ({width}x{height} sw_fmt={sw_format:?}, GPU cursor blend enabled)"
            );
            Ok(scaler)
        }
    }

    fn matches(&self, width: i32, height: i32, sw_format: ffi::AVPixelFormat) -> bool {
        self.width == width && self.height == height && self.sw_format == sw_format
    }

    unsafe fn ensure_cursor_frames(
        &mut self,
        width: i32,
        height: i32,
        sw_format: ffi::AVPixelFormat,
    ) -> Result<()> {
        let key = (width, height, sw_format);
        if self.cursor_key == Some(key)
            && !self.cursor_drm_frames_ctx.is_null()
            && !self.cursor_vaapi_frames_ctx.is_null()
        {
            return Ok(());
        }
        if !self.cursor_vaapi_frames_ctx.is_null() {
            ffi::av_buffer_unref(&mut self.cursor_vaapi_frames_ctx);
        }
        if !self.cursor_drm_frames_ctx.is_null() {
            ffi::av_buffer_unref(&mut self.cursor_drm_frames_ctx);
        }
        self.cursor_key = None;

        let drm_frames = ffi::av_hwframe_ctx_alloc(self.drm_device_ctx);
        if drm_frames.is_null() {
            bail!("failed to allocate cursor DRM hwframes context");
        }
        self.cursor_drm_frames_ctx = drm_frames;
        let fc = (*drm_frames).data as *mut ffi::AVHWFramesContext;
        (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
        (*fc).sw_format = sw_format;
        (*fc).width = width;
        (*fc).height = height;
        (*fc).initial_pool_size = 0;
        let ret = ffi::av_hwframe_ctx_init(drm_frames);
        if ret < 0 {
            ffi::av_buffer_unref(&mut self.cursor_drm_frames_ctx);
            bail!("av_hwframe_ctx_init(cursor DRM_PRIME) failed (error {ret})");
        }

        let ret = ffi::av_hwframe_ctx_create_derived(
            &mut self.cursor_vaapi_frames_ctx,
            ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
            self.vaapi_device_ctx,
            drm_frames,
            0,
        );
        if ret < 0 || self.cursor_vaapi_frames_ctx.is_null() {
            ffi::av_buffer_unref(&mut self.cursor_vaapi_frames_ctx);
            ffi::av_buffer_unref(&mut self.cursor_drm_frames_ctx);
            bail!("av_hwframe_ctx_create_derived(cursor VAAPI) failed (error {ret})");
        }

        self.cursor_key = Some(key);
        Ok(())
    }

    fn convert(
        &mut self,
        dmabuf: &DmaBufFrame,
        pts: i64,
        enc_hw_frames_ctx: *mut ffi::AVBufferRef,
        out: *mut ffi::AVFrame,
    ) -> Result<()> {
        if dmabuf.objects.is_empty() || dmabuf.objects.len() > 4 {
            bail!("invalid DMA-BUF object count: {}", dmabuf.objects.len());
        }
        if dmabuf.planes.is_empty() || dmabuf.planes.len() > 4 {
            bail!("invalid DMA-BUF plane count: {}", dmabuf.planes.len());
        }

        unsafe {
            // 1. Import primary plane DMA-BUF into VA-API surface.
            let mut desc = Box::new(std::mem::zeroed::<ffi::AVDRMFrameDescriptor>());
            for obj in &mut desc.objects {
                obj.fd = -1;
            }
            desc.nb_objects = dmabuf.objects.len() as i32;
            for (i, obj) in dmabuf.objects.iter().enumerate() {
                let dup_fd = libc::fcntl(obj.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0);
                if dup_fd < 0 {
                    for prev in &desc.objects[..i] {
                        if prev.fd >= 0 {
                            libc::close(prev.fd);
                        }
                    }
                    bail!("F_DUPFD_CLOEXEC failed for DMA-BUF object {i}");
                }
                desc.objects[i] = ffi::AVDRMObjectDescriptor {
                    fd: dup_fd,
                    size: obj.size as usize,
                    format_modifier: obj.modifier,
                };
            }

            desc.nb_layers = 1;
            desc.layers[0].format = dmabuf.pixel_format;
            desc.layers[0].nb_planes = dmabuf.planes.len() as i32;
            for (p, plane) in dmabuf.planes.iter().enumerate() {
                desc.layers[0].planes[p] = ffi::AVDRMPlaneDescriptor {
                    object_index: plane.object_index as i32,
                    offset: plane.offset as isize,
                    pitch: plane.pitch as isize,
                };
            }

            let desc_ptr = Box::into_raw(desc);
            let buf_ref = ffi::av_buffer_create(
                desc_ptr as *mut u8,
                std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
                Some(free_drm_frame_descriptor),
                ptr::null_mut(),
                ffi::AV_BUFFER_FLAG_READONLY as libc::c_int,
            );
            if buf_ref.is_null() {
                free_drm_frame_descriptor(ptr::null_mut(), desc_ptr as *mut u8);
                bail!("av_buffer_create failed for AVDRMFrameDescriptor");
            }

            ffi::av_frame_unref(self.drm_frame);
            (*self.drm_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            (*self.drm_frame).width = self.width;
            (*self.drm_frame).height = self.height;
            (*self.drm_frame).hw_frames_ctx = ffi::av_buffer_ref(self.drm_frames_ctx);
            (*self.drm_frame).buf[0] = buf_ref;
            (*self.drm_frame).data[0] = (*buf_ref).data;
            (*self.drm_frame).pts = pts;

            ffi::av_frame_unref(self.vaapi_in_frame);
            (*self.vaapi_in_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
            (*self.vaapi_in_frame).hw_frames_ctx = ffi::av_buffer_ref(self.vaapi_in_frames_ctx);

            let map_ret = ffi::av_hwframe_map(
                self.vaapi_in_frame,
                self.drm_frame,
                ffi::AV_HWFRAME_MAP_READ as libc::c_int,
            );
            ffi::av_frame_unref(self.drm_frame);
            if map_ret < 0 {
                ffi::av_frame_unref(self.vaapi_in_frame);
                bail!("av_hwframe_map(DRM_PRIME -> VAAPI) failed (error {map_ret})");
            }
            let primary_surface = (*self.vaapi_in_frame).data[3] as usize as u32;

            // 2. Best-effort import of hardware cursor plane DMA-BUF.
            let mut has_cursor = 0;
            let mut cursor_surface = 0u32;
            let mut cursor_x = 0i32;
            let mut cursor_y = 0i32;
            let mut cursor_src_w = 0i32;
            let mut cursor_src_h = 0i32;
            let mut cursor_dst_w = 0i32;
            let mut cursor_dst_h = 0i32;

            if let Some(cursor) = &dmabuf.cursor {
                if let Some(c_sw_fmt) = drm_fourcc_to_sw_pix_fmt(cursor.pixel_format) {
                    let cw = cursor.width as i32;
                    let ch = cursor.height as i32;
                    if self.ensure_cursor_frames(cw, ch, c_sw_fmt).is_ok() {
                        let dup_fd = libc::fcntl(cursor.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0);
                        if dup_fd >= 0 {
                            let mut cdesc =
                                Box::new(std::mem::zeroed::<ffi::AVDRMFrameDescriptor>());
                            for obj in &mut cdesc.objects {
                                obj.fd = -1;
                            }
                            cdesc.nb_objects = 1;
                            cdesc.objects[0] = ffi::AVDRMObjectDescriptor {
                                fd: dup_fd,
                                size: cursor.size as usize,
                                format_modifier: cursor.modifier,
                            };
                            cdesc.nb_layers = 1;
                            cdesc.layers[0].format = cursor.pixel_format;
                            cdesc.layers[0].nb_planes = 1;
                            cdesc.layers[0].planes[0] = ffi::AVDRMPlaneDescriptor {
                                object_index: 0,
                                offset: cursor.offset as isize,
                                pitch: cursor.pitch as isize,
                            };

                            let cdesc_ptr = Box::into_raw(cdesc);
                            let cbuf = ffi::av_buffer_create(
                                cdesc_ptr as *mut u8,
                                std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
                                Some(free_drm_frame_descriptor),
                                ptr::null_mut(),
                                ffi::AV_BUFFER_FLAG_READONLY as libc::c_int,
                            );
                            if !cbuf.is_null() {
                                ffi::av_frame_unref(self.cursor_drm_frame);
                                (*self.cursor_drm_frame).format =
                                    ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
                                (*self.cursor_drm_frame).width = cw;
                                (*self.cursor_drm_frame).height = ch;
                                (*self.cursor_drm_frame).hw_frames_ctx =
                                    ffi::av_buffer_ref(self.cursor_drm_frames_ctx);
                                (*self.cursor_drm_frame).buf[0] = cbuf;
                                (*self.cursor_drm_frame).data[0] = (*cbuf).data;

                                ffi::av_frame_unref(self.cursor_vaapi_frame);
                                (*self.cursor_vaapi_frame).format =
                                    ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
                                (*self.cursor_vaapi_frame).hw_frames_ctx =
                                    ffi::av_buffer_ref(self.cursor_vaapi_frames_ctx);

                                let cmap = ffi::av_hwframe_map(
                                    self.cursor_vaapi_frame,
                                    self.cursor_drm_frame,
                                    ffi::AV_HWFRAME_MAP_READ as libc::c_int,
                                );
                                ffi::av_frame_unref(self.cursor_drm_frame);
                                if cmap >= 0 {
                                    has_cursor = 1;
                                    cursor_surface =
                                        (*self.cursor_vaapi_frame).data[3] as usize as u32;
                                    cursor_x = cursor.x;
                                    cursor_y = cursor.y;
                                    cursor_src_w = cw;
                                    cursor_src_h = ch;
                                    cursor_dst_w = cursor.crtc_w as i32;
                                    cursor_dst_h = cursor.crtc_h as i32;
                                } else {
                                    ffi::av_frame_unref(self.cursor_vaapi_frame);
                                }
                            } else {
                                free_drm_frame_descriptor(ptr::null_mut(), cdesc_ptr as *mut u8);
                            }
                        }
                    }
                }
            }

            // 3. Allocate destination NV12 VA-API surface directly from encoder pool.
            ffi::av_frame_unref(out);
            let ret = ffi::av_hwframe_get_buffer(enc_hw_frames_ctx, out, 0);
            if ret < 0 {
                ffi::av_frame_unref(self.cursor_vaapi_frame);
                ffi::av_frame_unref(self.vaapi_in_frame);
                bail!("av_hwframe_get_buffer(NV12) failed (error {ret})");
            }
            let dst_surface = (*out).data[3] as usize as u32;

            // 4. Single-pass GPU color convert (+ alpha-blended cursor overlay).
            let vpp_ret = screx_va_vpp_compose(
                self.va_vpp,
                dst_surface,
                primary_surface,
                self.width,
                self.height,
                has_cursor,
                cursor_surface,
                cursor_x,
                cursor_y,
                cursor_src_w,
                cursor_src_h,
                cursor_dst_w,
                cursor_dst_h,
            );
            ffi::av_frame_unref(self.cursor_vaapi_frame);
            ffi::av_frame_unref(self.vaapi_in_frame);
            if vpp_ret < 0 {
                ffi::av_frame_unref(out);
                bail!("screx_va_vpp_compose failed (VAStatus error {})", -vpp_ret);
            }
            (*out).pts = pts;
            Ok(())
        }
    }
}

impl Drop for DmaBufVppScaler {
    fn drop(&mut self) {
        unsafe {
            if !self.cursor_vaapi_frame.is_null() {
                ffi::av_frame_free(&mut self.cursor_vaapi_frame);
            }
            if !self.cursor_drm_frame.is_null() {
                ffi::av_frame_free(&mut self.cursor_drm_frame);
            }
            if !self.cursor_vaapi_frames_ctx.is_null() {
                ffi::av_buffer_unref(&mut self.cursor_vaapi_frames_ctx);
            }
            if !self.cursor_drm_frames_ctx.is_null() {
                ffi::av_buffer_unref(&mut self.cursor_drm_frames_ctx);
            }
            if !self.vaapi_in_frame.is_null() {
                ffi::av_frame_free(&mut self.vaapi_in_frame);
            }
            if !self.drm_frame.is_null() {
                ffi::av_frame_free(&mut self.drm_frame);
            }
            if !self.va_vpp.is_null() {
                screx_va_vpp_destroy(self.va_vpp);
            }
            if !self.vaapi_in_frames_ctx.is_null() {
                ffi::av_buffer_unref(&mut self.vaapi_in_frames_ctx);
            }
            if !self.drm_frames_ctx.is_null() {
                ffi::av_buffer_unref(&mut self.drm_frames_ctx);
            }
            if !self.drm_device_ctx.is_null() {
                ffi::av_buffer_unref(&mut self.drm_device_ctx);
            }
            if !self.vaapi_device_ctx.is_null() {
                ffi::av_buffer_unref(&mut self.vaapi_device_ctx);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VaapiHevcMode {
    Cbr,
    Bitrate,
    Cqp(i64),
}

const INFINITE_GOP_FRAMES: i32 = 600_000;
const REOPEN_COOLDOWN: Duration = Duration::from_millis(2500);

/// Whether the given encoder exposes FFmpeg's `intra-refresh` private option.
/// Verified against FFmpeg 9: NVENC (H.264/H.265) exposes it; VA-API does
/// not, so it relies on infinite GOP (PLI-only) by default.
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
    } else if bits_per_pixel >= 0.015 {
        34
    } else if bits_per_pixel >= 0.008 {
        37
    } else {
        40
    }
}

fn significant_bitrate_change(current: u32, target: u32) -> bool {
    let diff = current.abs_diff(target);
    diff * 5 >= current.max(1) // >= 20% change
}

impl HwEncoder {
    /// Opens a fresh `AVCodecContext` on an existing `hw_device_ctx` + `hw_frames_ctx`.
    /// Used both at initial open and for VA-API live bitrate/QP reconfiguration.
    unsafe fn open_codec_context(
        config: &EncoderConfig,
        kind: HwKind,
        sw_pix_fmt: ffi::AVPixelFormat,
        vaapi_mode: VaapiHevcMode,
        hw_device_ctx: *mut ffi::AVBufferRef,
        hw_frames_ctx: *mut ffi::AVBufferRef,
    ) -> Result<(*mut ffi::AVCodecContext, Vec<u8>)> {
        let (codec_name, hw_pix_fmt) = match (kind, config.codec) {
            (HwKind::Vaapi, VideoCodec::H264) => ("h264_vaapi", ffi::AVPixelFormat::AV_PIX_FMT_VAAPI),
            (HwKind::Vaapi, VideoCodec::H265) => ("hevc_vaapi", ffi::AVPixelFormat::AV_PIX_FMT_VAAPI),
            (HwKind::Nvenc, VideoCodec::H264) => ("h264_nvenc", ffi::AVPixelFormat::AV_PIX_FMT_CUDA),
            (HwKind::Nvenc, VideoCodec::H265) => ("hevc_nvenc", ffi::AVPixelFormat::AV_PIX_FMT_CUDA),
        };
        let use_intra_refresh = config.intra_refresh && hw_supports_intra_refresh(kind);
        let codec_cstr = std::ffi::CString::new(codec_name).unwrap();
        let codec = ffi::avcodec_find_encoder_by_name(codec_cstr.as_ptr());
        if codec.is_null() {
            bail!("{codec_name} encoder not found");
        }
        let ctx = ffi::avcodec_alloc_context3(codec);
        if ctx.is_null() {
            bail!("failed to allocate codec context");
        }

        (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device_ctx);
        (*ctx).hw_frames_ctx = ffi::av_buffer_ref(hw_frames_ctx);
        (*ctx).pix_fmt = hw_pix_fmt;
        (*ctx).sw_pix_fmt = sw_pix_fmt;
        (*ctx).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        (*ctx).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        (*ctx).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
        (*ctx).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        if matches!(kind, HwKind::Vaapi)
            && config.codec == VideoCodec::H265
            && sw_pix_fmt == ffi::AVPixelFormat::AV_PIX_FMT_P010LE
        {
            (*ctx).profile = ffi::AV_PROFILE_HEVC_MAIN_10;
        }
        (*ctx).width = config.width as i32;
        (*ctx).height = config.height as i32;
        (*ctx).time_base = ffi::AVRational { num: 1, den: 90_000 };
        (*ctx).framerate = ffi::AVRational { num: config.fps.max(1) as i32, den: 1 };
        (*ctx).gop_size = if use_intra_refresh || config.gop == 0 {
            INFINITE_GOP_FRAMES
        } else {
            config.gop as i32
        };
        (*ctx).max_b_frames = 0;

        let is_vaapi_cqp = matches!((kind, vaapi_mode), (HwKind::Vaapi, VaapiHevcMode::Cqp(_)));
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
                ffi::av_opt_set_int(
                    (*ctx).priv_data,
                    key.as_ptr(),
                    i64::from(config.vaapi_async_depth.clamp(1, 8)),
                    0,
                );
                match vaapi_mode {
                    VaapiHevcMode::Cbr => {
                        let rc_mode = std::ffi::CString::new("rc_mode").unwrap();
                        let cbr = std::ffi::CString::new("CBR").unwrap();
                        ffi::av_opt_set((*ctx).priv_data, rc_mode.as_ptr(), cbr.as_ptr(), 0);
                    }
                    VaapiHevcMode::Bitrate => {}
                    VaapiHevcMode::Cqp(qp_value) => {
                        let rc_mode = std::ffi::CString::new("rc_mode").unwrap();
                        let cqp = std::ffi::CString::new("CQP").unwrap();
                        ffi::av_opt_set((*ctx).priv_data, rc_mode.as_ptr(), cqp.as_ptr(), 0);
                        let qp = std::ffi::CString::new("qp").unwrap();
                        ffi::av_opt_set_int((*ctx).priv_data, qp.as_ptr(), qp_value, 0);
                    }
                }
            }
            HwKind::Nvenc => {
                let set = |k: &str, v: &str| {
                    let kc = std::ffi::CString::new(k).unwrap();
                    let vc = std::ffi::CString::new(v).unwrap();
                    ffi::av_opt_set((*ctx).priv_data, kc.as_ptr(), vc.as_ptr(), 0);
                };
                set("preset", "p4");
                set("tune", "ull");
                set("rc", "cbr");
                set("zerolatency", "1");
                set("forced-idr", "1");
                set("no-scenecut", "1");
                let delay = std::ffi::CString::new("delay").unwrap();
                ffi::av_opt_set_int((*ctx).priv_data, delay.as_ptr(), 0, 0);
                if use_intra_refresh {
                    set("intra-refresh", "1");
                }
            }
        }

        let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
        if ret < 0 {
            ffi::avcodec_free_context(&mut (ctx as *mut _));
            bail!("failed to open {codec_name} encoder (error {ret})");
        }

        let extradata = if !(*ctx).extradata.is_null() && (*ctx).extradata_size > 0 {
            std::slice::from_raw_parts((*ctx).extradata, (*ctx).extradata_size as usize).to_vec()
        } else {
            Vec::new()
        };
        Ok((ctx, extradata))
    }

    fn reconfigure_bitrate(&mut self, bps: u32) -> Result<()> {
        unsafe {
            let ctx = self.ctx;
            if ctx.is_null() {
                bail!("encoder context is null");
            }
            match self.kind {
                HwKind::Nvenc => {
                    // FFmpeg's nvenc backend calls nvEncReconfigureEncoder() automatically
                    // on the next frame when bit_rate / rc_max_rate / rc_buffer_size change.
                    (*ctx).bit_rate = bps as i64;
                    (*ctx).rc_max_rate = bps as i64;
                    (*ctx).rc_buffer_size = (bps / self.fps.max(1) * 2).max(1) as i32;
                    self.config.bitrate_bps = bps;
                }
                HwKind::Vaapi => {
                    // FFmpeg's vaapi_encode only emits VAEncMiscParameterRateControl at
                    // avcodec_open2() time, and CQP mode ignores bit_rate altogether.
                    // Re-open only the codec context (reusing hw_device_ctx, hw_frames_ctx,
                    // and VppScaler) with hysteresis so we don't thrash.
                    if self.last_reopen_at.elapsed() < REOPEN_COOLDOWN {
                        return Ok(());
                    }
                    let mut next_config = self.config.clone();
                    next_config.bitrate_bps = bps;
                    let next_mode = if self.is_cqp {
                        let new_qp = vaapi_hevc_qp(&next_config);
                        if self.vaapi_rc_mode == VaapiHevcMode::Cqp(new_qp) {
                            return Ok(());
                        }
                        VaapiHevcMode::Cqp(new_qp)
                    } else {
                        if !significant_bitrate_change(self.config.bitrate_bps, bps) {
                            return Ok(());
                        }
                        self.vaapi_rc_mode
                    };
                    let (new_ctx, new_extradata) = Self::open_codec_context(
                        &next_config,
                        self.kind,
                        self.sw_pix_fmt,
                        next_mode,
                        self.hw_device_ctx,
                        self.hw_frames_ctx,
                    )?;
                    ffi::avcodec_free_context(&mut self.ctx);
                    self.ctx = new_ctx;
                    self.extradata = new_extradata;
                    self.config.bitrate_bps = bps;
                    self.vaapi_rc_mode = next_mode;
                    self.last_reopen_at = Instant::now();
                    crate::log_detail!(
                        "[encode] VA-API context reopened for bitrate={bps} (cqp={})",
                        self.is_cqp
                    );
                }
            }
        }
        Ok(())
    }

    fn supports_intra_refresh(&self) -> bool {
        hw_supports_intra_refresh(self.kind)
    }

    fn new_vaapi_with_format_fallback(
        config: &EncoderConfig,
        vaapi_mode: VaapiHevcMode,
    ) -> Result<Self> {
        // For H.265 with GPU VPP, prefer 10-bit P010 (HEVC Main10) for smoother
        // dark-UI gradients and better compression, silently falling back to
        // 8-bit NV12 on GPUs/drivers that lack 10-bit HEVC encode.
        if config.codec == VideoCodec::H265 && config.gpu_convert {
            if let Ok(enc) = Self::new_with_vaapi_hevc_mode(
                config,
                HwKind::Vaapi,
                ffi::AVPixelFormat::AV_PIX_FMT_P010LE,
                vaapi_mode,
            ) {
                return Ok(enc);
            }
            crate::log_detail!(
                "[encode] VA-API 10-bit HEVC (P010) unavailable, falling back to 8-bit NV12"
            );
        }
        Self::new_with_vaapi_hevc_mode(
            config,
            HwKind::Vaapi,
            ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            vaapi_mode,
        )
    }

    fn new_vaapi(config: &EncoderConfig) -> Result<Self> {
        if let Ok(enc) = Self::new_vaapi_with_format_fallback(config, VaapiHevcMode::Cbr) {
            crate::log_detail!("[encode] VA-API using CBR rate control");
            return Ok(enc);
        }
        match Self::new_vaapi_with_format_fallback(config, VaapiHevcMode::Bitrate) {
            Ok(enc) => {
                crate::log_detail!("[encode] VA-API using auto bitrate rate control");
                Ok(enc)
            }
            Err(primary_err) if config.codec == VideoCodec::H265 => {
                let qp = vaapi_hevc_qp(config);
                eprintln!(
                    "[encode] VA-API HEVC bitrate mode unavailable, retrying with CQP (qp={qp}): {primary_err:#}"
                );
                Self::new_vaapi_with_format_fallback(config, VaapiHevcMode::Cqp(qp))
                    .with_context(|| {
                        format!(
                            "VA-API HEVC init failed in bitrate mode and CQP fallback; bitrate mode error: {primary_err:#}"
                        )
                    })
            }
            Err(err) => Err(err),
        }
    }

    fn new_nvenc(config: &EncoderConfig) -> Result<Self> {
        Self::new_with_vaapi_hevc_mode(
            config,
            HwKind::Nvenc,
            ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            VaapiHevcMode::Bitrate,
        )
    }

    fn new_with_vaapi_hevc_mode(
        config: &EncoderConfig,
        kind: HwKind,
        sw_pix_fmt: ffi::AVPixelFormat,
        vaapi_hevc_mode: VaapiHevcMode,
    ) -> Result<Self> {
        let width = config.width as i32;
        let height = config.height as i32;

        let (hw_type, hw_pix_fmt, device_path) = match kind {
            HwKind::Vaapi => (
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
                "/dev/dri/renderD128",
            ),
            HwKind::Nvenc => (
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
            let mut hw_device_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
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
                bail!("failed to create {kind_name} device context (error {ret})");
            }

            let hw_frames_ctx = ffi::av_hwframe_ctx_alloc(hw_device_ctx);
            if hw_frames_ctx.is_null() {
                ffi::av_buffer_unref(&mut hw_device_ctx);
                bail!("failed to allocate hw frames context");
            }
            let frames_ctx = (*hw_frames_ctx).data as *mut ffi::AVHWFramesContext;
            (*frames_ctx).format = hw_pix_fmt;
            (*frames_ctx).sw_format = sw_pix_fmt;
            (*frames_ctx).width = width;
            (*frames_ctx).height = height;
            (*frames_ctx).initial_pool_size = 20;

            let ret = ffi::av_hwframe_ctx_init(hw_frames_ctx);
            if ret < 0 {
                ffi::av_buffer_unref(&mut (hw_frames_ctx as *mut _));
                ffi::av_buffer_unref(&mut hw_device_ctx);
                bail!("failed to init hw frames context (error {ret})");
            }

            let (ctx, extradata) = match Self::open_codec_context(
                config,
                kind,
                sw_pix_fmt,
                vaapi_hevc_mode,
                hw_device_ctx,
                hw_frames_ctx,
            ) {
                Ok(opened) => opened,
                Err(err) => {
                    ffi::av_buffer_unref(&mut (hw_frames_ctx as *mut _));
                    ffi::av_buffer_unref(&mut hw_device_ctx);
                    return Err(err);
                }
            };

            // VA-API can convert BGRA to NV12/P010 on the GPU, which keeps the
            // capture thread out of the pixel path. If the driver, the filter
            // set or the graph cannot provide it, keep the CPU scaler below.
            let vpp = if matches!(kind, HwKind::Vaapi) && config.gpu_convert {
                match VppScaler::new(width, height, sw_pix_fmt, hw_device_ctx) {
                    Some(vpp) => {
                        crate::log_detail!("[encode] VA-API VPP colour conversion enabled");
                        Some(vpp)
                    }
                    None => {
                        eprintln!("[encode] VA-API VPP unavailable; converting on the CPU");
                        None
                    }
                }
            } else {
                if matches!(kind, HwKind::Vaapi) {
                    crate::log_detail!("[encode] VA-API VPP disabled; converting on the CPU");
                }
                None
            };

            let Some(sws) = BandedScaler::new(width, height, sw_pix_fmt)
            else {
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                ffi::av_buffer_unref(&mut (hw_frames_ctx as *mut _));
                ffi::av_buffer_unref(&mut hw_device_ctx);
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

            let fmt_label = if sw_pix_fmt == ffi::AVPixelFormat::AV_PIX_FMT_P010LE {
                "P010 (10-bit)"
            } else {
                "NV12 (8-bit)"
            };
            crate::log_detail!(
                "[encode] {kind_name} {codec_label} encoder: {}x{}@{} fmt={fmt_label} color=BT.709 bitrate={} gop={} intra_refresh={}",
                width, height, config.fps, config.bitrate_bps, config.gop, use_intra_refresh
            );

            Ok(Self {
                kind,
                sw_pix_fmt,
                config: config.clone(),
                vaapi_rc_mode: vaapi_hevc_mode,
                last_reopen_at: Instant::now(),
                ctx,
                hw_device_ctx,
                hw_frames_ctx,
                sws,
                vpp,
                dmabuf_vpp: None,
                sw_bgra,
                sw_nv12,
                hw_frame,
                mapped_frame,
                pkt,
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
            let pts = (self.frame_index as i64 * 90_000) / self.fps as i64;
            match &frame.data {
                FrameData::DmaBuf(dmabuf) => {
                    if !matches!(self.kind, HwKind::Vaapi) || !self.config.gpu_convert {
                        bail!("DMA-BUF input requires VA-API with GPU VPP enabled");
                    }
                    let sw_format = drm_fourcc_to_sw_pix_fmt(dmabuf.pixel_format).ok_or_else(
                        || {
                            anyhow::anyhow!(
                                "unsupported DRM fourcc 0x{:08x} for DMA-BUF import",
                                dmabuf.pixel_format
                            )
                        },
                    )?;
                    let width = frame.width as i32;
                    let height = frame.height as i32;
                    let needs_init = self
                        .dmabuf_vpp
                        .as_ref()
                        .is_none_or(|s| !s.matches(width, height, sw_format));
                    if needs_init {
                        self.dmabuf_vpp = Some(DmaBufVppScaler::new(
                            width,
                            height,
                            sw_format,
                            self.hw_device_ctx,
                        )?);
                    }
                    let dmabuf_vpp = self.dmabuf_vpp.as_mut().unwrap();
                    if let Err(err) =
                        dmabuf_vpp.convert(dmabuf, pts, (*self.ctx).hw_frames_ctx, self.hw_frame)
                    {
                        ffi::av_frame_unref(self.hw_frame);
                        return Err(err);
                    }
                }
                FrameData::Bgra(bytes) => {
                    (*self.sw_bgra).data[0] = bytes.as_ptr() as *mut u8;
                    (*self.sw_bgra).pts = pts;

                    // The graph hands back an NV12 surface already on the encoder's
                    // VA-API device; nothing else to do before sending it.
                    let mut conversion_error = None;
                    if let Some(vpp) = self.vpp.as_mut() {
                        if let Err(error) = vpp.convert(self.sw_bgra, self.hw_frame) {
                            conversion_error = Some(error);
                        }
                    }
                    if let Some(error) = conversion_error {
                        eprintln!(
                            "[encode] VA-API VPP conversion failed, converting on the CPU: {error:#}"
                        );
                        self.vpp = None;
                        ffi::av_frame_unref(self.hw_frame);
                    }
                    let converted = self.vpp.is_some();

                    if !converted {
                        let ret =
                            ffi::av_hwframe_get_buffer((*self.ctx).hw_frames_ctx, self.hw_frame, 0);
                        if ret < 0 {
                            bail!("failed to get hw frame buffer (error {ret})");
                        }

                        let map_flags = (ffi::AV_HWFRAME_MAP_WRITE as libc::c_int)
                            | (ffi::AV_HWFRAME_MAP_OVERWRITE as libc::c_int);
                        let mapped =
                            ffi::av_hwframe_map(self.mapped_frame, self.hw_frame, map_flags);

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
                                eprintln!(
                                    "[encode] hwframe mapping unavailable, allocating sw_nv12 fallback"
                                );
                                self.sw_nv12 = ffi::av_frame_alloc();
                                if self.sw_nv12.is_null() {
                                    ffi::av_frame_unref(self.mapped_frame);
                                    ffi::av_frame_unref(self.hw_frame);
                                    bail!("failed to allocate NV12 fallback frame");
                                }
                                (*self.sw_nv12).format = self.sw_pix_fmt as i32;
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

                            let ret = ffi::av_hwframe_get_buffer(
                                (*self.ctx).hw_frames_ctx,
                                self.hw_frame,
                                0,
                            );
                            if ret < 0 {
                                bail!("failed to get hw frame buffer (error {ret})");
                            }

                            let ret =
                                ffi::av_hwframe_transfer_data(self.hw_frame, self.sw_nv12, 0);
                            if ret < 0 {
                                ffi::av_frame_unref(self.hw_frame);
                                bail!("failed to upload frame to hw surface (error {ret})");
                            }
                        }
                    }
                }
            }

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
        self.dmabuf_vpp = None;
        self.vpp = None;
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
            if !self.hw_frames_ctx.is_null() {
                ffi::av_buffer_unref(&mut self.hw_frames_ctx);
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
    config: EncoderConfig,
    last_reopen_at: Instant,
    ctx: *mut ffi::AVCodecContext,
    sws: BandedScaler,
    sw_bgra: *mut ffi::AVFrame,
    sw_yuv: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    frame_index: u64,
    fps: u32,
    codec: VideoCodec,
    extradata: Vec<u8>,
}

unsafe impl Send for SwEncoder {}

impl SwEncoder {
    unsafe fn open_codec_context(
        config: &EncoderConfig,
    ) -> Result<(*mut ffi::AVCodecContext, Vec<u8>, &'static str, bool)> {
        let use_intra_refresh = config.intra_refresh && config.codec == VideoCodec::H264;
        let (enc_name, opts): (&str, Vec<(&str, &str)>) = match config.codec {
            VideoCodec::H264 => {
                let mut opts = vec![
                    ("preset", "ultrafast"),
                    ("tune", "zerolatency"),
                    ("forced-idr", "1"),
                    ("profile", "main"),
                ];
                if use_intra_refresh {
                    opts.push(("intra-refresh", "1"));
                }
                if config.gop == 0 {
                    opts.push(("x264-params", "scenecut=0"));
                }
                ("libx264", opts)
            }
            VideoCodec::H265 => {
                let mut opts = vec![
                    ("preset", "ultrafast"),
                    ("tune", "zerolatency"),
                    ("forced-idr", "1"),
                ];
                if config.gop == 0 {
                    opts.push(("x265-params", "scenecut=0"));
                }
                ("libx265", opts)
            }
        };

        let codec_name = std::ffi::CString::new(enc_name).unwrap();
        let codec = ffi::avcodec_find_encoder_by_name(codec_name.as_ptr());
        if codec.is_null() {
            bail!("{enc_name} encoder not found");
        }
        let ctx = ffi::avcodec_alloc_context3(codec);
        if ctx.is_null() {
            bail!("failed to allocate codec context");
        }

        (*ctx).width = config.width as i32;
        (*ctx).height = config.height as i32;
        (*ctx).time_base = ffi::AVRational { num: 1, den: 90_000 };
        (*ctx).framerate = ffi::AVRational { num: config.fps.max(1) as i32, den: 1 };
        (*ctx).bit_rate = config.bitrate_bps as i64;
        (*ctx).rc_max_rate = config.bitrate_bps as i64;
        (*ctx).rc_buffer_size = (config.bitrate_bps / config.fps.max(1) * 2).max(1) as i32;
        (*ctx).gop_size = if use_intra_refresh || config.gop == 0 {
            INFINITE_GOP_FRAMES
        } else {
            config.gop as i32
        };
        (*ctx).max_b_frames = 0;
        (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
        (*ctx).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        (*ctx).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        (*ctx).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
        (*ctx).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
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
            std::slice::from_raw_parts((*ctx).extradata, (*ctx).extradata_size as usize).to_vec()
        } else {
            Vec::new()
        };

        Ok((ctx, extradata, enc_name, use_intra_refresh))
    }

    fn reconfigure_bitrate(&mut self, bps: u32) -> Result<()> {
        unsafe {
            let ctx = self.ctx;
            if ctx.is_null() {
                bail!("encoder context is null");
            }
            match self.codec {
                VideoCodec::H264 => {
                    // FFmpeg's libx264 calls x264_encoder_reconfig() live on bitrate changes.
                    (*ctx).bit_rate = bps as i64;
                    (*ctx).rc_max_rate = bps as i64;
                    (*ctx).rc_buffer_size = (bps / self.fps.max(1) * 2).max(1) as i32;
                    self.config.bitrate_bps = bps;
                }
                VideoCodec::H265 => {
                    // FFmpeg's libx265 does not reconfigure bitrate live; reopen with hysteresis.
                    if self.last_reopen_at.elapsed() < REOPEN_COOLDOWN
                        || !significant_bitrate_change(self.config.bitrate_bps, bps)
                    {
                        return Ok(());
                    }
                    let mut next_config = self.config.clone();
                    next_config.bitrate_bps = bps;
                    let (new_ctx, new_extradata, _, _) = Self::open_codec_context(&next_config)?;
                    ffi::avcodec_free_context(&mut self.ctx);
                    self.ctx = new_ctx;
                    self.extradata = new_extradata;
                    self.config.bitrate_bps = bps;
                    self.last_reopen_at = Instant::now();
                }
            }
        }
        Ok(())
    }

    fn supports_intra_refresh(&self) -> bool {
        self.codec == VideoCodec::H264
    }

    fn new(config: &EncoderConfig) -> Result<Self> {
        let width = config.width as i32;
        let height = config.height as i32;

        unsafe {
            let (ctx, extradata, enc_name, use_intra_refresh) = Self::open_codec_context(config)?;

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

            crate::log_detail!(
                "[encode] software encoder ({enc_name}): {}x{}@{} bitrate={} gop={} intra_refresh={}",
                width, height, config.fps, config.bitrate_bps, config.gop, use_intra_refresh
            );

            Ok(Self {
                config: config.clone(),
                last_reopen_at: Instant::now(),
                ctx,
                sws,
                sw_bgra,
                sw_yuv,
                pkt,
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
            let FrameData::Bgra(bytes) = frame.data else {
                bail!("software encoder does not accept zero-copy DMA-BUF frames");
            };
            // Ensure we have exclusive ownership of the YUV buffer
            // (libx264 may still hold a ref from the previous frame)
            let ret = ffi::av_frame_make_writable(self.sw_yuv);
            if ret < 0 {
                bail!("av_frame_make_writable failed (error {ret})");
            }

            (*self.sw_bgra).data[0] = bytes.as_ptr() as *mut u8;
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

    /// Encodes a frame through the full VA-API path, which also checks that the
    /// encoder accepts surfaces produced by the VPP graph.
    #[test]
    fn vaapi_encoder_accepts_vpp_frames() {
        let config = EncoderConfig {
            bitrate_bps: 4_000_000,
            vaapi_async_depth: 2,
            gop: 90,
            fps: 30,
            width: 256,
            height: 256,
            backend: EncoderBackend::Vaapi,
            codec: VideoCodec::H264,
            intra_refresh: false,
            gpu_convert: true,
        };
        let Ok(mut encoder) = Encoder::new(config) else {
            eprintln!("skipping: VA-API encoding unavailable");
            return;
        };
        let bgra = vec![128u8; 256 * 256 * 4];
        let capture = CaptureFrame::bgra(256, 256, &bgra);
        // async_depth pipelines, so the first packets arrive a frame later.
        let mut units = Vec::new();
        for index in 0..3 {
            units.extend(encoder.encode_frame(&capture, index == 0).expect("encode"));
        }
        assert!(!units.is_empty());
        assert!(units.iter().any(|unit| unit.is_idr));
    }

    /// Exports a real GPU DMA-BUF from a VA-API BGR0/BGRA surface, wraps it in
    /// `CaptureFrame::dmabuf`, verifies `DmaBufVppScaler` produces valid non-black
    /// NV12 pixels, and verifies `Encoder::encode_frame` emits an H.264 IDR.
    #[test]
    fn vaapi_dmabuf_zero_copy_converts_and_encodes() {
        use std::os::fd::FromRawFd;
        use crate::capture::{DmaBufObject, DmaBufPlane};

        let width: i32 = 256;
        let height: i32 = 256;
        unsafe {
            let mut va_dev = ptr::null_mut();
            let dev_path = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
            if ffi::av_hwdevice_ctx_create(
                &mut va_dev,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                dev_path.as_ptr(),
                ptr::null_mut(),
                0,
            ) < 0
            {
                eprintln!("skipping: no VA-API device for zero-copy test");
                return;
            }

            // Create a VA-API BGRA surface pool and fill one surface with color.
            let va_frames = ffi::av_hwframe_ctx_alloc(va_dev);
            assert!(!va_frames.is_null());
            let fc = (*va_frames).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA;
            (*fc).width = width;
            (*fc).height = height;
            (*fc).initial_pool_size = 2;
            if ffi::av_hwframe_ctx_init(va_frames) < 0 {
                eprintln!("skipping: VA-API BGRA frame pool unavailable");
                ffi::av_buffer_unref(&mut (va_frames as *mut _));
                ffi::av_buffer_unref(&mut va_dev);
                return;
            }

            let mut src_va = ffi::av_frame_alloc();
            assert!(ffi::av_hwframe_get_buffer(va_frames, src_va, 0) >= 0);

            let mut sw_src = ffi::av_frame_alloc();
            (*sw_src).format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA as i32;
            (*sw_src).width = width;
            (*sw_src).height = height;
            assert!(ffi::av_frame_get_buffer(sw_src, 0) >= 0);
            let stride = (*sw_src).linesize[0] as usize;
            for row in 0..height as usize {
                let row_ptr = (*sw_src).data[0].add(row * stride);
                for col in 0..width as usize {
                    *row_ptr.add(col * 4) = 40;
                    *row_ptr.add(col * 4 + 1) = 200;
                    *row_ptr.add(col * 4 + 2) = 90;
                    *row_ptr.add(col * 4 + 3) = 255;
                }
            }
            assert!(ffi::av_hwframe_transfer_data(src_va, sw_src, 0) >= 0);

            // Export the VA-API surface to a real DRM_PRIME DMA-BUF descriptor.
            let mut drm_dev = ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM);
            assert!(!drm_dev.is_null());
            let dev_ctx = (*drm_dev).data as *mut ffi::AVHWDeviceContext;
            let drm_hwctx = (*dev_ctx).hwctx as *mut ffi::AVDRMDeviceContext;
            (*drm_hwctx).fd = -1;
            assert!(ffi::av_hwdevice_ctx_init(drm_dev) >= 0);

            let mut drm_frames = ptr::null_mut();
            let derive_ret = ffi::av_hwframe_ctx_create_derived(
                &mut drm_frames,
                ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME,
                drm_dev,
                va_frames,
                0,
            );
            assert!(
                derive_ret >= 0,
                "av_hwframe_ctx_create_derived(DRM_PRIME <- VAAPI) failed: {derive_ret}"
            );
            let mut exported_drm = ffi::av_frame_alloc();
            (*exported_drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            (*exported_drm).hw_frames_ctx = ffi::av_buffer_ref(drm_frames);
            let export_map_ret = ffi::av_hwframe_map(
                exported_drm,
                src_va,
                ffi::AV_HWFRAME_MAP_READ as libc::c_int,
            );
            assert!(
                export_map_ret >= 0,
                "av_hwframe_map(VAAPI -> DRM_PRIME) failed: {export_map_ret}"
            );

            let desc = &*((*exported_drm).data[0] as *const ffi::AVDRMFrameDescriptor);
            assert!(desc.nb_objects >= 1 && desc.nb_layers >= 1);
            let layer = &desc.layers[0];
            let mut objects = Vec::new();
            for i in 0..desc.nb_objects as usize {
                let dup_fd = libc::fcntl(desc.objects[i].fd, libc::F_DUPFD_CLOEXEC, 0);
                assert!(dup_fd >= 0);
                objects.push(DmaBufObject {
                    fd: std::os::fd::OwnedFd::from_raw_fd(dup_fd),
                    size: desc.objects[i].size as u64,
                    modifier: desc.objects[i].format_modifier,
                });
            }
            let mut planes = Vec::new();
            for p in 0..layer.nb_planes as usize {
                planes.push(DmaBufPlane {
                    object_index: layer.planes[p].object_index as usize,
                    offset: layer.planes[p].offset as u32,
                    pitch: layer.planes[p].pitch as u32,
                });
            }
            let mut dmabuf = DmaBufFrame {
                fb_id: 1,
                pixel_format: layer.format,
                flags: 0,
                objects,
                planes,
                cursor: None,
            };

            // Also export a 64x64 premultiplied-alpha cursor surface (white opaque square).
            let cursor_size: i32 = 64;
            let cursor_va_frames = ffi::av_hwframe_ctx_alloc(va_dev);
            assert!(!cursor_va_frames.is_null());
            let cfc = (*cursor_va_frames).data as *mut ffi::AVHWFramesContext;
            (*cfc).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*cfc).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA;
            (*cfc).width = cursor_size;
            (*cfc).height = cursor_size;
            (*cfc).initial_pool_size = 2;
            assert!(ffi::av_hwframe_ctx_init(cursor_va_frames) >= 0);

            let mut cur_va = ffi::av_frame_alloc();
            assert!(ffi::av_hwframe_get_buffer(cursor_va_frames, cur_va, 0) >= 0);
            let mut cur_sw = ffi::av_frame_alloc();
            (*cur_sw).format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA as i32;
            (*cur_sw).width = cursor_size;
            (*cur_sw).height = cursor_size;
            assert!(ffi::av_frame_get_buffer(cur_sw, 0) >= 0);
            let cstride = (*cur_sw).linesize[0] as usize;
            for row in 0..cursor_size as usize {
                let row_ptr = (*cur_sw).data[0].add(row * cstride);
                for col in 0..cursor_size as usize {
                    // Left half opaque white (255,255,255,255), right half transparent (0,0,0,0).
                    let val = if col < 32 { 255u8 } else { 0u8 };
                    *row_ptr.add(col * 4) = val;
                    *row_ptr.add(col * 4 + 1) = val;
                    *row_ptr.add(col * 4 + 2) = val;
                    *row_ptr.add(col * 4 + 3) = val;
                }
            }
            assert!(ffi::av_hwframe_transfer_data(cur_va, cur_sw, 0) >= 0);

            let mut cur_drm_frames = ptr::null_mut();
            assert!(
                ffi::av_hwframe_ctx_create_derived(
                    &mut cur_drm_frames,
                    ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME,
                    drm_dev,
                    cursor_va_frames,
                    0,
                ) >= 0
            );
            let mut cur_drm = ffi::av_frame_alloc();
            (*cur_drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            (*cur_drm).hw_frames_ctx = ffi::av_buffer_ref(cur_drm_frames);
            assert!(
                ffi::av_hwframe_map(cur_drm, cur_va, ffi::AV_HWFRAME_MAP_READ as libc::c_int) >= 0
            );
            let cdesc = &*((*cur_drm).data[0] as *const ffi::AVDRMFrameDescriptor);
            let clayer = &cdesc.layers[0];
            let cdup_fd = libc::fcntl(cdesc.objects[0].fd, libc::F_DUPFD_CLOEXEC, 0);
            assert!(cdup_fd >= 0);
            dmabuf.cursor = Some(crate::capture::DmaBufCursor {
                fb_id: 2,
                x: 64,
                y: 64,
                width: cursor_size as u32,
                height: cursor_size as u32,
                crtc_w: cursor_size as u32,
                crtc_h: cursor_size as u32,
                pixel_format: clayer.format,
                fd: std::os::fd::OwnedFd::from_raw_fd(cdup_fd),
                size: cdesc.objects[0].size as u64,
                modifier: cdesc.objects[0].format_modifier,
                pitch: clayer.planes[0].pitch as u32,
                offset: clayer.planes[0].offset as u32,
            });

            // 1. Verify DmaBufVppScaler converts primary + alpha-blends cursor into NV12.
            let nv12_frames = ffi::av_hwframe_ctx_alloc(va_dev);
            assert!(!nv12_frames.is_null());
            let nfc = (*nv12_frames).data as *mut ffi::AVHWFramesContext;
            (*nfc).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*nfc).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            (*nfc).width = width;
            (*nfc).height = height;
            (*nfc).initial_pool_size = 4;
            assert!(ffi::av_hwframe_ctx_init(nv12_frames) >= 0);

            let sw_fmt = drm_fourcc_to_sw_pix_fmt(dmabuf.pixel_format)
                .expect("exported VA-API fourcc should map to supported pixel format");
            let mut scaler =
                DmaBufVppScaler::new(width, height, sw_fmt, va_dev).expect("DmaBufVppScaler::new");
            let mut nv12_hw = ffi::av_frame_alloc();
            scaler
                .convert(&dmabuf, 0, nv12_frames, nv12_hw)
                .expect("DmaBufVppScaler::convert");
            assert_eq!((*nv12_hw).format, ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32);
            let mut readback = ffi::av_frame_alloc();
            assert!(
                ffi::av_hwframe_map(readback, nv12_hw, ffi::AV_HWFRAME_MAP_READ as libc::c_int)
                    >= 0
            );
            let luma_stride = (*readback).linesize[0] as usize;
            let bg_luma = *(*readback).data[0];
            let cursor_opaque_luma = *(*readback).data[0].add(70 * luma_stride + 70);
            let cursor_transparent_luma = *(*readback).data[0].add(70 * luma_stride + 110);
            assert!(
                bg_luma > 100 && bg_luma < 190,
                "expected mid-bright NV12 background luma, got {bg_luma}"
            );
            assert!(
                cursor_opaque_luma > 220,
                "expected bright white composited cursor pixel at (70,70), got {cursor_opaque_luma}"
            );
            assert!(
                (cursor_transparent_luma as i32 - bg_luma as i32).abs() <= 4,
                "expected transparent half of cursor at (110,70) to preserve background ({bg_luma}), got {cursor_transparent_luma}"
            );
            ffi::av_frame_free(&mut readback);
            ffi::av_frame_free(&mut nv12_hw);
            ffi::av_buffer_unref(&mut (nv12_frames as *mut _));
            drop(scaler);

            // 2. Verify full Encoder::encode_frame path produces H.264 IDR and
            //    10-bit/8-bit HEVC IDR output from the same DMA-BUF.
            for codec in [VideoCodec::H264, VideoCodec::H265] {
                let config = EncoderConfig {
                    bitrate_bps: 4_000_000,
                    vaapi_async_depth: 1,
                    gop: 0,
                    fps: 60,
                    width: width as u32,
                    height: height as u32,
                    backend: EncoderBackend::Vaapi,
                    codec,
                    intra_refresh: false,
                    gpu_convert: true,
                };
                let mut encoder = Encoder::new(config).expect("VA-API encoder");
                assert!(encoder.supports_dmabuf());
                let capture = CaptureFrame::dmabuf(width as u32, height as u32, &dmabuf);
                let mut units = Vec::new();
                for i in 0..3 {
                    units.extend(encoder.encode_frame(&capture, i == 0).expect("dmabuf encode"));
                }
                assert!(!units.is_empty(), "{codec:?} produced no units");
                assert!(units.iter().any(|u| u.is_idr), "{codec:?} produced no IDR");
            }

            // Cleanup
            drop(dmabuf);
            ffi::av_frame_free(&mut cur_drm);
            ffi::av_frame_free(&mut cur_sw);
            ffi::av_frame_free(&mut cur_va);
            ffi::av_buffer_unref(&mut cur_drm_frames);
            ffi::av_buffer_unref(&mut (cursor_va_frames as *mut _));
            ffi::av_frame_free(&mut exported_drm);
            ffi::av_frame_free(&mut sw_src);
            ffi::av_frame_free(&mut src_va);
            ffi::av_buffer_unref(&mut drm_frames);
            ffi::av_buffer_unref(&mut drm_dev);
            ffi::av_buffer_unref(&mut (va_frames as *mut _));
            ffi::av_buffer_unref(&mut va_dev);
        }
    }

    /// Exercises the VA-API VPP path on a real device: converts a BGRA frame
    /// to an NV12 surface and maps it back to check the pixels arrived.
    #[test]
    fn vaapi_vpp_converts_bgra_to_nv12() {
        let mut device = ptr::null_mut();
        let device_name = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
        if unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                device_name.as_ptr(),
                ptr::null_mut(),
                0,
            )
        } < 0
        {
            eprintln!("skipping: no VA-API device for VPP");
            return;
        }
        let Some(mut vpp) = VppScaler::new(64, 48, ffi::AVPixelFormat::AV_PIX_FMT_NV12, device) else {
            eprintln!("skipping: VPP graph unavailable");
            unsafe { ffi::av_buffer_unref(&mut device) };
            return;
        };
        let width: i32 = 64;
        let stride = width * 4;
        let mut bgra = vec![0u8; (stride * 48) as usize];
        for row in 0..48 {
            for column in 0..width {
                let pixel = ((row * width + column) * 4) as usize;
                bgra[pixel] = 40;
                bgra[pixel + 1] = 200;
                bgra[pixel + 2] = 90;
                bgra[pixel + 3] = 255;
            }
        }
        unsafe {
            let input = ffi::av_frame_alloc();
            (*input).format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA as i32;
            (*input).width = width;
            (*input).height = 48;
            (*input).linesize[0] = stride;
            (*input).data[0] = bgra.as_mut_ptr();
            let output = ffi::av_frame_alloc();
            vpp.convert(input, output).expect("VPP conversion");
            assert_eq!((*output).format, ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32);
            assert_eq!((*output).width, width);
            assert_eq!((*output).height, 48);

            let mut readback = ffi::av_frame_alloc();
            assert!(ffi::av_hwframe_map(readback, output, ffi::AV_HWFRAME_MAP_READ as _) >= 0);
            let luma = (*readback).data[0];
            let rows = (*readback).linesize[0] as usize;
            let values: Vec<u8> = (0..48)
                .flat_map(|row| (0..width as usize).map(move |column| row * rows + column))
                .map(|index| *luma.add(index))
                .collect();
            assert!(
                values.iter().any(|&value| value > 16),
                "expected image data in the NV12 luma plane"
            );
            ffi::av_frame_free(&mut readback);
            ffi::av_buffer_unref(&mut device);
            let mut output = output;
            ffi::av_frame_free(&mut output);
            let mut input = input;
            ffi::av_frame_free(&mut input);
        }
    }

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
            let coeffs = ffi::sws_getCoefficients(ffi::SWS_CS_ITU709 as libc::c_int);
            ffi::sws_setColorspaceDetails(sws, coeffs, 1, coeffs, 0, 0, 1 << 16, 1 << 16);
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
