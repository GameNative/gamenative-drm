//! Wayland virtual-input injector: a small wayland client living on its own
//! thread inside `libgamenative_drm.so`. Connects to gamescope's wayland
//! socket (retrying until it appears — gamescope starts after the guest
//! launch), creates one virtual pointer + one virtual keyboard through
//! gamescope's I1 globals, and feeds events posted from JNI.
//!
//! Reconnects transparently when the socket dies (gamescope restart).
//! Events arriving while disconnected are drained and dropped — stale input
//! is worse than lost input.

use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use crate::{log_error, log_info, log_warn};

// virtual-keyboard-unstable-v1 is not exposed by wayland-protocols-wlr
// 0.3.x, so the XML is vendored (from wlroots, MIT) and bindings are
// generated with the same macro pattern the crate uses internally.
#[allow(dead_code, non_camel_case_types, clippy::all)]
mod virtual_keyboard {
    pub mod v1 {
        pub mod client {
            use wayland_client;
            use wayland_client::protocol::*;

            pub mod __interfaces {
                use wayland_client::protocol::__interfaces::*;
                wayland_scanner::generate_interfaces!("protocols/virtual-keyboard-unstable-v1.xml");
            }
            use self::__interfaces::*;
            wayland_scanner::generate_client_code!("protocols/virtual-keyboard-unstable-v1.xml");
        }
    }
}
use virtual_keyboard::v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use virtual_keyboard::v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;

// gamescope's private control protocol is not published as a wayland-protocols
// package; the XML is vendored (from gamescope, MIT) and bindings generated
// the same way as the keyboard above. Used for `set_app_target_refresh_cycle`
// (the fps cap) and its feature/display-info answers.
// The control protocol's enums are bitfields (hence bitflags) and it declares
// no external interfaces, so the usual glob imports are partly unused here.
#[allow(dead_code, non_camel_case_types, unused_imports, clippy::all)]
mod gscontrol {
    pub mod client {
        use wayland_client;
        use wayland_client::protocol::*;

        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/gamescope-control.xml");
        }
        use self::__interfaces::*;
        use bitflags::bitflags;
        wayland_scanner::generate_client_code!("protocols/gamescope-control.xml");
    }
}
use gscontrol::client::gamescope_control::{
    Event as GsControlEvent, GamescopeControl, TargetRefreshCycleFlag,
};

// gamescope's ReShade protocol, vendored (from gamescope, MIT) and generated the
// same way. It loads an effect by name and enables/disables it at runtime; the
// effect file is resolved by gamescope against
// `<home>/.local/share/gamescope/reshade/Shaders`.
#[allow(dead_code, non_camel_case_types, unused_imports, clippy::all)]
mod gsreshade {
    pub mod client {
        use wayland_client;
        use wayland_client::protocol::*;

        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/gamescope-reshade.xml");
        }
        use self::__interfaces::*;
        wayland_scanner::generate_client_code!("protocols/gamescope-reshade.xml");
    }
}
use gsreshade::client::gamescope_reshade::{Event as GsReshadeEvent, GamescopeReshade};

// gamescope's private concommand protocol, vendored (from gamescope, MIT) and
// generated the same way. Its `execute` request runs an allowlisted concommand
// inside the compositor — the route for settings that have no control request.
#[allow(dead_code, non_camel_case_types, unused_imports, clippy::all)]
mod gsprivate {
    pub mod client {
        use wayland_client;
        use wayland_client::protocol::*;

        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/gamescope-private.xml");
        }
        use self::__interfaces::*;
        wayland_scanner::generate_client_code!("protocols/gamescope-private.xml");
    }
}
use gsprivate::client::gamescope_private::{Event as GsPrivateEvent, GamescopePrivate};

/// Minimal xkb keymap (validated against libxkbcommon: compiles with 8
/// mods, 0 layouts). Contents barely matter: gamescope forces its seat
/// keymap onto every device it adopts (`wlserver_new_input` keyboard path).
const CANNED_KEYMAP: &str = "xkb_keymap {\n  xkb_keycodes \"basic\" { minimum = 8; maximum = 255; };\n  xkb_types \"basic\" { };\n  xkb_compat \"basic\" { };\n  xkb_symbols \"basic\" { };\n};\n";

const XKB_KEYMAP_FORMAT_TEXT_V1: u32 = 1;

/// Input events posted from JNI (any thread) into the injector thread.
enum InputEvent {
    PointerAbsolute(i32, i32),
    PointerMotion(i32, i32),
    PointerButton(i32, bool),
    PointerAxis(i32, f32),
    Key(i32, bool),
    /// An allowlisted concommand and its value, run through
    /// `gamescope_private.execute`.
    Command {
        name: String,
        value: String,
    },
    /// Load a ReShade effect by name (`game.fx`, relative to the shader set).
    ReshadeSetEffect(String),
    ReshadeEnable,
    ReshadeDisable,
}

struct InjectorHandle {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

/// The live channel into the injector thread (None = injector not running).
static INPUT_TX: Mutex<Option<mpsc::Sender<InputEvent>>> = Mutex::new(None);
static INJECTOR: Mutex<Option<InjectorHandle>> = Mutex::new(None);

/// Monotonic clock in milliseconds (wayland timestamp convention).
fn now_ms() -> u32 {
    // SAFETY: standard clock_gettime on a valid timespec.
    let ts = unsafe {
        let mut ts: libc::timespec = core::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        ts
    };
    (ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000) as u32
}

/// `GameNativeDrm.nativeInjectorStart(xdgRuntimeDir, waylandDisplay)`.
/// Idempotent; returns 0 or a negative code.
pub fn start(runtime_dir: String, display: String) -> i32 {
    // Poison-tolerant: a panic elsewhere must not turn this lock into a
    // process-killing panic on the next JNI call (see lib.rs containment).
    let mut slot = INJECTOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_some() {
        return 0;
    }
    // A fresh injector is never suspended. The flag is process-global, so a
    // session that ended while suspended would otherwise leave the next one
    // frozen: its commands would be applied and logged but never flushed to
    // the compositor. The app-side queue resets with the session; this mirrors
    // that at the daemon's own session boundary.
    SUSPENDED.store(false, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel::<InputEvent>();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let path = format!("{runtime_dir}/{display}");
    let thread_path = path.clone();
    let handle = thread::Builder::new()
        .name("gndrm-input".into())
        .spawn(move || {
            // The injector is a subsystem: a panic on its thread (a wayland
            // protocol error, a poisoned lock) is reported and ends the
            // thread, never the app process.
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(&thread_path, &rx, &thread_stop);
            })) {
                crate::report_panic("gndrm-input", e.as_ref());
            }
        });
    match handle {
        Ok(t) => {
            *INPUT_TX
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
            *slot = Some(InjectorHandle {
                stop,
                thread: Some(t),
            });
            log_info!("{}", gndrm_obf::obf!("injector started (target {path})"));
            0
        }
        Err(e) => {
            log_error!("{}", gndrm_obf::obf!("injector spawn: {e}"));
            -5
        }
    }
}

/// How long `nativeInjectorStop` may wait for the injector thread to leave its
/// loop. The thread tests `stop` at the top of that loop and inside a 100 ms
/// receive timeout, so a compositor that is answering lets it go almost
/// immediately; this bound is for the compositor that is present but silent.
const JOIN_TIMEOUT: Duration = Duration::from_millis(1000);

/// `GameNativeDrm.nativeInjectorStop()`.
pub fn stop() {
    let handle = INJECTOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(mut handle) = handle else { return };
    *INPUT_TX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    handle.stop.store(true, Ordering::SeqCst);
    if let Some(t) = handle.thread.take() {
        // Bounded join, because this runs on the app's MAIN thread: an
        // unbounded `join()` is an ANR whenever the thread cannot observe
        // `stop`, and it cannot while it waits on the Wayland socket — that wait
        // is `rustix::event::poll(fd, None)`, an infinite timeout, so a
        // compositor holding its socket open without answering parks the thread
        // where the stop check is never reached. The bound is the whole fix: the
        // app never waits longer than this. A thread that misses the deadline is
        // detached rather than joined, and returns on its own as soon as the
        // socket errors — which is what gamescope exiting does, closing it.
        let deadline = std::time::Instant::now() + JOIN_TIMEOUT;
        let mut finished = t.is_finished();
        while !finished && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
            finished = t.is_finished();
        }
        if finished {
            let _ = t.join();
        } else {
            log_warn!(
                "{}",
                gndrm_obf::obf!("injector thread parked past {JOIN_TIMEOUT:?}; detached")
            );
        }
    }
    log_info!("{}", gndrm_obf::obf!("injector stopped"));
}

fn post(ev: InputEvent) {
    if let Some(tx) = INPUT_TX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        // If the injector thread died, sending fails — drop silently.
        let _ = tx.send(ev);
    }
}

pub fn pointer_absolute(x: i32, y: i32) {
    post(InputEvent::PointerAbsolute(x, y));
}
pub fn pointer_motion(dx: i32, dy: i32) {
    post(InputEvent::PointerMotion(dx, dy));
}
pub fn pointer_button(button: i32, pressed: bool) {
    post(InputEvent::PointerButton(button, pressed));
}
pub fn pointer_axis(axis: i32, value: f32) {
    post(InputEvent::PointerAxis(axis, value));
}
pub fn key(code: i32, pressed: bool) {
    post(InputEvent::Key(code, pressed));
}

/// Frame-rate cap asked of gamescope; u32::MAX = nothing pending.
static FPS_LIMIT: AtomicU32 = AtomicU32::new(u32::MAX);

/// Ask gamescope to cap this app's frame rate (0 = uncapped). Applied by the
/// injector thread on its next wakeup, so this never blocks the caller.
pub fn set_fps_limit(fps: u32) {
    FPS_LIMIT.store(fps, Ordering::Relaxed);
}

/// Concommands a client may run through `gamescope_private.execute`. Each is
/// the runtime route for one of the settings with no `gamescope_control`
/// request (steamcompmgr.cpp / wlserver.cpp). The value string is passed
/// through unchanged; no other command name is accepted.
const ALLOWED_COMMANDS: &[&str] = &[
    "scaling_filter",
    "scaling_sharpness",
    "debug_set_fps_limit",
    "set_look",
    "screenshot",
    "debug_force_repaint",
    // Dirties the backend and re-polls it: the connector's mode list is read
    // again, so a rate the app changed mid-session becomes the mode.
    "backend_set_dirty",
];

/// Whether the session is suspended. Under the `auto`/`manual` suspend policy
/// the QuickMenu freezes the session's processes, gamescope among them, so while
/// suspended this loop must not touch the wayland socket: a flush or roundtrip to
/// the frozen compositor would block until it resumes. Commands issued then are
/// held app-side (GameNative's `GamescopeCommandQueue`) and arrive here on resume,
/// so nothing is queued on this thread.
static SUSPENDED: AtomicBool = AtomicBool::new(false);

/// `GameNativeDrm.nativeInjectorSetSuspended(suspended)` — mark the session
/// suspended (`true`) or resumed (`false`). A plain store, so it never blocks the
/// caller; it only gates the socket I/O in the loop below.
pub fn set_suspended(suspended: bool) {
    SUSPENDED.store(suspended, Ordering::SeqCst);
}

/// Queue a concommand for the injector thread. `command` is `"name [value]"`
/// (see [`ALLOWED_COMMANDS`]). Returns true when queued, false when the command
/// name is empty or not allowlisted. A queued command is run on the injector
/// thread's next wakeup; if gamescope is absent it is dropped.
pub fn gamescope_command(command: &str) -> bool {
    let mut parts = command.trim().splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("");
    let value = parts.next().unwrap_or("").trim();
    if name.is_empty() {
        return false;
    }
    if !ALLOWED_COMMANDS.contains(&name) {
        return false;
    }
    post(InputEvent::Command {
        name: name.to_string(),
        value: value.to_string(),
    });
    true
}

/// Load a ReShade effect and enable it. `name` is the path gamescope resolves
/// under `<home>/.local/share/gamescope/reshade/Shaders`, e.g.
/// `gamenative/Vibrant.fx`. Enabling after loading is what the protocol expects:
/// the effect stays disabled at `set_effect` so uniforms can be set first.
pub fn reshade_set_effect(name: &str) {
    let name = name.trim();
    // A pack subdirectory is a legitimate path, so separators are allowed; what
    // is refused is anything that would leave the shader tree.
    let escapes = name.starts_with('/')
        || name.contains('\\')
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if name.is_empty() || escapes {
        return;
    }
    post(InputEvent::ReshadeSetEffect(name.to_string()));
    post(InputEvent::ReshadeEnable);
}

/// Turn the loaded ReShade effect off.
pub fn reshade_disable() {
    post(InputEvent::ReshadeDisable);
}

/// Last `active_display_info` gamescope sent: connector, display flags and the
/// valid refresh rates for combined fps-limit + refresh control.
#[derive(Clone, Default)]
struct DisplayInfo {
    connector: String,
    flags: u32,
    rates: Vec<u32>,
}

/// Bitmask of `feature_support` entries received; bit N = feature id N
/// (see the `feature` enum in the protocol). 0 = none received yet.
static FEATURE_SUPPORT: AtomicU32 = AtomicU32::new(0);

/// Latest `active_display_info`; None until gamescope answers.
static DISPLAY_INFO: Mutex<Option<DisplayInfo>> = Mutex::new(None);

/// `display_flag` bit set for an internal panel (see the protocol's
/// `gamescope_control` enum).
const DISPLAY_FLAG_INTERNAL: u32 = 0x1;

/// Whether the connected display is an internal panel. gamescope files an fps
/// override under one screen type and `_update_app_target_refresh_cycle` reads
/// back the type of the live connector, so a cap sent without
/// [`TargetRefreshCycleFlag::InternalDisplay`] on an internal panel (a DSI/eDP
/// connector) is stored under the external type and never reaches the panel.
/// False until the display info arrives.
pub fn display_is_internal() -> bool {
    DISPLAY_INFO
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|info| info.flags & DISPLAY_FLAG_INTERNAL != 0)
}

/// `feature_support` + `active_display_info` as one compact string:
/// `features=0x<hex>;connector=<name>;display_flags=0x<hex>;rates=<r>,<r>,...`
/// (features bits and rates empty when gamescope has not answered). Read from
/// the cached values, so this never blocks and returns "" fields when the
/// compositor is absent.
pub fn display_and_features() -> String {
    let features = FEATURE_SUPPORT.load(Ordering::Relaxed);
    let info = DISPLAY_INFO
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_default();
    let rates = info
        .rates
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{}",
        gndrm_obf::obf!(
            "features=0x{features:X};connector={};display_flags=0x{:X};rates={rates}",
            info.connector,
            info.flags
        )
    )
}

// ---------------------------------------------------------------------------

/// Proxies + registry state for the current connection.
struct State {
    seat: Option<wl_seat::WlSeat>,
    ptr_mgr: Option<ZwlrVirtualPointerManagerV1>,
    kbd_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    pointer: Option<ZwlrVirtualPointerV1>,
    keyboard: Option<ZwpVirtualKeyboardV1>,
    /// gamescope's control global; absent if the compositor doesn't offer it.
    gs_control: Option<GamescopeControl>,
    /// gamescope's private concommand global; absent if not offered.
    gs_private: Option<GamescopePrivate>,
    /// gamescope's ReShade global; absent if the build has no ReShade support.
    gs_reshade: Option<GamescopeReshade>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" => {
                    state.seat =
                        Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(5), qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.ptr_mgr =
                        Some(registry.bind::<ZwlrVirtualPointerManagerV1, _, _>(name, 1, qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.kbd_mgr =
                        Some(registry.bind::<ZwpVirtualKeyboardManagerV1, _, _>(name, 1, qh, ()));
                }
                // The fps cap (`set_app_target_refresh_cycle`) is since v2, so
                // bind any version that carries it.
                "gamescope_control" if version >= 2 => {
                    state.gs_control =
                        Some(registry.bind::<GamescopeControl, _, _>(name, version.min(7), qh, ()));
                }
                "gamescope_private" => {
                    state.gs_private =
                        Some(registry.bind::<GamescopePrivate, _, _>(name, 1, qh, ()));
                }
                "gamescope_reshade" => {
                    state.gs_reshade =
                        Some(registry.bind::<GamescopeReshade, _, _>(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

macro_rules! empty_dispatch {
    ($ty:ty) => {
        impl Dispatch<$ty, ()> for State {
            fn event(
                _state: &mut Self,
                _proxy: &$ty,
                _event: <$ty as wayland_client::Proxy>::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    };
}
empty_dispatch!(wl_seat::WlSeat);
empty_dispatch!(ZwlrVirtualPointerManagerV1);
empty_dispatch!(ZwlrVirtualPointerV1);
empty_dispatch!(ZwpVirtualKeyboardManagerV1);
empty_dispatch!(ZwpVirtualKeyboardV1);
empty_dispatch!(wayland_client::protocol::wl_display::WlDisplay);

impl Dispatch<GamescopeReshade, ()> for State {
    fn event(
        _state: &mut Self,
        _reshade: &GamescopeReshade,
        event: GsReshadeEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The protocol has one event today, so the match is exhaustive.
        let GsReshadeEvent::EffectReady { effect_path } = event;
        log_info!("{}", gndrm_obf::obf!("reshade effect ready: {effect_path}"));
    }
}

impl Dispatch<GamescopePrivate, ()> for State {
    fn event(
        _state: &mut Self,
        _private: &GamescopePrivate,
        _event: GsPrivateEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // `Log` (a concommand's own output) and `CommandExecuted` (gamescope ran
        // it) need no action of ours.
    }
}

impl Dispatch<GamescopeControl, ()> for State {
    fn event(
        _state: &mut Self,
        _control: &GamescopeControl,
        event: GsControlEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // One entry per supported feature, terminated by "done". Cache the
            // bitmask the UI gates its controls on (ids are 0..=9; anything
            // wider is dropped rather than shifted out of range).
            GsControlEvent::FeatureSupport { feature, .. } => {
                FEATURE_SUPPORT.fetch_or(1u32.checked_shl(feature).unwrap_or(0), Ordering::Relaxed);
            }
            // Connector, display flags and valid refresh rates; the source for
            // refresh-rate / HDR / VRR gating.
            GsControlEvent::ActiveDisplayInfo {
                connector_name,
                display_flags,
                valid_refresh_rates,
                ..
            } => {
                let flags = match display_flags {
                    wayland_client::WEnum::Value(f) => f.bits(),
                    wayland_client::WEnum::Unknown(bits) => bits,
                };
                let rates = valid_refresh_rates
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_ne_bytes(*c))
                    .collect();
                *DISPLAY_INFO
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(DisplayInfo {
                    connector: connector_name,
                    flags,
                    rates,
                });
            }
            _ => {}
        }
    }
}

/// Outer retry loop: connect → run until socket death or stop → repeat.
fn run(path: &str, rx: &mpsc::Receiver<InputEvent>, stop: &AtomicBool) {
    let mut logged_wait = false;
    while !stop.load(Ordering::SeqCst) {
        match connect_and_run(path, rx, stop) {
            Ok(()) => return, // stop requested
            Err(e) => {
                if !logged_wait {
                    log_info!("{}", gndrm_obf::obf!("injector waiting for {path}: {e}"));
                    logged_wait = true;
                } else {
                    log_warn!("{}", gndrm_obf::obf!("injector reconnecting: {e}"));
                }
                // Drain stale input while disconnected, then wait.
                while rx.try_recv().is_ok() {}
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// One connection lifecycle. Returns Ok only on explicit stop.
fn connect_and_run(
    path: &str,
    rx: &mpsc::Receiver<InputEvent>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
    let conn = Connection::from_socket(stream)
        .map_err(|e| format!("{}", gndrm_obf::obf!("from_socket: {e}")))?;
    let mut event_queue = conn.new_event_queue::<State>();
    let qh = event_queue.handle();

    let display = conn.display();
    display.get_registry(&qh, ());
    let mut state = State {
        seat: None,
        ptr_mgr: None,
        kbd_mgr: None,
        pointer: None,
        keyboard: None,
        gs_control: None,
        gs_private: None,
        gs_reshade: None,
    };
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| format!("{}", gndrm_obf::obf!("registry roundtrip: {e}")))?;

    let (Some(seat), Some(ptr_mgr), Some(kbd_mgr)) = (&state.seat, &state.ptr_mgr, &state.kbd_mgr)
    else {
        return Err(format!(
            "{}",
            gndrm_obf::obf!(
                "missing globals (seat={} ptr={} kbd={})",
                state.seat.is_some(),
                state.ptr_mgr.is_some(),
                state.kbd_mgr.is_some()
            )
        ));
    };

    state.pointer = Some(ptr_mgr.create_virtual_pointer(Some(seat), &qh, ()));
    state.keyboard = Some(kbd_mgr.create_virtual_keyboard(seat, &qh, ()));
    let keyboard = state
        .keyboard
        .as_ref()
        .expect(&gndrm_obf::obf!("keyboard just created"));
    upload_keymap(keyboard);
    if state.gs_control.is_none() {
        log_warn!(
            "{}",
            gndrm_obf::obf!("gamescope_control not offered; fps cap unavailable")
        );
    }
    conn.flush()
        .map_err(|e| format!("{}", gndrm_obf::obf!("flush: {e}")))?;
    log_info!(
        "{}",
        gndrm_obf::obf!("injector connected: virtual pointer + keyboard created")
    );

    // Main loop: apply queued input, apply a pending fps cap, flush, drain
    // server events.
    let mut last_roundtrip = std::time::Instant::now();
    // Cap last sent, plus the screen type it was filed under: gamescope takes
    // the cap as a request, so re-issue it when either changes.
    let mut applied_fps: Option<(u32, bool)> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let suspended = SUSPENDED.load(Ordering::SeqCst);
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ev) => {
                // Input for a frozen session is dropped — stale input is worse
                // than lost input. A gamescope command is applied even while
                // suspended: the app queues commands app-side, so one arriving
                // here during the resume transition (this thread still holding
                // the pre-resume flag) is one the app just flushed. Left in the
                // connection's outgoing buffer, the next flush delivers it.
                if !suspended
                    || matches!(
                        ev,
                        InputEvent::Command { .. }
                            | InputEvent::ReshadeSetEffect(_)
                            | InputEvent::ReshadeEnable
                            | InputEvent::ReshadeDisable
                    )
                {
                    apply(&state, ev);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        // While suspended, leave the socket alone: a flush or roundtrip to the
        // frozen gamescope would block this loop until it resumes. The pending
        // commands live app-side and arrive after resume.
        if suspended {
            continue;
        }
        // gamescope takes the cap as a request, so re-issue it only when the
        // value changes (including the first pass after connecting) or when the
        // display info that decides its screen type finally arrives.
        if let Some(control) = &state.gs_control {
            let want = FPS_LIMIT.load(Ordering::Relaxed);
            let internal = display_is_internal();
            if want != u32::MAX && applied_fps != Some((want, internal)) {
                let flags = if internal {
                    TargetRefreshCycleFlag::InternalDisplay
                } else {
                    TargetRefreshCycleFlag::empty()
                };
                control.set_app_target_refresh_cycle(want, flags);
                applied_fps = Some((want, internal));
            }
        }
        conn.flush()
            .map_err(|e| format!("{}", gndrm_obf::obf!("flush: {e}")))?;
        // Keep the read side drained and detect server death promptly.
        if last_roundtrip.elapsed() >= Duration::from_millis(500) {
            event_queue
                .roundtrip(&mut state)
                .map_err(|e| format!("{}", gndrm_obf::obf!("roundtrip: {e}")))?;
            last_roundtrip = std::time::Instant::now();
        }
    }
}

fn apply(state: &State, ev: InputEvent) {
    let t = now_ms();
    match ev {
        InputEvent::PointerAbsolute(x, y) => {
            if let Some(p) = &state.pointer {
                // Extent = the gamescope output size; coordinates arrive in
                // the same space (the view the user touches IS the output).
                p.motion_absolute(t, x as u32, y as u32, 0, 0);
                p.frame();
            }
        }
        InputEvent::PointerMotion(dx, dy) => {
            if let Some(p) = &state.pointer {
                p.motion(t, f64::from(dx), f64::from(dy));
                p.frame();
            }
        }
        InputEvent::PointerButton(button, pressed) => {
            if let Some(p) = &state.pointer {
                let state_enum = if pressed {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                p.button(t, button as u32, state_enum);
                p.frame();
            }
        }
        InputEvent::PointerAxis(axis, value) => {
            if let Some(p) = &state.pointer {
                let axis_enum = if axis == 0 {
                    wl_pointer::Axis::VerticalScroll
                } else {
                    wl_pointer::Axis::HorizontalScroll
                };
                p.axis(t, axis_enum, f64::from(value));
                p.frame();
            }
        }
        InputEvent::ReshadeSetEffect(path) => {
            if let Some(r) = &state.gs_reshade {
                r.set_effect(path);
            }
        }
        InputEvent::ReshadeEnable => {
            if let Some(r) = &state.gs_reshade {
                r.enable_effect();
            }
        }
        InputEvent::ReshadeDisable => {
            if let Some(r) = &state.gs_reshade {
                r.disable_effect();
            }
        }
        InputEvent::Key(code, pressed) => {
            if let Some(kbd) = &state.keyboard {
                kbd.key(t, code as u32, u32::from(pressed));
            }
        }
        InputEvent::Command { name, value } => apply_command(state, name, value),
    }
}

/// Run one allowlisted concommand through gamescope. A command whose
/// `gamescope_private` global is absent is dropped.
fn apply_command(state: &State, name: String, value: String) {
    if let Some(private) = &state.gs_private {
        private.execute(name, value);
    }
}

/// memfd + canned keymap → `zwp_virtual_keyboard_v1.keymap`.
fn upload_keymap(keyboard: &ZwpVirtualKeyboardV1) {
    let bytes = CANNED_KEYMAP.as_bytes();
    // SAFETY: memfd_create with a valid name; on success fd is owned.
    let fd = unsafe { libc::memfd_create(c"gndrm-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        log_error!(
            "{}",
            gndrm_obf::obf!(
                "keymap memfd_create failed: {}",
                std::io::Error::last_os_error()
            )
        );
        return;
    }
    // SAFETY: fd is valid; bytes outlive the call.
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written != bytes.len() as isize {
        log_error!("{}", gndrm_obf::obf!("keymap write short: {written}"));
        // SAFETY: owned fd.
        unsafe { libc::close(fd) };
        return;
    }
    {
        use std::os::fd::BorrowedFd;
        // SAFETY: fd is valid for the duration of the call.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        keyboard.keymap(XKB_KEYMAP_FORMAT_TEXT_V1, borrowed, bytes.len() as u32);
    }
    // SAFETY: the compositor dups the fd over the wire; ours is no longer needed.
    unsafe { libc::close(fd) };
}
