//! macOS input backend — CGEventPost-based injection.
//!
//! Mouse and touch are injected as absolute-position `CGEvent`s (there is no
//! native macOS touch-injection API; touch is translated to pointer/scroll
//! events the same way Duet Display/Jump Desktop/Screens/Splashtop do).
//! Keyboard uses `CGEventCreateKeyboardEvent` + `CGEventKeyboardSetUnicodeString`
//! for text and a `kVK_*` virtual-keycode table for special/raw-HID keys.
//!
//! `CGEventPost` requires the Accessibility (TCC) permission; see
//! `MacInput::new` for the one-time warning when it's missing. Posting
//! without the permission silently no-ops — that's expected OS behavior, not
//! a bug here.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton, EventField,
    KeyCode, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use crate::input::{
    GamepadState, InputBackend, MouseEvent, TouchContact, SPECIAL_BACKSPACE, TOUCH_DOWN,
    TOUCH_MOVE, TOUCH_UP,
};

// ---------------------------------------------------------------------------
// FFI not covered by the `core-graphics` crate.
// ---------------------------------------------------------------------------

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Mouse buttons we track press state for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeldButton {
    Left,
    Right,
    Other,
}

/// Shared, lockable pointer/target-rect state. Mouse and touch both funnel
/// through this so touch-as-pointer translation shares tracked position with
/// real mouse events.
struct PointerState {
    /// (left, top, width, height) of the captured output on the desktop.
    rect: (i32, i32, u32, u32),
    /// Last-known absolute desktop position of the cursor.
    pos: (i32, i32),
    /// Currently-held mouse button, if any (for move-vs-drag event typing).
    held: Option<HeldButton>,
}

impl PointerState {
    fn clamp_to_rect(&self, x: i32, y: i32) -> (i32, i32) {
        let (left, top, width, height) = self.rect;
        let max_x = left + width.saturating_sub(1) as i32;
        let max_y = top + height.saturating_sub(1) as i32;
        (
            x.clamp(left, max_x.max(left)),
            y.clamp(top, max_y.max(top)),
        )
    }
}

/// Matches the Windows backend's contact-count limit.
const MAX_TOUCH_SLOTS: usize = 10;

/// Grace window used to disambiguate a single-finger tap/click from the
/// first frame of a two-finger scroll gesture — see `TouchState`/`touch()`.
const TAP_GRACE: Duration = Duration::from_millis(30);

/// Coarse gesture state derived from how many contacts are simultaneously
/// active, updated once per `touch()` call (which always carries the full
/// currently-active contact set, per `parse_touch_packet`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureMode {
    /// Nothing down.
    Idle,
    /// Exactly one contact went down; holding off on the synthetic
    /// LeftMouseDown for `TAP_GRACE` in case a second finger joins (which
    /// would mean this is actually the start of a two-finger scroll, not a
    /// click/drag). `generation` lets the deferred flush thread tell whether
    /// its pending tap is still the current one.
    Pending { generation: u64 },
    /// Single-finger click/drag committed; a real LeftMouseDown has been
    /// posted and is being held.
    Dragging,
    /// Two (or more) fingers down; translating movement to scroll-wheel
    /// events instead of a click.
    Scrolling,
}

struct TouchState {
    /// Last-known absolute desktop position of each slot, `None` when up.
    slots: [Option<(i32, i32)>; MAX_TOUCH_SLOTS],
    mode: GestureMode,
    /// Position to post the buffered click at (set when entering `Pending`).
    anchor: (i32, i32),
    /// Last centroid of the active contacts while `Scrolling`, for computing
    /// this call's incremental scroll delta.
    scroll_last: (i32, i32),
    /// Bumped on every transition out of `Pending`; a stale flush thread
    /// checks this against the generation it was armed with to detect that
    /// its tap was already resolved (cancelled into a scroll, or flushed
    /// early by a quick lift-off) and should no-op.
    generation: u64,
}

impl TouchState {
    fn new() -> Self {
        Self {
            slots: [None; MAX_TOUCH_SLOTS],
            mode: GestureMode::Idle,
            anchor: (0, 0),
            scroll_last: (0, 0),
            generation: 0,
        }
    }

    /// The position driving the current single-finger gesture: slot 0 if
    /// it's down, else whichever slot is (in practice slot 0 is always the
    /// first finger down, but this avoids depending on that).
    fn primary_pos(&self) -> Option<(i32, i32)> {
        self.slots[0].or_else(|| self.slots.iter().flatten().next().copied())
    }

    fn active_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Centroid of (at most) the first two active contacts, in slot-index
    /// order. 3+ simultaneous contacts are a spec'd no-op case for gestures
    /// generally — extras beyond the first two are ignored here rather than
    /// diluting the scroll centroid (and, being ignored, never error).
    fn centroid(&self) -> (i32, i32) {
        let active: Vec<(i32, i32)> = self.slots.iter().flatten().copied().take(2).collect();
        if active.is_empty() {
            return (0, 0);
        }
        let (sx, sy) = active
            .iter()
            .fold((0i64, 0i64), |(sx, sy), (x, y)| {
                (sx + *x as i64, sy + *y as i64)
            });
        (
            (sx / active.len() as i64) as i32,
            (sy / active.len() as i64) as i32,
        )
    }
}

pub struct MacInput {
    source: CGEventSource,
    pointer: Arc<Mutex<PointerState>>,
    touch: Arc<Mutex<TouchState>>,
}

// `CGEventSource` wraps a Core Foundation object (retain/release counted via
// CFRetain/CFRelease, which are thread-safe); the trait objects this backend
// is used through are always accessed behind an `Arc<Mutex<dyn
// InputBackend>>` in main.rs, so there is never concurrent access to the
// underlying `CGEventSourceRef` — only single-threaded moves between the
// caller thread and the keyboard-worker thread.
unsafe impl Send for MacInput {}

impl MacInput {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if !unsafe { AXIsProcessTrusted() } {
            eprintln!(
                "[input] Accessibility permission not granted — grant it to this binary in \
                 System Settings → Privacy & Security → Accessibility, then restart the daemon"
            );
        }

        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("CGEventSourceCreate failed"))?;

        Ok(Self {
            source,
            pointer: Arc::new(Mutex::new(PointerState {
                rect: (0, 0, width, height),
                pos: (0, 0),
                held: None,
            })),
            touch: Arc::new(Mutex::new(TouchState::new())),
        })
    }

    /// Spawn a one-shot thread that waits out `TAP_GRACE` and then, if the
    /// pending tap identified by `generation` hasn't been resolved in the
    /// meantime (cancelled into a scroll, or flushed early by a quick
    /// lift-off), posts the buffered LeftMouseDown and commits to Dragging.
    fn arm_tap_flush(&self, generation: u64) {
        let touch = Arc::clone(&self.touch);
        let source = SendSource(self.source.clone());
        thread::spawn(move || {
            thread::sleep(TAP_GRACE);
            let source = source; // move into this thread
            let mut state = touch.lock().unwrap();
            if state.mode == (GestureMode::Pending { generation }) {
                let (x, y) = state.anchor;
                state.mode = GestureMode::Dragging;
                drop(state);
                post_button_event(&source.0, x, y, HeldButton::Left, true);
            }
        });
    }

    fn post_move(&self, x: i32, y: i32, held: Option<HeldButton>, delta: Option<(i32, i32)>) {
        post_move_event(&self.source, x, y, held, delta);
    }

    fn post_button(&self, x: i32, y: i32, btn: HeldButton, pressed: bool) {
        post_button_event(&self.source, x, y, btn, pressed);
    }

    /// Post a pixel-unit scroll-wheel event. `dy`/`dx` pass through untuned —
    /// see report for details on units/sign conventions not yet calibrated
    /// against a real trackpad.
    fn post_scroll_pixels(&self, dy: i32, dx: i32) {
        post_scroll_event(&self.source, dy, dx);
    }

    /// Keep the shared cursor-position tracker in sync with wherever touch
    /// handling just moved the pointer, so a subsequent relative mouse move
    /// doesn't jump from a stale position.
    fn sync_pointer_pos(&self, x: i32, y: i32) {
        self.pointer.lock().unwrap().pos = (x, y);
    }
}

fn move_event_type(held: Option<HeldButton>) -> CGEventType {
    match held {
        None => CGEventType::MouseMoved,
        Some(HeldButton::Left) => CGEventType::LeftMouseDragged,
        Some(HeldButton::Right) => CGEventType::RightMouseDragged,
        Some(HeldButton::Other) => CGEventType::OtherMouseDragged,
    }
}

fn cg_button(held: Option<HeldButton>) -> CGMouseButton {
    match held {
        Some(HeldButton::Right) => CGMouseButton::Right,
        Some(HeldButton::Other) => CGMouseButton::Center,
        _ => CGMouseButton::Left,
    }
}

/// Post a mouse-moved/dragged event at `(x, y)`, with optional relative delta
/// fields set (for pointer-lock-style consumers reading raw deltas instead of
/// absolute position). Free function (not a method) so both `MacInput` and
/// the deferred tap-flush thread (which only has a cloned `CGEventSource`,
/// not `&MacInput`) can post through it.
fn post_move_event(
    source: &CGEventSource,
    x: i32,
    y: i32,
    held: Option<HeldButton>,
    delta: Option<(i32, i32)>,
) {
    let event_type = move_event_type(held);
    let button = cg_button(held);
    if let Ok(event) = CGEvent::new_mouse_event(
        source.clone(),
        event_type,
        CGPoint::new(x as f64, y as f64),
        button,
    ) {
        if let Some((dx, dy)) = delta {
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx as i64);
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy as i64);
        }
        event.post(CGEventTapLocation::HID);
    } else {
        crate::vlog!("[input] CGEventCreateMouseEvent (move) failed");
    }
}

fn post_button_event(source: &CGEventSource, x: i32, y: i32, btn: HeldButton, pressed: bool) {
    let event_type = match (btn, pressed) {
        (HeldButton::Left, true) => CGEventType::LeftMouseDown,
        (HeldButton::Left, false) => CGEventType::LeftMouseUp,
        (HeldButton::Right, true) => CGEventType::RightMouseDown,
        (HeldButton::Right, false) => CGEventType::RightMouseUp,
        (HeldButton::Other, true) => CGEventType::OtherMouseDown,
        (HeldButton::Other, false) => CGEventType::OtherMouseUp,
    };
    let button = cg_button(Some(btn));
    if let Ok(event) = CGEvent::new_mouse_event(
        source.clone(),
        event_type,
        CGPoint::new(x as f64, y as f64),
        button,
    ) {
        event.post(CGEventTapLocation::HID);
    } else {
        crate::vlog!("[input] CGEventCreateMouseEvent (button) failed");
    }
}

fn post_scroll_event(source: &CGEventSource, dy: i32, dx: i32) {
    if let Ok(event) = CGEvent::new_scroll_event(source.clone(), ScrollEventUnit::PIXEL, 2, dy, dx, 0)
    {
        event.post(CGEventTapLocation::HID);
    } else {
        crate::vlog!("[input] CGEventCreateScrollWheelEvent failed");
    }
}

/// Wire bit layout: 0x01=shift, 0x02=ctrl, 0x04=alt/option, 0x08=super/meta.
/// Mapping decided (not smart-remapped): wire ctrl -> macOS Control literally,
/// wire super/meta -> macOS Command literally.
fn modifier_flags(modifiers: u8) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    if modifiers & 0x01 != 0 {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if modifiers & 0x02 != 0 {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if modifiers & 0x04 != 0 {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if modifiers & 0x08 != 0 {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    flags
}

/// Post a Unicode-text key down+up pair via
/// `CGEventCreateKeyboardEvent`+`CGEventKeyboardSetUnicodeString` (exposed by
/// `core-graphics` as `CGEvent::new_keyboard_event`/`set_string`). Modifier
/// flags, if any, are set directly on the event via `CGEventSetFlags` rather
/// than as separate press/release keys.
fn post_unicode_text(source: &CGEventSource, text: &str, flags: Option<CGEventFlags>) {
    if text.is_empty() {
        return;
    }
    if let Ok(down) = CGEvent::new_keyboard_event(source.clone(), 0, true) {
        if let Some(f) = flags {
            down.set_flags(f);
        }
        down.set_string(text);
        down.post(CGEventTapLocation::HID);
    } else {
        crate::vlog!("[input] CGEventCreateKeyboardEvent (text down) failed");
    }
    if let Ok(up) = CGEvent::new_keyboard_event(source.clone(), 0, false) {
        if let Some(f) = flags {
            up.set_flags(f);
        }
        up.set_string(text);
        up.post(CGEventTapLocation::HID);
    } else {
        crate::vlog!("[input] CGEventCreateKeyboardEvent (text up) failed");
    }
}

/// Post a virtual-keycode down+up pair with modifier flags applied.
fn post_key_with_modifiers(source: &CGEventSource, vk: CGKeyCode, modifiers: u8) {
    let flags = modifier_flags(modifiers);
    if let Ok(down) = CGEvent::new_keyboard_event(source.clone(), vk, true) {
        if !flags.is_empty() {
            down.set_flags(flags);
        }
        down.post(CGEventTapLocation::HID);
    } else {
        crate::vlog!("[input] CGEventCreateKeyboardEvent (down) failed for vk {vk}");
    }
    if let Ok(up) = CGEvent::new_keyboard_event(source.clone(), vk, false) {
        if !flags.is_empty() {
            up.set_flags(flags);
        }
        up.post(CGEventTapLocation::HID);
    } else {
        crate::vlog!("[input] CGEventCreateKeyboardEvent (up) failed for vk {vk}");
    }
}

/// Map HID keyboard usage page 0x07 values to macOS `kVK_*` virtual
/// keycodes. Unlike Windows' `hid_usage_to_vk` (platform/windows/input.rs),
/// this can't use offset formulas for letters or F-keys — macOS ANSI
/// keycodes are physical-QWERTY-position order, not alphabetical or
/// sequential (e.g. `kVK_ANSI_A=0x00` but `kVK_ANSI_B=0x0B`; `kVK_F3=0x63`
/// sits between `kVK_F9=0x65`'s neighbors), so each usage is mapped
/// individually against `KeyCode`'s published constants.
fn hid_usage_to_vk(usage: u8) -> Option<CGKeyCode> {
    Some(match usage {
        0x04 => KeyCode::ANSI_A,
        0x05 => KeyCode::ANSI_B,
        0x06 => KeyCode::ANSI_C,
        0x07 => KeyCode::ANSI_D,
        0x08 => KeyCode::ANSI_E,
        0x09 => KeyCode::ANSI_F,
        0x0A => KeyCode::ANSI_G,
        0x0B => KeyCode::ANSI_H,
        0x0C => KeyCode::ANSI_I,
        0x0D => KeyCode::ANSI_J,
        0x0E => KeyCode::ANSI_K,
        0x0F => KeyCode::ANSI_L,
        0x10 => KeyCode::ANSI_M,
        0x11 => KeyCode::ANSI_N,
        0x12 => KeyCode::ANSI_O,
        0x13 => KeyCode::ANSI_P,
        0x14 => KeyCode::ANSI_Q,
        0x15 => KeyCode::ANSI_R,
        0x16 => KeyCode::ANSI_S,
        0x17 => KeyCode::ANSI_T,
        0x18 => KeyCode::ANSI_U,
        0x19 => KeyCode::ANSI_V,
        0x1A => KeyCode::ANSI_W,
        0x1B => KeyCode::ANSI_X,
        0x1C => KeyCode::ANSI_Y,
        0x1D => KeyCode::ANSI_Z,
        // Row wraps at the end (1..9 then 0), same non-formula caveat noted
        // in the Windows table's comment for this same HID range.
        0x1E => KeyCode::ANSI_1,
        0x1F => KeyCode::ANSI_2,
        0x20 => KeyCode::ANSI_3,
        0x21 => KeyCode::ANSI_4,
        0x22 => KeyCode::ANSI_5,
        0x23 => KeyCode::ANSI_6,
        0x24 => KeyCode::ANSI_7,
        0x25 => KeyCode::ANSI_8,
        0x26 => KeyCode::ANSI_9,
        0x27 => KeyCode::ANSI_0,
        0x28 => KeyCode::RETURN,
        0x29 => KeyCode::ESCAPE,
        0x2A => KeyCode::DELETE, // Backspace
        0x2B => KeyCode::TAB,
        0x2C => KeyCode::SPACE,
        0x2D => KeyCode::ANSI_MINUS,
        0x2E => KeyCode::ANSI_EQUAL,
        0x2F => KeyCode::ANSI_LEFT_BRACKET,
        0x30 => KeyCode::ANSI_RIGHT_BRACKET,
        0x31 => KeyCode::ANSI_BACKSLASH,
        0x33 => KeyCode::ANSI_SEMICOLON,
        0x34 => KeyCode::ANSI_QUOTE,
        0x35 => KeyCode::ANSI_GRAVE,
        0x36 => KeyCode::ANSI_COMMA,
        0x37 => KeyCode::ANSI_PERIOD,
        0x38 => KeyCode::ANSI_SLASH,
        0x39 => KeyCode::CAPS_LOCK,
        0x3A => KeyCode::F1,
        0x3B => KeyCode::F2,
        0x3C => KeyCode::F3,
        0x3D => KeyCode::F4,
        0x3E => KeyCode::F5,
        0x3F => KeyCode::F6,
        0x40 => KeyCode::F7,
        0x41 => KeyCode::F8,
        0x42 => KeyCode::F9,
        0x43 => KeyCode::F10,
        0x44 => KeyCode::F11,
        0x45 => KeyCode::F12,
        // 0x46 PrintScreen, 0x47 ScrollLock, 0x48 Pause: no macOS equivalent.
        0x49 => KeyCode::HELP, // Insert -> closest analog on Mac extended kbds
        0x4A => KeyCode::HOME,
        0x4B => KeyCode::PAGE_UP,
        0x4C => KeyCode::FORWARD_DELETE,
        0x4D => KeyCode::END,
        0x4E => KeyCode::PAGE_DOWN,
        0x4F => KeyCode::RIGHT_ARROW,
        0x50 => KeyCode::LEFT_ARROW,
        0x51 => KeyCode::DOWN_ARROW,
        0x52 => KeyCode::UP_ARROW,
        0x53 => KeyCode::ANSI_KEYPAD_CLEAR, // NumLock -> Mac's "Clear" position
        0x54 => KeyCode::ANSI_KEYPAD_DIVIDE,
        0x55 => KeyCode::ANSI_KEYPAD_MULTIPLY,
        0x56 => KeyCode::ANSI_KEYPAD_MINUS,
        0x57 => KeyCode::ANSI_KEYPAD_PLUS,
        0x58 => KeyCode::ANSI_KEYPAD_ENTER,
        0x59 => KeyCode::ANSI_KEYPAD_1,
        0x5A => KeyCode::ANSI_KEYPAD_2,
        0x5B => KeyCode::ANSI_KEYPAD_3,
        0x5C => KeyCode::ANSI_KEYPAD_4,
        0x5D => KeyCode::ANSI_KEYPAD_5,
        0x5E => KeyCode::ANSI_KEYPAD_6,
        0x5F => KeyCode::ANSI_KEYPAD_7,
        0x60 => KeyCode::ANSI_KEYPAD_8,
        0x61 => KeyCode::ANSI_KEYPAD_9,
        0x62 => KeyCode::ANSI_KEYPAD_0,
        0x63 => KeyCode::ANSI_KEYPAD_DECIMAL,
        0x64 => KeyCode::ISO_SECTION, // Non-US \ and | (ISO keyboards)
        0xE0 => KeyCode::CONTROL,
        0xE1 => KeyCode::SHIFT,
        0xE2 => KeyCode::OPTION,
        0xE3 => KeyCode::COMMAND,
        0xE4 => KeyCode::RIGHT_CONTROL,
        0xE5 => KeyCode::RIGHT_SHIFT,
        0xE6 => KeyCode::RIGHT_OPTION,
        0xE7 => KeyCode::RIGHT_COMMAND,
        _ => return None,
    })
}

/// `CGEventSource` isn't `Send` (it's a raw `NonNull` under the hood), but
/// it's just a CFType wrapper — CFRetain/CFRelease are thread-safe, and we
/// only ever touch the underlying source from one thread at a time (the
/// caller thread, or the one-shot tap-flush thread below, never both at
/// once). Lets a cloned source move into `thread::spawn`.
struct SendSource(CGEventSource);
unsafe impl Send for SendSource {}

impl InputBackend for MacInput {
    fn set_target_rect(&mut self, left: i32, top: i32, width: u32, height: u32) {
        let mut state = self.pointer.lock().unwrap();
        state.rect = (left, top, width, height);
    }

    fn touch(&mut self, contacts: &[TouchContact]) -> Result<()> {
        // No native macOS touch-injection API exists; translate to
        // pointer/scroll events the same way Duet Display/Jump
        // Desktop/Screens/Splashtop do. `contacts` is always the full
        // currently-active set (see `parse_touch_packet`), so per call we
        // can tell 1-finger from 2-finger without necessarily needing a
        // timer — the grace window below only covers the ambiguous instant
        // right at first touch-down, before we know if a second finger is
        // about to join.
        if contacts.is_empty() {
            return Ok(());
        }

        let (left, top, _w, _h) = self.pointer.lock().unwrap().rect;

        let mut state = self.touch.lock().unwrap();
        for c in contacts {
            let slot = c.slot as usize;
            if slot >= MAX_TOUCH_SLOTS {
                crate::vlog!("[input] touch slot {slot} out of range, ignoring");
                continue;
            }
            // Unlike MouseEvent::MoveAbsolute (0..65535 normalized), touch
            // wire coordinates are already in the daemon's target
            // width/height space (mirrors the Windows backend's `touch()`),
            // so just offset by where the captured output sits on screen.
            let x = left + c.x as i32;
            let y = top + c.y as i32;
            match c.event_type {
                TOUCH_DOWN | TOUCH_MOVE => state.slots[slot] = Some((x, y)),
                TOUCH_UP => state.slots[slot] = None,
                other => crate::vlog!("[input] unknown touch event_type {other}"),
            }
        }

        let active_count = state.active_count();

        match state.mode {
            GestureMode::Idle => {
                if active_count == 1 {
                    let anchor = state.primary_pos().unwrap_or((left, top));
                    state.generation = state.generation.wrapping_add(1);
                    let generation = state.generation;
                    state.anchor = anchor;
                    state.mode = GestureMode::Pending { generation };
                    drop(state);
                    self.arm_tap_flush(generation);
                } else if active_count >= 2 {
                    state.scroll_last = state.centroid();
                    state.mode = GestureMode::Scrolling;
                }
                // active_count == 0 here can't happen (contacts non-empty
                // implies at least one slot just went to Some/None, and if
                // it went to None while already Idle there's nothing to do).
            }
            GestureMode::Pending { generation } => {
                if active_count == 0 {
                    // Lifted before the grace window elapsed: a quick tap.
                    // Flush immediately instead of waiting out the timer.
                    let (x, y) = state.anchor;
                    state.generation = state.generation.wrapping_add(1);
                    state.mode = GestureMode::Idle;
                    drop(state);
                    post_button_event(&self.source, x, y, HeldButton::Left, true);
                    post_button_event(&self.source, x, y, HeldButton::Left, false);
                } else if active_count >= 2 {
                    // Second finger joined in time — this was actually a
                    // two-finger scroll starting, not a click. Cancel the
                    // pending tap (bump generation so the armed flush thread
                    // no-ops) and never post the LeftMouseDown at all.
                    let _ = generation;
                    state.generation = state.generation.wrapping_add(1);
                    state.scroll_last = state.centroid();
                    state.mode = GestureMode::Scrolling;
                }
                // active_count == 1: still just the one finger, keep
                // waiting for the deferred flush thread's timer.
            }
            GestureMode::Dragging => {
                if active_count == 0 {
                    let (x, y) = state.primary_pos().unwrap_or(state.anchor);
                    state.mode = GestureMode::Idle;
                    drop(state);
                    post_button_event(&self.source, x, y, HeldButton::Left, false);
                    self.sync_pointer_pos(x, y);
                } else if let Some((x, y)) = state.slots[0] {
                    drop(state);
                    post_move_event(&self.source, x, y, Some(HeldButton::Left), None);
                    self.sync_pointer_pos(x, y);
                }
                // A second finger appearing mid-drag is left alone here —
                // we've already committed to the click/drag gesture, and
                // silently dropping the drag on an incidental second-finger
                // brush would be worse than ignoring the extra contact.
            }
            GestureMode::Scrolling => {
                if active_count >= 2 {
                    let centroid = state.centroid();
                    let (lx, ly) = state.scroll_last;
                    let (dx, dy) = (centroid.0 - lx, centroid.1 - ly);
                    state.scroll_last = centroid;
                    drop(state);
                    if dx != 0 || dy != 0 {
                        // Sign/unit convention untuned against a real
                        // trackpad — see report.
                        self.post_scroll_pixels(dy, dx);
                    }
                } else {
                    // Down to 0 or 1 fingers ends the scroll gesture; per
                    // spec, two-finger lift never emits a click.
                    state.mode = GestureMode::Idle;
                }
            }
        }
        Ok(())
    }

    fn key_text(&mut self, text: &str) -> Result<()> {
        post_unicode_text(&self.source, text, None);
        Ok(())
    }

    fn key_special(&mut self, code: u8, modifiers: u8) -> Result<()> {
        // Mirrors `key_special` in platform/windows/input.rs 1:1 (itself
        // mirroring `special_to_keycode()` in platform/linux/uinput.rs) —
        // that table is the behavioral spec for this wire format.
        let vk = match code {
            SPECIAL_BACKSPACE => KeyCode::DELETE, // Mac "delete" key = PC backspace
            0x02 => KeyCode::RETURN,
            0x03 => KeyCode::TAB,
            0x04 => KeyCode::ESCAPE,
            0x05 => KeyCode::LEFT_ARROW,
            0x06 => KeyCode::RIGHT_ARROW,
            0x07 => KeyCode::UP_ARROW,
            0x08 => KeyCode::DOWN_ARROW,
            0x09 => KeyCode::FORWARD_DELETE, // PC "Delete" key = Mac forward-delete
            0x0A => KeyCode::HOME,
            0x0B => KeyCode::END,
            0x0C => KeyCode::CONTROL,
            0x0D => KeyCode::OPTION,
            0x0E => KeyCode::COMMAND, // win/meta -> Command, literally, not remapped
            0x0F => {
                crate::vlog!(
                    "[input] special key Insert (0x0F) has no macOS equivalent, ignoring"
                );
                return Ok(());
            }
            _ => {
                crate::vlog!("[input] unknown special key code 0x{code:02x}");
                return Ok(());
            }
        };
        post_key_with_modifiers(&self.source, vk, modifiers);
        Ok(())
    }

    fn key_text_with_modifiers(&mut self, text: &str, modifiers: u8) -> Result<()> {
        let flags = modifier_flags(modifiers);
        post_unicode_text(
            &self.source,
            text,
            if flags.is_empty() { None } else { Some(flags) },
        );
        Ok(())
    }

    fn key_raw_hid(&mut self, usage: u8, pressed: bool) -> Result<()> {
        if let Some(vk) = hid_usage_to_vk(usage) {
            if let Ok(event) = CGEvent::new_keyboard_event(self.source.clone(), vk, pressed) {
                event.post(CGEventTapLocation::HID);
            } else {
                crate::vlog!("[input] CGEventCreateKeyboardEvent failed for HID usage 0x{usage:02x}");
            }
        } else {
            crate::vlog!("[input] no macOS keycode mapping for HID usage 0x{usage:02x}");
        }
        Ok(())
    }

    fn mouse(&mut self, ev: MouseEvent) -> Result<()> {
        match ev {
            MouseEvent::MoveAbsolute { x, y } => {
                let mut state = self.pointer.lock().unwrap();
                let (left, top, width, height) = state.rect;
                let fx = x as f32 / 65535.0;
                let fy = y as f32 / 65535.0;
                let abs_x = left + (fx * width as f32) as i32;
                let abs_y = top + (fy * height as f32) as i32;
                let (cx, cy) = state.clamp_to_rect(abs_x, abs_y);
                let delta = (cx - state.pos.0, cy - state.pos.1);
                let held = state.held;
                state.pos = (cx, cy);
                drop(state);
                self.post_move(cx, cy, held, Some(delta));
            }
            MouseEvent::MoveRelative { dx, dy } => {
                let mut state = self.pointer.lock().unwrap();
                let (px, py) = state.pos;
                let (cx, cy) = state.clamp_to_rect(px + dx as i32, py + dy as i32);
                let held = state.held;
                state.pos = (cx, cy);
                drop(state);
                self.post_move(cx, cy, held, Some((dx as i32, dy as i32)));
            }
            MouseEvent::Button {
                btn,
                state: btn_state,
            } => {
                let held_kind = match btn {
                    0 => HeldButton::Left,
                    1 => HeldButton::Right,
                    2 => HeldButton::Other,
                    _ => return Ok(()),
                };
                let pressed = btn_state != 0;
                let mut state = self.pointer.lock().unwrap();
                let (x, y) = state.pos;
                state.held = if pressed { Some(held_kind) } else { None };
                drop(state);
                self.post_button(x, y, held_kind, pressed);
            }
            MouseEvent::Scroll { dy } => {
                self.post_scroll_pixels(dy as i32, 0);
            }
        }
        Ok(())
    }

    fn gamepad_attach(&mut self, _id: u8) -> Result<()> {
        Err(anyhow::anyhow!(
            "gamepad passthrough not supported on macOS"
        ))
    }

    fn gamepad_detach(&mut self, _id: u8) {}

    fn gamepad_state(&mut self, _id: u8, _st: &GamepadState) -> Result<()> {
        Err(anyhow::anyhow!(
            "gamepad passthrough not supported on macOS"
        ))
    }
}

#[cfg(test)]
mod manual_verification {
    //! Not real automated tests — these have observable side effects on the
    //! *real* desktop (move the cursor, click, type into whatever has
    //! focus). `#[ignore]`d so `cargo test` doesn't trigger them by
    //! accident; run explicitly with e.g.:
    //!   cargo test --lib platform::macos::input::manual_verification -- --ignored --nocapture
    use super::*;
    use std::time::Duration;

    #[test]
    #[ignore]
    fn move_and_click() {
        let mut input = MacInput::new(1280, 800).expect("MacInput::new failed");
        input.set_target_rect(0, 0, 1280, 800);
        // Dead center of the target rect.
        input
            .mouse(MouseEvent::MoveAbsolute { x: 32767, y: 32767 })
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        input.mouse(MouseEvent::Button { btn: 0, state: 1 }).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        input.mouse(MouseEvent::Button { btn: 0, state: 0 }).unwrap();
    }

    #[test]
    #[ignore]
    fn type_text() {
        let mut input = MacInput::new(1280, 800).expect("MacInput::new failed");
        // Give a manual tester a moment to focus a text field after starting
        // this test (Notes/TextEdit/terminal).
        std::thread::sleep(Duration::from_secs(3));
        input.key_text("hello from screx macOS input\n").unwrap();
    }

    /// Not visual — just exercises the touch gesture state machine
    /// (Idle->Pending->quick-tap-flush, Idle->Pending->2-finger-cancel->
    /// Scrolling->Idle) end to end without panicking, including the
    /// deferred tap-flush background thread.
    #[test]
    #[ignore]
    fn touch_gesture_smoke() {
        let mut input = MacInput::new(1280, 800).expect("MacInput::new failed");
        input.set_target_rect(0, 0, 1280, 800);

        // Quick single-finger tap, lifted well within the grace window.
        input
            .touch(&[TouchContact {
                slot: 0,
                event_type: TOUCH_DOWN,
                x: 100,
                y: 100,
            }])
            .unwrap();
        input
            .touch(&[TouchContact {
                slot: 0,
                event_type: TOUCH_UP,
                x: 100,
                y: 100,
            }])
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // Two-finger scroll: both contacts arrive together in one call.
        input
            .touch(&[
                TouchContact {
                    slot: 0,
                    event_type: TOUCH_DOWN,
                    x: 200,
                    y: 200,
                },
                TouchContact {
                    slot: 1,
                    event_type: TOUCH_DOWN,
                    x: 220,
                    y: 200,
                },
            ])
            .unwrap();
        input
            .touch(&[
                TouchContact {
                    slot: 0,
                    event_type: TOUCH_MOVE,
                    x: 200,
                    y: 170,
                },
                TouchContact {
                    slot: 1,
                    event_type: TOUCH_MOVE,
                    x: 220,
                    y: 170,
                },
            ])
            .unwrap();
        input
            .touch(&[
                TouchContact {
                    slot: 0,
                    event_type: TOUCH_UP,
                    x: 200,
                    y: 170,
                },
                TouchContact {
                    slot: 1,
                    event_type: TOUCH_UP,
                    x: 220,
                    y: 170,
                },
            ])
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // Single finger down long enough for the grace window to elapse and
        // the deferred flush thread to commit to Dragging, then a move and
        // a lift.
        input
            .touch(&[TouchContact {
                slot: 0,
                event_type: TOUCH_DOWN,
                x: 300,
                y: 300,
            }])
            .unwrap();
        std::thread::sleep(Duration::from_millis(60));
        input
            .touch(&[TouchContact {
                slot: 0,
                event_type: TOUCH_MOVE,
                x: 310,
                y: 310,
            }])
            .unwrap();
        input
            .touch(&[TouchContact {
                slot: 0,
                event_type: TOUCH_UP,
                x: 310,
                y: 310,
            }])
            .unwrap();
    }
}
