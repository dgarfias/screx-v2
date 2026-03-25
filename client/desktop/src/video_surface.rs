// Qt Quick video surface that displays decoded RGBA frames.
//
// This implements QQuickPaintedItem so it can be placed directly in QML.
// The backend pushes RGBA frame data into the global FRAME_SLOT, then
// the paint() method reads the latest frame and draws it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use cpp::cpp;
use qmetaobject::prelude::*;
use qmetaobject::QQuickPaintedItem;
use qttypes::{QImage, QPainter, QRectF};

cpp! {{
    #include <QtGui/QImage>
}}

// ---------------------------------------------------------------------------
// Shared frame buffer — written by the backend decode thread, read by paint()
// ---------------------------------------------------------------------------

/// A decoded video frame stored as tightly-packed RGBA pixels.
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Thread-safe frame slot. The decode thread overwrites this each frame;
/// the paint method reads the latest.
pub type FrameSlot = Arc<Mutex<Option<RawFrame>>>;

/// Global frame slot shared between the backend decode thread and all
/// VideoSurface instances. Initialized once at startup.
static GLOBAL_FRAME_SLOT: OnceLock<FrameSlot> = OnceLock::new();
static PAINT_COUNT: AtomicU64 = AtomicU64::new(0);

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

// ---------------------------------------------------------------------------
// QQuickPaintedItem implementation
// ---------------------------------------------------------------------------

#[derive(QObject)]
pub struct VideoSurface {
    base: qt_base_class!(trait QQuickItem),
}

impl Default for VideoSurface {
    fn default() -> Self {
        Self {
            base: Default::default(),
        }
    }
}

impl QQuickPaintedItem for VideoSurface {
    fn paint(&mut self, painter: &mut QPainter) {
        let slot = match global_frame_slot() {
            Some(s) => s,
            None => return,
        };
        let guard = match slot.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let frame = match guard.as_ref() {
            Some(f) => f,
            None => return,
        };

        let w = frame.width;
        let h = frame.height;
        let data_ptr = frame.rgba.as_ptr();
        let data_len = frame.rgba.len();
        let expected = (w * h * 4) as usize;
        if data_len < expected || w == 0 || h == 0 {
            return;
        }

        // Construct QImage from raw RGBA data (zero-copy, does not take ownership).
        let image: QImage = cpp!(unsafe [
            data_ptr as "const uchar *",
            w as "int",
            h as "int"
        ] -> QImage as "QImage" {
            return QImage(data_ptr, w, h, w * 4, QImage::Format_RGBA8888);
        });

        // Draw scaled to fill the item, preserving aspect ratio.
        let item_rect = (self as &dyn QQuickItem).bounding_rect();
        if item_rect.width <= 0.0 || item_rect.height <= 0.0 {
            return;
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

        let dest = QRectF {
            x: item_rect.x + dx,
            y: item_rect.y + dy,
            width: dw,
            height: dh,
        };

        let paint_count = PAINT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if paint_count == 1 || paint_count % 120 == 0 {
            println!(
                "[desktop/video] paints={} frame={}x{} item={:.0}x{:.0}",
                paint_count, w, h, item_rect.width, item_rect.height
            );
        }

        painter.draw_image_fit_rect(dest, image);
    }
}

// Force the QQuickItem trait implementation (required by QQuickPaintedItem)
impl QQuickItem for VideoSurface {}
