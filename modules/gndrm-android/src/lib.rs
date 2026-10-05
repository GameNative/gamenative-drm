//! # gndrm-android (`libgamenative_drm.so`)
//!
//! The in-app half of gamenative-drm (doc `02` §7): JNI entry points called
//! from GameNative's display screen, the SEQPACKET server gamescope's shim
//! connects to, and the native SurfaceControl presenter (doc `android/06`,
//! see `asc.rs`).
//!
//! JNI class: `app.gamenative.display.GameNativeDrm`.
//!
//! Lifecycle: `nativeStart(socket, surface, w, h, hz)` → the SurfaceControl
//! layer is created on a dedicated server thread → accept the RPC and event
//! connections → `serve()`, while a second thread accepts the scan-out
//! allocator channel for as long as the session runs. `nativeStop()` shuts the
//! sockets down (unblocks accept/recv), joins the threads (presenter teardown
//! happens on the server thread), releases the window.
//!
//! A display power cycle is not a teardown. `nativeRetargetSurface(surface, w,
//! h, hz)` rebuilds only the publish layer from the new window and re-presents
//! the retained last frame, keeping the listen sockets, the object stores and
//! the gamescope connection alive; `nativeStop()` stays the session-exit path.
//!
//! On non-Android targets this crate is an empty stub so host-side
//! workspace gates (fmt/clippy/test) stay green; the real gates for this
//! module are `tools/build-android.sh` and on-device runs.

#![deny(clippy::all)]
#![cfg(target_os = "android")]

mod asc;
mod dump;
pub mod ffi;
mod injector;
#[macro_use]
mod log;
mod log_policy;

use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::Duration;

use jni::objects::{GlobalRef, JClass, JObject, JString};
use jni::sys::{jboolean, jfloat, jint, jlong, jstring};
use jni::{JNIEnv, JavaVM};

use gndrm_daemon::diag;
use gndrm_daemon::server;
use gndrm_daemon::{Device, NullPresenter, PanelDesc};

/// Return codes are negative errno-style values (`0` = success).
const E_BUSY: jint = -16;
const E_INVAL: jint = -22;
const E_FAULT: jint = -14;
const E_IO: jint = -5;
/// `nativeRetargetSurface` with no running session; the caller starts one.
const E_NOENT: jint = -2;

/// Real frames put on the panel (fb != 0 commits — gamescope's blank fb=0
/// commits excluded). Reset by `nativeStart`/`nativeStop`; polled by the app
/// via `nativeFramesPresented` for first-frame splash dismissal.
pub(crate) static PRESENTED_FRAMES: AtomicU64 = AtomicU64::new(0);

/// The app's `JavaVM`, cached from the first JNI entry point (`nativeStart`).
/// Needed by the presenter thread, which is a Rust-spawned thread with no
/// `JNIEnv` of its own: it attaches with [`JavaVM::attach_current_thread`] to
/// emit the booting-splash event. Set once per process; a missing VM only means
/// the event cannot be pushed (the app still polls `nativeFramesPresented`).
static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();

/// Global ref to `GameNativeDrm`, resolved on a Java thread by
/// [`cache_java_vm`]. A name lookup from the presenter thread cannot work: an
/// attached native thread has no context class loader, so `FindClass` falls back
/// to the system loader, which cannot see app classes.
static CALLBACK_CLASS: OnceLock<GlobalRef> = OnceLock::new();

/// Cache the process `JavaVM` and the callback class from a JNI entry point. Best-effort: any failure
/// is logged and ignored — the rest of the daemon does not depend on it.
fn cache_java_vm(env: &mut JNIEnv) {
    match env.get_java_vm() {
        Ok(vm) => {
            if JAVA_VM.set(vm).is_ok() {
                log_info!("cached JavaVM for JNI callbacks");
            }
        }
        Err(e) => log_warn!("get_java_vm failed: {e}"),
    }
    match env.find_class("app/gamenative/display/GameNativeDrm") {
        Ok(class) => match env.new_global_ref(class) {
            Ok(global) => {
                if CALLBACK_CLASS.set(global).is_ok() {
                    log_info!("cached GameNativeDrm class for JNI callbacks");
                }
            }
            Err(e) => log_warn!("new_global_ref(GameNativeDrm) failed: {e}"),
        },
        Err(e) => {
            log_warn!("find_class(GameNativeDrm) failed: {e}");
            let _ = env.exception_clear();
        }
    }
}

/// Emit `AndroidEvent.ClearBootingSplash` on the app's event bus. Best-effort —
/// a failure to attach, a missing cached VM, or a failing call is logged and
/// swallowed; nothing on this path may panic.
pub(crate) fn clear_booting_splash() {
    call_app_static("clearBootingSplash");
}

/// Attach the calling thread, call a no-arg static method on `GameNativeDrm`,
/// detach. Attaching needs the class resolved on a Java thread, so it comes from
/// [`CALLBACK_CLASS`]; every failure is logged and swallowed.
fn call_app_static(method: &str) {
    let Some(vm) = JAVA_VM.get() else {
        log_warn!("{method}: no cached JavaVM; call skipped");
        return;
    };
    let Some(class) = CALLBACK_CLASS.get() else {
        log_warn!("{method}: GameNativeDrm class not cached; call skipped");
        return;
    };
    // AttachGuard detaches on drop, including on the early returns below.
    let mut env = match vm.attach_current_thread() {
        Ok(env) => env,
        Err(e) => {
            log_warn!("{method}: attach_current_thread failed: {e}");
            return;
        }
    };
    match env.call_static_method(class, method, "()V", &[]) {
        Ok(_) => log_info!("{method}: called"),
        Err(e) => {
            log_warn!("{method}: call failed: {e}");
            // Never leave a pending exception on this thread.
            let _ = env.exception_clear();
        }
    }
}

/// Client fds accepted so far (`-1` = not yet). Written by the server
/// thread right after each accept; read by `nativeStop` to unblock a
/// `serve()` stuck in `recvmsg`.
struct AcceptedFds {
    rpc: RawFd,
    event: RawFd,
    /// Current scan-out allocator channel, `-1` while no client holds one. The
    /// acceptor thread rewrites this on every (re)connection; `nativeStop` and
    /// the server thread read it to unblock a `serve_scanout` parked in
    /// `recvmsg`.
    scanout: RawFd,
}

/// Send-able window handle: the raw pointer is only dereferenced on the
/// server thread (presenter lifetime ⊂ thread lifetime) and released in
/// `nativeStop` after the thread is joined.
#[derive(Copy, Clone)]
struct WindowPtr(*mut ffi::ANativeWindow);

// SAFETY: see type docs — dereference happens on exactly one thread.
unsafe impl Send for WindowPtr {}

struct Runtime {
    name: String,
    listener: RawFd,
    /// Dedicated render-node listener (`name + "-render"`): Xwayland's
    /// glamor/dri3 open is served here, one Device per client, so it never
    /// becomes a scan-out channel on `listener` (see `server` module docs).
    render_listener: RawFd,
    /// Stops the render-listener loop in [`stop`].
    render_stop: RawFd,
    render_thread: Option<thread::JoinHandle<()>>,
    fds: Arc<Mutex<AcceptedFds>>,
    window: WindowPtr,
    /// The presenter's state, shared with the JNI thread so a display power
    /// cycle can retarget the publish layer without tearing the session down.
    /// Weak: the server thread's presenter owns the session's strong reference,
    /// so the layer and the pool are still released on the server thread when
    /// `serve` returns.
    asc: Weak<Mutex<asc::AscState>>,
    thread: Option<thread::JoinHandle<()>>,
}

static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);

/// Log a caught panic payload, naming the subsystem that stopped. Every
/// containment site funnels through here so a contained panic is always
/// reported rather than silently swallowed.
fn report_panic(subsystem: &str, payload: &(dyn std::any::Any + Send)) {
    let msg = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>");
    log_error!("{subsystem}: PANICKED: {msg}; subsystem stopped, app continues");
}

/// Presenter lifetime: the SurfaceControl layer is created on the server
/// thread and released there when `serve` returns (see `asc.rs`, doc
/// `android/06`).
///
/// This is a hard panic barrier: a panic anywhere in the daemon session
/// (device bring-up, the serve loop, the presenter, teardown) stops the
/// display subsystem and is reported, but never unwinds into the JNI caller or
/// aborts the process. The spawned worker threads have their own barriers.
fn server_thread(
    listener: RawFd,
    window: WindowPtr,
    panel: PanelDesc,
    fds: Arc<Mutex<AcceptedFds>>,
    ready: mpsc::Sender<Result<Arc<Mutex<asc::AscState>>, String>>,
) {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        server_thread_inner(listener, window, panel, fds, ready);
    }));
    if let Err(e) = caught {
        report_panic("gndrm server", e.as_ref());
    }
}

fn server_thread_inner(
    listener: RawFd,
    window: WindowPtr,
    panel: PanelDesc,
    fds: Arc<Mutex<AcceptedFds>>,
    ready: mpsc::Sender<Result<Arc<Mutex<asc::AscState>>, String>>,
) {
    // 1. Build the presenter first; it is owned by this thread for the whole
    // session and released when `serve` returns. The shared handle is sent back
    // to `start` so a display power cycle can retarget the layer from JNI.
    let asc_state = Arc::new(Mutex::new(asc::AscState::new(
        window.0,
        i32::from(panel.width),
        i32::from(panel.height),
    )));
    let presenter: Box<dyn gndrm_daemon::Presenter> =
        Box::new(asc::AscPresenter::new(Arc::clone(&asc_state)));
    let _ = ready.send(Ok(Arc::clone(&asc_state)));

    // 2. Wire contract: first connection = RPC, second = events.
    let rpc = match server::accept(listener) {
        Ok(fd) => fd,
        Err(e) => {
            log_warn!("accept(rpc): {e} (stopped before client?)");
            return;
        }
    };
    fds.lock().expect("fds poisoned").rpc = rpc;
    let event = match server::accept(listener) {
        Ok(fd) => fd,
        Err(e) => {
            log_warn!("accept(event): {e}");
            // SAFETY: rpc is owned by this thread.
            unsafe { libc::close(rpc) };
            return;
        }
    };
    fds.lock().expect("fds poisoned").event = event;
    log_info!("client connected (rpc={rpc} event={event})");

    // 3. Third and later connections: the scan-out allocator channel (see
    // `asc.rs`). The shim asks for a buffer here and gets a dma-buf fd back,
    // because this process is the one that has to publish it and an
    // `AHardwareBuffer` cannot be obtained from a bare fd on API 33. The
    // accept is a loop, not a one-shot: a client that reconnects — after its
    // own restart, or a previous channel dying — is served without anything
    // being relaunched.
    //
    // The loop polls the listener together with a stop eventfd rather than
    // blocking in `accept()`, so teardown can end it without shutting the
    // listener down from under a pending accept.
    // SAFETY: an eventfd with no other users in this process.
    let stop_fd = match unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) } {
        fd if fd >= 0 => Some(fd),
        _ => {
            log_warn!("eventfd for the scan-out acceptor failed; no client allocator");
            None
        }
    };
    // The allocator's own handle on the pool, cloned once here: the scan-out
    // thread must never take the publish state's lock, which the presenter
    // holds across its blocking SurfaceFlinger round-trip.
    let pool = asc_state.lock().expect("asc state poisoned").pool();
    // The scan-out thread takes the pool Arc below, so the ADDFB2 geometry
    // sink installed further down needs its own clone.
    let addfb_pool = Arc::clone(&pool);
    // The spare-buffer worker keeps a pre-allocated buffer ready so a scan-out
    // request never has to call AHardwareBuffer_allocate on the reply path (a
    // gralloc allocation has no latency bound). It waits on the doorbell, never
    // on the pool lock.
    let doorbell = Arc::new(asc::WarmDoorbell::new());
    let warm_thread = {
        let pool = Arc::clone(&pool);
        let doorbell = Arc::clone(&doorbell);
        thread::Builder::new()
            .name("gndrm-warm".into())
            .spawn(move || {
                // A panic in the spare-buffer worker must not take the app
                // down: report it and let the worker stop (the pool simply
                // loses its pre-allocated spare).
                if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    asc::warm_worker(pool, doorbell);
                })) {
                    report_panic("gndrm-warm", e.as_ref());
                }
            })
            .ok()
    };
    let scanout_thread = stop_fd.and_then(|signal| {
        let doorbell = Arc::clone(&doorbell);
        let fds = Arc::clone(&fds);
        thread::Builder::new()
            .name("gndrm-scanout".into())
            .spawn(move || {
                // The acceptor is a subsystem: a panic here stops scan-out
                // allocation and is reported, but the app (and the serve loop
                // on the main gndrm thread) stay alive.
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    loop {
                        let mut pfds = [
                            libc::pollfd {
                                fd: listener,
                                events: libc::POLLIN,
                                revents: 0,
                            },
                            libc::pollfd {
                                fd: signal,
                                events: libc::POLLIN,
                                revents: 0,
                            },
                        ];
                        // Parked waiting for a connection: this row is what proves
                        // the daemon never accepted a request a shim timed out on.
                        diag::diag().set_role(diag::Role::Scanout, diag::State::ScanoutAccepting);
                        // SAFETY: two valid pollfds waited on indefinitely.
                        let n = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
                        if n < 0 {
                            let e = io::Error::last_os_error();
                            if e.raw_os_error() == Some(libc::EINTR) {
                                continue;
                            }
                            log_info!("scan-out acceptor poll: {e}");
                            return;
                        }
                        if pfds[1].revents != 0 {
                            log_info!("scan-out acceptor stopping");
                            return;
                        }
                        if pfds[0].revents == 0 {
                            continue;
                        }
                        let fd = match server::accept_newest(listener) {
                            Ok(fd) => fd,
                            Err(e) => {
                                log_info!("accept(scanout): {e}; acceptor stopping");
                                return;
                            }
                        };
                        fds.lock().expect("fds poisoned").scanout = fd;
                        log_info!("scan-out channel connected (fd={fd})");
                        let outcome = asc::serve_scanout(fd, listener, signal, &pool, &doorbell);
                        // The channel is done with this fd: it is either gone or
                        // superseded by a newer connection. Clear the published fd
                        // BEFORE closing it, and close it exactly once here (this
                        // thread accepted it and is its only owner), so no reader is
                        // left on a closed or replaced descriptor and nothing
                        // double-closes the number.
                        if let Ok(mut guard) = fds.lock() {
                            if guard.scanout == fd {
                                guard.scanout = -1;
                            }
                        }
                        log_info!("scan-out channel closed (fd={fd})");
                        // SAFETY: this thread accepted the fd and is its only owner.
                        unsafe { libc::close(fd) };
                        if matches!(outcome, asc::ScanoutChannel::Stop) {
                            log_info!("scan-out acceptor stopping");
                            return;
                        }
                        // Closed or Superseded: loop and adopt the newest connection
                        // (Superseded left one queued on the listener, so the poll
                        // below returns immediately).
                    }
                }));
                if let Err(e) = caught {
                    report_panic("gndrm-scanout", e.as_ref());
                }
            })
            .ok()
    });
    // The diagnostic watchdog. While a scan-out request is in flight it samples
    // the shared registry and, once that request has been stuck past the
    // warning threshold, logs exactly one snapshot naming the pool lock's
    // holder and every thread's state — the one device run that has to answer
    // "which thread or lock stopped the display". It only reads atomics, so a
    // tick cannot perturb the timing it is measuring.
    let watch_thread = thread::Builder::new()
        .name("gndrm-diag".into())
        .spawn(|| {
            let d = diag::diag();
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                while !d.watchdog_stopped() {
                    thread::sleep(Duration::from_millis(diag::STALL_TICK_MS));
                    if d.watchdog_stopped() {
                        break;
                    }
                    if let Some(line) = d.maybe_stall_dump(diag::STALL_WARN_MS) {
                        log_warn!("asc: {line}");
                    }
                }
            })) {
                report_panic("gndrm-diag", e.as_ref());
            }
        })
        .ok();

    // 4. Serve until the peer closes or nativeStop shuts the fds down.
    let mut dev = Device::new(panel, presenter);
    dev.set_logger(Box::new(crate::log::daemon_info));
    // Teach the pool the geometry of every framebuffer the client registers,
    // so a length-only scan-out request for a window-sized buffer resolves
    // even before that buffer is first published. This takes the pool's own
    // lock, never `AscState`'s: the presenter holds the latter across the
    // blocking SurfaceFlinger apply, while ADDFB2 runs here on the serve
    // thread. The lock is held for one map insert. The registration is logged
    // (a bounded handful per session, one per distinct geometry) so the
    // compressed-class allocation length entering the index is visible.
    let addfb_logged = std::sync::atomic::AtomicU32::new(0);
    dev.set_addfb_geometry(Box::new(move |width, height, alloc_len| {
        if addfb_logged.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
            log_info!("asc: addfb geometry {width}x{height} alloc_len={alloc_len}");
        }
        addfb_pool
            .lock(diag::Role::Commit)
            .note_framebuffer(width, height, alloc_len);
    }));
    let log = |msg: &str| crate::log::daemon_info(&format!("server: {msg}"));
    server::serve(&mut dev, rpc, event, &log);

    // 5. Teardown. Wake and join the scan-out acceptor first (it may be parked
    // in poll or in serve_scanout — both are signalled), so the presenter's
    // shared state ends up referenced by `dev` alone, then drop it here — the
    // SurfaceControl layer is therefore released on this thread.
    if let Some(signal) = stop_fd {
        let one: u64 = 1;
        // SAFETY: an 8-byte counter write to the eventfd created above.
        unsafe { libc::write(signal, (&raw const one).cast(), 8) };
    }
    if let Ok(f) = fds.lock() {
        if f.scanout >= 0 {
            // SAFETY: shutdown only unblocks a recvmsg; the acceptor thread
            // remains the owner that closes the descriptor.
            unsafe { libc::shutdown(f.scanout, libc::SHUT_RDWR) };
        }
    }
    if let Some(t) = scanout_thread {
        let _ = t.join();
    }
    // Stop the spare worker: its wait ends, it exits before the pool is
    // released, and any spare it still holds is returned by the presenter's
    // drop below.
    doorbell.stop();
    if let Some(t) = warm_thread {
        let _ = t.join();
    }
    // Stop the diagnostic watchdog. It reads only atomics, so it sees the
    // stopped flag within one tick; joining it before the presenter drops keeps
    // a final snapshot from racing teardown.
    diag::diag().watchdog_stop();
    if let Some(t) = watch_thread {
        let _ = t.join();
    }
    if let Some(signal) = stop_fd {
        // SAFETY: the acceptor is joined; nothing else uses this descriptor.
        unsafe { libc::close(signal) };
    }
    drop(dev);
    if let Ok(state) = asc_state.lock() {
        log_info!("asc: {} frames published", state.frames_presented());
    }
    // SAFETY: this thread owns both descriptors; nothing else uses them.
    unsafe {
        libc::close(rpc);
        libc::close(event);
    }
    log_info!("server thread exit");
}

fn start(
    env: &mut JNIEnv,
    socket_name: &JString,
    surface: &JObject,
    width: jint,
    height: jint,
    refresh_hz: jfloat,
) -> jint {
    PRESENTED_FRAMES.store(0, Ordering::Relaxed);
    // First JNI entry point to run: capture the VM so the presenter thread can
    // push the booting-splash event on the first frame.
    cache_java_vm(env);
    let name: String = match env.get_string(socket_name) {
        Ok(s) => s.to_string_lossy().into_owned(),
        Err(_) => return E_INVAL,
    };
    if width < 64 || height < 64 || !(1.0..=480.0).contains(&refresh_hz) {
        log_error!("bad geometry {width}x{height} @{refresh_hz}");
        return E_INVAL;
    }

    let mut slot = RUNTIME.lock().expect("runtime poisoned");
    if slot.is_some() {
        log_warn!("nativeStart while running");
        return E_BUSY;
    }

    // SAFETY: `surface` is a live android.view.Surface jobject; the
    // acquired window is released in nativeStop.
    let raw_window =
        unsafe { ffi::ANativeWindow_fromSurface(env.get_native_interface(), surface.as_raw()) };
    if raw_window.is_null() {
        log_error!("ANativeWindow_fromSurface returned null");
        return E_INVAL;
    }
    let window = WindowPtr(raw_window);

    let listener = match server::listen_seqpacket(&name) {
        Ok(fd) => fd,
        Err(e) => {
            log_error!("listen({name}): {e}");
            // SAFETY: window was acquired above and nothing else holds it.
            unsafe { ffi::ANativeWindow_release(window.0) };
            return E_IO;
        }
    };

    // Dedicated render-node listener for Xwayland's glamor/dri3 open. It must
    // NOT share the card listener: every post-primary connection there is the
    // scan-out allocator channel, so a render-node client on it would be
    // adopted and closed (the on-device EPIPE). Served by the same
    // `server::serve_render_listener` the host harness runs.
    let render_name = server::render_socket_name(&name);
    let render_listener = match server::listen_seqpacket(&render_name) {
        Ok(fd) => fd,
        Err(e) => {
            log_error!("listen({render_name}): {e}");
            // SAFETY: listener/window acquired above; nothing else holds them.
            unsafe {
                libc::close(listener);
                ffi::ANativeWindow_release(window.0);
            }
            return E_IO;
        }
    };
    let render_stop = match server::new_stop_eventfd() {
        Ok(fd) => fd,
        Err(e) => {
            log_error!("render stop eventfd: {e}");
            // SAFETY: listener/render_listener/window acquired above.
            unsafe {
                libc::close(listener);
                libc::close(render_listener);
                ffi::ANativeWindow_release(window.0);
            }
            return E_IO;
        }
    };

    // The window is the authority on the size actually presented into: the
    // caller's width/height can predate layout (a view measured before its
    // insets apply), which would size the emulated mode differently from the
    // surface it draws into and resample every frame.
    let (win_w, win_h) = unsafe {
        (
            ffi::ANativeWindow_getWidth(window.0),
            ffi::ANativeWindow_getHeight(window.0),
        )
    };
    let (panel_w, panel_h) = if win_w > 0 && win_h > 0 {
        (win_w as u16, win_h as u16)
    } else {
        (width as u16, height as u16)
    };
    let panel = PanelDesc {
        width: panel_w,
        height: panel_h,
        refresh_mhz: (refresh_hz * 1000.0) as u32,
        mm_width: 0,
        mm_height: 0,
    };
    let fds = Arc::new(Mutex::new(AcceptedFds {
        rpc: -1,
        event: -1,
        scanout: -1,
    }));

    // Render-node clients (Xwayland) on their own thread, served by the shared
    // device-side loop. A render node has no scan-out, so a `NullPresenter` is
    // exactly right: the render Device only ever serves GEM/PRIME/syncobj and
    // the DRM_RENDER_ALLOW gate.
    let render_log: server::SharedLog = Arc::new(crate::log::daemon_info);
    let render_handle = thread::Builder::new()
        .name("gndrm-render".into())
        .spawn(move || {
            server::serve_render_listener(
                render_listener,
                render_stop,
                Arc::new(move || Device::new(panel, Box::<NullPresenter>::default())),
                render_log,
            );
        });
    let render_handle = match render_handle {
        Ok(h) => h,
        Err(e) => {
            log_error!("render thread spawn: {e}");
            // SAFETY: nothing else can own these now.
            unsafe {
                libc::close(listener);
                libc::close(render_listener);
                libc::close(render_stop);
                ffi::ANativeWindow_release(window.0);
            }
            return E_IO;
        }
    };
    let (tx, rx) = mpsc::channel::<Result<Arc<Mutex<asc::AscState>>, String>>();
    let thread_fds = Arc::clone(&fds);
    let handle = thread::Builder::new()
        .name("gndrm".into())
        .spawn(move || server_thread(listener, window, panel, thread_fds, tx));
    let handle = match handle {
        Ok(h) => h,
        Err(e) => {
            log_error!("spawn: {e}");
            // Stop the render thread already started above, then reclaim.
            server::signal_stop(render_stop);
            // SAFETY: nothing else can own these now.
            unsafe {
                libc::shutdown(render_listener, libc::SHUT_RDWR);
                libc::close(listener);
                libc::close(render_listener);
                libc::close(render_stop);
            }
            let _ = render_handle.join();
            // SAFETY: render thread is joined; the window is ours again.
            unsafe { ffi::ANativeWindow_release(window.0) };
            return E_IO;
        }
    };

    // Wait for the presenter bring-up handshake before reporting success. The
    // server thread hands back its shared presenter state, which is kept so a
    // display power cycle can retarget the publish layer from JNI.
    let asc = match rx.recv() {
        Ok(Ok(asc)) => asc,
        other => {
            if let Ok(Err(e)) = other {
                log_error!("server thread init: {e}");
            }
            // Thread already exited (or is exiting); reclaim everything.
            let _ = handle.join();
            server::signal_stop(render_stop);
            // SAFETY: server thread is gone; listener/window/render still owned.
            unsafe {
                libc::shutdown(render_listener, libc::SHUT_RDWR);
                libc::close(listener);
                libc::close(render_listener);
                libc::close(render_stop);
            }
            let _ = render_handle.join();
            // SAFETY: render thread is joined; the window is ours again.
            unsafe { ffi::ANativeWindow_release(window.0) };
            return E_IO;
        }
    };

    log_info!("started: socket={name} {panel_w}x{panel_h} @{refresh_hz}Hz");
    log_info!("gndrm build 20260926-1: ADDFB2_MODIFIERS + IN_FORMATS (LINEAR) on both planes");
    let asc = Arc::downgrade(&asc);
    *slot = Some(Runtime {
        name,
        listener,
        render_listener,
        render_stop,
        render_thread: Some(render_handle),
        fds,
        window,
        asc,
        thread: Some(handle),
    });
    0
}

fn stop() {
    let rt = RUNTIME.lock().expect("runtime poisoned").take();
    let Some(mut rt) = rt else { return };
    log_info!("stopping ({})", rt.name);
    // Unblock accept()/recvmsg() regardless of which phase the thread is in.
    // SAFETY: fds are valid or -1; shutdown only unblocks, the thread
    // remains the owner that closes them.
    unsafe {
        libc::shutdown(rt.listener, libc::SHUT_RDWR);
        libc::close(rt.listener);
    }
    // Stop the render-node listener too: signal its poll, then shut the
    // listener down (it unblocks accept and, on the way out, shuts down every
    // live render client so its serve loop returns).
    server::signal_stop(rt.render_stop);
    // SAFETY: the render thread owns the accept side but only reads the
    // listener; shutdown unblocks it and the thread closes any accepted fds.
    unsafe {
        libc::shutdown(rt.render_listener, libc::SHUT_RDWR);
    }
    if let Some(t) = rt.render_thread.take() {
        if t.join().is_err() {
            log_error!("render thread panicked");
        }
    }
    // SAFETY: the render thread is joined; nothing else uses these now.
    unsafe {
        libc::close(rt.render_listener);
        libc::close(rt.render_stop);
    }
    if let Ok(f) = rt.fds.lock() {
        // SAFETY: as above; thread closes after serve() returns.
        unsafe {
            if f.rpc >= 0 {
                libc::shutdown(f.rpc, libc::SHUT_RDWR);
            }
            if f.event >= 0 {
                libc::shutdown(f.event, libc::SHUT_RDWR);
            }
            // Also unblocks a serve_scanout() parked in recvmsg.
            if f.scanout >= 0 {
                libc::shutdown(f.scanout, libc::SHUT_RDWR);
            }
        }
    }
    if let Some(t) = rt.thread.take() {
        if t.join().is_err() {
            log_error!("server thread panicked");
        }
    }
    // The presenter is down (dropped on the joined thread); the window is ours
    // again.
    // SAFETY: presenter no longer references the window.
    unsafe { ffi::ANativeWindow_release(rt.window.0) };
    PRESENTED_FRAMES.store(0, Ordering::Relaxed);
    log_info!("stopped");
}

/// Adopt a new display window for a RUNNING session: rebuild the publish layer
/// from `surface` and re-present the retained last frame. Nothing else is
/// touched — the listen sockets, the object stores and every gamescope
/// connection stay exactly as they are, which is what a display power cycle
/// needs (gamescope connects only once at startup, so a torn-down daemon can
/// never get it back).
///
/// Returns 0, [`E_NOENT`] when no session is running (the caller starts one),
/// or a negative code when the surface is unusable or the layer could not be
/// rebuilt (the current layer is then kept).
fn retarget(
    env: &mut JNIEnv,
    surface: &JObject,
    width: jint,
    height: jint,
    refresh_hz: jfloat,
) -> jint {
    if width < 64 || height < 64 || !(1.0..=480.0).contains(&refresh_hz) {
        log_error!("bad retarget geometry {width}x{height} @{refresh_hz}");
        return E_INVAL;
    }
    let mut slot = RUNTIME.lock().expect("runtime poisoned");
    let Some(rt) = slot.as_mut() else {
        log_info!("retarget: no running session");
        return E_NOENT;
    };
    // The presenter owns the session's strong reference to the shared state, so
    // a failed upgrade means the session is already shutting down.
    let Some(state) = rt.asc.upgrade() else {
        log_warn!("retarget: the presenter is shutting down; no layer to rebuild");
        return E_NOENT;
    };
    // SAFETY: `surface` is a live android.view.Surface jobject; the acquired
    // window is either released below or kept as the session's window.
    let raw =
        unsafe { ffi::ANativeWindow_fromSurface(env.get_native_interface(), surface.as_raw()) };
    if raw.is_null() {
        log_error!("retarget: ANativeWindow_fromSurface returned null");
        return E_INVAL;
    }
    let (win_w, win_h) = unsafe {
        (
            ffi::ANativeWindow_getWidth(raw),
            ffi::ANativeWindow_getHeight(raw),
        )
    };
    let (panel_w, panel_h) = if win_w > 0 && win_h > 0 {
        (win_w, win_h)
    } else {
        (width, height)
    };
    // The same window means a redundant or geometry-only callback (a resize, or
    // the `surfaceChanged` that always follows `surfaceCreated`): adopt the
    // size and drop the extra reference. Rebuilding the layer would only churn
    // it — it already holds the retained frame.
    if raw == rt.window.0 {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_panel(panel_w, panel_h);
        // SAFETY: `raw` is the extra reference this call acquired.
        unsafe { ffi::ANativeWindow_release(raw) };
        return 0;
    }
    let rebuilt = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retarget(raw, panel_w, panel_h);
    if !rebuilt {
        log_error!("retarget: the publish layer could not be rebuilt; keeping the current window");
        // SAFETY: `raw` is the extra reference this call acquired.
        unsafe { ffi::ANativeWindow_release(raw) };
        return E_IO;
    }
    let old = rt.window.0;
    rt.window = WindowPtr(raw);
    // SAFETY: the old window was acquired by `nativeStart`; the presenter now
    // builds on `raw` and nothing else references the old one.
    unsafe { ffi::ANativeWindow_release(old) };
    // The layer holds the retained frame again; ask gamescope to repaint so the
    // picture is redrawn now instead of waiting for the next guest frame (which
    // may never come if the guest is still paused).
    let _ = injector::gamescope_command("debug_force_repaint");
    log_info!("retarget: adopted the new window {panel_w}x{panel_h}");
    0
}

/// `GameNativeDrm.nativeStart` — see module docs. Returns 0 or a
/// negative errno-style code.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    socket_name: JString,
    surface: JObject,
    width: jint,
    height: jint,
    refresh_hz: jfloat,
) -> jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        start(&mut env, &socket_name, &surface, width, height, refresh_hz)
    }))
    .unwrap_or(E_FAULT)
}

/// `GameNativeDrm.nativeStop`.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativeStop(
    _env: JNIEnv,
    _class: JClass,
) {
    let _ = std::panic::catch_unwind(stop);
}

/// `GameNativeDrm.nativeRetargetSurface` — adopt a new display window for a
/// running session without ending it (see [`retarget`]). Returns 0, [`E_NOENT`]
/// when no session is running (the caller starts one), or another negative code
/// on failure.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativeRetargetSurface(
    mut env: JNIEnv,
    _class: JClass,
    surface: JObject,
    width: jint,
    height: jint,
    refresh_hz: jfloat,
) -> jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        retarget(&mut env, &surface, width, height, refresh_hz)
    }))
    .unwrap_or(E_FAULT)
}

/// `GameNativeDrm.nativeFramesPresented` — real frames put on the panel so
/// far (see [`PRESENTED_FRAMES`]); 0 before the first start and after stop.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativeFramesPresented(
    _env: JNIEnv,
    _class: JClass,
) -> jlong {
    std::panic::catch_unwind(|| PRESENTED_FRAMES.load(Ordering::Relaxed) as jlong).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// input injector (wayland virtual pointer/keyboard; see injector.rs)
// ---------------------------------------------------------------------------

/// `GameNativeDrm.nativeInjectorStart(xdgRuntimeDir, waylandDisplay)` —
/// starts the injector thread; it connects (with retry) to gamescope's
/// wayland socket at `<xdgRuntimeDir>/<waylandDisplay>`.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativeInjectorStart(
    mut env: JNIEnv,
    _class: JClass,
    xdg_runtime_dir: JString,
    wayland_display: JString,
    app_id: jint,
) -> jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dir: String = match env.get_string(&xdg_runtime_dir) {
            Ok(s) => s.to_string_lossy().into_owned(),
            Err(_) => return E_INVAL,
        };
        let display: String = match env.get_string(&wayland_display) {
            Ok(s) => s.to_string_lossy().into_owned(),
            Err(_) => return E_INVAL,
        };
        injector::start(dir, display, app_id as u32)
    }))
    .unwrap_or(E_FAULT)
}

/// `GameNativeDrm.nativeFps()` — gamescope's most recently reported app
/// frametime as a rate. 0 until gamescope has answered a stats request.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativeFps(
    _env: JNIEnv,
    _class: JClass,
) -> jfloat {
    std::panic::catch_unwind(|| {
        let ns = injector::app_frametime_ns();
        if ns == 0 {
            0.0
        } else {
            (1_000_000_000f64 / ns as f64) as jfloat
        }
    })
    .unwrap_or(0.0)
}

/// `GameNativeDrm.nativeInjectorStop()`.
/// `GameNativeDrm.nativeSetFpsLimit(fps)` — cap gamescope's frame rate for this
/// app (0 = uncapped). Never blocks.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativeSetFpsLimit(
    _env: JNIEnv,
    _class: JClass,
    fps: jint,
) {
    if let Err(e) = std::panic::catch_unwind(|| injector::set_fps_limit(fps.max(0) as u32)) {
        report_panic("gndrm-fpslimit", e.as_ref());
    }
}

#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativeInjectorStop(
    _env: JNIEnv,
    _class: JClass,
) {
    let _ = std::panic::catch_unwind(injector::stop);
}

/// `GameNativeDrm.nativeGamescopeCommand(command)` — run one allowlisted
/// gamescope concommand, e.g. `"scaling_filter fsr"` or `"scaling_sharpness 5"`
/// (see `injector::ALLOWED_COMMANDS`). Returns 0 when queued, [`E_INVAL`] when
/// the command name is empty or not allowlisted. Fire-and-forget: never blocks;
/// a command queued while gamescope is absent is logged and dropped.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativeGamescopeCommand(
    mut env: JNIEnv,
    _class: JClass,
    command: JString,
) -> jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let cmd: String = match env.get_string(&command) {
            Ok(s) => s.to_string_lossy().into_owned(),
            Err(_) => return E_INVAL,
        };
        if injector::gamescope_command(&cmd) {
            0
        } else {
            E_INVAL
        }
    }))
    .unwrap_or(E_FAULT)
}

/// `GameNativeDrm.nativeDisplayInfo()` — `feature_support` + `active_display_info`
/// as the compact string
/// `features=0x<hex>;connector=<name>;display_flags=0x<hex>;rates=<r>,<r>,...`
/// (features bit N = feature id N; rates empty when the display is fixed).
/// Reads cached values, so it never blocks and returns empty fields / 0 before
/// gamescope has answered, or while it is absent.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativeDisplayInfo(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match env.new_string(injector::display_and_features()) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// `GameNativeDrm.nativePointerAbsolute(x, y)`.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativePointerAbsolute(
    _env: JNIEnv,
    _class: JClass,
    x: jint,
    y: jint,
) {
    // Input handlers are a subsystem too: a poisoned injector lock must not
    // abort the app (see injector.rs), and anything else is contained here.
    if let Err(e) = std::panic::catch_unwind(|| injector::pointer_absolute(x, y)) {
        report_panic("gndrm-input", e.as_ref());
    }
}

/// `GameNativeDrm.nativePointerMotion(dx, dy)`.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativePointerMotion(
    _env: JNIEnv,
    _class: JClass,
    dx: jint,
    dy: jint,
) {
    if let Err(e) = std::panic::catch_unwind(|| injector::pointer_motion(dx, dy)) {
        report_panic("gndrm-input", e.as_ref());
    }
}

/// `GameNativeDrm.nativePointerButton(button, pressed)` — evdev BTN_* code.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativePointerButton(
    _env: JNIEnv,
    _class: JClass,
    button: jint,
    pressed: jboolean,
) {
    if let Err(e) = std::panic::catch_unwind(|| injector::pointer_button(button, pressed != 0)) {
        report_panic("gndrm-input", e.as_ref());
    }
}

/// `GameNativeDrm.nativePointerAxis(axis, value)` — 0 = vertical, 1 = horizontal.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativePointerAxis(
    _env: JNIEnv,
    _class: JClass,
    axis: jint,
    value: jfloat,
) {
    if let Err(e) = std::panic::catch_unwind(|| injector::pointer_axis(axis, value)) {
        report_panic("gndrm-input", e.as_ref());
    }
}

/// `GameNativeDrm.nativeKey(keyCode, pressed)` — evdev key code.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_display_GameNativeDrm_nativeKey(
    _env: JNIEnv,
    _class: JClass,
    key_code: jint,
    pressed: jboolean,
) {
    if let Err(e) = std::panic::catch_unwind(|| injector::key(key_code, pressed != 0)) {
        report_panic("gndrm-input", e.as_ref());
    }
}
