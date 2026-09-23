//! # gndrm-android (`libgamenative_drm.so`)
//!
//! The in-app half of gamenative-drm (doc `02` §7): JNI entry points called
//! from GameNative's display screen, the SEQPACKET server gamescope's shim
//! connects to, and the EGL presenter.
//!
//! Lifecycle: `nativeStart(socket, surface, w, h, hz)` → EGL up on a
//! dedicated server thread → accept RPC + event connections → `serve()`.
//! `nativeStop()` shuts the sockets down (unblocks accept/recv), joins the
//! thread (EGL teardown happens there), releases the window.
//!
//! On non-Android targets this crate is an empty stub so host-side
//! workspace gates (fmt/clippy/test) stay green; the real gates for this
//! module are `tools/build-android.sh` and on-device runs.

#![deny(clippy::all)]
#![cfg(target_os = "android")]

mod egl;
pub mod ffi;
#[macro_use]
mod log;

use std::os::fd::RawFd;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use jni::objects::{JClass, JObject, JString};
use jni::sys::{jfloat, jint};
use jni::JNIEnv;

use gndrm_daemon::server;
use gndrm_daemon::{Device, PanelDesc};

/// Return codes are negative errno-style values (`0` = success).
const E_BUSY: jint = -16;
const E_INVAL: jint = -22;
const E_FAULT: jint = -14;
const E_IO: jint = -5;

/// Client fds accepted so far (`-1` = not yet). Written by the server
/// thread right after each accept; read by `nativeStop` to unblock a
/// `serve()` stuck in `recvmsg`.
struct AcceptedFds {
    rpc: RawFd,
    event: RawFd,
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
    fds: Arc<Mutex<AcceptedFds>>,
    window: WindowPtr,
    thread: Option<thread::JoinHandle<()>>,
}

static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);

fn server_thread(
    listener: RawFd,
    window: WindowPtr,
    panel: PanelDesc,
    fds: Arc<Mutex<AcceptedFds>>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    // 1. EGL first: init errors surface to nativeStart synchronously.
    let presenter =
        match egl::EglPresenter::new(window.0, i32::from(panel.width), i32::from(panel.height)) {
            Ok(p) => p,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
    let _ = ready.send(Ok(()));

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

    // 3. Serve until the peer closes or nativeStop shuts the fds down.
    let mut dev = Device::new(panel, Box::new(presenter));
    let log = |msg: &str| log_info!("server: {msg}");
    server::serve(&mut dev, rpc, event, &log);

    // 4. Owned-fd cleanup; the presenter (EGL) drops here, on its thread.
    // SAFETY: serve() has returned; no further use of either fd.
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

    let panel = PanelDesc {
        width: width as u16,
        height: height as u16,
        refresh_mhz: (refresh_hz * 1000.0) as u32,
        mm_width: 0,
        mm_height: 0,
    };
    let fds = Arc::new(Mutex::new(AcceptedFds { rpc: -1, event: -1 }));
    let (tx, rx) = mpsc::channel();
    let thread_fds = Arc::clone(&fds);
    let handle = thread::Builder::new()
        .name("gndrm".into())
        .spawn(move || server_thread(listener, window, panel, thread_fds, tx));
    let handle = match handle {
        Ok(h) => h,
        Err(e) => {
            log_error!("spawn: {e}");
            // SAFETY: nothing else can own these now.
            unsafe {
                libc::close(listener);
                ffi::ANativeWindow_release(window.0);
            }
            return E_IO;
        }
    };

    // Wait for the EGL bring-up handshake before reporting success.
    match rx.recv() {
        Ok(Ok(())) => {}
        other => {
            if let Ok(Err(e)) = other {
                log_error!("EGL init: {e}");
            }
            // Thread already exited (or is exiting); reclaim everything.
            let _ = handle.join();
            // SAFETY: server thread is gone; listener/window still owned.
            unsafe {
                libc::close(listener);
                ffi::ANativeWindow_release(window.0);
            }
            return E_IO;
        }
    }

    log_info!("started: socket={name} {width}x{height} @{refresh_hz}Hz");
    *slot = Some(Runtime {
        name,
        listener,
        fds,
        window,
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
    if let Ok(f) = rt.fds.lock() {
        // SAFETY: as above; thread closes after serve() returns.
        unsafe {
            if f.rpc >= 0 {
                libc::shutdown(f.rpc, libc::SHUT_RDWR);
            }
            if f.event >= 0 {
                libc::shutdown(f.event, libc::SHUT_RDWR);
            }
        }
    }
    if let Some(t) = rt.thread.take() {
        if t.join().is_err() {
            log_error!("server thread panicked");
        }
    }
    // EGL is down (dropped on the joined thread); the window is ours again.
    // SAFETY: presenter no longer references the window.
    unsafe { ffi::ANativeWindow_release(rt.window.0) };
    log_info!("stopped");
}

/// `app.gamenative.drm.GnDrm.nativeStart` — see module docs. Returns 0 or a
/// negative errno-style code.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_drm_GnDrm_nativeStart(
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

/// `app.gamenative.drm.GnDrm.nativeStop`.
#[no_mangle]
pub extern "system" fn Java_app_gamenative_drm_GnDrm_nativeStop(_env: JNIEnv, _class: JClass) {
    let _ = std::panic::catch_unwind(stop);
}

/// `app.gamenative.drm.GnDrm.nativeSurfaceChanged` — v1 semantics: full
/// restart with the new surface/geometry (SurfaceView rotation/resize
/// delivers a brand-new surface anyway).
#[no_mangle]
pub extern "system" fn Java_app_gamenative_drm_GnDrm_nativeSurfaceChanged(
    env: JNIEnv,
    class: JClass,
    surface: JObject,
    width: jint,
    height: jint,
    refresh_hz: jfloat,
) -> jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let name = RUNTIME
            .lock()
            .expect("runtime poisoned")
            .as_ref()
            .map(|rt| rt.name.clone());
        let Some(name) = name else { return E_INVAL };
        stop();
        let jname = match env.new_string(&name) {
            Ok(s) => s,
            Err(_) => return E_FAULT,
        };
        Java_app_gamenative_drm_GnDrm_nativeStart(
            env, class, jname, surface, width, height, refresh_hz,
        )
    }))
    .unwrap_or(E_FAULT)
}
