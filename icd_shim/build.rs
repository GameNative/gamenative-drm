// build.rs — link wiring for the Android payload layout.
//
// The shim's NEEDEDs are libc/libdl/libwayland-client; libwayland-client.so
// comes from the gamescope termux prefix at build time and resolves via
// LD_LIBRARY_PATH on device (same as the C build in package-gamescope.sh).
// Host builds use the system wayland via the default search paths.

use std::env;

fn main() {
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
