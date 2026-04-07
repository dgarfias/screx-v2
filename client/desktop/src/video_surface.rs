// Qt Quick video surface backed by the scene graph.
//
// Renders decoded frames via:
//   - Zero-copy (macOS): CVPixelBuffer → CVMetalTextureCache → MTLTexture → QSGMetalTexture
//   - Zero-copy (Linux): VASurface → DMA-BUF → EGLImage → GL texture → QSGOpenGLTexture
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
// Linux zero-copy: VA-API → DMA-BUF → EGLImage → GL texture → QSGOpenGLTexture
// (Placeholder — not yet implemented)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn render_hw_frame_linux(
    _hw_frame: &HwFrame,
    raw_node: *mut c_void,
    _item_ptr: *mut c_void,
    _dest_x: f64,
    _dest_y: f64,
    _dest_w: f64,
    _dest_h: f64,
) -> *mut c_void {
    // TODO: Implement VA-API → EGL DMA-BUF → GL texture zero-copy path
    eprintln!("[video_surface] Linux VA-API zero-copy not yet implemented, frame dropped");
    raw_node
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
                #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
