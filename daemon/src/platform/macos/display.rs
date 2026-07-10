//! macOS display backend.
//!
//! Virtual monitor via the private `CGVirtualDisplay` API, frame capture via
//! the public (if deprecated) `CGDisplayStream` API. Ported from DeskPad
//! (https://github.com/Stengo/DeskPad), adapted for a headless daemon: no
//! CFRunLoop/AppKit main thread, hiDPI forced off (1x scale, pixels==points),
//! raw BGRA bytes handed to the encoder instead of drawing into a CALayer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_core_foundation::{CFRetained, CGFloat, CGSize};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayStream, CGDisplayStreamFrameStatus,
    CGDisplayStreamUpdate, CGError, CGGetOnlineDisplayList, CGPreflightScreenCaptureAccess,
    CGRequestScreenCaptureAccess,
};
use objc2_foundation::{NSArray, NSString};
use objc2_io_surface::{IOSurfaceLockOptions, IOSurfaceRef};

use crate::capture::{CaptureFrame, DisplayBackend, DisplayMode};

/// Look up an Objective-C class by name, for the private CGVirtualDisplay*
/// API surface that has no static objc2 bindings anywhere.
fn get_class(name: &std::ffi::CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name)
        .ok_or_else(|| anyhow!("class {:?} not found (unsupported macOS version?)", name))
}

/// Shared frame buffer written by the CGDisplayStream callback (on a GCD
/// worker thread) and read by `run_capture_loop` (on the capture thread).
struct FrameSlot {
    data: Vec<u8>,
    width: u32,
    height: u32,
    seq: u64,
}

struct FrameState {
    slot: Mutex<FrameSlot>,
    cvar: Condvar,
}

impl FrameState {
    fn new(width: u32, height: u32) -> Self {
        Self {
            slot: Mutex::new(FrameSlot {
                data: vec![0u8; width as usize * height as usize * 4],
                width,
                height,
                seq: 0,
            }),
            cvar: Condvar::new(),
        }
    }
}

type FrameHandlerBlock =
    RcBlock<dyn Fn(CGDisplayStreamFrameStatus, u64, *mut IOSurfaceRef, *const CGDisplayStreamUpdate)>;

#[allow(dead_code)]
pub struct MacDisplay {
    width: u32,
    height: u32,
    fps: u32,

    display_id: Option<CGDirectDisplayID>,
    // The CGVirtualDisplay instance. Dropping this unplugs the monitor.
    virtual_display: Option<Retained<AnyObject>>,
    descriptor_queue: Option<DispatchRetained<DispatchQueue>>,
    stream_queue: Option<DispatchRetained<DispatchQueue>>,
    stream: Option<CFRetained<CGDisplayStream>>,
    frame_block: Option<FrameHandlerBlock>,
    frame_state: Option<Arc<FrameState>>,
}

// SAFETY: MacDisplay is only ever driven from a single OS thread (the
// capture thread: attach/run_capture_loop/detach all run there, in that
// order, per session). The CGDisplayStream frame callback runs on its own
// GCD queue but only touches the `Arc<FrameState>` (Mutex/Condvar guarded),
// never the ObjC display/stream objects directly. This mirrors the
// `unsafe impl Send` used for EvdiDisplay's raw handle on Linux.
unsafe impl Send for MacDisplay {}

impl MacDisplay {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        Ok(Self {
            width,
            height,
            fps,
            display_id: None,
            virtual_display: None,
            descriptor_queue: None,
            stream_queue: None,
            stream: None,
            frame_block: None,
            frame_state: None,
        })
    }
}

impl DisplayBackend for MacDisplay {
    fn attach(&mut self, mode: DisplayMode) -> Result<()> {
        // TCC preflight: fail with an actionable message instead of letting
        // this surface as a cryptic downstream CGDisplayStream error.
        if !CGPreflightScreenCaptureAccess() {
            CGRequestScreenCaptureAccess();
            bail!(
                "Screen Recording permission not granted — grant it to this binary in \
                 System Settings → Privacy & Security → Screen Recording, then restart the daemon"
            );
        }

        println!(
            "[display] creating virtual display {}x{}@{}",
            mode.width, mode.height, mode.fps
        );

        // A single serial dispatch queue backs both the descriptor (used
        // internally by CGVirtualDisplay for mode-change/termination
        // callbacks) and the display stream below. Unlike DeskPad (a GUI
        // app that hands these DispatchQueue.main, serviced by its already-
        // running CFRunLoop/AppKit main loop), this is a headless daemon
        // with no main-thread run loop — GCD services a custom serial queue
        // from its own worker-thread pool regardless, so this works without
        // one.
        let descriptor_queue = DispatchQueue::new("com.screx.daemon.display.descriptor", None);

        let descriptor_cls = get_class(c"CGVirtualDisplayDescriptor")?;
        let descriptor_alloc: Allocated<AnyObject> = unsafe { msg_send![descriptor_cls, alloc] };
        let descriptor: Retained<AnyObject> = unsafe { msg_send![descriptor_alloc, init] };

        unsafe {
            let _: () = msg_send![&*descriptor, setQueue: &*descriptor_queue];
            let name = NSString::from_str("Screx Virtual");
            let _: () = msg_send![&*descriptor, setName: &*name];
            let _: () = msg_send![&*descriptor, setMaxPixelsWide: mode.width as usize];
            let _: () = msg_send![&*descriptor, setMaxPixelsHigh: mode.height as usize];
            let _: () = msg_send![
                &*descriptor,
                setSizeInMillimeters: CGSize { width: 1600.0, height: 1000.0 }
            ];
            // Arbitrary but fixed placeholder identifiers, matching DeskPad's
            // style — the OS doesn't validate these against anything.
            let _: () = msg_send![&*descriptor, setProductID: 0x1234u32];
            let _: () = msg_send![&*descriptor, setVendorID: 0x3456u32];
            let _: () = msg_send![&*descriptor, setSerialNum: 0x0001u32];
        }

        let display_cls = get_class(c"CGVirtualDisplay")?;
        let display_alloc: Allocated<AnyObject> = unsafe { msg_send![display_cls, alloc] };
        let display_obj: Retained<AnyObject> =
            unsafe { msg_send![display_alloc, initWithDescriptor: &*descriptor] };

        let display_id: CGDirectDisplayID = unsafe { msg_send![&*display_obj, displayID] };
        println!("[display] CGVirtualDisplay created, displayID={display_id}");

        // hiDPI is forced OFF (0), unlike DeskPad which turns it on: our
        // design requires 1x scale so pixels==points, which the parallel
        // input-injection task relies on for coordinate math.
        let settings_cls = get_class(c"CGVirtualDisplaySettings")?;
        let settings_alloc: Allocated<AnyObject> = unsafe { msg_send![settings_cls, alloc] };
        let settings: Retained<AnyObject> = unsafe { msg_send![settings_alloc, init] };
        unsafe {
            let _: () = msg_send![&*settings, setHiDPI: 0u32];
        }

        let mode_cls = get_class(c"CGVirtualDisplayMode")?;
        let mode_alloc: Allocated<AnyObject> = unsafe { msg_send![mode_cls, alloc] };
        let mode_obj: Retained<AnyObject> = unsafe {
            msg_send![
                mode_alloc,
                initWithWidth: mode.width as usize,
                height: mode.height as usize,
                refreshRate: mode.fps as CGFloat,
            ]
        };
        let modes_array = NSArray::<AnyObject>::from_retained_slice(&[mode_obj]);
        unsafe {
            let _: () = msg_send![&*settings, setModes: &*modes_array];
        }

        let applied: bool = unsafe { msg_send![&*display_obj, applySettings: &*settings] };
        if !applied {
            bail!("CGVirtualDisplay applySettings: failed (returned NO)");
        }

        // Defensive poll: the trait contract requires attach() to block
        // until the OS actually sees the new monitor.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut online = false;
        while Instant::now() < deadline {
            let mut ids = [0u32; 32];
            let mut count: u32 = 0;
            let err = unsafe {
                CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count)
            };
            if err == CGError::Success && ids[..count as usize].contains(&display_id) {
                online = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !online {
            bail!(
                "virtual display {display_id} did not appear in CGGetOnlineDisplayList within 5s"
            );
        }
        println!("[display] virtual display {display_id} is online");

        // --- Frame capture via CGDisplayStream ---
        let frame_state = Arc::new(FrameState::new(mode.width, mode.height));
        let cb_state = Arc::clone(&frame_state);

        let block: FrameHandlerBlock = RcBlock::new(
            move |status: CGDisplayStreamFrameStatus,
                  _display_time: u64,
                  surface: *mut IOSurfaceRef,
                  _update: *const CGDisplayStreamUpdate| {
                if status != CGDisplayStreamFrameStatus::FrameComplete || surface.is_null() {
                    return;
                }
                // SAFETY: non-null, valid for the duration of the callback
                // per CGDisplayStream's contract.
                let surface: &IOSurfaceRef = unsafe { &*surface };
                let mut seed: u32 = 0;
                let lock_result = unsafe { surface.lock(IOSurfaceLockOptions::ReadOnly, &mut seed) };
                if lock_result != 0 {
                    return;
                }
                let w = surface.width();
                let h = surface.height();
                let bpr = surface.bytes_per_row();
                let base = surface.base_address().as_ptr() as *const u8;

                {
                    let mut slot = cb_state.slot.lock().unwrap();
                    if slot.width as usize == w && slot.height as usize == h {
                        let dst_stride = w * 4;
                        // Stride-safe row-by-row copy: bytesPerRow is
                        // virtually always > width*4 due to alignment
                        // padding. Copying the whole buffer flat instead
                        // produces a visibly sheared/skewed image.
                        for row in 0..h {
                            unsafe {
                                let src = base.add(row * bpr);
                                let dst = slot.data.as_mut_ptr().add(row * dst_stride);
                                std::ptr::copy_nonoverlapping(src, dst, dst_stride);
                            }
                        }
                        slot.seq = slot.seq.wrapping_add(1);
                    }
                }
                cb_state.cvar.notify_all();

                unsafe {
                    surface.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
                }
            },
        );
        let handler_ptr = RcBlock::as_ptr(&block);

        let stream_queue = DispatchQueue::new("com.screx.daemon.display.stream", None);
        let stream = unsafe {
            CGDisplayStream::with_dispatch_queue(
                display_id,
                mode.width as usize,
                mode.height as usize,
                0x4247_5241, // 'BGRA' packed little-endian ARGB8888
                None,
                &stream_queue,
                handler_ptr,
            )
        }
        .ok_or_else(|| anyhow!("CGDisplayStreamCreateWithDispatchQueue returned NULL"))?;

        let start_err = CGDisplayStream::start(Some(&stream));
        if start_err != CGError::Success {
            bail!("CGDisplayStreamStart failed: {start_err:?}");
        }
        println!("[display] CGDisplayStream started");

        self.width = mode.width;
        self.height = mode.height;
        self.fps = mode.fps;
        self.display_id = Some(display_id);
        self.virtual_display = Some(display_obj);
        self.descriptor_queue = Some(descriptor_queue);
        self.stream_queue = Some(stream_queue);
        self.stream = Some(stream);
        self.frame_block = Some(block);
        self.frame_state = Some(frame_state);

        Ok(())
    }

    fn run_capture_loop(
        &mut self,
        stop: &AtomicBool,
        force_refresh: &AtomicBool,
        on_frame: &mut dyn FnMut(CaptureFrame<'_>),
    ) -> Result<()> {
        let frame_state = self
            .frame_state
            .clone()
            .ok_or_else(|| anyhow!("run_capture_loop called before attach"))?;
        let fps = self.fps.max(1);
        // "timeout ≈ 2/fps" per the force_refresh/starvation-resend contract:
        // if no new frame (or force_refresh) shows up within that window,
        // resend the last captured (or black bootstrap) frame so the client
        // never starves.
        let wait_timeout = Duration::from_secs_f64(2.0 / fps as f64);
        let mut last_seq: u64 = 0;
        let mut scratch = vec![0u8; self.width as usize * self.height as usize * 4];

        println!(
            "[capture] entering CGDisplayStream capture loop ({}x{}@{fps})",
            self.width, self.height
        );

        while !stop.load(Ordering::Relaxed) {
            let slot_guard = frame_state.slot.lock().unwrap();
            let (slot_guard, _timeout_result) = frame_state
                .cvar
                .wait_timeout_while(slot_guard, wait_timeout, |slot| {
                    slot.seq == last_seq
                        && !force_refresh.load(Ordering::Relaxed)
                        && !stop.load(Ordering::Relaxed)
                })
                .unwrap();

            if stop.load(Ordering::Relaxed) {
                break;
            }

            last_seq = slot_guard.seq;
            let w = slot_guard.width;
            let h = slot_guard.height;
            if scratch.len() == slot_guard.data.len() {
                scratch.copy_from_slice(&slot_guard.data);
            }
            drop(slot_guard);
            force_refresh.store(false, Ordering::Relaxed);

            on_frame(CaptureFrame {
                width: w,
                height: h,
                data: &scratch,
            });
        }

        println!("[capture] CGDisplayStream capture loop stopped");
        Ok(())
    }

    fn detach(&mut self) {
        if let Some(stream) = self.stream.take() {
            let err = CGDisplayStream::stop(Some(&stream));
            if err != CGError::Success {
                eprintln!("[display] CGDisplayStreamStop failed: {err:?}");
            }
        }
        self.frame_block = None;
        self.stream_queue = None;
        self.frame_state = None;

        if let Some(display) = self.virtual_display.take() {
            // Dropping the last reference to the CGVirtualDisplay instance
            // is what unplugs the monitor.
            drop(display);
        }
        self.descriptor_queue = None;
        self.display_id = None;
        println!("[display] detached");
    }

    fn output_rect(&self) -> Option<(i32, i32, u32, u32)> {
        let id = self.display_id?;
        // 1x scale (hiDPI forced off in attach()) means points==pixels, so
        // no scaling math is needed here.
        let bounds = CGDisplayBounds(id);
        Some((
            bounds.origin.x as i32,
            bounds.origin.y as i32,
            bounds.size.width as u32,
            bounds.size.height as u32,
        ))
    }
}

impl Drop for MacDisplay {
    /// Defensive cleanup in case a caller drops the backend without calling
    /// `detach()` first (the normal per-session lifecycle in main.rs always
    /// calls `detach()` explicitly, but this guards against leaking the
    /// virtual display / stream if that ever changes).
    fn drop(&mut self) {
        if self.virtual_display.is_some() || self.stream.is_some() {
            self.detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual smoke test exercising the real CGVirtualDisplay/CGDisplayStream
    /// path end to end: attach, pump the capture loop for a few seconds
    /// while logging frame byte counts, then detach and attach again (the
    /// repeated-session lifecycle main.rs relies on). Ignored by default
    /// since it requires Screen Recording permission granted to the test
    /// binary and touches real system state (creates/destroys a display).
    ///
    /// Run manually with:
    ///   cargo test --release -- --ignored macos_display_smoke --nocapture
    #[test]
    #[ignore]
    fn macos_display_smoke() {
        let mode = DisplayMode {
            width: 1280,
            height: 800,
            fps: 30,
        };

        for round in 0..2 {
            println!("=== round {round} ===");
            let mut display = MacDisplay::new(mode.width, mode.height, mode.fps).unwrap();
            display.attach(mode).expect("attach failed");
            println!("output_rect = {:?}", display.output_rect());

            let stop = AtomicBool::new(false);
            let force_refresh = AtomicBool::new(false);

            std::thread::scope(|scope| {
                scope.spawn(|| {
                    std::thread::sleep(Duration::from_secs(3));
                    stop.store(true, Ordering::Relaxed);
                });

                let mut frame_count = 0usize;
                display
                    .run_capture_loop(&stop, &force_refresh, &mut |frame| {
                        frame_count += 1;
                        let sum: u64 = frame.data.iter().map(|&b| b as u64).sum();
                        println!(
                            "frame {frame_count}: {}x{} bytes={} byte_sum={sum}",
                            frame.width,
                            frame.height,
                            frame.data.len()
                        );
                    })
                    .unwrap();
            });

            display.detach();
        }
    }
}
