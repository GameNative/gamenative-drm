//! Per-module log gating for gndrm's Rust logging.
//!
//! Every line carries the tag/env that [`crate::log_policy`] selects from the
//! call site's `module_path!()`, so a log call names no module and a whole
//! subsystem (daemon/asc/input/…) can be switched on independently. A gated-off
//! call returns before formatting; cold paths only — the per-frame hot path
//! never logs.

use std::fmt;

/// Emit `text` at `prio` under the tag/env `module` classifies to. Returns
/// before formatting when the module's gate is off.
pub(crate) fn write(prio: i32, module: &'static str, args: fmt::Arguments<'_>) {
    let (tag, env) = crate::log_policy::classify(module);
    if !crate::log_policy::enabled(env) {
        return;
    }
    let text = fmt::format(args);
    // CString::new fails only on interior NUL, which format! output of our
    // messages never contains; fall back to a fixed string.
    let c = std::ffi::CString::new(text)
        .unwrap_or_else(|_| std::ffi::CString::new("<log error>").unwrap());
    // SAFETY: both pointers are NUL-terminated and outlive the call.
    unsafe {
        crate::ffi::__android_log_write(prio, tag.as_ptr(), c.as_ptr());
    }
}

/// Log a line the daemon produced (its logger callbacks land here), tagged with
/// the daemon's own module path so it classifies to the daemon gate rather than
/// this library's, and so stays off unless `GNDRM_LOG_DAEMON` is set.
pub(crate) fn daemon_info(text: &str) {
    write(
        crate::ffi::ANDROID_LOG_INFO,
        "gndrm_daemon",
        format_args!("{text}"),
    );
}

/// One log line at `prio`; `fmt`/`args` go through `format_args!`.
#[macro_export]
macro_rules! alog {
    ($prio:expr, $($arg:tt)*) => {{
        $crate::log::write($prio, module_path!(), format_args!($($arg)*))
    }};
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::alog!($crate::ffi::ANDROID_LOG_INFO, $($arg)*) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::alog!($crate::ffi::ANDROID_LOG_WARN, $($arg)*) };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::alog!($crate::ffi::ANDROID_LOG_ERROR, $($arg)*) };
}
