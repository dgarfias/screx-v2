//! macOS display backend.
//!
//! Virtual monitor via the private `CGVirtualDisplay` API, frame capture via
//! ScreenCaptureKit. Ported from DeskPad
//! (https://github.com/Stengo/DeskPad), adapted for a headless daemon: no
//! CFRunLoop/AppKit main thread, hiDPI turned ON (2x scale, matching
//! DeskPad) so the client gets Retina-quality rendering, native `420v`
//! CVPixelBuffers handed to the encoder instead of drawing into a CALayer. `CGDisplayMode`
//! width/height (points) and pixel_width/pixel_height (framebuffer pixels)
//! can now legitimately differ — see `create_virtual_display` and
//! `force_display_mode` — so the input-injection layer's live-bounds-based
//! scaling (`MacInput::set_output_size`) is what keeps touches/clicks
//! landing correctly regardless of which mode ends up active.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::ProtocolObject;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_foundation::{
    CFDictionary, CFNumber, CFRetained, CFString, CFType, CGFloat, CGSize,
};
use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration,
    CGConfigureDisplayMirrorOfDisplay, CGConfigureDisplayOrigin, CGConfigureOption,
    CGDirectDisplayID, CGDisplayBounds, CGDisplayConfigRef, CGDisplayCopyAllDisplayModes,
    CGDisplayIsInMirrorSet, CGDisplayIsMain, CGDisplayMirrorsDisplay, CGDisplayMode,
    CGDisplaySetDisplayMode, CGError, CGGetOnlineDisplayList, CGMainDisplayID,
    CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferGetHeight,
    CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidth,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use super::audio::{nserror_string, wait_for_completion, SendRetained, SCK_CALLBACK_TIMEOUT};
use crate::capture::{CaptureFrame, DisplayBackend, DisplayMode};

/// Look up an Objective-C class by name, for the private CGVirtualDisplay*
/// API surface that has no static objc2 bindings anywhere.
fn get_class(name: &std::ffi::CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name)
        .ok_or_else(|| anyhow!("class {:?} not found (unsupported macOS version?)", name))
}

fn display_is_online(display_id: CGDirectDisplayID) -> bool {
    let mut ids = [0u32; 32];
    let mut count: u32 = 0;
    let err = unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    err == CGError::Success && ids[..count as usize].contains(&display_id)
}

fn wait_for_display_offline(display_id: CGDirectDisplayID, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !display_is_online(display_id) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !display_is_online(display_id)
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

/// Finds the `CGDisplayMode` registered for `display_id` for which `matches`
/// returns true, given `(width, height, pixel_width, pixel_height)` — points
/// and pixels respectively, which can legitimately differ now that hiDPI is
/// on (a HiDPI mode has `pixel_width/height == 2 * width/height`; a 1x mode
/// has them equal).
///
/// The returned `CGDisplayMode` is retained independently of the backing
/// `CGDisplayCopyAllDisplayModes` array (which this function's caller never
/// sees), so it's safe for the caller to use after this function returns.
///
/// Logs every mode actually registered for the display when no match is
/// found, for diagnosis — this "shouldn't" happen since we registered the
/// requested mode(s) ourselves via `CGVirtualDisplaySettings.setModes:`, but
/// if it does, the caller needs to know what WAS available instead.
fn find_mode_where(
    display_id: CGDirectDisplayID,
    label: &str,
    matches: impl Fn(usize, usize, usize, usize) -> bool,
) -> Option<CFRetained<CGDisplayMode>> {
    // Follows the CF "Copy" ownership rule: this array is ours to release,
    // handled automatically by `CFRetained`'s `Drop` impl. The individual
    // `CGDisplayModeRef` values inside it are NOT independently owned by us
    // (per CFArrayGetValueAtIndex semantics) — they stay valid only as long
    // as the array itself is alive, which is exactly the scope of this
    // function, so any match found is explicitly re-retained before
    // returning it to a caller that will use it after the array is gone.
    let modes = unsafe { CGDisplayCopyAllDisplayModes(display_id, None) }?;
    let count = modes.count();
    let mut available = Vec::with_capacity(count as usize);
    let mut found: Option<NonNull<CGDisplayMode>> = None;
    for i in 0..count {
        // SAFETY: `i` is in `0..modes.count()`, and the array is not
        // mutated while this reference is alive (it's a local, freshly
        // copied array we never hand a mutable/shared reference to
        // anything else).
        let ptr = unsafe { modes.value_at_index(i) };
        if ptr.is_null() {
            continue;
        }
        let mode_ptr = ptr as *const CGDisplayMode;
        // SAFETY: non-null CGDisplayModeRef from CFArrayGetValueAtIndex on
        // an array CGDisplayCopyAllDisplayModes populated with
        // CGDisplayModeRefs.
        let mode_ref: &CGDisplayMode = unsafe { &*mode_ptr };
        let w = CGDisplayMode::width(Some(mode_ref));
        let h = CGDisplayMode::height(Some(mode_ref));
        let pw = CGDisplayMode::pixel_width(Some(mode_ref));
        let ph = CGDisplayMode::pixel_height(Some(mode_ref));
        available.push((w, h, pw, ph));
        if found.is_none() && matches(w, h, pw, ph) {
            found = NonNull::new(mode_ptr as *mut CGDisplayMode);
        }
    }
    match found {
        Some(ptr) => Some(unsafe { CFRetained::retain(ptr) }),
        None => {
            println!(
                "[display] no mode matching \"{label}\" found among {count} modes registered \
                 for display {display_id}: {available:?} (each tuple is \
                 width,height,pixel_width,pixel_height in points/pixels)"
            );
            None
        }
    }
}

/// Forces the virtual display's live mode to match `mode`, working around
/// WindowServer silently substituting a persisted per-display-identity mode
/// preference for the mode actually requested via
/// `CGVirtualDisplaySettings.setModes:` — the same persistence mechanism
/// documented in `create_virtual_display`'s identity-strategy comment, just
/// manifesting as a wrong *mode* instead of a wedged identity. Empirically
/// observed on this machine: requesting 1280x800 for a virtual display, the
/// display came online at 1024x768 instead (a size that was never asked
/// for, but apparently sticks per display identity across attach/detach
/// cycles).
///
/// This is a best-effort fixup, not fatal to `attach()` if it fails — see
/// the caller. The downstream input-injection path (`MacInput::set_output_size`)
/// scales wire coordinates onto whatever the live rect actually turns out to
/// be, so a failure here degrades to "not quite the requested resolution"
/// rather than "touches land in the wrong place".
///
/// Scale-aware since hiDPI was turned on (see `create_virtual_display`):
/// `CGDisplayBounds` reports in POINTS, but the mode requested by the caller
/// (`DisplayMode`) is in the negotiated session PIXEL resolution. Two
/// outcomes now count as "the requested mode is active" — bounds equal to
/// `mode.width x mode.height` (the 1x mode: points==pixels) or equal to
/// `mode.width/2 x mode.height/2` (the HiDPI mode: pixels==2x points,
/// registered only when both dimensions are even — see
/// `create_virtual_display`). When neither is active, prefer forcing the
/// HiDPI mode when one is registered — `CGDisplayModeGetPixelWidth/Height`
/// vs `CGDisplayModeGetWidth/Height` is what tells the two apart in the
/// `CGDisplayCopyAllDisplayModes` list, since both modes may be present with
/// the same nominal request but different point/pixel ratios.
fn force_display_mode(display_id: CGDirectDisplayID, mode: DisplayMode) -> Result<()> {
    let full = (mode.width, mode.height);
    let both_even = mode.width % 2 == 0 && mode.height % 2 == 0;
    let half = (mode.width / 2, mode.height / 2);
    // The mode we WANT active by default: HiDPI when registered (both_even),
    // else the 1x mode — matches the modes-array ordering in
    // create_virtual_display (HiDPI first/primary when present).
    let preferred = if both_even { half } else { full };

    let bounds = CGDisplayBounds(display_id);
    let (actual_w, actual_h) = (bounds.size.width as u32, bounds.size.height as u32);

    if (actual_w, actual_h) == preferred {
        println!(
            "[display] display {display_id} already online at the preferred mode \
             {actual_w}x{actual_h} points, no override needed"
        );
        return Ok(());
    }

    // Note: landing on the OTHER acceptable outcome (e.g. the 1x fallback
    // when HiDPI is preferred) is deliberately NOT treated as "close enough"
    // here — that's exactly the persisted-mode-preference substitution this
    // function exists to override (see doc comment above), just now with
    // two candidate modes instead of one, and skipping the force would
    // silently give up on Retina by default whenever a stale preference
    // happens to match the fallback mode.
    let also_registered = if both_even {
        format!(
            " ({}x{} 1x, {}x{} HiDPI points also registered)",
            full.0, full.1, half.0, half.1
        )
    } else {
        String::new()
    };
    println!(
        "[display] display {display_id} came online at {actual_w}x{actual_h} points, not the \
         preferred {}x{} points{also_registered} — WindowServer likely substituted a persisted \
         mode preference for this display identity; attempting to force the preferred mode",
        preferred.0, preferred.1
    );

    // Prefer the HiDPI mode (point dims == half request, pixel dims == full
    // request) when the requested resolution supports one; fall back to the
    // 1x mode (point dims == pixel dims == full request) either way.
    let hidpi_match = both_even
        .then(|| {
            find_mode_where(
                display_id,
                "HiDPI (points=half, pixels=full)",
                |w, h, pw, ph| {
                    w == half.0 as usize
                        && h == half.1 as usize
                        && pw == full.0 as usize
                        && ph == full.1 as usize
                },
            )
        })
        .flatten();
    if hidpi_match.is_none() && (actual_w, actual_h) == full {
        println!(
            "[display] display {display_id} is already at the requested {actual_w}x{actual_h} \
             1x mode and no usable HiDPI mode is registered; no override needed"
        );
        return Ok(());
    }
    let display_mode = hidpi_match.or_else(|| {
        find_mode_where(display_id, "1x (points=pixels=full)", |w, h, _, _| {
            w == full.0 as usize && h == full.1 as usize
        })
    });

    let Some(display_mode) = display_mode else {
        bail!(
            "no registered CGDisplayMode matches the requested {}x{} (1x) or {}x{} (HiDPI \
             points) for display {display_id} (see logged available modes above) — cannot \
             force it",
            full.0,
            full.1,
            half.0,
            half.1
        );
    };

    let set_err = unsafe { CGDisplaySetDisplayMode(display_id, Some(&display_mode), None) };
    if set_err != CGError::Success {
        bail!("CGDisplaySetDisplayMode failed: {set_err:?}");
    }

    let new_bounds = CGDisplayBounds(display_id);
    let (new_w, new_h) = (new_bounds.size.width as u32, new_bounds.size.height as u32);
    let ok = (new_w, new_h) == full || (both_even && (new_w, new_h) == half);
    if !ok {
        bail!(
            "CGDisplaySetDisplayMode returned Success but display {display_id} bounds are \
             still {new_w}x{new_h}, not the requested {}x{} (1x) or {}x{} (HiDPI points)",
            full.0,
            full.1,
            half.0,
            half.1
        );
    }

    println!("[display] forced display {display_id} mode to {new_w}x{new_h} points");
    Ok(())
}

/// Ensures the newly-attached virtual display is extended and non-overlapping.
/// A valid arrangement chosen by WindowServer is left untouched; otherwise,
/// the display is placed immediately to the right of another online display.
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
    let list_err =
        unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
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

    let virtual_bounds = CGDisplayBounds(display_id);
    let overlaps_other = online.iter().copied().any(|id| {
        if id == display_id {
            return false;
        }
        let other = CGDisplayBounds(id);
        virtual_bounds.origin.x < other.origin.x + other.size.width
            && other.origin.x < virtual_bounds.origin.x + virtual_bounds.size.width
            && virtual_bounds.origin.y < other.origin.y + other.size.height
            && other.origin.y < virtual_bounds.origin.y + virtual_bounds.size.height
    });
    if !CGDisplayIsInMirrorSet(display_id) && !overlaps_other {
        println!(
            "[display] virtual display {display_id} is already extended and non-overlapping; \
             leaving WindowServer's placement unchanged"
        );
        return Ok(());
    }

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
    let unmirror_err =
        unsafe { CGConfigureDisplayMirrorOfDisplay(config, display_id, K_CG_NULL_DIRECT_DISPLAY) };
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

/// Allocates and initializes one `CGVirtualDisplayMode` (`width`/`height`
/// are POINT dimensions — see the hiDPI comment in `create_virtual_display`).
fn build_virtual_display_mode(
    mode_cls: &AnyClass,
    width: usize,
    height: usize,
    refresh_rate: CGFloat,
) -> Retained<AnyObject> {
    let mode_alloc: Allocated<AnyObject> = unsafe { msg_send![mode_cls, alloc] };
    unsafe {
        msg_send![
            mode_alloc,
            initWithWidth: width,
            height: height,
            refreshRate: refresh_rate,
        ]
    }
}

/// Create a `CGVirtualDisplay` with the given product ID, apply a
/// HiDPI-plus-1x-fallback settings object (see `build_virtual_display_mode`
/// and the hiDPI comment below), and block until the display actually
/// appears in `CGGetOnlineDisplayList`.
///
/// The online wait is 30s, not the original 5s: healthy attaches on this
/// machine were observed to take anywhere from 0ms (identity reuse) to
/// ~15s (new identity, or shortly after a WindowServer restart / a
/// previous virtual-display detach), so 5s produced spurious attach
/// failures for displays that were about to appear. A slow attach still
/// beats a failed session.
///
/// On failure the (never-online) display object is dropped, which unplugs
/// whatever half-registered state WindowServer holds for it.
fn create_virtual_display(
    descriptor_queue: &DispatchQueue,
    mode: DisplayMode,
    product_id: u32,
) -> Result<(Retained<AnyObject>, CGDirectDisplayID)> {
    let descriptor_cls = get_class(c"CGVirtualDisplayDescriptor")?;
    let descriptor_alloc: Allocated<AnyObject> = unsafe { msg_send![descriptor_cls, alloc] };
    let descriptor: Retained<AnyObject> = unsafe { msg_send![descriptor_alloc, init] };

    unsafe {
        let _: () = msg_send![&*descriptor, setQueue: descriptor_queue];
        let name = NSString::from_str("Screx Virtual");
        let _: () = msg_send![&*descriptor, setName: &*name];
        let _: () = msg_send![&*descriptor, setMaxPixelsWide: mode.width as usize];
        let _: () = msg_send![&*descriptor, setMaxPixelsHigh: mode.height as usize];
        let _: () = msg_send![
            &*descriptor,
            setSizeInMillimeters: CGSize { width: 1600.0, height: 1000.0 }
        ];
        println!("[display] virtual display identity: vendor=0x3456 product={product_id:#06x}");
        let _: () = msg_send![&*descriptor, setProductID: product_id];
        let _: () = msg_send![&*descriptor, setVendorID: 0x3456u32];
        let _: () = msg_send![&*descriptor, setSerialNum: 0x0001u32];
    }

    let display_cls = get_class(c"CGVirtualDisplay")?;
    let display_alloc: Allocated<AnyObject> = unsafe { msg_send![display_cls, alloc] };
    let display_obj: Retained<AnyObject> =
        unsafe { msg_send![display_alloc, initWithDescriptor: &*descriptor] };

    let display_id: CGDirectDisplayID = unsafe { msg_send![&*display_obj, displayID] };
    println!("[display] CGVirtualDisplay created, displayID={display_id}");

    // hiDPI is turned ON (1), matching DeskPad: gives the client
    // Retina-quality rendering (2x pixel backing) instead of soft 1x text.
    // `CGVirtualDisplayMode(width:height:)` args are POINT dimensions (per
    // DeskPad's own mode list, e.g. registering 1280x800 — the real
    // MacBook Air 13" Retina point size, backed by 2560x1600 pixels); with
    // hiDPI on, WindowServer reports that mode's `CGDisplayBounds` at the
    // registered point size while giving it a 2x pixel framebuffer.
    let settings_cls = get_class(c"CGVirtualDisplaySettings")?;
    let settings_alloc: Allocated<AnyObject> = unsafe { msg_send![settings_cls, alloc] };
    let settings: Retained<AnyObject> = unsafe { msg_send![settings_alloc, init] };
    unsafe {
        let _: () = msg_send![&*settings, setHiDPI: 1u32];
    }

    let mode_cls = get_class(c"CGVirtualDisplayMode")?;
    let both_even = mode.width % 2 == 0 && mode.height % 2 == 0;
    let onex_mode = build_virtual_display_mode(
        mode_cls,
        mode.width as usize,
        mode.height as usize,
        mode.fps as CGFloat,
    );
    let modes_array = if both_even {
        // HiDPI mode goes FIRST — CGVirtualDisplaySettings.modes' first
        // entry is the default/primary mode a freshly attached display
        // comes online at. The full-resolution 1x mode is registered too,
        // as a fallback the user can still pick in System Settings ->
        // Displays if they want non-Retina.
        let (hw, hh) = (mode.width / 2, mode.height / 2);
        let hidpi_mode =
            build_virtual_display_mode(mode_cls, hw as usize, hh as usize, mode.fps as CGFloat);
        println!(
            "[display] registering HiDPI mode {hw}x{hh} points (-> {}x{} pixels, 2x) as the \
             default, plus a {}x{} 1x fallback mode",
            mode.width, mode.height, mode.width, mode.height
        );
        NSArray::<AnyObject>::from_retained_slice(&[hidpi_mode, onex_mode])
    } else {
        println!(
            "[display] requested mode {}x{} has an odd dimension, so no clean HiDPI point-mode \
             (would need a fractional {}x{} half-size) can be registered; using the 1x mode only",
            mode.width,
            mode.height,
            mode.width as f64 / 2.0,
            mode.height as f64 / 2.0,
        );
        NSArray::<AnyObject>::from_retained_slice(&[onex_mode])
    };
    unsafe {
        let _: () = msg_send![&*settings, setModes: &*modes_array];
    }

    let applied: bool = unsafe { msg_send![&*display_obj, applySettings: &*settings] };
    if !applied {
        bail!("CGVirtualDisplay applySettings: failed (returned NO)");
    }

    // Defensive poll: the trait contract requires attach() to block
    // until the OS actually sees the new monitor.
    let poll_start = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut online = false;
    while Instant::now() < deadline {
        if display_is_online(display_id) {
            online = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !online {
        bail!(
            "virtual display {display_id} (product id {product_id:#06x}) did not appear in \
             CGGetOnlineDisplayList within 30s — WindowServer's persisted display state may \
             have wedged this display identity (see identity-strategy comment in attach())"
        );
    }
    println!(
        "[display] virtual display {display_id} is online (took {:?})",
        poll_start.elapsed()
    );

    Ok((display_obj, display_id))
}

/// Latest retained display surface written by the ScreenCaptureKit callback
/// and consumed by `run_capture_loop` on the capture thread.
struct FrameSlot {
    pixel_buffer: Option<SendPixelBuffer>,
    width: u32,
    height: u32,
    seq: u64,
}

struct SendPixelBuffer(CFRetained<CVPixelBuffer>);

// CoreVideo objects are retainable across threads. This wrapper only transfers
// ownership between the serial display callback and capture thread.
unsafe impl Send for SendPixelBuffer {}
unsafe impl Sync for SendPixelBuffer {}

struct FrameState {
    slot: Mutex<FrameSlot>,
    cvar: Condvar,
    terminal_error: Mutex<Option<String>>,
}

fn black_nv12(width: u32, height: u32) -> Vec<u8> {
    let y_len = width as usize * height as usize;
    let mut data = vec![16u8; y_len + y_len / 2];
    data[y_len..].fill(128);
    data
}

impl FrameState {
    fn new(width: u32, height: u32) -> Self {
        Self {
            slot: Mutex::new(FrameSlot {
                pixel_buffer: None,
                width,
                height,
                seq: 0,
            }),
            cvar: Condvar::new(),
            terminal_error: Mutex::new(None),
        }
    }
}

fn sample_frame_is_complete(sample_buffer: &CMSampleBuffer) -> bool {
    let Some(attachments) = (unsafe { sample_buffer.sample_attachments_array(false) }) else {
        return false;
    };
    if attachments.count() == 0 {
        return false;
    }
    let dictionary = unsafe { attachments.value_at_index(0).cast::<CFType>().as_ref() }
        .and_then(|value| value.downcast_ref::<CFDictionary>());
    let Some(dictionary) = dictionary else {
        return false;
    };
    let status_key = unsafe { SCStreamFrameInfoStatus };
    let status_key: &CFString = unsafe { &*((status_key as *const NSString).cast()) };
    let status = unsafe {
        dictionary
            .value((status_key as *const CFString).cast())
            .cast::<CFType>()
            .as_ref()
    }
    .and_then(|value| value.downcast_ref::<CFNumber>())
    .and_then(CFNumber::as_isize);
    status == Some(SCFrameStatus::Complete.0)
}

pub struct DisplayOutputIvars {
    state: Arc<FrameState>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = DisplayOutputIvars]
    struct DisplayStreamOutput;

    unsafe impl NSObjectProtocol for DisplayStreamOutput {}

    unsafe impl SCStreamOutput for DisplayStreamOutput {
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            if r#type != SCStreamOutputType::Screen {
                return;
            }
            let state = Arc::clone(&self.ivars().state);
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if !unsafe { sample_buffer.is_valid() }
                    || !unsafe { sample_buffer.data_is_ready() }
                    || !sample_frame_is_complete(sample_buffer)
                {
                    return;
                }
                let Some(pixel_buffer) = (unsafe { sample_buffer.image_buffer() }) else {
                    return;
                };
                if CVPixelBufferGetPixelFormatType(&pixel_buffer)
                    != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                    || CVPixelBufferGetPlaneCount(&pixel_buffer) < 2
                {
                    return;
                }

                let width = CVPixelBufferGetWidth(&pixel_buffer);
                let height = CVPixelBufferGetHeight(&pixel_buffer);
                if CVPixelBufferGetIOSurface(Some(&pixel_buffer)).is_none() {
                    return;
                }
                let mut slot = state.slot.lock().unwrap();
                if slot.width as usize == width && slot.height as usize == height {
                    slot.pixel_buffer = Some(SendPixelBuffer(pixel_buffer));
                    slot.seq = slot.seq.wrapping_add(1);
                    drop(slot);
                    state.cvar.notify_all();
                }
            }));
        }
    }

    unsafe impl SCStreamDelegate for DisplayStreamOutput {
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, error: &NSError) {
            let state = Arc::clone(&self.ivars().state);
            let _ = catch_unwind(AssertUnwindSafe(|| {
                *state.terminal_error.lock().unwrap() = Some(nserror_string(error));
                state.cvar.notify_all();
            }));
        }
    }
);

impl DisplayStreamOutput {
    fn new(state: Arc<FrameState>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DisplayOutputIvars { state });
        unsafe { msg_send![super(this), init] }
    }
}

fn fetch_sck_display(display_id: CGDirectDisplayID) -> Result<Retained<SCDisplay>> {
    let pair: Arc<(
        Mutex<Option<Result<SendRetained<SCDisplay>, String>>>,
        Condvar,
    )> = Arc::new((Mutex::new(None), Condvar::new()));
    let cb_pair = Arc::clone(&pair);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = (|| {
                if content.is_null() {
                    return Err(if error.is_null() {
                        "no shareable content and no error".to_string()
                    } else {
                        nserror_string(unsafe { &*error })
                    });
                }
                let content = unsafe { Retained::retain(content) }
                    .ok_or_else(|| "SCShareableContent retain failed".to_string())?;
                let displays = unsafe { content.displays() };
                let available: Vec<_> = displays
                    .to_vec()
                    .into_iter()
                    .map(|display| (unsafe { display.displayID() }, display))
                    .collect();
                available
                    .into_iter()
                    .find_map(|(id, display)| (id == display_id).then_some(SendRetained(display)))
                    .ok_or_else(|| format!("display {display_id} is not yet shareable"))
            })();
            let (lock, cvar) = &*cb_pair;
            *lock.lock().unwrap() = Some(result);
            cvar.notify_all();
        },
    );
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };

    let (lock, cvar) = &*pair;
    let guard = lock.lock().unwrap();
    let (mut guard, timeout) = cvar
        .wait_timeout_while(guard, SCK_CALLBACK_TIMEOUT, |result| result.is_none())
        .unwrap();
    if timeout.timed_out() && guard.is_none() {
        bail!(
            "timed out waiting for display {display_id} in SCShareableContent after {SCK_CALLBACK_TIMEOUT:?}"
        );
    }
    guard
        .take()
        .unwrap()
        .map(|display| display.0)
        .map_err(|error| anyhow!("SCShareableContent fetch failed: {error}"))
}

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
    stream: Option<Retained<SCStream>>,
    stream_output: Option<Retained<DisplayStreamOutput>>,
    frame_state: Option<Arc<FrameState>>,
}

// SAFETY: MacDisplay is only ever driven from a single OS thread (the
// capture thread: attach/run_capture_loop/detach all run there, in that
// order, per session). The SCStream frame callback runs on its own
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
            stream_output: None,
            frame_state: None,
        })
    }
}

impl DisplayBackend for MacDisplay {
    fn attach(&mut self, mode: DisplayMode) -> Result<()> {
        // TCC preflight: fail with an actionable message instead of letting
        // this surface as a cryptic downstream ScreenCaptureKit error.
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

        // A serial dispatch queue backs the descriptor (used internally by
        // CGVirtualDisplay for mode-change/termination callbacks). Unlike DeskPad (a GUI
        // app that hands these DispatchQueue.main, serviced by its already-
        // running CFRunLoop/AppKit main loop), this is a headless daemon
        // with no main-thread run loop — GCD services a custom serial queue
        // from its own worker-thread pool regardless, so this works without
        // one.
        let descriptor_queue = DispatchQueue::new("com.screx.daemon.display.descriptor", None);

        // Display identity strategy, driven by two empirically-observed
        // WindowServer behaviors (this machine, 2026-07-13):
        //
        //  1. WindowServer persists per-display-identity state on disk
        //     (/Library/Preferences/com.apple.windowserver.displays.plist
        //     and the ByHost variant), and an unclean virtual-display
        //     teardown can wedge that record such that a display with the
        //     SAME product ID is silently never brought online again —
        //     applySettings: returns YES, a display ID is assigned, but it
        //     never appears in CGGetOnlineDisplayList, surviving reboots.
        //     (Observed with the original fixed placeholder 0x1234: it never
        //     came online regardless of vendor/serial variation, while any
        //     other product ID worked with identical code.)
        //
        //  2. A previously-seen identity comes back online near-instantly,
        //     while a BRAND-NEW identity can take 0-15s normally and can
        //     stall indefinitely once WindowServer has churned through many
        //     virtual-display create/destroy cycles in a boot.
        //
        // So: try a STABLE product ID first (fast, predictable reuse), and
        // if that display never comes online, fall back once to a fresh
        // randomized ID (escapes a wedged persisted record for the stable
        // ID at the cost of the slower new-identity registration).
        // SCREX_VDISP_PRODUCT_ID overrides the stable ID (hex or decimal)
        // if a machine's persisted state ever needs manual sidestepping.
        // The default value is arbitrary (any value except a wedged one
        // works; the specific choice is already cleanly registered on the
        // original dev machine).
        let stable_product_id: u32 = std::env::var("SCREX_VDISP_PRODUCT_ID")
            .ok()
            .and_then(|s| {
                let s = s.trim();
                s.strip_prefix("0x")
                    .map(|h| u32::from_str_radix(h, 16).ok())
                    .unwrap_or_else(|| s.parse().ok())
            })
            .unwrap_or(0xA5A5);

        let (display_obj, display_id) =
            match create_virtual_display(&descriptor_queue, mode, stable_product_id) {
                Ok(v) => v,
                Err(e) => {
                    let fresh_product_id: u32 = 0x0100
                        + (std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.subsec_nanos())
                            .unwrap_or(0)
                            ^ std::process::id())
                            % 0xFE00;
                    eprintln!(
                        "[display] attach with stable product id {stable_product_id:#06x} failed \
                         ({e:#}); retrying once with fresh product id {fresh_product_id:#06x}"
                    );
                    create_virtual_display(&descriptor_queue, mode, fresh_product_id)?
                }
            };

        // Best-effort, not fatal: see `force_display_mode`'s doc comment for
        // the exact failure mode this works around (WindowServer silently
        // substituting a persisted mode preference for the requested one).
        // Run this BEFORE the placement fixup below — a mode change can
        // itself perturb placement, so fix size first, then position.
        if let Err(e) = force_display_mode(display_id, mode) {
            let bounds = CGDisplayBounds(display_id);
            eprintln!(
                "[display] WARNING: failed to force display {display_id} to the requested mode \
                 {}x{}@{} ({e:#}); continuing with whatever mode WindowServer actually gave it \
                 (currently {}x{}) — MacInput::set_output_size compensates for this at the \
                 input-injection layer, so touches/clicks still land correctly, just at a \
                 possibly-different resolution than negotiated",
                mode.width,
                mode.height,
                mode.fps,
                bounds.size.width as u32,
                bounds.size.height as u32,
            );
        }

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
        // Single retry with a short backoff: the observed CGError(1014) from
        // CGCompleteDisplayConfiguration is transient (~50% of first
        // attempts in stress testing, same inputs succeed on other tries),
        // so one more attempt recovers most sessions. This matters for
        // input, not just aesthetics: a failed fixup can leave the virtual
        // display mirrored/overlapping at an unplanned origin, and
        // `output_rect()` -> `set_target_rect` downstream would then aim
        // every injected touch/click at that unplanned region.
        let placement = force_extended_placement(display_id).or_else(|first_err| {
            eprintln!(
                "[display] extend/placement fixup failed ({first_err:#}), retrying once in 200ms"
            );
            std::thread::sleep(Duration::from_millis(200));
            force_extended_placement(display_id)
        });
        if let Err(e) = placement {
            let bounds = CGDisplayBounds(display_id);
            eprintln!(
                "[display] WARNING: extend/placement fixup failed twice, continuing with \
                 whatever arrangement the OS already gave display {display_id}: {e:#}"
            );
            eprintln!(
                "[display] WARNING: display {display_id} final bounds are (x={}, y={}, w={}, h={}) \
                 in_mirror_set={} — injected input will target this region; if it overlaps \
                 another display, touches will land there",
                bounds.origin.x as i32,
                bounds.origin.y as i32,
                bounds.size.width as u32,
                bounds.size.height as u32,
                CGDisplayIsInMirrorSet(display_id),
            );
        }

        // --- Frame capture via ScreenCaptureKit ---
        let frame_state = Arc::new(FrameState::new(mode.width, mode.height));
        let lookup_deadline = Instant::now() + Duration::from_secs(10);
        let sck_display = loop {
            match fetch_sck_display(display_id) {
                Ok(display) => break display,
                Err(error) if Instant::now() < lookup_deadline => {
                    crate::vlog!(
                        "[display] waiting for ScreenCaptureKit to expose display {display_id}: {error:#}"
                    );
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(error),
            }
        };
        let excluded_windows = NSArray::<SCWindow>::from_retained_slice(&[]);
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &sck_display,
                &excluded_windows,
            )
        };
        let config = unsafe { SCStreamConfiguration::new() };
        let stream = unsafe {
            config.setWidth(mode.width as usize);
            config.setHeight(mode.height as usize);
            config.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            config.setMinimumFrameInterval(CMTime::new(1, mode.fps.max(1) as i32));
            config.setQueueDepth(8);
            config.setCapturesAudio(false);
            config.setShowsCursor(true);

            let output = DisplayStreamOutput::new(Arc::clone(&frame_state));
            let delegate: &ProtocolObject<dyn SCStreamDelegate> =
                ProtocolObject::from_ref(&*output);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(delegate),
            );
            (stream, output)
        };
        let (stream, stream_output) = stream;
        let stream_queue = DispatchQueue::new("com.screx.daemon.display.sck-sample", None);
        let output: &ProtocolObject<dyn SCStreamOutput> = ProtocolObject::from_ref(&*stream_output);
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    output,
                    SCStreamOutputType::Screen,
                    Some(&stream_queue),
                )
                .map_err(|error| {
                    anyhow!(
                        "addStreamOutput (screen) failed: {}",
                        nserror_string(&error)
                    )
                })?;
        }
        let start_result = wait_for_completion(|block| unsafe {
            stream.startCaptureWithCompletionHandler(Some(block));
        });
        if let Err(error) = start_result {
            let _ =
                unsafe { stream.removeStreamOutput_type_error(output, SCStreamOutputType::Screen) };
            stream_queue.exec_sync(|| {});
            return Err(error).context("SCStream startCapture (screen) failed");
        }
        println!(
            "[display] ScreenCaptureKit stream started for display {display_id} (native 420v)"
        );

        self.width = mode.width;
        self.height = mode.height;
        self.fps = mode.fps;
        self.display_id = Some(display_id);
        self.virtual_display = Some(display_obj);
        self.descriptor_queue = Some(descriptor_queue);
        self.stream_queue = Some(stream_queue);
        self.stream = Some(stream);
        self.stream_output = Some(stream_output);
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
        // The timeout keeps stop/refresh response bounded. Plain timeouts do
        // not submit duplicate frames: heartbeats keep idle sessions alive.
        let wait_timeout = Duration::from_secs_f64(1.0 / fps as f64);
        let mut last_seq: u64 = 0;
        let bootstrap = black_nv12(self.width, self.height);
        let mut last_pixel_buffer: Option<SendPixelBuffer> = None;

        println!(
            "[capture] entering ScreenCaptureKit capture loop ({}x{}@{fps})",
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
            if let Some(error) = frame_state.terminal_error.lock().unwrap().take() {
                bail!("ScreenCaptureKit stream stopped unexpectedly: {error}");
            }

            let has_new_frame = slot_guard.seq != last_seq;
            let refresh_requested = force_refresh.swap(false, Ordering::Relaxed);
            last_seq = slot_guard.seq;
            let w = slot_guard.width;
            let h = slot_guard.height;
            let mut slot_guard = slot_guard;
            if has_new_frame {
                if let Some(pixel_buffer) = slot_guard.pixel_buffer.take() {
                    last_pixel_buffer = Some(pixel_buffer);
                }
            }
            drop(slot_guard);

            if !has_new_frame && !refresh_requested {
                continue;
            }

            let surface = last_pixel_buffer
                .as_ref()
                .and_then(|pixel_buffer| CVPixelBufferGetIOSurface(Some(&pixel_buffer.0)));
            on_frame(CaptureFrame {
                width: w,
                height: h,
                format: crate::capture::CapturePixelFormat::Nv12,
                data: if last_pixel_buffer.is_some() {
                    &[]
                } else {
                    &bootstrap
                },
                io_surface: surface.as_deref(),
                cv_pixel_buffer: last_pixel_buffer
                    .as_ref()
                    .map(|pixel_buffer| &*pixel_buffer.0),
            });
        }

        println!("[capture] ScreenCaptureKit capture loop stopped");
        Ok(())
    }

    fn detach(&mut self) {
        if let Some(stream) = self.stream.take() {
            if let Err(error) = wait_for_completion(|block| unsafe {
                stream.stopCaptureWithCompletionHandler(Some(block));
            }) {
                eprintln!("[display] SCStream stopCapture (screen) error: {error:#}");
            }
            if let Some(output) = &self.stream_output {
                let output: &ProtocolObject<dyn SCStreamOutput> =
                    ProtocolObject::from_ref(&**output);
                if let Err(error) = unsafe {
                    stream.removeStreamOutput_type_error(output, SCStreamOutputType::Screen)
                } {
                    eprintln!(
                        "[display] removeStreamOutput (screen) failed: {}",
                        nserror_string(&error)
                    );
                }
            }
            if let Some(queue) = &self.stream_queue {
                queue.exec_sync(|| {});
            }
        }
        self.stream_output = None;
        self.stream_queue = None;
        self.frame_state = None;

        let display_id = self.display_id.take();
        if let Some(display) = self.virtual_display.take() {
            // Dropping the last reference to the CGVirtualDisplay instance
            // is what unplugs the monitor.
            drop(display);
        }
        if let Some(queue) = &self.descriptor_queue {
            queue.exec_sync(|| {});
        }
        if let Some(display_id) = display_id {
            if wait_for_display_offline(display_id, Duration::from_secs(30)) {
                println!("[display] virtual display {display_id} is offline");
            } else {
                eprintln!(
                    "[display] WARNING: virtual display {display_id} remained online for 30s after teardown"
                );
            }
        }
        self.descriptor_queue = None;
        println!("[display] detached");
    }

    fn output_rect(&self) -> Option<(i32, i32, u32, u32)> {
        let id = self.display_id?;
        // Live CGDisplayBounds, in POINTS. With hiDPI on (see
        // create_virtual_display), this can now be half the negotiated
        // pixel resolution (HiDPI mode active) or equal to it (1x mode
        // active) — either way this is the correct value for input-
        // injection callers: MacInput::set_output_size scales wire pixel
        // coordinates onto whatever this actually is.
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

    /// Manual smoke test exercising the real CGVirtualDisplay/ScreenCaptureKit
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
                        println!(
                            "frame {frame_count}: {}x{} bytes={} iosurface={} cv_pixel_buffer={}",
                            frame.width,
                            frame.height,
                            frame.data.len(),
                            frame.io_surface.is_some(),
                            frame.cv_pixel_buffer.is_some()
                        );
                    })
                    .unwrap();
            });

            display.detach();

            // Empirical WindowServer behavior on this machine (2026-07-13):
            // a re-attach shortly after a detach takes much longer to come
            // online (~15-20s, vs 0-8s cold) — attach()'s 30s online wait
            // absorbs that, but give WindowServer a moment of quiet between
            // rounds so round 1 measures re-attach, not teardown contention.
            if round == 0 {
                println!("[smoke] waiting 10s for WindowServer to settle before re-attach");
                std::thread::sleep(Duration::from_secs(10));
            }
        }
    }
}
