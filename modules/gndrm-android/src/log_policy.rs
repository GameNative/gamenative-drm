//! Per-module log gating for gndrm's Rust logging (see `log.rs`).
//!
//! A log call site identifies its subsystem purely by `module_path!()` —
//! [`classify`] turns that path into a (logcat tag, env var) pair and
//! [`enabled`] answers whether that env var turns the subsystem on. Every gate
//! is off unless its env var is present, non-empty and not the literal `"0"`.
//!
//! The env var is read once per process and cached in a per-subsystem atomic,
//! so a gated-off log call is one relaxed load plus a return, never a
//! `getenv`.
//!
//! This module carries no Android dependency on purpose: it is the piece of
//! `gndrm-android` that is unit-tested on the host (the rest of the crate is
//! `cfg(target_os = "android")`). `tests/log_policy.rs` compiles this same
//! file, so the table below has exactly one home.

use std::ffi::{CStr, OsStr};
use std::sync::atomic::{AtomicU8, Ordering};

/// Daemon crate (`gndrm_daemon::*`): the KMS emulator's own diagnostics.
pub const DAEMON: (&CStr, &str) = (c"gndrm-daemon", "GNDRM_LOG_DAEMON");
/// In-app library modules (crate root / `asc` / `ffi`): server wiring and the
/// native SurfaceControl presenter.
pub const ASC: (&CStr, &str) = (c"gndrm-asc", "GNDRM_LOG_ASC");
/// The wayland input injector (`gamenative_drm::injector`).
pub const INPUT: (&CStr, &str) = (c"gndrm-input", "GNDRM_LOG_INPUT");
/// The Vulkan ICD shim (a separate crate/process; gated there).
pub const VKSHIM: (&CStr, &str) = (c"gn-vkshim", "GNDRM_LOG_VKSHIM");
/// The LD_PRELOAD ioctl shim (C; gated in `gndrm_shim.c`).
pub const SHIM: (&CStr, &str) = (c"gndrm-shim", "GNDRM_LOG_SHIM");
/// Any module path the table does not know: still gated, by `GNDRM_LOG`.
pub const FALLBACK: (&CStr, &str) = (c"gndrm", "GNDRM_LOG");

/// Map a call site's `module_path!()` to its (tag, env var). The fallback arm
/// exists so a module added later is never silently ungated.
#[must_use]
pub fn classify(module: &str) -> (&'static CStr, &'static str) {
    if module.starts_with("gndrm_daemon") {
        DAEMON
    } else if module == "gamenative_drm::injector" {
        INPUT
    } else if module == "gamenative_drm"
        || module == "gamenative_drm::asc"
        || module == "gamenative_drm::ffi"
    {
        ASC
    } else {
        FALLBACK
    }
}

const UNRESOLVED: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;

/// Cached verdicts, one atomic per env var.
static GATES: [(&str, AtomicU8); 6] = [
    (DAEMON.1, AtomicU8::new(UNRESOLVED)),
    (ASC.1, AtomicU8::new(UNRESOLVED)),
    (INPUT.1, AtomicU8::new(UNRESOLVED)),
    (VKSHIM.1, AtomicU8::new(UNRESOLVED)),
    (SHIM.1, AtomicU8::new(UNRESOLVED)),
    (FALLBACK.1, AtomicU8::new(UNRESOLVED)),
];

/// Whether `env`'s subsystem is enabled. Resolves the env var on first call
/// and serves the cached verdict afterwards.
#[must_use]
pub fn enabled(env: &str) -> bool {
    for (name, gate) in &GATES {
        if *name == env {
            return match gate.load(Ordering::Relaxed) {
                ON => true,
                OFF => false,
                _ => {
                    let on = gate_on(std::env::var_os(*name).as_deref());
                    gate.store(if on { ON } else { OFF }, Ordering::Relaxed);
                    on
                }
            };
        }
    }
    false
}

/// A gate is on only for a present, non-empty value that is not `"0"`.
fn gate_on(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != OsStr::new("0"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_each_subsystem() {
        assert_eq!(classify("gndrm_daemon"), DAEMON);
        assert_eq!(classify("gndrm_daemon::commit"), DAEMON);
        assert_eq!(classify("gamenative_drm"), ASC);
        assert_eq!(classify("gamenative_drm::asc"), ASC);
        assert_eq!(classify("gamenative_drm::ffi"), ASC);
        assert_eq!(classify("gamenative_drm::injector"), INPUT);
        assert_eq!(classify("some::future::module"), FALLBACK);
    }

    #[test]
    fn gate_is_off_unless_set_to_a_nonzero_value() {
        assert!(!gate_on(None));
        assert!(!gate_on(Some(OsStr::new(""))));
        assert!(!gate_on(Some(OsStr::new("0"))));
        assert!(gate_on(Some(OsStr::new("1"))));
    }

    #[test]
    fn gates_are_off_in_a_fresh_process() {
        // Nothing here sets these variables; every gate must resolve to off.
        assert!(!enabled(DAEMON.1));
        assert!(!enabled(ASC.1));
        assert!(!enabled(INPUT.1));
        assert!(!enabled(FALLBACK.1));
    }
}
