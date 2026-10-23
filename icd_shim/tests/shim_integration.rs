//! Host integration tests for the adrenotools ICD shim, driven through the
//! `icd_probe` child binary (the loader-side harness). Each scenario spawns
//! a fresh process because the knobs under test are process-global:
//! LD_LIBRARY_PATH (read once at process start), /proc/self/comm (the
//! gamescope detection), and the shim's own init-once state.
//!
//! Fixture .so's (all land in the same target dir as the probe and the
//! shim's cdylib):
//!   libmock_vkdriver.so    — fake driver; ext lists via MOCK_*_EXTS env
//!   libvulkan_freedreno.so — same code under the payload-fallback SONAME
//!   libadrenotools.so      — fake opener; dlopens <path>/<name> directly
//!
//! What this guards: the shim↔loader contract (exports, negotiation, GIPA
//! resolution ordering) and the shim↔driver contract (selection logic,
//! extension-list parsing, fallback). A mismatch in either — e.g. an
//! export typo, a broken probe ordering, a botched selection env var —
//! fails here on the host instead of on device.

use std::path::PathBuf;
use std::process::Command;

fn target_dir() -> PathBuf {
    // tests run from target/<profile>/deps/<name>-<hash>
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().to_path_buf()
}

fn shim_so() -> PathBuf {
    let p = target_dir().join("libadrenotools_gamescope.so");
    assert!(p.exists(), "shim cdylib not built: {}", p.display());
    p
}

struct Scenario {
    comm: Option<&'static str>,
    env: Vec<(&'static str, String)>,
}

/// Runs icd_probe against the shim with the scenario's environment.
/// Returns (exit_code, stdout, stderr).
fn run_probe(s: &Scenario) -> (i32, String, String) {
    run_probe_args(s, &[])
}

/// Runs icd_probe with extra argv after the scenario environment is applied.
fn run_probe_args(s: &Scenario, extra: &[&str]) -> (i32, String, String) {
    ensure_fixtures();
    let dir = target_dir();
    let mut cmd = Command::new(dir.join("icd_probe"));
    // Nothing leaks in from the developer's shell: the selection logic is
    // driven entirely by the env each scenario declares.
    cmd.env_clear();
    cmd.env("LD_LIBRARY_PATH", &dir);
    // These scenarios assert on the shim's `gn-vkshim:` diagnostics, which are
    // gated off by default; turn the gate on for the child under test.
    cmd.env("GNDRM_LOG_VKSHIM", "1");
    // The probe runs on a host with no Adreno device node: pin the payload-driver
    // path so the tests exercise what this suite is about, not the system loader.
    cmd.env("GN_VKSHIM_ADRENO", "1");
    for (k, v) in &s.env {
        cmd.env(k, v);
    }
    if let Some(comm) = s.comm {
        cmd.arg("--comm").arg(comm);
    }
    for a in extra {
        cmd.arg(a);
    }
    cmd.arg(shim_so());
    let out = cmd.output().expect("spawn icd_probe");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn assert_ok(code: i32, stdout: &str, stderr: &str) {
    assert!(
        code == 0,
        "probe exit {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(stdout.contains("PROBE done"), "stdout:\n{stdout}");
}

fn adrenotools_env(dir: &std::path::Path) -> Vec<(&'static str, String)> {
    vec![
        ("ADRENOTOOLS_HOOKS_PATH", dir.display().to_string()),
        ("ADRENOTOOLS_DRIVER_PATH", dir.display().to_string()),
        ("ADRENOTOOLS_DRIVER_NAME", "libmock_vkdriver.so".to_string()),
    ]
}

/// The scenarios dlopen the shim's CDYLIB, but `cargo test` only rebuilds
/// the rlib it links against — and the fixture crates are separate
/// workspace members `cargo test -p` doesn't build at all. Build all of
/// them once up front so the probe never sees a stale .so.
fn ensure_fixtures() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let status = Command::new("cargo")
            .args([
                "build",
                "--offline",
                "-p",
                "adrenotools_gamescope",
                "-p",
                "mock-vkdriver",
                "-p",
                "mock-freedreno",
                "-p",
                "mock-adrenotools",
            ])
            .status()
            .expect("spawn cargo build for fixtures");
        assert!(status.success(), "fixture build failed: {status}");
    });
}

/// Runs icd_probe on the shim's system-driver arm: unlike `run_probe`, this
/// leaves the Adreno pin off, so the shim consults `GNDRM_DRIVER_TYPE`/`PREFIX`
/// for the guest's ICD instead of taking the payload path.
fn run_probe_system(s: &Scenario) -> (i32, String, String) {
    ensure_fixtures();
    let dir = target_dir();
    let mut cmd = Command::new(dir.join("icd_probe"));
    cmd.env_clear();
    cmd.env("LD_LIBRARY_PATH", &dir);
    cmd.env("GNDRM_LOG_VKSHIM", "1");
    for (k, v) in &s.env {
        cmd.env(k, v);
    }
    cmd.arg(shim_so());
    let out = cmd.output().expect("spawn icd_probe");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A wrapper driver is driven from the two values the launcher already sets: the
/// driver id (`GNDRM_DRIVER_TYPE`) and the container's `usr` (`PREFIX`). Nothing
/// else has to name the ICD path.
#[test]
fn wrapper_driver_is_driven_from_the_prefix() {
    let dir = target_dir();
    let prefix = dir.join(format!("gn-prefix-{}", std::process::id()));
    std::fs::create_dir_all(prefix.join("lib")).unwrap();
    let icd = prefix.join("lib/libvulkan_wrapper.so");
    std::fs::copy(dir.join("libmock_vkdriver.so"), &icd).unwrap();
    let env = vec![
        ("GNDRM_DRIVER_TYPE", "wrapper-gamenative".to_string()),
        ("PREFIX", prefix.display().to_string()),
    ];
    let (_code, _out, err) = run_probe_system(&Scenario { comm: None, env });
    let _ = std::fs::remove_dir_all(&prefix);
    assert!(
        err.contains(&format!("wrapper icd: driving {}", icd.display())),
        "stderr:\n{err}"
    );
}

/// A native driver never has an ICD under the prefix, even when a wrapper's .so
/// is still lying there after a driver switch.
#[test]
fn a_native_driver_never_drives_a_container_icd() {
    let dir = target_dir();
    let prefix = dir.join(format!("gn-prefix-native-{}", std::process::id()));
    std::fs::create_dir_all(prefix.join("lib")).unwrap();
    std::fs::copy(
        dir.join("libmock_vkdriver.so"),
        prefix.join("lib/libvulkan_wrapper.so"),
    )
    .unwrap();
    let env = vec![
        ("GNDRM_DRIVER_TYPE", "turnip".to_string()),
        ("PREFIX", prefix.display().to_string()),
    ];
    let (_code, _out, err) = run_probe_system(&Scenario { comm: None, env });
    let _ = std::fs::remove_dir_all(&prefix);
    assert!(!err.contains("wrapper icd"), "stderr:\n{err}");
}

#[test]
fn gamescope_process_gets_payload_freedreno() {
    let (code, out, err) = run_probe(&Scenario {
        comm: Some("gamescope"),
        env: vec![],
    });
    assert_ok(code, &out, &err);
    assert!(
        err.contains("gamescope process: payload freedreno loaded"),
        "stderr:\n{err}"
    );
    // Must NOT take the adrenotools path even though comm is gamescope.
    assert!(!err.contains("adrenotools"), "stderr:\n{err}");
}

#[test]
fn guest_without_env_falls_back_to_freedreno() {
    let (code, out, err) = run_probe(&Scenario {
        comm: None,
        env: vec![],
    });
    assert_ok(code, &out, &err);
    assert!(
        err.contains("falling back to payload freedreno loaded"),
        "stderr:\n{err}"
    );
}

#[test]
fn guest_with_wayland_driver_passes_through() {
    let dir = target_dir();
    let mut env = adrenotools_env(&dir);
    env.push((
        "MOCK_INSTANCE_EXTS",
        "VK_KHR_wayland_surface,VK_EXT_headless_surface".into(),
    ));
    let (code, out, err) = run_probe(&Scenario { comm: None, env });
    assert_ok(code, &out, &err);
    assert!(
        err.contains("VK_KHR_wayland_surface present"),
        "stderr:\n{err}"
    );
    assert!(!err.contains("proxy"), "stderr:\n{err}");
    // Passthrough = the app's extension set reaches the driver untouched.
    assert!(
        err.contains("mock-driver: vkCreateInstance exts=[]"),
        "stderr:\n{err}"
    );
}

#[test]
fn guest_with_headless_driver_engages_proxy() {
    let dir = target_dir();
    let env = adrenotools_env(&dir); // default MOCK_INSTANCE_EXTS = headless only
    let (code, out, err) = run_probe(&Scenario { comm: None, env });
    assert_ok(code, &out, &err);
    assert!(err.contains("via WSI proxy"), "stderr:\n{err}");
    // The proxy's throwaway probe instance must ask the driver for the
    // headless backing, never for wayland.
    assert!(
        err.contains("mock-driver: vkCreateInstance"),
        "stderr:\n{err}"
    );
    assert!(!err.contains("VK_KHR_wayland_surface\"]"), "stderr:\n{err}");
}

#[test]
fn no_proxy_escape_hatch_falls_back() {
    let dir = target_dir();
    let mut env = adrenotools_env(&dir);
    env.push(("GN_VKSHIM_NO_PROXY", "1".into()));
    let (code, out, err) = run_probe(&Scenario { comm: None, env });
    assert_ok(code, &out, &err);
    assert!(err.contains("GN_VKSHIM_NO_PROXY set"), "stderr:\n{err}");
    assert!(
        err.contains("falling back to payload freedreno loaded"),
        "stderr:\n{err}"
    );
}

#[test]
fn driver_refusing_instances_falls_back() {
    let dir = target_dir();
    let mut env = adrenotools_env(&dir);
    env.push(("MOCK_INSTANCE_EXTS", "".into())); // neither wayland nor headless
    let (code, out, err) = run_probe(&Scenario { comm: None, env });
    assert_ok(code, &out, &err);
    assert!(
        err.contains("lacks VK_KHR_wayland_surface and VK_EXT_headless_surface"),
        "stderr:\n{err}"
    );
    assert!(
        err.contains("falling back to payload freedreno loaded"),
        "stderr:\n{err}"
    );
}

/// `vkCreateXlibSurfaceKHR` is the entry point winex11.drv actually reaches
/// (winevulkan translates the app's VK_KHR_win32_surface to a host Xlib
/// surface). The DRI3 route into wine's own X window is now the OPT-IN
/// (`GN_VKSHIM_GUEST_SURFACE=1`; unset takes the override/Wayland route), so
/// this scenario selects it explicitly. The shim must
/// intercept the call and always report the route decision, gated or not: a
/// session that shows nothing is otherwise indistinguishable from a route that
/// never engaged. With a null XID the call must still succeed and leave the
/// surface usable-but-dropping, never block the guest.
#[test]
fn xlib_surface_entry_point_is_intercepted_and_reports_route() {
    let dir = target_dir();
    let mut env = adrenotools_env(&dir); // headless driver -> the shim's proxy answers GIPA
    env.push(("GN_VKSHIM_GUEST_SURFACE", "1".into())); // select the DRI3 route
    let (code, out, err) = run_probe_args(&Scenario { comm: None, env }, &["--xlib-window", "0"]);
    assert_ok(code, &out, &err);
    assert!(out.contains("PROBE xlib_surface"), "stdout:\n{out}");
    assert!(
        err.contains("x11 route: XID 0x0 unavailable (null XID)"),
        "the route decision must be reported unconditionally; stderr:\n{err}"
    );
}

/// Mesa's zink/kopper and EGL x11 create their surfaces through
/// `vkCreateXcbSurfaceKHR`; it must land on the same X11 window route as the
/// Xlib entry point.
#[test]
fn xcb_surface_entry_point_takes_the_xlib_route() {
    let dir = target_dir();
    let mut env = adrenotools_env(&dir);
    env.push(("GN_VKSHIM_GUEST_SURFACE", "1".into()));
    let (code, out, err) = run_probe_args(&Scenario { comm: None, env }, &["--xcb-window", "0"]);
    assert_ok(code, &out, &err);
    assert!(out.contains("PROBE xcb_surface"), "stdout:\n{out}");
    assert!(
        err.contains("x11 route: XID 0x0 unavailable (null XID)"),
        "stderr:\n{err}"
    );
}

#[test]
fn xcb_surface_defaults_to_override_route() {
    let dir = target_dir();
    let env = adrenotools_env(&dir);
    let (_code, out, err) = run_probe_args(&Scenario { comm: None, env }, &["--xcb-window", "0"]);
    assert!(out.contains("PROBE xcb_surface"), "stdout:\n{out}");
    assert!(err.contains("override: XID 0x0"), "stderr:\n{err}");
}

/// A non-null XID with no reachable X server must name that check (rather than
/// going silent), which is the case the device session could not distinguish.
#[test]
fn xlib_surface_without_x_names_the_unreachable_check() {
    let dir = target_dir();
    let mut env = adrenotools_env(&dir);
    env.push(("GN_VKSHIM_GUEST_SURFACE", "1".into())); // select the DRI3 route
                                                       // run_probe_args env_clears, so DISPLAY is unset for the child.
    let (code, out, err) = run_probe_args(&Scenario { comm: None, env }, &["--xlib-window", "1"]);
    assert_ok(code, &out, &err);
    assert!(out.contains("PROBE xlib_surface"), "stdout:\n{out}");
    assert!(
        err.contains("x11 route: XID 0x1 unavailable (guest X server unreachable (DISPLAY))"),
        "stderr:\n{err}"
    );
}

/// The DEFAULT (`GN_VKSHIM_GUEST_SURFACE` unset) must take the override/Wayland
/// route, not the guest-surface DRI3 one: the route decision must be reported,
/// and the DRI3 route line must be absent. On the host there is no Wayland
/// display, so the override route fails cleanly with
/// VK_ERROR_INITIALIZATION_FAILED and the probe exits non-zero — that is the
/// expected host outcome, not a test error.
#[test]
fn xlib_surface_defaults_to_override_route() {
    let dir = target_dir();
    let env = adrenotools_env(&dir);
    let (_code, out, err) = run_probe_args(&Scenario { comm: None, env }, &["--xlib-window", "0"]);
    assert!(out.contains("PROBE xlib_surface"), "stdout:\n{out}");
    assert!(
        err.contains("override: XID 0x0"),
        "the default must take the override/Wayland route; stderr:\n{err}"
    );
    assert!(
        !err.contains("x11 route:"),
        "the guest-surface DRI3 route must not be the default; stderr:\n{err}"
    );
}

/// The opt-in (`GN_VKSHIM_GUEST_SURFACE=1`) must take the guest-surface DRI3
/// route, not the override: the route decision must be reported, and the
/// override line must be absent. With a null XID the DRI3 arm reports the
/// null-XID check and carries on, which tells the two routes apart without
/// needing a reachable X server.
#[test]
fn xlib_surface_opt_in_takes_the_guest_surface_route() {
    let dir = target_dir();
    let mut env = adrenotools_env(&dir);
    env.push(("GN_VKSHIM_GUEST_SURFACE", "1".into())); // opt in to the DRI3 route
    let (_code, out, err) = run_probe_args(&Scenario { comm: None, env }, &["--xlib-window", "0"]);
    assert!(out.contains("PROBE xlib_surface"), "stdout:\n{out}");
    assert!(
        err.contains("x11 route: XID 0x0 unavailable (null XID)"),
        "the opt-in must take the guest-surface DRI3 route; stderr:\n{err}"
    );
    assert!(
        !err.contains("override: XID 0x0"),
        "the override route must not run when the opt-in is set; stderr:\n{err}"
    );
}
