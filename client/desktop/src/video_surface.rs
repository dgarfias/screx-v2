// Qt Quick video surface backed by the scene graph.
//
// The backend writes decoded RGBA frames into a global frame slot, then calls
// `request_video_surface_update()`. The QQuickItem invalidates itself and the
// scene graph callback uploads the latest frame into a QSGImageNode.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use cpp::cpp;
use qmetaobject::prelude::*;
use qmetaobject::{queued_callback, QPointer};

cpp! {{
    #include <QtGui/QImage>
    #include <QtQuick/QQuickItem>
    #include <QtQuick/QQuickWindow>
    #include <QtQuick/QSGImageNode>
}}

pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub type FrameSlot = Arc<Mutex<Option<RawFrame>>>;

static GLOBAL_FRAME_SLOT: OnceLock<FrameSlot> = OnceLock::new();
static REQUEST_UPDATE: OnceLock<Box<dyn Fn(()) + Send + Sync>> = OnceLock::new();
static UPDATE_PENDING: AtomicBool = AtomicBool::new(false);
static UPDATE_COUNT: AtomicU64 = AtomicU64::new(0);
static UPDATE_SKIP_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn init_global_frame_slot() -> FrameSlot {
    if let Some(existing) = GLOBAL_FRAME_SLOT.get() {
        return existing.clone();
    }
    let slot = Arc::new(Mutex::new(None));
    let _ = GLOBAL_FRAME_SLOT.set(slot.clone());
    slot
}

fn global_frame_slot() -> Option<&'static FrameSlot> {
    GLOBAL_FRAME_SLOT.get()
}

pub fn global_frame_slot_clone() -> FrameSlot {
    init_global_frame_slot()
}

pub fn request_video_surface_update() {
    if !UPDATE_PENDING.swap(true, Ordering::AcqRel) {
        if let Some(cb) = REQUEST_UPDATE.get() {
            cb(());
        } else {
            UPDATE_PENDING.store(false, Ordering::Release);
        }
    } else {
        let skipped = UPDATE_SKIP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if skipped == 1 || skipped % 120 == 0 {
            println!("[desktop/video] coalesced pending updates={skipped}");
        }
    }
}

fn clear_pending_update() {
    UPDATE_PENDING.store(false, Ordering::Release);
}

#[derive(QObject)]
pub struct VideoSurface {
    base: qt_base_class!(trait QQuickItem),
    pub content_x: qt_property!(f64; NOTIFY content_rect_changed),
    pub content_y: qt_property!(f64; NOTIFY content_rect_changed),
    pub content_width: qt_property!(f64; NOTIFY content_rect_changed),
    pub content_height: qt_property!(f64; NOTIFY content_rect_changed),
    pub content_rect_changed: qt_signal!(),
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
        println!("[desktop/video] video surface component completed (ItemHasContents set)");
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
        let guard = match slot.lock() {
            Ok(g) => g,
            Err(_) => return node,
        };
        let frame = match guard.as_ref() {
            Some(f) => f,
            None => {
                let count = UPDATE_COUNT.load(Ordering::Relaxed);
                if count == 0 || count % 120 == 0 {
                    println!("[desktop/video] update_paint_node had no frame available");
                }
                return node;
            }
        };

        let w = frame.width as i32;
        let h = frame.height as i32;
        if w <= 0 || h <= 0 || frame.rgba.len() < (w as usize * h as usize * 4) {
            return node;
        }

        let item_rect = <dyn QQuickItem>::bounding_rect(self);
        if item_rect.width <= 0.0 || item_rect.height <= 0.0 {
            return node;
        }

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
        let data_ptr = frame.rgba.as_ptr();
        let raw = node.raw;

        let new_raw = cpp!(unsafe [
            raw as "QSGNode*",
            item as "QQuickItem*",
            data_ptr as "const uchar*",
            w as "int",
            h as "int",
            dest_x as "double",
            dest_y as "double",
            dest_w as "double",
            dest_h as "double"
        ] -> *mut c_void as "void*" {
            if (!item) return raw;
            auto window = item->window();
            if (!window) return raw;

            auto imageNode = static_cast<QSGImageNode*>(raw);
            if (!imageNode) {
                imageNode = window->createImageNode();
                if (!imageNode) return raw;
                imageNode->setOwnsTexture(true);
            }

            QImage image(data_ptr, w, h, w * 4, QImage::Format_RGBA8888);
            auto texture = window->createTextureFromImage(image, QQuickWindow::TextureIsOpaque);
            imageNode->setTexture(texture);
            imageNode->setRect(dest_x, dest_y, dest_w, dest_h);
            imageNode->setSourceRect(0, 0, w, h);
            return imageNode;
        });

        node.raw = new_raw;

        let count = UPDATE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count % 120 == 0 {
            println!(
                "[desktop/video] scenegraph updates={} frame={}x{} item={:.0}x{:.0}",
                count, w, h, item_rect.width, item_rect.height
            );
        }

        node
    }
}
