//! Loads the real `liblsfg_engine.so` the way the ICD does.
//!
//! Build it first: `bash tools/build-lsfg.sh` (host). The test skips when the
//! library is absent, so a checkout without a C++ toolchain still runs the rest
//! of the suite.
//!
//! What this proves, without a GPU: the library's ABI tag matches, every
//! `vkr_lsfg_*` entry point resolves, the host table installs, and the engine
//! refuses a null device instead of crashing — which is the failure mode that
//! matters when frame generation is unavailable.

use std::path::PathBuf;
use std::sync::Mutex;

/// `GNDRM_LSFG_ENGINE` is process-wide and each test needs a different value,
/// so they take turns rather than race on it.
static SESSION: Mutex<()> = Mutex::new(());

fn engine_path() -> Option<PathBuf> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lsfg-host/liblsfg_engine.so");
    path.is_file().then_some(path)
}

#[test]
fn opens_the_engine_and_installs_the_host_table() {
    let _guard = SESSION.lock().unwrap();
    let Some(path) = engine_path() else {
        eprintln!("skipping: build it with `bash tools/build-lsfg.sh` first");
        return;
    };
    // The loader reads this once per process; one test, one value.
    std::env::set_var("GNDRM_LSFG_ENGINE", &path);

    let engine = adrenotools_gamescope::lsfg::open().expect("engine opens");
    assert!(engine.library().ends_with("liblsfg_engine.so"));

    // A null device is refused, not dereferenced.
    let lsfg = unsafe {
        (engine.api.create)(std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null())
    };
    assert!(lsfg.is_null());
}

#[test]
fn a_wrong_path_is_a_missing_engine_not_a_panic() {
    let _guard = SESSION.lock().unwrap();
    std::env::set_var("GNDRM_LSFG_ENGINE", "/nonexistent/liblsfg_engine.so");
    assert!(adrenotools_gamescope::lsfg::open().is_none());
}
