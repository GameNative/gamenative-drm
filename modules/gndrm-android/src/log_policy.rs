//! Per-module log gating for gndrm's Rust logging (see `log.rs`).
//!
//! A log call site identifies its subsystem purely by `module_path!()` —
//! [`gate_of`] maps that path to a [`Gate`] and [`enabled`] answers whether the
//! gate's env var turns the subsystem on. Every gate is off unless its env var
//! is present, non-empty and not the literal `"0"`.
//!
//! [`gate_of`] is a `const fn` evaluated at the call site, so the module path
//! is a compile-time input and never reaches `.rodata` — the shipped library
//! carries no module-name strings. The env var NAMES stay readable on purpose:
//! they are compared at runtime by `var_os`, so a decoded copy would not be
//! load-bearing.
//!
//! The env var is read once per process and cached in a per-subsystem atomic,
//! so a gated-off log call is one relaxed load plus a return, never a
//! `getenv`.
//!
//! This module carries no Android dependency on purpose: it is the piece of
//! `gndrm-android` that is unit-tested on the host (the rest of the crate is
//! `cfg(target_os = "android")`). `tests/log_policy.rs` compiles this same
//! file, so the table below has exactly one home.

use std::ffi::OsStr;
use std::sync::atomic::{AtomicU8, Ordering};

/// The subsystems a call site can belong to. One env var gates each.
///
/// The `Vkshim`/`Shim` gates are enforced in their own crates (the ICD and the
/// C interposer); their variants are kept here as the single home of the
/// env-var table so a name cannot drift.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gate {
    /// Daemon crate (`gndrm_daemon::*`): the KMS emulator's own diagnostics.
    Daemon,
    /// In-app library modules (crate root / `asc` / `ffi`): server wiring and
    /// the native SurfaceControl presenter.
    Asc,
    /// The wayland input injector (`gamenative_drm::injector`).
    Input,
    /// The Vulkan ICD shim (a separate crate/process; gated there).
    Vkshim,
    /// The LD_PRELOAD ioctl shim (C; gated in `gndrm_shim.c`).
    Shim,
    /// Any module path the table does not know: still gated, by `GNDRM_LOG`.
    Fallback,
}

impl Gate {
    /// The env var that turns this subsystem on.
    #[must_use]
    pub const fn env(self) -> &'static str {
        match self {
            Gate::Daemon => "GNDRM_LOG_DAEMON",
            Gate::Asc => "GNDRM_LOG_ASC",
            Gate::Input => "GNDRM_LOG_INPUT",
            Gate::Vkshim => "GNDRM_LOG_VKSHIM",
            Gate::Shim => "GNDRM_LOG_SHIM",
            Gate::Fallback => "GNDRM_LOG",
        }
    }
}

/// Const string equality (`str` methods are not const).
const fn str_eq(a: &str, b: &str) -> bool {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    if x.len() != y.len() {
        return false;
    }
    let mut i = 0;
    while i < x.len() {
        if x[i] != y[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Const prefix test.
const fn str_prefix(a: &str, b: &str) -> bool {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    if x.len() < y.len() {
        return false;
    }
    let mut i = 0;
    while i < y.len() {
        if x[i] != y[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Map a call site's `module_path!()` to its [`Gate`]. The fallback arm exists
/// so a module added later is never silently ungated. `const` so the caller's
/// module path is consumed at compile time.
#[must_use]
pub const fn gate_of(module: &str) -> Gate {
    if str_prefix(module, "gndrm_daemon") {
        Gate::Daemon
    } else if str_eq(module, "gamenative_drm::injector") {
        Gate::Input
    } else if str_eq(module, "gamenative_drm")
        || str_eq(module, "gamenative_drm::asc")
        || str_eq(module, "gamenative_drm::ffi")
    {
        Gate::Asc
    } else {
        Gate::Fallback
    }
}

const UNRESOLVED: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;
const N_GATES: usize = 6;

/// Cached verdicts, one atomic per gate (indexed by the gate's discriminant).
static GATES: [AtomicU8; N_GATES] = [const { AtomicU8::new(UNRESOLVED) }; N_GATES];

/// Whether `gate`'s subsystem is enabled. Resolves the env var on first call
/// and serves the cached verdict afterwards.
#[must_use]
pub fn enabled(gate: Gate) -> bool {
    let slot = &GATES[gate as usize];
    match slot.load(Ordering::Relaxed) {
        ON => true,
        OFF => false,
        _ => {
            let on = gate_on(std::env::var_os(gate.env()).as_deref());
            slot.store(if on { ON } else { OFF }, Ordering::Relaxed);
            on
        }
    }
}

/// A gate is on only for a present, non-empty value that is not `"0"`.
fn gate_on(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != OsStr::new("0"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_of_maps_each_subsystem() {
        assert_eq!(gate_of("gndrm_daemon"), Gate::Daemon);
        assert_eq!(gate_of("gndrm_daemon::commit"), Gate::Daemon);
        assert_eq!(gate_of("gamenative_drm"), Gate::Asc);
        assert_eq!(gate_of("gamenative_drm::asc"), Gate::Asc);
        assert_eq!(gate_of("gamenative_drm::ffi"), Gate::Asc);
        assert_eq!(gate_of("gamenative_drm::injector"), Gate::Input);
        assert_eq!(gate_of("some::future::module"), Gate::Fallback);
    }

    #[test]
    fn every_gate_has_its_env_var() {
        assert_eq!(Gate::Daemon.env(), "GNDRM_LOG_DAEMON");
        assert_eq!(Gate::Asc.env(), "GNDRM_LOG_ASC");
        assert_eq!(Gate::Input.env(), "GNDRM_LOG_INPUT");
        assert_eq!(Gate::Vkshim.env(), "GNDRM_LOG_VKSHIM");
        assert_eq!(Gate::Shim.env(), "GNDRM_LOG_SHIM");
        assert_eq!(Gate::Fallback.env(), "GNDRM_LOG");
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
        let all = [
            Gate::Daemon,
            Gate::Asc,
            Gate::Input,
            Gate::Vkshim,
            Gate::Shim,
            Gate::Fallback,
        ];
        for g in all {
            assert!(!enabled(g), "{g:?} must be off");
        }
    }
}
