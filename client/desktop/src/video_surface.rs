// Qt Quick video surface backed by the scene graph.
//
// Renders decoded RGBA frames via QSGImageNode + createTextureFromImage.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use cpp::cpp;
use qmetaobject::prelude::*;
use qmetaobject::{queued_callback, QPointer};

cpp! {{
    #include <QtGui/QImage>
    #include <QtQuick/QQuickItem>
    #include <QtQuick/QQuickWindow>
    #include <QtQuick/QSGImageNode>
    #include <QtQuick/QSGTexture>
}}

pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Lock-free frame slot: writer publishes Arc<RawFrame> via atomic pointer swap.
/// Reader takes the latest frame without blocking the writer.
pub struct FrameSlot {
    ptr: AtomicPtr<Arc<RawFrame>>,
}

impl FrameSlot {
    pub fn new() -> Self {
        Self {
            ptr: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    /// Publish a new frame. Returns immediately without blocking.
    pub fn publish(&self, frame: Arc<RawFrame>) {
        let boxed = Box::into_raw(Box::new(frame));
        let old = self.ptr.swap(boxed, Ordering::AcqRel);
        if !old.is_null() {
            unsafe { drop(Box::from_raw(old)) };
        }
    }

    /// Take the latest frame if one is available. Non-blocking.
    pub fn take_latest(&self) -> Option<Arc<RawFrame>> {
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
static REQUEST_UPDATE: OnceLock<Box<dyn Fn(()) + Send + Sync>> = OnceLock::new();
static UPDATE_PENDING: AtomicBool = AtomicBool::new(false);
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

pub fn request_video_surface_update() {
    if !UPDATE_PENDING.swap(true, Ordering::AcqRel) {
        if let Some(cb) = REQUEST_UPDATE.get() {
            cb(());
        } else {
            UPDATE_PENDING.store(false, Ordering::Release);
        }
    }
}

fn clear_pending_update() {
    UPDATE_PENDING.store(false, Ordering::Release);
}

// ---------------------------------------------------------------------------
// RGBA upload path
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
    current_frame: Option<Arc<RawFrame>>,
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
            current_frame: None,
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

        let qptr = QPointer::from(&*self);
        let cb = queued_callback(move |()| {
            clear_pending_update();
            if let Some(pinned) = qptr.as_pinned() {
                let obj = pinned.borrow();
                <dyn QQuickItem>::update(&*obj);
            }
        });
        let _ = REQUEST_UPDATE.set(Box::new(cb));
    }

    fn update_paint_node(
        &mut self,
        mut node: qmetaobject::scenegraph::SGNode<qmetaobject::scenegraph::ContainerNode>,
    ) -> qmetaobject::scenegraph::SGNode<qmetaobject::scenegraph::ContainerNode> {
        clear_pending_update();
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

        let w = frame.width as i32;
        let h = frame.height as i32;
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

        if frame.rgba.len() < (w as usize * h as usize * 4) {
            return node;
        }
        let item = self.get_cpp_object();
        node.raw = render_rgba_frame(
            frame.as_ref(),
            node.raw,
            item as *mut c_void,
            dest_x,
            dest_y,
            dest_w,
            dest_h,
        );

        UPDATE_COUNT.fetch_add(1, Ordering::Relaxed);
        node
    }
}
