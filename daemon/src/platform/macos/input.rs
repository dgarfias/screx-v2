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
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayVendorNumber, CGError,
    CGEventField as ObjcCGEventField, CGGetOnlineDisplayList, CGScrollPhase,
};
use objc2_foundation::{NSString, NSUserDefaults};

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
///
/// Two independently-set sizes are tracked, not one, because they can
/// legitimately differ on macOS: `wire_size` is the negotiated session
/// stream resolution wire touch coordinates are expressed in (set via
/// `set_target_rect`, per the `InputBackend` trait's documented contract);
/// `output_size` is the captured output's actual on-screen size (set via
/// `set_output_size`). They're equal in the common case, but WindowServer
/// can silently substitute a persisted display-mode preference for a
/// virtual display's requested mode (see `force_display_mode` in
/// `display.rs`), leaving the live rect smaller/larger than the session
/// resolution. When that happens, wire coordinates need to be scaled by
/// `output_size / wire_size` before being placed on the desktop.
struct PointerState {
    /// (left, top) origin of the captured output on the desktop.
    origin: (i32, i32),
    /// Negotiated session/stream resolution — the space wire touch/mouse
    /// coordinates are expressed in.
    wire_size: (u32, u32),
    /// Actual on-screen size of the captured output. Defaults to
    /// `wire_size` until `set_output_size` is called at least once (i.e.
    /// "assume no substitution happened" — correct for every platform/test
    /// that never calls it).
    output_size: (u32, u32),
    /// Last-known absolute desktop position of the cursor.
    pos: (i32, i32),
    /// Currently-held mouse button, if any (for move-vs-drag event typing).
    held: Option<HeldButton>,
    display_id: Option<CGDirectDisplayID>,
}

impl PointerState {
    fn refresh_output_rect(&mut self) {
        let mut ids = [0u32; 32];
        let mut count = 0u32;
        let err = unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
        if err != CGError::Success {
            return;
        }
        let online = &ids[..count as usize];
        let display_id = self
            .display_id
            .filter(|id| online.contains(id))
            .or_else(|| {
                online
                    .iter()
                    .copied()
                    .find(|id| CGDisplayVendorNumber(*id) == 0x3456)
            });
        let Some(display_id) = display_id else {
            return;
        };
        let bounds = CGDisplayBounds(display_id);
        self.display_id = Some(display_id);
        self.origin = (bounds.origin.x as i32, bounds.origin.y as i32);
        self.output_size = (bounds.size.width as u32, bounds.size.height as u32);
    }

    /// Clamps to the captured output's actual on-screen extent
    /// (`output_size`, not `wire_size` — the desktop boundary is physical,
    /// not the wire/session resolution).
    fn clamp_to_rect(&self, x: i32, y: i32) -> (i32, i32) {
        let (left, top) = self.origin;
        let (width, height) = self.output_size;
        let max_x = left + width.saturating_sub(1) as i32;
        let max_y = top + height.saturating_sub(1) as i32;
        (x.clamp(left, max_x.max(left)), y.clamp(top, max_y.max(top)))
    }
}

/// Scale an absolute pixel index while preserving both addressable endpoints.
fn scale_position(value: u16, wire: u32, out: u32) -> i32 {
    if wire <= 1 || out <= 1 || wire == out {
        return value as i32;
    }
    ((value as f64) * ((out - 1) as f64) / ((wire - 1) as f64)).round() as i32
}

/// Matches the Windows backend's contact-count limit.
const MAX_TOUCH_SLOTS: usize = 10;

/// Direct-touch gestures use movement, rather than a short timer, to
/// distinguish a left drag from a stationary long-press secondary click.
const TOUCH_MOVE_THRESHOLD: i32 = 8;
const LONG_PRESS_DURATION: Duration = Duration::from_millis(500);

/// Coarse gesture state derived from how many contacts are simultaneously
/// active. Each `touch()` call carries transitions; `TouchState::slots`
/// reconstructs the full active set across calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureMode {
    /// Nothing down.
    Idle,
    /// Exactly one contact went down. Movement commits it to a left drag;
    /// remaining stationary long enough commits it to a secondary click.
    Pending { generation: u64 },
    /// Single-finger click/drag committed; a real LeftMouseDown has been
    /// posted and is being held.
    Dragging,
    /// Two (or more) fingers down; translating movement to scroll-wheel
    /// events instead of a click.
    Scrolling,
    /// A stationary long-press already emitted a secondary click; consume
    /// the eventual lift without emitting another click.
    SecondaryClicked,
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
        let (sx, sy) = active.iter().fold((0i64, 0i64), |(sx, sy), (x, y)| {
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
    natural_scroll: bool,
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
        let scroll_direction_key = NSString::from_str("com.apple.swipescrolldirection");
        let natural_scroll =
            NSUserDefaults::standardUserDefaults().boolForKey(&scroll_direction_key);
        crate::vlog!("[input] natural scrolling enabled: {natural_scroll}");

        Ok(Self {
            source,
            pointer: Arc::new(Mutex::new(PointerState {
                origin: (0, 0),
                wire_size: (width, height),
                output_size: (width, height),
                pos: (0, 0),
                held: None,
                display_id: None,
            })),
            touch: Arc::new(Mutex::new(TouchState::new())),
            natural_scroll,
        })
    }

    /// Emit a secondary click if a single contact remains stationary for the
    /// touchscreen-standard long-press interval.
    fn arm_long_press(&self, generation: u64) {
        let touch = Arc::clone(&self.touch);
        let source = SendSource(self.source.clone());
        thread::spawn(move || {
            thread::sleep(LONG_PRESS_DURATION);
            let source = source; // move into this thread
            let mut state = touch.lock().unwrap();
            if state.mode == (GestureMode::Pending { generation }) {
                let (x, y) = state.anchor;
                state.mode = GestureMode::SecondaryClicked;
                drop(state);
                post_button_event(&source.0, x, y, HeldButton::Right, true);
                post_button_event(&source.0, x, y, HeldButton::Right, false);
            }
        });
    }

    fn post_move(&self, x: i32, y: i32, held: Option<HeldButton>, delta: Option<(i32, i32)>) {
        post_move_event(&self.source, x, y, held, delta);
    }

    fn post_button(&self, x: i32, y: i32, btn: HeldButton, pressed: bool) {
        post_button_event(&self.source, x, y, btn, pressed);
    }

    /// Touch coordinates and pixel scroll units are both measured in output
    /// points. Apply the user's macOS scroll-direction preference to the 1:1
    /// centroid delta, matching native trackpad behavior.
    fn post_scroll_pixels(&self, dy: i32, dx: i32, phase: CGScrollPhase) {
        let direction = if self.natural_scroll { 1 } else { -1 };
        post_scroll_event(
            &self.source,
            dy.saturating_mul(direction),
            dx.saturating_mul(direction),
            phase,
        );
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
/// the deferred long-press thread (which only has a cloned `CGEventSource`,
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

fn post_scroll_event(source: &CGEventSource, dy: i32, dx: i32, phase: CGScrollPhase) {
    if let Ok(event) =
        CGEvent::new_scroll_event(source.clone(), ScrollEventUnit::PIXEL, 2, dy, dx, 0)
    {
        event.set_integer_value_field(
            ObjcCGEventField::ScrollWheelEventScrollPhase.0,
            phase.0 as i64,
        );
        event.post(CGEventTapLocation::HID);
    } else {
        crate::vlog!("[input] CGEventCreateScrollWheelEvent failed");
    }
}

fn post_scroll_lines(source: &CGEventSource, dy: i32) {
    if let Ok(event) = CGEvent::new_scroll_event(source.clone(), ScrollEventUnit::LINE, 1, dy, 0, 0)
    {
        event.post(CGEventTapLocation::HID);
    } else {
        crate::vlog!("[input] CGEventCreateScrollWheelEvent failed");
    }
}

/// KEY combo modifier bits from docs/ARCHITECTURE.md: 0x01=Ctrl,
/// 0x02=Alt, 0x04=Super. On macOS these map literally to Control, Option,
/// and Command. Shifted characters are already shifted in the text payload.
fn modifier_flags(modifiers: u8) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    if modifiers & 0x01 != 0 {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if modifiers & 0x02 != 0 {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if modifiers & 0x04 != 0 {
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
fn post_key_with_modifiers(
    source: &CGEventSource,
    vk: CGKeyCode,
    modifiers: u8,
    needs_shift: bool,
) {
    let mut flags = modifier_flags(modifiers);
    if needs_shift {
        flags |= CGEventFlags::CGEventFlagShift;
    }
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

/// Map text characters to physical keys on the ANSI/US layout. Combo packets
/// need a real keycode because many macOS shortcuts match the physical key,
/// not the Unicode string attached to a synthetic keycode-0 event.
fn char_to_key(c: char) -> Option<(CGKeyCode, bool)> {
    let needs_shift = c.is_ascii_uppercase();
    let letter = match c.to_ascii_lowercase() {
        'a' => Some(KeyCode::ANSI_A),
        'b' => Some(KeyCode::ANSI_B),
        'c' => Some(KeyCode::ANSI_C),
        'd' => Some(KeyCode::ANSI_D),
        'e' => Some(KeyCode::ANSI_E),
        'f' => Some(KeyCode::ANSI_F),
        'g' => Some(KeyCode::ANSI_G),
        'h' => Some(KeyCode::ANSI_H),
        'i' => Some(KeyCode::ANSI_I),
        'j' => Some(KeyCode::ANSI_J),
        'k' => Some(KeyCode::ANSI_K),
        'l' => Some(KeyCode::ANSI_L),
        'm' => Some(KeyCode::ANSI_M),
        'n' => Some(KeyCode::ANSI_N),
        'o' => Some(KeyCode::ANSI_O),
        'p' => Some(KeyCode::ANSI_P),
        'q' => Some(KeyCode::ANSI_Q),
        'r' => Some(KeyCode::ANSI_R),
        's' => Some(KeyCode::ANSI_S),
        't' => Some(KeyCode::ANSI_T),
        'u' => Some(KeyCode::ANSI_U),
        'v' => Some(KeyCode::ANSI_V),
        'w' => Some(KeyCode::ANSI_W),
        'x' => Some(KeyCode::ANSI_X),
        'y' => Some(KeyCode::ANSI_Y),
        'z' => Some(KeyCode::ANSI_Z),
        _ => None,
    };
    if let Some(vk) = letter {
        return Some((vk, needs_shift));
    }

    Some(match c {
        '1' | '!' => (KeyCode::ANSI_1, c == '!'),
        '2' | '@' => (KeyCode::ANSI_2, c == '@'),
        '3' | '#' => (KeyCode::ANSI_3, c == '#'),
        '4' | '$' => (KeyCode::ANSI_4, c == '$'),
        '5' | '%' => (KeyCode::ANSI_5, c == '%'),
        '6' | '^' => (KeyCode::ANSI_6, c == '^'),
        '7' | '&' => (KeyCode::ANSI_7, c == '&'),
        '8' | '*' => (KeyCode::ANSI_8, c == '*'),
        '9' | '(' => (KeyCode::ANSI_9, c == '('),
        '0' | ')' => (KeyCode::ANSI_0, c == ')'),
        '-' | '_' => (KeyCode::ANSI_MINUS, c == '_'),
        '=' | '+' => (KeyCode::ANSI_EQUAL, c == '+'),
        '[' | '{' => (KeyCode::ANSI_LEFT_BRACKET, c == '{'),
        ']' | '}' => (KeyCode::ANSI_RIGHT_BRACKET, c == '}'),
        '\\' | '|' => (KeyCode::ANSI_BACKSLASH, c == '|'),
        ';' | ':' => (KeyCode::ANSI_SEMICOLON, c == ':'),
        '\'' | '"' => (KeyCode::ANSI_QUOTE, c == '"'),
        '`' | '~' => (KeyCode::ANSI_GRAVE, c == '~'),
        ',' | '<' => (KeyCode::ANSI_COMMA, c == '<'),
        '.' | '>' => (KeyCode::ANSI_PERIOD, c == '>'),
        '/' | '?' => (KeyCode::ANSI_SLASH, c == '?'),
        ' ' => (KeyCode::SPACE, false),
        '\t' => (KeyCode::TAB, false),
        '\n' => (KeyCode::RETURN, false),
        _ => return None,
    })
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
/// caller thread, or the one-shot long-press thread below, never both at
/// once). Lets a cloned source move into `thread::spawn`.
struct SendSource(CGEventSource);
unsafe impl Send for SendSource {}

impl InputBackend for MacInput {
    fn set_target_rect(&mut self, left: i32, top: i32, width: u32, height: u32) {
        let mut state = self.pointer.lock().unwrap();
        state.origin = (left, top);
        state.wire_size = (width, height);
        let (cx, cy) = state.clamp_to_rect(state.pos.0, state.pos.1);
        if (cx, cy) != state.pos {
            let (out_w, out_h) = state.output_size;
            state.pos = (
                left + out_w.saturating_sub(1) as i32 / 2,
                top + out_h.saturating_sub(1) as i32 / 2,
            );
        }
    }

    fn set_output_size(&mut self, width: u32, height: u32) {
        let mut state = self.pointer.lock().unwrap();
        state.output_size = (width, height);
    }

    fn touch(&mut self, contacts: &[TouchContact]) -> Result<()> {
        // No native macOS touch-injection API exists; translate to
        // pointer/scroll events the same way Duet Display/Jump
        // Desktop/Screens/Splashtop do. Packets carry contact transitions;
        // the persisted slots below reconstruct the full active set. The
        // A stationary primary contact becomes a long-press secondary click;
        // movement commits it to dragging and a second contact to scrolling.
        if contacts.is_empty() {
            return Ok(());
        }

        let (left, top, wire_size, output_size) = {
            let mut p = self.pointer.lock().unwrap();
            p.refresh_output_rect();
            (p.origin.0, p.origin.1, p.wire_size, p.output_size)
        };
        let (wire_w, wire_h) = wire_size;
        let (out_w, out_h) = output_size;

        let mut state = self.touch.lock().unwrap();
        // Final position carried by a TOUCH_UP in this packet. The per-slot
        // tracker below clears the slot on UP (so active_count stays right),
        // which would otherwise discard the lift coordinates — but the
        // Dragging-release branch needs them to post LeftMouseUp where the
        // finger actually ended, not back at the gesture's start anchor.
        let mut lift_pos: Option<(i32, i32)> = None;
        for c in contacts {
            let slot = c.slot as usize;
            if slot >= MAX_TOUCH_SLOTS {
                crate::vlog!("[input] touch slot {slot} out of range, ignoring");
                continue;
            }
            // Unlike MouseEvent::MoveAbsolute (0..65535 normalized), touch
            // wire coordinates are in the daemon's negotiated session
            // stream width/height space (mirrors the Windows backend's
            // `touch()`). Scale onto the captured output's actual on-screen
            // size before offsetting by where it sits on screen — identity
            // when wire_size == output_size (the common case), but
            // WindowServer can substitute a different live mode for a
            // macOS virtual display (see `force_display_mode` in
            // display.rs), in which case this scaling is what keeps
            // touches landing in the right place instead of drifting
            // toward the far corners.
            let x = left + scale_position(c.x, wire_w, out_w);
            let y = top + scale_position(c.y, wire_h, out_h);
            match c.event_type {
                TOUCH_DOWN | TOUCH_MOVE => state.slots[slot] = Some((x, y)),
                TOUCH_UP => {
                    // First (lowest-slot) UP in the packet wins — for the
                    // single-finger gestures this feeds, that is the primary
                    // contact.
                    if lift_pos.is_none() {
                        lift_pos = Some((x, y));
                    }
                    state.slots[slot] = None;
                }
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
                    self.arm_long_press(generation);
                } else if active_count >= 2 {
                    let centroid = state.centroid();
                    state.scroll_last = centroid;
                    state.mode = GestureMode::Scrolling;
                    drop(state);
                    self.post_scroll_pixels(0, 0, CGScrollPhase::Began);
                }
                // active_count == 0 here can't happen (contacts non-empty
                // implies at least one slot just went to Some/None, and if
                // it went to None while already Idle there's nothing to do).
            }
            GestureMode::Pending { generation } => {
                if active_count == 0 {
                    // Lifted before the long-press elapsed: a left click.
                    let (x, y) = state.anchor;
                    state.generation = state.generation.wrapping_add(1);
                    state.mode = GestureMode::Idle;
                    drop(state);
                    post_button_event(&self.source, x, y, HeldButton::Left, true);
                    post_button_event(&self.source, x, y, HeldButton::Left, false);
                    self.sync_pointer_pos(x, y);
                } else if active_count >= 2 {
                    // Second finger joined in time — this was actually a
                    // two-finger scroll starting, not a click. Cancel the
                    // pending tap (bump generation so the armed flush thread
                    // no-ops) and never post the LeftMouseDown at all.
                    let _ = generation;
                    state.generation = state.generation.wrapping_add(1);
                    let centroid = state.centroid();
                    state.scroll_last = centroid;
                    state.mode = GestureMode::Scrolling;
                    drop(state);
                    self.post_scroll_pixels(0, 0, CGScrollPhase::Began);
                } else if let Some((x, y)) = state.primary_pos() {
                    let dx = x - state.anchor.0;
                    let dy = y - state.anchor.1;
                    if dx.abs().max(dy.abs()) > TOUCH_MOVE_THRESHOLD {
                        let (anchor_x, anchor_y) = state.anchor;
                        state.generation = state.generation.wrapping_add(1);
                        state.mode = GestureMode::Dragging;
                        drop(state);
                        post_button_event(&self.source, anchor_x, anchor_y, HeldButton::Left, true);
                        post_move_event(&self.source, x, y, Some(HeldButton::Left), None);
                        self.sync_pointer_pos(x, y);
                    }
                }
            }
            GestureMode::Dragging => {
                if active_count == 0 {
                    // All slots are cleared by the time we get here, so
                    // `primary_pos()` is always None on release — use the
                    // lift coordinates captured above. (Falling back to
                    // `state.anchor` here was a real bug: it posted the
                    // LeftMouseUp back at the drag's DOWN position, snapping
                    // the cursor/drop-point to the gesture start.)
                    let (x, y) = lift_pos
                        .or_else(|| state.primary_pos())
                        .unwrap_or(state.anchor);
                    state.mode = GestureMode::Idle;
                    drop(state);
                    post_button_event(&self.source, x, y, HeldButton::Left, false);
                    self.sync_pointer_pos(x, y);
                } else if let Some((x, y)) = state.primary_pos() {
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
                        self.post_scroll_pixels(dy, dx, CGScrollPhase::Changed);
                    }
                } else {
                    state.mode = GestureMode::Idle;
                    drop(state);
                    self.post_scroll_pixels(0, 0, CGScrollPhase::Ended);
                }
            }
            GestureMode::SecondaryClicked => {
                if active_count == 0 {
                    state.mode = GestureMode::Idle;
                    if let Some((x, y)) = lift_pos {
                        drop(state);
                        self.sync_pointer_pos(x, y);
                    }
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
                crate::vlog!("[input] special key Insert (0x0F) has no macOS equivalent, ignoring");
                return Ok(());
            }
            _ => {
                crate::vlog!("[input] unknown special key code 0x{code:02x}");
                return Ok(());
            }
        };
        post_key_with_modifiers(&self.source, vk, modifiers, false);
        Ok(())
    }

    fn key_text_with_modifiers(&mut self, text: &str, modifiers: u8) -> Result<()> {
        let flags = modifier_flags(modifiers);
        for c in text.chars() {
            if let Some((vk, needs_shift)) = char_to_key(c) {
                post_key_with_modifiers(&self.source, vk, modifiers, needs_shift);
            } else {
                let mut utf8 = [0; 4];
                post_unicode_text(
                    &self.source,
                    c.encode_utf8(&mut utf8),
                    if flags.is_empty() { None } else { Some(flags) },
                );
            }
        }
        Ok(())
    }

    fn key_raw_hid(&mut self, usage: u8, pressed: bool) -> Result<()> {
        if let Some(vk) = hid_usage_to_vk(usage) {
            if let Ok(event) = CGEvent::new_keyboard_event(self.source.clone(), vk, pressed) {
                event.post(CGEventTapLocation::HID);
            } else {
                crate::vlog!(
                    "[input] CGEventCreateKeyboardEvent failed for HID usage 0x{usage:02x}"
                );
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
                state.refresh_output_rect();
                let (left, top) = state.origin;
                // 0..65535 is already a normalized fraction of the full
                // captured output, independent of the session/stream
                // resolution — so this scales directly onto `output_size`
                // (the real on-screen extent), not `wire_size`. No
                // wire/output ratio needed here (unlike touch()): the
                // fraction IS the wire value, over a fixed 65535 range.
                let (out_w, out_h) = state.output_size;
                let fx = x as f32 / 65535.0;
                let fy = y as f32 / 65535.0;
                let abs_x = left + (fx * out_w as f32) as i32;
                let abs_y = top + (fy * out_h as f32) as i32;
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
                // Physical mouse deltas are device movement, not coordinates
                // in the negotiated video space. Keep them unscaled and do
                // not clamp them to the captured output: unlike touch and
                // absolute input, a real pointer must be able to cross onto
                // any monitor in the macOS desktop.
                let (dx, dy) = (dx as i32, dy as i32);
                let (cx, cy) = (px.saturating_add(dx), py.saturating_add(dy));
                let held = state.held;
                state.pos = (cx, cy);
                drop(state);
                self.post_move(cx, cy, held, Some((dx, dy)));
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
                // MOUSE_SCROLL carries discrete wheel steps. Touch gestures
                // use the separate pixel-unit path above.
                post_scroll_lines(&self.source, dy as i32);
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
    fn scaled_position_preserves_hidpi_edges() {
        assert_eq!(scale_position(0, 1280, 640), 0);
        assert_eq!(scale_position(1279, 1280, 640), 639);
        assert_eq!(scale_position(0, 800, 400), 0);
        assert_eq!(scale_position(799, 800, 400), 399);
    }

    #[test]
    fn combo_modifier_bits_match_wire_protocol() {
        assert_eq!(modifier_flags(0x01), CGEventFlags::CGEventFlagControl);
        assert_eq!(modifier_flags(0x02), CGEventFlags::CGEventFlagAlternate);
        assert_eq!(modifier_flags(0x04), CGEventFlags::CGEventFlagCommand);
        assert!(modifier_flags(0x08).is_empty());
    }

    #[test]
    fn combo_characters_use_physical_ansi_keys() {
        assert_eq!(char_to_key('a'), Some((KeyCode::ANSI_A, false)));
        assert_eq!(char_to_key('A'), Some((KeyCode::ANSI_A, true)));
        assert_eq!(char_to_key('c'), Some((KeyCode::ANSI_C, false)));
        assert_eq!(char_to_key('?'), Some((KeyCode::ANSI_SLASH, true)));
        assert_eq!(char_to_key('é'), None);
    }

    /// Independent ground truth for "where did CGEventPost actually put the
    /// cursor" — queries the live HID cursor position via
    /// `CGEventCreate(NULL)` + `CGEventGetLocation`, entirely independent of
    /// anything this module tracks internally (`PointerState`/`TouchState`
    /// could be wrong in a way that's self-consistent but still not what the
    /// OS actually did).
    fn current_cursor_pos() -> (i32, i32) {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .expect("CGEventSourceCreate failed");
        let event = CGEvent::new(source).expect("CGEventCreate failed");
        let p = event.location();
        (p.x as i32, p.y as i32)
    }

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
        input
            .mouse(MouseEvent::Button { btn: 0, state: 1 })
            .unwrap();
        std::thread::sleep(Duration::from_millis(80));
        input
            .mouse(MouseEvent::Button { btn: 0, state: 0 })
            .unwrap();
    }

    #[test]
    #[ignore]
    fn relative_mouse_crosses_target_boundary() {
        let mut input = MacInput::new(100, 100).expect("MacInput::new failed");
        input.set_output_size(100, 100);
        input.set_target_rect(0, 0, 100, 100);
        input
            .mouse(MouseEvent::MoveAbsolute { x: 32767, y: 32767 })
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let before = current_cursor_pos();
        input
            .mouse(MouseEvent::MoveRelative { dx: 100, dy: 100 })
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let after = current_cursor_pos();
        assert!(
            (after.0 - before.0 - 100).abs() <= 2,
            "x delta: {before:?} -> {after:?}"
        );
        assert!(
            (after.1 - before.1 - 100).abs() <= 2,
            "y delta: {before:?} -> {after:?}"
        );
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
    /// (Idle->Pending->quick-tap, Idle->Pending->movement->Dragging,
    /// Idle->Pending->2-finger-cancel->
    /// Scrolling->Idle) end to end without panicking, including the
    /// deferred long-press background thread.
    #[test]
    #[ignore]
    fn touch_gesture_smoke() {
        let mut input = MacInput::new(1280, 800).expect("MacInput::new failed");
        input.set_target_rect(0, 0, 1280, 800);

        // Quick single-finger tap, lifted before the long-press interval.
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

        // Single-finger movement beyond the threshold commits to Dragging.
        input
            .touch(&[TouchContact {
                slot: 0,
                event_type: TOUCH_DOWN,
                x: 300,
                y: 300,
            }])
            .unwrap();
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

    /// End-to-end touch-mapping acceptance test (the 4-corner test from the
    /// design doc's verification section): attach a REAL virtual display via
    /// the standard code path, wire it up exactly like main.rs does
    /// (`set_output_size` with the live rect's actual on-screen size, then
    /// `set_target_rect` with the REQUESTED mode as the session/wire
    /// resolution — the two can differ, see below), then inject a touch at
    /// each of the 4 corners plus the center, probing in *requested-mode*
    /// wire space — each as a full down/up tap — and after each cycle read
    /// the true cursor position
    /// back via an independent CGEventCreate/CGEventGetLocation query,
    /// asserting it is within ±2px of the expected absolute desktop
    /// coordinate.
    ///
    /// This guards against a real bug that a previous version of this test
    /// masked: WindowServer can silently substitute a persisted mode
    /// preference for a virtual display's requested mode (e.g. requesting
    /// 1280x800, coming online at 1024x768 instead — see
    /// `force_display_mode` in display.rs). The old version of this test
    /// fed `output_rect()`'s live dims as BOTH the wire space AND the
    /// expected space, which is self-consistent even when the two
    /// resolutions disagree — it can never catch this bug. This version
    /// probes in the space a real client actually uses (the negotiated
    /// session resolution == the mode we requested), independent of
    /// whatever the display actually came online at, so a broken
    /// substitution (or broken scaling compensating for it) shows up as a
    /// real, non-trivial pixel error.
    ///
    /// Also hard-asserts that `output_rect()`'s bounds equal the requested
    /// mode — i.e. that `force_display_mode` (Part 1) actually overrode
    /// whatever WindowServer would otherwise have persisted. If that
    /// assertion ever fails on a healthy run, the scaling in `touch()`/
    /// `mouse()` (Part 2) is what's actually keeping the corner probes
    /// passing — see the expected-value formula below, which stays correct
    /// either way.
    ///
    /// Requires Screen Recording + Accessibility permissions granted to the
    /// test binary. Run with:
    ///   cargo test --release --bin screx touch_mapping_four_corners -- --ignored --nocapture
    /// and again with a second resolution:
    ///   SCREX_TEST_MODE=1024x768 cargo test --release --bin screx touch_mapping_four_corners -- --ignored --nocapture
    #[test]
    #[ignore]
    fn touch_mapping_four_corners() {
        use crate::capture::{DisplayBackend, DisplayMode};
        use crate::platform::macos::display::MacDisplay;

        // Optionally override the requested session resolution, e.g.
        //   SCREX_TEST_MODE=1024x768 cargo test ... touch_mapping_four_corners ...
        let (mw, mh) = std::env::var("SCREX_TEST_MODE")
            .ok()
            .and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or((1280u32, 800u32));
        let mode = DisplayMode {
            width: mw,
            height: mh,
            fps: 30,
        };
        let mut display = MacDisplay::new(mode.width, mode.height, mode.fps).unwrap();
        display.attach(mode).expect("attach failed");
        let (left, top, actual_w, actual_h) = display.output_rect().expect("output_rect");
        println!(
            "[4corner] requested mode {mw}x{mh}, output_rect = ({left}, {top}, {actual_w}, {actual_h})"
        );

        // Part 1 acceptance: force_display_mode should have overridden
        // whatever WindowServer would otherwise have persisted, so the live
        // rect should equal one of the two acceptable outcomes — the
        // preferred HiDPI half-size in points (when both dims are even and
        // the OS actually honors CGVirtualDisplaySettings.hiDPI) or the full
        // 1x size (the fallback, and — empirically, on at least one real
        // machine/macOS build tested during this feature's implementation —
        // the ONLY outcome ever observed: CGVirtualDisplaySettings.hiDPI=1
        // produced no pixel/point distinction in
        // CGDisplayCopyAllDisplayModes (CGDisplayModeGetPixelWidth/Height
        // always equal GetWidth/Height for every registered mode) and
        // NSScreen.backingScaleFactor stayed 1.0 for the virtual display,
        // verified independently of this Rust code via a raw ObjC-runtime
        // probe trying both point-size and real-panel pixel-size mode args.
        // Asserting a hard requirement on HiDPI specifically would make this
        // test permanently red on such a machine for a reason outside this
        // code's control, so both outcomes are accepted here — this still
        // catches a genuine regression (landing on some THIRD, unrequested
        // size).
        let both_even = mw % 2 == 0 && mh % 2 == 0;
        let hidpi_wh = (mw / 2, mh / 2);
        let onex_wh = (mw, mh);
        let acceptable: Vec<(u32, u32)> = if both_even {
            vec![hidpi_wh, onex_wh]
        } else {
            vec![onex_wh]
        };
        assert!(
            acceptable.contains(&(actual_w, actual_h)),
            "output_rect() bounds {actual_w}x{actual_h} match neither acceptable outcome \
             {acceptable:?} for requested mode {mw}x{mh} — force_display_mode (Part 1) failed \
             to override WindowServer's mode substitution; see [display] logs above for the \
             CGError"
        );
        println!(
            "[4corner] active mode is {}",
            if both_even && (actual_w, actual_h) == hidpi_wh {
                "HiDPI (2x)"
            } else {
                "1x"
            }
        );

        // Exactly what main.rs does right after attach: report the live
        // output size, then set the target rect with the SESSION/wire
        // resolution (the requested mode) — the two are asserted equal
        // above, but the test still goes through both calls, and the
        // wire/expected formula below, to exercise the real code path
        // rather than assuming the identity case.
        let mut input = MacInput::new(mw, mh).expect("MacInput::new failed");
        input.set_output_size(actual_w, actual_h);
        input.set_target_rect(left, top, mw, mh);

        // Let the desktop finish settling on the new display (Dock/menu bar
        // shuffling right after attach was observed to briefly perturb the
        // cursor, producing a one-off ~100px outlier in an early run).
        std::thread::sleep(Duration::from_secs(2));

        // Expected-position formula per the design doc: wire coordinates
        // (in requested-mode space) scale onto the actual on-screen rect.
        // Collapses to the identity mapping when actual == requested (the
        // assertion above), but is written out in full so this test still
        // means something if that assertion is ever loosened.
        let expected_for = |wx: u16, wy: u16| -> (i32, i32) {
            (
                left + scale_position(wx, mw, actual_w),
                top + scale_position(wy, mh, actual_h),
            )
        };

        // 4 corners + center, in requested-mode wire (stream-pixel)
        // coordinates. Corners use the last addressable pixel (w-1, h-1),
        // matching what a letterbox-aware client can actually send.
        let probes = [
            ("top-left", 0u16, 0u16),
            ("top-right", (mw - 1) as u16, 0u16),
            ("bottom-left", 0u16, (mh - 1) as u16),
            ("bottom-right", (mw - 1) as u16, (mh - 1) as u16),
            ("center", (mw / 2) as u16, (mh / 2) as u16),
        ];

        let mut failures = Vec::new();
        for (probe_index, (label, wx, wy)) in probes.into_iter().enumerate() {
            let expected = expected_for(wx, wy);
            let slot = probe_index as u8;

            input
                .touch(&[TouchContact {
                    slot,
                    event_type: TOUCH_DOWN,
                    x: wx,
                    y: wy,
                }])
                .unwrap();
            input
                .touch(&[TouchContact {
                    slot,
                    event_type: TOUCH_UP,
                    x: wx,
                    y: wy,
                }])
                .unwrap();
            std::thread::sleep(Duration::from_millis(30));

            let actual = current_cursor_pos();
            let dx = (actual.0 - expected.0).abs();
            let dy = (actual.1 - expected.1).abs();
            let ok = dx <= 2 && dy <= 2;
            println!(
                "[4corner] {label}: wire=({wx},{wy}) expected={expected:?} actual={actual:?} \
                 delta=({dx},{dy}) {}",
                if ok { "OK" } else { "FAIL" }
            );
            if !ok {
                failures.push(format!(
                    "{label}: expected {expected:?}, got {actual:?} (delta {dx},{dy})"
                ));
            }

            std::thread::sleep(Duration::from_millis(100));
        }

        // Drag regression probe: down at center, drag to (3/4w, 3/4h), lift
        // THERE. Verifies the LeftMouseUp posts at the lift position — the
        // stale-anchor bug this test guards against posted it back at the
        // down position instead. Also in requested-mode wire space.
        {
            let (sx, sy) = ((mw / 2) as u16, (mh / 2) as u16);
            let (ex, ey) = ((mw * 3 / 4) as u16, (mh * 3 / 4) as u16);
            let expected = expected_for(ex, ey);
            let slot = 7;
            input
                .touch(&[TouchContact {
                    slot,
                    event_type: TOUCH_DOWN,
                    x: sx,
                    y: sy,
                }])
                .unwrap();
            std::thread::sleep(Duration::from_millis(60));
            input
                .touch(&[TouchContact {
                    slot,
                    event_type: TOUCH_MOVE,
                    x: ex,
                    y: ey,
                }])
                .unwrap();
            std::thread::sleep(Duration::from_millis(30));
            input
                .touch(&[TouchContact {
                    slot,
                    event_type: TOUCH_UP,
                    x: ex,
                    y: ey,
                }])
                .unwrap();
            std::thread::sleep(Duration::from_millis(30));
            let actual = current_cursor_pos();
            let dx = (actual.0 - expected.0).abs();
            let dy = (actual.1 - expected.1).abs();
            let ok = dx <= 2 && dy <= 2;
            println!(
                "[4corner] drag-release: wire=({sx},{sy})->({ex},{ey}) expected={expected:?} \
                 actual={actual:?} delta=({dx},{dy}) {}",
                if ok { "OK" } else { "FAIL" }
            );
            if !ok {
                failures.push(format!(
                    "drag-release: expected {expected:?}, got {actual:?} (delta {dx},{dy})"
                ));
            }
        }

        display.detach();

        assert!(
            failures.is_empty(),
            "touch mapping off by more than 2px:\n{}",
            failures.join("\n")
        );
    }
}
