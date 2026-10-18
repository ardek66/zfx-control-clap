//! Opt-in diagnostic logging for the 32-bit worker.
//!
//! Enable with `ZFX_WORKER_LOG=1` (or any value). Disabled by default so load
//! and process paths stay quiet under DAW hosts.

use std::sync::OnceLock;

#[must_use]
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ZFX_WORKER_LOG").is_some())
}

#[macro_export]
macro_rules! worker_log {
    ($($arg:tt)*) => {{
        if $crate::log::enabled() {
            eprintln!($($arg)*);
        }
    }};
}
