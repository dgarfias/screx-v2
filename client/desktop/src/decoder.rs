// Hardware-accelerated video decoder using FFmpeg.
//
// Decode pipeline:
//   Annex-B H.264/H.265 access unit
//   → avcodec_send_packet   (hw decoder: VA-API or CUDA/NVDEC; fallback: sw)
//   → avcodec_receive_frame  (hw surface or sw YUV)
//   → av_hwframe_transfer_data  (hw → CPU NV12, no-op for sw)
//   → sws_scale NV12 → RGBA
//   → DecodedFrame { width, height, rgba }

use std::ptr;

use anyhow::{bail, Context as _, Result};
use ffi::SwsFlags;
use ffmpeg_sys_next as ffi;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A decoded RGBA frame ready for display.
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
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
    codec: CodecId,
    /// Latest decoded resolution (updated every frame).
    pub last_width: u32,
    pub last_height: u32,
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

        // --- macOS: VideoToolbox ---
        #[cfg(target_os = "macos")]
        {
            if let Ok(hw) = HwDecoder::new_videotoolbox(codec) {
                println!("[decoder] using VideoToolbox {label} hw decode");
                return Ok(Self {
                    inner: DecoderInner::HwAccel(hw),
                    codec,
                    last_width: 0,
                    last_height: 0,
                });
            }
        }

        // --- Windows: D3D11VA ---
        #[cfg(target_os = "windows")]
        {
            if let Ok(hw) = HwDecoder::new_d3d11va(codec) {
                println!("[decoder] using D3D11VA {label} hw decode");
                return Ok(Self {
                    inner: DecoderInner::HwAccel(hw),
                    codec,
                    last_width: 0,
                    last_height: 0,
                });
            }
        }

        // --- Linux: VA-API ---
        #[cfg(target_os = "linux")]
        {
            if let Ok(hw) = HwDecoder::new_vaapi(codec) {
                println!("[decoder] using VA-API {label} hw decode");
                return Ok(Self {
                    inner: DecoderInner::HwAccel(hw),
                    codec,
                    last_width: 0,
                    last_height: 0,
                });
            }
        }

        // --- CUDA/NVDEC (Linux + Windows) ---
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            if let Ok(hw) = HwDecoder::new_cuda(codec) {
                println!("[decoder] using CUDA/NVDEC {label} hw decode");
                return Ok(Self {
                    inner: DecoderInner::HwAccel(hw),
                    codec,
                    last_width: 0,
                    last_height: 0,
                });
            }
        }

        // --- Software fallback (all platforms) ---
        let sw = SwDecoder::new(codec)?;
        println!("[decoder] using software {label} decode (no hw accelerator available)");
        Ok(Self {
            inner: DecoderInner::Software(sw),
            codec,
            last_width: 0,
            last_height: 0,
        })
    }

    /// Current codec this decoder was opened for.
    pub fn codec(&self) -> CodecId {
        self.codec
    }

    /// Feed one Annex-B access unit and collect any decoded frames.
    /// Usually produces 0 or 1 frame per call.
    pub fn decode(&mut self, annex_b: &[u8]) -> Result<Vec<DecodedFrame>> {
        let frames = match &mut self.inner {
            DecoderInner::HwAccel(hw) => hw.decode(annex_b)?,
            DecoderInner::Software(sw) => sw.decode(annex_b)?,
        };
        for f in &frames {
            self.last_width = f.width;
            self.last_height = f.height;
        }
        Ok(frames)
    }

    /// Flush any remaining buffered frames (call at end of stream).
    pub fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        match &mut self.inner {
            DecoderInner::HwAccel(hw) => hw.flush(),
            DecoderInner::Software(sw) => sw.flush(),
        }
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
    parser: *mut ffi::AVCodecParserContext,
    pkt: *mut ffi::AVPacket,
    hw_frame: *mut ffi::AVFrame,
    sw_frame: *mut ffi::AVFrame,
    sws: *mut ffi::SwsContext,
    sws_width: i32,
    sws_height: i32,
    sws_src_fmt: ffi::AVPixelFormat,
    rgba_frame: *mut ffi::AVFrame,
}

unsafe impl Send for HwDecoder {}

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
        Self::new_hw(decoder_name, hw_type, Some(device_path))
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
        Self::new_hw(decoder_name, hw_type, Some(device_path))
    }

    /// VideoToolbox (macOS).
    #[cfg(target_os = "macos")]
    fn new_videotoolbox(codec: CodecId) -> Result<Self> {
        let decoder_name = match codec {
            CodecId::H264 => "h264",
            CodecId::H265 => "hevc",
        };
        // VideoToolbox does not require a device path — pass NULL.
        let hw_type = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX;
        Self::new_hw(decoder_name, hw_type, None)
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
        Self::new_hw(decoder_name, hw_type, None)
    }

    fn new_hw(
        decoder_name: &str,
        hw_type: ffi::AVHWDeviceType,
        device_path: Option<&str>,
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

            // Allow the decoder to use incomplete frames / frame threading
            (*ctx).flags2 |= ffi::AV_CODEC_FLAG2_FAST as i32;
            (*ctx).thread_count = 1;

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
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to create hw device context (error {ret})");
            }

            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device_ctx);

            // For decoding we do NOT need hw_frames_ctx up front — the decoder
            // allocates it internally once it knows the stream dimensions.

            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                ffi::av_buffer_unref(&mut hw_device_ctx);
                ffi::avcodec_free_context(&mut (ctx as *mut _));
                bail!("failed to open hw decoder (error {ret})");
            }

            // Parser for splitting Annex-B streams into packets
            let parser = ffi::av_parser_init((*codec).id as i32);
            if parser.is_null() {
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
                parser,
                pkt,
                hw_frame,
                sw_frame,
                sws: ptr::null_mut(),
                sws_width: 0,
                sws_height: 0,
                sws_src_fmt: ffi::AVPixelFormat::AV_PIX_FMT_NONE,
                rgba_frame,
            })
        }
    }

    fn decode(&mut self, annex_b: &[u8]) -> Result<Vec<DecodedFrame>> {
        let mut frames = Vec::new();
        unsafe {
            let mut data = annex_b.as_ptr();
            let mut data_size = annex_b.len() as i32;

            while data_size > 0 {
                let consumed = ffi::av_parser_parse2(
                    self.parser,
                    self.ctx,
                    &mut (*self.pkt).data,
                    &mut (*self.pkt).size,
                    data,
                    data_size,
                    ffi::AV_NOPTS_VALUE,
                    ffi::AV_NOPTS_VALUE,
                    0,
                );
                if consumed < 0 {
                    bail!("parser error");
                }
                data = data.add(consumed as usize);
                data_size -= consumed;

                if (*self.pkt).size > 0 {
                    self.send_and_receive(&mut frames)?;
                }
            }
        }
        Ok(frames)
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        let mut frames = Vec::new();
        unsafe {
            // Flush parser
            ffi::av_parser_parse2(
                self.parser,
                self.ctx,
                &mut (*self.pkt).data,
                &mut (*self.pkt).size,
                ptr::null(),
                0,
                ffi::AV_NOPTS_VALUE,
                ffi::AV_NOPTS_VALUE,
                0,
            );
            if (*self.pkt).size > 0 {
                self.send_and_receive(&mut frames)?;
            }
            // Flush decoder
            ffi::avcodec_send_packet(self.ctx, ptr::null());
            self.receive_frames(&mut frames)?;
        }
        Ok(frames)
    }

    unsafe fn send_and_receive(&mut self, frames: &mut Vec<DecodedFrame>) -> Result<()> {
        let ret = ffi::avcodec_send_packet(self.ctx, self.pkt);
        ffi::av_packet_unref(self.pkt);
        if ret < 0 {
            // EAGAIN is OK — just means the decoder needs us to receive first
            if ret != ffi::AVERROR(libc::EAGAIN) && ret != ffi::AVERROR_EOF {
                bail!("avcodec_send_packet error {ret}");
            }
        }
        self.receive_frames(frames)
    }

    unsafe fn receive_frames(&mut self, frames: &mut Vec<DecodedFrame>) -> Result<()> {
        loop {
            let ret = ffi::avcodec_receive_frame(self.ctx, self.hw_frame);
            if ret == ffi::AVERROR(libc::EAGAIN) || ret == ffi::AVERROR_EOF {
                break;
            }
            if ret < 0 {
                bail!("avcodec_receive_frame error {ret}");
            }

            // Transfer hw surface → CPU; let FFmpeg pick the best CPU format.
            ffi::av_frame_unref(self.sw_frame);
            let ret = ffi::av_hwframe_transfer_data(self.sw_frame, self.hw_frame, 0);
            if ret < 0 {
                ffi::av_frame_unref(self.hw_frame);
                bail!("av_hwframe_transfer_data error {ret}");
            }

            let w = (*self.sw_frame).width;
            let h = (*self.sw_frame).height;
            let src_fmt = std::mem::transmute::<i32, ffi::AVPixelFormat>((*self.sw_frame).format);

            // Ensure sws context matches current resolution and pixel format.
            self.ensure_sws(w, h, src_fmt)?;

            // sws_scale NV12 → RGBA
            ffi::av_frame_unref(self.rgba_frame);
            (*self.rgba_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_RGBA as i32;
            (*self.rgba_frame).width = w;
            (*self.rgba_frame).height = h;
            let ret = ffi::av_frame_get_buffer(self.rgba_frame, 0);
            if ret < 0 {
                bail!("failed to allocate RGBA frame buffer");
            }

            ffi::sws_scale(
                self.sws,
                (*self.sw_frame).data.as_ptr() as *const *const u8,
                (*self.sw_frame).linesize.as_ptr(),
                0,
                h,
                (*self.rgba_frame).data.as_mut_ptr(),
                (*self.rgba_frame).linesize.as_mut_ptr(),
            );

            // Copy RGBA data out
            let stride = (*self.rgba_frame).linesize[0] as usize;
            let width = w as usize;
            let height = h as usize;
            let mut rgba = Vec::with_capacity(width * 4 * height);
            let src = (*self.rgba_frame).data[0];
            for row in 0..height {
                let row_start = src.add(row * stride);
                rgba.extend_from_slice(std::slice::from_raw_parts(row_start, width * 4));
            }

            frames.push(DecodedFrame {
                width: w as u32,
                height: h as u32,
                rgba,
            });

            ffi::av_frame_unref(self.hw_frame);
            ffi::av_frame_unref(self.sw_frame);
            ffi::av_frame_unref(self.rgba_frame);
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

            (*ctx).flags2 |= ffi::AV_CODEC_FLAG2_FAST as i32;
            (*ctx).thread_count = 2;

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
            })
        }
    }

    fn decode(&mut self, annex_b: &[u8]) -> Result<Vec<DecodedFrame>> {
        let mut frames = Vec::new();
        unsafe {
            let mut data = annex_b.as_ptr();
            let mut data_size = annex_b.len() as i32;

            while data_size > 0 {
                let consumed = ffi::av_parser_parse2(
                    self.parser,
                    self.ctx,
                    &mut (*self.pkt).data,
                    &mut (*self.pkt).size,
                    data,
                    data_size,
                    ffi::AV_NOPTS_VALUE,
                    ffi::AV_NOPTS_VALUE,
                    0,
                );
                if consumed < 0 {
                    bail!("sw parser error");
                }
                data = data.add(consumed as usize);
                data_size -= consumed;

                if (*self.pkt).size > 0 {
                    self.send_and_receive(&mut frames)?;
                }
            }
        }
        Ok(frames)
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        let mut frames = Vec::new();
        unsafe {
            ffi::av_parser_parse2(
                self.parser,
                self.ctx,
                &mut (*self.pkt).data,
                &mut (*self.pkt).size,
                ptr::null(),
                0,
                ffi::AV_NOPTS_VALUE,
                ffi::AV_NOPTS_VALUE,
                0,
            );
            if (*self.pkt).size > 0 {
                self.send_and_receive(&mut frames)?;
            }
            ffi::avcodec_send_packet(self.ctx, ptr::null());
            self.receive_frames(&mut frames)?;
        }
        Ok(frames)
    }

    unsafe fn send_and_receive(&mut self, frames: &mut Vec<DecodedFrame>) -> Result<()> {
        let ret = ffi::avcodec_send_packet(self.ctx, self.pkt);
        ffi::av_packet_unref(self.pkt);
        if ret < 0 {
            if ret != ffi::AVERROR(libc::EAGAIN) && ret != ffi::AVERROR_EOF {
                bail!("sw avcodec_send_packet error {ret}");
            }
        }
        self.receive_frames(frames)
    }

    unsafe fn receive_frames(&mut self, frames: &mut Vec<DecodedFrame>) -> Result<()> {
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

            ffi::av_frame_unref(self.rgba_frame);
            (*self.rgba_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_RGBA as i32;
            (*self.rgba_frame).width = w;
            (*self.rgba_frame).height = h;
            let ret = ffi::av_frame_get_buffer(self.rgba_frame, 0);
            if ret < 0 {
                bail!("failed to allocate sw RGBA frame buffer");
            }

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
            let width = w as usize;
            let height = h as usize;
            let mut rgba = Vec::with_capacity(width * 4 * height);
            let src = (*self.rgba_frame).data[0];
            for row in 0..height {
                let row_start = src.add(row * stride);
                rgba.extend_from_slice(std::slice::from_raw_parts(row_start, width * 4));
            }

            frames.push(DecodedFrame {
                width: w as u32,
                height: h as u32,
                rgba,
            });

            ffi::av_frame_unref(self.frame);
            ffi::av_frame_unref(self.rgba_frame);
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
