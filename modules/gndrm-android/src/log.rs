//! Per-module log gating for gndrm's Rust logging.
//!
//! Every line carries the tag/env that [`crate::log_policy`] selects from the
//! call site's `module_path!()`, so a log call names no module and a whole
//! subsystem (daemon/asc/input/…) can be switched on independently. A gated-off
//! call returns before formatting; cold paths only — the per-frame hot path
//! never logs.
//!
//! The logcat tag is decoded at runtime from an obfuscated literal
//! ([`gndrm_obf::obfc`]), so the shipped library's tag strings are not
//! recoverable with `strings`.

use std::fmt;

use crate::log_policy::Gate;

/// Emit `text` at `prio` under `gate`. Returns before formatting when the
/// gate is off.
pub(crate) fn write(prio: i32, gate: Gate, args: fmt::Arguments<'_>) {
    if !crate::log_policy::enabled(gate) {
        return;
    }
    emit(prio, gate, &fmt::format(args));
}

/// Emit `text` at `prio` with **no gate at all**: the line prints whenever the
/// process runs, whatever `GNDRM_LOG_*` says. This exists for one-shot
/// instrumentation on the app side, where a missing env var would otherwise
/// silence the whole diagnostic — the container's environment reaches the guest
/// (gamescope's process), not this app process, so a gated line here is
/// invisible on device. Never call it on a per-frame path: every call site
/// latches itself so it cannot format on a repeated path.
pub(crate) fn write_ungated(prio: i32, args: fmt::Arguments<'_>) {
    emit(prio, Gate::Daemon, &fmt::format(args));
}

/// Format `text` into a C string and hand it to logcat under the gate's tag.
fn emit(prio: i32, gate: Gate, text: &str) {
    // CString::new fails only on interior NUL, which format! output of our
    // messages never contains; fall back to a fixed string.
    let c = std::ffi::CString::new(text)
        .unwrap_or_else(|_| std::ffi::CString::new("<log error>").unwrap());
    macro_rules! tag {
        ($t:literal) => {{
            let t = gndrm_obf::obfc!($t);
            // SAFETY: `t` and `c` are NUL-terminated and outlive the call.
            unsafe {
                crate::ffi::__android_log_write(prio, t.as_cstr().as_ptr(), c.as_ptr());
            }
        }};
    }
    match gate {
        Gate::Daemon => tag!("gndrm-daemon"),
        Gate::Asc => tag!("gndrm-asc"),
        Gate::Input => tag!("gndrm-input"),
        Gate::Vkshim => tag!("gn-vkshim"),
        Gate::Shim => tag!("gndrm-shim"),
        Gate::Fallback => tag!("gndrm"),
    }
}

/// Log a line the daemon produced (its logger callbacks land here), tagged with
/// the daemon's own module path so it classifies to the daemon gate rather than
/// this library's, and so stays off unless `GNDRM_LOG_DAEMON` is set.
pub(crate) fn daemon_info(text: &str) {
    write(
        crate::ffi::ANDROID_LOG_INFO,
        Gate::Daemon,
        format_args!("{text}"),
    );
}

/// Log a line the daemon produced with **no gate at all** — the sink the
/// daemon's commit probe prints through (see `Device::set_always_logger`).
/// The probe is unsilenceable on purpose: its caller applies its own rate
/// limit, and the daemon runs in this app process, where the container's
/// `GNDRM_LOG_*` cannot reach — a gated probe could not be switched on for a
/// device pass at all.
pub(crate) fn daemon_always(text: &str) {
    write_ungated(crate::ffi::ANDROID_LOG_INFO, format_args!("{text}"));
}

/// One log line at `prio`; `fmt`/`args` go through `format_args!`. The
/// call site's module path is classified at compile time (`const`), so it is
/// never stored in the binary.
///
/// The gate is tested *before* `format_args!`: an `obf!` literal at the call
/// site is a stack decode, so a disabled line must not build the arguments at
/// all. `write` re-checks (it is also the entry for the daemon's forwarded
/// lines); this only short-circuits the decode.
#[macro_export]
macro_rules! alog {
    ($prio:expr, $($arg:tt)*) => {{
        const GATE: $crate::log_policy::Gate = $crate::log_policy::gate_of(module_path!());
        if $crate::log_policy::enabled(GATE) {
            $crate::log::write($prio, GATE, format_args!($($arg)*))
        }
    }};
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::alog!($crate::ffi::ANDROID_LOG_INFO, $($arg)*) };
}

/// Log one line with **NO gate** (no `GNDRM_LOG_*` can silence it). One-shot
/// instrumentation only: every call site must latch itself so it cannot format
/// on a repeated path. See [`crate::log::write_ungated`].
#[macro_export]
macro_rules! log_always {
    ($($arg:tt)*) => {
        $crate::log::write_ungated($crate::ffi::ANDROID_LOG_INFO, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::alog!($crate::ffi::ANDROID_LOG_WARN, $($arg)*) };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::alog!($crate::ffi::ANDROID_LOG_ERROR, $($arg)*) };
}
