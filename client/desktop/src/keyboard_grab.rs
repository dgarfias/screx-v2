//! Cross-platform keyboard grab.
//!
//! When active, every physical key press/release is captured at the OS level
//! (before the toolkit/Qt sees it) and forwarded to a callback as a USB HID
//! usage code + pressed state.  This mirrors what the iPad does with
//! GameController's `keyChangedHandler` and what VirtualBox/Parallels/SPICE
//! do for VM keyboard passthrough.
//!
//! Platform implementations:
//!   Windows:  `WH_KEYBOARD_LL` low-level hook (dedicated thread + message pump)
//!   Linux:    (TODO)
//!   macOS:    (TODO)
//!
//! The grab is released by pressing Ctrl+Alt+G, which calls the release
//! callback so the UI can toggle `keyboard_enabled` off.

use std::sync::Arc;

/// Called for every raw key event while the grab is active.
/// `hid_usage` is a USB HID usage code (e.g. 0x04 = 'a', 0xE0 = LeftCtrl).
/// `pressed` is true for key-down, false for key-up.
pub type KeyCallback = Arc<dyn Fn(u16, bool) + Send + Sync>;

/// Called when the user presses the release shortcut (Ctrl+Alt+G).
pub type ReleaseCallback = Arc<dyn Fn() + Send + Sync>;

/// Platform-specific keyboard grab handle.  Dropping it uninstalls the grab.
pub trait KeyboardGrab: Send {
    fn is_active(&self) -> bool;
}

/// Install a keyboard grab on the current platform.
/// Returns `None` if the platform doesn't support it yet.
pub fn install_grab(on_key: KeyCallback, on_release: ReleaseCallback) -> Option<Box<dyn KeyboardGrab>> {
    #[cfg(target_os = "windows")]
    {
        return Some(Box::new(crate::keyboard_grab::windows::WindowsKeyboardGrab::install(on_key, on_release)?));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (on_key, on_release);
        None
    }
}

// ---------------------------------------------------------------------------
// Windows: WH_KEYBOARD_LL hook via raw FFI (avoids windows-rs version issues)
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows {
    use super::{KeyCallback, KeyboardGrab, ReleaseCallback};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    // Raw Win32 types and constants
    type HHOOK = isize;
    type WPARAM = usize;
    type LPARAM = isize;
    type LRESULT = isize;

    const WH_KEYBOARD_LL: i32 = 13;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_KEYUP: u32 = 0x0101;
    const WM_SYSKEYDOWN: u32 = 0x0104;
    const WM_SYSKEYUP: u32 = 0x0105;
    const WM_QUIT: u32 = 0x0012;

    #[repr(C)]
    struct KBDLLHOOKSTRUCT {
        vk_code: u32,
        scan_code: u32,
        flags: u32,
        time: u32,
        dw_extra_info: usize,
    }

    type HookProc = unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT;

    #[link(name = "user32")]
    extern "system" {
        fn SetWindowsHookExW(id_hook: i32, lpfn: HookProc, hmod: isize, dw_thread_id: u32) -> HHOOK;
        fn UnhookWindowsHookEx(hhk: HHOOK) -> i32;
        fn CallNextHookEx(hhk: HHOOK, n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT;
        fn GetMessageW(lp_msg: *mut MSG, hwnd: isize, w_msg_filter_min: u32, w_msg_filter_max: u32) -> i32;
        fn TranslateMessage(lp_msg: *const MSG) -> i32;
        fn DispatchMessageW(lp_msg: *const MSG) -> isize;
        fn PostThreadMessageW(thread_id: u32, msg: u32, w_param: WPARAM, l_param: LPARAM) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn GetModuleHandleW(lp_module_name: *const u16) -> isize;
    }

    #[repr(C)]
    struct MSG {
        hwnd: isize,
        message: u32,
        w_param: WPARAM,
        l_param: LPARAM,
        time: u32,
        pt_x: i32,
        pt_y: i32,
    }

    struct GrabState {
        on_key: KeyCallback,
        on_release: ReleaseCallback,
        active: AtomicBool,
        ctrl_down: AtomicBool,
        alt_down: AtomicBool,
    }

    pub struct WindowsKeyboardGrab {
        thread_handle: Option<thread::JoinHandle<()>>,
        state: Arc<GrabState>,
    }

    impl WindowsKeyboardGrab {
        pub fn install(on_key: KeyCallback, on_release: ReleaseCallback) -> Option<Self> {
            let state = Arc::new(GrabState {
                on_key,
                on_release,
                active: AtomicBool::new(true),
                ctrl_down: AtomicBool::new(false),
                alt_down: AtomicBool::new(false),
            });

            let thread_state = Arc::clone(&state);
            let handle = thread::Builder::new()
                .name("keyboard-grab".into())
                .spawn(move || {
                    // Get module handle for the current process
                    let hmod = unsafe { GetModuleHandleW(std::ptr::null()) };
                    if hmod == 0 {
                        eprintln!("[keyboard-grab] GetModuleHandleW failed");
                        thread_state.active.store(false, Ordering::SeqCst);
                        return;
                    }

                    let hook = unsafe {
                        SetWindowsHookExW(WH_KEYBOARD_LL, hook_proc, hmod, 0)
                    };
                    if hook == 0 {
                        eprintln!("[keyboard-grab] SetWindowsHookExW failed: {}", std::io::Error::last_os_error());
                        thread_state.active.store(false, Ordering::SeqCst);
                        return;
                    }

                    // Store state pointer in thread-local
                    STATE.with(|s| {
                        *s.borrow_mut() = Some(Arc::clone(&thread_state));
                    });

                    // Message pump — must run on the same thread that installed the hook
                    let mut msg = MSG {
                        hwnd: 0,
                        message: 0,
                        w_param: 0,
                        l_param: 0,
                        time: 0,
                        pt_x: 0,
                        pt_y: 0,
                    };

                    while thread_state.active.load(Ordering::Relaxed) {
                        let ret = unsafe { GetMessageW(&mut msg, 0, 0, 0) };
                        if ret <= 0 {
                            // WM_QUIT or error
                            break;
                        }
                        unsafe {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }

                    unsafe { UnhookWindowsHookEx(hook) };
                    thread_state.active.store(false, Ordering::SeqCst);
                })
                .ok()?;

            // Wait briefly for the hook to install
            for _ in 0..100 {
                if state.active.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(1));
            }

            Some(Self {
                thread_handle: Some(handle),
                state,
            })
        }
    }

    impl Drop for WindowsKeyboardGrab {
        fn drop(&mut self) {
            self.state.active.store(false, Ordering::SeqCst);
            if let Some(handle) = self.thread_handle.take() {
                let _ = handle.join();
            }
        }
    }

    impl KeyboardGrab for WindowsKeyboardGrab {
        fn is_active(&self) -> bool {
            self.state.active.load(Ordering::Relaxed)
        }
    }

    thread_local! {
        static STATE: std::cell::RefCell<Option<Arc<GrabState>>> = std::cell::RefCell::new(None);
    }

    /// VK → HID usage code mapping (inverse of the daemon's hid_usage_to_vk)
    fn vk_to_hid(vk: u32) -> Option<u16> {
        let vk = vk as u16;
        Some(match vk {
            0x41..=0x5A => 0x04 + (vk - 0x41), // A-Z
            0x31..=0x39 => 0x1E + (vk - 0x31), // 1-9
            0x30 => 0x27,                     // 0
            0x0D => 0x28,                     // Return
            0x1B => 0x29,                     // Escape
            0x08 => 0x2A,                     // Backspace
            0x09 => 0x2B,                     // Tab
            0x20 => 0x2C,                     // Space
            0xBD => 0x2D,                     // Minus
            0xBB => 0x2E,                     // Equal
            0xDB => 0x2F,                     // LeftBrace
            0xDD => 0x30,                     // RightBrace
            0xDC => 0x31,                     // Backslash
            0xBA => 0x33,                     // Semicolon
            0xDE => 0x34,                     // Quote
            0xC0 => 0x35,                     // Grave
            0xBC => 0x36,                     // Comma
            0xBE => 0x37,                     // Dot
            0xBF => 0x38,                     // Slash
            0x14 => 0x39,                     // CapsLock
            0x70..=0x7B => 0x3A + (vk - 0x70), // F1-F12
            0x2C => 0x46,                     // PrintScreen
            0x91 => 0x47,                     // ScrollLock
            0x13 => 0x48,                     // Pause
            0x2D => 0x49,                     // Insert
            0x24 => 0x4A,                     // Home
            0x21 => 0x4B,                     // PageUp
            0x2E => 0x4C,                     // Delete
            0x23 => 0x4D,                     // End
            0x22 => 0x4E,                     // PageDown
            0x27 => 0x4F,                     // Right
            0x25 => 0x50,                     // Left
            0x28 => 0x51,                     // Down
            0x26 => 0x52,                     // Up
            0xA2 => 0xE0,                     // Left Control
            0xA0 => 0xE1,                     // Left Shift
            0xA4 => 0xE2,                     // Left Alt
            0x5B => 0xE3,                     // Left Win
            0xA3 => 0xE4,                     // Right Control
            0xA1 => 0xE5,                     // Right Shift
            0xA5 => 0xE6,                     // Right Alt
            0x5C => 0xE7,                     // Right Win
            0x6F => 0x54,                     // Keypad /
            0x6A => 0x55,                     // Keypad *
            0x6D => 0x56,                     // Keypad -
            0x6B => 0x57,                     // Keypad +
            0x61..=0x69 => 0x59 + (vk - 0x61), // Keypad 1-9
            0x60 => 0x62,                     // Keypad 0
            0x6E => 0x63,                     // Keypad .
            _ => return None,
        })
    }

    unsafe extern "system" fn hook_proc(
        n_code: i32,
        w_param: WPARAM,
        l_param: LPARAM,
    ) -> LRESULT {
        if n_code != 0 {
            return CallNextHookEx(0, n_code, w_param, l_param);
        }

        let pressed = match w_param as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => true,
            WM_KEYUP | WM_SYSKEYUP => false,
            _ => return CallNextHookEx(0, n_code, w_param, l_param),
        };

        let state = STATE.with(|s| s.borrow().clone());
        let Some(state) = state else {
            return CallNextHookEx(0, n_code, w_param, l_param);
        };

        if !state.active.load(Ordering::Relaxed) {
            return CallNextHookEx(0, n_code, w_param, l_param);
        }

        let kb = &*(l_param as *const KBDLLHOOKSTRUCT);
        let vk = kb.vk_code;

        // Track modifier state for the release shortcut (Ctrl+Alt+G)
        match vk {
            0xA2 | 0xA3 => state.ctrl_down.store(pressed, Ordering::Relaxed),
            0xA4 | 0xA5 => state.alt_down.store(pressed, Ordering::Relaxed),
            _ => {}
        }

        // Release shortcut: Ctrl+Alt+G
        if vk == 0x47 // 'G'
            && pressed
            && state.ctrl_down.load(Ordering::Relaxed)
            && state.alt_down.load(Ordering::Relaxed)
        {
            state.active.store(false, Ordering::SeqCst);
            (state.on_release)();
            return 1; // eat the key
        }

        // Convert VK to HID usage and forward
        if let Some(hid) = vk_to_hid(vk) {
            (state.on_key)(hid, pressed);
        }

        // Eat the key (prevent local processing)
        1
    }
}
