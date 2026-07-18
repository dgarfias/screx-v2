//! macOS menu bar (status bar) indicator for the daemon.
//!
//! Shows connection status ("Waiting for client..." / "Client connected.")
//! and live send/receive throughput, with a Quit action. `run()` is the
//! daemon's terminal blocking call on macOS: it pumps the AppKit main run
//! loop on the calling thread (which must be the real process main thread),
//! replacing the plain `tokio::signal::ctrl_c()` wait used on other
//! platforms — AppKit requires an actual run loop to service the status
//! item and its menu.
//!
//! The status item is built from `applicationDidFinishLaunching:` rather
//! than before `app.run()` — AppKit only finishes wiring up the process's
//! WindowServer connection once `-finishLaunching` runs (which `-run` calls
//! as its first step), and an `NSStatusItem` created earlier never actually
//! registers: it exists as a Rust-side object but is invisible and exposes
//! no accessibility UI elements. Confirmed empirically (pre-launch
//! construction produced a status item that never appeared on screen and
//! that `System Events` reported zero menu bars for), not just per the
//! crate's own doc note to do UI init in `applicationDidFinishLaunching:`.

use std::cell::{Cell, OnceCell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
    Message,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSImage, NSMenu,
    NSMenuItem, NSStatusBar, NSVariableStatusItemLength,
};
use objc2_foundation::{ns_string, NSData, NSNotification, NSSize, NSString, NSTimer};

use crate::stream_server::SharedState;

/// Menu bar icon — a template (alpha-only) image; macOS derives the
/// on-screen tint from `isTemplate`, so this renders correctly in both
/// light and dark menu bars and while highlighted.
static MENUBAR_ICON_PNG: &[u8] = include_bytes!("assets/menubar-icon.png");

/// Set from a SIGINT/SIGTERM handler and polled once a second from the
/// status timer on the main thread, rather than acted on directly inside
/// the signal handler — AppKit calls and Rust cleanup are not
/// async-signal-safe.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_quit_signal(_sig: i32) {
    QUIT_REQUESTED.store(true, Ordering::SeqCst);
}

fn format_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1_000_000.0 {
        format!("{:.1} MB/s", bytes_per_sec / 1_000_000.0)
    } else if bytes_per_sec >= 1_000.0 {
        format!("{:.1} KB/s", bytes_per_sec / 1_000.0)
    } else {
        format!("{bytes_per_sec:.0} B/s")
    }
}

struct AppDelegateIvars {
    shared: Arc<SharedState>,
    shutdown: RefCell<Option<Box<dyn FnOnce()>>>,
    status_row: OnceCell<Retained<NSMenuItem>>,
    stats_row: OnceCell<Retained<NSMenuItem>>,
    last_tx: Cell<u64>,
    last_rx: Cell<u64>,
    last_tick: Cell<Instant>,
    // Keeps the repeating timer (and the block it owns) alive for the
    // process lifetime; never read again after being set.
    timer: OnceCell<Retained<NSTimer>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; AppDelegate does
    // not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppDelegateIvars]
    #[name = "ScrexStatusBarDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.build_status_item();
        }
    }

    impl AppDelegate {
        #[unsafe(method(quitClicked:))]
        fn quit_clicked(&self, _sender: Option<&AnyObject>) {
            QUIT_REQUESTED.store(true, Ordering::SeqCst);
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker, shared: Arc<SharedState>, shutdown: Box<dyn FnOnce()>) -> Retained<Self> {
        let ivars = AppDelegateIvars {
            shared,
            shutdown: RefCell::new(Some(shutdown)),
            status_row: OnceCell::new(),
            stats_row: OnceCell::new(),
            last_tx: Cell::new(0),
            last_rx: Cell::new(0),
            last_tick: Cell::new(Instant::now()),
            timer: OnceCell::new(),
        };
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: `NSObject`'s `init` method signature is correct.
        unsafe { msg_send![super(this), init] }
    }

    fn build_status_item(&self) {
        let mtm = self.mtm();

        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);

        if let Some(button) = status_item.button(mtm) {
            let data = NSData::with_bytes(MENUBAR_ICON_PNG);
            if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
                image.setSize(NSSize::new(18.0, 18.0));
                image.setTemplate(true);
                button.setImage(Some(&image));
            }
        }

        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);

        let title_row = NSMenuItem::new(mtm);
        title_row.setTitle(ns_string!("Screx Daemon"));
        title_row.setEnabled(false);
        menu.addItem(&title_row);

        let status_row = NSMenuItem::new(mtm);
        status_row.setTitle(ns_string!("Waiting for client..."));
        status_row.setEnabled(false);
        menu.addItem(&status_row);

        let stats_row = NSMenuItem::new(mtm);
        stats_row.setTitle(ns_string!("\u{2193} 0 B/s   \u{2191} 0 B/s"));
        stats_row.setEnabled(false);
        menu.addItem(&stats_row);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit_row = NSMenuItem::new(mtm);
        quit_row.setTitle(ns_string!("Quit Screx Daemon"));
        quit_row.setKeyEquivalent(ns_string!("q"));
        // SAFETY: `self` is a valid `AppDelegate` implementing
        // `quitClicked:`, retained by the menu item as its target.
        unsafe { quit_row.setTarget(Some(self)) };
        // SAFETY: `sel!(quitClicked:)` matches the selector above.
        unsafe { quit_row.setAction(Some(sel!(quitClicked:))) };
        menu.addItem(&quit_row);

        status_item.setMenu(Some(&menu));

        // Leaked on purpose: the status item is never removed and must
        // live for the remaining life of the process.
        let _ = Retained::into_raw(status_item);
        let _ = Retained::into_raw(menu);
        let _ = self.ivars().status_row.set(status_row);
        let _ = self.ivars().stats_row.set(stats_row);

        let this = self.retain();
        let block = RcBlock::new(move |_timer: std::ptr::NonNull<NSTimer>| {
            this.tick();
        });
        // SAFETY: `block` is `Fn`, only ever invoked on this thread's run
        // loop (the timer is scheduled on the current run loop below).
        let timer =
            unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0, true, &block) };
        let _ = self.ivars().timer.set(timer);
    }

    fn tick(&self) {
        if QUIT_REQUESTED.load(Ordering::SeqCst) {
            self.perform_shutdown();
            return;
        }

        let ivars = self.ivars();
        let connected = ivars.shared.has_active_client.load(Ordering::Relaxed);
        if let Some(row) = ivars.status_row.get() {
            row.setTitle(if connected {
                ns_string!("Client connected.")
            } else {
                ns_string!("Waiting for client...")
            });
        }

        let now = Instant::now();
        let elapsed = now.duration_since(ivars.last_tick.get()).as_secs_f64().max(0.001);
        let tx = ivars.shared.bytes_tx.load(Ordering::Relaxed);
        let rx = ivars.shared.bytes_rx.load(Ordering::Relaxed);
        let tx_rate = tx.saturating_sub(ivars.last_tx.get()) as f64 / elapsed;
        let rx_rate = rx.saturating_sub(ivars.last_rx.get()) as f64 / elapsed;
        ivars.last_tx.set(tx);
        ivars.last_rx.set(rx);
        ivars.last_tick.set(now);

        if let Some(row) = ivars.stats_row.get() {
            row.setTitle(&NSString::from_str(&format!(
                "\u{2193} {}   \u{2191} {}",
                format_rate(rx_rate),
                format_rate(tx_rate)
            )));
        }
    }

    fn perform_shutdown(&self) {
        if let Some(f) = self.ivars().shutdown.borrow_mut().take() {
            f();
        }
        std::process::exit(0);
    }
}

/// Runs the menu bar UI on the calling thread (must be the process main
/// thread) until Quit is chosen from the menu or SIGINT/SIGTERM is
/// received, then runs `shutdown` exactly once and exits the process.
/// Never returns.
pub fn run(shared: Arc<SharedState>, shutdown: impl FnOnce() + 'static) -> ! {
    let mtm =
        MainThreadMarker::new().expect("statusbar::run must be called from the main thread");

    unsafe {
        libc::signal(libc::SIGINT, handle_quit_signal as *const () as usize);
        libc::signal(libc::SIGTERM, handle_quit_signal as *const () as usize);
    }

    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let delegate = AppDelegate::new(mtm, shared, Box::new(shutdown));
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    app.run();
    unreachable!("NSApplication::run returned unexpectedly");
}
