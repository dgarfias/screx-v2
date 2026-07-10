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
    #[cfg(target_os = "linux")]
    Vaapi,
    Nvenc,
    #[cfg(target_os = "windows")]
    Amf,
    #[cfg(target_os = "windows")]
    Qsv,
    #[cfg(target_os = "windows")]
    Mf,
    #[cfg(target_os = "macos")]
    VideoToolbox,
    Software,
}

impl EncoderBackend {
    pub fn from_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            #[cfg(target_os = "linux")]
            "vaapi" => Self::Vaapi,
            "nvenc" | "nvidia" | "cuda" => Self::Nvenc,
            #[cfg(target_os = "windows")]
            "amf" => Self::Amf,
            #[cfg(target_os = "windows")]
            "qsv" => Self::Qsv,
            #[cfg(target_os = "windows")]
            "mf" => Self::Mf,
            #[cfg(target_os = "macos")]
            "videotoolbox" | "vt" => Self::VideoToolbox,
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
        #[cfg(target_os = "linux")]
        (EncoderBackend::Vaapi, VideoCodec::H264) => "h264_vaapi",
        #[cfg(target_os = "linux")]
        (EncoderBackend::Vaapi, VideoCodec::H265) => "hevc_vaapi",
        (EncoderBackend::Nvenc, VideoCodec::H264) => "h264_nvenc",
        (EncoderBackend::Nvenc, VideoCodec::H265) => "hevc_nvenc",
        #[cfg(target_os = "windows")]
        (EncoderBackend::Amf, VideoCodec::H264) => "h264_amf",
        #[cfg(target_os = "windows")]
        (EncoderBackend::Amf, VideoCodec::H265) => "hevc_amf",
        #[cfg(target_os = "windows")]
        (EncoderBackend::Qsv, VideoCodec::H264) => "h264_qsv",
        #[cfg(target_os = "windows")]
        (EncoderBackend::Qsv, VideoCodec::H265) => "hevc_qsv",
        #[cfg(target_os = "windows")]
        (EncoderBackend::Mf, VideoCodec::H264) => "h264_mf",
        #[cfg(target_os = "windows")]
        (EncoderBackend::Mf, VideoCodec::H265) => "hevc_mf",
        #[cfg(target_os = "macos")]
        (EncoderBackend::VideoToolbox, VideoCodec::H264) => "h264_videotoolbox",
        #[cfg(target_os = "macos")]
        (EncoderBackend::VideoToolbox, VideoCodec::H265) => "hevc_videotoolbox",
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

/// Which codecs this daemon can actually encode right now for `backend`
/// (which may be `Auto`). Does not just parrot the operator's `--codec`
/// flag — actually resolves an encoder name and checks ffmpeg has it.
/// `Auto` reports the union of what any backend it would consider at
/// encoder-construction time supports, since the one that actually wins is
/// only known once real hardware probing happens in `Encoder::new`.
pub fn probe_available_codecs(backend: EncoderBackend) -> Vec<VideoCodec> {
    let _ = ffmpeg::init();

    let candidates: Vec<EncoderBackend> = match backend {
        EncoderBackend::Auto => {
            #[cfg(target_os = "linux")]
            {
                vec![
                    EncoderBackend::Vaapi,
                    EncoderBackend::Nvenc,
                    EncoderBackend::Software,
                ]
            }
            #[cfg(target_os = "windows")]
            {
                vec![
                    EncoderBackend::Nvenc,
                    EncoderBackend::Amf,
                    EncoderBackend::Qsv,
                    EncoderBackend::Mf,
                    EncoderBackend::Software,
                ]
            }
            #[cfg(target_os = "macos")]
            {
                vec![EncoderBackend::VideoToolbox, EncoderBackend::Software]
            }
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
            #[cfg(target_os = "linux")]
            EncoderBackend::Vaapi => {
                let enc = HwEncoder::new_vaapi(&config)?;
                ActiveEncoder::HwAccel(enc)
            }
            EncoderBackend::Nvenc => {
                let enc = HwEncoder::new_nvenc(&config)?;
                ActiveEncoder::HwAccel(enc)
            }
            #[cfg(target_os = "windows")]
            EncoderBackend::Amf => {
                let enc = HwEncoder::new_amf(&config)?;
                ActiveEncoder::HwAccel(enc)
            }
            #[cfg(target_os = "windows")]
            EncoderBackend::Qsv => {
                let enc = HwEncoder::new_qsv(&config)?;
                ActiveEncoder::HwAccel(enc)
            }
            #[cfg(target_os = "windows")]
            EncoderBackend::Mf => {
                let enc = HwEncoder::new_mf(&config)?;
                ActiveEncoder::HwAccel(enc)
            }
            #[cfg(target_os = "macos")]
            EncoderBackend::VideoToolbox => {
                let enc = HwEncoder::new_videotoolbox(&config)?;
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
                #[cfg(target_os = "linux")]
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
                #[cfg(target_os = "windows")]
                {
                    if let Ok(enc) = HwEncoder::new_nvenc(&config) {
                        println!("[encode] auto-selected: nvenc ({codec_name})");
                        ActiveEncoder::HwAccel(enc)
                    } else if let Ok(enc) = HwEncoder::new_amf(&config) {
                        println!("[encode] auto-selected: amf ({codec_name})");
                        ActiveEncoder::HwAccel(enc)
                    } else if let Ok(enc) = HwEncoder::new_qsv(&config) {
                        println!("[encode] auto-selected: qsv ({codec_name})");
                        ActiveEncoder::HwAccel(enc)
                    } else {
                        println!("[encode] no hw encoder available, using software ({codec_name})");
                        let enc = SwEncoder::new(&config)?;
                        ActiveEncoder::Software(enc)
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    if let Ok(enc) = HwEncoder::new_videotoolbox(&config) {
                        println!("[encode] auto-selected: videotoolbox ({codec_name})");
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
                    #[cfg(target_os = "linux")]
                    HwKind::Vaapi => ActiveEncoder::HwAccel(HwEncoder::new_vaapi(&self.config)?),
                    HwKind::Nvenc => ActiveEncoder::HwAccel(HwEncoder::new_nvenc(&self.config)?),
                    #[cfg(target_os = "windows")]
                    HwKind::Amf => ActiveEncoder::HwAccel(HwEncoder::new_amf(&self.config)?),
                    #[cfg(target_os = "windows")]
                    HwKind::Qsv => ActiveEncoder::HwAccel(HwEncoder::new_qsv(&self.config)?),
                    #[cfg(target_os = "windows")]
                    HwKind::Mf => ActiveEncoder::HwAccel(HwEncoder::new_mf(&self.config)?),
                    #[cfg(target_os = "macos")]
                    HwKind::VideoToolbox => {
                        ActiveEncoder::HwAccel(HwEncoder::new_videotoolbox(&self.config)?)
                    }
                },
                ActiveEncoder::Software(_) => {
                    ActiveEncoder::Software(SwEncoder::new(&self.config)?)
                }
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
            crate::vlog!(
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
    #[cfg(target_os = "linux")]
    Vaapi,
    Nvenc,
    #[cfg(target_os = "windows")]
    Amf,
    #[cfg(target_os = "windows")]
    Qsv,
    #[cfg(target_os = "windows")]
    Mf,
    #[cfg(target_os = "macos")]
    VideoToolbox,
}

struct HwEncoder {
    kind: HwKind,
    ctx: *mut ffi::AVCodecContext,
    hw_device_ctx: *mut ffi::AVBufferRef,
    sws: *mut ffi::SwsContext,
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
    // AMF/MF accept plain NV12 system-memory frames and do their own internal
    // GPU upload — unlike VAAPI/NVENC they don't need (and on at least some
    // AMD iGPU/driver combinations, don't work with) an explicit
    // AVHWFramesContext. See push_frame() for the resulting split path.
    uses_sw_frames: bool,
}

unsafe impl Send for HwEncoder {}

#[derive(Clone, Copy)]
enum VaapiHevcMode {
    Bitrate,
    Cqp(i64),
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

            #[cfg(target_os = "linux")]
            if matches!(self.kind, HwKind::Vaapi) && !self.is_cqp {
                let key = std::ffi::CString::new("b").unwrap();
                ffi::av_opt_set_int((*ctx).priv_data, key.as_ptr(), bps as i64, 0);
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "windows")]
    fn new_amf(config: &EncoderConfig) -> Result<Self> {
        Self::new_with_vaapi_hevc_mode(config, HwKind::Amf, VaapiHevcMode::Bitrate)
    }

    #[cfg(target_os = "windows")]
    fn new_qsv(config: &EncoderConfig) -> Result<Self> {
        Self::new_with_vaapi_hevc_mode(config, HwKind::Qsv, VaapiHevcMode::Bitrate)
    }

    #[cfg(target_os = "windows")]
    fn new_mf(config: &EncoderConfig) -> Result<Self> {
        Self::new_with_vaapi_hevc_mode(config, HwKind::Mf, VaapiHevcMode::Bitrate)
    }

    /// VideoToolbox (h264_videotoolbox/hevc_videotoolbox) takes plain NV12
    /// system-memory frames and does its own internal GPU upload, exactly
    /// like the AMF/MF path on Windows — see `uses_sw_frames` and
    /// `push_frame_sw`. So construction reuses the same generic codepath as
    /// every other HwKind; no AVHWFramesContext/hw_device_ctx is built for
    /// it (that branch is skipped via `uses_sw_frames`).
    #[cfg(target_os = "macos")]
    fn new_videotoolbox(config: &EncoderConfig) -> Result<Self> {
        Self::new_with_vaapi_hevc_mode(config, HwKind::VideoToolbox, VaapiHevcMode::Bitrate)
    }

    fn new_with_vaapi_hevc_mode(
        config: &EncoderConfig,
        kind: HwKind,
        #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
        vaapi_hevc_mode: VaapiHevcMode,
    ) -> Result<Self> {
        let width = config.width as i32;
        let height = config.height as i32;

        let (codec_name, hw_type, hw_pix_fmt, device_path) = match (kind, config.codec) {
            #[cfg(target_os = "linux")]
            (HwKind::Vaapi, VideoCodec::H264) => (
                "h264_vaapi",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
                "/dev/dri/renderD128",
            ),
            #[cfg(target_os = "linux")]
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
            #[cfg(target_os = "windows")]
            (HwKind::Amf, VideoCodec::H264) => (
                "h264_amf",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
                "0",
            ),
            #[cfg(target_os = "windows")]
            (HwKind::Amf, VideoCodec::H265) => (
                "hevc_amf",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
                "0",
            ),
            #[cfg(target_os = "windows")]
            (HwKind::Qsv, VideoCodec::H264) => (
                "h264_qsv",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV,
                ffi::AVPixelFormat::AV_PIX_FMT_QSV,
                "0",
            ),
            #[cfg(target_os = "windows")]
            (HwKind::Qsv, VideoCodec::H265) => (
                "hevc_qsv",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV,
                ffi::AVPixelFormat::AV_PIX_FMT_QSV,
                "0",
            ),
            #[cfg(target_os = "windows")]
            (HwKind::Mf, VideoCodec::H264) => (
                "h264_mf",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
                "0",
            ),
            #[cfg(target_os = "windows")]
            (HwKind::Mf, VideoCodec::H265) => (
                "hevc_mf",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
                "0",
            ),
            // VideoToolbox uses uses_sw_frames = true (below), so hw_type/
            // hw_pix_fmt/device_path here are dead values: they're only read
            // inside the `if !uses_sw_frames { ... }` branch, which
            // VideoToolbox skips entirely. AV_HWDEVICE_TYPE_NONE/whatever
            // pixel format is irrelevant, but needs to type-check.
            #[cfg(target_os = "macos")]
            (HwKind::VideoToolbox, VideoCodec::H264) => (
                "h264_videotoolbox",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE,
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                "",
            ),
            #[cfg(target_os = "macos")]
            (HwKind::VideoToolbox, VideoCodec::H265) => (
                "hevc_videotoolbox",
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE,
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                "",
            ),
        };

        let kind_name = match kind {
            #[cfg(target_os = "linux")]
            HwKind::Vaapi => "VA-API",
            HwKind::Nvenc => "NVENC",
            #[cfg(target_os = "windows")]
            HwKind::Amf => "AMF",
            #[cfg(target_os = "windows")]
            HwKind::Qsv => "QSV",
            #[cfg(target_os = "windows")]
            HwKind::Mf => "MediaFoundation",
            #[cfg(target_os = "macos")]
            HwKind::VideoToolbox => "VideoToolbox",
        };

        #[cfg(target_os = "windows")]
        let uses_sw_frames = matches!(kind, HwKind::Amf | HwKind::Mf);
        // VideoToolbox takes plain NV12 system-memory frames and does its own
        // internal GPU upload, same as AMF/MF — see push_frame()'s split path.
        #[cfg(target_os = "macos")]
        let uses_sw_frames = matches!(kind, HwKind::VideoToolbox);
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let uses_sw_frames = false;

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

            if !uses_sw_frames {
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
            } else {
                // AMF/MF: feed plain NV12 system-memory frames and let the
                // encoder do its own internal GPU upload (this matches how
                // `ffmpeg -c:v h264_amf` behaves by default with no
                // -init_hw_device/hwupload — no AVHWFramesContext involved).
                // Building one explicitly, as the branch above does, fails
                // with "Could not create the texture" (E_INVALIDARG) on at
                // least some AMD iGPU/driver combinations.
                (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
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
            (*ctx).gop_size = config.gop as i32;
            (*ctx).max_b_frames = 0;

            #[cfg(target_os = "linux")]
            let is_vaapi_cqp = matches!(
                (kind, config.codec, vaapi_hevc_mode),
                (HwKind::Vaapi, VideoCodec::H265, VaapiHevcMode::Cqp(_))
            );
            #[cfg(not(target_os = "linux"))]
            let is_vaapi_cqp = false;

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
                #[cfg(target_os = "linux")]
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
                }
                #[cfg(target_os = "windows")]
                HwKind::Amf => {
                    let usage = std::ffi::CString::new("usage").unwrap();
                    let ull = std::ffi::CString::new("ultralowlatency").unwrap();
                    ffi::av_opt_set((*ctx).priv_data, usage.as_ptr(), ull.as_ptr(), 0);
                    let rc = std::ffi::CString::new("rc").unwrap();
                    let cbr = std::ffi::CString::new("cbr").unwrap();
                    ffi::av_opt_set((*ctx).priv_data, rc.as_ptr(), cbr.as_ptr(), 0);
                }
                #[cfg(target_os = "windows")]
                HwKind::Qsv => {
                    let async_depth = std::ffi::CString::new("async_depth").unwrap();
                    ffi::av_opt_set_int((*ctx).priv_data, async_depth.as_ptr(), 1, 0);
                    let low_power = std::ffi::CString::new("low_power").unwrap();
                    let one = std::ffi::CString::new("1").unwrap();
                    ffi::av_opt_set((*ctx).priv_data, low_power.as_ptr(), one.as_ptr(), 0);
                }
                #[cfg(target_os = "windows")]
                HwKind::Mf => {
                    let hw_encoding = std::ffi::CString::new("hw_encoding").unwrap();
                    let one = std::ffi::CString::new("1").unwrap();
                    ffi::av_opt_set((*ctx).priv_data, hw_encoding.as_ptr(), one.as_ptr(), 0);
                    let low_latency = std::ffi::CString::new("low_latency").unwrap();
                    ffi::av_opt_set((*ctx).priv_data, low_latency.as_ptr(), one.as_ptr(), 0);
                }
                #[cfg(target_os = "macos")]
                HwKind::VideoToolbox => {
                    // Confirmed via `ffmpeg -h encoder=h264_videotoolbox`:
                    // `-realtime <boolean> ... Hint that encoding should
                    // happen in real-time if not faster`.
                    let realtime = std::ffi::CString::new("realtime").unwrap();
                    let one = std::ffi::CString::new("1").unwrap();
                    ffi::av_opt_set((*ctx).priv_data, realtime.as_ptr(), one.as_ptr(), 0);
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

            #[cfg(target_os = "linux")]
            let is_cqp = matches!(
                (kind, config.codec, vaapi_hevc_mode),
                (HwKind::Vaapi, VideoCodec::H265, VaapiHevcMode::Cqp(_))
            );
            #[cfg(not(target_os = "linux"))]
            let is_cqp = false;

            let codec_label = match config.codec {
                VideoCodec::H264 => "H.264",
                VideoCodec::H265 => "H.265",
            };

            println!(
                "[encode] {kind_name} {codec_label} encoder: {}x{}@{} bitrate={} gop={}",
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
                mapped_frame,
                pkt,
                height,
                frame_index: 0,
                fps: config.fps.max(1),
                extradata,
                is_cqp,
                uses_sw_frames,
            })
        }
    }

    fn push_frame(
        &mut self,
        frame: &CaptureFrame<'_>,
        is_idr: bool,
    ) -> Result<Vec<EncodedAccessUnit>> {
        if self.uses_sw_frames {
            return self.push_frame_sw(frame, is_idr);
        }
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
                ffi::sws_scale(
                    self.sws,
                    (*self.sw_bgra).data.as_ptr() as *const *const u8,
                    (*self.sw_bgra).linesize.as_ptr(),
                    0,
                    self.height,
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

                ffi::sws_scale(
                    self.sws,
                    (*self.sw_bgra).data.as_ptr() as *const *const u8,
                    (*self.sw_bgra).linesize.as_ptr(),
                    0,
                    self.height,
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

    /// push_frame() path for AMF/MF (uses_sw_frames): feed a plain NV12
    /// system-memory frame directly, same as the software encoder does.
    fn push_frame_sw(
        &mut self,
        frame: &CaptureFrame<'_>,
        is_idr: bool,
    ) -> Result<Vec<EncodedAccessUnit>> {
        unsafe {
            if self.sw_nv12.is_null() {
                self.sw_nv12 = ffi::av_frame_alloc();
                if self.sw_nv12.is_null() {
                    bail!("failed to allocate NV12 frame");
                }
                (*self.sw_nv12).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
                (*self.sw_nv12).width = (*self.ctx).width;
                (*self.sw_nv12).height = (*self.ctx).height;
                let ret = ffi::av_frame_get_buffer(self.sw_nv12, 0);
                if ret < 0 {
                    ffi::av_frame_free(&mut self.sw_nv12);
                    bail!("failed to allocate NV12 buffer (error {ret})");
                }
            } else {
                let ret = ffi::av_frame_make_writable(self.sw_nv12);
                if ret < 0 {
                    bail!("av_frame_make_writable failed (error {ret})");
                }
            }

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

            let pts = (self.frame_index as i64 * 90_000) / self.fps as i64;
            (*self.sw_nv12).pts = pts;
            (*self.sw_nv12).pict_type = if is_idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };

            let ret = ffi::avcodec_send_frame(self.ctx, self.sw_nv12);
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

    fn new(config: &EncoderConfig) -> Result<Self> {
        let width = config.width as i32;
        let height = config.height as i32;

        let (enc_name, opts): (&str, Vec<(&str, &str)>) = match config.codec {
            VideoCodec::H264 => (
                "libx264",
                vec![
                    ("preset", "ultrafast"),
                    ("tune", "zerolatency"),
                    ("forced-idr", "1"),
                    ("profile", "baseline"),
                ],
            ),
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
                std::slice::from_raw_parts((*ctx).extradata, (*ctx).extradata_size as usize)
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
            if !self.sws.is_null() {
                ffi::sws_freeContext(self.sws);
            }
            if !self.ctx.is_null() {
                ffi::avcodec_free_context(&mut self.ctx);
            }
        }
    }
}

// Scratch verification test for the M1 VideoToolbox encoder path — isolated,
// no display/capture subsystem involved: just Encoder::new + encode_frame on
// synthetic BGRA frames.
#[cfg(all(test, target_os = "macos"))]
mod videotoolbox_smoke_test {
    use super::*;

    #[test]
    fn videotoolbox_encodes_first_frame_as_idr() {
        let width = 320u32;
        let height = 240u32;
        let config = EncoderConfig {
            bitrate_bps: 2_000_000,
            gop: 60,
            fps: 30,
            width,
            height,
            backend: EncoderBackend::VideoToolbox,
            codec: VideoCodec::H264,
        };

        let mut encoder = Encoder::new(config).expect("VideoToolbox encoder should construct");

        let frame_bytes = (width as usize) * (height as usize) * 4;
        let mut aus_seen = Vec::new();
        for i in 0..5u8 {
            // Synthetic BGRA content; value doesn't matter for this test, just
            // needs to be the right size and not literally identical every
            // time so a real encoder wouldn't just emit skip frames.
            let data = vec![i.wrapping_mul(17); frame_bytes];
            let capture_frame = CaptureFrame {
                width,
                height,
                data: &data,
            };
            let aus = encoder
                .encode_frame(&capture_frame, false)
                .expect("encode_frame should succeed");
            aus_seen.push(aus);
        }

        let all_aus: Vec<&EncodedAccessUnit> = aus_seen.iter().flatten().collect();
        assert!(
            !all_aus.is_empty(),
            "expected at least one encoded access unit across 5 pushed frames"
        );
        assert!(
            all_aus.iter().any(|au| !au.annex_b.is_empty()),
            "expected at least one non-empty encoded access unit"
        );
        assert!(
            all_aus[0].is_idr,
            "first encoded access unit should be an IDR/keyframe"
        );
    }

    #[test]
    fn videotoolbox_reconfigure_bitrate_changes_output_size() {
        let width = 640u32;
        let height = 480u32;
        let config = EncoderConfig {
            bitrate_bps: 300_000,
            gop: 300,
            fps: 30,
            width,
            height,
            backend: EncoderBackend::VideoToolbox,
            codec: VideoCodec::H264,
        };

        // Per-frame pseudo-random noise (changes every frame, seeded by frame
        // index) so P-frames have real residual/motion to encode instead of
        // compressing to near-zero regardless of the configured bitrate cap.
        let frame_bytes = (width as usize) * (height as usize) * 4;
        let mut encoder = Encoder::new(config).expect("construct");

        fn measure(
            encoder: &mut Encoder,
            width: u32,
            height: u32,
            frame_bytes: usize,
            seed_start: u32,
            frames: u32,
        ) -> u64 {
            let mut total = 0u64;
            for f in 0..frames {
                let seed = seed_start + f;
                let mut data = vec![0u8; frame_bytes];
                for (i, b) in data.iter_mut().enumerate() {
                    *b = ((i as u32).wrapping_mul(2654435761).wrapping_add(seed.wrapping_mul(40503)) % 256)
                        as u8;
                }
                let cf = CaptureFrame {
                    width,
                    height,
                    data: &data,
                };
                let aus = encoder.encode_frame(&cf, false).expect("encode");
                for au in &aus {
                    total += au.annex_b.len() as u64;
                }
            }
            total
        }

        let low = measure(&mut encoder, width, height, frame_bytes, 0, 15);
        encoder
            .reconfigure_bitrate(20_000_000)
            .expect("reconfigure_bitrate should succeed");
        let high = measure(&mut encoder, width, height, frame_bytes, 1000, 15);
        println!("[bitrate check] low(300kbps)={low} bytes, after reconfigure to 20mbps={high} bytes");
        assert!(
            high > low,
            "expected reconfigure_bitrate to a much higher bitrate to produce more encoded bytes over 15 frames (low={low}, high={high})"
        );
    }
}
