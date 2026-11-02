//! The display's vsync, read from `AChoreographer` and handed to the daemon's
//! flip timeline (`gndrm_daemon::vblank`).
//!
//! A KMS driver stamps page-flip events with the hardware vblank; this process
//! has no vblank interrupt, but it has the vsync `SurfaceFlinger` hands every
//! app. One thread owns an `ALooper` and a Choreographer and keeps one callback
//! posted per vsync while a session runs:
//!
//! - API 33+: `AChoreographer_postVsyncCallback`. The callback data carries the
//!   vsync time and the preferred frame timeline's deadline, the instant a
//!   buffer must reach `SurfaceFlinger` by to make that timeline's present —
//!   the instant the flip events stamp (see `gndrm_daemon::vblank`).
//! - API 29-32: `AChoreographer_postFrameCallback64`, the vsync time only; the
//!   phase then comes from the latch grid (`vsync.rs`).
//!
//! The daemon fits the period to the measured spacing, so the integer rate the
//! app reports never reaches the timeline. Between sessions the thread stays
//! parked in its looper with no callback posted: no wakeups.

use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use gndrm_daemon::vblank::{self, VsyncLock, VsyncReport};

/// Opaque `AChoreographer`, `AChoreographerFrameCallbackData`.
type Choreographer = c_void;
type FrameData = c_void;

type FnGetInstance = unsafe extern "C" fn() -> *mut Choreographer;
type VsyncCallback = unsafe extern "C" fn(data: *const FrameData, user: *mut c_void);
// Both post functions return `void` (`<android/choreographer.h>`): there is no
// status to read, so a callback that never comes is caught by the poll timeout
// in `run`, not by a return value.
type FnPostVsyncCallback =
    unsafe extern "C" fn(ch: *mut Choreographer, cb: VsyncCallback, user: *mut c_void);
type FrameCallback64 = unsafe extern "C" fn(frame_time_nanos: i64, user: *mut c_void);
type FnPostFrameCallback64 =
    unsafe extern "C" fn(ch: *mut Choreographer, cb: FrameCallback64, user: *mut c_void);
type FnGetFrameTime = unsafe extern "C" fn(data: *const FrameData) -> i64;
type FnGetPreferredIndex = unsafe extern "C" fn(data: *const FrameData) -> usize;
type FnGetDeadline = unsafe extern "C" fn(data: *const FrameData, index: usize) -> i64;
type FnLooperWake = unsafe extern "C" fn(looper: *mut c_void);
type FnLooperAcquire = unsafe extern "C" fn(looper: *mut c_void);

/// API 33's per-vsync data accessors.
#[derive(Clone, Copy)]
struct VsyncApi {
    post: FnPostVsyncCallback,
    frame_time: FnGetFrameTime,
    preferred: FnGetPreferredIndex,
    deadline: FnGetDeadline,
}

#[derive(Clone, Copy)]
struct Api {
    get_instance: FnGetInstance,
    vsync: Option<VsyncApi>,
    frame64: Option<FnPostFrameCallback64>,
    prepare: crate::ffi::FnALooperPrepare,
    poll_once: crate::ffi::FnALooperPollOnce,
    wake: FnLooperWake,
    acquire: FnLooperAcquire,
}

impl Api {
    fn load() -> Option<Self> {
        // SAFETY: dlopen of the platform library this process already has
        // loaded (RTLD_NOLOAD), with a plain dlopen as the fallback; each
        // symbol is cast to the signature `<android/choreographer.h>` and
        // `<android/looper.h>` declare for it.
        unsafe {
            let mut handle = libc::dlopen(
                c"libandroid.so".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_NOLOAD,
            );
            if handle.is_null() {
                handle = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
            }
            if handle.is_null() {
                return None;
            }
            use crate::ffi::sym;
            let vsync = match (
                sym::<FnPostVsyncCallback>(handle, c"AChoreographer_postVsyncCallback"),
                sym::<FnGetFrameTime>(handle, c"AChoreographerFrameCallbackData_getFrameTimeNanos"),
                sym::<FnGetPreferredIndex>(
                    handle,
                    c"AChoreographerFrameCallbackData_getPreferredFrameTimelineIndex",
                ),
                sym::<FnGetDeadline>(
                    handle,
                    c"AChoreographerFrameCallbackData_getFrameTimelineDeadlineNanos",
                ),
            ) {
                (Some(post), Some(frame_time), Some(preferred), Some(deadline)) => Some(VsyncApi {
                    post,
                    frame_time,
                    preferred,
                    deadline,
                }),
                _ => None,
            };
            let frame64 =
                sym::<FnPostFrameCallback64>(handle, c"AChoreographer_postFrameCallback64");
            if vsync.is_none() && frame64.is_none() {
                return None;
            }
            Some(Self {
                get_instance: sym(handle, c"AChoreographer_getInstance")?,
                vsync,
                frame64,
                prepare: sym(handle, c"ALooper_prepare")?,
                poll_once: sym(handle, c"ALooper_pollOnce")?,
                wake: sym(handle, c"ALooper_wake")?,
                acquire: sym(handle, c"ALooper_acquire")?,
            })
        }
    }
}

/// Whether a session wants vsyncs: the callback is re-posted only while set.
static WANTED: AtomicBool = AtomicBool::new(false);
/// Whether the thread was spawned (once per process).
static SPAWNED: AtomicBool = AtomicBool::new(false);
/// Whether a callback is posted and has not run yet. Touched on the thread only.
static OUTSTANDING: AtomicBool = AtomicBool::new(false);
/// Set by every callback, cleared by the thread: a callback arrived.
static FIRED: AtomicBool = AtomicBool::new(false);
/// The thread's looper, to wake it when a session starts; null until prepared
/// and again once the thread has ended. The thread takes a reference on it
/// (`ALooper_acquire`) that is never dropped, so a `start` that read the pointer
/// just before the thread ended still wakes a live object, not a freed one.
static LOOPER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// How long a posted callback may take before it is posted again: far beyond
/// any refresh period, so only a callback that was lost (or a display that is
/// off) reaches it.
const CALLBACK_TIMEOUT_MS: c_int = 1000;
/// The resolved table, set once by the thread before its first post.
static API: std::sync::OnceLock<Api> = std::sync::OnceLock::new();

/// Follow the display's vsync for a session. Spawns the thread on first use.
pub(crate) fn start() {
    WANTED.store(true, Ordering::Release);
    if SPAWNED.swap(true, Ordering::AcqRel) {
        let looper = LOOPER.load(Ordering::Acquire);
        if let (false, Some(api)) = (looper.is_null(), API.get()) {
            // SAFETY: the looper the thread prepared, kept alive by the
            // reference the thread took on it (see `LOOPER`).
            unsafe { (api.wake)(looper) };
        }
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("gndrm-vsync".into())
        .spawn(|| {
            let _ = std::panic::catch_unwind(run);
            // However the thread ended, the next session starts a new one.
            LOOPER.store(std::ptr::null_mut(), Ordering::Release);
            OUTSTANDING.store(false, Ordering::Relaxed);
            SPAWNED.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        SPAWNED.store(false, Ordering::Release);
    }
}

/// Stop following: the next callback is the last.
pub(crate) fn stop() {
    WANTED.store(false, Ordering::Release);
}

fn run() {
    let Some(api) = Api::load() else {
        crate::log_always!(
            "{}",
            gndrm_obf::obf!(
                "vsync: no Choreographer vsync callback here; flip events follow the latch grid"
            )
        );
        return;
    };
    let api = *API.get_or_init(|| api);
    // SAFETY: prepares (or returns) this thread's looper; Choreographer needs one.
    let looper = unsafe { (api.prepare)(0) };
    // SAFETY: called on a thread that has a looper, as the NDK requires.
    let ch = unsafe { (api.get_instance)() };
    if looper.is_null() || ch.is_null() {
        crate::log_always!(
            "{}",
            gndrm_obf::obf!("vsync: Choreographer unavailable; flip events follow the latch grid")
        );
        return;
    }
    // SAFETY: the looper this thread prepared; the reference is never
    // released (see `LOOPER`).
    unsafe { (api.acquire)(looper) };
    LOOPER.store(looper, Ordering::Release);
    // Whether a callback was posted again after a timeout and none has come
    // since: a callback that never comes is posted again once, not every
    // second, so a display that is off cannot pile them up.
    let mut reposted = false;
    let mut logged_lost = false;
    loop {
        if FIRED.swap(false, Ordering::Relaxed) {
            reposted = false;
        }
        if WANTED.load(Ordering::Acquire) && !OUTSTANDING.load(Ordering::Relaxed) {
            // SAFETY: `ch` is this thread's Choreographer; the callbacks are
            // `extern "C"` functions that live for the process and take no
            // user pointer.
            unsafe {
                match (api.vsync, api.frame64) {
                    (Some(v), _) => (v.post)(ch, on_vsync, std::ptr::null_mut()),
                    (None, Some(post)) => post(ch, on_frame, std::ptr::null_mut()),
                    (None, None) => {}
                }
            }
            OUTSTANDING.store(true, Ordering::Relaxed);
        }
        // SAFETY: this thread's looper; null out-pointers are allowed. Returns
        // after a callback ran, a wake, or the timeout.
        let r = unsafe {
            (api.poll_once)(
                if OUTSTANDING.load(Ordering::Relaxed) { CALLBACK_TIMEOUT_MS } else { -1 },
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        const ALOOPER_POLL_TIMEOUT: c_int = -3;
        if r == ALOOPER_POLL_TIMEOUT
            && OUTSTANDING.load(Ordering::Relaxed)
            && WANTED.load(Ordering::Acquire)
            && !reposted
        {
            // The callback was not delivered within a second. The flip
            // events fall back to the latch grid by themselves once the
            // vsync goes stale; post once more so the stream resumes if the
            // callback was lost rather than the display stopped.
            if !logged_lost {
                logged_lost = true;
                crate::log_always!(
                    "{}",
                    gndrm_obf::obf!(
                        "vsync: no Choreographer callback for 1 s; posting again, flip events follow the latch grid meanwhile"
                    )
                );
            }
            OUTSTANDING.store(false, Ordering::Relaxed);
            reposted = true;
        }
    }
}

/// API 33+: one vsync with its frame timelines.
unsafe extern "C" fn on_vsync(data: *const FrameData, _user: *mut c_void) {
    OUTSTANDING.store(false, Ordering::Relaxed);
    FIRED.store(true, Ordering::Relaxed);
    let _ = std::panic::catch_unwind(|| {
        let Some(v) = API.get().and_then(|a| a.vsync) else {
            return;
        };
        // SAFETY: `data` is valid for the duration of this callback.
        let (vsync, deadline) = unsafe {
            let idx = (v.preferred)(data);
            ((v.frame_time)(data), (v.deadline)(data, idx))
        };
        if vsync > 0 {
            let deadline = u64::try_from(deadline).ok().filter(|&d| d > 0);
            feed(vsync as u64, deadline);
        }
    });
}

/// API 29-32: the vsync time only.
unsafe extern "C" fn on_frame(frame_time_nanos: i64, _user: *mut c_void) {
    OUTSTANDING.store(false, Ordering::Relaxed);
    FIRED.store(true, Ordering::Relaxed);
    let _ = std::panic::catch_unwind(|| {
        if frame_time_nanos > 0 {
            feed(frame_time_nanos as u64, None);
        }
    });
}

fn feed(vsync_ns: u64, deadline_ns: Option<u64>) {
    if let Some(lock) = vblank::note_choreographer_vsync(vsync_ns, deadline_ns) {
        log_lock(&lock);
    }
    // Once per run of rejected fits, so not a per-frame line.
    if let Some((fit, mode)) = vblank::take_rejected_fit() {
        crate::log_always!(
            "{}",
            gndrm_obf::obf!(
                "vsync: measured period {}.{:03}us is more than 2% from the mode's {}.{:03}us; kept the previous period",
                fit / 1000,
                fit % 1000,
                mode / 1000,
                mode % 1000
            )
        );
    }
}

/// One line per lock: the first, and the first after each rate change. Not a
/// per-frame path, so it is ungated — this process never sees the guest's
/// `GNDRM_LOG_*`, and the line is what says the real vsync is in use.
fn log_lock(lock: &VsyncLock) {
    let p = lock.grid.period_ns;
    let mode_mhz = crate::panel_refresh_mhz();
    let rate_mhz = 1_000_000_000_000 / p.max(1);
    let offset = match lock.latch_offset_ns {
        Some(o) => gndrm_obf::obf!("{:+}us", o / 1000).to_string(),
        None => gndrm_obf::obf!("none yet").to_string(),
    };
    crate::log_always!(
        "{}",
        gndrm_obf::obf!(
            "vsync: flip events on the display's vsync: source={} period={}.{:03}us ({}.{:03} Hz, mode {}.{:03} Hz) latch-vs-grid={}",
            lock.grid.source.name(),
            p / 1000,
            p % 1000,
            rate_mhz / 1000,
            rate_mhz % 1000,
            mode_mhz / 1000,
            mode_mhz % 1000,
            offset
        )
    );
}

/// Signed nanoseconds as `+1.234ms`.
fn ms(ns: i64) -> String {
    let sign = if ns < 0 { '-' } else { '+' };
    let a = ns.unsigned_abs();
    gndrm_obf::obf!("{sign}{}.{:03}ms", a / 1_000_000, (a / 1000) % 1000).to_string()
}

/// The cross-check, once per 10 s while publishes complete (see
/// `vblank::DisplayVsync::note_publish`). Ungated for the same reason as the
/// lock line: it is what a device run reads to know the timeline holds.
pub(crate) fn log_report(r: &VsyncReport) {
    let p = r.period_ns;
    let rate_mhz = 1_000_000_000_000 / p.max(1);
    let lad = match r.latch_after_deadline_ns {
        Some(d) => ms(d),
        None => gndrm_obf::obf!("n/a").to_string(),
    };
    crate::log_always!(
        "{}",
        gndrm_obf::obf!(
            "vsync: check source={} period={}.{:03}us ({}.{:03} Hz) publishes={} vsyncs={} late={} pushed={} margin p10/p50/p90={}/{}/{} lead={} latch-after-deadline={}",
            r.source.map_or("none", |s| s.name()),
            p / 1000,
            p % 1000,
            rate_mhz / 1000,
            rate_mhz % 1000,
            r.publishes,
            r.vsyncs,
            r.late,
            r.pushed,
            ms(r.margin_ns[0]),
            ms(r.margin_ns[1]),
            ms(r.margin_ns[2]),
            ms(r.lead_ns as i64),
            lad
        )
    );
}
