// Qt Quick video surface backed by the scene graph.
//
// Renders decoded frames via:
//   - Zero-copy (macOS): CVPixelBuffer → CVMetalTextureCache → MTLTexture → QSGMetalTexture
//   - Zero-copy (Linux): VASurface → DMA-BUF → EGLImage → GL texture → QSGOpenGLTexture
//   - Zero-copy (Windows): D3D11VA texture → Video Processor Blt → BGRA → QSGD3D11Texture
//   - Fallback (all):    RGBA pixels → QImage → createTextureFromImage

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use cpp::cpp;
use qmetaobject::prelude::*;

use crate::decoder::HwFrame;

cpp! {{
    #include <QtGui/QImage>
    #include <QtQuick/QQuickItem>
    #include <QtQuick/QQuickWindow>
    #include <QtQuick/QSGImageNode>
    #include <QtQuick/QSGTexture>
    #include <QtQuick/QSGRendererInterface>
}}

// Platform-specific includes for zero-copy render paths.
// Use C preprocessor guards because cpp! blocks are always emitted
// regardless of Rust #[cfg] attributes.
cpp! {{
    #include <QtQuick/qsgtexture_platform.h>
    #ifdef __APPLE__
    #include <CoreVideo/CoreVideo.h>
    #include <Metal/Metal.h>
    #endif
    #ifdef __linux__
    #include <EGL/egl.h>
    #include <EGL/eglext.h>
    #include <GLES2/gl2.h>
    #include <GLES2/gl2ext.h>
    // VA-API headers
    #include <va/va.h>
    #include <va/va_drmcommon.h>
    // For the FFmpeg AVHWFramesContext/AVVAAPIDeviceContext structs
    extern "C" {
    #include <libavutil/hwcontext.h>
    #include <libavutil/hwcontext_vaapi.h>
    }
    #include <unistd.h>  // close()
    #include <drm_fourcc.h>
    #endif
    #ifdef _WIN32
    #include <d3d11.h>
    #include <dxgi.h>
    // FFmpeg D3D11VA hardware context
    extern "C" {
    #include <libavutil/hwcontext.h>
    #include <libavutil/hwcontext_d3d11va.h>
    }
    #endif
}}

pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A frame ready for display: either GPU-resident (zero-copy) or CPU RGBA.
pub enum DisplayFrame {
    Rgba(RawFrame),
    Hw(HwFrame),
}

impl DisplayFrame {
    pub fn width(&self) -> u32 {
        match self {
            DisplayFrame::Rgba(f) => f.width,
            DisplayFrame::Hw(f) => f.width,
        }
    }
    pub fn height(&self) -> u32 {
        match self {
            DisplayFrame::Rgba(f) => f.height,
            DisplayFrame::Hw(f) => f.height,
        }
    }
}

/// Lock-free frame slot: writer publishes Arc<DisplayFrame> via atomic pointer swap.
/// Reader takes the latest frame without blocking the writer.
pub struct FrameSlot {
    ptr: AtomicPtr<Arc<DisplayFrame>>,
}

impl FrameSlot {
    pub fn new() -> Self {
        Self {
            ptr: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    /// Publish a new frame. Returns immediately without blocking.
    pub fn publish(&self, frame: Arc<DisplayFrame>) {
        let boxed = Box::into_raw(Box::new(frame));
        let old = self.ptr.swap(boxed, Ordering::AcqRel);
        if !old.is_null() {
            unsafe { drop(Box::from_raw(old)) };
        }
    }

    /// Take the latest frame if one is available. Non-blocking.
    pub fn take_latest(&self) -> Option<Arc<DisplayFrame>> {
        let ptr = self.ptr.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { *Box::from_raw(ptr) })
        }
    }
}

impl Drop for FrameSlot {
    fn drop(&mut self) {
        let ptr = self.ptr.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !ptr.is_null() {
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

unsafe impl Send for FrameSlot {}
unsafe impl Sync for FrameSlot {}

pub type FrameSlotRef = Arc<FrameSlot>;

static GLOBAL_FRAME_SLOT: OnceLock<FrameSlotRef> = OnceLock::new();
/// Set to true by the decoder thread when a new frame is published.
/// The QML timer's poll_frame() checks this to decide whether to call update().
static HAS_NEW_FRAME: AtomicBool = AtomicBool::new(false);
static UPDATE_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn init_global_frame_slot() -> FrameSlotRef {
    if let Some(existing) = GLOBAL_FRAME_SLOT.get() {
        return existing.clone();
    }
    let slot = Arc::new(FrameSlot::new());
    let _ = GLOBAL_FRAME_SLOT.set(slot.clone());
    slot
}

fn global_frame_slot() -> Option<&'static FrameSlotRef> {
    GLOBAL_FRAME_SLOT.get()
}

pub fn global_frame_slot_clone() -> FrameSlotRef {
    init_global_frame_slot()
}

/// Called by the decoder thread after publishing a new frame to the slot.
/// Sets a flag that the QML-side timer polls to trigger scene graph updates.
pub fn request_video_surface_update() {
    HAS_NEW_FRAME.store(true, Ordering::Release);
}

// ---------------------------------------------------------------------------
// RGBA upload path (fallback)
// ---------------------------------------------------------------------------

fn render_rgba_frame(
    frame: &RawFrame,
    raw_node: *mut c_void,
    item_ptr: *mut c_void,
    dest_x: f64,
    dest_y: f64,
    dest_w: f64,
    dest_h: f64,
) -> *mut c_void {
    let w = frame.width as i32;
    let h = frame.height as i32;
    let data_ptr = frame.rgba.as_ptr();
    let mut out_node = raw_node;

    cpp!(unsafe [
        mut out_node as "void*",
        item_ptr as "QQuickItem*",
        data_ptr as "const uchar*",
        w as "int",
        h as "int",
        dest_x as "double",
        dest_y as "double",
        dest_w as "double",
        dest_h as "double"
    ] {
        if (!item_ptr) return;
        auto window = item_ptr->window();
        if (!window) return;

        auto imageNode = static_cast<QSGImageNode*>((QSGNode*)out_node);
        if (!imageNode) {
            imageNode = window->createImageNode();
            if (!imageNode) return;
            imageNode->setOwnsTexture(true);
            imageNode->setFiltering(QSGTexture::Linear);
        }

        QImage image(data_ptr, w, h, w * 4, QImage::Format_RGBX8888);
        auto texture = window->createTextureFromImage(image, QQuickWindow::TextureIsOpaque);
        if (texture) {
            texture->setFiltering(QSGTexture::Linear);
            imageNode->setTexture(texture);
        }

        imageNode->setRect(dest_x, dest_y, dest_w, dest_h);
        imageNode->setSourceRect(0, 0, w, h);
        out_node = (void*)imageNode;
    });

    out_node
}

// ---------------------------------------------------------------------------
// macOS zero-copy: CVPixelBuffer → CVMetalTextureCache → NV12 Y+UV textures
//                  → Metal compute shader NV12→BGRA → QSGMetalTexture
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn render_hw_frame_macos(
    hw_frame: &HwFrame,
    raw_node: *mut c_void,
    item_ptr: *mut c_void,
    dest_x: f64,
    dest_y: f64,
    dest_w: f64,
    dest_h: f64,
) -> *mut c_void {
    let pixbuf = hw_frame.native_surface_ptr(); // CVPixelBufferRef
    let w = hw_frame.width as i32;
    let h = hw_frame.height as i32;
    let mut out_node = raw_node;

    cpp!(unsafe [
        mut out_node as "void*",
        item_ptr as "QQuickItem*",
        pixbuf as "void*",
        w as "int",
        h as "int",
        dest_x as "double",
        dest_y as "double",
        dest_w as "double",
        dest_h as "double"
    ] {
        #ifdef __APPLE__
        if (!item_ptr || !pixbuf) return;
        auto window = item_ptr->window();
        if (!window) return;

        // Check that Qt is using Metal
        auto ri = window->rendererInterface();
        if (!ri || ri->graphicsApi() != QSGRendererInterface::Metal) {
            return;
        }

        // Get the MTLDevice and command queue from Qt's renderer
        auto mtlDevice = static_cast<id<MTLDevice>>(
            ri->getResource(window, QSGRendererInterface::DeviceResource));
        if (!mtlDevice) return;

        // Persistent state across frames
        static CVMetalTextureCacheRef s_texCache = nullptr;
        static id<MTLComputePipelineState> s_pipeline = nil;
        static id<MTLTexture> s_bgraTexture = nil;
        static int s_bgraW = 0, s_bgraH = 0;
        // Ring buffer for CVMetalTextureRefs (Y + UV = 2 refs per frame, 2 frames deep)
        static CVMetalTextureRef s_cvTexRing[4] = {};
        static int s_ringIdx = 0;

        // --- One-time init: texture cache ---
        if (!s_texCache) {
            CVReturn ret = CVMetalTextureCacheCreate(
                kCFAllocatorDefault, nullptr, mtlDevice, nullptr, &s_texCache);
            if (ret != kCVReturnSuccess) {
                qWarning("[video_surface] CVMetalTextureCacheCreate failed: %d", ret);
                return;
            }
        }

        // --- One-time init: NV12→BGRA compute pipeline ---
        if (!s_pipeline) {
            // BT.709 limited-range NV12 → BGRA Metal compute shader
            NSString *src = @
                "#include <metal_stdlib>\n"
                "using namespace metal;\n"
                "kernel void nv12ToBgra(\n"
                "    texture2d<float, access::read>  yTex  [[texture(0)]],\n"
                "    texture2d<float, access::read>  uvTex [[texture(1)]],\n"
                "    texture2d<float, access::write> out   [[texture(2)]],\n"
                "    uint2 gid [[thread_position_in_grid]])\n"
                "{\n"
                "    if (gid.x >= out.get_width() || gid.y >= out.get_height()) return;\n"
                "    float y  = yTex.read(gid).r;\n"
                "    float2 uv = uvTex.read(uint2(gid.x / 2, gid.y / 2)).rg;\n"
                "    // BT.709 limited range: Y [16..235], UV [16..240]\n"
                "    y = (y - 16.0/255.0) * (255.0/219.0);\n"
                "    float u = uv.r - 0.5;\n"
                "    float v = uv.g - 0.5;\n"
                "    float r = y + 1.5748 * v;\n"
                "    float g = y - 0.1873 * u - 0.4681 * v;\n"
                "    float b = y + 1.8556 * u;\n"
                "    out.write(float4(clamp(r, 0.0, 1.0),\n"
                "                     clamp(g, 0.0, 1.0),\n"
                "                     clamp(b, 0.0, 1.0),\n"
                "                     1.0), gid);\n"
                "}\n";
            NSError *err = nil;
            id<MTLLibrary> lib = [mtlDevice newLibraryWithSource:src options:nil error:&err];
            if (!lib) {
                qWarning("[video_surface] Metal shader compile failed: %s",
                         [[err localizedDescription] UTF8String]);
                return;
            }
            id<MTLFunction> fn = [lib newFunctionWithName:@"nv12ToBgra"];
            s_pipeline = [mtlDevice newComputePipelineStateWithFunction:fn error:&err];
            if (!s_pipeline) {
                qWarning("[video_surface] Metal pipeline creation failed: %s",
                         [[err localizedDescription] UTF8String]);
                return;
            }
            qDebug("[video_surface] created NV12->BGRA Metal compute pipeline");
        }

        auto cvPixbuf = static_cast<CVPixelBufferRef>(pixbuf);
        size_t cvW = CVPixelBufferGetWidth(cvPixbuf);
        size_t cvH = CVPixelBufferGetHeight(cvPixbuf);
        OSType cvFmt = CVPixelBufferGetPixelFormatType(cvPixbuf);

        // Release oldest pair of CVMetalTextureRefs (from 2 frames ago)
        int slot = s_ringIdx * 2;
        for (int i = 0; i < 2; i++) {
            if (s_cvTexRing[slot + i]) {
                CFRelease(s_cvTexRing[slot + i]);
                s_cvTexRing[slot + i] = nullptr;
            }
        }

        // --- Create Metal textures from CVPixelBuffer ---
        id<MTLTexture> srcTexture = nil;
        bool isBGRA = (cvFmt == kCVPixelFormatType_32BGRA);
        bool isNV12 = (cvFmt == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ||
                       cvFmt == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange);

        if (isBGRA) {
            // Direct BGRA — single texture, no conversion needed
            CVMetalTextureRef cvTex = nullptr;
            CVReturn ret = CVMetalTextureCacheCreateTextureFromImage(
                kCFAllocatorDefault, s_texCache, cvPixbuf, nullptr,
                MTLPixelFormatBGRA8Unorm, cvW, cvH, 0, &cvTex);
            if (ret != kCVReturnSuccess || !cvTex) return;
            srcTexture = CVMetalTextureGetTexture(cvTex);
            s_cvTexRing[slot] = cvTex;
        } else if (isNV12) {
            // NV12: Y plane (R8, full res) + UV plane (RG8, half res)
            CVMetalTextureRef yTex = nullptr, uvTex = nullptr;
            CVReturn ret = CVMetalTextureCacheCreateTextureFromImage(
                kCFAllocatorDefault, s_texCache, cvPixbuf, nullptr,
                MTLPixelFormatR8Unorm, cvW, cvH, 0, &yTex);
            if (ret != kCVReturnSuccess || !yTex) return;

            ret = CVMetalTextureCacheCreateTextureFromImage(
                kCFAllocatorDefault, s_texCache, cvPixbuf, nullptr,
                MTLPixelFormatRG8Unorm, cvW / 2, cvH / 2, 1, &uvTex);
            if (ret != kCVReturnSuccess || !uvTex) {
                CFRelease(yTex);
                return;
            }

            s_cvTexRing[slot]     = yTex;
            s_cvTexRing[slot + 1] = uvTex;

            // Ensure BGRA output texture matches current resolution
            if ((int)cvW != s_bgraW || (int)cvH != s_bgraH) {
                s_bgraTexture = nil; // release old
                auto desc = [MTLTextureDescriptor
                    texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm
                    width:cvW height:cvH mipmapped:NO];
                desc.usage = MTLTextureUsageShaderWrite | MTLTextureUsageShaderRead;
                desc.storageMode = MTLStorageModePrivate;
                s_bgraTexture = [mtlDevice newTextureWithDescriptor:desc];
                s_bgraW = (int)cvW;
                s_bgraH = (int)cvH;
                qDebug("[video_surface] created %dx%d BGRA output texture", s_bgraW, s_bgraH);
            }

            // Run the NV12->BGRA compute shader
            id<MTLCommandQueue> cmdQueue = static_cast<id<MTLCommandQueue>>(
                ri->getResource(window, QSGRendererInterface::CommandQueueResource));
            if (!cmdQueue) {
                qWarning("[video_surface] no Metal command queue from Qt");
                return;
            }
            id<MTLCommandBuffer> cmdBuf = [cmdQueue commandBuffer];
            id<MTLComputeCommandEncoder> enc = [cmdBuf computeCommandEncoder];
            [enc setComputePipelineState:s_pipeline];
            [enc setTexture:CVMetalTextureGetTexture(yTex) atIndex:0];
            [enc setTexture:CVMetalTextureGetTexture(uvTex) atIndex:1];
            [enc setTexture:s_bgraTexture atIndex:2];
            MTLSize threadgroup = MTLSizeMake(16, 16, 1);
            MTLSize grid = MTLSizeMake(cvW, cvH, 1);
            [enc dispatchThreads:grid threadsPerThreadgroup:threadgroup];
            [enc endEncoding];
            [cmdBuf commit];

            srcTexture = s_bgraTexture;
        } else {
            static bool s_warned = false;
            if (!s_warned) {
                qWarning("[video_surface] unsupported CVPixelBuffer format 0x%08x", (unsigned)cvFmt);
                s_warned = true;
            }
            return;
        }

        s_ringIdx = (s_ringIdx + 1) & 1;

        if (!srcTexture) return;

        // Wrap the BGRA MTLTexture in a QSGTexture
        auto qsgTexture = QNativeInterface::QSGMetalTexture::fromNative(
            srcTexture, window, QSize(cvW, cvH), QQuickWindow::TextureIsOpaque);
        if (!qsgTexture) return;
        qsgTexture->setFiltering(QSGTexture::Linear);

        // Set up the scene graph node
        auto imageNode = static_cast<QSGImageNode*>((QSGNode*)out_node);
        if (!imageNode) {
            imageNode = window->createImageNode();
            if (!imageNode) {
                delete qsgTexture;
                return;
            }
            imageNode->setOwnsTexture(false);
            imageNode->setFiltering(QSGTexture::Linear);
        }

        auto oldTex = imageNode->texture();
        imageNode->setTexture(qsgTexture);
        if (oldTex) delete oldTex;

        imageNode->setRect(dest_x, dest_y, dest_w, dest_h);
        imageNode->setSourceRect(0, 0, cvW, cvH);
        out_node = (void*)imageNode;

        static int s_frameCount = 0;
        s_frameCount++;
        if (s_frameCount <= 5 || (s_frameCount & 0xFF) == 0) {
            qDebug("[video_surface] zero-copy frame #%d: %zux%zu %s, node=%p",
                   s_frameCount, cvW, cvH, isBGRA ? "BGRA" : "NV12->BGRA", imageNode);
        }

        CVMetalTextureCacheFlush(s_texCache, 0);
        #endif // __APPLE__
    });

    out_node
}

// ---------------------------------------------------------------------------
// Linux zero-copy: VA-API -> DMA-BUF -> EGLImage (Y+UV) -> NV12 GLSL -> FBO -> QSGOpenGLTexture
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn render_hw_frame_linux(
    hw_frame: &HwFrame,
    raw_node: *mut c_void,
    item_ptr: *mut c_void,
    dest_x: f64,
    dest_y: f64,
    dest_w: f64,
    dest_h: f64,
) -> *mut c_void {
    let surface_ptr = hw_frame.native_surface_ptr(); // VASurfaceID as void*
    let frames_ctx = hw_frame.hw_frames_ctx(); // AVBufferRef*
    let w = hw_frame.width as i32;
    let h = hw_frame.height as i32;
    let mut out_node = raw_node;

    cpp!(unsafe [
        mut out_node as "void*",
        item_ptr as "QQuickItem*",
        surface_ptr as "void*",
        frames_ctx as "void*",
        w as "int",
        h as "int",
        dest_x as "double",
        dest_y as "double",
        dest_w as "double",
        dest_h as "double"
    ] {
        #ifdef __linux__
        if (!item_ptr || !surface_ptr || !frames_ctx) return;
        auto window = item_ptr->window();
        if (!window) return;

        auto ri = window->rendererInterface();
        if (!ri || ri->graphicsApi() != QSGRendererInterface::OpenGL) {
            static bool s_warned = false;
            if (!s_warned) {
                qWarning("[video/linux] Qt not using OpenGL (api=%d)", (int)ri->graphicsApi());
                s_warned = true;
            }
            return;
        }

        // --- Extract VADisplay ---
        auto bufRef = static_cast<AVBufferRef*>(frames_ctx);
        auto hwFramesCtx = reinterpret_cast<AVHWFramesContext*>(bufRef->data);
        auto vaDevCtx = static_cast<AVVAAPIDeviceContext*>(hwFramesCtx->device_ctx->hwctx);
        VADisplay vaDisplay = vaDevCtx->display;
        VASurfaceID surfaceId = (VASurfaceID)(uintptr_t)surface_ptr;

        // --- EGL / GL function pointers (resolved once) ---
        typedef EGLImageKHR (EGLAPIENTRYP PFN_eglCreateImageKHR)(EGLDisplay, EGLContext, EGLenum, EGLClientBuffer, const EGLint*);
        typedef EGLBoolean  (EGLAPIENTRYP PFN_eglDestroyImageKHR)(EGLDisplay, EGLImageKHR);
        typedef void        (GL_APIENTRYP PFN_glEGLImageTargetTexture2DOES)(GLenum, GLeglImageOES);

        static PFN_eglCreateImageKHR           s_eglCreateImage   = nullptr;
        static PFN_eglDestroyImageKHR          s_eglDestroyImage  = nullptr;
        static PFN_glEGLImageTargetTexture2DOES s_glEGLImageTarget = nullptr;

        if (!s_eglCreateImage) {
            s_eglCreateImage   = (PFN_eglCreateImageKHR)           eglGetProcAddress("eglCreateImageKHR");
            s_eglDestroyImage  = (PFN_eglDestroyImageKHR)          eglGetProcAddress("eglDestroyImageKHR");
            s_glEGLImageTarget = (PFN_glEGLImageTargetTexture2DOES)eglGetProcAddress("glEGLImageTargetTexture2DOES");
            if (!s_eglCreateImage || !s_eglDestroyImage || !s_glEGLImageTarget) {
                qWarning("[video/linux] required EGL extensions not available");
                s_eglCreateImage = nullptr;
                return;
            }
        }

        EGLDisplay eglDisp = eglGetCurrentDisplay();
        if (eglDisp == EGL_NO_DISPLAY) { return; }

        // --- Persistent NV12->RGB conversion resources ---
        static GLuint s_prog = 0;
        static GLuint s_fbo = 0;
        static GLuint s_fboTex = 0;
        static int    s_fboW = 0, s_fboH = 0;
        static GLuint s_vbo = 0;
        static GLint  s_locTexY = -1;
        static GLint  s_locTexUV = -1;

        // Ring buffer for per-frame EGL/GL resources (2 frames deep)
        struct PlaneRes {
            EGLImageKHR imgY  = EGL_NO_IMAGE_KHR;
            EGLImageKHR imgUV = EGL_NO_IMAGE_KHR;
            GLuint texY = 0, texUV = 0;
            int fd0 = -1, fd1 = -1;
        };
        static PlaneRes s_ring[2] = {};
        static int s_ringIdx = 0;

        // --- One-time: compile NV12->RGB shader + create quad ---
        if (s_prog == 0) {
            const char* vsSrc =
                "#version 100\n"
                "attribute vec2 a_pos;\n"
                "attribute vec2 a_uv;\n"
                "varying vec2 v_uv;\n"
                "void main() {\n"
                "  gl_Position = vec4(a_pos, 0.0, 1.0);\n"
                "  v_uv = a_uv;\n"
                "}\n";
            const char* fsSrc =
                "#version 100\n"
                "precision mediump float;\n"
                "varying vec2 v_uv;\n"
                "uniform sampler2D tex_y;\n"
                "uniform sampler2D tex_uv;\n"
                "void main() {\n"
                "  float y  = texture2D(tex_y,  v_uv).r;\n"
                "  float cb = texture2D(tex_uv, v_uv).r - 0.5;\n"
                "  float cr = texture2D(tex_uv, v_uv).g - 0.5;\n"
                "  y = (y - 0.0625) * 1.1644;\n"
                "  float r = y + 1.7928 * cr;\n"
                "  float g = y - 0.2133 * cb - 0.5330 * cr;\n"
                "  float b = y + 2.1124 * cb;\n"
                "  gl_FragColor = vec4(clamp(r,0.0,1.0), clamp(g,0.0,1.0), clamp(b,0.0,1.0), 1.0);\n"
                "}\n";

            auto compileSh = [](GLenum type, const char* src) -> GLuint {
                GLuint s = glCreateShader(type);
                glShaderSource(s, 1, &src, nullptr);
                glCompileShader(s);
                GLint ok = 0; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
                if (!ok) {
                    char log[512]; glGetShaderInfoLog(s, 512, nullptr, log);
                    qWarning("[video/linux] shader compile: %s", log);
                    glDeleteShader(s); return 0;
                }
                return s;
            };
            GLuint vs = compileSh(GL_VERTEX_SHADER, vsSrc);
            GLuint fs = compileSh(GL_FRAGMENT_SHADER, fsSrc);
            if (!vs || !fs) { return; }
            s_prog = glCreateProgram();
            glAttachShader(s_prog, vs);
            glAttachShader(s_prog, fs);
            glBindAttribLocation(s_prog, 0, "a_pos");
            glBindAttribLocation(s_prog, 1, "a_uv");
            glLinkProgram(s_prog);
            glDeleteShader(vs); glDeleteShader(fs);
            GLint linked = 0; glGetProgramiv(s_prog, GL_LINK_STATUS, &linked);
            if (!linked) {
                char log[512]; glGetProgramInfoLog(s_prog, 512, nullptr, log);
                qWarning("[video/linux] shader link: %s", log);
                glDeleteProgram(s_prog); s_prog = 0; return;
            }
            s_locTexY  = glGetUniformLocation(s_prog, "tex_y");
            s_locTexUV = glGetUniformLocation(s_prog, "tex_uv");

            const float quad[] = {
                -1, -1,  0, 0,
                 1, -1,  1, 0,
                -1,  1,  0, 1,
                 1,  1,  1, 1,
            };
            glGenBuffers(1, &s_vbo);
            glBindBuffer(GL_ARRAY_BUFFER, s_vbo);
            glBufferData(GL_ARRAY_BUFFER, sizeof(quad), quad, GL_STATIC_DRAW);
            glBindBuffer(GL_ARRAY_BUFFER, 0);

            glGenFramebuffers(1, &s_fbo);
            qDebug("[video/linux] NV12->RGB shader compiled, fbo=%u prog=%u", s_fbo, s_prog);
        }

        // --- Ensure FBO texture matches current resolution ---
        if (s_fboW != w || s_fboH != h) {
            if (s_fboTex) glDeleteTextures(1, &s_fboTex);
            glGenTextures(1, &s_fboTex);
            glBindTexture(GL_TEXTURE_2D, s_fboTex);
            glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, w, h, 0, GL_RGBA, GL_UNSIGNED_BYTE, nullptr);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            glBindTexture(GL_TEXTURE_2D, 0);
            glBindFramebuffer(GL_FRAMEBUFFER, s_fbo);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, s_fboTex, 0);
            auto st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            if (st != GL_FRAMEBUFFER_COMPLETE) {
                qWarning("[video/linux] FBO incomplete: 0x%x", st);
                return;
            }
            s_fboW = w; s_fboH = h;
            qDebug("[video/linux] FBO resized to %dx%d tex=%u", w, h, s_fboTex);
        }

        // --- Release oldest ring slot ---
        auto& old = s_ring[s_ringIdx];
        if (old.imgY  != EGL_NO_IMAGE_KHR) s_eglDestroyImage(eglDisp, old.imgY);
        if (old.imgUV != EGL_NO_IMAGE_KHR) s_eglDestroyImage(eglDisp, old.imgUV);
        if (old.texY)  glDeleteTextures(1, &old.texY);
        if (old.texUV) glDeleteTextures(1, &old.texUV);
        if (old.fd0 >= 0) close(old.fd0);
        if (old.fd1 >= 0) close(old.fd1);
        old = {};
        old.fd0 = -1; old.fd1 = -1;

        // --- Sync + export DMA-BUF with SEPARATE_LAYERS ---
        vaSyncSurface(vaDisplay, surfaceId);

        VADRMPRIMESurfaceDescriptor desc = {};
        VAStatus vaSt = vaExportSurfaceHandle(
            vaDisplay, surfaceId,
            VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
            VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS,
            &desc);
        if (vaSt != VA_STATUS_SUCCESS) {
            qWarning("[video/linux] vaExportSurfaceHandle failed: %d", vaSt);
            return;
        }

        if (desc.num_layers < 2) {
            for (uint32_t i = 0; i < desc.num_objects; i++) close(desc.objects[i].fd);
            qWarning("[video/linux] expected 2 layers for NV12, got %u", desc.num_layers);
            return;
        }

        // Helper: import one plane as EGLImage + GL_TEXTURE_2D
        auto importPlane = [&](int layerIdx, int planeW, int planeH, uint32_t drmFmt) -> bool {
            auto& lay = desc.layers[layerIdx];
            uint32_t objIdx = lay.object_index[0];
            EGLint attr[32]; int ai2 = 0;
            attr[ai2++] = EGL_WIDTH;  attr[ai2++] = planeW;
            attr[ai2++] = EGL_HEIGHT; attr[ai2++] = planeH;
            attr[ai2++] = EGL_LINUX_DRM_FOURCC_EXT; attr[ai2++] = (EGLint)drmFmt;
            attr[ai2++] = EGL_DMA_BUF_PLANE0_FD_EXT;     attr[ai2++] = desc.objects[objIdx].fd;
            attr[ai2++] = EGL_DMA_BUF_PLANE0_OFFSET_EXT;  attr[ai2++] = (EGLint)lay.offset[0];
            attr[ai2++] = EGL_DMA_BUF_PLANE0_PITCH_EXT;   attr[ai2++] = (EGLint)lay.pitch[0];
            uint64_t mod = desc.objects[objIdx].drm_format_modifier;
            if (mod != 0 && mod != DRM_FORMAT_MOD_INVALID) {
                attr[ai2++] = EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT; attr[ai2++] = (EGLint)(mod & 0xFFFFFFFF);
                attr[ai2++] = EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT; attr[ai2++] = (EGLint)(mod >> 32);
            }
            attr[ai2++] = EGL_NONE;

            EGLImageKHR img = s_eglCreateImage(eglDisp, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, nullptr, attr);
            if (img == EGL_NO_IMAGE_KHR) {
                qWarning("[video/linux] eglCreateImage plane %d failed (err=0x%x fmt=0x%08x %dx%d)",
                         layerIdx, eglGetError(), drmFmt, planeW, planeH);
                return false;
            }
            GLuint tex = 0;
            glGenTextures(1, &tex);
            glBindTexture(GL_TEXTURE_2D, tex);
            s_glEGLImageTarget(GL_TEXTURE_2D, img);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            glBindTexture(GL_TEXTURE_2D, 0);

            if (layerIdx == 0) { old.imgY = img; old.texY = tex; }
            else                { old.imgUV = img; old.texUV = tex; }
            return true;
        };

        // Import Y plane (R8, full res) and UV plane (GR88, half res)
        if (!importPlane(0, w, h, DRM_FORMAT_R8) ||
            !importPlane(1, w / 2, h / 2, DRM_FORMAT_GR88)) {
            if (old.imgY  != EGL_NO_IMAGE_KHR) s_eglDestroyImage(eglDisp, old.imgY);
            if (old.imgUV != EGL_NO_IMAGE_KHR) s_eglDestroyImage(eglDisp, old.imgUV);
            if (old.texY)  glDeleteTextures(1, &old.texY);
            if (old.texUV) glDeleteTextures(1, &old.texUV);
            old = {}; old.fd0 = -1; old.fd1 = -1;
            for (uint32_t i = 0; i < desc.num_objects; i++) close(desc.objects[i].fd);
            return;
        }

        // Track DMA-BUF fds for cleanup
        old.fd0 = desc.objects[0].fd;
        old.fd1 = (desc.num_objects > 1) ? (int)desc.objects[1].fd : -1;
        for (uint32_t i = 2; i < desc.num_objects; i++) close(desc.objects[i].fd);
        s_ringIdx = (s_ringIdx + 1) & 1;

        // --- Save Qt's GL state ---
        GLint prevFbo = 0, prevProg = 0;
        GLint prevVp[4] = {};
        glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prevFbo);
        glGetIntegerv(GL_CURRENT_PROGRAM, &prevProg);
        glGetIntegerv(GL_VIEWPORT, prevVp);

        // --- Render NV12->RGB into FBO ---
        glBindFramebuffer(GL_FRAMEBUFFER, s_fbo);
        glViewport(0, 0, w, h);
        glDisable(GL_BLEND);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(s_prog);

        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, old.texY);
        glUniform1i(s_locTexY, 0);

        glActiveTexture(GL_TEXTURE1);
        glBindTexture(GL_TEXTURE_2D, old.texUV);
        glUniform1i(s_locTexUV, 1);

        glBindBuffer(GL_ARRAY_BUFFER, s_vbo);
        glEnableVertexAttribArray(0);
        glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 16, (void*)0);
        glEnableVertexAttribArray(1);
        glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 16, (void*)8);

        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

        glDisableVertexAttribArray(0);
        glDisableVertexAttribArray(1);
        glBindBuffer(GL_ARRAY_BUFFER, 0);

        // --- Restore Qt's GL state ---
        glBindFramebuffer(GL_FRAMEBUFFER, prevFbo);
        glUseProgram(prevProg);
        glViewport(prevVp[0], prevVp[1], prevVp[2], prevVp[3]);
        glActiveTexture(GL_TEXTURE0);

        // --- Wrap FBO output texture in QSGTexture ---
        auto qsgTex = QNativeInterface::QSGOpenGLTexture::fromNative(
            s_fboTex, window, QSize(w, h), QQuickWindow::TextureIsOpaque);
        if (!qsgTex) {
            static bool s_warnedQt = false;
            if (!s_warnedQt) {
                qWarning("[video/linux] fromNative returned null");
                s_warnedQt = true;
            }
            return;
        }
        qsgTex->setFiltering(QSGTexture::Linear);

        // --- Set up scene graph node ---
        auto imageNode = static_cast<QSGImageNode*>((QSGNode*)out_node);
        if (!imageNode) {
            imageNode = window->createImageNode();
            if (!imageNode) { delete qsgTex; return; }
            imageNode->setOwnsTexture(false);
            imageNode->setFiltering(QSGTexture::Linear);
        }
        auto prevTex = imageNode->texture();
        imageNode->setTexture(qsgTex);
        if (prevTex) delete prevTex;
        imageNode->setRect(dest_x, dest_y, dest_w, dest_h);
        out_node = (void*)imageNode;

        static int s_fc = 0;
        s_fc++;
        if (s_fc <= 5 || (s_fc & 0xFF) == 0) {
            qDebug("[video/linux] zero-copy frame #%d: %dx%d fbo=%u node=%p", s_fc, w, h, s_fboTex, imageNode);
        }
        #endif // __linux__
    });

    out_node
}
// ---------------------------------------------------------------------------
// Windows D3D11VA zero-copy: NV12 → BGRA via D3D11 Video Processor
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
fn render_hw_frame_windows(
    hw_frame: &HwFrame,
    raw_node: *mut c_void,
    item_ptr: *mut c_void,
    dest_x: f64,
    dest_y: f64,
    dest_w: f64,
    dest_h: f64,
) -> *mut c_void {
    let surface_ptr = hw_frame.native_surface_ptr(); // ID3D11Texture2D*
    let array_index = hw_frame.native_array_index() as u32;
    let hw_ctx_ptr = hw_frame.hw_frames_ctx(); // AVBufferRef*
    let w = hw_frame.width as i32;
    let h = hw_frame.height as i32;

    let mut out_node = raw_node;

    unsafe {
        cpp!([
            surface_ptr as "void*",
            array_index as "uint32_t",
            hw_ctx_ptr as "void*",
            w as "int", h as "int",
            item_ptr as "QQuickItem*",
            dest_x as "double", dest_y as "double",
            dest_w as "double", dest_h as "double",
            mut out_node as "void*"
        ] {
            #ifdef _WIN32
            if (!item_ptr || !surface_ptr || !hw_ctx_ptr) return;
            auto *window = item_ptr->window();
            if (!window) return;

            auto *nv12Tex = static_cast<ID3D11Texture2D*>(surface_ptr);
            auto *ri = window->rendererInterface();
            if (!ri || ri->graphicsApi() != QSGRendererInterface::Direct3D11) {
                static bool s_warnedApi = false;
                if (!s_warnedApi) {
                    qWarning("[video/win] Qt is not using Direct3D11 (api=%d)",
                             ri ? int(ri->graphicsApi()) : -1);
                    s_warnedApi = true;
                }
                return;
            }
            auto *qtDevice = static_cast<ID3D11Device*>(
                ri->getResource(window, QSGRendererInterface::DeviceResource));
            if (!qtDevice) {
                static bool s_warnedQtDev = false;
                if (!s_warnedQtDev) {
                    qWarning("[video/win] Qt D3D11 device not available");
                    s_warnedQtDev = true;
                }
                return;
            }

            // --- Persistent state (created once, reused across frames) ---
            static ID3D11VideoDevice           *s_videoDevice    = nullptr;
            static ID3D11VideoContext          *s_videoContext   = nullptr;
            static ID3D11VideoProcessorEnumerator *s_enumerator  = nullptr;
            static ID3D11VideoProcessor        *s_videoProc      = nullptr;
            static ID3D11Texture2D             *s_bgraTex        = nullptr;
            static ID3D11Texture2D             *s_qtSharedTex    = nullptr;
            static ID3D11Texture2D             *s_qtTex          = nullptr;
            static ID3D11VideoProcessorOutputView *s_outputView  = nullptr;
            static DXGI_FORMAT                  s_outFormat      = DXGI_FORMAT_UNKNOWN;
            static int s_outW = 0, s_outH = 0;

            // --- Get device from FFmpeg's hw context ---
            auto *avBufRef = static_cast<AVBufferRef*>(hw_ctx_ptr);
            auto *framesCtx = reinterpret_cast<AVHWFramesContext*>(avBufRef->data);
            auto *d3d11DevCtx = static_cast<AVD3D11VADeviceContext*>(framesCtx->device_ctx->hwctx);
            ID3D11Device *device = d3d11DevCtx->device;
            ID3D11DeviceContext *devCtx = d3d11DevCtx->device_context;
            D3D11_TEXTURE2D_DESC srcDesc = {};
            nv12Tex->GetDesc(&srcDesc);

            // --- One-time init: video device + video context ---
            if (!s_videoDevice) {
                HRESULT hr = device->QueryInterface(__uuidof(ID3D11VideoDevice),
                                                     reinterpret_cast<void**>(&s_videoDevice));
                if (FAILED(hr)) {
                    qWarning("[video/win] QueryInterface(ID3D11VideoDevice) failed: 0x%08lx", hr);
                    return;
                }
                hr = devCtx->QueryInterface(__uuidof(ID3D11VideoContext),
                                             reinterpret_cast<void**>(&s_videoContext));
                if (FAILED(hr)) {
                    qWarning("[video/win] QueryInterface(ID3D11VideoContext) failed: 0x%08lx", hr);
                    s_videoDevice->Release(); s_videoDevice = nullptr;
                    return;
                }
                qDebug("[video/win] D3D11 video processor interfaces obtained");
            }

            // --- Recreate enumerator + processor + output texture on resolution change ---
            if (s_outW != w || s_outH != h) {
                // Release old resources
                if (s_outputView)  { s_outputView->Release();  s_outputView  = nullptr; }
                if (s_qtTex)       { s_qtTex->Release();       s_qtTex       = nullptr; }
                if (s_qtSharedTex) { s_qtSharedTex->Release(); s_qtSharedTex = nullptr; }
                if (s_bgraTex)     { s_bgraTex->Release();     s_bgraTex     = nullptr; }
                if (s_videoProc)   { s_videoProc->Release();   s_videoProc   = nullptr; }
                if (s_enumerator)  { s_enumerator->Release();  s_enumerator  = nullptr; }

                // Create enumerator
                D3D11_VIDEO_PROCESSOR_CONTENT_DESC contentDesc = {};
                contentDesc.InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
                contentDesc.InputFrameRate   = { 30, 1 };
                contentDesc.InputWidth       = (UINT)w;
                contentDesc.InputHeight      = (UINT)h;
                contentDesc.OutputFrameRate  = { 30, 1 };
                contentDesc.OutputWidth      = (UINT)w;
                contentDesc.OutputHeight     = (UINT)h;
                contentDesc.Usage            = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;

                HRESULT hr = s_videoDevice->CreateVideoProcessorEnumerator(&contentDesc, &s_enumerator);
                if (FAILED(hr)) {
                    qWarning("[video/win] CreateVideoProcessorEnumerator failed: 0x%08lx", hr);
                    return;
                }

                // Create video processor
                hr = s_videoDevice->CreateVideoProcessor(s_enumerator, 0, &s_videoProc);
                if (FAILED(hr)) {
                    qWarning("[video/win] CreateVideoProcessor failed: 0x%08lx", hr);
                    s_enumerator->Release(); s_enumerator = nullptr;
                    return;
                }

                // Set color space: BT.709 limited → RGB full
                D3D11_VIDEO_PROCESSOR_COLOR_SPACE inputCS = {};
                inputCS.YCbCr_Matrix  = 1; // BT.709
                inputCS.Nominal_Range = 1; // 16-235
                s_videoContext->VideoProcessorSetStreamColorSpace(s_videoProc, 0, &inputCS);

                D3D11_VIDEO_PROCESSOR_COLOR_SPACE outputCS = {};
                outputCS.RGB_Range    = 0; // full range 0-255
                outputCS.YCbCr_Matrix = 1; // BT.709
                s_videoContext->VideoProcessorSetOutputColorSpace(s_videoProc, &outputCS);

                UINT rgbaSupport = 0;
                bool useRgba = SUCCEEDED(s_enumerator->CheckVideoProcessorFormat(
                    DXGI_FORMAT_R8G8B8A8_UNORM, &rgbaSupport))
                    && (rgbaSupport & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT);
                s_outFormat = useRgba ? DXGI_FORMAT_R8G8B8A8_UNORM : DXGI_FORMAT_B8G8R8A8_UNORM;

                // Create video-processor output texture.
                D3D11_TEXTURE2D_DESC outDesc = {};
                outDesc.Width            = (UINT)w;
                outDesc.Height           = (UINT)h;
                outDesc.MipLevels        = 1;
                outDesc.ArraySize        = 1;
                outDesc.Format           = s_outFormat;
                outDesc.SampleDesc.Count = 1;
                outDesc.Usage            = D3D11_USAGE_DEFAULT;
                outDesc.BindFlags        = D3D11_BIND_RENDER_TARGET;
                outDesc.MiscFlags        = qtDevice != device ? D3D11_RESOURCE_MISC_SHARED : 0;

                hr = device->CreateTexture2D(&outDesc, nullptr, &s_bgraTex);
                if (FAILED(hr)) {
                    qWarning("[video/win] CreateTexture2D (BGRA output) failed: 0x%08lx", hr);
                    s_videoProc->Release();  s_videoProc  = nullptr;
                    s_enumerator->Release(); s_enumerator = nullptr;
                    return;
                }

                if (qtDevice != device) {
                    IDXGIResource *dxgiRes = nullptr;
                    hr = s_bgraTex->QueryInterface(
                        __uuidof(IDXGIResource),
                        reinterpret_cast<void**>(&dxgiRes));
                    if (FAILED(hr) || !dxgiRes) {
                        qWarning("[video/win] QueryInterface(IDXGIResource) failed: 0x%08lx", hr);
                        s_bgraTex->Release();    s_bgraTex    = nullptr;
                        s_videoProc->Release();  s_videoProc  = nullptr;
                        s_enumerator->Release(); s_enumerator = nullptr;
                        return;
                    }

                    HANDLE sharedHandle = nullptr;
                    hr = dxgiRes->GetSharedHandle(&sharedHandle);
                    dxgiRes->Release();
                    if (FAILED(hr) || !sharedHandle) {
                        qWarning("[video/win] GetSharedHandle failed: 0x%08lx", hr);
                        s_bgraTex->Release();    s_bgraTex    = nullptr;
                        s_videoProc->Release();  s_videoProc  = nullptr;
                        s_enumerator->Release(); s_enumerator = nullptr;
                        return;
                    }

                    hr = qtDevice->OpenSharedResource(
                        sharedHandle,
                        __uuidof(ID3D11Texture2D),
                        reinterpret_cast<void**>(&s_qtSharedTex));
                    if (FAILED(hr) || !s_qtSharedTex) {
                        qWarning("[video/win] OpenSharedResource failed: 0x%08lx", hr);
                        s_bgraTex->Release();    s_bgraTex    = nullptr;
                        s_videoProc->Release();  s_videoProc  = nullptr;
                        s_enumerator->Release(); s_enumerator = nullptr;
                        return;
                    }

                }

                // Qt only ever sees a plain sampled texture on its own device.
                D3D11_TEXTURE2D_DESC qtDesc = {};
                qtDesc.Width            = (UINT)w;
                qtDesc.Height           = (UINT)h;
                qtDesc.MipLevels        = 1;
                qtDesc.ArraySize        = 1;
                qtDesc.Format           = s_outFormat;
                qtDesc.SampleDesc.Count = 1;
                qtDesc.Usage            = D3D11_USAGE_DEFAULT;
                qtDesc.BindFlags        = D3D11_BIND_SHADER_RESOURCE;
                qtDesc.MiscFlags        = 0;
                hr = qtDevice->CreateTexture2D(&qtDesc, nullptr, &s_qtTex);
                if (FAILED(hr) || !s_qtTex) {
                    qWarning("[video/win] Qt CreateTexture2D failed: 0x%08lx", hr);
                    if (s_qtSharedTex) { s_qtSharedTex->Release(); s_qtSharedTex = nullptr; }
                    s_bgraTex->Release();     s_bgraTex     = nullptr;
                    s_videoProc->Release();   s_videoProc   = nullptr;
                    s_enumerator->Release();  s_enumerator  = nullptr;
                    return;
                }

                ID3D11ShaderResourceView *testSrv = nullptr;
                hr = qtDevice->CreateShaderResourceView(s_qtTex, nullptr, &testSrv);
                if (FAILED(hr) || !testSrv) {
                    qWarning("[video/win] self-check CreateShaderResourceView failed: 0x%08lx format=%u bind=0x%x",
                             hr, (unsigned)s_outFormat, (unsigned)qtDesc.BindFlags);
                    s_qtTex->Release();       s_qtTex       = nullptr;
                    if (s_qtSharedTex) { s_qtSharedTex->Release(); s_qtSharedTex = nullptr; }
                    s_bgraTex->Release();     s_bgraTex     = nullptr;
                    s_videoProc->Release();   s_videoProc   = nullptr;
                    s_enumerator->Release();  s_enumerator  = nullptr;
                    return;
                }
                testSrv->Release();

                // Create output view
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC ovd = {};
                ovd.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
                ovd.Texture2D.MipSlice = 0;

                hr = s_videoDevice->CreateVideoProcessorOutputView(
                    s_bgraTex, s_enumerator, &ovd, &s_outputView);
                if (FAILED(hr)) {
                    qWarning("[video/win] CreateVideoProcessorOutputView failed: 0x%08lx", hr);
                    s_bgraTex->Release();    s_bgraTex    = nullptr;
                    s_videoProc->Release();  s_videoProc  = nullptr;
                    s_enumerator->Release(); s_enumerator = nullptr;
                    return;
                }

                s_outW = w;
                s_outH = h;
                qDebug("[video/win] D3D11 video processor created: %dx%d shared=%s fmt=%s",
                       w, h,
                       qtDevice != device ? "yes" : "no",
                       s_outFormat == DXGI_FORMAT_R8G8B8A8_UNORM ? "RGBA" : "BGRA");
            }

            // --- Per-frame: create input view for this array slice ---
            D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC ivd = {};
            ivd.FourCC        = 0;
            ivd.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
            ivd.Texture2D.MipSlice   = 0;
            ivd.Texture2D.ArraySlice = array_index;

            ID3D11VideoProcessorInputView *inputView = nullptr;
            HRESULT hr = s_videoDevice->CreateVideoProcessorInputView(
                nv12Tex, s_enumerator, &ivd, &inputView);
            if (FAILED(hr)) {
                qWarning("[video/win] CreateVideoProcessorInputView failed: 0x%08lx", hr);
                return;
            }

            // --- NV12 → BGRA via Video Processor Blt ---
            D3D11_VIDEO_PROCESSOR_STREAM stream = {};
            stream.Enable        = TRUE;
            stream.pInputSurface = inputView;

            RECT srcRect = { 0, 0, w, h };
            RECT dstRect = { 0, 0, w, h };
            RECT outRect = { 0, 0, w, h };
            s_videoContext->VideoProcessorSetStreamSourceRect(s_videoProc, 0, TRUE, &srcRect);
            s_videoContext->VideoProcessorSetStreamDestRect(s_videoProc, 0, TRUE, &dstRect);
            s_videoContext->VideoProcessorSetOutputTargetRect(s_videoProc, TRUE, &outRect);

            // Use FFmpeg's lock to protect the device context (it's shared)
            d3d11DevCtx->lock(d3d11DevCtx->lock_ctx);
            hr = s_videoContext->VideoProcessorBlt(s_videoProc, s_outputView, 0, 1, &stream);
            d3d11DevCtx->unlock(d3d11DevCtx->lock_ctx);

            inputView->Release();

            if (FAILED(hr)) {
                qWarning("[video/win] VideoProcessorBlt failed: 0x%08lx", hr);
                return;
            }

            auto *qtCtx = static_cast<ID3D11DeviceContext*>(
                ri->getResource(window, QSGRendererInterface::DeviceContextResource));
            if (!qtCtx || !s_qtTex) {
                static bool s_warnedQtCtx = false;
                if (!s_warnedQtCtx) {
                    qWarning("[video/win] Qt D3D11 context or sampled texture unavailable");
                    s_warnedQtCtx = true;
                }
                return;
            }

            if (qtDevice != device) {
                if (!s_qtSharedTex) {
                    static bool s_warnedQtCtx = false;
                    if (!s_warnedQtCtx) {
                        qWarning("[video/win] Qt shared texture unavailable");
                        s_warnedQtCtx = true;
                    }
                    return;
                }

                // Make the producer's writes visible before the consumer device copies.
                devCtx->Flush();
                qtCtx->CopyResource(s_qtTex, s_qtSharedTex);
                qtCtx->Flush();
            } else {
                devCtx->CopyResource(s_qtTex, s_bgraTex);
            }

            // --- Wrap sampled Qt-local texture in QSGTexture ---
            auto qsgTex = QNativeInterface::QSGD3D11Texture::fromNative(
                static_cast<void*>(s_qtTex), window, QSize(w, h),
                QQuickWindow::TextureIsOpaque);
            if (!qsgTex) {
                static bool s_warned = false;
                if (!s_warned) {
                    qWarning("[video/win] QSGD3D11Texture::fromNative returned null");
                    s_warned = true;
                }
                return;
            }
            qsgTex->setFiltering(QSGTexture::Linear);

            // --- Set up scene graph node ---
            auto imageNode = static_cast<QSGImageNode*>((QSGNode*)out_node);
            if (!imageNode) {
                imageNode = window->createImageNode();
                if (!imageNode) { delete qsgTex; return; }
                imageNode->setOwnsTexture(false);
                imageNode->setFiltering(QSGTexture::Linear);
            }
            auto prevTex = imageNode->texture();
            imageNode->setTexture(qsgTex);
            if (prevTex) delete prevTex;
            imageNode->setRect(dest_x, dest_y, dest_w, dest_h);
            out_node = (void*)imageNode;

            static int s_fc = 0;
            s_fc++;
            if (s_fc <= 5 || (s_fc & 0xFF) == 0) {
                qDebug("[video/win] zero-copy frame #%d: visible=%dx%d tex=%ux%u slice=%u node=%p",
                       s_fc, w, h, srcDesc.Width, srcDesc.Height, array_index, imageNode);
            }
            #endif // _WIN32
        });
    }

    out_node
}
// ---------------------------------------------------------------------------
// VideoSurface QQuickItem
// ---------------------------------------------------------------------------

#[derive(QObject)]
pub struct VideoSurface {
    base: qt_base_class!(trait QQuickItem),
    pub content_x: qt_property!(f64; NOTIFY content_rect_changed),
    pub content_y: qt_property!(f64; NOTIFY content_rect_changed),
    pub content_width: qt_property!(f64; NOTIFY content_rect_changed),
    pub content_height: qt_property!(f64; NOTIFY content_rect_changed),
    pub content_rect_changed: qt_signal!(),
    /// Called by QML Timer every ~16ms. Checks for a new frame and calls update() if needed.
    pub poll_frame: qt_method!(fn(&mut self)),
    current_frame: Option<Arc<DisplayFrame>>,
}

impl Default for VideoSurface {
    fn default() -> Self {
        Self {
            base: Default::default(),
            content_x: 0.0,
            content_y: 0.0,
            content_width: 0.0,
            content_height: 0.0,
            content_rect_changed: Default::default(),
            poll_frame: Default::default(),
            current_frame: None,
        }
    }
}

impl VideoSurface {
    /// Called by QML Timer at ~60Hz.  If the decoder has published a new frame,
    /// schedules a scene-graph update so `update_paint_node` runs this cycle.
    fn poll_frame(&mut self) {
        if HAS_NEW_FRAME.swap(false, Ordering::Acquire) {
            <dyn QQuickItem>::update(self);
        }
    }
}

impl QQuickItem for VideoSurface {
    fn component_complete(&mut self) {
        let item = self.get_cpp_object();
        cpp!(unsafe [item as "QQuickItem*"] {
            if (item) {
                item->setFlag(QQuickItem::ItemHasContents, true);
            }
        });
    }

    fn update_paint_node(
        &mut self,
        mut node: qmetaobject::scenegraph::SGNode<qmetaobject::scenegraph::ContainerNode>,
    ) -> qmetaobject::scenegraph::SGNode<qmetaobject::scenegraph::ContainerNode> {
        let slot = match global_frame_slot() {
            Some(s) => s,
            None => return node,
        };

        if let Some(new_frame) = slot.take_latest() {
            self.current_frame = Some(new_frame);
        }
        let frame = match self.current_frame.as_ref() {
            Some(f) => f,
            None => return node,
        };

        let w = frame.width() as i32;
        let h = frame.height() as i32;
        if w <= 0 || h <= 0 {
            return node;
        }

        let item_rect = <dyn QQuickItem>::bounding_rect(self);
        if item_rect.width <= 0.0 || item_rect.height <= 0.0 {
            return node;
        }

        // Letterbox computation
        let src_aspect = w as f64 / h as f64;
        let dst_aspect = item_rect.width / item_rect.height;
        let (dw, dh) = if src_aspect > dst_aspect {
            (item_rect.width, item_rect.width / src_aspect)
        } else {
            (item_rect.height * src_aspect, item_rect.height)
        };
        let dx = (item_rect.width - dw) / 2.0;
        let dy = (item_rect.height - dh) / 2.0;

        let dest_x = item_rect.x + dx;
        let dest_y = item_rect.y + dy;
        let dest_w = dw;
        let dest_h = dh;

        if self.content_x != dest_x
            || self.content_y != dest_y
            || self.content_width != dest_w
            || self.content_height != dest_h
        {
            self.content_x = dest_x;
            self.content_y = dest_y;
            self.content_width = dest_w;
            self.content_height = dest_h;
            self.content_rect_changed();
        }

        let item = self.get_cpp_object();

        node.raw = match frame.as_ref() {
            DisplayFrame::Hw(hw_frame) => {
                #[cfg(target_os = "macos")]
                {
                    render_hw_frame_macos(
                        hw_frame,
                        node.raw,
                        item as *mut c_void,
                        dest_x,
                        dest_y,
                        dest_w,
                        dest_h,
                    )
                }
                #[cfg(target_os = "linux")]
                {
                    render_hw_frame_linux(
                        hw_frame,
                        node.raw,
                        item as *mut c_void,
                        dest_x,
                        dest_y,
                        dest_w,
                        dest_h,
                    )
                }
                #[cfg(target_os = "windows")]
                {
                    render_hw_frame_windows(
                        hw_frame,
                        node.raw,
                        item as *mut c_void,
                        dest_x,
                        dest_y,
                        dest_w,
                        dest_h,
                    )
                }
                #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
                {
                    // No zero-copy support on this platform — should not happen
                    // because the decoder wouldn't return HwFrame.
                    eprintln!("[video_surface] HwFrame received on unsupported platform");
                    node.raw
                }
            }
            DisplayFrame::Rgba(raw_frame) => {
                if raw_frame.rgba.len() < (w as usize * h as usize * 4) {
                    return node;
                }
                render_rgba_frame(
                    raw_frame,
                    node.raw,
                    item as *mut c_void,
                    dest_x,
                    dest_y,
                    dest_w,
                    dest_h,
                )
            }
        };

        UPDATE_COUNT.fetch_add(1, Ordering::Relaxed);
        node
    }
}
