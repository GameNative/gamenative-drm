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
use std::sync::atomic::{AtomicBool, Ordering};
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
    let mut slot = INJECTOR.lock().expect("injector poisoned");
    if slot.is_some() {
        return 0;
    }
    let (tx, rx) = mpsc::channel::<InputEvent>();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let path = format!("{runtime_dir}/{display}");
    let thread_path = path.clone();
    let handle = thread::Builder::new()
        .name("gndrm-input".into())
        .spawn(move || {
            run(&thread_path, &rx, &thread_stop);
        });
    match handle {
        Ok(t) => {
            *INPUT_TX.lock().expect("input tx poisoned") = Some(tx);
            *slot = Some(InjectorHandle {
                stop,
                thread: Some(t),
            });
            log_info!("injector started (target {path})");
            0
        }
        Err(e) => {
            log_error!("injector spawn: {e}");
            -5
        }
    }
}

/// `GameNativeDrm.nativeInjectorStop()`.
pub fn stop() {
    let handle = INJECTOR.lock().expect("injector poisoned").take();
    let Some(mut handle) = handle else { return };
    *INPUT_TX.lock().expect("input tx poisoned") = None;
    handle.stop.store(true, Ordering::SeqCst);
    if let Some(t) = handle.thread.take() {
        let _ = t.join();
    }
    log_info!("injector stopped");
}

fn post(ev: InputEvent) {
    if let Some(tx) = INPUT_TX.lock().expect("input tx poisoned").as_ref() {
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

// ---------------------------------------------------------------------------

/// Proxies + registry state for the current connection.
struct State {
    seat: Option<wl_seat::WlSeat>,
    ptr_mgr: Option<ZwlrVirtualPointerManagerV1>,
    kbd_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    pointer: Option<ZwlrVirtualPointerV1>,
    keyboard: Option<ZwpVirtualKeyboardV1>,
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

/// Outer retry loop: connect → run until socket death or stop → repeat.
fn run(path: &str, rx: &mpsc::Receiver<InputEvent>, stop: &AtomicBool) {
    let mut logged_wait = false;
    while !stop.load(Ordering::SeqCst) {
        match connect_and_run(path, rx, stop) {
            Ok(()) => return, // stop requested
            Err(e) => {
                if !logged_wait {
                    log_info!("injector waiting for {path}: {e}");
                    logged_wait = true;
                } else {
                    log_warn!("injector reconnecting: {e}");
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
    let conn = Connection::from_socket(stream).map_err(|e| format!("from_socket: {e}"))?;
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
    };
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| format!("registry roundtrip: {e}"))?;

    let (Some(seat), Some(ptr_mgr), Some(kbd_mgr)) = (&state.seat, &state.ptr_mgr, &state.kbd_mgr)
    else {
        return Err(format!(
            "missing globals (seat={} ptr={} kbd={})",
            state.seat.is_some(),
            state.ptr_mgr.is_some(),
            state.kbd_mgr.is_some()
        ));
    };

    state.pointer = Some(ptr_mgr.create_virtual_pointer(Some(seat), &qh, ()));
    state.keyboard = Some(kbd_mgr.create_virtual_keyboard(seat, &qh, ()));
    let keyboard = state.keyboard.as_ref().expect("keyboard just created");
    upload_keymap(keyboard);
    conn.flush().map_err(|e| format!("flush: {e}"))?;
    log_info!("injector connected: virtual pointer + keyboard created");

    // Main loop: apply queued input, flush, drain server events.
    let mut last_roundtrip = std::time::Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ev) => apply(&state, ev),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        conn.flush().map_err(|e| format!("flush: {e}"))?;
        // Keep the read side drained and detect server death promptly.
        if last_roundtrip.elapsed() >= Duration::from_millis(500) {
            event_queue
                .roundtrip(&mut state)
                .map_err(|e| format!("roundtrip: {e}"))?;
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
        InputEvent::Key(code, pressed) => {
            if let Some(kbd) = &state.keyboard {
                kbd.key(t, code as u32, u32::from(pressed));
            }
        }
    }
}

/// memfd + canned keymap → `zwp_virtual_keyboard_v1.keymap`.
fn upload_keymap(keyboard: &ZwpVirtualKeyboardV1) {
    let bytes = CANNED_KEYMAP.as_bytes();
    // SAFETY: memfd_create with a valid name; on success fd is owned.
    let fd = unsafe { libc::memfd_create(c"gndrm-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        log_error!(
            "keymap memfd_create failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    // SAFETY: fd is valid; bytes outlive the call.
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written != bytes.len() as isize {
        log_error!("keymap write short: {written}");
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
