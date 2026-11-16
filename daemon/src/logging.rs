use std::sync::atomic::{AtomicU8, Ordering};

/// 0 = default (lifecycle, display changes, warnings, errors), 1 = `-v` (connection, device and
/// pipeline details, periodic stats), 2 = `-vv` (per-packet diagnostics).
static VERBOSITY: AtomicU8 = AtomicU8::new(0);

pub fn set_verbosity(level: u8) {
    VERBOSITY.store(level, Ordering::Relaxed);
}

pub fn verbosity() -> u8 {
    VERBOSITY.load(Ordering::Relaxed)
}

/// Details shown with `-v`.
#[macro_export]
macro_rules! log_detail {
    ($($arg:tt)*) => {
        if $crate::logging::verbosity() >= 1 {
            println!($($arg)*);
        }
    };
}

/// Per-packet diagnostics shown with `-vv`.
#[macro_export]
macro_rules! log_trace {
    ($($arg:tt)*) => {
        if $crate::logging::verbosity() >= 2 {
            println!($($arg)*);
        }
    };
}
