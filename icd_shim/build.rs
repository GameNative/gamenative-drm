// build.rs — link wiring for the Android payload layout.
//
// The shim's NEEDEDs are libc/libdl/libwayland-client plus the X11 present set
// (libxcb, libxcb-dri3, libxcb-present, libxcb-sync); all of them come from the
// gamescope termux prefix at build time and resolve via LD_LIBRARY_PATH on
// device (same as the C build in package-gamescope.sh). Host builds use the
// system libraries via the default search paths.

use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

/// UTC `YYYYMMDDhhmm` from a Unix timestamp, no external crate.
fn utc_stamp(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hh, mm) = (rem / 3600, (rem % 3600) / 60);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}{hh:02}{mm:02}")
}

fn main() {
    // Build stamp: derived at compile time so a rebuilt shim can never print a
    // stale tag. The tag used to be a hardcoded constant that kept reporting one
    // date for every later build, which made "stale payload vs stale binary"
    // undecidable from the log. Recompute whenever the crate's sources change.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=GN_SHIM_BUILD={}", utc_stamp(secs));
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");

    // Export allowlist: the ICD's dynamic table is exactly the three Khronos
    // loader entry points (tools/exports/libadrenotools_gamescope.map). The
    // version script pins it; `--exclude-libs,ALL` keeps statically-linked
    // archive symbols (compiler-rt, the Rust std objects) out of the table too.
    // Applied to the cdylib only, so the host integration tests and icd_probe
    // are unaffected; the rlib half keeps its symbols.
    let map = format!(
        "{}/../tools/exports/libadrenotools_gamescope.map",
        env::var("CARGO_MANIFEST_DIR").unwrap()
    );
    println!("cargo:rustc-cdylib-link-arg=-Wl,--version-script={map}");
    println!("cargo:rustc-cdylib-link-arg=-Wl,--exclude-libs,ALL");
    println!("cargo:rerun-if-changed={map}");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    let prefix = env::var("GAMESCOPE_TERMUX_PREFIX").unwrap_or_else(|_| {
        let home = env::var("HOME").unwrap_or_default();
        format!("{home}/termuxfs/aarch64/data/data/com.termux/files/usr")
    });
    println!("cargo:rustc-link-search=native={prefix}/lib");
    println!("cargo:rerun-if-env-changed=GAMESCOPE_TERMUX_PREFIX");
}
