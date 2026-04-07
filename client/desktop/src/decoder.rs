// Hardware-accelerated video decoder using FFmpeg.
//
// Decode pipeline (zero-copy, macOS VideoToolbox BGRA):
//   Annex-B → avcodec_send_packet → avcodec_receive_frame
//   → HwFrame { AVFrame with CVPixelBufferRef in data[3] }
//   → Display thread renders via CVMetalTextureCache → QSGMetalTexture
//
// Decode pipeline (fallback / software / CUDA):
//   Annex-B → avcodec_send_packet → avcodec_receive_frame
//   → av_hwframe_transfer_data (hw → CPU NV12)
//   → sws_scale NV12 → RGBA
//   → DecodedFrame { width, height, rgba }

use std::ffi::c_void;
use std::ptr;

use anyhow::{bail, Context as _, Result};
use ffi::SwsFlags;
use ffmpeg_sys_next as ffi;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A decoded RGBA frame ready for display (CPU readback path).
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A hardware-decoded frame whose GPU surface is still alive.
/// On macOS: data[3] is a CVPixelBufferRef (BGRA, IOSurface-backed).
/// On Linux: data[3] is a VASurfaceID.
/// On Windows: data[0] is an ID3D11Texture2D*, data[1] is the array slice index.
/// Dropping this frees the AVFrame reference, releasing the GPU surface.
pub struct HwFrame {
    pub width: u32,
    pub height: u32,
    /// The hw pixel format (e.g. AV_PIX_FMT_VIDEOTOOLBOX).
    pub hw_pix_fmt: i32,
    frame: *mut ffi::AVFrame,
}

unsafe impl Send for HwFrame {}
unsafe impl Sync for HwFrame {}

impl HwFrame {
    /// Get the native surface pointer (CVPixelBufferRef on macOS, VASurfaceID on Linux,
    /// ID3D11Texture2D* on Windows).
    /// The pointer is valid as long as this HwFrame is alive.
    pub fn native_surface_ptr(&self) -> *mut c_void {
        unsafe {
            // D3D11VA stores texture in data[0], others store in data[3]
            #[cfg(target_os = "windows")]
            {
                (*self.frame).data[0] as *mut c_void
            }
            #[cfg(not(target_os = "windows"))]
            {
                (*self.frame).data[3] as *mut c_void
            }
        }
    }

    /// Get the D3D11VA texture array slice index (Windows only).
    /// Returns data[1] as intptr_t — the index into the texture array.
    pub fn native_array_index(&self) -> usize {
        unsafe { (*self.frame).data[1] as usize }
    }

    /// Get the hw_frames_ctx for accessing the underlying device context.
    pub fn hw_frames_ctx(&self) -> *mut ffi::AVBufferRef {
        unsafe { (*self.frame).hw_frames_ctx }
    }
}

impl Drop for HwFrame {
    fn drop(&mut self) {
        if !self.frame.is_null() {
            unsafe { ffi::av_frame_free(&mut self.frame) };
        }
    }
}

/// Output from the decoder: either a zero-copy GPU frame or a CPU RGBA frame.
pub enum DecodedOutput {
    /// Zero-copy: the GPU surface is still live. The render thread will
    /// create a native texture (Metal/EGL/D3D11) from it.
    HwFrame(HwFrame),
    /// Fallback: fully decoded RGBA pixels in CPU memory.
    Rgba(DecodedFrame),
}

/// Simple single-buffer pool: caller takes a pre-allocated Vec, fills it, and
/// it gets recycled when the previous frame is no longer referenced.
pub struct FrameBufferPool {
    spare: Option<Vec<u8>>,
}

impl FrameBufferPool {
    pub fn new() -> Self {
        Self { spare: None }
    }

    /// Take a buffer from the pool (or allocate a new one).
    /// The buffer is cleared but retains its capacity.
    fn take(&mut self, needed: usize) -> Vec<u8> {
        if let Some(mut buf) = self.spare.take() {
            buf.clear();
            if buf.capacity() >= needed {
                return buf;
            }
            // Capacity too small — drop and allocate fresh
        }
        Vec::with_capacity(needed)
    }

    /// Return a buffer to the pool for reuse.
    pub fn recycle(&mut self, buf: Vec<u8>) {
        // Keep the larger buffer
        if self
            .spare
            .as_ref()
            .map_or(true, |s| buf.capacity() > s.capacity())
        {
            self.spare = Some(buf);
        }
    }
}

/// Which video codec the incoming stream uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodecId {
    H264,
    H265,
}

impl CodecId {
    pub fn from_transport_id(id: u8) -> Self {
        if id == 0x01 {
            Self::H265
        } else {
            Self::H264
        }
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

pub struct VideoDecoder {
    inner: DecoderInner,
    /// Latest decoded resolution (updated every frame).
    pub last_width: u32,
    pub last_height: u32,
    /// When true, HwDecoder returns HwFrame (GPU surface stays alive) instead
    /// of doing readback + sws_scale. Enabled for VideoToolbox (macOS BGRA)
    /// and VA-API (Linux, with GPU VPP to BGRA planned).
    zero_copy: bool,
}

impl VideoDecoder {
    pub fn is_zero_copy(&self) -> bool {
        self.zero_copy
    }
}

enum DecoderInner {
    HwAccel(HwDecoder),
    Software(SwDecoder),
}

impl VideoDecoder {
    /// Create a new decoder. Tries platform-native hw acceleration first,
    /// then common accelerators, then software as last resort.
    ///
    /// Probe order:
    ///   macOS:   VideoToolbox → software
    ///   Windows: D3D11VA → CUDA/NVDEC → software
    ///   Linux:   VA-API → CUDA/NVDEC → software
    pub fn new(codec: CodecId) -> Result<Self> {
        ffi_init();

        let label = match codec {
            CodecId::H264 => "H.264",
            CodecId::H265 => "H.265",
        };

        let no_zerocopy = std::env::var_os("SCREX_NO_ZEROCOPY").is_some();
        let force_software = std::env::var_os("SCREX_FORCE_SW_DECODE").is_some();
        if force_software {
            let sw = SwDecoder::new(codec)?;
            println!("[decoder] forcing software {label} decode via SCREX_FORCE_SW_DECODE");
            return Ok(Self {
                inner: DecoderInner::Software(sw),
                last_width: 0,
                last_height: 0,
                zero_copy: false,
            });
        }

        // --- macOS: VideoToolbox ---
        #[cfg(target_os = "macos")]
        {
            if let Ok(hw) = HwDecoder::new_videotoolbox(codec) {
                let zc = !no_zerocopy;
                println!(
                    "[decoder] using VideoToolbox {label} hw decode (zero_copy={})",
                    zc
                );
                return Ok(Self {
                    inner: DecoderInner::HwAccel(hw),
                    last_width: 0,
                    last_height: 0,
                    zero_copy: zc,
                });
            }
        }

        // --- Windows: D3D11VA ---
        #[cfg(target_os = "windows")]
        {
            if let Ok(hw) = HwDecoder::new_d3d11va(codec) {
                let zc = !no_zerocopy;
                println!(
                    "[decoder] using D3D11VA {label} hw decode (zero_copy={})",
                    zc
                );
                return Ok(Self {
                    inner: DecoderInner::HwAccel(hw),
                    last_width: 0,
                    last_height: 0,
                    zero_copy: zc,
                });
            }
        }

        // --- Linux: VA-API ---
        #[cfg(target_os = "linux")]
        {
            if let Ok(hw) = HwDecoder::new_vaapi(codec) {
                let zc = !no_zerocopy;
                println!(
                    "[decoder] using VA-API {label} hw decode (zero_copy={})",
                    zc
                );
                return Ok(Self {
                    inner: DecoderInner::HwAccel(hw),
                    last_width: 0,
                    last_height: 0,
                    zero_copy: zc,
                });
            }
        }

        // --- CUDA/NVDEC (Linux + Windows) ---
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            if let Ok(hw) = HwDecoder::new_cuda(codec) {
                // CUDA zero-copy not yet implemented.
                println!("[decoder] using CUDA/NVDEC {label} hw decode (zero_copy=false)");
                return Ok(Self {
                    inner: DecoderInner::HwAccel(hw),
                    last_width: 0,
                    last_height: 0,
                    zero_copy: false,
                });
            }
        }

        // --- Software fallback (all platforms) ---
        let sw = SwDecoder::new(codec)?;
        println!("[decoder] using software {label} decode (no hw accelerator available)");
        Ok(Self {
            inner: DecoderInner::Software(sw),
            last_width: 0,
            last_height: 0,
            zero_copy: false,
        })
    }

    /// Feed one Annex-B access unit and collect any decoded frames.
    pub fn decode(
        &mut self,
        annex_b: &[u8],
        pool: &mut FrameBufferPool,
    ) -> Result<Vec<DecodedOutput>> {
        let zc = self.zero_copy;
        let frames = match &mut self.inner {
            DecoderInner::HwAccel(hw) => hw.decode(annex_b, pool, zc)?,
            DecoderInner::Software(sw) => sw.decode(annex_b, pool)?,
        };
        for f in &frames {
            match f {
                DecodedOutput::HwFrame(hw) => {
                    self.last_width = hw.width;
                    self.last_height = hw.height;
                }
                DecodedOutput::Rgba(df) => {
                    self.last_width = df.width;
                    self.last_height = df.height;
                }
            }
        }
        Ok(frames)
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        // inner types handle their own cleanup
    }
}

// ---------------------------------------------------------------------------
// FFmpeg init (once)
// ---------------------------------------------------------------------------

static FFMPEG_INIT: std::sync::Once = std::sync::Once::new();

fn ffi_init() {
    FFMPEG_INIT.call_once(|| {
        // Safety: called once, no other FFmpeg calls have happened yet.
        // avcodec_register_all / av_register_all are no-ops in FFmpeg >= 4.0
        // but ffmpeg-sys-next does not call anything implicitly.
    });
}

// ---------------------------------------------------------------------------
// Hardware-accelerated decoder (VA-API or CUDA/NVDEC)
// ---------------------------------------------------------------------------

struct HwDecoder {
    ctx: *mut ffi::AVCodecContext,
    hw_device_ctx: *mut ffi::AVBufferRef,
    hw_pix_fmt: ffi::AVPixelFormat,
    parser: *mut ffi::AVCodecParserContext,
    pkt: *mut ffi::AVPacket,
    hw_frame: *mut ffi::AVFrame,
    sw_frame: *mut ffi::AVFrame,
    sws: *mut ffi::SwsContext,
    sws_width: i32,
    sws_height: i32,
    sws_src_fmt: ffi::AVPixelFormat,
    rgba_frame: *mut ffi::AVFrame,
    /// Cached RGBA frame buffer dimensions — skip av_frame_get_buffer when unchanged.
    rgba_buf_width: i32,
    rgba_buf_height: i32,
}

unsafe impl Send for HwDecoder {}

unsafe extern "C" fn get_hw_format(
    ctx: *mut ffi::AVCodecContext,
    pix_fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if ctx.is_null() || pix_fmts.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }

    let desired_ptr = (*ctx).opaque as *const ffi::AVPixelFormat;
    if desired_ptr.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    let desired = *desired_ptr;

    let mut current = pix_fmts;
    while *current != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if *current == desired {
            return desired;
        }
        current = current.add(1);
    }

    eprintln!("[decoder] requested hw pixel format not offered by codec");
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

unsafe fn apply_low_latency_decoder_flags(ctx: *mut ffi::AVCodecContext) {
    (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
    (*ctx).flags2 |= (ffi::AV_CODEC_FLAG2_FAST | ffi::AV_CODEC_FLAG2_CHUNKS) as i32;
    (*ctx).thread_count = 1;
    (*ctx).thread_type = 0;
}

impl HwDecoder {
    /// VA-API (Linux).
    #[cfg(target_os = "linux")]
    fn new_vaapi(codec: CodecId) -> Result<Self> {
        let decoder_name = match codec {
            CodecId::H264 => "h264",
            CodecId::H265 => "hevc",
        };
        let hw_type = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI;
        let device_path = "/dev/dri/renderD128";
        Self::new_hw(decoder_name, hw_type, Some(device_path), None)
    }

    /// CUDA / NVDEC (Linux + Windows).
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn new_cuda(codec: CodecId) -> Result<Self> {
        let decoder_name = match codec {
            CodecId::H264 => "h264",
            CodecId::H265 => "hevc",
        };
        let hw_type = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA;
        let device_path = "0";
        Self::new_hw(decoder_name, hw_type, Some(device_path), None)
    }

    /// VideoToolbox (macOS).
    /// VT outputs NV12 by default; the render path handles NV12→BGRA via Metal compute.
    #[cfg(target_os = "macos")]
    fn new_videotoolbox(codec: CodecId) -> Result<Self> {
        let decoder_name = match codec {
            CodecId::H264 => "h264",
            CodecId::H265 => "hevc",
        };
        let hw_type = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX;
        Self::new_hw(decoder_name, hw_type, None, None)
    }

    /// D3D11VA (Windows).
    #[cfg(target_os = "windows")]
    fn new_d3d11va(codec: CodecId) -> Result<Self> {
        let decoder_name = match codec {
            CodecId::H264 => "h264",
            CodecId::H265 => "hevc",
        };
        // D3D11VA does not require a device path — pass NULL to use the default adapter.
        let hw_type = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA;
        Self::new_hw(decoder_name, hw_type, None, None)
    }

    fn new_hw(
        decoder_name: &str,
        hw_type: ffi::AVHWDeviceType,
        device_path: Option<&str>,
        sw_format_override: Option<ffi::AVPixelFormat>,
    ) -> Result<Self> {
        unsafe {
            let codec_cstr =
                std::ffi::CString::new(decoder_name).context("invalid decoder name")?;
            let codec = ffi::avcodec_find_decoder_by_name(codec_cstr.as_ptr());
            if codec.is_null() {
                bail!("{decoder_name} decoder not found");
            }

            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                bail!("failed to allocate decoder context");
            }

            let mut hw_pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_NONE;
            let mut config_index = 0;
            loop {
                let config = ffi::avcodec_get_hw_config(codec, config_index);
                if config.is_null() {
                    break;
                }
                if (*config).device_type == hw_type
                    && ((*config).methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32)
                        != 0
                {
                    hw_pix_fmt = (*config).pix_fmt;
                    break;
                }
                config_index += 1;
            }
            if hw_pix_fmt == ffi::AVPixelFormat::AV_PIX_FMT_NONE {
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("decoder does not expose a compatible hw pixel format");
            }

            let hw_pix_fmt_box = Box::new(hw_pix_fmt);
            (*ctx).opaque = Box::into_raw(hw_pix_fmt_box) as *mut _;
            (*ctx).get_format = Some(get_hw_format);

            apply_low_latency_decoder_flags(ctx);

            // Create hw device context
            let mut hw_device_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
            let device_cstr = device_path.map(|p| std::ffi::CString::new(p).unwrap());
            let device_ptr = device_cstr
                .as_ref()
                .map(|c| c.as_ptr())
                .unwrap_or(ptr::null());
            let ret = ffi::av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                hw_type,
                device_ptr,
                ptr::null_mut(),
                0,
            );
            if ret < 0 {
                let opaque = (*ctx).opaque as *mut ffi::AVPixelFormat;
                if !opaque.is_null() {
                    drop(Box::from_raw(opaque));
                    (*ctx).opaque = ptr::null_mut();
                }
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to create hw device context (error {ret})");
            }

            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device_ctx);

            // If caller wants a specific sw_format (e.g. BGRA for VideoToolbox
            // zero-copy), pre-create hw_frames_ctx so the decoder uses it.
            if let Some(sw_fmt) = sw_format_override {
                let frames_ref = ffi::av_hwframe_ctx_alloc(hw_device_ctx);
                if frames_ref.is_null() {
                    let opaque = (*ctx).opaque as *mut ffi::AVPixelFormat;
                    if !opaque.is_null() {
                        drop(Box::from_raw(opaque));
                        (*ctx).opaque = ptr::null_mut();
                    }
                    ffi::av_buffer_unref(&mut hw_device_ctx);
                    ffi::avcodec_free_context(&mut (ctx as *mut _));
                    bail!("failed to allocate hw_frames_ctx");
                }
                let fc = &mut *((*frames_ref).data as *mut ffi::AVHWFramesContext);
                fc.format = hw_pix_fmt;
                fc.sw_format = sw_fmt;
                // Use a reasonable initial size — the decoder will adapt to the
                // actual stream dimensions.
                fc.width = 1920;
                fc.height = 1080;
                fc.initial_pool_size = 0; // dynamic pool
                let ret = ffi::av_hwframe_ctx_init(frames_ref);
                if ret < 0 {
                    ffi::av_buffer_unref(&mut (frames_ref as *mut _));
                    let opaque = (*ctx).opaque as *mut ffi::AVPixelFormat;
                    if !opaque.is_null() {
                        drop(Box::from_raw(opaque));
                        (*ctx).opaque = ptr::null_mut();
                    }
                    ffi::av_buffer_unref(&mut hw_device_ctx);
                    ffi::avcodec_free_context(&mut (ctx as *mut _));
                    bail!("failed to init hw_frames_ctx with sw_format override (error {ret})");
                }
                (*ctx).hw_frames_ctx = ffi::av_buffer_ref(frames_ref);
                ffi::av_buffer_unref(&mut (frames_ref as *mut _));
                println!(
                    "[decoder] pre-set hw_frames_ctx with sw_format={:?}",
                    sw_fmt
                );
            }

            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                let opaque = (*ctx).opaque as *mut ffi::AVPixelFormat;
                if !opaque.is_null() {
                    drop(Box::from_raw(opaque));
                    (*ctx).opaque = ptr::null_mut();
                }
                ffi::av_buffer_unref(&mut hw_device_ctx);
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to open hw decoder (error {ret})");
            }

            // Parser for splitting Annex-B streams into packets
            let parser = ffi::av_parser_init((*codec).id as i32);
            if parser.is_null() {
                let opaque = (*ctx).opaque as *mut ffi::AVPixelFormat;
                if !opaque.is_null() {
                    drop(Box::from_raw(opaque));
                    (*ctx).opaque = ptr::null_mut();
                }
                ffi::av_buffer_unref(&mut hw_device_ctx);
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to create parser");
            }

            let pkt = ffi::av_packet_alloc();
            let hw_frame = ffi::av_frame_alloc();
            let sw_frame = ffi::av_frame_alloc();
            let rgba_frame = ffi::av_frame_alloc();

            if pkt.is_null() || hw_frame.is_null() || sw_frame.is_null() || rgba_frame.is_null() {
                bail!("failed to allocate FFmpeg structures");
            }

            Ok(Self {
                ctx,
                hw_device_ctx,
                hw_pix_fmt,
                parser,
                pkt,
                hw_frame,
                sw_frame,
                sws: ptr::null_mut(),
                sws_width: 0,
                sws_height: 0,
                sws_src_fmt: ffi::AVPixelFormat::AV_PIX_FMT_NONE,
                rgba_frame,
                rgba_buf_width: 0,
                rgba_buf_height: 0,
            })
        }
    }

    fn decode(
        &mut self,
        annex_b: &[u8],
        pool: &mut FrameBufferPool,
        zero_copy: bool,
    ) -> Result<Vec<DecodedOutput>> {
        let mut frames = Vec::new();
        unsafe {
            // Feed the complete access unit directly — no parser needed.
            (*self.pkt).data = annex_b.as_ptr() as *mut u8;
            (*self.pkt).size = annex_b.len() as i32;
            self.send_and_receive(&mut frames, pool, zero_copy)?;
            (*self.pkt).data = ptr::null_mut();
            (*self.pkt).size = 0;
        }
        Ok(frames)
    }

    unsafe fn send_and_receive(
        &mut self,
        frames: &mut Vec<DecodedOutput>,
        pool: &mut FrameBufferPool,
        zero_copy: bool,
    ) -> Result<()> {
        let ret = ffi::avcodec_send_packet(self.ctx, self.pkt);
        if ret < 0 {
            if ret != ffi::AVERROR(libc::EAGAIN) && ret != ffi::AVERROR_EOF {
                bail!("avcodec_send_packet error {ret}");
            }
        }
        self.receive_frames(frames, pool, zero_copy)
    }

    unsafe fn receive_frames(
        &mut self,
        frames: &mut Vec<DecodedOutput>,
        pool: &mut FrameBufferPool,
        zero_copy: bool,
    ) -> Result<()> {
        loop {
            let ret = ffi::avcodec_receive_frame(self.ctx, self.hw_frame);
            if ret == ffi::AVERROR(libc::EAGAIN) || ret == ffi::AVERROR_EOF {
                break;
            }
            if ret < 0 {
                bail!("avcodec_receive_frame error {ret}");
            }

            let frame_fmt = std::mem::transmute::<i32, ffi::AVPixelFormat>((*self.hw_frame).format);
            let is_hw = frame_fmt == self.hw_pix_fmt;

            // --- Zero-copy path: clone the AVFrame to keep the GPU surface alive ---
            if zero_copy && is_hw {
                let cloned = ffi::av_frame_clone(self.hw_frame);
                if cloned.is_null() {
                    ffi::av_frame_unref(self.hw_frame);
                    bail!("av_frame_clone failed for zero-copy path");
                }
                let w = (*cloned).width as u32;
                let h = (*cloned).height as u32;
                frames.push(DecodedOutput::HwFrame(HwFrame {
                    width: w,
                    height: h,
                    hw_pix_fmt: (*cloned).format,
                    frame: cloned,
                }));
                ffi::av_frame_unref(self.hw_frame);
                continue;
            }

            // --- Readback path: GPU→CPU→sws_scale→RGBA ---
            let src_frame = if is_hw {
                ffi::av_frame_unref(self.sw_frame);
                let ret = ffi::av_hwframe_transfer_data(self.sw_frame, self.hw_frame, 0);
                if ret < 0 {
                    ffi::av_frame_unref(self.hw_frame);
                    bail!("av_hwframe_transfer_data error {ret}");
                }
                self.sw_frame
            } else {
                self.hw_frame
            };

            let w = (*src_frame).width;
            let h = (*src_frame).height;
            let src_fmt = std::mem::transmute::<i32, ffi::AVPixelFormat>((*src_frame).format);

            // Ensure sws context matches current resolution and pixel format.
            self.ensure_sws(w, h, src_fmt)?;

            // Reuse the RGBA frame buffer when resolution is unchanged.
            if self.rgba_buf_width != w || self.rgba_buf_height != h {
                ffi::av_frame_unref(self.rgba_frame);
                (*self.rgba_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_RGB0 as i32;
                (*self.rgba_frame).width = w;
                (*self.rgba_frame).height = h;
                let ret = ffi::av_frame_get_buffer(self.rgba_frame, 32);
                if ret < 0 {
                    bail!("failed to allocate RGBA frame buffer");
                }
                self.rgba_buf_width = w;
                self.rgba_buf_height = h;
            }
            // Make the frame writable (refcount == 1) so sws_scale can write into it.
            ffi::av_frame_make_writable(self.rgba_frame);

            ffi::sws_scale(
                self.sws,
                (*src_frame).data.as_ptr() as *const *const u8,
                (*src_frame).linesize.as_ptr(),
                0,
                h,
                (*self.rgba_frame).data.as_mut_ptr(),
                (*self.rgba_frame).linesize.as_mut_ptr(),
            );

            // Copy RGBA data into a pooled buffer (avoids per-frame heap allocation)
            let stride = (*self.rgba_frame).linesize[0] as usize;
            let row_bytes = w as usize * 4;
            let height = h as usize;
            let src_ptr = (*self.rgba_frame).data[0];
            let needed = row_bytes * height;
            let mut rgba = pool.take(needed);
            if stride == row_bytes {
                rgba.extend_from_slice(std::slice::from_raw_parts(src_ptr, needed));
            } else {
                for row in 0..height {
                    rgba.extend_from_slice(std::slice::from_raw_parts(
                        src_ptr.add(row * stride),
                        row_bytes,
                    ));
                }
            }

            frames.push(DecodedOutput::Rgba(DecodedFrame {
                width: w as u32,
                height: h as u32,
                rgba,
            }));

            ffi::av_frame_unref(self.hw_frame);
            if src_frame == self.sw_frame {
                ffi::av_frame_unref(self.sw_frame);
            }
        }
        Ok(())
    }

    unsafe fn ensure_sws(&mut self, w: i32, h: i32, src_fmt: ffi::AVPixelFormat) -> Result<()> {
        if !self.sws.is_null()
            && self.sws_width == w
            && self.sws_height == h
            && self.sws_src_fmt == src_fmt
        {
            return Ok(());
        }
        if !self.sws.is_null() {
            ffi::sws_freeContext(self.sws);
            self.sws = ptr::null_mut();
        }
        self.sws = ffi::sws_getContext(
            w,
            h,
            src_fmt,
            w,
            h,
            ffi::AVPixelFormat::AV_PIX_FMT_RGB0,
            SwsFlags::SWS_FAST_BILINEAR as i32,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        );
        if self.sws.is_null() {
            bail!("failed to create sws context for {w}x{h}");
        }
        self.sws_width = w;
        self.sws_height = h;
        self.sws_src_fmt = src_fmt;
        Ok(())
    }
}

impl Drop for HwDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.sws.is_null() {
                ffi::sws_freeContext(self.sws);
            }
            ffi::av_frame_free(&mut self.rgba_frame);
            ffi::av_frame_free(&mut self.sw_frame);
            ffi::av_frame_free(&mut self.hw_frame);
            ffi::av_packet_free(&mut self.pkt);
            ffi::av_parser_close(self.parser);
            ffi::av_buffer_unref(&mut self.hw_device_ctx);
            let opaque = (*self.ctx).opaque as *mut ffi::AVPixelFormat;
            if !opaque.is_null() {
                drop(Box::from_raw(opaque));
                (*self.ctx).opaque = ptr::null_mut();
            }
            ffi::avcodec_free_context(&mut self.ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// Software decoder (fallback)
// ---------------------------------------------------------------------------

struct SwDecoder {
    ctx: *mut ffi::AVCodecContext,
    parser: *mut ffi::AVCodecParserContext,
    pkt: *mut ffi::AVPacket,
    frame: *mut ffi::AVFrame,
    sws: *mut ffi::SwsContext,
    sws_width: i32,
    sws_height: i32,
    sws_src_fmt: ffi::AVPixelFormat,
    rgba_frame: *mut ffi::AVFrame,
    /// Cached RGBA frame buffer dimensions — skip av_frame_get_buffer when unchanged.
    rgba_buf_width: i32,
    rgba_buf_height: i32,
}

unsafe impl Send for SwDecoder {}

impl SwDecoder {
    fn new(codec: CodecId) -> Result<Self> {
        let decoder_name = match codec {
            CodecId::H264 => "h264",
            CodecId::H265 => "hevc",
        };
        unsafe {
            let codec_cstr = std::ffi::CString::new(decoder_name).unwrap();
            let codec = ffi::avcodec_find_decoder_by_name(codec_cstr.as_ptr());
            if codec.is_null() {
                bail!("{decoder_name} software decoder not found");
            }

            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                bail!("failed to allocate sw decoder context");
            }

            apply_low_latency_decoder_flags(ctx);

            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to open sw decoder (error {ret})");
            }

            let parser = ffi::av_parser_init((*codec).id as i32);
            if parser.is_null() {
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to create sw parser");
            }

            let pkt = ffi::av_packet_alloc();
            let frame = ffi::av_frame_alloc();
            let rgba_frame = ffi::av_frame_alloc();

            if pkt.is_null() || frame.is_null() || rgba_frame.is_null() {
                bail!("failed to allocate FFmpeg structures");
            }

            Ok(Self {
                ctx,
                parser,
                pkt,
                frame,
                sws: ptr::null_mut(),
                sws_width: 0,
                sws_height: 0,
                sws_src_fmt: ffi::AVPixelFormat::AV_PIX_FMT_NONE,
                rgba_frame,
                rgba_buf_width: 0,
                rgba_buf_height: 0,
            })
        }
    }

    fn decode(&mut self, annex_b: &[u8], pool: &mut FrameBufferPool) -> Result<Vec<DecodedOutput>> {
        let mut frames = Vec::new();
        unsafe {
            // Feed the complete access unit directly without re-parsing.
            (*self.pkt).data = annex_b.as_ptr() as *mut u8;
            (*self.pkt).size = annex_b.len() as i32;
            self.send_and_receive(&mut frames, pool)?;
            // Do NOT call av_packet_unref here — we don't own the data pointer.
            (*self.pkt).data = ptr::null_mut();
            (*self.pkt).size = 0;
        }
        Ok(frames)
    }

    unsafe fn send_and_receive(
        &mut self,
        frames: &mut Vec<DecodedOutput>,
        pool: &mut FrameBufferPool,
    ) -> Result<()> {
        let ret = ffi::avcodec_send_packet(self.ctx, self.pkt);
        if ret < 0 {
            if ret != ffi::AVERROR(libc::EAGAIN) && ret != ffi::AVERROR_EOF {
                bail!("sw avcodec_send_packet error {ret}");
            }
        }
        self.receive_frames(frames, pool)
    }

    unsafe fn receive_frames(
        &mut self,
        frames: &mut Vec<DecodedOutput>,
        pool: &mut FrameBufferPool,
    ) -> Result<()> {
        loop {
            let ret = ffi::avcodec_receive_frame(self.ctx, self.frame);
            if ret == ffi::AVERROR(libc::EAGAIN) || ret == ffi::AVERROR_EOF {
                break;
            }
            if ret < 0 {
                bail!("sw avcodec_receive_frame error {ret}");
            }

            let w = (*self.frame).width;
            let h = (*self.frame).height;
            let src_fmt = std::mem::transmute::<i32, ffi::AVPixelFormat>((*self.frame).format);

            self.ensure_sws(w, h, src_fmt)?;

            // Reuse the RGBA frame buffer when resolution is unchanged.
            if self.rgba_buf_width != w || self.rgba_buf_height != h {
                ffi::av_frame_unref(self.rgba_frame);
                (*self.rgba_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_RGBA as i32;
                (*self.rgba_frame).width = w;
                (*self.rgba_frame).height = h;
                let ret = ffi::av_frame_get_buffer(self.rgba_frame, 32);
                if ret < 0 {
                    bail!("failed to allocate sw RGBA frame buffer");
                }
                self.rgba_buf_width = w;
                self.rgba_buf_height = h;
            }
            ffi::av_frame_make_writable(self.rgba_frame);

            ffi::sws_scale(
                self.sws,
                (*self.frame).data.as_ptr() as *const *const u8,
                (*self.frame).linesize.as_ptr(),
                0,
                h,
                (*self.rgba_frame).data.as_mut_ptr(),
                (*self.rgba_frame).linesize.as_mut_ptr(),
            );

            let stride = (*self.rgba_frame).linesize[0] as usize;
            let row_bytes = w as usize * 4;
            let height = h as usize;
            let src_ptr = (*self.rgba_frame).data[0];
            let needed = row_bytes * height;
            let mut rgba = pool.take(needed);
            if stride == row_bytes {
                rgba.extend_from_slice(std::slice::from_raw_parts(src_ptr, needed));
            } else {
                for row in 0..height {
                    rgba.extend_from_slice(std::slice::from_raw_parts(
                        src_ptr.add(row * stride),
                        row_bytes,
                    ));
                }
            }

            frames.push(DecodedOutput::Rgba(DecodedFrame {
                width: w as u32,
                height: h as u32,
                rgba,
            }));

            ffi::av_frame_unref(self.frame);
        }
        Ok(())
    }

    unsafe fn ensure_sws(&mut self, w: i32, h: i32, src_fmt: ffi::AVPixelFormat) -> Result<()> {
        if !self.sws.is_null()
            && self.sws_width == w
            && self.sws_height == h
            && self.sws_src_fmt == src_fmt
        {
            return Ok(());
        }
        if !self.sws.is_null() {
            ffi::sws_freeContext(self.sws);
            self.sws = ptr::null_mut();
        }
        self.sws = ffi::sws_getContext(
            w,
            h,
            src_fmt,
            w,
            h,
            ffi::AVPixelFormat::AV_PIX_FMT_RGBA,
            SwsFlags::SWS_FAST_BILINEAR as i32,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        );
        if self.sws.is_null() {
            bail!("failed to create sw sws context for {w}x{h}");
        }
        self.sws_width = w;
        self.sws_height = h;
        self.sws_src_fmt = src_fmt;
        Ok(())
    }
}

impl Drop for SwDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.sws.is_null() {
                ffi::sws_freeContext(self.sws);
            }
            ffi::av_frame_free(&mut self.rgba_frame);
            ffi::av_frame_free(&mut self.frame);
            ffi::av_packet_free(&mut self.pkt);
            ffi::av_parser_close(self.parser);
            ffi::avcodec_free_context(&mut self.ctx);
        }
    }
}
