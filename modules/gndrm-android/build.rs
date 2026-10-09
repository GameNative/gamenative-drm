// build.rs — export allowlist for the JNI cdylib.
//
// The dynamic table of libgamenative_drm.so is pinned to exactly the JNI entry
// points the Java side resolves through System.loadLibrary("gamenative_drm")
// (tools/exports/libgamenative_drm.map). The version script is the allowlist;
// `--exclude-libs,ALL` also keeps statically-linked archive symbols out of the
// table. Applied to the cdylib only, so nothing else in the crate's build is
// affected. Audited by tools/check-exports.sh.
//
// On non-Android targets the crate is an empty stub (no JNI symbols exist), so
// the script is emitted only when linking the Android cdylib.

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    let map = format!(
        "{}/../../tools/exports/libgamenative_drm.map",
        env::var("CARGO_MANIFEST_DIR").unwrap()
    );
    println!("cargo:rustc-cdylib-link-arg=-Wl,--version-script={map}");
    println!("cargo:rustc-cdylib-link-arg=-Wl,--exclude-libs,ALL");
    println!("cargo:rerun-if-changed={map}");
}
