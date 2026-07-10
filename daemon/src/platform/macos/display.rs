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
    CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration,
    CGConfigureDisplayMirrorOfDisplay, CGConfigureDisplayOrigin, CGConfigureOption,
    CGDirectDisplayID, CGDisplayBounds, CGDisplayConfigRef, CGDisplayIsInMirrorSet,
    CGDisplayIsMain, CGDisplayMirrorsDisplay, CGDisplayStream, CGDisplayStreamFrameStatus,
    CGDisplayStreamUpdate, CGError, CGGetOnlineDisplayList, CGMainDisplayID,
    CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess,
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

/// `kCGNullDirectDisplay`: the CoreGraphics sentinel meaning "no display" —
/// e.g. "stop mirroring" when passed as the `master` argument to
/// `CGConfigureDisplayMirrorOfDisplay`. It's a C `#define 0`, not exposed as
/// a named binding by objc2-core-graphics, so it's spelled out here.
const K_CG_NULL_DIRECT_DISPLAY: CGDirectDisplayID = 0;

/// Prints the current placement/mirroring state of a display: its global
/// desktop bounds, whether it's the main display, whether it's part of a
/// mirror set, and (if so) which display it mirrors. Used both to diagnose
/// the OS's default behavior for a newly attached virtual display and to
/// verify the outcome of `force_extended_placement` below.
fn log_display_state(label: &str, id: CGDirectDisplayID) {
    let bounds = CGDisplayBounds(id);
    let in_mirror_set = CGDisplayIsInMirrorSet(id);
    let mirrors_display = CGDisplayMirrorsDisplay(id);
    let is_main = CGDisplayIsMain(id);
    println!(
        "[display]   {label}: id={id} bounds=(x={}, y={}, w={}, h={}) is_main={is_main} \
         in_mirror_set={in_mirror_set} mirrors_display={mirrors_display}",
        bounds.origin.x as i32,
        bounds.origin.y as i32,
        bounds.size.width as u32,
        bounds.size.height as u32,
    );
}

/// Forces the newly-attached virtual display into the product-required
/// default: an *extended* desktop (not mirrored), placed at a deterministic,
/// non-overlapping origin immediately to the right of the main display.
///
/// Background: macOS has no documented guarantee about how a freshly
/// attached display is initially arranged — it may come up mirrored onto
/// the main display (sharing/overlapping desktop coordinates), or extended
/// at some origin the Arrangement system picked on its own. Either way,
/// `output_rect()` (and the input-injection coordinate math downstream of
/// it) needs a known-good, non-overlapping global rect, so this pins the
/// arrangement explicitly instead of trusting the default.
///
/// Uses the public `CGBeginDisplayConfiguration` / `CGConfigureDisplay*` /
/// `CGCompleteDisplayConfiguration` transaction API with
/// `kCGConfigureForSession` (not `kCGConfigurePermanently`): the change is
/// process/session-scoped and nothing durable gets written to the user's
/// saved display preferences (the virtual display disappears at detach()
/// anyway, and CoreGraphics also unwinds session-scoped config when the
/// display goes offline). This only sets the *initial* state — it does not
/// fight a user who later switches to mirroring via System Settings.
///
/// Empirically observed on this machine: `CGCompleteDisplayConfiguration`
/// intermittently returns a non-Success `CGError` (seen: `CGError(1014)`,
/// not one of the documented `kCGError*` constants — likely a private
/// SkyLight/WindowServer-internal code surfacing through the public
/// display-configuration transaction when applied to a `CGVirtualDisplay`)
/// even with valid arguments and a display already confirmed online via
/// `CGGetOnlineDisplayList`, at roughly a 50% rate across repeated
/// attach/detach cycles in manual testing. When it happens, the display
/// stays registered/online for on the order of minutes afterward (far
/// longer than the sub-5s teardown seen on a clean detach) before the OS
/// reclaims it, even though the Rust-side `CGVirtualDisplay` object is
/// dropped immediately. The caller in `attach()` treats failures from this
/// function as non-fatal (logs a warning, keeps going) specifically
/// because of this: failing the whole attach() over a placement nicety
/// would abort capture AND leave the display stuck for minutes, which is
/// strictly worse than an imperfectly-placed-but-working capture session.
/// If this proves to be a real limitation of `CGConfigureDisplayMirrorOfDisplay`
/// / `CGConfigureDisplayOrigin` on virtual displays rather than a transient
/// WindowServer hiccup, alternatives worth trying: (a) split the unmirror
/// and origin changes into two separate Begin/Complete transactions instead
/// of one, in case combining them is what trips the internal validation;
/// (b) retry the transaction a couple of times with a short backoff before
/// giving up, since the same inputs succeeded on other attempts; (c) skip
/// `CGConfigureDisplayMirrorOfDisplay` entirely when `CGDisplayIsInMirrorSet`
/// is already false (this environment's failures did not correlate with
/// that flag, but it's an obvious first thing to try in an environment
/// where they do).
fn force_extended_placement(display_id: CGDirectDisplayID) -> Result<()> {
    // Ground truth for "is there another display to extend onto" is the
    // online display list, NOT CGMainDisplayID(). Attaching the virtual
    // display can itself become the new main display (observed on this
    // machine: CGMainDisplayID() returns the virtual display's own id
    // immediately after attach), which does not mean no other display
    // exists — it just means the OS handed the new display the menu
    // bar/main status. Trusting CGMainDisplayID() here previously caused
    // this function to skip the fixup even when a real second display
    // (id=1) was sitting right there in the online list.
    let mut ids = [0u32; 32];
    let mut count: u32 = 0;
    let list_err = unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if list_err != CGError::Success {
        bail!("CGGetOnlineDisplayList failed: {list_err:?}");
    }
    let online = ids[..count as usize].to_vec();
    println!(
        "[display] online displays at fixup time: {online:?} (CGMainDisplayID()={})",
        CGMainDisplayID()
    );

    println!("[display] arrangement before extend/placement fixup:");
    log_display_state("virtual", display_id);
    for &id in &online {
        if id != display_id {
            log_display_state("other", id);
        }
    }

    // Pick any other currently-online display to place/extend relative to.
    let Some(reference_id) = online.iter().copied().find(|&id| id != display_id) else {
        println!(
            "[display] no other online display besides the virtual one ({display_id}); \
             nothing to extend/place it relative to, leaving as-is"
        );
        return Ok(());
    };

    let reference_bounds = CGDisplayBounds(reference_id);
    let target_x = (reference_bounds.origin.x + reference_bounds.size.width) as i32;
    let target_y = reference_bounds.origin.y as i32;

    let mut config: CGDisplayConfigRef = std::ptr::null_mut();
    let begin_err = unsafe { CGBeginDisplayConfiguration(&mut config) };
    if begin_err != CGError::Success {
        bail!("CGBeginDisplayConfiguration failed: {begin_err:?}");
    }

    // Un-mirror (harmless/idempotent no-op if it wasn't mirrored) and place
    // it deterministically to the right of the reference display, in the
    // same transaction.
    let unmirror_err = unsafe {
        CGConfigureDisplayMirrorOfDisplay(config, display_id, K_CG_NULL_DIRECT_DISPLAY)
    };
    let origin_err = unsafe { CGConfigureDisplayOrigin(config, display_id, target_x, target_y) };

    if unmirror_err != CGError::Success || origin_err != CGError::Success {
        unsafe { CGCancelDisplayConfiguration(config) };
        bail!(
            "display configuration failed: CGConfigureDisplayMirrorOfDisplay={unmirror_err:?} \
             CGConfigureDisplayOrigin={origin_err:?}"
        );
    }

    let complete_err =
        unsafe { CGCompleteDisplayConfiguration(config, CGConfigureOption::ForSession) };
    if complete_err != CGError::Success {
        bail!("CGCompleteDisplayConfiguration failed: {complete_err:?}");
    }

    println!(
        "[display] applied extend+placement fixup: reference={reference_id} \
         target_origin=(x={target_x}, y={target_y})"
    );
    println!("[display] arrangement after extend/placement fixup:");
    log_display_state("virtual", display_id);
    log_display_state("other", reference_id);

    if CGDisplayIsInMirrorSet(display_id) {
        eprintln!(
            "[display] WARNING: display {display_id} still reports in_mirror_set=true after \
             CGConfigureDisplayMirrorOfDisplay(..., kCGNullDirectDisplay) — mirroring may not \
             be undoable this way for a CGVirtualDisplay, or something else re-mirrored it"
        );
    }
    let final_bounds = CGDisplayBounds(display_id);
    if final_bounds.origin.x as i32 != target_x || final_bounds.origin.y as i32 != target_y {
        eprintln!(
            "[display] WARNING: display {display_id} bounds after fixup are \
             (x={}, y={}) but the requested origin was (x={target_x}, y={target_y})",
            final_bounds.origin.x as i32, final_bounds.origin.y as i32
        );
    }

    Ok(())
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

        // Best-effort, not fatal: the private display-configuration
        // transaction this performs has been observed (empirically, on
        // this machine) to intermittently fail with a non-Success CGError
        // from CGCompleteDisplayConfiguration even with legitimate
        // arguments and a display already confirmed online — see the
        // detailed comment on `force_extended_placement` for the exact
        // error and reproduction rate observed. Treating that as fatal to
        // attach() would be strictly worse than the placement glitch it's
        // trying to fix: it would abort the whole capture session AND
        // leave the freshly-created virtual display occupying a
        // CGDirectDisplayID (observed to linger online for on the order of
        // minutes before the OS reclaims it) for no capture benefit at
        // all. So: try to force extended placement, but if it fails, warn
        // loudly and continue attaching/streaming with whatever placement
        // the OS already gave the display — `output_rect()` reads live
        // bounds, so callers still get a geometrically correct (if
        // possibly not ideally positioned) rect either way.
        if let Err(e) = force_extended_placement(display_id) {
            eprintln!(
                "[display] WARNING: extend/placement fixup failed, continuing with whatever \
                 arrangement the OS already gave display {display_id}: {e:#}"
            );
        }

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

        // Baseline, before any virtual display exists: what does this
        // session's display landscape look like already? Distinguishes "the
        // virtual display became main because it's the only display this
        // session can see" from "something in our fixup logic is wrong".
        {
            let mut ids = [0u32; 32];
            let mut count: u32 = 0;
            let err =
                unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
            println!(
                "[baseline] CGGetOnlineDisplayList err={err:?} count={count} ids={:?}",
                &ids[..count as usize]
            );
            println!("[baseline] CGMainDisplayID()={}", CGMainDisplayID());
        }

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
