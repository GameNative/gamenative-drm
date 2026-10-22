//! wayland WSI proxy for guest vulkan drivers that lack
//! VK_KHR_wayland_surface (the whole adrenotools turnip family: no wayland
//! platform compiled in, so no passthrough can ever present on GameScope's
//! wayland socket).
//!
//! The proxy advertises VK_KHR_wayland_surface itself and implements the
//! whole swapchain contract client-side. Nothing hooks-side may ever touch
//! a surface handle: the hooks' surface-query/swapchain trampolines assume
//! loader-wrapped surface objects their WSI never produces, so the
//! swapchain is a shim-owned ring of dma-buf images (system-heap fds
//! IMPORTED into vulkan) the app renders into directly.
//!
//! Backpressure: a slot is reusable only after the compositor's
//! wl_buffer.release, so presentation throttles to compositor rate.
//!
//! Handle ownership: the outer loader stamps its trampoline dispatch data
//! over the first words of the ICD's device handle after vkCreateDevice
//! returns (its terminator_CreateDevice stores into [handle]). The hooks'
//! exported device functions are `ldr x8,[x0]; br [x8,#slot]` stubs that
//! dispatch through that first word, so calling them on a stamped handle
//! reads the OUTER loader's table and lands back in this shim — mutual
//! recursion. The proxy therefore keeps every hooks handle private: the
//! loader only ever sees the shim's own wrapper objects (device, queue,
//! command buffer), each with a scratch prefix the loader may stamp; the
//! hooks' handles underneath stay pristine, so their stubs/trampolines
//! behave exactly as in a direct adrenotools load. Device functions the
//! proxy does not intercept are reached through per-name thunks that swap
//! the wrapper for the real handle and tail-branch to the hooks' export.

#![allow(non_snake_case)]

use crate::thunk;
use crate::vk::*;
use crate::wlproto::{self, wl_display, wl_proxy};
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

// First-N diagnostics for the present pipeline handoffs (acquire ->
// present -> worker commit -> compositor release). Bounded counters, so
// steady-state frame pacing stays silent in logcat.
static LOG_ACQ: AtomicUsize = AtomicUsize::new(0);
static LOG_PRESENT: AtomicUsize = AtomicUsize::new(0);
static LOG_SUBMIT: AtomicUsize = AtomicUsize::new(0);
// Always-on, bounded proof that the guest's submit/present reached the shim
// handlers at all. The `logf!` counters above are invisible in a real session
// (the guest env does not set GNDRM_LOG_VKSHIM); these two say "the path is
// live" without one line per frame.
static LOUD_SUBMIT: AtomicUsize = AtomicUsize::new(0);
static LOUD_SUBMIT2: AtomicUsize = AtomicUsize::new(0);
static LOUD_PRESENT: AtomicUsize = AtomicUsize::new(0);
static LOUD_INSTANCE: AtomicBool = AtomicBool::new(false);

// --------------------------------------- last-calls ring (bounded, gated off)
//
// A tiny per-thread history of the last entry points the guest invoked, so
// the call the guest made just before its threads park is named without a
// diagnostic env knob. Always recording (cheap: one push per intercepted
// call), but only *printed* once — when a stall is suspected — so a healthy
// session stays silent. The dump uses `logf!` (GNDRM_LOG_VKSHIM-gated, off by
// default): a session that wants the ring turns the gate on.
struct RingEnt {
    tid: u32,
    name: &'static str,
    args: [usize; 5],
}
const RING_CAP: usize = 48;
static RING: Mutex<Vec<RingEnt>> = Mutex::new(Vec::new());
static RING_DUMPED: AtomicBool = AtomicBool::new(false);

fn ring_tid() -> u32 {
    // gettid(2): the kernel thread id, matching what `wchan` dumps show.
    unsafe { libc::gettid() as u32 }
}

fn ring_push(name: &'static str, args: [usize; 5]) {
    let Ok(mut r) = RING.lock() else {
        return;
    };
    if r.len() == RING_CAP {
        r.remove(0);
    }
    r.push(RingEnt {
        tid: ring_tid(),
        name,
        args,
    });
}

/// Print the ring once (bounded one-shot) with the reason the stall is
/// suspected. The tail of the ring is the park point.
fn ring_dump(reason: &str) {
    if RING_DUMPED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Ok(r) = RING.lock() else {
        return;
    };
    crate::logf!(gndrm_obf::obf!(
        "x11 stall ring ({reason}): last {} entry points:",
        r.len()
    ));
    let n = r.len();
    for (i, e) in r.iter().enumerate() {
        crate::logf!(gndrm_obf::obf!(
            "x11 stall ring [{}/{}] tid={} {} args={:x} {:x} {:x} {:x} {:x}",
            i + 1,
            n,
            e.tid,
            e.name,
            e.args[0],
            e.args[1],
            e.args[2],
            e.args[3],
            e.args[4]
        ));
    }
}
static LOG_COMMIT: AtomicUsize = AtomicUsize::new(0);

/// Presents between game-window tree walks (see `maybe_report_game_window`).
/// The splash-clear transition is one-shot, so a walk every `N` presents — a
/// handful of milliseconds of latency — is ample, and it keeps a per-frame X
/// round trip out of the present path.
const GAME_WINDOW_CHECK_EVERY: u32 = 120;
/// Walks to attempt before giving up: minutes of presents, so a guest that
/// spends its first boot compiling shaders behind a loader window is still
/// seen when its own window finally maps. A window that is not identifiable
/// after this many walks (the guest never maps one, or it maps one the
/// predicate cannot classify) will not become identifiable later, and each
/// walk is an X11 round trip on the present path.
const GAME_WINDOW_GIVE_UP: u32 = 240;
/// Presents counted; a walk runs on every `GAME_WINDOW_CHECK_EVERY`th.
static GAME_WINDOW_CHECK: AtomicU32 = AtomicU32::new(0);
/// Set once the game-window notification has been sent; stops all further
/// walks, so a signalled or idle session costs nothing.
static GAME_WINDOW_REPORTED: AtomicBool = AtomicBool::new(false);
static LOG_RELEASE: AtomicUsize = AtomicUsize::new(0);
const LOG_FIRST_N: usize = 8;

// Swapchain handshake liveness. The stall class that matters — the app stops
// presenting, or the compositor stops releasing — is invisible under first-N
// budgets, so these sites are RATE-LIMITED instead of budgeted, and the idle
// present worker prints the same line as a heartbeat. Every line carries the
// per-slot flags, so a frozen pipeline always leaves a trail.
static N_ACQ: AtomicUsize = AtomicUsize::new(0);
static N_PRES: AtomicUsize = AtomicUsize::new(0);
static N_REL: AtomicUsize = AtomicUsize::new(0);
static N_COMMIT_OK: AtomicUsize = AtomicUsize::new(0);
static N_STARVE: AtomicUsize = AtomicUsize::new(0);
/// One named line the first time the X11 route drops a frame because no
/// present could be made; later drops stay silent (a route that is down would
/// otherwise print once per frame).
static X11_DROP_LOGGED: AtomicBool = AtomicBool::new(false);
/// Bounded one-shot trace of the guest's swapchain acquire/present indices and
/// the slot content each present carries. Always on (not gated by a diagnostic
/// env var) but bounded to the first `X11_TRACE_N` presents of each swapchain,
/// so it names why a session shows nothing without spamming a healthy one.
static ACQ_SEQ: AtomicUsize = AtomicUsize::new(0);
// Acquire-dwell census, in microseconds, reported on the commit line. The
// counters are incremented before any log call, so the values are not inflated
// by logging; reading them does need GNDRM_LOG_VKSHIM=1, because the commit
// line is itself logf!-gated. Microseconds because the signal being chased (a
// missed 8.33 ms vblank) is sub-ms at the acquire itself: a 0.5 ms wait is
// invisible at millisecond resolution.
static ACQ_US_N: AtomicUsize = AtomicUsize::new(0);
static ACQ_US_SUM: AtomicUsize = AtomicUsize::new(0);
static ACQ_US_MAX: AtomicUsize = AtomicUsize::new(0);
static ACQ_US_NONZERO: AtomicUsize = AtomicUsize::new(0);
static ACQ_GE_8MS: AtomicUsize = AtomicUsize::new(0); // >= 8333 us: one missed 120 Hz vblank
static ACQ_GE_16MS: AtomicUsize = AtomicUsize::new(0); // >= 16667 us: two
                                                       // Present-to-present interval census, in microseconds — the same shape and the
                                                       // same reasoning as the acquire dwell above, reported on the same commit line.
                                                       // This is the guest's own frame period as the shim sees it: the client <->
                                                       // gamescope lap that doc 12 leaves unmeasured end to end, so a hard throttle
                                                       // and plain jitter are told apart by the tail shares, not just the mean (a
                                                       // throttle parks the mass at one packet time; jitter spreads it). Counters are
                                                       // bumped before any log call, so they are honest without the gate; reading them
                                                       // needs GNDRM_LOG_VKSHIM=1 only because the commit line is itself logf!-gated.
                                                       // The delta needs the previous present's stamp, so one mutex touch per present
                                                       // — no formatting, no allocation, on the hot path. us, not ms: the signal is
                                                       // sub-ms and disappears at millisecond resolution.
static PRES_IV_N: AtomicUsize = AtomicUsize::new(0);
static PRES_IV_SUM_US: AtomicUsize = AtomicUsize::new(0);
static PRES_IV_MIN_US: AtomicUsize = AtomicUsize::new(usize::MAX);
static PRES_IV_MAX_US: AtomicUsize = AtomicUsize::new(0);
static PRES_IV_GE_16667: AtomicUsize = AtomicUsize::new(0); // >= 16.67 ms: a missed 60 Hz frame
static PRES_IV_GE_33333: AtomicUsize = AtomicUsize::new(0); // >= 33.3 ms: two
static PRES_IV_LAST: Mutex<Option<Instant>> = Mutex::new(None);
// Release-floor census, in microseconds, the same shape as the acquire dwell
// and the present interval above and reported on the same lines. A release is
// held when it arrives sooner than one target period after this shim's own
// previous release, so `REL_HOLD_N` counts the releases the floor actually
// delayed, `REL_HOLD_SUM_US` is the total time they were held — the pacing the
// cap added — and the threshold bucket separates a floor that bit hard from the
// sub-millisecond residue of one that had almost elapsed. Read against
// `REL_CAPPED_N` (releases seen while a target was set) and `cap=[fps=…
// notes=…]`, which is what tells "the target never arrived" (notes=0) from "a
// target arrived and the floor never bound" (notes>0, capped_rel>0, held=0).
static REL_HOLD_N: AtomicUsize = AtomicUsize::new(0);
static REL_HOLD_SUM_US: AtomicUsize = AtomicUsize::new(0);
static REL_HOLD_MAX_US: AtomicUsize = AtomicUsize::new(0);
static REL_HOLD_GE_8333: AtomicUsize = AtomicUsize::new(0); // >= 8.33 ms: one 120 Hz frame
/// Releases seen while a target was set, held or not: the denominator the
/// `held=` count is read against.
static REL_CAPPED_N: AtomicUsize = AtomicUsize::new(0);

/// Record the interval since the previous present. Called once per present at
/// entry; the first present only seeds the stamp, so the delta count is
/// presents - 1 per process.
fn pres_iv_note(now: Instant) {
    let Ok(mut g) = PRES_IV_LAST.lock() else {
        return;
    };
    if let Some(prev) = *g {
        let us = now.duration_since(prev).as_micros() as usize;
        PRES_IV_N.fetch_add(1, Ordering::Relaxed);
        PRES_IV_SUM_US.fetch_add(us, Ordering::Relaxed);
        PRES_IV_MIN_US.fetch_min(us, Ordering::Relaxed);
        PRES_IV_MAX_US.fetch_max(us, Ordering::Relaxed);
        if us >= 16_667 {
            PRES_IV_GE_16667.fetch_add(1, Ordering::Relaxed);
        }
        if us >= 33_333 {
            PRES_IV_GE_33333.fetch_add(1, Ordering::Relaxed);
        }
    }
    *g = Some(now);
}

/// The minimum interval, or 0 while no delta exists — the initialised sentinel
/// would otherwise print as a huge number.
fn pres_iv_min_us() -> usize {
    let m = PRES_IV_MIN_US.load(Ordering::Relaxed);
    if m == usize::MAX {
        0
    } else {
        m
    }
}
const X11_TRACE_N: usize = 24;
/// The two-view experiment (driver copy vs shim mmap) is heavier per present,
/// so it is bounded to the first few presents only.
const X11_VIEW_N: u32 = 3;
static RATE_STALL: Mutex<Option<Instant>> = Mutex::new(None);
static RATE_WAIT: Mutex<Option<Instant>> = Mutex::new(None);
/// The last (semaphore, value) the guest asked us to *signal*. A GPU wait for a
/// value the driver never reaches shows up as `want` > `last_signalled`.
static LAST_SIG: Mutex<Option<(String, u64)>> = Mutex::new(None);
static RATE_REL: Mutex<Option<Instant>> = Mutex::new(None);
static RATE_IDLE: Mutex<Option<Instant>> = Mutex::new(None);
/// The surface-format list the guest is served (once a second at most).
/// WSI entry-point call counters, printed once a second by [`wsi_hb`]. A client
/// stuck in its own present loop still shows up here as the calls it keeps
/// making, which is the one thing a driver-side trace cannot show.
static HB_FMT: AtomicUsize = AtomicUsize::new(0);
static HB_FMT2: AtomicUsize = AtomicUsize::new(0);
static HB_CAPS: AtomicUsize = AtomicUsize::new(0);
static HB_CAPS2: AtomicUsize = AtomicUsize::new(0);
static HB_PMODES: AtomicUsize = AtomicUsize::new(0);
static HB_CREATE: AtomicUsize = AtomicUsize::new(0);
static HB_DESTROY: AtomicUsize = AtomicUsize::new(0);
static HB_ACQ: AtomicUsize = AtomicUsize::new(0);
static HB_PRESENT: AtomicUsize = AtomicUsize::new(0);
static HB_IMAGES: AtomicUsize = AtomicUsize::new(0);
/// Heartbeat gate as raw monotonic milliseconds. The heartbeat runs on the
/// present path, so it must not take a lock: a `Mutex<Option<Instant>>` there
/// competed with the swapchain state for the same cache lines every frame.
static RATE_HB_MS: AtomicU64 = AtomicU64::new(0);
/// Process-relative origin for the heartbeat's raw-millisecond gate.
static PROC_EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Print the WSI call counters at most once a second. Cumulative, so two
/// consecutive lines differ by the calls made in between.
fn wsi_hb() {
    let epoch = *PROC_EPOCH.get_or_init(Instant::now);
    let ms = epoch.elapsed().as_millis() as u64;
    let prev = RATE_HB_MS.load(Ordering::Relaxed);
    if ms.saturating_sub(prev) < 1000 {
        return;
    }
    if RATE_HB_MS
        .compare_exchange(prev, ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    crate::logf!(gndrm_obf::obf!(
        "proxy hb: fmt={} fmt2={} caps={} caps2={} pmodes={} create={} destroy={} images={} acq={} present={}",
        HB_FMT.load(Ordering::Relaxed),
        HB_FMT2.load(Ordering::Relaxed),
        HB_CAPS.load(Ordering::Relaxed),
        HB_CAPS2.load(Ordering::Relaxed),
        HB_PMODES.load(Ordering::Relaxed),
        HB_CREATE.load(Ordering::Relaxed),
        HB_DESTROY.load(Ordering::Relaxed),
        HB_IMAGES.load(Ordering::Relaxed),
        HB_ACQ.load(Ordering::Relaxed),
        HB_PRESENT.load(Ordering::Relaxed),
    ));
}
const RATE_EVERY: Duration = Duration::from_secs(1);
const IDLE_EVERY: Duration = Duration::from_secs(2);

fn rate_ok(slot: &'static Mutex<Option<Instant>>, every: Duration) -> bool {
    let Ok(mut g) = slot.lock() else {
        return false;
    };
    let now = Instant::now();
    if let Some(prev) = *g {
        if now.duration_since(prev) < every {
            return false;
        }
    }
    *g = Some(now);
    true
}

/// The mean hold per delayed release, or 0 while none has been delayed — the
/// aggregate `held_us=` divided by `held=`.
fn held_avg_us() -> usize {
    REL_HOLD_SUM_US
        .load(Ordering::Relaxed)
        .checked_div(REL_HOLD_N.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// "0:-F 1:A- …": A = held by the app, F = committed and awaiting release.
fn slot_flags(sc: *mut GnSwapchain) -> String {
    let mut flags = String::new();
    let mut in_flight = 0usize;
    let Ok(g) = (unsafe { (*sc).shared.lock() }) else {
        return String::from("<locked>");
    };
    for (i, s) in unsafe { (*sc).slots.iter() }.enumerate() {
        if s.in_flight {
            in_flight += 1;
        }
        flags.push_str(&format!(
            "{i}:{}{} ",
            if s.acquired { "A" } else { "-" },
            if s.in_flight { "F" } else { "-" }
        ));
    }
    drop(g);
    format!(
        "{}",
        gndrm_obf::obf!(
            "acq={} pres={} rel={} commit={} starved={} in_flight={in_flight} slots=[{}] acqus=[n={} sum={} max={} nz={} ge8={} ge16={}] presiv=[n={} sum={} min={} max={} ge16667={} ge33333={}] cap=[fps={} notes={} capped_rel={} held={} held_us={} held_avg_us={} held_max_us={} held_ge8333={}]",
            N_ACQ.load(Ordering::Relaxed),
            N_PRES.load(Ordering::Relaxed),
            N_REL.load(Ordering::Relaxed),
            N_COMMIT_OK.load(Ordering::Relaxed),
            N_STARVE.load(Ordering::Relaxed),
            flags.trim_end(),
                ACQ_US_N.load(Ordering::Relaxed),
                ACQ_US_SUM.load(Ordering::Relaxed),
                ACQ_US_MAX.load(Ordering::Relaxed),
                ACQ_US_NONZERO.load(Ordering::Relaxed),
                ACQ_GE_8MS.load(Ordering::Relaxed),
                ACQ_GE_16MS.load(Ordering::Relaxed),
                PRES_IV_N.load(Ordering::Relaxed),
                PRES_IV_SUM_US.load(Ordering::Relaxed),
                pres_iv_min_us(),
                PRES_IV_MAX_US.load(Ordering::Relaxed),
                PRES_IV_GE_16667.load(Ordering::Relaxed),
                PRES_IV_GE_33333.load(Ordering::Relaxed),
                crate::scanout::target_fps(),
                crate::scanout::notes_seen(),
                REL_CAPPED_N.load(Ordering::Relaxed),
                REL_HOLD_N.load(Ordering::Relaxed),
                REL_HOLD_SUM_US.load(Ordering::Relaxed),
                held_avg_us(),
                REL_HOLD_MAX_US.load(Ordering::Relaxed),
                REL_HOLD_GE_8333.load(Ordering::Relaxed),
        )
    )
}

/// Counters only — safe to call while `shared` is held.
fn counters() -> String {
    format!(
        "{}",
        gndrm_obf::obf!(
            "acq={} pres={} rel={} commit={} starved={} acqus=[n={} sum={} max={} nz={} ge8={} ge16={}] presiv=[n={} sum={} min={} max={} ge16667={} ge33333={}] cap=[fps={} notes={} capped_rel={} held={} held_us={} held_avg_us={} held_max_us={} held_ge8333={}]",
            N_ACQ.load(Ordering::Relaxed),
            N_PRES.load(Ordering::Relaxed),
            N_REL.load(Ordering::Relaxed),
            N_COMMIT_OK.load(Ordering::Relaxed),
            N_STARVE.load(Ordering::Relaxed),
                ACQ_US_N.load(Ordering::Relaxed),
                ACQ_US_SUM.load(Ordering::Relaxed),
                ACQ_US_MAX.load(Ordering::Relaxed),
                ACQ_US_NONZERO.load(Ordering::Relaxed),
                ACQ_GE_8MS.load(Ordering::Relaxed),
                ACQ_GE_16MS.load(Ordering::Relaxed),
                PRES_IV_N.load(Ordering::Relaxed),
                PRES_IV_SUM_US.load(Ordering::Relaxed),
                pres_iv_min_us(),
                PRES_IV_MAX_US.load(Ordering::Relaxed),
                PRES_IV_GE_16667.load(Ordering::Relaxed),
                PRES_IV_GE_33333.load(Ordering::Relaxed),
                crate::scanout::target_fps(),
                crate::scanout::notes_seen(),
                REL_CAPPED_N.load(Ordering::Relaxed),
                REL_HOLD_N.load(Ordering::Relaxed),
                REL_HOLD_SUM_US.load(Ordering::Relaxed),
                held_avg_us(),
                REL_HOLD_MAX_US.load(Ordering::Relaxed),
                REL_HOLD_GE_8333.load(Ordering::Relaxed),
        )
    )
}

/// Serializes every send-side wl_connection access (request marshaling +
/// wl_display_flush). libwayland does not internally lock the out-buffer;
/// concurrent send-side access from the present worker and the dispatch
/// thread's internal pre-poll flush corrupts the buffer state, which makes
/// a marshal's inline flush fail EAGAIN and wl_display_fatal() latch the
/// display error — after which wl_display_flush returns -1 forever without
/// sending anything (the "commits never reach the compositor" stall).
static SEND_LOCK: Mutex<()> = Mutex::new(());

/// Routes libwayland's own diagnostics (e.g. "Error marshalling request for
/// X.Y") to logcat; the default handler writes to stderr, which the wine
/// process does not surface.
extern "C" fn gn_wl_log(fmt: *const c_char, ap: *mut c_void) {
    let mut buf = [0 as c_char; 512];
    unsafe { wlproto::vsnprintf(buf.as_mut_ptr(), buf.len(), fmt, ap) };
    let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    crate::logf!(gndrm_obf::obf!("libwayland: {}", msg.trim_end()));
}

// dma-buf sync ioctl (same values as the daemon's EGL presenter).
const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_START: u64 = 0 << 2;
const DMA_BUF_SYNC_END: u64 = 1 << 2;
const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;

/// Bounded one-shot: a cheap content signature of a LINEAR slot, read on the CPU
/// (alpha=0 pixel count, non-zero byte count, first pixel RGBA). Used by the
/// always-on acquire/present trace so an all-zero present can be attributed to
/// the guest writing nothing rather than to the shim presenting a stale slot.
unsafe fn slot_signature(sc: *mut GnSwapchain, s: *mut GnSlot) -> (usize, usize, [u8; 4]) {
    unsafe {
        if (*s).modifier != GN_DRM_MOD_LINEAR || (*s).fd < 0 {
            return (0, 0, [0; 4]);
        }
        let w = (*sc).extent.width as usize;
        let h = (*sc).extent.height as usize;
        let stride = (*s).stride as usize;
        let fd = (*s).fd;
        let len = stride * h;
        let base = libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if base == libc::MAP_FAILED {
            return (0, 0, [0; 4]);
        }
        let begin = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_START;
        libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &begin);
        let p = (base as *const u8).add((*s).offset as usize);
        let mut a0 = 0usize;
        let mut nz = 0usize;
        for y in 0..h {
            for x in 0..w {
                let q = p.add(y * stride + x * 4);
                let px = [*q, *q.add(1), *q.add(2), *q.add(3)];
                if px[3] == 0 {
                    a0 += 1;
                }
                if px != [0, 0, 0, 0] {
                    nz += 1;
                }
            }
        }
        let first = [*p, *p.add(1), *p.add(2), *p.add(3)];
        let end = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END;
        libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &end);
        libc::munmap(base, len);
        (a0, nz, first)
    }
}

/// Bounded one-shot diagnostic: read the slot's *imported* device memory through
/// `vkMapMemory` (the driver's own mapping of the same dma-buf), using the
/// image's reported stride/offset. Independent of the shim's CPU mmap.
/// Returns (alpha0, nonzero, first-pixel).
unsafe fn slot_mem_signature(
    sc: *mut GnSwapchain,
    s: *mut GnSlot,
) -> Option<(usize, usize, [u8; 4])> {
    let gd = (*sc).dev;
    if gd.is_null() || (*s).mem == 0 {
        return None;
    }
    let dev = (*gd).real;
    let map = (*gd).f.map_mem?;
    let unmap = (*gd).f.unmap_mem?;
    let mut p: *mut c_void = ptr::null_mut();
    if unsafe { map(dev, (*s).mem, 0, VK_WHOLE_SIZE, 0, &mut p) } != VK_SUCCESS || p.is_null() {
        return None;
    }
    let base = p.cast::<u8>();
    let w = (*sc).extent.width as usize;
    let h = (*sc).extent.height as usize;
    let stride = (*s).stride as usize;
    let off = (*s).offset as usize;
    let mut a0 = 0usize;
    let mut nz = 0usize;
    for y in 0..h {
        for x in 0..w {
            let q = unsafe { base.add(off + y * stride + x * 4) };
            let px = unsafe { [*q, *q.add(1), *q.add(2), *q.add(3)] };
            if px[3] == 0 {
                a0 += 1;
            }
            if px != [0, 0, 0, 0] {
                nz += 1;
            }
        }
    }
    let first = unsafe {
        [
            *base.add(off),
            *base.add(off + 1),
            *base.add(off + 2),
            *base.add(off + 3),
        ]
    };
    unsafe { unmap(dev, (*s).mem) };
    Some((a0, nz, first))
}

/// X11 route: make the slot's dma-buf the window's content with DRI3 + Present.
/// The pixmap and idle fence are created once per slot (worker thread); the
/// slot stays in flight until the fence triggers (see `x11_reap`). Every
/// failure is named and leaves the slot free, so the guest never waits on a
/// frame that cannot be shown.
unsafe fn x11_present_slot(sc: *mut GnSwapchain, s: *mut GnSlot, idx: i32) -> bool {
    let present = (*(*sc).surf).x11;
    if present.is_null() {
        return false;
    }
    // The caller has already waited the slot's own fence, and that fence is
    // signalled by the shim's present submit, which waits the guest's present
    // semaphores (see `gn_vkQueuePresentKHR`). So the guest's render into this
    // slot has completed before the slot is handed to Xwayland or read on the
    // CPU here: no whole-device drain is needed, and taking one would destroy
    // all CPU/GPU overlap on the present path.
    // Bounded one-shot (always on): record the presented index, the acquire
    // sequence that filled the slot, and the slot's content signature after the
    // GPU is idle. An all-zero signature here means the guest presented a frame
    // whose contents are empty; a non-zero one means the guest did write.
    let trace_n = unsafe { (*sc).x11_trace };
    if (trace_n as usize) < X11_TRACE_N {
        unsafe { (*sc).x11_trace = trace_n + 1 };
        let (a0, nz, px) = unsafe { slot_signature(sc, s) };
        let acq = unsafe { (*s).acq_seq };
        crate::logf!(gndrm_obf::obf!(
            "x11 trace {trace_n}: sc={sc:?} idx={idx} acq#{acq} {}x{} alpha0={a0}/{} nz={nz} px0=({},{},{},{})",
            unsafe { (*sc).extent.width },
            unsafe { (*sc).extent.height },
            unsafe { (*sc).extent.width as usize * (*sc).extent.height as usize },
            px[0],
            px[1],
            px[2],
            px[3]
        ));
        // Bounded one-shot two-view probe (first presents): the shim's own
        // linear mmap above vs the driver's own mapping of the same dma-buf
        // (vkMapMemory of the slot's imported memory). They only differ if the
        // image's real layout/stride is not what the shim advertises/reads.
        if trace_n < X11_VIEW_N {
            if let Some((va0, vnz, vpx)) = unsafe { slot_mem_signature(sc, s) } {
                crate::logf!(gndrm_obf::obf!(
                    "x11 view {trace_n}: idx={idx} driver(mapmem) alpha0={va0}/{} nz={vnz} px0=({},{},{},{})",
                    unsafe { (*sc).extent.width as usize * (*sc).extent.height as usize },
                    vpx[0],
                    vpx[1],
                    vpx[2],
                    vpx[3]
                ));
            }
        }
    }
    if unsafe { (*s).xpix } == 0 {
        match unsafe {
            (*present).make_pixmap(
                (*s).fd,
                (*sc).extent.width,
                (*sc).extent.height,
                (*s).stride,
                (*s).modifier,
            )
        } {
            Some(p) => unsafe { (*s).xpix = p },
            None => {
                crate::logf!(gndrm_obf::obf!(
                    "x11: slot {idx} has no pixmap - frame dropped"
                ));
                return false;
            }
        }
    }
    if unsafe { (*s).xfence } == 0 {
        match unsafe { (*present).make_fence() } {
            Some(f) => unsafe { (*s).xfence = f },
            None => return false,
        }
    }
    match unsafe { (*present).present((*s).xpix, (*s).xfence) } {
        Some(serial) => {
            let n = LOG_COMMIT.fetch_add(1, Ordering::Relaxed);
            if n < LOG_FIRST_N || n.is_multiple_of(120) {
                crate::logf!(gndrm_obf::obf!(
                    "x11 present: sc={sc:?} idx={idx} pix={} serial={serial} {}x{} {}",
                    unsafe { (*s).xpix },
                    unsafe { (*sc).extent.width },
                    unsafe { (*sc).extent.height },
                    counters()
                ));
            }
        }
        None => {
            crate::logf!(gndrm_obf::obf!(
                "x11: slot {idx} present refused - frame dropped"
            ));
            return false;
        }
    }
    true
}

/// Walk the bound window at a fraction of the present cadence and tell the
/// daemon (once) when the game's own window appears, so it can clear the
/// booting splash on that event instead of on the first published frame.
/// A no-op once reported, so an idle or already-signalled session costs
/// nothing; the walk itself is bounded and never hangs.
///
/// Reached from BOTH present routes, at the same N-present cadence, so the
/// override/Wayland route (the default) runs the check too (it previously sat
/// behind the DRI3-only X11 present and never fired on the shipping
/// configuration):
///  - guest-surface route (the opt-in; `surf.x11` set): the present's own live
///    connection, with `surf.window` — the guest's own window — as the bound
///    window (see [`crate::x11::window_present`]).
///  - override route (the default; `surf.x11` null): `surf.window` is the window
///    the override resolved gamescope will paint; the walk opens its own
///    connection (see [`crate::x11::game_window_present`]). Rides the cadence,
///    never the per-frame present path.
unsafe fn maybe_report_game_window(surf: *mut GnSurface) {
    if GAME_WINDOW_REPORTED.load(Ordering::Relaxed) {
        return;
    }
    let n = GAME_WINDOW_CHECK.fetch_add(1, Ordering::Relaxed);
    if !n.is_multiple_of(GAME_WINDOW_CHECK_EVERY) || n / GAME_WINDOW_CHECK_EVERY >= GAME_WINDOW_GIVE_UP {
        return;
    }
    let present = unsafe { (*surf).x11 };
    let bound = unsafe { (*surf).window };
    let game_window_up = if present.is_null() {
        unsafe { crate::x11::game_window_present(bound) }
    } else {
        unsafe { (*present).window_present(bound) }
    };
    if game_window_up {
        GAME_WINDOW_REPORTED.store(true, Ordering::Relaxed);
        crate::scanout::report_game_window_present();
    }
}

/// Frees every X11 slot whose idle fence has triggered - the server is done
/// with its pixmap, the slot-reuse contract the wayland arm gets from
/// wl_buffer.release.
unsafe fn x11_reap(sc: *mut GnSwapchain) {
    let present = (*(*sc).surf).x11;
    if present.is_null() {
        return;
    }
    for i in 0..unsafe { (*sc).slots.len() } {
        let s = unsafe { (*sc).slots.as_mut_ptr().add(i) };
        // in_flight is the app/worker handshake field: read and clear it under
        // the same lock the wayland release listener uses.
        let (fence, busy) = {
            let _g = unsafe { (*sc).shared.lock().unwrap() };
            unsafe { ((*s).xfence, (*s).in_flight) }
        };
        if fence == 0 || !busy {
            continue;
        }
        if unsafe { (*s).release_held } {
            // Already returned; waiting for the paced floor. `acquire_take_free`
            // frees it when the floor passes, so re-arming here would advance the
            // schedule once per reap for the same release.
            continue;
        }
        if unsafe { (*present).fence_idle(fence) } {
            // Same floor the wayland arm applies on `wl_buffer.release`: the
            // X11 route has no release listener, so a gamescope session would
            // otherwise see no pacing at all.
            let now_ns = monotonic_ns();
            // The X11 route only ever reaps guest slots.
            let floor_ns = crate::scanout::note_release(now_ns, true);
            let _g = unsafe { (*sc).shared.lock().unwrap() };
            if now_ns < floor_ns {
                unsafe {
                    (*s).floor_ns = floor_ns;
                    (*s).release_held = true;
                }
            } else {
                unsafe {
                    (*s).in_flight = false;
                    (*sc).cv_free.notify_all();
                }
            }
            drop(_g);
            N_REL.fetch_add(1, Ordering::Relaxed);
            if rate_ok(&RATE_REL, RATE_EVERY) {
                crate::logf!(gndrm_obf::obf!("x11 release: slot={i} {}", slot_flags(sc)));
            }
        }
    }
}

/// One image barrier for the capture round trip.
fn lsfg_barrier(
    image: VkImage,
    src_access: VkAccessFlags,
    dst_access: VkAccessFlags,
    old: VkImageLayout,
    new: VkImageLayout,
) -> VkImageMemoryBarrier {
    let mut b: VkImageMemoryBarrier = unsafe { std::mem::zeroed() };
    b.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER;
    b.srcAccessMask = src_access;
    b.dstAccessMask = dst_access;
    b.oldLayout = old;
    b.newLayout = new;
    b.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    b.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    b.image = image;
    b.subresourceRange.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
    b.subresourceRange.levelCount = 1;
    b.subresourceRange.layerCount = 1;
    b
}

/// Commit a slot the present path did not produce (a generation destination).
/// The caller has already waited the submission that wrote it.
unsafe fn commit_generated(sc: *mut GnSwapchain, slot_index: usize) {
    let s = unsafe { (*sc).slots.as_mut_ptr().add(slot_index) };
    {
        let _g = unsafe { (*sc).shared.lock().unwrap() };
        unsafe {
            (*s).in_flight = true;
            (*s).acquired = false;
        }
    }
    if unsafe { (*(*sc).surf).parent.is_null() } {
        unsafe { x11_present_slot(sc, s, slot_index as i32) };
        return;
    }
    if unsafe { (*s).buf.is_null() } {
        unsafe { (*s).buf = slot_make_buffer(sc, s) };
    }
    if unsafe { (*s).buf.is_null() } {
        let _g = unsafe { (*sc).shared.lock().unwrap() };
        unsafe {
            (*s).in_flight = false;
            (*sc).cv_free.notify_all();
        }
        return;
    }
    let parent = unsafe { (*(*sc).surf).parent };
    let _sg = SEND_LOCK.lock().unwrap();
    unsafe {
        wlproto::surface_attach(parent, (*s).buf, 0, 0);
        wlproto::surface_damage_buffer(
            parent,
            0,
            0,
            (*sc).extent.width as i32,
            (*sc).extent.height as i32,
        );
        wlproto::surface_commit(parent);
        wlproto::wl_display_flush((*(*sc).surf).dpy);
    }
}

/// The device's generation session, opened or replaced when the settings call
/// for a different factor.
///
/// The app's settings arrive through the daemon after the device is created, so
/// the session cannot be opened at create time. An attempt is made once per
/// settings change: a failure (no DLL, unreadable cache, engine refused) is not
/// retried on every frame, and a change of multiplier replaces the engine
/// context, which is fixed at creation.
unsafe fn lsfg_session(gd: *mut GnDevice) -> *mut crate::lsfg::Session {
    let wanted = crate::lsfg::wanted();
    let mut current = unsafe { (*gd).lsfg };
    if !current.is_null() && wanted == Some(unsafe { (*current).multiplier() }) {
        return current;
    }
    if !current.is_null() {
        unsafe { (*current).close((*gd).real, &(*gd).f) };
        drop(unsafe { Box::from_raw(current) });
        unsafe { (*gd).lsfg = ptr::null_mut() };
        current = ptr::null_mut();
    }
    if wanted.is_none() {
        return current;
    }
    let epoch = crate::lsfg::epoch();
    if epoch == unsafe { (*gd).lsfg_epoch } {
        return current;
    }
    unsafe { (*gd).lsfg_epoch = epoch };
    let gipa = G.lock().unwrap().real_gipa;
    let opened = unsafe {
        crate::lsfg::Session::open((*(*gd).inst).real, (*gd).pd, (*gd).real, gipa, (*gd).fp16)
    };
    match opened {
        Some(session) => {
            let ptr = Box::into_raw(Box::new(session));
            unsafe { (*gd).lsfg = ptr };
            ptr
        }
        None => ptr::null_mut(),
    }
}

/// Grow the generation destinations so `count` of them exist.
///
/// The swapchain's capacity for them is reserved at creation, so appending
/// cannot move the guest's slots — the guest only ever indexes below
/// `guest_slots`, which is what makes this safe to do from the present worker
/// while the guest is acquiring.
unsafe fn lsfg_destinations(sc: *mut GnSwapchain, count: usize) -> bool {
    let guest = unsafe { (*sc).guest_slots as usize };
    if unsafe { (*sc).gen_no_grow } {
        return unsafe { (*sc).slots.len() } > guest;
    }
    const GEN_USAGE: VkImageUsageFlags = VK_IMAGE_USAGE_TRANSFER_DST_BIT
        | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT
        | VK_IMAGE_USAGE_SAMPLED_BIT;
    while unsafe { (*sc).slots.len() } < guest + count {
        let index = unsafe { (*sc).slots.len() };
        unsafe {
            (*sc).slots.push(GnSlot {
                owner: sc,
                image: 0,
                mem: 0,
                fd: -1,
                stride: 0,
                offset: 0,
                modifier: 0,
                buf: ptr::null_mut(),
                xpix: 0,
                xfence: 0,
                fence: 0,
                sem: 0,
                acquired: false,
                in_flight: false,
                acq_seq: 0,
                ino: 0,
                floor_ns: 0,
                release_held: false,
                ahb: ptr::null_mut(),
            });
        }
        let res =
            unsafe { slot_init_with_usage(sc, (*sc).slots.as_mut_ptr().add(index), GEN_USAGE) };
        if res != VK_SUCCESS {
            crate::logf!(gndrm_obf::obf!("lsfg: destination slot init failed: {res}"));
            // The half-made slot must not stay in the table: every later pass over
            // the slots would take it for a real one. And no further growth: the
            // allocator refused once, so the set stays at what it has.
            unsafe {
                slot_release(sc, (*sc).slots.as_mut_ptr().add(index));
                (*sc).slots.pop();
                (*sc).gen_no_grow = true;
            }
            return unsafe { (*sc).slots.len() } > guest;
        }
    }
    true
}

/// Serialises every submit the shim makes or forwards on the queues it shares
/// with the guest: the guest's `vkQueueSubmit`/`vkQueueSubmit2`, the acquire and
/// present-wait submits, and the generation batch from the present worker. A
/// queue needs external synchronisation, and any two of these can run on
/// different threads at once; an overlap loses a submission, its fences never
/// signal, and the guest waits on them forever.
static SUBMIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Counts for the pass: how many generated frames were committed, and how many
/// frames were skipped because no destination was free.
static GEN_COMMITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static GEN_SKIPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Releases seen for generation destinations: the compositor taking the buffer
/// back. Compared with [`GEN_COMMITS`], it separates "presented" from
/// "superseded".
static GEN_RELEASES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Batches that built fewer frames than the engine planned, and why: no free
/// destination, or the engine's plan itself under the multiplier.
static GEN_SHORT_FREE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static GEN_SHORT_PLAN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static GEN_QUEUED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static GEN_LATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Batches kept in flight. Each holds a frame's generations until they are
/// committed, so the ring covers the frame being built, the one whose
/// generations are being shown, and one of slack.
const GEN_BATCHES: usize = 3;
/// Marks a queued source frame: it was written before it was queued, so it
/// waits on no batch.
const GEN_NO_BATCH: usize = usize::MAX;

pub struct GenBatch {
    pub fence: VkFence,
    pub cmd: VkCommandBuffer,
}

/// One frame waiting for its slot on the output grid.
pub struct GenDue {
    pub deadline: u64,
    pub slot: i32,
    pub batch: usize,
    /// Past this instant a generated frame whose batch has still not retired
    /// is dropped, so the source frame behind it goes out and the guest keeps
    /// its images coming back.
    pub drop_at: u64,
}

/// How long a generated frame may wait on its batch past its slot before it is
/// dropped: a couple of source periods at the slowest cap this is used with.
const GEN_GRACE_NS: u64 = 60_000_000;

/// The most destinations a swapchain gets: three batches' worth plus one, which
/// covers a compositor that returns a buffer two source periods after it was
/// committed.
fn gen_destination_cap(capacity: u32) -> usize {
    3 * capacity as usize + 2
}

/// Whether a batch has retired, without waiting for it.
unsafe fn gen_batch_idle(gd: *mut GnDevice, batch: usize) -> bool {
    let fence = match unsafe { (*gd).gen_batches.as_slice().get(batch) } {
        Some(b) => b.fence,
        None => return true,
    };
    if fence == 0 {
        return true;
    }
    let Some(status) = (unsafe { (*gd).f.get_fence_status }) else {
        return true;
    };
    (unsafe { status((*gd).real, fence) }) == VK_SUCCESS
}

/// Whether the swapchain's worker has a queued frame whose deadline has passed.
unsafe fn gen_due_now(sc: *mut GnSwapchain) -> bool {
    let gd = unsafe { (*sc).dev };
    if unsafe { (*gd).gen_sc } != sc {
        return false;
    }
    match unsafe { (*gd).gen_due.front() } {
        Some(d) => d.deadline <= monotonic_ns(),
        None => false,
    }
}

/// Generate the extra frames the multiplier asks for and queue them, then the
/// source frame behind them. Returns whether the source frame was queued; the
/// caller commits it inline otherwise.
///
/// Runs from the present worker before the guest's frame is committed: the slot
/// is still in flight, so the capture reads the frame the guest just finished
/// and nothing can reuse the slot underneath it.
unsafe fn lsfg_generate(sc: *mut GnSwapchain, src_idx: i32) -> bool {
    let gd = unsafe { (*sc).dev };
    let now = monotonic_ns();
    // The source interval, smoothed: the output grid when the user set no cap,
    // and the floor of the grid when the guest runs under its cap.
    {
        let last = unsafe { (*gd).gen_last_source_ns };
        unsafe { (*gd).gen_last_source_ns = now };
        if last != 0 {
            let d = now.saturating_sub(last);
            if (2_000_000..=250_000_000).contains(&d) {
                let p = unsafe { (*gd).gen_period_ns };
                unsafe { (*gd).gen_period_ns = if p == 0 { d } else { (p * 3 + d) / 4 } };
            }
        }
    }
    let session = lsfg_session(gd);
    if session.is_null() {
        return false;
    }
    unsafe { (*session).sync_refresh_rate() };
    let multiplier = unsafe { (*session).multiplier() };
    let capacity = multiplier.saturating_sub(1);
    if capacity == 0 {
        return false;
    }
    // A batch slot whose previous recording has retired and whose frames are
    // all committed. Nothing waits here: a batch still running means the chain
    // is over budget, and this frame goes out on its own.
    let batch = unsafe { (*gd).gen_next } % GEN_BATCHES;
    if !unsafe { gen_batch_idle(gd, batch) }
        || unsafe { (*gd).gen_due.iter().any(|d| d.batch == batch) }
    {
        GEN_SKIPS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // The queue ahead is bounded: a guest running ahead of its cap must not
    // stack frames further and further into the future.
    let period = unsafe { (*gd).gen_period_ns };
    if period != 0 && unsafe { (*gd).gen_last_deadline } > now + 2 * period {
        GEN_SKIPS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    while unsafe { (*gd).gen_batches.len() } < GEN_BATCHES {
        unsafe {
            (*gd).gen_batches.push(GenBatch {
                fence: 0,
                cmd: ptr::null_mut(),
            })
        };
    }
    // The retired batch's recording goes back to the pool now, not at submit
    // time, when the GPU may still be executing it.
    let old_cmd = unsafe { (&mut (*gd).gen_batches)[batch].cmd };
    if !old_cmd.is_null() {
        if let Some(free) = unsafe { (*gd).f.free_cmd } {
            unsafe { free((*gd).real, (*gd).cmd_pool, 1, &old_cmd) };
        }
        unsafe { (&mut (*gd).gen_batches)[batch].cmd = ptr::null_mut() };
    }
    // Destinations for the batch being shown and the one being built, plus one
    // so a late release never costs a whole batch. The compositor can hold a
    // buffer longer than that, so the set grows below when a batch finds too
    // few free, up to `gen_destination_cap`.
    if !lsfg_destinations(sc, 2 * capacity as usize + 1) {
        return false;
    }

    let width = unsafe { (*sc).extent.width };
    let height = unsafe { (*sc).extent.height };
    let f = unsafe { (*gd).f };
    if !unsafe {
        (*session).prepare(
            (*gd).real,
            &f,
            &(*gd).mem_props,
            width,
            height,
            (*sc).format,
        )
    } {
        return false;
    }
    // The engine reports no generation until its frame history exists, and that
    // history is built by `process` — so the capture runs on every presented
    // frame and only generation and blit are gated on the plan.
    // The requested plan, not a panel-clamped one: the session's mode advertises
    // the output rate this produces, so the compositor paces to it rather than
    // discarding the extra frames.
    let planned = unsafe { (*session).plan(capacity) };
    let guest = unsafe { (*sc).guest_slots as usize };
    // A destination is free once the compositor has released it and nothing
    // queued still points at it.
    let free: Vec<usize> = (guest..unsafe { (*sc).slots.len() })
        .filter(|&i| {
            let s = unsafe { (*sc).slots.as_ptr().add(i).as_ref().unwrap() };
            !s.in_flight && !unsafe { (*gd).gen_due.iter().any(|d| d.slot as usize == i) }
        })
        .collect();
    let mut free = free;
    if free.len() < planned as usize {
        GEN_SHORT_FREE.fetch_add(1, Ordering::Relaxed);
        // The compositor returns buffers later than the ring assumed: add what
        // this batch is short, within the cap. A new slot is free by definition.
        let total = unsafe { (*sc).slots.len() } - guest;
        let grow = (planned as usize - free.len())
            .min(gen_destination_cap(capacity).saturating_sub(total));
        if !crate::lsfg::skip("grow") && grow > 0 && lsfg_destinations(sc, total + grow) {
            free.extend(guest + total..guest + total + grow);
        }
    }
    let generations = (planned as usize).min(free.len()) as u32;
    if planned < capacity {
        GEN_SHORT_PLAN.fetch_add(1, Ordering::Relaxed);
    }

    let (Some(alloc_cmd), Some(free_cmd), Some(submit)) = (f.alloc_cmd, f.free_cmd, f.submit)
    else {
        return false;
    };
    let Some(create_fence) = f.create_fence else {
        return false;
    };
    let (Some(barrier), Some(blit)) = (f.cmd_pipeline_barrier, f.cmd_blit_image) else {
        return false;
    };
    if unsafe { (*gd).cmd_pool } == 0 {
        return false;
    }
    // A command buffer recorded in this pool is only valid on a queue of the
    // pool's own family; the guest's newest queue is not necessarily one.
    let queue = unsafe { lsfg_queue(gd) };
    if queue.is_null() {
        return false;
    }
    let real_queue = unsafe { (*queue).real };

    let Some(begin_ptr) = (unsafe { gn_real_device_proc(gd, c"vkBeginCommandBuffer") }) else {
        return false;
    };
    let Some(end_ptr) = (unsafe { gn_real_device_proc(gd, c"vkEndCommandBuffer") }) else {
        return false;
    };
    let begin: unsafe extern "C" fn(VkCommandBuffer, *const VkCommandBufferBeginInfo) -> VkResult =
        unsafe { std::mem::transmute(begin_ptr) };
    let end: unsafe extern "C" fn(VkCommandBuffer) -> VkResult =
        unsafe { std::mem::transmute(end_ptr) };

    let mut cbi: VkCommandBufferAllocateInfo = unsafe { std::mem::zeroed() };
    cbi.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO;
    cbi.commandPool = unsafe { (*gd).cmd_pool };
    cbi.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    cbi.commandBufferCount = 1;
    let mut cmd: VkCommandBuffer = std::ptr::null_mut();
    let alloc_rc = unsafe { alloc_cmd((*gd).real, &cbi, &mut cmd) };
    if alloc_rc != VK_SUCCESS {
        return false;
    }

    let mut bi: VkCommandBufferBeginInfo = unsafe { std::mem::zeroed() };
    bi.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
    bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    let begin_rc = unsafe { begin(cmd, &bi) };
    if begin_rc != VK_SUCCESS {
        unsafe { free_cmd((*gd).real, (*gd).cmd_pool, 1, &cmd) };
        return false;
    }

    let destinations: Vec<usize> = free.into_iter().take(generations as usize).collect();

    // Capture: the guest slot was presented, so it is in PRESENT_SRC; the engine
    // reads it as a general storage image.
    let src_image = unsafe {
        (*sc)
            .slots
            .as_ptr()
            .add(src_idx as usize)
            .as_ref()
            .unwrap()
            .image
    };
    let to_general = lsfg_barrier(
        src_image,
        VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
        VK_ACCESS_SHADER_READ_BIT,
        VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
        VK_IMAGE_LAYOUT_GENERAL,
    );
    unsafe {
        barrier(
            cmd,
            VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
            VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT | VK_PIPELINE_STAGE_TRANSFER_BIT,
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            1,
            &to_general,
        )
    };

    let api = unsafe { (*session).api() };
    unsafe {
        (api.process)(
            (*session).root(),
            cmd,
            src_image,
            width,
            height,
            generations,
        )
    };

    let targets = unsafe { (*session).targets() };
    // The engine indexes its per-generation target views by an opaque slot id;
    // the reference renderer offsets them past its frame-in-flight images
    // (`MAX_FRAMES_IN_FLIGHT + g`), so mirror that spacing here.
    const GEN_TARGET_BASE: usize = 3;
    let mut built = 0usize;
    for (generation, target) in targets.iter().take(generations as usize).enumerate() {
        unsafe {
            (api.generate_into)(
                (*session).root(),
                cmd,
                generation as u32,
                (GEN_TARGET_BASE + generation) as u32,
                target.image,
                target.view,
                width,
                height,
            )
        };
        let slot_image = unsafe {
            (*sc)
                .slots
                .as_ptr()
                .add(destinations[generation])
                .as_ref()
                .unwrap()
                .image
        };
        let ready = [
            lsfg_barrier(
                target.image,
                VK_ACCESS_SHADER_WRITE_BIT,
                VK_ACCESS_TRANSFER_READ_BIT,
                VK_IMAGE_LAYOUT_GENERAL,
                VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
            ),
            lsfg_barrier(
                slot_image,
                0,
                VK_ACCESS_TRANSFER_WRITE_BIT,
                VK_IMAGE_LAYOUT_UNDEFINED,
                VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            ),
        ];
        unsafe {
            barrier(
                cmd,
                VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                0,
                0,
                ptr::null(),
                0,
                ptr::null(),
                2,
                ready.as_ptr(),
            )
        };
        let mut region: VkImageBlit = unsafe { std::mem::zeroed() };
        region.srcSubresource.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
        region.srcSubresource.layerCount = 1;
        region.srcOffsets[1] = VkOffset3D {
            x: width as i32,
            y: height as i32,
            z: 1,
        };
        region.dstSubresource.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
        region.dstSubresource.layerCount = 1;
        region.dstOffsets[1] = VkOffset3D {
            x: width as i32,
            y: height as i32,
            z: 1,
        };
        unsafe {
            blit(
                cmd,
                target.image,
                VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                slot_image,
                VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                1,
                &region,
                VK_FILTER_NEAREST,
            )
        };
        let presented = lsfg_barrier(
            slot_image,
            VK_ACCESS_TRANSFER_WRITE_BIT,
            0,
            VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
        );
        unsafe {
            barrier(
                cmd,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                0,
                0,
                ptr::null(),
                0,
                ptr::null(),
                1,
                &presented,
            )
        };
        built += 1;
    }

    let back = lsfg_barrier(
        src_image,
        VK_ACCESS_SHADER_READ_BIT,
        VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
        VK_IMAGE_LAYOUT_GENERAL,
        VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
    );
    unsafe {
        barrier(
            cmd,
            VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT | VK_PIPELINE_STAGE_TRANSFER_BIT,
            VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            1,
            &back,
        )
    };

    let end_rc = unsafe { end(cmd) };
    if end_rc != VK_SUCCESS {
        unsafe { free_cmd((*gd).real, (*gd).cmd_pool, 1, &cmd) };
        return false;
    }
    // Submitted even with nothing built: the capture is what feeds the engine's
    // frame history, and `process` has already counted this frame.
    let fence = {
        let existing = unsafe { (&mut (*gd).gen_batches)[batch].fence };
        if existing == 0 {
            let mut fci: VkFenceCreateInfo = unsafe { std::mem::zeroed() };
            fci.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO;
            let mut created: VkFence = 0;
            if unsafe { create_fence((*gd).real, &fci, ptr::null(), &mut created) } != VK_SUCCESS {
                unsafe { free_cmd((*gd).real, (*gd).cmd_pool, 1, &cmd) };
                return false;
            }
            unsafe { (&mut (*gd).gen_batches)[batch].fence = created };
            created
        } else {
            if let Some(reset) = f.reset_fences {
                unsafe { reset((*gd).real, 1, &existing) };
            }
            existing
        }
    };
    let mut si: VkSubmitInfo = unsafe { std::mem::zeroed() };
    si.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO;
    si.commandBufferCount = 1;
    si.pCommandBuffers = &cmd;
    let rc = {
        let _submit = SUBMIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { submit(real_queue, 1, &si, fence) }
    };
    unsafe {
        (&mut (*gd).gen_batches)[batch].cmd = cmd;
        (*gd).gen_next = batch + 1;
        (*gd).gen_sc = sc;
    }
    let mut queued = false;
    if rc == VK_SUCCESS && built > 0 {
        // One output slot per frame, the generated frames first and the source
        // frame last, spaced evenly over one source period: the engine's frames
        // sit between the previous source frame and this one. The period is the
        // cap's, or the measured one when the guest runs slower than the cap or
        // has none.
        let rate = crate::lsfg::output_rate();
        let cap_period = if rate > 0 {
            1_000_000_000u64 / u64::from(rate) * u64::from(multiplier)
        } else {
            0
        };
        let source_period = match (cap_period, period) {
            (0, 0) => 16_666_667,
            (c, 0) => c,
            (0, p) => p,
            (c, p) => c.max(p),
        };
        let step = source_period / (built as u64 + 1);
        let base = now.max(unsafe { (*gd).gen_last_deadline });
        for (k, index) in destinations.iter().take(built).enumerate() {
            unsafe {
                (*gd).gen_due.push_back(GenDue {
                    deadline: base + step * (k as u64 + 1),
                    slot: *index as i32,
                    batch,
                    drop_at: base + step * (k as u64 + 1) + GEN_GRACE_NS,
                })
            };
        }
        GEN_QUEUED.fetch_add(built as u32, Ordering::Relaxed);
        let real_at = base + step * (built as u64 + 1);
        unsafe {
            (*gd).gen_due.push_back(GenDue {
                deadline: real_at,
                slot: src_idx,
                batch: GEN_NO_BATCH,
                drop_at: u64::MAX,
            });
            (*gd).gen_last_deadline = real_at;
        }
        queued = true;
    }
    // Health, not a trace: one line every 600 batches.
    static BATCHES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if BATCHES
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .is_multiple_of(600)
    {
        crate::logf!(gndrm_obf::obf!(
            "lsfg: generated {} (skips {}, queued {}, short: free {} plan {}, late {}, released {})",
            GEN_COMMITS.load(std::sync::atomic::Ordering::Relaxed),
            GEN_SKIPS.load(std::sync::atomic::Ordering::Relaxed),
            GEN_QUEUED.load(std::sync::atomic::Ordering::Relaxed),
            GEN_SHORT_FREE.load(std::sync::atomic::Ordering::Relaxed),
            GEN_SHORT_PLAN.load(std::sync::atomic::Ordering::Relaxed),
            GEN_LATE.load(std::sync::atomic::Ordering::Relaxed),
            GEN_RELEASES.load(std::sync::atomic::Ordering::Relaxed)
        ));
    }
    queued
}

/// Commit every queued frame whose slot on the output grid has arrived.
///
/// Runs on the present worker whenever the next deadline passes, between jobs.
/// A generated frame waits for its batch: the buffer is a plain dma-buf, so
/// neither gamescope nor the display can wait on the Vulkan fence, and attaching
/// early shows an unwritten image. The wait is bounded; past it the frame is
/// pushed back rather than shown corrupt.
unsafe fn flush_due(sc: *mut GnSwapchain) {
    let gd = unsafe { (*sc).dev };
    if unsafe { (*gd).gen_sc } != sc {
        return;
    }
    let mut committed = 0u32;
    while let Some((deadline, slot, batch, drop_at)) =
        (unsafe { (*gd).gen_due.front() }).map(|d| (d.deadline, d.slot, d.batch, d.drop_at))
    {
        let now = monotonic_ns();
        if deadline > now {
            break;
        }
        if batch != GEN_NO_BATCH {
            let fence = unsafe { (*gd).gen_batches.as_slice().get(batch) }.map_or(0, |b| b.fence);
            if fence != 0 {
                let mut ready = match unsafe { (*gd).f.get_fence_status } {
                    Some(status) => (unsafe { status((*gd).real, fence) }) == VK_SUCCESS,
                    None => true,
                };
                if !ready {
                    if let Some(wait) = unsafe { (*gd).f.wait_fences } {
                        ready = unsafe { wait((*gd).real, 1, &fence, VK_TRUE, 4_000_000) }
                            == VK_SUCCESS;
                    }
                }
                if !ready {
                    GEN_LATE.fetch_add(1, Ordering::Relaxed);
                    if now > drop_at {
                        // The batch is not coming in time for this frame to mean
                        // anything: drop it rather than hold the source frame
                        // behind it.
                        GEN_SKIPS.fetch_add(1, Ordering::Relaxed);
                        unsafe { (*gd).gen_due.pop_front() };
                        continue;
                    }
                    if let Some(d) = unsafe { (*gd).gen_due.front_mut() } {
                        d.deadline = now + 1_000_000;
                    }
                    break;
                }
            }
        }
        unsafe { (*gd).gen_due.pop_front() };
        if batch == GEN_NO_BATCH {
            unsafe { commit_real(sc, slot, 0) };
        } else {
            unsafe { commit_generated(sc, slot as usize) };
            committed += 1;
        }
    }
    if committed > 0 {
        GEN_COMMITS.fetch_add(committed, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The queue the generation batch submits on.
///
/// `(*gd).queues` is a stack of every queue the guest asked for, newest first,
/// while the shim's command pool belongs to the family named by the *first*
/// `VkDeviceQueueCreateInfo`. A command buffer recorded in a pool of family A
/// may only be submitted to a queue of family A: anything else is invalid usage,
/// and a driver is free to drop the whole submission without an error. A fence
/// that comes back signalled ~40 us later with nothing written is what that
/// looks like. Prefer a queue of the pool's family; fall back to the head only
/// when the device has no queue of that family.
unsafe fn lsfg_queue(gd: *mut GnDevice) -> *mut GnQueue {
    let head = unsafe { (*gd).queues };
    let mut q = head;
    while !q.is_null() {
        if unsafe { (*q).family } == unsafe { (*gd).queue_family } {
            return q;
        }
        q = unsafe { (*q).next };
    }
    head
}

/// `pop_job`'s answer when a queued frame is due before any guest job.
const JOB_FLUSH: i32 = -2;

/// Take the pending job: a mailbox entry (the newest frame) wins, otherwise
/// the oldest queued fifo job.
fn take_job(g: &mut ScShared) -> Option<i32> {
    if let Some(job) = g.mailbox.take() {
        return Some(job);
    }
    g.jobs.pop_front()
}

fn pop_job(sc: *mut GnSwapchain) -> i32 {
    let mut g = unsafe { (*sc).shared.lock().unwrap() };
    loop {
        // A due frame goes out before the next guest frame is taken: its commit
        // is quick, and the guest's fence wait is not.
        if unsafe { gen_due_now(sc) } {
            return JOB_FLUSH;
        }
        if let Some(job) = take_job(&mut g) {
            return job;
        }
        if g.stopping {
            return -1;
        }
        // Wake at the next queued frame's deadline, or on the idle tick.
        let timeout = {
            let gd = unsafe { (*sc).dev };
            let head = unsafe { (*gd).gen_due.front().map(|d| d.deadline) };
            match head {
                Some(deadline) => {
                    let now = monotonic_ns();
                    let wait = deadline.saturating_sub(now);
                    Duration::from_nanos(wait.min(IDLE_EVERY.as_nanos() as u64))
                        .max(Duration::from_micros(200))
                }
                None => IDLE_EVERY,
            }
        };
        let (guard, res) = unsafe { (*sc).cv_put.wait_timeout(g, timeout).unwrap() };
        g = guard;
        if res.timed_out() {
            if unsafe { gen_due_now(sc) } {
                return JOB_FLUSH;
            }
            // No present arrived: say so once every IDLE_EVERY with the slot
            // flags, so an app that stops presenting is visible instead of
            // silent.
            drop(g);
            // Let X11 releases progress even when the guest has stopped
            // presenting: the fence fires when the server drops the pixmap.
            unsafe { x11_reap(sc) };
            if rate_ok(&RATE_IDLE, IDLE_EVERY) {
                crate::logf!(gndrm_obf::obf!(
                    "gn-vkshim idle: present worker has no work {}",
                    slot_flags(sc)
                ));
            }
            g = unsafe { (*sc).shared.lock().unwrap() };
        }
    }
}

fn worker_main(sc: *mut GnSwapchain) {
    loop {
        let idx = pop_job(sc);
        if idx == JOB_FLUSH {
            unsafe { flush_due(sc) };
            continue;
        }
        if idx < 0 {
            break;
        }
        unsafe {
            let s = (*sc).slots.as_mut_ptr().add(idx as usize);
            let t0 = Instant::now();
            if let Some(wait_fences) = (*(*sc).dev).f.wait_fences {
                wait_fences((*(*sc).dev).real, 1, &(*s).fence, VK_TRUE, u64::MAX);
            }
            let fence_ms = t0.elapsed().as_millis();
            // Before the commit, so the capture reads the frame the guest just
            // finished while the slot is still ours. When the frame's generations
            // were queued, the frame itself is queued behind them.
            if lsfg_generate(sc, idx) {
                continue;
            }
            commit_real(sc, idx, fence_ms);
        }
    }
}

/// Commit a guest frame: into its X11 window on that route, otherwise attached
/// to the surface and committed. The slot's own fence has already been waited.
unsafe fn commit_real(sc: *mut GnSwapchain, idx: i32, fence_ms: u128) {
    unsafe {
        let s = (*sc).slots.as_mut_ptr().add(idx as usize);
        // X11 route: the window carries its own content, so the frame is
        // presented into it (DRI3 + Present) instead of committed to a
        // wl_surface. The slot stays in flight until its idle fence
        // triggers; x11_reap frees every slot whose fence has fired.
        if (*(*sc).surf).parent.is_null() {
            // X11 route: the window carries its own content, so the frame
            // is presented into it (DRI3 + Present) instead of committed to
            // a wl_surface. A present that cannot be made (route never came
            // up, or pixmap/fence/present refused) frees the slot here:
            // only a live present's idle fence releases a slot otherwise,
            // and a guest waiting on a release that never comes stalls on
            // its next acquire.
            if x11_present_slot(sc, s, idx) {
                maybe_report_game_window((*sc).surf);
                x11_reap(sc);
            } else {
                if !X11_DROP_LOGGED.swap(true, Ordering::Relaxed) {
                    crate::logf!(gndrm_obf::obf!(
                        "x11 route: present unavailable - frames dropped, slots kept free (guest keeps running)"
                    ));
                }
                let _g = (*sc).shared.lock().unwrap();
                (*s).in_flight = false;
                (*sc).cv_free.notify_all();
                drop(_g);
            }
            return;
        }
        if (*s).buf.is_null() {
            (*s).buf = slot_make_buffer(sc, s);
        }
        if (*s).buf.is_null() {
            crate::logf!(gndrm_obf::obf!(
                "proxy: wl_buffer creation failed — dropping frame"
            ));
            let _g = (*sc).shared.lock().unwrap();
            (*s).in_flight = false;
            (*sc).cv_free.notify_all();
            drop(_g);
            return;
        }
        let parent = (*(*sc).surf).parent;
        let t1 = Instant::now();
        let flush_rc = {
            let _sg = SEND_LOCK.lock().unwrap();
            // Declare this swapchain's *effective* present mode — the mode
            // `present_dispatch` honours — so gamescope's view matches what
            // the shim actually does. A slot becomes reusable only on the
            // compositor's wl_buffer.release, and gamescope reads the
            // declared mode once per commit and paces only the commits it
            // reads as FIFO — so the declaration rides every commit.
            // Declaring the effective mode (not a hardcoded FIFO) is what
            // lets a window's fps limit, or the absence of one, be seen by
            // the compositor as the shim applies it.
            wlproto::swapchain_set_present_mode(
                (*(*sc).surf).swapchain_obj,
                gamescope_present_mode((*sc).present_mode),
            );
            wlproto::surface_attach(parent, (*s).buf, 0, 0);
            wlproto::surface_damage_buffer(
                parent,
                0,
                0,
                (*sc).extent.width as i32,
                (*sc).extent.height as i32,
            );
            wlproto::surface_commit(parent);
            wlproto::wl_display_flush((*(*sc).surf).dpy)
        };
        // Override/Wayland route (the default; `GN_VKSHIM_GUEST_SURFACE`
        // unset or not exactly "1"): same tree-walk cadence as the
        // guest-surface arm above, so the game's own window clears the
        // splash on this route too.
        maybe_report_game_window((*sc).surf);
        let commit_ms = t1.elapsed().as_millis();
        let n = LOG_COMMIT.fetch_add(1, Ordering::Relaxed);
        let done = N_COMMIT_OK.fetch_add(1, Ordering::Relaxed);
        if n < LOG_FIRST_N || n.is_multiple_of(120) || fence_ms >= 100 {
            crate::logf!(gndrm_obf::obf!(
                "proxy commit: sc={sc:?} idx={idx} {}x{} flush_rc={flush_rc} dpy_err={} fence={fence_ms}ms commit={commit_ms}ms #{} {}",
                (*sc).extent.width,
                (*sc).extent.height,
                wlproto::wl_display_get_error((*(*sc).surf).dpy),
                done + 1,
                counters(),
            ));
        }
    }
}

unsafe extern "C" fn gn_vkCreateSwapchainKHR(
    device: VkDevice,
    ci: *const VkSwapchainCreateInfoKHR,
    _alloc: *const VkAllocationCallbacks,
    out: *mut VkSwapchainKHR,
) -> VkResult {
    HB_CREATE.fetch_add(1, Ordering::Relaxed);
    wsi_hb();
    let ci = unsafe { &*ci };
    let gd = unsafe { find_device(device) };
    let surf = unsafe { find_surface(ci.surface) };
    // The app pushes its LSFG settings to the daemon before the session starts,
    // so the daemon's answer is what the first present opens its session from.
    // Adopting it here fills the live copy, which nothing else does at launch:
    // the push path only fires on a change.
    if let Some(config) = crate::scanout::lsfg_config() {
        crate::lsfg::apply(config);
    }
    crate::logf!(gndrm_obf::obf!(
        "proxy swapchain create: device={device:?} surface={:#x} {}x{} fmt={} cs={} min={} mode={} old={:#x} -> gd={:?} surf={:?}",
        ci.surface,
        ci.imageExtent.width,
        ci.imageExtent.height,
        ci.imageFormat,
        ci.imageColorSpace,
        ci.minImageCount,
        ci.presentMode,
        ci.oldSwapchain as usize,
        gd,
        surf
    ));
    if gd.is_null() || surf.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let fourcc = vk_format_to_fourcc(ci.imageFormat);
    if fourcc == 0 {
        crate::logf!(gndrm_obf::obf!(
            "proxy: unsupported swapchain format {}",
            ci.imageFormat
        ));
        return VK_ERROR_FORMAT_NOT_SUPPORTED;
    }
    // Only the scan-out pool's own layout can be served end to end (see
    // `advertises_rgba`): the guest often renders straight into a daemon-owned
    // BGRA buffer, so a swapchain in any other channel order would publish
    // R/B-exchanged pixels. Refuse it loudly rather than render wrong colours.
    if !swapchain_format_allowed(ci.imageFormat, advertises_rgba()) {
        crate::logf!(gndrm_obf::obf!(
            "proxy: refusing swapchain format {} cs={} (scan-out pool is B8G8R8A8; set GN_VKSHIM_ALLOW_RGBA to opt in)",
            ci.imageFormat,
            ci.imageColorSpace
        ));
        return VK_ERROR_FORMAT_NOT_SUPPORTED;
    }
    // Declare what this swapchain is to gamescope. The colourspace is the load
    // bearing field: gamescope maps HDR10_ST2084 to its HDR10_PQ app-texture
    // colourspace, and that tag is what decides whether its composite passes PQ
    // through or applies the SDR→PQ LUT. It is taken from the create info, not
    // inferred from the format: the same 10-bit format is legitimately requested
    // with the plain sRGB colours by clients that want the precision without HDR
    // (DXVK does), and declaring that PQ makes gamescope decode sRGB as PQ.
    {
        let colorspace = ci.imageColorSpace;
        let scobj = unsafe { (*surf).swapchain_obj };
        if scobj.is_null() {
            crate::logf!(gndrm_obf::obf!(
                "proxy: no gamescope_swapchain to declare format {} colourspace {} on",
                ci.imageFormat,
                colorspace
            ));
        } else {
            let engine = c"gn-shim";
            let _sg = SEND_LOCK.lock().unwrap();
            unsafe {
                wlproto::swapchain_feedback(
                    scobj,
                    ci.minImageCount,
                    ci.imageFormat as u32,
                    colorspace as u32,
                    ci.compositeAlpha,
                    ci.preTransform,
                    ci.clipped,
                    engine.as_ptr(),
                );
                wlproto::wl_display_flush((*surf).dpy);
            }
            crate::logf!(gndrm_obf::obf!(
                "proxy: declared swapchain format {} colourspace {} to gamescope",
                ci.imageFormat,
                colorspace
            ));
        }
    }
    let mode = effective_present_mode(ci);
    // Mesa's wayland WSI derives minImageCount from the present mode
    // (wsi_common_wayland.c:1188): the extra image exists so one can be queued
    // for scan-out while the app renders the next. Mailbox therefore carries
    // one more slot than fifo; fifo keeps its existing count.
    let extra_slot = u32::from(mode == VK_PRESENT_MODE_MAILBOX_KHR);
    let mut n_slots = ci.minImageCount + 1 + extra_slot;
    if n_slots < 2 {
        n_slots = 2;
    }
    if n_slots > max_swapchain_images() {
        n_slots = max_swapchain_images();
    }
    let mut sc = Box::new(GnSwapchain {
        surf,
        dev: gd,
        format: ci.imageFormat,
        // Exactly the extent the guest asked for. The shim must never allocate a
        // different size: the guest renders into this image at its own chosen
        // viewport, so a smaller allocation (e.g. clamped to the guest's window)
        // is written out of bounds and the presented frame is corrupt.
        extent: ci.imageExtent,
        fourcc,
        present_mode: mode,
        // The slot image carries the usage the guest asked for, plus TRANSFER_SRC:
        // the engine copies the presented frame out of it, and generation can be
        // turned on after the swapchain exists.
        usage: ci.imageUsage | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
        slots: Vec::new(),
        guest_slots: 0,
        gen_no_grow: false,
        worker: None,
        shared: Mutex::new(ScShared {
            jobs: VecDeque::with_capacity(GN_JOB_CAP),
            mailbox: None,
            stopping: false,
        }),
        cv_put: Condvar::new(),
        cv_free: Condvar::new(),
        x11_trace: 0,
        next: ptr::null_mut(),
    });
    let scp: *mut GnSwapchain = &mut *sc;
    for _ in 0..n_slots {
        sc.slots.push(GnSlot {
            owner: scp,
            image: 0,
            mem: 0,
            fd: -1,
            stride: 0,
            offset: 0,
            modifier: 0,
            buf: ptr::null_mut(),
            xpix: 0,
            xfence: 0,
            fence: 0,
            sem: 0,
            acquired: false,
            in_flight: false,
            acq_seq: 0,
            ino: 0,
            floor_ns: 0,
            release_held: false,
            ahb: ptr::null_mut(),
        });
    }
    for inited in 0..n_slots as usize {
        let res = unsafe { slot_init(scp, sc.slots.as_mut_ptr().add(inited)) };
        if res != VK_SUCCESS {
            // `inited` is the slot that just failed; release it too, or its
            // dma-buf fd, image and memory (allocated before the failing step)
            // leak with the swapchain that is being abandoned.
            for j in (0..=inited).rev() {
                unsafe { slot_release(scp, sc.slots.as_mut_ptr().add(j)) };
            }
            crate::logf!(gndrm_obf::obf!("proxy: slot init failed: {res}"));
            return res;
        }
    }
    // The guest's slots are a prefix; generation destinations follow them. Both
    // the count and the capacity are settled here, at creation, because a slot
    // appended while the compositor holds buffers is not just a new image: a
    // push past the vector's capacity moves every `GnSlot`, and each buffer the
    // compositor still holds names its slot by raw pointer (`wl_buffer` user
    // data, `on_buffer_release`). A release arriving after that move writes
    // through a dangling pointer and the live slot never comes back — the guest
    // then loses an image mid-frame and takes its own window down within a
    // frame or two. So: room for every destination the cap allows, and the
    // destinations themselves created up front, before the first present.
    sc.guest_slots = n_slots;
    let capacity = crate::lsfg::wanted().unwrap_or(1).saturating_sub(1);
    let dest_cap = if capacity == 0 {
        0
    } else {
        gen_destination_cap(capacity)
    };
    sc.slots.reserve(dest_cap.max(gen_destination_cap(3)));
    if dest_cap > 0 {
        unsafe { lsfg_destinations(scp, dest_cap) };
    }

    // What the guest was told the surface can be, next to what the slot images
    // actually are. One line, once: a disagreement here names a guest that
    // sized its viewport to a different image than the shim renders into.
    if !CAPS_COMPARE_LOGGED.swap(true, Ordering::Relaxed) {
        if let Ok(adv) = ADVERTISED_CAPS.lock() {
            if let Some(a) = *adv {
                let stride = sc.slots.first().map_or(0, |s| s.stride);
                crate::logf!(gndrm_obf::obf!(
                    "x11 caps vs slot: advertised current={}x{} min={}x{} max={}x{} -> image {}x{} stride={} slots={}",
                    a.current.0,
                    a.current.1,
                    a.min.0,
                    a.min.1,
                    a.max.0,
                    a.max.1,
                    ci.imageExtent.width,
                    ci.imageExtent.height,
                    stride,
                    sc.slots.len()
                ));
            }
        }
    }
    let sc_addr = scp as usize;
    if !unsafe { (*(*scp).surf).x11 }.is_null() {
        crate::logf!(gndrm_obf::obf!(
            "X11 present engages: swapchain {scp:?} {}x{} presents through DRI3 + Present",
            ci.imageExtent.width,
            ci.imageExtent.height
        ));
    }
    let worker = std::thread::Builder::new()
        .name("gn-present".into())
        .spawn(move || worker_main(sc_addr as *mut GnSwapchain));
    let Ok(worker) = worker else {
        for j in 0..n_slots as usize {
            unsafe { slot_release(scp, sc.slots.as_mut_ptr().add(j)) };
        }
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    sc.worker = Some(worker);
    let sc = Box::into_raw(sc);
    {
        let mut g = G.lock().unwrap();
        unsafe {
            (*sc).next = g.swapchains;
            g.swapchains = sc;
        }
    }
    unsafe {
        *out = sc as usize as VkSwapchainKHR;
        crate::logf!(gndrm_obf::obf!(
            "proxy swapchain {:?}: {}x{} fmt {} -> fourcc {:#x}, {} images; requested usage={:#x} minImages={} presentMode={} compositeAlpha={:#x} colorspace={} clipped={} tiling=LINEAR effective_mode={} gamescope_mode={}",
            sc,
            (*sc).extent.width,
            (*sc).extent.height,
            (*sc).format,
            (*sc).fourcc,
            (*sc).slots.len(),
            ci.imageUsage,
            ci.minImageCount,
            ci.presentMode,
            ci.compositeAlpha,
            ci.imageColorSpace,
            ci.clipped,
            mode,
            gamescope_present_mode(mode)
        ));
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkGetSwapchainImagesKHR(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    count: *mut u32,
    images: *mut VkImage,
) -> VkResult {
    HB_IMAGES.fetch_add(1, Ordering::Relaxed);
    wsi_hb();
    let sc = unsafe { find_swapchain(swapchain) };
    if sc.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe {
        let n_slots = (*sc).guest_slots as usize;
        if images.is_null() {
            *count = n_slots as u32;
            return VK_SUCCESS;
        }
        let n = (*count).min(n_slots as u32) as usize;
        for i in 0..n {
            *images.add(i) = (*(*sc).slots.as_ptr().add(i)).image;
        }
        if crate::vkshim_enabled() {
            let hs: Vec<String> = (0..n)
                .map(|i| format!("{:#x}", *images.add(i) as usize))
                .collect();
            crate::logf!(gndrm_obf::obf!("x11 scimages: sc={sc:?} n={n} imgs={hs:?}"));
        }
        *count = n as u32;
        if n < n_slots {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        }
    }
}

unsafe extern "C" fn gn_vkDestroySwapchainKHR(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    _alloc: *const VkAllocationCallbacks,
) {
    HB_DESTROY.fetch_add(1, Ordering::Relaxed);
    wsi_hb();
    let sc = unsafe { find_swapchain(swapchain) };
    if sc.is_null() {
        return;
    }
    // A batch still in flight on this swapchain owns slots that are about to be
    // destroyed: wait it out, once, and drop the batch.
    {
        let gd = unsafe { (*sc).dev };
        if unsafe { !(*gd).gen_sc.is_null() && (*gd).gen_sc == sc } {
            for batch in unsafe { (*gd).gen_batches.iter() } {
                if batch.fence != 0 {
                    if let Some(wait) = unsafe { (*gd).f.wait_fences } {
                        unsafe { wait((*gd).real, 1, &batch.fence, VK_TRUE, u64::MAX) };
                    }
                }
            }
            unsafe {
                (*gd).gen_due.clear();
                (*gd).gen_last_deadline = 0;
                (*gd).gen_last_source_ns = 0;
                (*gd).gen_sc = ptr::null_mut();
            }
        }
    }
    {
        let mut g = unsafe { (*sc).shared.lock().unwrap() };
        g.stopping = true;
        unsafe {
            (*sc).cv_put.notify_all();
            (*sc).cv_free.notify_all();
        }
        drop(g);
    }
    {
        // One line per destroyed swapchain: a swapchain that goes away while the
        // guest keeps rendering is otherwise invisible, and nothing downstream
        // can tell it from a guest that simply stopped presenting.
        crate::logf!(gndrm_obf::obf!(
            "proxy swapchain destroyed: sc={sc:?} {}x{} presents={} commits={}",
            unsafe { (*sc).extent.width },
            unsafe { (*sc).extent.height },
            N_PRES.load(Ordering::Relaxed),
            N_COMMIT_OK.load(Ordering::Relaxed)
        ));
    }
    if let Some(worker) = unsafe { (*sc).worker.take() } {
        let _ = worker.join();
    }
    for i in 0..unsafe { (*sc).slots.len() } {
        unsafe { slot_release(sc, (*sc).slots.as_mut_ptr().add(i)) };
    }
    if !unsafe { (*(*sc).surf).dpy }.is_null() {
        let _sg = SEND_LOCK.lock().unwrap();
        unsafe { wlproto::wl_display_flush((*(*sc).surf).dpy) };
    }
    {
        let mut g = G.lock().unwrap();
        let mut pp: *mut *mut GnSwapchain = &raw mut g.swapchains;
        unsafe {
            while !(*pp).is_null() && *pp != sc {
                pp = &raw mut (**pp).next;
            }
            if !(*pp).is_null() {
                *pp = (*sc).next;
            }
        }
    }
    unsafe {
        drop(Box::from_raw(sc));
    }
}

// Acquire picks a free slot (blocks up to timeout for the compositor's
// wl_buffer.release) and signals the app's semaphore/fence with an empty
// submit: the slot carries no GPU work of ours, the app renders into the
// imported dma-buf image directly.
const REAP_SLICE: Duration = Duration::from_millis(4);

/// Bound `slice` by the soonest floor among the slots the compositor has
/// already returned, so such a slot is handed back the moment its floor passes
/// rather than on the next poll of the slice: the guest is blocked in the
/// acquire that called this, and a late free there is a late frame. Called with
/// `shared` held; takes no other lock.
unsafe fn slice_to_floor(sc: *mut GnSwapchain, slice: Duration) -> Duration {
    let now_ns = monotonic_ns();
    let mut soonest = 0u64;
    for k in 0..unsafe { (*sc).slots.len() } {
        let s = unsafe { (*sc).slots.as_mut_ptr().add(k) };
        let (held, floor) = unsafe { ((*s).release_held, (*s).floor_ns) };
        if held && (soonest == 0 || floor < soonest) {
            soonest = floor;
        }
    }
    if soonest == 0 || now_ns >= soonest {
        return slice;
    }
    slice.min(Duration::from_nanos(soonest - now_ns))
}

/// Claim the first swapchain slot that is neither held by the app nor awaiting a
/// compositor release. On success `idx` is set to the slot (marked acquired);
/// when none is free `idx` is left unchanged.
///
/// Withholding an in-flight slot is what paces the guest to gamescope's vblank
/// grid, i.e. to the panel refresh. Letting the app acquire one instead is not a
/// safe way to decouple the guest's rate: this scan hands out the *first* free
/// slot, so the app takes slot 0, presents it, re-acquires slot 0 and presents it
/// again — the compositor is handed the same buffer repeatedly and starves, which
/// measured as 8 commits against 5334 for the gated path on the same title. Any
/// free-running scheme needs a rotating slot choice (so each commit carries a
/// newer buffer) and its own back-pressure, not just this condition relaxed.
unsafe fn acquire_take_free(sc: *mut GnSwapchain, idx: &mut i32) {
    let now_ns = monotonic_ns();
    for k in 0..unsafe { (*sc).guest_slots as usize } {
        let s = unsafe { (*sc).slots.as_mut_ptr().add(k) };
        // A slot the compositor returned while the daemon's floor for it was
        // still ahead becomes free the instant that floor passes, and this scan
        // is what performs that free: the guest is blocked in the acquire that
        // called us, so nothing has to be woken and no timer is needed. The
        // floor is at most one target period past the daemon's last release and
        // the comparison is on a monotonic clock, so a held slot always becomes
        // free.
        if unsafe { (*s).release_held && now_ns >= (*s).floor_ns } {
            unsafe {
                (*s).release_held = false;
                (*s).in_flight = false;
            }
        }
        if unsafe { !(*s).acquired && !(*s).in_flight } {
            *idx = k as i32;
            unsafe {
                (*s).acquired = true;
            }
            let seq = ACQ_SEQ.fetch_add(1, Ordering::Relaxed);
            unsafe {
                (*s).acq_seq = seq as u64;
            }
            if seq < X11_TRACE_N {
                crate::logf!(gndrm_obf::obf!(
                    "x11 acquire {seq}: sc={sc:?} idx={} img={:#x}",
                    *idx,
                    unsafe { (*s).image } as usize
                ));
            }
            return;
        }
    }
}

unsafe fn gn_acquire_common(
    sc: *mut GnSwapchain,
    timeout: u64,
    sem: VkSemaphore,
    fence: VkFence,
    index: *mut u32,
) -> VkResult {
    let gd = (*sc).dev;
    let acq_t0 = Instant::now();
    N_ACQ.fetch_add(1, Ordering::Relaxed);
    ring_push(
        "vkAcquireNextImageKHR",
        [timeout as usize, sem as usize, fence as usize, 0, 0],
    );
    let deadline = (timeout != u64::MAX).then(|| Instant::now() + Duration::from_nanos(timeout));
    let mut idx: i32 = -1;
    let mut timed_out = false;
    {
        let mut g = (*sc).shared.lock().unwrap();
        loop {
            unsafe { acquire_take_free(sc, &mut idx) };
            if idx >= 0 || g.stopping {
                break;
            }
            if timeout == 0 {
                break;
            }
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    timed_out = true;
                    break;
                }
            }
            // Blocked here the guest is waiting for exactly a slot release. The
            // present worker only reaps X11 Present idle fences on its 2 s idle
            // tick, so left to it the acquire always ends on that timeout — a
            // self-throttle at ~1/2 s per freed slot. Drop the lock and poll the
            // fences directly so a slot frees as soon as the server drops its
            // pixmap, instead of a timer.
            drop(g);
            unsafe { x11_reap(sc) };
            g = (*sc).shared.lock().unwrap();
            unsafe { acquire_take_free(sc, &mut idx) };
            if idx >= 0 || g.stopping {
                break;
            }
            let slice = match deadline {
                Some(dl) => {
                    let now = Instant::now();
                    if now >= dl {
                        timed_out = true;
                        break;
                    }
                    (dl - now).min(REAP_SLICE)
                }
                None => REAP_SLICE,
            };
            let slice = unsafe { slice_to_floor(sc, slice) };
            let (guard, _res) = (*sc).cv_free.wait_timeout(g, slice).unwrap();
            g = guard;
        }
        if g.stopping && idx < 0 {
            drop(g);
            return VK_ERROR_OUT_OF_DATE_KHR;
        }
        drop(g);
    }
    let _ = timed_out;
    if idx < 0 {
        // Every slot is held by the compositor. This is the signature to look
        // for when the game goes silent: an app that spins here is waiting for
        // a wl_buffer.release that never comes.
        N_STARVE.fetch_add(1, Ordering::Relaxed);
        if rate_ok(&RATE_STALL, RATE_EVERY) {
            crate::logf!(gndrm_obf::obf!(
                "gn-vkshim stall: acquire found no free slot (timeout={timeout}) {}",
                slot_flags(sc)
            ));
        }
        return if timeout == 0 {
            VK_NOT_READY
        } else {
            VK_TIMEOUT
        };
    }
    if sem != 0 || fence != 0 {
        let gq = unsafe { (*gd).queues };
        if gq.is_null() {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        let si = VkSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            pNext: ptr::null(),
            waitSemaphoreCount: 0,
            pWaitSemaphores: ptr::null(),
            pWaitDstStageMask: ptr::null(),
            commandBufferCount: 0,
            pCommandBuffers: ptr::null(),
            signalSemaphoreCount: if sem != 0 { 1 } else { 0 },
            pSignalSemaphores: if sem != 0 { &sem } else { ptr::null() },
        };
        let res = {
            let _submit = SUBMIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            unsafe { (*gd).f.submit.unwrap()((*gq).real, 1, &si, fence) }
        };
        if res != VK_SUCCESS {
            let _g = (*sc).shared.lock().unwrap();
            (*(*sc).slots.as_mut_ptr().add(idx as usize)).acquired = false;
            (*sc).cv_free.notify_all();
            drop(_g);
            return res;
        }
    }
    *index = idx as u32;
    let acq_ms = acq_t0.elapsed().as_millis();
    let acq_us = acq_t0.elapsed().as_micros() as usize;
    ACQ_US_N.fetch_add(1, Ordering::Relaxed);
    ACQ_US_SUM.fetch_add(acq_us, Ordering::Relaxed);
    ACQ_US_MAX.fetch_max(acq_us, Ordering::Relaxed);
    if acq_us > 0 {
        ACQ_US_NONZERO.fetch_add(1, Ordering::Relaxed);
    }
    if acq_us >= 8_333 {
        ACQ_GE_8MS.fetch_add(1, Ordering::Relaxed);
    }
    if acq_us >= 16_667 {
        ACQ_GE_16MS.fetch_add(1, Ordering::Relaxed);
    }
    // Bounded one-shot: a guest thread blocked in acquire for a second or more
    // is the stall class this ring exists to name, so print the last calls the
    // guest made before it parked (once per process).
    if acq_ms >= 1000 {
        ring_dump("acquire blocked >=1s for a swapchain slot");
    }
    let n = LOG_ACQ.fetch_add(1, Ordering::Relaxed);
    if n < LOG_FIRST_N || n.is_multiple_of(120) || acq_ms >= 100 {
        crate::logf!(gndrm_obf::obf!(
            "proxy acquire: sc={sc:?} slot={idx} wait={acq_ms}ms"
        ));
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkAcquireNextImageKHR(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    timeout: u64,
    sem: VkSemaphore,
    fence: VkFence,
    index: *mut u32,
) -> VkResult {
    HB_ACQ.fetch_add(1, Ordering::Relaxed);
    let sc = unsafe { find_swapchain(swapchain) };
    if sc.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe { gn_acquire_common(sc, timeout, sem, fence, index) }
}

unsafe extern "C" fn gn_vkAcquireNextImage2KHR(
    _device: VkDevice,
    ai: *const VkAcquireNextImageInfoKHR,
    index: *mut u32,
) -> VkResult {
    let ai = unsafe { &*ai };
    let sc = unsafe { find_swapchain(ai.swapchain) };
    if sc.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe { gn_acquire_common(sc, ai.timeout, ai.semaphore, ai.fence, index) }
}

// Present discipline, one function per effective present mode; `present_dispatch`
// is the single entry point the present path uses.

/// fifo: one pending frame; a present blocks the guest only when the bounded job
/// queue is full, and then commits inline on the guest's thread, so presentation
/// paces to the compositor's release.
unsafe fn present_fifo(sc: *mut GnSwapchain, slot: *mut GnSlot, idx: i32) {
    let mut g = unsafe { (*sc).shared.lock().unwrap() };
    unsafe {
        (*slot).acquired = false;
        (*slot).in_flight = true;
        if g.jobs.len() < GN_JOB_CAP {
            g.jobs.push_back(idx);
            (*sc).cv_put.notify_one();
            drop(g);
        } else {
            // Job queue full: commit synchronously instead of dropping.
            (*slot).in_flight = false;
            (*sc).cv_free.notify_all();
            drop(g);
            present_sync_commit(sc, slot, idx);
        }
    }
}

/// One 60 Hz frame: the longest fifo_relaxed waits for a slot release before it
/// hands the late frame to the worker anyway.
const PRESENT_RELAXED_FRAME_BUDGET: Duration = Duration::from_micros(16_667);

/// fifo_relaxed: fifo's in-order worker queue, but a full queue waits for a slot
/// release only up to one frame budget, then hands the frame to the worker
/// anyway, so a late present is delayed rather than pushed back a whole vblank
/// (and never dropped).
unsafe fn present_fifo_relaxed(sc: *mut GnSwapchain, slot: *mut GnSlot, idx: i32) {
    let mut g = unsafe { (*sc).shared.lock().unwrap() };
    unsafe {
        (*slot).acquired = false;
        (*slot).in_flight = true;
        if g.jobs.len() >= GN_JOB_CAP {
            let deadline = Instant::now() + PRESENT_RELAXED_FRAME_BUDGET;
            while g.jobs.len() >= GN_JOB_CAP {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let (guard, _res) = (*sc).cv_free.wait_timeout(g, deadline - now).unwrap();
                g = guard;
            }
        }
        g.jobs.push_back(idx);
        (*sc).cv_put.notify_one();
    }
    drop(g);
}

/// mailbox: single-entry mailbox. The guest never blocks; the newest frame
/// replaces any pending one and the replaced slot is freed immediately.
unsafe fn present_mailbox(sc: *mut GnSwapchain, slot: *mut GnSlot, idx: i32) {
    let mut g = unsafe { (*sc).shared.lock().unwrap() };
    unsafe {
        (*slot).acquired = false;
        (*slot).in_flight = true;
        if let Some(old) = g.mailbox.replace(idx) {
            let os = (*sc).slots.as_mut_ptr().add(old as usize);
            (*os).in_flight = false;
        }
        (*sc).cv_free.notify_all();
        (*sc).cv_put.notify_one();
    }
    drop(g);
}

/// immediate: no vsync wait and no frame replacement. Every present is queued
/// for the worker in order; the bounded job queue (GN_JOB_CAP) is the only
/// backpressure, so the guest never blocks here and no frame is dropped.
unsafe fn present_immediate(sc: *mut GnSwapchain, slot: *mut GnSlot, idx: i32) {
    let mut g = unsafe { (*sc).shared.lock().unwrap() };
    unsafe {
        (*slot).acquired = false;
        (*slot).in_flight = true;
        g.jobs.push_back(idx);
        (*sc).cv_put.notify_one();
    }
    drop(g);
}

/// Commit a present inline on the calling thread and wait the slot's fence.
unsafe fn present_sync_commit(sc: *mut GnSwapchain, slot: *mut GnSlot, idx: i32) {
    if let Some(wait_fences) = unsafe { (*(*sc).dev).f.wait_fences } {
        unsafe { wait_fences((*(*sc).dev).real, 1, &(*slot).fence, VK_TRUE, u64::MAX) };
    }
    let parent = unsafe { (*(*sc).surf).parent };
    if parent.is_null() {
        // The X11 present holds the slot until its idle fence triggers; the
        // immediate in_flight clear above is restored on success.
        if unsafe { x11_present_slot(sc, slot, idx) } {
            maybe_report_game_window((*sc).surf);
            let g = unsafe { (*sc).shared.lock().unwrap() };
            unsafe { (*slot).in_flight = true };
            drop(g);
        }
        unsafe { x11_reap(sc) };
    } else {
        if unsafe { (*slot).buf.is_null() } {
            unsafe { (*slot).buf = slot_make_buffer(sc, slot) };
        }
        if !unsafe { (*slot).buf.is_null() } {
            let _sg = SEND_LOCK.lock().unwrap();
            unsafe {
                // Declare this swapchain's *effective* present mode, as on the
                // worker path above (gamescope reads the mode once per commit).
                wlproto::swapchain_set_present_mode(
                    (*(*sc).surf).swapchain_obj,
                    gamescope_present_mode((*sc).present_mode),
                );
                wlproto::surface_attach(parent, (*slot).buf, 0, 0);
                wlproto::surface_damage_buffer(
                    parent,
                    0,
                    0,
                    (*sc).extent.width as i32,
                    (*sc).extent.height as i32,
                );
                wlproto::surface_commit(parent);
                wlproto::wl_display_flush((*(*sc).surf).dpy);
            }
        }
        // Override/Wayland route (the default; `GN_VKSHIM_GUEST_SURFACE` unset
        // or not exactly "1"): same tree-walk cadence as the guest-surface arm above.
        maybe_report_game_window((*sc).surf);
    }
}

/// Route a present to the discipline its effective present mode names.
unsafe fn present_dispatch(sc: *mut GnSwapchain, slot: *mut GnSlot, idx: i32) {
    match unsafe { (*sc).present_mode } {
        VK_PRESENT_MODE_IMMEDIATE_KHR => unsafe { present_immediate(sc, slot, idx) },
        VK_PRESENT_MODE_MAILBOX_KHR => unsafe { present_mailbox(sc, slot, idx) },
        VK_PRESENT_MODE_FIFO_RELAXED_KHR => unsafe { present_fifo_relaxed(sc, slot, idx) },
        _ => unsafe { present_fifo(sc, slot, idx) },
    }
}

// Present: the app's render completion
unsafe extern "C" fn gn_vkQueuePresentKHR(queue: VkQueue, pi: *const VkPresentInfoKHR) -> VkResult {
    HB_PRESENT.fetch_add(1, Ordering::Relaxed);
    wsi_hb();
    N_PRES.fetch_add(1, Ordering::Relaxed);
    // The guest's frame period as the shim sees it: stamped at present entry,
    // before any work, so the census is the client's own cadence, not ours.
    pres_iv_note(Instant::now());
    let pi = unsafe { &*pi };
    let gq = unsafe { find_queue(queue) };
    let mut gd = if gq.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*gq).gd }
    };
    if gd.is_null() {
        // A queue the shim never wrapped here is not fatal on its own: the
        // present still runs against the device table head, but say so, because
        // an unrecognised queue is the same wrong-handle class as submit.
        let (first, n) = unsafe { device_table() };
        loud("present.queue_miss", || {
            format!(
                "{}",
                gndrm_obf::obf!(
                    "proxy: gn_vkQueuePresentKHR QUEUE MISS q={:#x} {}",
                    queue as usize,
                    unsafe { handle_note(false, queue as usize, first, n) }
                )
            )
        });
        gd = G.lock().unwrap().devices;
    }
    if gd.is_null() {
        loud("present.device_miss", || {
            format!(
                "{}",
                gndrm_obf::obf!(
                    "proxy: gn_vkQueuePresentKHR DEVICE MISS q={:#x} — no device wrapper at all",
                    queue as usize
                )
            )
        });
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let real_q = if gq.is_null() {
        queue
    } else {
        unsafe { (*gq).real }
    };
    let nsc = (pi.swapchainCount as usize).min(GN_MAX_PRESENT);
    let mut chain_prev: VkSemaphore = 0;
    let swapchains = unsafe { slice_or_empty(pi.pSwapchains, nsc) };
    let indices = unsafe { slice_or_empty(pi.pImageIndices, nsc) };
    for i in 0..nsc {
        let sc = unsafe { find_swapchain(swapchains[i]) };
        if sc.is_null() || indices[i] as usize >= unsafe { (*sc).guest_slots as usize } {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        let slot = unsafe { (*sc).slots.as_mut_ptr().add(indices[i] as usize) };
        let (waits, mut n_waits) = if chain_prev == 0 {
            (pi.pWaitSemaphores, pi.waitSemaphoreCount)
        } else {
            (&chain_prev as *const VkSemaphore, 1)
        };
        // Signal the chain semaphore only when a later present will wait
        // it; an unwaited binary semaphore could not be re-signaled.
        let signal_chain = i + 1 < nsc;
        unsafe {
            (*gd).f.reset_fences.unwrap()((*gd).real, 1, &(*slot).fence);
        }
        let mut wait_stages = [VK_PIPELINE_STAGE_ALL_COMMANDS_BIT; GN_MAX_PRESENT];
        if n_waits as usize > GN_MAX_PRESENT {
            n_waits = GN_MAX_PRESENT as u32;
        }
        let si = VkSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            pNext: ptr::null(),
            waitSemaphoreCount: n_waits,
            pWaitSemaphores: waits,
            pWaitDstStageMask: wait_stages.as_mut_ptr(),
            commandBufferCount: 0,
            pCommandBuffers: ptr::null(),
            signalSemaphoreCount: if signal_chain { 1 } else { 0 },
            pSignalSemaphores: if signal_chain {
                unsafe { &(*slot).sem }
            } else {
                ptr::null()
            },
        };
        let res = {
            let _submit = SUBMIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            unsafe { (*gd).f.submit.unwrap()(real_q, 1, &si, (*slot).fence) }
        };
        if res != VK_SUCCESS {
            crate::logf!(gndrm_obf::obf!("proxy: present wait submit failed: {res}"));
            return res;
        }
        if signal_chain {
            chain_prev = unsafe { (*slot).sem };
        }
        unsafe { present_dispatch(sc, slot, indices[i] as i32) };
        if LOG_PRESENT.fetch_add(1, Ordering::Relaxed) < LOG_FIRST_N {
            crate::logf!(gndrm_obf::obf!(
                "proxy present: sc={sc:?} idx={} {}",
                indices[i],
                counters()
            ));
        }
        if LOUD_PRESENT.fetch_add(1, Ordering::Relaxed) < 4 {
            crate::logf!(gndrm_obf::obf!(
                "proxy: present flow pid={} sc={sc:?} idx={} {}",
                std::process::id(),
                indices[i],
                counters()
            ));
        }
    }
    VK_SUCCESS
}

// The app's own submits carry shim wrapper command buffers; swap each for
// the real handle before handing the batch to the hooks.
unsafe extern "C" fn gn_vkQueueSubmit(
    queue: VkQueue,
    count: u32,
    submits: *const VkSubmitInfo,
    fence: VkFence,
) -> VkResult {
    ring_push(
        "vkQueueSubmit",
        [
            queue as usize,
            count as usize,
            submits as usize,
            fence as usize,
            0,
        ],
    );
    unsafe { submit_impl(queue, count, submits, fence) }
}

unsafe fn submit_impl(
    queue: VkQueue,
    count: u32,
    submits: *const VkSubmitInfo,
    fence: VkFence,
) -> VkResult {
    let gq = unsafe { find_queue(queue) };
    if gq.is_null() {
        // No queue wrapper: the guest's VkQueue is not one `gn_vkGetDeviceQueue`
        // produced, so the batch has no device to reach the driver through.
        let (first, n) = unsafe { device_table() };
        loud("submit.miss", || {
            format!(
                "{}",
                gndrm_obf::obf!(
                    "proxy: vkQueueSubmit QUEUE MISS q={:#x} {} — submitting guest CBs directly",
                    queue as usize,
                    unsafe { handle_note(false, queue as usize, first, n) }
                )
            )
        });
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let gd = unsafe { (*gq).gd };
    // Nested wrapped command buffers are translated from the registry plan
    // (`VK_NESTED_HANDLES`); the stage owns the copies the driver reads.
    let mut stage = NestedStage::new();
    let mut args = [
        queue as usize,
        count as usize,
        submits as usize,
        fence as usize,
        0,
        0,
    ];
    unsafe { translate_nested("vkQueueSubmit", &mut args, &mut stage) };
    let seq = LOG_SUBMIT.fetch_add(1, Ordering::Relaxed);
    crate::logf!(gndrm_obf::obf!(
        "proxy submit #{seq}: n={count} q={queue:?} entries={:?} fence={fence:?}",
        unsafe { slice_or_empty(submits, count as usize) }
            .iter()
            .map(|s| s.commandBufferCount)
            .collect::<Vec<_>>()
    ));
    if LOUD_SUBMIT.fetch_add(1, Ordering::Relaxed) < 4 {
        crate::logf!(gndrm_obf::obf!(
            "proxy: submit flow #{seq} pid={} q={queue:?} real_q={:?} n={count} ncmd={:?}",
            std::process::id(),
            unsafe { (*gq).real },
            unsafe { slice_or_empty(submits, count as usize) }
                .iter()
                .map(|s| s.commandBufferCount)
                .collect::<Vec<_>>()
        ));
    }
    let _submit = SUBMIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { (*gd).f.submit.unwrap()((*gq).real, count, args[2] as *const VkSubmitInfo, fence) }
}

/// GPU waits. The guest blocks here when submitted work never completes, so log
/// what it wants against the last value it asked us to signal. The line is
/// printed BEFORE the wait, so if the wait never returns, the last line in the
/// log *is* the block; a returned wait longer than a second is logged too.
unsafe extern "C" fn gn_vkWaitSemaphores(
    device: VkDevice,
    wi: *const VkSemaphoreWaitInfo,
    timeout: u64,
) -> VkResult {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let Some(f) = (unsafe { (*gd).f.wait_semaphores }) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    if rate_ok(&RATE_WAIT, RATE_EVERY) {
        let info = unsafe { &*wi };
        let n = info.semaphoreCount as usize;
        let sems = unsafe { slice_or_empty(info.pSemaphores, n) };
        let vals = unsafe { slice_or_empty(info.pValues, n) };
        let wants = sems
            .iter()
            .zip(vals.iter())
            .take(4)
            .map(|(s, v)| format!("{s:?}@{v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let last = LAST_SIG
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|(s, v)| format!("{s}@{v}"))
            .unwrap_or_else(|| String::from("<none>"));
        crate::logf!(gndrm_obf::obf!(
            "proxy wait: n={n} want=[{wants}] last_signalled={last} timeout={timeout}"
        ));
    }
    let t0 = Instant::now();
    let res = unsafe { f((*gd).real, wi, timeout) };
    let ms = t0.elapsed().as_millis();
    if ms >= 1000 {
        crate::logf!(gndrm_obf::obf!("proxy wait done: res={res} after={ms}ms"));
    }
    res
}

unsafe extern "C" fn gn_vkQueueSubmit2(
    queue: VkQueue,
    count: u32,
    submits: *const VkSubmitInfo2,
    fence: VkFence,
) -> VkResult {
    ring_push(
        "vkQueueSubmit2",
        [
            queue as usize,
            count as usize,
            submits as usize,
            fence as usize,
            0,
        ],
    );
    unsafe { submit2_impl(queue, count, submits, fence) }
}

unsafe fn submit2_impl(
    queue: VkQueue,
    count: u32,
    submits: *const VkSubmitInfo2,
    fence: VkFence,
) -> VkResult {
    let gq = unsafe { find_queue(queue) };
    if gq.is_null() {
        let (first, n) = unsafe { device_table() };
        loud("submit2.miss", || {
            format!(
                "{}",
                gndrm_obf::obf!(
                    "proxy: vkQueueSubmit2 QUEUE MISS q={:#x} {} — batch dropped",
                    queue as usize,
                    unsafe { handle_note(false, queue as usize, first, n) }
                )
            )
        });
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let gd = unsafe { (*gq).gd };
    // Per device (see gn_vkGetDeviceQueue2): the hooks' resolved entry points
    // are tied to the device handle that resolved them.
    let Some(real_fn) = (unsafe { (*gd).f.submit2 }) else {
        return VK_ERROR_FEATURE_NOT_PRESENT;
    };
    // Nested wrapped command buffers (VkCommandBufferSubmitInfo::commandBuffer)
    // are translated from the registry plan (`VK_NESTED_HANDLES`); the stage
    // owns the copies the driver reads.
    let mut stage = NestedStage::new();
    let mut args = [
        queue as usize,
        count as usize,
        submits as usize,
        fence as usize,
        0,
        0,
    ];
    unsafe { translate_nested("vkQueueSubmit2", &mut args, &mut stage) };
    let translated = args[2] as *const VkSubmitInfo2;
    if LOUD_SUBMIT2.fetch_add(1, Ordering::Relaxed) < 4 {
        crate::logf!(gndrm_obf::obf!(
            "proxy: submit2 flow pid={} q={queue:?} real_q={:?} n={count} ncmd={:?}",
            std::process::id(),
            unsafe { (*gq).real },
            unsafe { slice_or_empty(submits, count as usize) }
                .iter()
                .map(|s| s.commandBufferInfoCount)
                .collect::<Vec<_>>()
        ));
    }
    {
        // Every submit, terse: the faulting call is the last line before the
        // exception, and logcat still holds that tail. The abandonment tracker
        // reports whether it returned. The pairs are what we hand the driver —
        // a real handle on the left of no arrow means it was never translated.
        let seq = LOG_SUBMIT.fetch_add(1, Ordering::Relaxed);
        let mut cbs: Vec<String> = Vec::new();
        let mut sems: Vec<String> = Vec::new();
        {
            let originals = unsafe { slice_or_empty(submits, count as usize) };
            // Semaphores are the guest's cross-submit ordering: a wait that is
            // never signalled is what a spinning DXVK thread looks like from
            // here, so log the first few of each kind per entry.
            for si in originals.iter().take(2) {
                let nw = si.waitSemaphoreInfoCount as usize;
                let ws = unsafe {
                    slice_or_empty(si.pWaitSemaphoreInfos as *const VkSemaphoreSubmitInfo, nw)
                };
                for s in ws.iter().take(3) {
                    sems.push(format!(
                        "{}",
                        gndrm_obf::obf!("W{:?}@{}", s.semaphore, s.value)
                    ));
                }
                let ns = si.signalSemaphoreInfoCount as usize;
                let ss = unsafe {
                    slice_or_empty(si.pSignalSemaphoreInfos as *const VkSemaphoreSubmitInfo, ns)
                };
                for s in ss.iter().take(3) {
                    sems.push(format!(
                        "{}",
                        gndrm_obf::obf!("S{:?}@{}", s.semaphore, s.value)
                    ));
                    if let Ok(mut g) = LAST_SIG.lock() {
                        *g = Some((format!("{:?}", s.semaphore), s.value));
                    }
                }
            }
            for (i, si) in originals.iter().enumerate() {
                let n = si.commandBufferInfoCount as usize;
                let pcs = unsafe { slice_or_empty(si.pCommandBufferInfos, n) };
                let ti = unsafe { &*translated.add(i) };
                let newpcs = unsafe { slice_or_empty(ti.pCommandBufferInfos, n) };
                for (j, c) in pcs.iter().enumerate().take(4) {
                    match newpcs.get(j) {
                        Some(rc) => {
                            cbs.push(format!("{:?}->{:?}", c.commandBuffer, rc.commandBuffer))
                        }
                        None => cbs.push(format!("{:?}->?", c.commandBuffer)),
                    }
                }
            }
        }
        // Rate-limited: submits run at the guest's frame rate, so one line per
        // submit floods the 256 KiB log ring in seconds and evicts the startup
        // sequence this log exists to explain.
        if seq < LOG_FIRST_N || seq.is_multiple_of(240) {
            crate::logf!(gndrm_obf::obf!(
                "proxy submit2 #{seq}: n={count} q={queue:?} real_q={:?} entries={:?} cbs={cbs:?} sems={sems:?} fence={fence:?}",
                unsafe { (*gq).real },
                unsafe { slice_or_empty(submits, count as usize) }
                    .iter()
                    .map(|s| s.commandBufferInfoCount)
                    .collect::<Vec<_>>()
            ));
        }
    }
    let _submit = SUBMIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { real_fn((*gq).real, count, translated, fence) }
}

// Command buffers handed to the app are shim wrappers (same scratch-prefix
// layout as the device/queue handles), keeping the hooks' buffers private.
// This is load-bearing beyond privacy: every entry point the shim does not
// intercept is served by a generated thunk whose whole body is
// `ldr x0, [x0, #scratch*8]` — it reads the real handle out of the wrapper at
// a fixed offset and tail-branches to the hooks. Handing out a real buffer
// makes that read garbage and the driver faults on it (vkBeginCommandBuffer
// 0xc0000005).
unsafe extern "C" fn gn_vkAllocateCommandBuffers(
    device: VkDevice,
    ci: *const VkCommandBufferAllocateInfo,
    out: *mut VkCommandBuffer,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        let (first, n) = unsafe { device_table() };
        loud("alloc_cmd.miss", || {
            format!(
                "{}",
                gndrm_obf::obf!(
                    "proxy: vkAllocateCommandBuffers DEVICE MISS {} — guest CBs will be driver-raw",
                    unsafe { handle_note(false, device as usize, first, n) }
                )
            )
        });
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let mut rc = vec![ptr::null_mut::<c_void>(); ci.commandBufferCount as usize];
    let res = unsafe { (*gd).f.alloc_cmd.unwrap()((*gd).real, ci, rc.as_mut_ptr()) };
    if res != VK_SUCCESS {
        return res;
    }
    {
        let _g = G.lock().unwrap();
        for (i, &real) in rc.iter().enumerate() {
            let gc = Box::new(GnCmd {
                scratch: [0; GN_HANDLE_SCRATCH],
                real,
                gd,
                next: ptr::null_mut(),
            });
            let gc = Box::into_raw(gc);
            unsafe {
                (*gc).next = (*gd).cmds;
                (*gd).cmds = gc;
                noted_wrapper(gc as usize);
                *out.add(i) = gc.cast::<c_void>();
                crate::logf!(gndrm_obf::obf!(
                    "proxy alloc cmd: wrapper={:?} real={real:?}",
                    gc.cast::<c_void>()
                ));
            }
        }
    }
    VK_SUCCESS
}

// ------------------------------ pipeline creation (winding passes through)
//
// The driver applies a negative-height (D3D-style) viewport but does not
// invert the effective winding. Compensating by swapping frontFace on culling
// pipelines is wrong: a vkd3d-proton (D3D12) two-sided material shades from
// gl_FrontFacing and renders a black silhouette under the swap, and neither
// DXVK (D3D11) nor the X11 arm needs it — a synthetic-quad measurement does
// not generalise. Culling pipelines are counted and passed through unmodified.
unsafe extern "C" fn gn_vkCreateGraphicsPipelines(
    device: VkDevice,
    cache: VkPipelineCache,
    count: u32,
    cis: *const VkGraphicsPipelineCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkPipeline,
) -> VkResult {
    let gd = unsafe { find_device(device) };
    if gd.is_null() || unsafe { (*gd).f.create_pipelines.is_none() } {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let real = unsafe { (*gd).f.create_pipelines.unwrap() };
    if cis.is_null() {
        return unsafe { real((*gd).real, cache, count, cis, alloc, out) };
    }
    // Count the pipelines that cull; the client's create infos pass through
    // unmodified.
    let mut culled = 0u32;
    for i in 0..count as usize {
        let ci = unsafe { *cis.add(i) };
        if !ci.pRasterizationState.is_null()
            && unsafe { (*ci.pRasterizationState).cullMode } != VK_CULL_MODE_NONE
        {
            culled += 1;
        }
    }
    let res = unsafe { real((*gd).real, cache, count, cis, alloc, out) };
    if culled > 0 {
        crate::logf!(gndrm_obf::obf!(
            "proxy: winding compensation skipped on {culled}/{count} culling pipeline(s)"
        ));
    }
    res
}

// Bring-up: log the first occurrence of each image format the guest creates.
// Texture formats are the one Vulkan-level input we cannot see otherwise —
// DXVK logs a format only when its own table lacks it, so emulation gaps in
// the driver (e.g. BCn on Adreno) stay silent and show up as black elements.
// Bounded to LOG_IMAGE_FIRST_N distinct (format, usage, tiling) triples, so a
// session pays one lock per created image and one line per new format only.
static SEEN_IMAGE: Mutex<Vec<(VkFormat, VkImageUsageFlags, VkImageTiling)>> =
    Mutex::new(Vec::new());
const LOG_IMAGE_FIRST_N: usize = 48;

unsafe extern "C" fn gn_vkCreateImage(
    device: VkDevice,
    ci: *const VkImageCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkImage,
) -> VkResult {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let ci_ref = unsafe { &*ci };
    if let Some(r) = unsafe { crate::export::try_create_image(gd, ci, alloc, out) } {
        return r;
    }
    let res = unsafe { (*gd).f.create_image.unwrap()((*gd).real, ci, alloc, out) };
    // Gate first: when GNDRM_LOG_VKSHIM is off (the default) this is one flag
    // load and a branch — no lock, no map insert, no formatting. The detector
    // below is a diagnostic, so it must not pay a per-created-image cost when
    // it cannot emit a line anyway.
    if res == VK_SUCCESS && crate::vkshim_enabled() {
        let key = (ci_ref.format, ci_ref.usage, ci_ref.tiling);
        let mut seen = SEEN_IMAGE.lock().unwrap();
        if seen.len() < LOG_IMAGE_FIRST_N && !seen.contains(&key) {
            seen.push(key);
            let pnext = unsafe { pnext_image_create_summary(ci_ref.pNext) };
            crate::logf!(gndrm_obf::obf!(
                "guest image: fmt={} {}x{}x{} type={} mips={} layers={} samples={:#x} tiling={} usage={:#x} flags={:#x} pNext=[{pnext}]",
                ci_ref.format,
                ci_ref.extent.width,
                ci_ref.extent.height,
                ci_ref.extent.depth,
                ci_ref.imageType,
                ci_ref.mipLevels,
                ci_ref.arrayLayers,
                ci_ref.samples,
                ci_ref.tiling,
                ci_ref.usage,
                ci_ref.flags
            ));
        }
    }
    res
}

// ---- guest-command trace (bounded one-shot, always on) -------------------
//
// The guest records its own transfers into command buffers the shim never
// otherwise inspects; the image a presented frame must carry is one of the
// shim's swapchain slot images. This names, for the first TRACE_SC_CMD_N
// recorded commands that touch a slot image, which slot and in which
// direction, so a single run says whether the guest renders into the image
// the shim presents, or into some other image it never presents. Always on
// (not gated by GNDRM_LOG_VKSHIM); bounded by construction so steady-state
// frame pacing stays silent.
const TRACE_SC_CMD_N: usize = 48;
static TRACE_SC_CMD: AtomicUsize = AtomicUsize::new(0);

/// Scan every live swapchain belonging to `gd` for the slot whose image is
/// `image`. Returns (swapchain, slot index).
unsafe fn sc_slot_of(gd: *mut GnDevice, image: VkImage) -> Option<(*mut GnSwapchain, usize)> {
    let g = G.lock().unwrap();
    let mut p = g.swapchains;
    while !p.is_null() {
        unsafe {
            if (*p).dev == gd {
                for (i, s) in (*p).slots.iter().enumerate() {
                    if s.image == image {
                        return Some((p, i));
                    }
                }
            }
            p = (*p).next;
        }
    }
    None
}

/// Emit one bounded `x11 cmd` line. `hits` is only formatted when the line
/// is actually emitted (the counter gate is checked first).
fn trace_sc_cmd(name: &str, cmd: VkCommandBuffer, hits: &str) {
    let n = TRACE_SC_CMD.fetch_add(1, Ordering::Relaxed);
    if n < TRACE_SC_CMD_N {
        crate::logf!(gndrm_obf::obf!("x11 cmd {n}: {name} cb={cmd:?} {hits}"));
    }
}

/// One bounded line the first time the guest resolves/records each traced
/// command through the shim at all (whatever image it targets). Distinguishes
/// "the guest never uses this entry point" from "it does, but never on a
/// swapchain slot image".
/// Resolve the guest's real driver export for a command entry point. Used
/// when the shim intercepts a name but the proxy is not active in this process
/// (then the call must reach the driver unchanged, not be dropped).
unsafe fn driver_sym(name: *const std::os::raw::c_char) -> Option<PFN_vkVoidFunction> {
    let h = crate::driver_handle();
    if h.is_null() {
        return None;
    }
    let sym = unsafe { libc::dlsym(h, name) };
    if sym.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut c_void, PFN_vkVoidFunction>(sym) })
    }
}

/// Call the driver's function for an intercepted command: the proxy's resolved
/// pointer when the device is a shim wrapper, else the driver export (so a
/// process where the proxy never activated still runs the command).
macro_rules! call_cmd {
    ($gd:expr, $field:ident, $name:literal, |$f:ident| $call:expr) => {{
        let gd = $gd;
        let f_opt = unsafe { gd.as_ref() }.and_then(|d| d.f.$field);
        let f_opt = if f_opt.is_none() && gd.is_null() {
            unsafe { driver_sym(concat!($name, "\0").as_ptr() as *const std::os::raw::c_char) }
                .map(|p| unsafe { std::mem::transmute(p) })
        } else {
            f_opt
        };
        if let Some($f) = f_opt {
            unsafe { $call }
        }
    }};
}

static TRACE_SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
fn trace_seen(name: &'static str) {
    // Gate first: this bounded dedupe set exists only to suppress repeat lines
    // of a logf!-gated trace, so with the log off it is dead state and must not
    // take the lock on every intercepted command.
    if !crate::vkshim_enabled() {
        return;
    }
    if let Ok(mut v) = TRACE_SEEN.lock() {
        if v.len() < 32 && !v.contains(&name) {
            crate::logf!(gndrm_obf::obf!(
                "x11 cmd-seen: {name} (guest records it through the shim)"
            ));
            v.push(name);
        }
    }
}

/// The real handle for a guest command buffer (the driver must never see the
/// shim's wrapper), plus the device it belongs to. A command buffer the shim
/// never wrapped is the driver's own and is passed through untouched.
unsafe fn cmd_real_and_dev(cmd: VkCommandBuffer) -> (VkCommandBuffer, *mut GnDevice) {
    let gc = unsafe { find_cmd_any(cmd) };
    if gc.is_null() {
        (cmd, ptr::null_mut())
    } else {
        (unsafe { (*gc).real }, unsafe { (*gc).gd })
    }
}

fn slot_note(s: Option<(*mut GnSwapchain, usize)>) -> String {
    match s {
        Some((sc, i)) => format!("{}", gndrm_obf::obf!("SLOT sc={sc:?} slot={i}")),
        None => "guest".into(),
    }
}

/// A transfer region's extent, as a log field.
fn extent_note(e: VkExtent3D) -> String {
    format!("{}", gndrm_obf::obf!("r0={}x{}", e.width, e.height))
}

/// Extent of the first copy region of a v1 `VkImageCopy` array (does the
/// transfer cover the frame or a sub-rectangle?).
fn copy_extent(n: u32, regions: *const VkImageCopy) -> String {
    if n == 0 || regions.is_null() {
        return String::new();
    }
    extent_note(unsafe { (*regions).extent })
}

/// Extent of the first copy region of a v2 `VkImageCopy2` array.
fn copy_extent2(n: u32, regions: *const VkImageCopy2) -> String {
    if n == 0 || regions.is_null() {
        return String::new();
    }
    extent_note(unsafe { (*regions).extent })
}

/// Extent of the first blit region's destination.
fn blit_extent(n: u32, regions: *const VkImageBlit) -> String {
    if n == 0 || regions.is_null() {
        return String::new();
    }
    let r = unsafe { &*regions };
    format!(
        "{}",
        gndrm_obf::obf!(
            "blit={}x{} -> {}x{}",
            r.srcOffsets[1].x - r.srcOffsets[0].x,
            r.srcOffsets[1].y - r.srcOffsets[0].y,
            r.dstOffsets[1].x - r.dstOffsets[0].x,
            r.dstOffsets[1].y - r.dstOffsets[0].y
        )
    )
}

/// Render-pass trace: DXVK's actual rendering path (it uses render passes, not
/// copies/blits). Log the render area and the first clear value, bounded, so a
/// single run says whether the game is actually rendering fullscreen and what
/// it clears the target to.
static RPB_SEEN: AtomicUsize = AtomicUsize::new(0);

// view -> image and framebuffer -> views, so a render pass can be tied to the
// swapchain slot image it renders into. Bounded by construction: entries are
// added only for views/framebuffers the guest actually creates.
static VIEW_IMAGE: Mutex<Option<HashMap<u64, u64>>> = Mutex::new(None);
static FB_VIEWS: Mutex<Option<HashMap<u64, Vec<u64>>>> = Mutex::new(None);

unsafe extern "C" fn gn_vkCreateImageView(
    device: VkDevice,
    ci: *const VkImageViewCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkImageView,
) -> VkResult {
    let gd = unsafe { find_device(device) };
    let f = unsafe { gd.as_ref() }.and_then(|d| d.f.create_image_view);
    let f = match f {
        Some(f) => Some(f),
        None if gd.is_null() => unsafe { driver_sym(c"vkCreateImageView".as_ptr()) }
            .map(|p| unsafe { std::mem::transmute(p) }),
        None => None,
    };
    let Some(f) = f else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let real_dev = if gd.is_null() {
        device
    } else {
        unsafe { (*gd).real }
    };
    let res = unsafe { f(real_dev, ci, alloc, out) };
    // Gate first: VIEW_IMAGE feeds only the GNDRM_LOG_VKSHIM-gated render-pass
    // trace (view_image / fb_slots), so with the log off this map serves
    // nothing and the per-created-view lock+insert is pure overhead.
    if res == VK_SUCCESS && !ci.is_null() && crate::vkshim_enabled() {
        let img = unsafe { (*ci).image };
        if let Ok(mut m) = VIEW_IMAGE.lock() {
            let m = m.get_or_insert_with(HashMap::new);
            if m.len() < 4096 {
                m.insert(unsafe { *out }, img);
            }
        }
    }
    res
}

unsafe extern "C" fn gn_vkCreateFramebuffer(
    device: VkDevice,
    ci: *const VkFramebufferCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkFramebuffer,
) -> VkResult {
    let gd = unsafe { find_device(device) };
    let f = unsafe { gd.as_ref() }.and_then(|d| d.f.create_framebuffer);
    let f = match f {
        Some(f) => Some(f),
        None if gd.is_null() => unsafe { driver_sym(c"vkCreateFramebuffer".as_ptr()) }
            .map(|p| unsafe { std::mem::transmute(p) }),
        None => None,
    };
    let Some(f) = f else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let real_dev = if gd.is_null() {
        device
    } else {
        unsafe { (*gd).real }
    };
    let res = unsafe { f(real_dev, ci, alloc, out) };
    // Gate first (same shape as VIEW_IMAGE above): FB_VIEWS is read only by the
    // gated render-pass trace, so the to_vec() allocation and the lock are
    // skipped when the log is off.
    if res == VK_SUCCESS && !ci.is_null() && crate::vkshim_enabled() {
        let n = unsafe { (*ci).attachmentCount as usize };
        let views = unsafe { slice_or_empty((*ci).pAttachments, n) }.to_vec();
        if let Ok(mut m) = FB_VIEWS.lock() {
            let m = m.get_or_insert_with(HashMap::new);
            if m.len() < 4096 {
                m.insert(unsafe { *out }, views);
            }
        }
    }
    res
}

// ---- viewport / scissor trace (bounded one-shot, GNDRM_LOG_VKSHIM-gated) --
// The drawn geometry never lands while clears and copies do; a viewport whose
// extent is zero, or whose rect lies outside the swapchain image, clips every
// draw while leaving a loadOp clear intact. Log the first non-empty values and
// any change thereafter, bounded, with the running call count. `logf!`-gated
// (off by default), silent once the budget is spent.
const VS_LOG_N: usize = 6;
static VP_CALLS: AtomicUsize = AtomicUsize::new(0);
static VP_LOGGED: AtomicUsize = AtomicUsize::new(0);
static VP_LAST: Mutex<Option<[u32; 6]>> = Mutex::new(None);
static SC_CALLS: AtomicUsize = AtomicUsize::new(0);
static SC_LOGGED: AtomicUsize = AtomicUsize::new(0);
static SC_LAST: Mutex<Option<[i32; 4]>> = Mutex::new(None);

fn trace_viewports(name: &str, count: u32, vps: *const VkViewport) {
    // Gate first: the viewport trace is logf!-gated, so with the log off the
    // counter and the VP_LAST lock must not run on every vkCmdSetViewport.
    if !crate::vkshim_enabled() {
        return;
    }
    let n = VP_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    if count == 0 || vps.is_null() {
        return;
    }
    let v = unsafe { &*vps };
    let key = [
        v.x.to_bits(),
        v.y.to_bits(),
        v.width.to_bits(),
        v.height.to_bits(),
        v.minDepth.to_bits(),
        v.maxDepth.to_bits(),
    ];
    let Ok(mut last) = VP_LAST.lock() else {
        return;
    };
    if last.as_ref() == Some(&key) || VP_LOGGED.load(Ordering::Relaxed) >= VS_LOG_N {
        return;
    }
    *last = Some(key);
    VP_LOGGED.fetch_add(1, Ordering::Relaxed);
    crate::logf!(gndrm_obf::obf!(
        "x11 {name}: call={n} count={count} vp0 x={} y={} w={} h={} depth={}..{}",
        v.x,
        v.y,
        v.width,
        v.height,
        v.minDepth,
        v.maxDepth
    ));
}

fn trace_scissors(name: &str, count: u32, rects: *const VkRect2D) {
    // Gate first, same as trace_viewports: no counter, no SC_LAST lock when
    // the log is off.
    if !crate::vkshim_enabled() {
        return;
    }
    let n = SC_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    if count == 0 || rects.is_null() {
        return;
    }
    let r = unsafe { &*rects };
    let key = [
        r.offset.x,
        r.offset.y,
        r.extent.width as i32,
        r.extent.height as i32,
    ];
    let Ok(mut last) = SC_LAST.lock() else {
        return;
    };
    if last.as_ref() == Some(&key) || SC_LOGGED.load(Ordering::Relaxed) >= VS_LOG_N {
        return;
    }
    *last = Some(key);
    SC_LOGGED.fetch_add(1, Ordering::Relaxed);
    crate::logf!(gndrm_obf::obf!(
        "x11 {name}: call={n} count={count} sc0 off=({},{}) ext={}x{}",
        r.offset.x,
        r.offset.y,
        r.extent.width,
        r.extent.height
    ));
}

unsafe extern "C" fn gn_vkCmdSetViewport(
    cmd: VkCommandBuffer,
    first: u32,
    count: u32,
    vps: *const VkViewport,
) {
    trace_seen("vkCmdSetViewport");
    trace_viewports("vkCmdSetViewport", count, vps);
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    call_cmd!(gd, cmd_set_viewport, "vkCmdSetViewport", |f| f(
        rcmd, first, count, vps
    ));
}

unsafe extern "C" fn gn_vkCmdSetViewportWithCount(
    cmd: VkCommandBuffer,
    count: u32,
    vps: *const VkViewport,
) {
    trace_seen("vkCmdSetViewportWithCount");
    trace_viewports("vkCmdSetViewportWithCount", count, vps);
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    call_cmd!(
        gd,
        cmd_set_viewport_with_count,
        "vkCmdSetViewportWithCount",
        |f| f(rcmd, count, vps)
    );
}

unsafe extern "C" fn gn_vkCmdSetScissor(
    cmd: VkCommandBuffer,
    first: u32,
    count: u32,
    rects: *const VkRect2D,
) {
    trace_seen("vkCmdSetScissor");
    trace_scissors("vkCmdSetScissor", count, rects);
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    call_cmd!(gd, cmd_set_scissor, "vkCmdSetScissor", |f| f(
        rcmd, first, count, rects
    ));
}

unsafe extern "C" fn gn_vkCmdSetScissorWithCount(
    cmd: VkCommandBuffer,
    count: u32,
    rects: *const VkRect2D,
) {
    trace_seen("vkCmdSetScissorWithCount");
    trace_scissors("vkCmdSetScissorWithCount", count, rects);
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    call_cmd!(
        gd,
        cmd_set_scissor_with_count,
        "vkCmdSetScissorWithCount",
        |f| f(rcmd, count, rects)
    );
}

// ---- render-pass load/store capture (bounded, always on) -----------------
// vkCmdBeginRenderPass names a VkRenderPass; its attachments' loadOp/storeOp
// live in the render pass object, not the begin info. Record each render pass's
// subpass-0 colour attachment ops so the begin trace can name them (a
// storeOp=DONT_CARE discards draws while a loadOp clear still shows).
/// Subpass-0 colour (loadOp, storeOp) pairs of one render pass.
type RenderPassOps = Vec<(i32, i32)>;
/// Render pass handle -> its subpass-0 colour (loadOp, storeOp) pairs.
type RenderPassOpsMap = HashMap<u64, RenderPassOps>;
static RP_OPS: Mutex<Option<RenderPassOpsMap>> = Mutex::new(None);

/// Subpass-0 colour attachment (loadOp, storeOp) pairs. `u32::MAX` is
/// VK_ATTACHMENT_UNUSED; the pair `(-1, -1)` marks one.
unsafe fn render_pass_ops(ci: &VkRenderPassCreateInfo) -> RenderPassOps {
    let mut out = Vec::new();
    if ci.subpassCount == 0 || ci.pSubpasses.is_null() {
        return out;
    }
    let sp = unsafe { &*ci.pSubpasses };
    let n = sp.colorAttachmentCount as usize;
    for k in 0..n {
        if sp.pColorAttachments.is_null() {
            break;
        }
        let r = unsafe { &*sp.pColorAttachments.add(k) };
        if r.attachment == u32::MAX || r.attachment as usize >= ci.attachmentCount as usize {
            out.push((-1, -1));
            continue;
        }
        let a = unsafe { &*ci.pAttachments.add(r.attachment as usize) };
        out.push((a.loadOp, a.storeOp));
    }
    out
}

/// The subpass-0 colour (loadOp, storeOp) pairs of a render pass the shim
/// recorded, as a log field; `?` when it was never created through the shim.
fn rp_ops_note(rp: u64) -> String {
    let ops = RP_OPS
        .lock()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.get(&rp).cloned()));
    match ops {
        Some(v) if !v.is_empty() => v
            .iter()
            .map(|(l, s)| format!("{l}/{s}"))
            .collect::<Vec<_>>()
            .join(","),
        _ => "?".into(),
    }
}

unsafe extern "C" fn gn_vkCreateRenderPass(
    device: VkDevice,
    ci: *const VkRenderPassCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut u64,
) -> VkResult {
    let gd = unsafe { find_device(device) };
    let f = unsafe { gd.as_ref() }.and_then(|d| d.f.create_render_pass);
    let f = match f {
        Some(f) => Some(f),
        None if gd.is_null() => unsafe { driver_sym(c"vkCreateRenderPass".as_ptr()) }
            .map(|p| unsafe { std::mem::transmute(p) }),
        None => None,
    };
    let Some(f) = f else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let real_dev = if gd.is_null() {
        device
    } else {
        unsafe { (*gd).real }
    };
    let res = unsafe { f(real_dev, ci, alloc, out) };
    // Gate first (same shape as VIEW_IMAGE above): RP_OPS is read only by the
    // gated render-pass trace via rp_ops_note, so render_pass_ops()'s Vec
    // allocation and the lock are skipped when the log is off.
    if res == VK_SUCCESS && !ci.is_null() && crate::vkshim_enabled() {
        let ops = unsafe { render_pass_ops(&*ci) };
        if let Ok(mut m) = RP_OPS.lock() {
            let m = m.get_or_insert_with(HashMap::new);
            if m.len() < 4096 {
                m.insert(unsafe { *out }, ops);
            }
        }
    }
    res
}

/// vkCreateSamplerYcbcrConversion is a device-level command whose handle the
/// shim wraps. Intercepted so it is never handed to the guest as the driver's
/// raw entry point: called with a shim device wrapper, Turnip's own
/// implementation faults on the wrapper (gamescope resolves this at Vulkan
/// device init, before any window exists). Forward with the real device.
unsafe extern "C" fn gn_vkCreateSamplerYcbcrConversion(
    device: VkDevice,
    ci: *const c_void,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkSamplerYcbcrConversion,
) -> VkResult {
    let gd = unsafe { find_device(device) };
    let f = unsafe { gd.as_ref() }.and_then(|d| d.f.create_sampler_ycbcr_conversion);
    let f = match f {
        Some(f) => Some(f),
        None if gd.is_null() => unsafe { driver_sym(c"vkCreateSamplerYcbcrConversion".as_ptr()) }
            .map(|p| unsafe { std::mem::transmute(p) }),
        None => None,
    };
    let Some(f) = f else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let real_dev = if gd.is_null() {
        device
    } else {
        unsafe { (*gd).real }
    };
    unsafe { f(real_dev, ci, alloc, out) }
}

/// Resolve a framebuffer handle to the swapchain slots its attachments use.
unsafe fn fb_slots(gd: *mut GnDevice, fb: u64) -> Vec<usize> {
    let mut out = Vec::new();
    let views = FB_VIEWS
        .lock()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.get(&fb).cloned()))
        .unwrap_or_default();
    for v in views {
        let img = VIEW_IMAGE
            .lock()
            .ok()
            .and_then(|m| m.as_ref().and_then(|m| m.get(&v).copied()));
        if let Some(img) = img {
            if let Some((_, i)) = unsafe { sc_slot_of(gd, img) } {
                if !out.contains(&i) {
                    out.push(i);
                }
            }
        }
    }
    out
}

/// Render-pass trace (vkCmdBeginRenderPass2, DXVK's core-1.2 path): the
/// framebuffer resolves through the view/image map to the swapchain slot the
/// pass renders into, so one run says whether the guest renders into the image
/// the shim presents.
unsafe fn view_image(view: u64) -> Option<u64> {
    VIEW_IMAGE
        .lock()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.get(&view).copied()))
}

/// Dynamic-rendering trace: the color attachment imageView resolves through
/// the view/image map to the image the guest renders into; if it is a swapchain
/// slot, the guest renders straight into the image the shim presents.
unsafe extern "C" fn gn_vkCmdBeginRendering(cmd: VkCommandBuffer, info: *const VkRenderingInfo) {
    trace_seen("vkCmdBeginRendering");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() && !info.is_null() {
        let ri = unsafe { &*info };
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        if n < 12 && !ri.pColorAttachments.is_null() {
            let mut parts: Vec<String> = Vec::new();
            let na = (ri.colorAttachmentCount as usize).min(4);
            for k in 0..na {
                let a = unsafe { &*ri.pColorAttachments.add(k) };
                let img = unsafe { view_image(a.imageView) };
                let note = match img {
                    Some(i) => match unsafe { sc_slot_of(gd, i) } {
                        Some((sc, si)) => {
                            format!("{}", gndrm_obf::obf!("SLOT sc={sc:?} slot={si}"))
                        }
                        None => format!("{}", gndrm_obf::obf!("img={:#x}", i as usize)),
                    },
                    None => format!("{}", gndrm_obf::obf!("view={:#x}", a.imageView as usize)),
                };
                let c = a.clearValue.float32;
                parts.push(format!(
                    "{}",
                    gndrm_obf::obf!(
                        "a{k}[{note} layout={} load={} store={} clear=({},{},{},{})]",
                        a.imageLayout,
                        a.loadOp,
                        a.storeOp,
                        c[0],
                        c[1],
                        c[2],
                        c[3]
                    )
                ));
            }
            crate::logf!(gndrm_obf::obf!(
                "x11 rndr {n}: {}x{} colors={na} {}",
                ri.renderArea.extent.width,
                ri.renderArea.extent.height,
                parts.join(" ")
            ));
        }
    }
    call_cmd!(gd, cmd_begin_rendering, "vkCmdBeginRendering", |f| f(
        rcmd, info
    ));
}

unsafe extern "C" fn gn_vkCmdBeginRenderPass2(
    cmd: VkCommandBuffer,
    info: *const VkRenderPassBeginInfo,
    sub: *const VkSubpassBeginInfo,
) {
    trace_seen("vkCmdBeginRenderPass2");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() && !info.is_null() {
        static N2: AtomicUsize = AtomicUsize::new(0);
        let n = N2.fetch_add(1, Ordering::Relaxed);
        if n < 12 {
            let bi = unsafe { &*info };
            let slots = unsafe { fb_slots(gd, bi.framebuffer) };
            let c = if bi.clearValueCount > 0 && !bi.pClearValues.is_null() {
                unsafe { (*bi.pClearValues).float32 }
            } else {
                [0f32; 4]
            };
            crate::logf!(gndrm_obf::obf!(
                "x11 rpb2 {n}: {}x{} off=({},{}) fb={:#x} rp={:#x} clears={} clear0=({},{},{},{}) rpOps={} slots={slots:?}",
                bi.renderArea.extent.width,
                bi.renderArea.extent.height,
                bi.renderArea.offset.x,
                bi.renderArea.offset.y,
                bi.framebuffer as usize,
                bi.renderPass as usize,
                bi.clearValueCount,
                c[0], c[1], c[2], c[3],
                rp_ops_note(bi.renderPass)
            ));
        }
    }
    call_cmd!(gd, cmd_begin_render_pass2, "vkCmdBeginRenderPass2", |f| f(
        rcmd, info, sub
    ));
}

unsafe extern "C" fn gn_vkCmdBeginRenderPass(
    cmd: VkCommandBuffer,
    info: *const VkRenderPassBeginInfo,
    contents: i32,
) {
    trace_seen("vkCmdBeginRenderPass");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() && !info.is_null() {
        let n = RPB_SEEN.fetch_add(1, Ordering::Relaxed);
        if n < 12 {
            let bi = unsafe { &*info };
            let c = if bi.clearValueCount > 0 && !bi.pClearValues.is_null() {
                unsafe { (*bi.pClearValues).float32 }
            } else {
                [0f32; 4]
            };
            let slots = unsafe { fb_slots(gd, bi.framebuffer) };
            crate::logf!(gndrm_obf::obf!(
                "x11 rpb {n}: {}x{} off=({},{}) fb={:#x} rp={:#x} clears={} clear0=({},{},{},{}) contents={contents} rpOps={} slots={slots:?}",
                bi.renderArea.extent.width,
                bi.renderArea.extent.height,
                bi.renderArea.offset.x,
                bi.renderArea.offset.y,
                bi.framebuffer as usize,
                bi.renderPass as usize,
                bi.clearValueCount,
                c[0], c[1], c[2], c[3],
                rp_ops_note(bi.renderPass)
            ));
        }
    }
    call_cmd!(gd, cmd_begin_render_pass, "vkCmdBeginRenderPass", |f| f(
        rcmd, info, contents
    ));
}

unsafe extern "C" fn gn_vkCmdClearColorImage(
    cmd: VkCommandBuffer,
    image: VkImage,
    layout: VkImageLayout,
    color: *const VkClearColorValue,
    n: u32,
    _ranges: *const VkImageSubresourceRange,
) {
    trace_seen("vkCmdClearColorImage");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() {
        {
            static ANY: AtomicUsize = AtomicUsize::new(0);
            let n2 = ANY.fetch_add(1, Ordering::Relaxed);
            if n2 < 8 {
                crate::logf!(gndrm_obf::obf!(
                    "x11 any clear: img={:#x} layout={layout}",
                    image as usize
                ));
            }
        }
        if let Some((sc, i)) = unsafe { sc_slot_of(gd, image) } {
            let c = if color.is_null() {
                [0f32; 4]
            } else {
                unsafe { (*color).float32 }
            };
            trace_sc_cmd(
                "vkCmdClearColorImage",
                cmd,
                &format!(
                    "{}",
                    gndrm_obf::obf!(
                        "SLOT sc={sc:?} slot={i} layout={layout} n={n} clear=({},{},{},{})",
                        c[0],
                        c[1],
                        c[2],
                        c[3]
                    )
                ),
            );
        }
    }
    call_cmd!(gd, cmd_clear_color_image, "vkCmdClearColorImage", |f| f(
        rcmd, image, layout, color, n, _ranges
    ));
}

unsafe extern "C" fn gn_vkCmdCopyImage(
    cmd: VkCommandBuffer,
    src: VkImage,
    src_layout: VkImageLayout,
    dst: VkImage,
    dst_layout: VkImageLayout,
    n: u32,
    regions: *const VkImageCopy,
) {
    trace_seen("vkCmdCopyImage");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() {
        let s = unsafe { sc_slot_of(gd, src) };
        let d = unsafe { sc_slot_of(gd, dst) };
        if s.is_some() || d.is_some() {
            trace_sc_cmd(
                "vkCmdCopyImage",
                cmd,
                &format!("{}", gndrm_obf::obf!(
                    "src={:#x}({}) dst={:#x}({}) srcLayout={src_layout} dstLayout={dst_layout} n={n} {}",
                    src as usize,
                    slot_note(s),
                    dst as usize,
                    slot_note(d),
                    copy_extent(n, regions)
                )),
            );
        }
    }
    call_cmd!(gd, cmd_copy_image, "vkCmdCopyImage", |f| f(
        rcmd, src, src_layout, dst, dst_layout, n, regions
    ));
}

unsafe extern "C" fn gn_vkCmdCopyImage2(cmd: VkCommandBuffer, info: *const VkCopyImageInfo2) {
    trace_seen("vkCmdCopyImage2");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() && !info.is_null() {
        let ci = unsafe { &*info };
        {
            static ANY: AtomicUsize = AtomicUsize::new(0);
            let n2 = ANY.fetch_add(1, Ordering::Relaxed);
            if n2 < 12 {
                crate::logf!(gndrm_obf::obf!(
                    "x11 any copyimg2: src={:#x} dst={:#x} {}x{}",
                    ci.srcImage as usize,
                    ci.dstImage as usize,
                    ci.regionCount,
                    if ci.regionCount > 0 && !ci.pRegions.is_null() {
                        unsafe { (*ci.pRegions).extent.width }
                    } else {
                        0
                    }
                ));
            }
        }
        let s = unsafe { sc_slot_of(gd, ci.srcImage) };
        let d = unsafe { sc_slot_of(gd, ci.dstImage) };
        if s.is_some() || d.is_some() {
            trace_sc_cmd(
                "vkCmdCopyImage2",
                cmd,
                &format!(
                    "{}",
                    gndrm_obf::obf!(
                        "src={:#x}({}) dst={:#x}({}) srcLayout={} dstLayout={} n={} {}",
                        ci.srcImage as usize,
                        slot_note(s),
                        ci.dstImage as usize,
                        slot_note(d),
                        ci.srcImageLayout,
                        ci.dstImageLayout,
                        ci.regionCount,
                        copy_extent2(ci.regionCount, ci.pRegions)
                    )
                ),
            );
        }
    }
    call_cmd!(gd, cmd_copy_image2, "vkCmdCopyImage2", |f| f(rcmd, info));
}

unsafe extern "C" fn gn_vkCmdBlitImage(
    cmd: VkCommandBuffer,
    src: VkImage,
    src_layout: VkImageLayout,
    dst: VkImage,
    dst_layout: VkImageLayout,
    n: u32,
    regions: *const VkImageBlit,
    filter: VkFilter,
) {
    trace_seen("vkCmdBlitImage");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() {
        let s = unsafe { sc_slot_of(gd, src) };
        let d = unsafe { sc_slot_of(gd, dst) };
        if s.is_some() || d.is_some() {
            trace_sc_cmd(
                "vkCmdBlitImage",
                cmd,
                &format!("{}", gndrm_obf::obf!(
                    "src={:#x}({}) dst={:#x}({}) srcLayout={src_layout} dstLayout={dst_layout} n={n} filter={filter} {}",
                    src as usize,
                    slot_note(s),
                    dst as usize,
                    slot_note(d),
                    blit_extent(n, regions)
                )),
            );
        }
    }
    call_cmd!(gd, cmd_blit_image, "vkCmdBlitImage", |f| f(
        rcmd, src, src_layout, dst, dst_layout, n, regions, filter
    ));
}

unsafe extern "C" fn gn_vkCmdBlitImage2(cmd: VkCommandBuffer, info: *const VkBlitImageInfo2) {
    trace_seen("vkCmdBlitImage2");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() && !info.is_null() {
        let ci = unsafe { &*info };
        let s = unsafe { sc_slot_of(gd, ci.srcImage) };
        let d = unsafe { sc_slot_of(gd, ci.dstImage) };
        if s.is_some() || d.is_some() {
            trace_sc_cmd(
                "vkCmdBlitImage2",
                cmd,
                &format!(
                    "{}",
                    gndrm_obf::obf!(
                        "src={:#x}({}) dst={:#x}({}) srcLayout={} dstLayout={} n={} filter={}",
                        ci.srcImage as usize,
                        slot_note(s),
                        ci.dstImage as usize,
                        slot_note(d),
                        ci.srcImageLayout,
                        ci.dstImageLayout,
                        ci.regionCount,
                        ci.filter
                    )
                ),
            );
        }
    }
    call_cmd!(gd, cmd_blit_image2, "vkCmdBlitImage2", |f| f(rcmd, info));
}

/// synchronization2 variant of the layout-transition trace (DXVK's actual
/// path): name the slot each image barrier in the dependency targets.
unsafe extern "C" fn gn_vkCmdPipelineBarrier2(cmd: VkCommandBuffer, dep: *const VkDependencyInfo) {
    trace_seen("vkCmdPipelineBarrier2");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() && !dep.is_null() {
        let di = unsafe { &*dep };
        if di.imageMemoryBarrierCount > 0 && !di.pImageMemoryBarriers.is_null() {
            for k in 0..di.imageMemoryBarrierCount as usize {
                let b = unsafe { &*di.pImageMemoryBarriers.add(k) };
                {
                    static ANY: AtomicUsize = AtomicUsize::new(0);
                    let n2 = ANY.fetch_add(1, Ordering::Relaxed);
                    if n2 < 12 {
                        crate::logf!(gndrm_obf::obf!(
                            "x11 any bar2: img={:#x} srcStage={:#x} dstStage={:#x}",
                            b.image as usize,
                            b.srcStageMask,
                            b.dstStageMask
                        ));
                    }
                }
                if let Some((sc, i)) = unsafe { sc_slot_of(gd, b.image) } {
                    trace_sc_cmd(
                        "vkCmdPipelineBarrier2(image)",
                        cmd,
                        &format!("{}", gndrm_obf::obf!(
                            "SLOT sc={sc:?} slot={i} srcStage={:#x} srcAccess={:#x} dstStage={:#x} dstAccess={:#x}",
                            b.srcStageMask, b.srcAccessMask, b.dstStageMask, b.dstAccessMask
                        )),
                    );
                }
            }
        }
    }
    call_cmd!(gd, cmd_pipeline_barrier2, "vkCmdPipelineBarrier2", |f| f(
        rcmd, dep
    ));
}

unsafe extern "C" fn gn_vkCmdCopyBufferToImage(
    cmd: VkCommandBuffer,
    src_buffer: VkBuffer,
    dst: VkImage,
    dst_layout: VkImageLayout,
    n: u32,
    regions: *const VkBufferImageCopy,
) {
    trace_seen("vkCmdCopyBufferToImage");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() {
        if let Some((sc, i)) = unsafe { sc_slot_of(gd, dst) } {
            trace_sc_cmd(
                "vkCmdCopyBufferToImage",
                cmd,
                &format!(
                    "{}",
                    gndrm_obf::obf!(
                    "buf={src_buffer:#x} -> SLOT sc={sc:?} slot={i} dstLayout={dst_layout} n={n}"
                )
                ),
            );
        }
    }
    call_cmd!(
        gd,
        cmd_copy_buffer_to_image,
        "vkCmdCopyBufferToImage",
        |f| f(rcmd, src_buffer, dst, dst_layout, n, regions)
    );
}

unsafe extern "C" fn gn_vkCmdCopyBufferToImage2(
    cmd: VkCommandBuffer,
    info: *const VkCopyBufferToImageInfo2,
) {
    trace_seen("vkCmdCopyBufferToImage2");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() && !info.is_null() {
        let ci = unsafe { &*info };
        if let Some((sc, i)) = unsafe { sc_slot_of(gd, ci.dstImage) } {
            trace_sc_cmd(
                "vkCmdCopyBufferToImage2",
                cmd,
                &format!(
                    "{}",
                    gndrm_obf::obf!(
                        "buf={:#x} -> SLOT sc={sc:?} slot={i} dstLayout={} n={}",
                        ci.srcBuffer as usize,
                        ci.dstImageLayout,
                        ci.regionCount
                    )
                ),
            );
        }
    }
    call_cmd!(
        gd,
        cmd_copy_buffer_to_image2,
        "vkCmdCopyBufferToImage2",
        |f| f(rcmd, info)
    );
}

/// Pipeline barriers carry the guest's layout transitions of the swapchain
/// image; a transition to/from PRESENT_SRC proves the guest is treating that
/// exact slot image as the frame it will present.
unsafe extern "C" fn gn_vkCmdPipelineBarrier(
    cmd: VkCommandBuffer,
    src_stage: VkPipelineStageFlags,
    dst_stage: VkPipelineStageFlags,
    flags: u32,
    mb_count: u32,
    mb: *const c_void,
    bb_count: u32,
    bb: *const c_void,
    ib_count: u32,
    ib: *const VkImageMemoryBarrier,
) {
    trace_seen("vkCmdPipelineBarrier");
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if !gd.is_null() && ib_count > 0 && !ib.is_null() {
        for k in 0..ib_count as usize {
            let b = unsafe { &*ib.add(k) };
            if let Some((sc, i)) = unsafe { sc_slot_of(gd, b.image) } {
                trace_sc_cmd(
                    "vkCmdPipelineBarrier(image)",
                    cmd,
                    &format!("{}", gndrm_obf::obf!(
                        "SLOT sc={sc:?} slot={i} {}->{} srcStage={src_stage:#x} dstStage={dst_stage:#x}",
                        b.oldLayout, b.newLayout
                    )),
                );
            }
        }
    }
    call_cmd!(gd, cmd_pipeline_barrier, "vkCmdPipelineBarrier", |f| f(
        rcmd, src_stage, dst_stage, flags, mb_count, mb, bb_count, bb, ib_count, ib
    ));
}

// Command buffers the shim wraps can arrive at the driver nested in
// `pCommandBuffers`; translate them (registry table) before forwarding, or the
// driver walks a 72-byte `GnCmd` wrapper as its own command buffer.
unsafe extern "C" fn gn_vkCmdExecuteCommands(
    cmd: VkCommandBuffer,
    count: u32,
    cmds: *const VkCommandBuffer,
) {
    let (rcmd, gd) = unsafe { cmd_real_and_dev(cmd) };
    if gd.is_null() {
        // The primary command buffer is not a shim wrapper, so the shim has no
        // device to route the driver export through and no way to translate the
        // secondary handles. Forwarding raw is what fed the driver wrapper
        // pointers before; dropping it is silent. Say it happened.
        loud("exec_cmds.miss", || {
            format!("{}", gndrm_obf::obf!(
                "proxy: vkCmdExecuteCommands CB MISS cmd={:#x} count={count} — secondaries not translated",
                cmd as usize
            ))
        });
        return;
    }
    let mut stage = NestedStage::new();
    let mut args = [cmd as usize, count as usize, cmds as usize, 0, 0, 0];
    unsafe { translate_nested("vkCmdExecuteCommands", &mut args, &mut stage) };
    if let Some(f) = unsafe { (*gd).f.execute_commands } {
        unsafe { f(rcmd, count, args[2] as *const VkCommandBuffer) };
    }
}

unsafe extern "C" fn gn_vkFreeCommandBuffers(
    device: VkDevice,
    pool: VkCommandPool,
    count: u32,
    cmds: *const VkCommandBuffer,
) {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return;
    }
    let mut rc = vec![ptr::null_mut::<c_void>(); count as usize];
    {
        let _g = G.lock().unwrap();
        for (i, slot) in rc.iter_mut().enumerate() {
            let cmd = unsafe { *cmds.add(i) };
            let gc = unsafe { find_cmd(gd, cmd) };
            if !gc.is_null() {
                *slot = unsafe { (*gc).real };
                unsafe {
                    let mut pp: *mut *mut GnCmd = &raw mut (*gd).cmds;
                    while !(*pp).is_null() && *pp != gc {
                        pp = &raw mut (**pp).next;
                    }
                    if !(*pp).is_null() {
                        *pp = (*gc).next;
                    }
                    drop(Box::from_raw(gc));
                }
            } else {
                *slot = cmd;
            }
        }
    }
    unsafe {
        if let Some(free_cmd) = (*gd).f.free_cmd {
            free_cmd((*gd).real, pool, count, rc.as_ptr());
        }
    }
}

// ------------------------------------------------------- dispatch tables

/// Handler registry: the commands the shim implements, keyed by name. This is
/// the shim's own implementation surface, not a dispatch policy — a name
/// absent here is served by the generated registry policy in
/// [`serve_unlisted`], so an unimplemented command can never be handed to the
/// guest as the driver's raw pointer when its argument 0 is a wrapped handle.
/// `HandlerScope::ProxyOnly` marks the handlers that only work while the WSI
/// proxy is active (instances/surfaces); `Always` marks the device handlers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HandlerScope {
    ProxyOnly,
    Always,
}

static SHIM_HANDLERS: &[(&CStr, HandlerScope, PFN_vkVoidFunction)] = &[
    (c"vkCreateInstance", HandlerScope::ProxyOnly, unsafe {
        std::mem::transmute(
            gn_vkCreateInstance
                as unsafe extern "C" fn(
                    *const VkInstanceCreateInfo,
                    *const VkAllocationCallbacks,
                    *mut VkInstance,
                ) -> VkResult,
        )
    }),
    (c"vkDestroyInstance", HandlerScope::ProxyOnly, unsafe {
        std::mem::transmute(
            gn_vkDestroyInstance as unsafe extern "C" fn(VkInstance, *const VkAllocationCallbacks),
        )
    }),
    (
        c"vkEnumerateInstanceExtensionProperties",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkEnumerateInstanceExtensionProperties
                    as unsafe extern "C" fn(
                        *const c_char,
                        *mut u32,
                        *mut VkExtensionProperties,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkEnumerateDeviceExtensionProperties",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkEnumerateDeviceExtensionProperties
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        *const c_char,
                        *mut u32,
                        *mut VkExtensionProperties,
                    ) -> VkResult,
            )
        },
    ),
    (c"vkCreateDevice", HandlerScope::ProxyOnly, unsafe {
        std::mem::transmute(
            gn_vkCreateDevice
                as unsafe extern "C" fn(
                    VkPhysicalDevice,
                    *const VkDeviceCreateInfo,
                    *const VkAllocationCallbacks,
                    *mut VkDevice,
                ) -> VkResult,
        )
    }),
    (c"vkGetDeviceProcAddr", HandlerScope::ProxyOnly, unsafe {
        std::mem::transmute(
            gn_vkGetDeviceProcAddr
                as unsafe extern "C" fn(VkDevice, *const c_char) -> PFN_vkVoidFunction,
        )
    }),
    (
        c"vkCreateWaylandSurfaceKHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkCreateWaylandSurfaceKHR
                    as unsafe extern "C" fn(
                        VkInstance,
                        *const VkWaylandSurfaceCreateInfoKHR,
                        *const VkAllocationCallbacks,
                        *mut VkSurfaceKHR,
                    ) -> VkResult,
            )
        },
    ),
    (c"vkCreateXlibSurfaceKHR", HandlerScope::ProxyOnly, unsafe {
        std::mem::transmute(
            gn_vkCreateXlibSurfaceKHR
                as unsafe extern "C" fn(
                    VkInstance,
                    *const VkXlibSurfaceCreateInfoKHR,
                    *const VkAllocationCallbacks,
                    *mut VkSurfaceKHR,
                ) -> VkResult,
        )
    }),
    (c"vkCreateXcbSurfaceKHR", HandlerScope::ProxyOnly, unsafe {
        std::mem::transmute(
            gn_vkCreateXcbSurfaceKHR
                as unsafe extern "C" fn(
                    VkInstance,
                    *const VkXcbSurfaceCreateInfoKHR,
                    *const VkAllocationCallbacks,
                    *mut VkSurfaceKHR,
                ) -> VkResult,
        )
    }),
    (c"vkDestroySurfaceKHR", HandlerScope::ProxyOnly, unsafe {
        std::mem::transmute(
            gn_vkDestroySurfaceKHR
                as unsafe extern "C" fn(VkInstance, VkSurfaceKHR, *const VkAllocationCallbacks),
        )
    }),
    (
        c"vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceSurfaceCapabilitiesKHR
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        VkSurfaceKHR,
                        *mut VkSurfaceCapabilitiesKHR,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceSurfaceCapabilities2KHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceSurfaceCapabilities2KHR
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        *const VkPhysicalDeviceSurfaceInfo2KHR,
                        *mut VkSurfaceCapabilities2KHR,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceSurfaceFormatsKHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceSurfaceFormatsKHR
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        VkSurfaceKHR,
                        *mut u32,
                        *mut VkSurfaceFormatKHR,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceSurfacePresentModesKHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceSurfacePresentModesKHR
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        VkSurfaceKHR,
                        *mut u32,
                        *mut VkPresentModeKHR,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceSurfaceSupportKHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceSurfaceSupportKHR
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        u32,
                        VkSurfaceKHR,
                        *mut VkBool32,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceWaylandPresentationSupportKHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceWaylandPresentationSupportKHR
                    as unsafe extern "C" fn(VkPhysicalDevice, u32, *mut wl_display) -> VkBool32,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceXlibPresentationSupportKHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceXlibPresentationSupportKHR
                    as unsafe extern "C" fn(VkPhysicalDevice, u32, *mut c_void, usize) -> VkBool32,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceXcbPresentationSupportKHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceXcbPresentationSupportKHR
                    as unsafe extern "C" fn(VkPhysicalDevice, u32, *mut c_void, u32) -> VkBool32,
            )
        },
    ),
    (c"vkCreateSwapchainKHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCreateSwapchainKHR
                as unsafe extern "C" fn(
                    VkDevice,
                    *const VkSwapchainCreateInfoKHR,
                    *const VkAllocationCallbacks,
                    *mut VkSwapchainKHR,
                ) -> VkResult,
        )
    }),
    (c"vkDestroySwapchainKHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkDestroySwapchainKHR
                as unsafe extern "C" fn(VkDevice, VkSwapchainKHR, *const VkAllocationCallbacks),
        )
    }),
    (c"vkGetSwapchainImagesKHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkGetSwapchainImagesKHR
                as unsafe extern "C" fn(
                    VkDevice,
                    VkSwapchainKHR,
                    *mut u32,
                    *mut VkImage,
                ) -> VkResult,
        )
    }),
    (c"vkQueuePresentKHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkQueuePresentKHR
                as unsafe extern "C" fn(VkQueue, *const VkPresentInfoKHR) -> VkResult,
        )
    }),
    (c"vkAcquireNextImageKHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkAcquireNextImageKHR
                as unsafe extern "C" fn(
                    VkDevice,
                    VkSwapchainKHR,
                    u64,
                    VkSemaphore,
                    VkFence,
                    *mut u32,
                ) -> VkResult,
        )
    }),
    (c"vkAcquireNextImage2KHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkAcquireNextImage2KHR
                as unsafe extern "C" fn(
                    VkDevice,
                    *const VkAcquireNextImageInfoKHR,
                    *mut u32,
                ) -> VkResult,
        )
    }),
    (c"vkQueueSubmit", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkQueueSubmit
                as unsafe extern "C" fn(VkQueue, u32, *const VkSubmitInfo, VkFence) -> VkResult,
        )
    }),
    (c"vkQueueSubmit2", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkQueueSubmit2
                as unsafe extern "C" fn(VkQueue, u32, *const VkSubmitInfo2, VkFence) -> VkResult,
        )
    }),
    (c"vkQueueSubmit2KHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkQueueSubmit2
                as unsafe extern "C" fn(VkQueue, u32, *const VkSubmitInfo2, VkFence) -> VkResult,
        )
    }),
    (c"vkWaitSemaphoresKHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkWaitSemaphores
                as unsafe extern "C" fn(VkDevice, *const VkSemaphoreWaitInfo, u64) -> VkResult,
        )
    }),
    (c"vkWaitSemaphores", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkWaitSemaphores
                as unsafe extern "C" fn(VkDevice, *const VkSemaphoreWaitInfo, u64) -> VkResult,
        )
    }),
    (c"vkAllocateCommandBuffers", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkAllocateCommandBuffers
                as unsafe extern "C" fn(
                    VkDevice,
                    *const VkCommandBufferAllocateInfo,
                    *mut VkCommandBuffer,
                ) -> VkResult,
        )
    }),
    (c"vkFreeCommandBuffers", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkFreeCommandBuffers
                as unsafe extern "C" fn(VkDevice, VkCommandPool, u32, *const VkCommandBuffer),
        )
    }),
    (c"vkCmdExecuteCommands", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdExecuteCommands
                as unsafe extern "C" fn(VkCommandBuffer, u32, *const VkCommandBuffer),
        )
    }),
    (c"vkCreateGraphicsPipelines", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCreateGraphicsPipelines
                as unsafe extern "C" fn(
                    VkDevice,
                    VkPipelineCache,
                    u32,
                    *const VkGraphicsPipelineCreateInfo,
                    *const VkAllocationCallbacks,
                    *mut VkPipeline,
                ) -> VkResult,
        )
    }),
    (c"vkCreateImage", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCreateImage
                as unsafe extern "C" fn(
                    VkDevice,
                    *const VkImageCreateInfo,
                    *const VkAllocationCallbacks,
                    *mut VkImage,
                ) -> VkResult,
        )
    }),
    (c"vkGetDeviceQueue", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkGetDeviceQueue as unsafe extern "C" fn(VkDevice, u32, u32, *mut VkQueue),
        )
    }),
    (c"vkGetDeviceQueue2", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkGetDeviceQueue2
                as unsafe extern "C" fn(VkDevice, *const VkDeviceQueueInfo2, *mut VkQueue),
        )
    }),
    (c"vkDestroyDevice", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkDestroyDevice as unsafe extern "C" fn(VkDevice, *const VkAllocationCallbacks),
        )
    }),
    // Guest-command trace: intercept the recorded transfers that can write a
    // swapchain slot image, name the slot, then tail-call the driver.
    (c"vkCmdClearColorImage", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdClearColorImage
                as unsafe extern "C" fn(
                    VkCommandBuffer,
                    VkImage,
                    VkImageLayout,
                    *const VkClearColorValue,
                    u32,
                    *const VkImageSubresourceRange,
                ),
        )
    }),
    (c"vkCmdCopyImage", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdCopyImage
                as unsafe extern "C" fn(
                    VkCommandBuffer,
                    VkImage,
                    VkImageLayout,
                    VkImage,
                    VkImageLayout,
                    u32,
                    *const VkImageCopy,
                ),
        )
    }),
    (c"vkCmdCopyImage2KHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdCopyImage2 as unsafe extern "C" fn(VkCommandBuffer, *const VkCopyImageInfo2),
        )
    }),
    (c"vkCmdCopyImage2", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdCopyImage2 as unsafe extern "C" fn(VkCommandBuffer, *const VkCopyImageInfo2),
        )
    }),
    (c"vkCmdBlitImage", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdBlitImage
                as unsafe extern "C" fn(
                    VkCommandBuffer,
                    VkImage,
                    VkImageLayout,
                    VkImage,
                    VkImageLayout,
                    u32,
                    *const VkImageBlit,
                    VkFilter,
                ),
        )
    }),
    (c"vkCmdBlitImage2KHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdBlitImage2 as unsafe extern "C" fn(VkCommandBuffer, *const VkBlitImageInfo2),
        )
    }),
    (c"vkCmdBlitImage2", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdBlitImage2 as unsafe extern "C" fn(VkCommandBuffer, *const VkBlitImageInfo2),
        )
    }),
    (c"vkCmdCopyBufferToImage", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdCopyBufferToImage
                as unsafe extern "C" fn(
                    VkCommandBuffer,
                    VkBuffer,
                    VkImage,
                    VkImageLayout,
                    u32,
                    *const VkBufferImageCopy,
                ),
        )
    }),
    (
        c"vkCmdCopyBufferToImage2KHR",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                gn_vkCmdCopyBufferToImage2
                    as unsafe extern "C" fn(VkCommandBuffer, *const VkCopyBufferToImageInfo2),
            )
        },
    ),
    (c"vkCmdCopyBufferToImage2", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdCopyBufferToImage2
                as unsafe extern "C" fn(VkCommandBuffer, *const VkCopyBufferToImageInfo2),
        )
    }),
    (c"vkCmdPipelineBarrier", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdPipelineBarrier
                as unsafe extern "C" fn(
                    VkCommandBuffer,
                    VkPipelineStageFlags,
                    VkPipelineStageFlags,
                    u32,
                    u32,
                    *const c_void,
                    u32,
                    *const c_void,
                    u32,
                    *const VkImageMemoryBarrier,
                ),
        )
    }),
    (c"vkCmdPipelineBarrier2KHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdPipelineBarrier2
                as unsafe extern "C" fn(VkCommandBuffer, *const VkDependencyInfo),
        )
    }),
    (c"vkCmdPipelineBarrier2", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdPipelineBarrier2
                as unsafe extern "C" fn(VkCommandBuffer, *const VkDependencyInfo),
        )
    }),
    (c"vkCmdBeginRenderPass", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdBeginRenderPass
                as unsafe extern "C" fn(VkCommandBuffer, *const VkRenderPassBeginInfo, i32),
        )
    }),
    (c"vkCmdBeginRenderPass2KHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdBeginRenderPass2
                as unsafe extern "C" fn(
                    VkCommandBuffer,
                    *const VkRenderPassBeginInfo,
                    *const VkSubpassBeginInfo,
                ),
        )
    }),
    (c"vkCmdBeginRenderPass2", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdBeginRenderPass2
                as unsafe extern "C" fn(
                    VkCommandBuffer,
                    *const VkRenderPassBeginInfo,
                    *const VkSubpassBeginInfo,
                ),
        )
    }),
    (c"vkCmdBeginRenderingKHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdBeginRendering as unsafe extern "C" fn(VkCommandBuffer, *const VkRenderingInfo),
        )
    }),
    (c"vkCmdBeginRendering", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdBeginRendering as unsafe extern "C" fn(VkCommandBuffer, *const VkRenderingInfo),
        )
    }),
    (c"vkCreateImageView", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCreateImageView
                as unsafe extern "C" fn(
                    VkDevice,
                    *const VkImageViewCreateInfo,
                    *const VkAllocationCallbacks,
                    *mut VkImageView,
                ) -> VkResult,
        )
    }),
    (c"vkCreateFramebuffer", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCreateFramebuffer
                as unsafe extern "C" fn(
                    VkDevice,
                    *const VkFramebufferCreateInfo,
                    *const VkAllocationCallbacks,
                    *mut VkFramebuffer,
                ) -> VkResult,
        )
    }),
    // Dynamic-state trace: the viewport/scissor the guest sets (core 1.0 and
    // the 1.3/EXT WithCount forms), and the render pass whose load/store ops
    // the begin-render-pass trace names.
    (c"vkCmdSetViewport", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdSetViewport
                as unsafe extern "C" fn(VkCommandBuffer, u32, u32, *const VkViewport),
        )
    }),
    (c"vkCmdSetViewportWithCount", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdSetViewportWithCount
                as unsafe extern "C" fn(VkCommandBuffer, u32, *const VkViewport),
        )
    }),
    (
        c"vkCmdSetViewportWithCountEXT",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                gn_vkCmdSetViewportWithCount
                    as unsafe extern "C" fn(VkCommandBuffer, u32, *const VkViewport),
            )
        },
    ),
    (c"vkCmdSetScissor", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdSetScissor as unsafe extern "C" fn(VkCommandBuffer, u32, u32, *const VkRect2D),
        )
    }),
    (c"vkCmdSetScissorWithCount", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCmdSetScissorWithCount
                as unsafe extern "C" fn(VkCommandBuffer, u32, *const VkRect2D),
        )
    }),
    (
        c"vkCmdSetScissorWithCountEXT",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                gn_vkCmdSetScissorWithCount
                    as unsafe extern "C" fn(VkCommandBuffer, u32, *const VkRect2D),
            )
        },
    ),
    (c"vkCreateRenderPass", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkCreateRenderPass
                as unsafe extern "C" fn(
                    VkDevice,
                    *const VkRenderPassCreateInfo,
                    *const VkAllocationCallbacks,
                    *mut u64,
                ) -> VkResult,
        )
    }),
    // A wrapped-handle device command: intercepted so the guest never reaches
    // the driver's raw entry point with a shim device wrapper (gamescope
    // resolves this at Vulkan device init, before any window exists).
    (
        c"vkCreateSamplerYcbcrConversion",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                gn_vkCreateSamplerYcbcrConversion
                    as unsafe extern "C" fn(
                        VkDevice,
                        *const c_void,
                        *const VkAllocationCallbacks,
                        *mut VkSamplerYcbcrConversion,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkCreateSamplerYcbcrConversionKHR",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                gn_vkCreateSamplerYcbcrConversion
                    as unsafe extern "C" fn(
                        VkDevice,
                        *const c_void,
                        *const VkAllocationCallbacks,
                        *mut VkSamplerYcbcrConversion,
                    ) -> VkResult,
            )
        },
    ),
    // The WSI dispatch-coverage set (see the block above `gn_vkSetHdrMetadataEXT`):
    // each takes a shim surface/swapchain in a non-argument-0 position, so the
    // unlisted wrapper-swapping thunk would hand the driver a handle it never
    // issued. `Always` for the device-level swapchain commands, `ProxyOnly` for
    // the physical-device surface queries, matching the other WSI wrappers.
    (c"vkSetHdrMetadataEXT", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkSetHdrMetadataEXT
                as unsafe extern "C" fn(
                    VkDevice,
                    u32,
                    *const VkSwapchainKHR,
                    *const c_void,
                ) -> VkResult,
        )
    }),
    (c"vkSetLocalDimmingAMD", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkSetLocalDimmingAMD
                as unsafe extern "C" fn(VkDevice, VkSwapchainKHR, VkBool32) -> VkResult,
        )
    }),
    (c"vkGetSwapchainStatusKHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkGetSwapchainStatusKHR
                as unsafe extern "C" fn(VkDevice, VkSwapchainKHR) -> VkResult,
        )
    }),
    (c"vkGetSwapchainCounterEXT", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkGetSwapchainCounterEXT
                as unsafe extern "C" fn(VkDevice, VkSwapchainKHR, u32, *mut u64) -> VkResult,
        )
    }),
    (
        c"vkGetDeviceGroupSurfacePresentModesKHR",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                gn_vkGetDeviceGroupSurfacePresentModesKHR
                    as unsafe extern "C" fn(VkDevice, VkSurfaceKHR, *mut u32) -> VkResult,
            )
        },
    ),
    (
        c"vkAcquireFullScreenExclusiveModeEXT",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                gn_vkAcquireFullScreenExclusiveModeEXT
                    as unsafe extern "C" fn(VkDevice, VkSwapchainKHR) -> VkResult,
            )
        },
    ),
    (
        c"vkReleaseFullScreenExclusiveModeEXT",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                gn_vkReleaseFullScreenExclusiveModeEXT
                    as unsafe extern "C" fn(VkDevice, VkSwapchainKHR) -> VkResult,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceImageFormatProperties2",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                crate::export::gn_vkGetPhysicalDeviceImageFormatProperties2
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        *const VkPhysicalDeviceImageFormatInfo2,
                        *mut VkImageFormatProperties2,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceImageFormatProperties2KHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                crate::export::gn_vkGetPhysicalDeviceImageFormatProperties2
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        *const VkPhysicalDeviceImageFormatInfo2,
                        *mut VkImageFormatProperties2,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkAllocateMemory",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::export::gn_vkAllocateMemory
                    as unsafe extern "C" fn(
                        VkDevice,
                        *const VkMemoryAllocateInfo,
                        *const VkAllocationCallbacks,
                        *mut VkDeviceMemory,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkDestroyImage",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::export::gn_vkDestroyImage
                    as unsafe extern "C" fn(VkDevice, VkImage, *const VkAllocationCallbacks),
            )
        },
    ),
    (
        c"vkGetMemoryFdKHR",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::export::gn_vkGetMemoryFdKHR
                    as unsafe extern "C" fn(VkDevice, *const VkMemoryGetFdInfoKHR, *mut i32) -> VkResult,
            )
        },
    ),
    (
        c"vkGetImageDrmFormatModifierPropertiesEXT",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::export::gn_vkGetImageDrmFormatModifierPropertiesEXT
                    as unsafe extern "C" fn(VkDevice, VkImage, *mut c_void) -> VkResult,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceFeatures2",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                crate::placed::gn_vkGetPhysicalDeviceFeatures2
                    as unsafe extern "C" fn(VkPhysicalDevice, *mut c_void),
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceFeatures2KHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                crate::placed::gn_vkGetPhysicalDeviceFeatures2
                    as unsafe extern "C" fn(VkPhysicalDevice, *mut c_void),
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceProperties2",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                crate::placed::gn_vkGetPhysicalDeviceProperties2
                    as unsafe extern "C" fn(VkPhysicalDevice, *mut c_void),
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceProperties2KHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                crate::placed::gn_vkGetPhysicalDeviceProperties2
                    as unsafe extern "C" fn(VkPhysicalDevice, *mut c_void),
            )
        },
    ),
    (
        c"vkMapMemory2KHR",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::placed::gn_vkMapMemory2KHR
                    as unsafe extern "C" fn(VkDevice, *const c_void, *mut *mut c_void) -> VkResult,
            )
        },
    ),
    (
        c"vkMapMemory2",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::placed::gn_vkMapMemory2KHR
                    as unsafe extern "C" fn(VkDevice, *const c_void, *mut *mut c_void) -> VkResult,
            )
        },
    ),
    (
        c"vkUnmapMemory2KHR",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::placed::gn_vkUnmapMemory2KHR
                    as unsafe extern "C" fn(VkDevice, *const c_void) -> VkResult,
            )
        },
    ),
    (
        c"vkUnmapMemory2",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::placed::gn_vkUnmapMemory2KHR
                    as unsafe extern "C" fn(VkDevice, *const c_void) -> VkResult,
            )
        },
    ),
    (
        c"vkUnmapMemory",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::placed::gn_vkUnmapMemory as unsafe extern "C" fn(VkDevice, VkDeviceMemory),
            )
        },
    ),
    (
        c"vkFreeMemory",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::placed::gn_vkFreeMemory
                    as unsafe extern "C" fn(VkDevice, VkDeviceMemory, *const VkAllocationCallbacks),
            )
        },
    ),
    (
        c"vkUpdateDescriptorSets",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::export::gn_vkUpdateDescriptorSets
                    as unsafe extern "C" fn(VkDevice, u32, *const c_void, u32, *const c_void),
            )
        },
    ),
    (
        c"vkGetImageSubresourceLayout",
        HandlerScope::Always,
        unsafe {
            std::mem::transmute(
                crate::export::gn_vkGetImageSubresourceLayout
                    as unsafe extern "C" fn(
                        VkDevice,
                        VkImage,
                        *const VkImageSubresource,
                        *mut VkSubresourceLayout,
                    ),
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceSurfaceFormats2KHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceSurfaceFormats2KHR
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        *const VkPhysicalDeviceSurfaceInfo2KHR,
                        *mut u32,
                        *mut VkSurfaceFormat2KHR,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceSurfacePresentModes2EXT",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceSurfacePresentModes2EXT
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        *const VkPhysicalDeviceSurfaceInfo2KHR,
                        *mut u32,
                        *mut VkPresentModeKHR,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkGetPhysicalDeviceSurfaceCapabilities2EXT",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceSurfaceCapabilities2EXT
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        VkSurfaceKHR,
                        *mut VkSurfaceCapabilities2EXT,
                    ) -> VkResult,
            )
        },
    ),
    (
        c"vkGetPhysicalDevicePresentRectanglesKHR",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDevicePresentRectanglesKHR
                    as unsafe extern "C" fn(
                        VkPhysicalDevice,
                        VkSurfaceKHR,
                        *mut u32,
                        *mut VkRect2D,
                    ) -> VkResult,
            )
        },
    ),
];

const EXPORT_EMU_NAMES: [&CStr; 7] = [
    c"vkDestroyImage",
    c"vkUpdateDescriptorSets",
    c"vkGetPhysicalDeviceImageFormatProperties2",
    c"vkGetPhysicalDeviceImageFormatProperties2KHR",
    c"vkGetMemoryFdKHR",
    c"vkGetImageDrmFormatModifierPropertiesEXT",
    c"vkGetImageSubresourceLayout",
];

const PLACED_NAMES: [&CStr; 11] = [
    c"vkAllocateMemory",
    c"vkGetPhysicalDeviceFeatures2",
    c"vkGetPhysicalDeviceFeatures2KHR",
    c"vkGetPhysicalDeviceProperties2",
    c"vkGetPhysicalDeviceProperties2KHR",
    c"vkMapMemory2KHR",
    c"vkMapMemory2",
    c"vkUnmapMemory2KHR",
    c"vkUnmapMemory2",
    c"vkUnmapMemory",
    c"vkFreeMemory",
];

fn export_emu_gated(name: &CStr) -> bool {
    (!crate::export::enabled() && EXPORT_EMU_NAMES.contains(&name))
        || (!crate::placed::enabled()
            && PLACED_NAMES.contains(&name)
            && !(crate::export::enabled()
                && (name == c"vkAllocateMemory" || name == c"vkFreeMemory")))
}

/// Handler-registry lookup for GIPA and GPDPA: the proxy-only handlers
/// (instances, devices, surfaces), which only work while the WSI proxy is up.
pub fn instance_func(name: &CStr) -> PFN_vkVoidFunction {
    if export_emu_gated(name) {
        return None;
    }
    SHIM_HANDLERS
        .iter()
        .find(|(n, scope, _)| *scope == HandlerScope::ProxyOnly && *n == name)
        .and_then(|(_, _, f)| *f)
}

/// Handler-registry lookup for vkGetDeviceProcAddr and the ICD entry points:
/// the always-available device handlers.
pub fn device_func(name: &CStr) -> PFN_vkVoidFunction {
    if export_emu_gated(name) {
        return None;
    }
    SHIM_HANDLERS
        .iter()
        .find(|(n, scope, _)| *scope == HandlerScope::Always && *n == name)
        .and_then(|(_, _, f)| *f)
}

/// Handler-registry lookup for the loader-facing ICD entry points. The shim
/// only hands out its own handlers while the WSI proxy is active: every handler
/// translates shim wrapper handles (devices, queues, command buffers,
/// surfaces, swapchains) into the driver's real ones, and the proxy is the only
/// thing that creates them. With the proxy inactive no wrapper exists, so no
/// shim handler may be served — the caller must fall through to the driver's
/// own entry point, which `serve_unlisted` returns unchanged. Without this gate
/// a proxy-owned WSI command (e.g. `vkCreateSwapchainKHR`) is handed out in the
/// fallback path and fails there with `VK_ERROR_INITIALIZATION_FAILED`, because
/// the surface it needs is one the proxy would have created. `proxy` is passed
/// in rather than read from `DRIVER` so the rule is host-testable.
pub fn entrypoint_handler(name: &CStr, proxy: bool) -> PFN_vkVoidFunction {
    if !proxy {
        return None;
    }
    instance_func(name).or_else(|| device_func(name))
}

/// Which entry point is resolving an unlisted name. It decides how a name the
/// registry cannot classify is treated, because the entry point already fixes
/// the argument-0 domain: `vkGetDeviceProcAddr` is only ever asked for
/// device-level names, `vk_icdGetPhysicalDeviceProcAddr` never is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DispatchCtx {
    Instance,
    PhysicalDevice,
    Device,
}

/// Whether an unlisted name's argument 0 must be swapped from a shim wrapper to
/// the driver's real handle. Purely static: derived from the registry level of
/// the command, with no runtime inspection of the handle. A device-level name
/// (argument 0 is VkDevice/VkQueue/VkCommandBuffer) always needs the swap; every
/// other level's argument 0 is the driver's own handle and must be passed
/// through untouched. `Device` context treats an unclassified name (a vendor
/// command absent from the registry) as device-level, because that is the only
/// thing vkGetDeviceProcAddr resolves.
fn needs_swap(name: &CStr, ctx: DispatchCtx) -> bool {
    let device_level = name
        .to_str()
        .ok()
        .and_then(crate::vk_dispatch_gen::classify)
        .is_some_and(|d| d.1 == crate::vk_dispatch_gen::Level::Device);
    match ctx {
        DispatchCtx::Device => true,
        DispatchCtx::Instance => device_level,
        DispatchCtx::PhysicalDevice => false,
    }
}

/// The single unlisted-name policy shared by every entry point. `raw` is the
/// driver's own resolver result for `name`. When the shim owns wrapper handles
/// (the proxy is active) and the name's argument 0 is one of them, the guest is
/// served a wrapper-swapping thunk instead of the raw pointer; otherwise the raw
/// pointer is correct and is returned unchanged. This is what keeps
/// `vkCreateSamplerYcbcrConversion` and every other device-level name off the
/// driver's raw entry point by construction, rather than by a hand table.
pub fn serve_unlisted(
    name: &CStr,
    raw: PFN_vkVoidFunction,
    ctx: DispatchCtx,
) -> PFN_vkVoidFunction {
    // The WSI commands the proxy cannot honour are never handed out, whichever
    // entry point is asked: `vkGetDeviceProcAddr` filters before it gets here,
    // and the loader resolves device functions through the *instance* path, so
    // without this a client still receives `vkWaitForPresentKHR` from the
    // driver with a shim swapchain handle the driver has never seen. vkd3d-proton
    // takes that pointer, budgets its frame latency on it, and its present task
    // then iterates without ever presenting.
    if unimplemented_wsi_ext(name) {
        return None;
    }
    let raw = raw?;
    if !crate::proxy_active() {
        // No wrappers exist, so every handle is the driver's own.
        return Some(raw);
    }
    if needs_swap(name, ctx) {
        let n = name.to_str().ok()?;
        thunk::thunk_for(n, Some(raw))
    } else {
        Some(raw)
    }
}

pub static GDPA_MISSED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Entry points the *driver* has no implementation for, asked through the
/// instance and physical-device paths. Named once each, like `GDPA_MISSED`:
/// the loader resolves device functions through these paths too, so a name the
/// driver lacks here is a name a client receives as a no-op.
pub static GIPA_MISSED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Record an unresolvable entry point asked through the instance or
/// physical-device path. Names the shim masks deliberately are not misses —
/// they are refusals, and `unimplemented_wsi_ext` already says why.
pub fn note_instance_miss(name: &CStr, driver_has_none: bool) {
    if !driver_has_none || unimplemented_wsi_ext(name) {
        return;
    }
    if let Ok(mut missed) = GIPA_MISSED.lock() {
        let n = name.to_string_lossy().into_owned();
        if missed.len() < 64 && !missed.contains(&n) {
            crate::logf!(gndrm_obf::obf!(
                "proxy: instance-path MISS (driver has no entry point): {n}"
            ));
            missed.push(n);
        }
    }
}
static REAL_PROC_SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" fn gn_vkGetDeviceProcAddr(
    device: VkDevice,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    let name = unsafe { CStr::from_ptr(name) };
    if unimplemented_wsi_ext(name) {
        // The proxy owns the swapchain: never hand the driver one of these with
        // a shim swapchain handle it has never seen.
        return None;
    }
    if let Some(f) = device_func(name) {
        return Some(f);
    }
    // Never the hooks' vkGetDeviceProcAddr (see gn_real_device_proc). The
    // loader will call whatever we return with the shim's wrapper handle, so
    // the rest goes through the shared unlisted-name policy: device context,
    // which always swaps.
    let target = unsafe { gn_real_device_proc(find_device(device), name) };
    if target.is_none() {
        // Returning NULL hands the guest the loader's no-op stub: the call
        // silently does nothing. Name every such entry point once.
        if let Ok(mut missed) = GDPA_MISSED.lock() {
            let n = name.to_string_lossy().into_owned();
            if missed.len() < 64 && !missed.contains(&n) {
                crate::logf!(gndrm_obf::obf!(
                    "proxy: vkGetDeviceProcAddr MISS (no-op stub): {n}"
                ));
                missed.push(n);
            }
        }
    }
    serve_unlisted(name, target, DispatchCtx::Device)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device-level commands whose handles the shim wraps must stay in the
    /// interception table: resolving one of these through `device_func` is what
    /// keeps the guest from being handed the driver's raw entry point (see the
    /// `gn_vkCreateSamplerYcbcrConversion` note). This is the class of command
    /// the gamescope-at-device-init crash regressed on.
    #[test]
    fn required_device_commands_are_intercepted() {
        let required: [&CStr; 13] = [
            c"vkCreateSamplerYcbcrConversion",
            c"vkCreateSamplerYcbcrConversionKHR",
            c"vkCmdSetViewport",
            c"vkCmdSetViewportWithCount",
            c"vkCmdSetViewportWithCountEXT",
            c"vkCmdSetScissor",
            c"vkCmdSetScissorWithCount",
            c"vkCmdSetScissorWithCountEXT",
            c"vkCreateRenderPass",
            c"vkQueueSubmit",
            c"vkQueueSubmit2",
            c"vkQueuePresentKHR",
            c"vkCreateSwapchainKHR",
        ];
        for name in required {
            assert!(
                device_func(name).is_some(),
                "device_func({}) is empty: the shim no longer intercepts it",
                name.to_str().unwrap()
            );
        }
    }

    /// The handler registry is a static slice, so it has no capacity to
    /// overflow or displace an entry: adding a name can only add. Assert the
    /// two commands the regression lost and gained are both present.
    #[test]
    fn command_registry_is_unbounded() {
        assert!(device_func(c"vkCreateSamplerYcbcrConversion").is_some());
        assert!(instance_func(c"vkGetDeviceProcAddr").is_some());
        assert!(SHIM_HANDLERS.len() >= 61);
    }

    /// The WSI commands whose wrapper handle (surface/swapchain) sits in a
    /// non-argument-0 position can never be served by the unlisted
    /// wrapper-swapping thunk: it swaps only argument 0 and the driver then
    /// dereferences a handle it never issued (the `vkSetHdrMetadataEXT`
    /// 0xc0000005 class). Each must stay in the handler registry, in the scope
    /// its level dictates.
    #[test]
    fn wsi_handle_commands_are_intercepted() {
        let device_level: [&CStr; 7] = [
            c"vkSetHdrMetadataEXT",
            c"vkSetLocalDimmingAMD",
            c"vkGetSwapchainStatusKHR",
            c"vkGetSwapchainCounterEXT",
            c"vkGetDeviceGroupSurfacePresentModesKHR",
            c"vkAcquireFullScreenExclusiveModeEXT",
            c"vkReleaseFullScreenExclusiveModeEXT",
        ];
        for name in device_level {
            assert!(
                device_func(name).is_some(),
                "device_func({}) is empty: the shim no longer intercepts it",
                name.to_str().unwrap()
            );
        }
        let physical_device_level: [&CStr; 4] = [
            c"vkGetPhysicalDeviceSurfaceFormats2KHR",
            c"vkGetPhysicalDeviceSurfacePresentModes2EXT",
            c"vkGetPhysicalDeviceSurfaceCapabilities2EXT",
            c"vkGetPhysicalDevicePresentRectanglesKHR",
        ];
        for name in physical_device_level {
            assert!(
                instance_func(name).is_some(),
                "instance_func({}) is empty: the shim no longer intercepts it",
                name.to_str().unwrap()
            );
        }
    }

    /// A shim handler may only be handed out while the WSI proxy is active:
    /// that is the only time the shim owns wrapper handles, and every handler
    /// translates those wrappers into the driver's real ones. The proxy-owned
    /// WSI commands are the case that regressed the fallback path — served
    /// without the proxy, `gn_vkCreateSwapchainKHR` found no shim surface and
    /// returned `VK_ERROR_INITIALIZATION_FAILED` (see `gn_vkCreateSwapchainKHR`
    /// and the `find_surface` guard). With the proxy inactive the entry point
    /// must serve nothing and let the driver's own entry point through, which
    /// `serve_unlisted` returns unchanged.
    #[test]
    fn every_handler_is_gated_on_the_proxy() {
        let proxy_owned_wsi: [&CStr; 6] = [
            c"vkCreateSwapchainKHR",
            c"vkGetSwapchainImagesKHR",
            c"vkAcquireNextImageKHR",
            c"vkAcquireNextImage2KHR",
            c"vkQueuePresentKHR",
            c"vkDestroySwapchainKHR",
        ];
        for name in proxy_owned_wsi {
            assert!(
                entrypoint_handler(name, false).is_none(),
                "{} is handed out without the proxy: the fallback path would reach the shim's WSI",
                name.to_str().unwrap()
            );
            assert!(
                entrypoint_handler(name, true).is_some(),
                "{} is not served while the proxy is active",
                name.to_str().unwrap()
            );
        }
        // Instance- and device-level handlers follow the same gate.
        for name in [c"vkCreateDevice", c"vkQueueSubmit"] {
            assert!(entrypoint_handler(name, false).is_none());
            assert!(entrypoint_handler(name, true).is_some());
        }
    }

    /// The registry (generated from vk.xml) must classify each command's level
    /// and argument-0 kind. This is the fact the unlisted-name policy is built
    /// on, so it is pinned for a representative set: a device-level command
    /// (argument 0 is a shim-wrapped handle), physical-device- and
    /// instance-level commands (argument 0 is the driver's own handle), the
    /// `vkCreateSamplerYcbcrConversion` pair the crash class is named after, and
    /// the alias resolution both spellings share.
    #[test]
    fn registry_classifies_representative_commands() {
        use crate::vk_dispatch_gen::{classify, Arg0, Level};
        let cases: &[(&str, Level, Arg0)] = &[
            // Device: argument 0 is a shim wrapper (VkDevice/VkQueue/VkCommandBuffer).
            (
                "vkCreateSamplerYcbcrConversion",
                Level::Device,
                Arg0::Device,
            ),
            (
                "vkCreateSamplerYcbcrConversionKHR",
                Level::Device,
                Arg0::Device,
            ),
            ("vkCreateSwapchainKHR", Level::Device, Arg0::Device),
            ("vkQueueSubmit", Level::Device, Arg0::Queue),
            ("vkCmdDraw", Level::Device, Arg0::CommandBuffer),
            // Physical device: argument 0 is the driver's own handle.
            (
                "vkGetPhysicalDeviceProperties",
                Level::PhysicalDevice,
                Arg0::PhysicalDevice,
            ),
            (
                "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
                Level::PhysicalDevice,
                Arg0::PhysicalDevice,
            ),
            (
                "vkCreateDevice",
                Level::PhysicalDevice,
                Arg0::PhysicalDevice,
            ),
            // Instance: argument 0 is the driver's own instance handle.
            // vkEnumeratePhysicalDevices enumerates physical devices but takes
            // VkInstance, so it is instance-level and passes through untouched.
            ("vkDestroyInstance", Level::Instance, Arg0::Instance),
            ("vkGetInstanceProcAddr", Level::Instance, Arg0::Instance),
            (
                "vkEnumeratePhysicalDevices",
                Level::Instance,
                Arg0::Instance,
            ),
            // Global: no dispatchable argument 0.
            ("vkCreateInstance", Level::Global, Arg0::Other),
            ("vkEnumerateInstanceVersion", Level::Global, Arg0::Other),
        ];
        for &(name, level, arg0) in cases {
            let d = classify(name)
                .unwrap_or_else(|| panic!("{}", gndrm_obf::obf!("{name} missing from registry")));
            assert_eq!(d.1, level, "{name} level");
            assert_eq!(d.2, arg0, "{name} arg0");
        }
    }

    /// The thunk swaps the wrapper for the real handle at a fixed offset
    /// (`GN_HANDLE_SCRATCH * 8`); a struct-layout drift here would make every
    /// device-level call reach the driver with a garbage device.
    #[test]
    fn device_wrapper_real_offset_matches_thunk_swap() {
        use std::mem::offset_of;
        assert_eq!(offset_of!(GnDevice, scratch), 0);
        assert_eq!(offset_of!(GnDevice, real), GN_HANDLE_SCRATCH * 8);
        assert_eq!(offset_of!(GnQueue, scratch), 0);
        assert_eq!(offset_of!(GnQueue, real), GN_HANDLE_SCRATCH * 8);
        assert_eq!(offset_of!(GnCmd, scratch), 0);
        assert_eq!(offset_of!(GnCmd, real), GN_HANDLE_SCRATCH * 8);
    }

    /// The generated nested-handle table is what `translate_nested` walks, so
    /// it must record the command-buffer arrays the submit and execute
    /// commands consume, with the struct offsets read from the same headers
    /// (`offset_of!`) so a struct-layout drift fails here.
    #[test]
    fn nested_handles_cover_command_buffer_arrays() {
        use crate::vk_dispatch_gen::{nested_handles, HandleKind};
        use std::mem::offset_of;

        let e = nested_handles("vkCmdExecuteCommands");
        assert_eq!(e.len(), 1, "vkCmdExecuteCommands plan");
        assert_eq!(e[0].param, 2);
        assert_eq!(e[0].count_param, 1);
        assert_eq!(e[0].kind, HandleKind::CommandBuffer);
        assert!(e[0].hops.is_empty(), "the parameter is the handle array");

        let s = nested_handles("vkQueueSubmit");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].param, 2);
        assert_eq!(s[0].count_param, 1);
        assert_eq!(
            s[0].elem_stride,
            std::mem::size_of::<crate::vk::VkSubmitInfo>()
        );
        assert_eq!(s[0].hops.len(), 1);
        assert_eq!(
            s[0].hops[0].off,
            offset_of!(crate::vk::VkSubmitInfo, pCommandBuffers)
        );
        assert_eq!(
            s[0].hops[0].count_off,
            offset_of!(crate::vk::VkSubmitInfo, commandBufferCount)
        );

        // VkSubmitInfo2 -> pCommandBufferInfos -> VkCommandBufferSubmitInfo::commandBuffer.
        let s2 = nested_handles("vkQueueSubmit2");
        assert_eq!(s2[0].hops.len(), 2);
        assert_eq!(
            s2[0].hops[0].off,
            offset_of!(crate::vk::VkSubmitInfo2, pCommandBufferInfos)
        );
        assert_eq!(
            s2[0].hops[1].off,
            offset_of!(crate::vk::VkCommandBufferSubmitInfo, commandBuffer)
        );
        assert_eq!(s2[0].hops[1].count_off, usize::MAX);
        // The KHR spelling inherits the target's plan.
        assert_eq!(nested_handles("vkQueueSubmit2KHR"), s2);

        // A command carrying no wrapped handle has no plan.
        assert!(nested_handles("vkCmdDraw").is_empty());
    }

    /// Every command the registry records a nested wrapped-handle input for
    /// must be served by a shim handler: a name served by the arg0-only thunk
    /// would forward the nested wrapper untranslated — the round-W storm.
    #[test]
    fn every_nested_handle_command_is_intercepted() {
        for (name, _) in crate::vk_dispatch_gen::VK_NESTED_HANDLES {
            let c = CString::new(*name).unwrap();
            assert!(
                device_func(&c).is_some(),
                "{name} carries a nested wrapped handle but is not intercepted"
            );
        }
    }

    /// The policy is decided from the name + entry point alone — never from the
    /// handle — so a device-level name is always swapped (in device context) and
    /// an instance- or physical-device-level name never is. This is the
    /// invariant that closes the `vkCreateSamplerYcbcrConversion` class by
    /// construction: no entry point can hand out the driver's raw pointer for a
    /// command whose argument 0 is a shim wrapper.
    #[test]
    fn unlisted_policy_follows_registry_level_only() {
        // Device context (vkGetDeviceProcAddr): everything is swapped, including
        // a vendor command absent from the registry.
        assert!(needs_swap(
            c"vkCreateSamplerYcbcrConversion",
            DispatchCtx::Device
        ));
        assert!(needs_swap(c"vkSomeVendorCommandEXT", DispatchCtx::Device));
        // Instance context (GIPA asked for a device-level name): swapped.
        assert!(needs_swap(
            c"vkCreateSamplerYcbcrConversion",
            DispatchCtx::Instance
        ));
        assert!(needs_swap(c"vkQueueSubmit2", DispatchCtx::Instance));
        // Instance context, non-device levels: passed through untouched.
        assert!(!needs_swap(c"vkCreateInstance", DispatchCtx::Instance));
        assert!(!needs_swap(
            c"vkEnumeratePhysicalDevices",
            DispatchCtx::Instance
        ));
        assert!(!needs_swap(
            c"vkGetPhysicalDeviceProperties",
            DispatchCtx::Instance
        ));
        // Unknown vendor name at instance level: not a device handle -> raw.
        assert!(!needs_swap(
            c"vkSomeVendorCommandEXT",
            DispatchCtx::Instance
        ));
        // Physical-device context never swaps.
        assert!(!needs_swap(
            c"vkGetPhysicalDeviceProperties",
            DispatchCtx::PhysicalDevice
        ));
        assert!(!needs_swap(
            c"vkCreateSamplerYcbcrConversion",
            DispatchCtx::PhysicalDevice
        ));
    }

    /// The generated alias table replaces the hand-written core<->KHR remap:
    /// every pair the old list carried must still resolve.
    #[test]
    fn generated_aliases_cover_the_former_hand_list() {
        use crate::vk_dispatch_gen::{alias_of, VK_ALIASES};
        for (core, khr) in [
            ("vkMapMemory2", "vkMapMemory2KHR"),
            ("vkUnmapMemory2", "vkUnmapMemory2KHR"),
            (
                "vkGetImageSubresourceLayout2",
                "vkGetImageSubresourceLayout2KHR",
            ),
            (
                "vkGetDeviceImageSubresourceLayout",
                "vkGetDeviceImageSubresourceLayoutKHR",
            ),
            ("vkCopyMemoryToImage", "vkCopyMemoryToImageEXT"),
            ("vkCopyImageToMemory", "vkCopyImageToMemoryEXT"),
            ("vkCopyImageToImage", "vkCopyImageToImageEXT"),
            ("vkCmdBindIndexBuffer2", "vkCmdBindIndexBuffer2KHR"),
            ("vkCmdBindDescriptorSets2", "vkCmdBindDescriptorSets2KHR"),
            ("vkCmdPushConstants2", "vkCmdPushConstants2KHR"),
            ("vkCmdPushDescriptorSet", "vkCmdPushDescriptorSetKHR"),
            ("vkCmdPushDescriptorSet2", "vkCmdPushDescriptorSet2KHR"),
            (
                "vkCmdPushDescriptorSetWithTemplate",
                "vkCmdPushDescriptorSetWithTemplateKHR",
            ),
            (
                "vkCmdPushDescriptorSetWithTemplate2",
                "vkCmdPushDescriptorSetWithTemplate2KHR",
            ),
            (
                "vkCmdSetRenderingAttachmentLocations",
                "vkCmdSetRenderingAttachmentLocationsKHR",
            ),
            (
                "vkCmdSetRenderingInputAttachmentIndices",
                "vkCmdSetRenderingInputAttachmentIndicesKHR",
            ),
            (
                "vkGetRenderingAreaGranularity",
                "vkGetRenderingAreaGranularityKHR",
            ),
            ("vkCmdSetLineStipple", "vkCmdSetLineStippleEXT"),
        ] {
            assert!(
                VK_ALIASES.contains(&(core, khr)) || VK_ALIASES.contains(&(khr, core)),
                "alias pair {core}/{khr} missing from the registry table"
            );
            // Both spellings resolve, and the core name's partner resolves back
            // to it. A core name may have more than one extension spelling (the
            // registry lists both EXT and KHR for vkGetImageSubresourceLayout2),
            // so the exact partner is not pinned — the round trip is.
            assert_eq!(alias_of(khr), Some(core), "alias_of({khr})");
            let partner = alias_of(core)
                .unwrap_or_else(|| panic!("{}", gndrm_obf::obf!("alias_of({core}) is empty")));
            assert_eq!(alias_of(partner), Some(core), "round trip from {core}");
        }
    }

    /// The two layouts the proxy knows must map to the DRM fourcc of their
    /// memory order: BGRA memory is ARGB8888, RGBA memory is ABGR8888. The
    /// alpha-bearing A-formats are deliberate — both Vulkan formats have a real
    /// alpha channel, and an X-format would make the compositor ignore it and
    /// composite the frame opaque.
    #[test]
    fn format_to_fourcc_names_the_memory_order() {
        assert_eq!(
            vk_format_to_fourcc(VK_FORMAT_B8G8R8A8_UNORM),
            GN_DRM_FORMAT_ARGB8888
        );
        assert_eq!(
            vk_format_to_fourcc(VK_FORMAT_R8G8B8A8_UNORM),
            GN_DRM_FORMAT_ABGR8888
        );
        assert_eq!(
            vk_format_to_fourcc(37 + 1000),
            0,
            "unknown formats are refused"
        );
    }

    /// Regression for the Nidhogg B/R swap: only the scan-out pool's layout is
    /// accepted by default, so a guest cannot render RGBA into a buffer the
    /// daemon publishes as BGRA. RGBA is reachable only under the explicit A/B
    /// opt-in.
    #[test]
    fn only_the_scanout_layout_is_accepted_by_default() {
        assert!(swapchain_format_allowed(VK_FORMAT_B8G8R8A8_UNORM, false));
        assert!(swapchain_format_allowed(VK_FORMAT_B8G8R8A8_UNORM, true));
        assert!(!swapchain_format_allowed(VK_FORMAT_R8G8B8A8_UNORM, false));
        assert!(swapchain_format_allowed(VK_FORMAT_R8G8B8A8_UNORM, true));
        assert_eq!(SCANOUT_SWAPCHAIN_FORMAT, VK_FORMAT_B8G8R8A8_UNORM);
    }

    /// The override parser mirrors mesa's `MESA_VK_WSI_PRESENT_MODE`: exactly
    /// four names, matched case-sensitively; anything else is ignored.
    #[test]
    fn present_mode_override_matches_mesa() {
        assert_eq!(
            parse_present_mode_override(Some("fifo")),
            Some(VK_PRESENT_MODE_FIFO_KHR)
        );
        assert_eq!(
            parse_present_mode_override(Some("relaxed")),
            Some(VK_PRESENT_MODE_FIFO_RELAXED_KHR)
        );
        assert_eq!(
            parse_present_mode_override(Some("mailbox")),
            Some(VK_PRESENT_MODE_MAILBOX_KHR)
        );
        assert_eq!(
            parse_present_mode_override(Some("immediate")),
            Some(VK_PRESENT_MODE_IMMEDIATE_KHR)
        );
        // mesa's strcmp is case-sensitive, and its UI also carries label
        // strings ("Never"/"Normal"/"Always") that are not valid names.
        for bad in ["FIFO", "Immediate", "Never", "Normal", "Always", ""] {
            assert_eq!(parse_present_mode_override(Some(bad)), None, "{bad}");
        }
        assert_eq!(parse_present_mode_override(None), None);
    }

    /// Every mode the override parser accepts is also served, so selecting any
    /// MESA_VK_WSI_PRESENT_MODE name changes what the guest is offered.
    #[test]
    fn every_parsed_present_mode_is_served() {
        assert_eq!(SERVED_PRESENT_MODES.len(), 4);
        for name in ["fifo", "relaxed", "mailbox", "immediate"] {
            let m = parse_present_mode_override(Some(name)).expect(name);
            assert!(
                SERVED_PRESENT_MODES.contains(&m),
                "{name} is parsed but not served"
            );
            assert_eq!(
                offered_present_modes(&SERVED_PRESENT_MODES, Some(m))[0],
                m,
                "{name} does not lead the offered list"
            );
        }
    }

    /// The offered list leads with a served override, keeps the served order
    /// otherwise, drops an override the ring cannot serve, and always offers
    /// FIFO.
    #[test]
    fn offered_list_prefers_override_and_always_offers_fifo() {
        assert_eq!(
            offered_present_modes(&SERVED_PRESENT_MODES, None),
            [
                VK_PRESENT_MODE_IMMEDIATE_KHR,
                VK_PRESENT_MODE_MAILBOX_KHR,
                VK_PRESENT_MODE_FIFO_KHR,
                VK_PRESENT_MODE_FIFO_RELAXED_KHR
            ]
        );
        assert_eq!(
            offered_present_modes(&SERVED_PRESENT_MODES, Some(VK_PRESENT_MODE_IMMEDIATE_KHR)),
            [
                VK_PRESENT_MODE_IMMEDIATE_KHR,
                VK_PRESENT_MODE_MAILBOX_KHR,
                VK_PRESENT_MODE_FIFO_KHR,
                VK_PRESENT_MODE_FIFO_RELAXED_KHR
            ]
        );
        // FIFO leads when it is the override, and is not duplicated.
        assert_eq!(
            offered_present_modes(&SERVED_PRESENT_MODES, Some(VK_PRESENT_MODE_FIFO_KHR)),
            [
                VK_PRESENT_MODE_FIFO_KHR,
                VK_PRESENT_MODE_IMMEDIATE_KHR,
                VK_PRESENT_MODE_MAILBOX_KHR,
                VK_PRESENT_MODE_FIFO_RELAXED_KHR
            ]
        );
        // fifo_relaxed is served, so it leads and is not duplicated.
        assert_eq!(
            offered_present_modes(
                &SERVED_PRESENT_MODES,
                Some(VK_PRESENT_MODE_FIFO_RELAXED_KHR)
            ),
            [
                VK_PRESENT_MODE_FIFO_RELAXED_KHR,
                VK_PRESENT_MODE_IMMEDIATE_KHR,
                VK_PRESENT_MODE_MAILBOX_KHR,
                VK_PRESENT_MODE_FIFO_KHR
            ]
        );
        // An override the served slice cannot serve is dropped, not advertised.
        assert_eq!(
            offered_present_modes(
                &[VK_PRESENT_MODE_MAILBOX_KHR, VK_PRESENT_MODE_FIFO_KHR],
                Some(VK_PRESENT_MODE_IMMEDIATE_KHR)
            ),
            [VK_PRESENT_MODE_MAILBOX_KHR, VK_PRESENT_MODE_FIFO_KHR]
        );
        // FIFO is appended when the served set lacks it.
        assert_eq!(
            offered_present_modes(
                &[VK_PRESENT_MODE_MAILBOX_KHR, VK_PRESENT_MODE_IMMEDIATE_KHR],
                None
            ),
            [
                VK_PRESENT_MODE_MAILBOX_KHR,
                VK_PRESENT_MODE_IMMEDIATE_KHR,
                VK_PRESENT_MODE_FIFO_KHR
            ]
        );
        assert_eq!(
            offered_present_modes(&[VK_PRESENT_MODE_FIFO_KHR], None),
            [VK_PRESENT_MODE_FIFO_KHR]
        );
    }
}

// ---- reconstructed from HEAD (items missing after the damaged edit) ----

// First-N diagnostics for the present pipeline handoffs (acquire ->
// present -> worker commit -> compositor release). Bounded counters, so
// steady-state frame pacing stays silent in logcat.
use std::ptr;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const GN_MAX_SC_IMAGES: usize = 8;
pub const GN_MAX_PRESENT: usize = 8; // swapchainCount handled per vkQueuePresentKHR
pub const GN_JOB_CAP: usize = 16;

pub const fn gn_fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}
pub const GN_DRM_FORMAT_XRGB8888: u32 = gn_fourcc(b'X', b'R', b'2', b'4');
pub const GN_DRM_FORMAT_XBGR8888: u32 = gn_fourcc(b'X', b'B', b'2', b'4');
// Alpha-carrying siblings of the two X-formats above: same red/green/blue byte
// order, but the fourth byte is real alpha the compositor must honour instead
// of ignore. A guest swapchain buffer genuinely has that channel (BGRA/RGBA),
// so describing it as an X-format would force the compositor to discard it.
pub const GN_DRM_FORMAT_ARGB8888: u32 = gn_fourcc(b'A', b'R', b'2', b'4');
pub const GN_DRM_FORMAT_ABGR8888: u32 = gn_fourcc(b'A', b'B', b'2', b'4');
pub const GN_DRM_FORMAT_ABGR2101010: u32 = gn_fourcc(b'A', b'B', b'3', b'0');
pub const GN_DRM_FORMAT_ARGB2101010: u32 = gn_fourcc(b'A', b'R', b'3', b'0');
pub const GN_DRM_MOD_LINEAR: u64 = 0;
fn max_swapchain_images() -> u32 {
    match std::env::var("WRAPPER_MAX_IMAGE_COUNT")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        Some(v) if v >= 2 => v.min(GN_MAX_SC_IMAGES as u32),
        _ => GN_MAX_SC_IMAGES as u32,
    }
}

const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ROBUSTNESS_2_FEATURES_EXT: VkStructureType = 1000286000;

// gamescope hard-requires VK_EXT_robustness2; some system drivers (Mali-G710)
// lack it. Advertised to gamescope only, then stripped with its feature struct
// before the driver sees the create.
static FAKE_ROBUSTNESS2: AtomicBool = AtomicBool::new(false);
/// Whether this process may import swapchain slots as `AHardwareBuffer`s (set
/// with the driver choice; see [`device_prereqs`]).
static AHB_IMPORT_ALLOWED: AtomicBool = AtomicBool::new(false);
static AHB_MODE_LOGGED: AtomicBool = AtomicBool::new(false);

static ROBUSTNESS2_FAKED: AtomicBool = AtomicBool::new(false);

pub fn set_fake_robustness2(on: bool) {
    FAKE_ROBUSTNESS2.store(on, Ordering::Relaxed);
}

/// The WSI surface the proxy does not implement, by extension name and by the
/// command names those extensions add.
///
/// Both forms are needed because the same predicate guards two different
/// questions: which extensions to enumerate (`vkEnumerate*ExtensionProperties`,
/// extension names) and which entry points `vkGetDeviceProcAddr` may resolve
/// (command names). Testing extension names alone left the commands resolvable,
/// so the guest got `vkWaitForPresentKHR` from the driver together with a shim
/// swapchain handle the driver has never seen — vkd3d-proton asks for it
/// whenever it resolves, which is why DX12 titles stalled in their present task
/// while DXVK titles, which never call it, presented normally.
fn unimplemented_wsi_ext(name: &CStr) -> bool {
    name == c"VK_KHR_present_wait"
        || name == c"VK_KHR_present_id"
        || name == c"VK_EXT_swapchain_maintenance1"
        || name == c"vkWaitForPresentKHR"
        || name == c"vkWaitForPresent2KHR"
        || name == c"vkReleaseSwapchainImagesEXT"
}

// ---------------------------------------------------------------- objects

#[derive(Default, Clone, Copy)]
pub struct RealInstFns {
    pub gdp: Option<PFN_vkGetDeviceProcAddr>,
    pub enum_pds: Option<PFN_vkEnumeratePhysicalDevices>,
    pub create_device: Option<PFN_vkCreateDevice>,
    pub destroy_instance: Option<PFN_vkDestroyInstance>,
}

#[derive(Default, Clone, Copy)]
pub struct RealDevFns {
    pub gdp: Option<PFN_vkGetDeviceProcAddr>,
    pub get_queue: Option<PFN_vkGetDeviceQueue>,
    pub create_image: Option<PFN_vkCreateImage>,
    pub create_pipelines: Option<PFN_vkCreateGraphicsPipelines>,
    pub get_device_queue2: Option<PFN_vkGetDeviceQueue2>,
    pub submit2: Option<PFN_vkQueueSubmit2>,
    pub wait_semaphores:
        Option<unsafe extern "C" fn(VkDevice, *const VkSemaphoreWaitInfo, u64) -> VkResult>,
    pub destroy_image: Option<PFN_vkDestroyImage>,
    pub get_mem_reqs: Option<PFN_vkGetImageMemoryRequirements>,
    pub alloc_mem: Option<PFN_vkAllocateMemory>,
    pub free_mem: Option<PFN_vkFreeMemory>,
    pub bind_mem: Option<PFN_vkBindImageMemory>,
    pub create_fence: Option<PFN_vkCreateFence>,
    pub destroy_fence: Option<PFN_vkDestroyFence>,
    pub wait_fences: Option<PFN_vkWaitForFences>,
    pub reset_fences: Option<PFN_vkResetFences>,
    pub get_fence_status: Option<PFN_vkGetFenceStatus>,
    pub create_sem: Option<PFN_vkCreateSemaphore>,
    pub destroy_sem: Option<PFN_vkDestroySemaphore>,
    pub alloc_cmd: Option<PFN_vkAllocateCommandBuffers>,
    pub free_cmd: Option<PFN_vkFreeCommandBuffers>,
    pub submit: Option<PFN_vkQueueSubmit>,
    // Secondary command buffers the shim wraps arrive nested in
    // `vkCmdExecuteCommands::pCommandBuffers`; resolved so the command can be
    // forwarded with the driver's real handles (see `translate_nested`).
    pub execute_commands:
        Option<unsafe extern "C" fn(VkCommandBuffer, u32, *const VkCommandBuffer)>,
    pub wait_idle: Option<PFN_vkDeviceWaitIdle>,
    pub queue_wait_idle: Option<PFN_vkQueueWaitIdle>,
    pub destroy_device: Option<PFN_vkDestroyDevice>,
    /// Command pool for the shim's own submissions (frame generation). Created
    /// with the device and destroyed before it.
    pub create_cmd_pool: Option<
        unsafe extern "C" fn(
            VkDevice,
            *const VkCommandPoolCreateInfo,
            *const c_void,
            *mut VkCommandPool,
        ) -> VkResult,
    >,
    pub destroy_cmd_pool: Option<unsafe extern "C" fn(VkDevice, VkCommandPool, *const c_void)>,
    // Slot read-back for the bounded one-shot `x11 view` line.
    pub map_mem: Option<PFN_vkMapMemory>,
    pub unmap_mem: Option<PFN_vkUnmapMemory>,
    // Legacy-sync layout transitions (vkCmdPipelineBarrier) carried by the
    // guest's command stream; forwarded through the trace hook.
    pub cmd_pipeline_barrier: Option<PFN_vkCmdPipelineBarrier>,
    // Guest-command trace: the recorded transfers that could write a
    // swapchain slot image, so a recorded command can name the slot it targets.
    pub cmd_clear_color_image: Option<PFN_vkCmdClearColorImage>,
    pub cmd_copy_image: Option<PFN_vkCmdCopyImage>,
    pub cmd_copy_image2: Option<PFN_vkCmdCopyImage2>,
    pub cmd_blit_image: Option<PFN_vkCmdBlitImage>,
    pub cmd_blit_image2: Option<PFN_vkCmdBlitImage2>,
    pub cmd_copy_buffer_to_image: Option<PFN_vkCmdCopyBufferToImage>,
    pub cmd_copy_buffer_to_image2: Option<PFN_vkCmdCopyBufferToImage2>,
    pub cmd_pipeline_barrier2: Option<PFN_vkCmdPipelineBarrier2>,
    pub cmd_begin_render_pass: Option<PFN_vkCmdBeginRenderPass>,
    pub cmd_begin_render_pass2: Option<PFN_vkCmdBeginRenderPass2>,
    pub cmd_begin_rendering: Option<PFN_vkCmdBeginRendering>,
    pub create_image_view: Option<PFN_vkCreateImageView>,
    pub create_framebuffer: Option<PFN_vkCreateFramebuffer>,
    // Dynamic-state trace: the viewport/scissor the guest sets (bounded
    // one-shot), plus the render pass created ahead of vkCmdBeginRenderPass so
    // the attachment loadOp/storeOp can be named.
    pub cmd_set_viewport: Option<PFN_vkCmdSetViewport>,
    pub cmd_set_viewport_with_count: Option<PFN_vkCmdSetViewportWithCount>,
    pub cmd_set_scissor: Option<PFN_vkCmdSetScissor>,
    pub cmd_set_scissor_with_count: Option<PFN_vkCmdSetScissorWithCount>,
    pub create_render_pass: Option<PFN_vkCreateRenderPass>,
    // Device-level command whose handle the shim wraps: resolved so the shim can
    // forward with the real device instead of letting the guest reach Turnip raw.
    pub create_sampler_ycbcr_conversion: Option<PFN_vkCreateSamplerYcbcrConversion>,
}

pub struct GnInstance {
    pub real: VkInstance,
    pub f: RealInstFns,
    pub pds: [VkPhysicalDevice; 8],
    pub n_pds: u32,
    pub next: *mut GnInstance,
}

pub const GN_HANDLE_SCRATCH: usize = 6;

/// `repr(C)` is REQUIRED: the wrapper-swapping thunks (`thunk::emit`) load the
/// real handle from a fixed byte offset (`GN_HANDLE_SCRATCH * 8`) derived from
/// this declaration order. With the default `repr(Rust)` layout rustc reorders
/// fields, which silently moved `scratch`/`real` and made every thunked
/// device-level call reach the driver with a garbage device.
#[repr(C)]
pub struct GnDevice {
    pub scratch: [usize; GN_HANDLE_SCRATCH],
    pub real: VkDevice,
    pub inst: *mut GnInstance,
    // The physical device this device was created from, and its memory
    // properties, captured once at create time. Read-only: used only by the
    // image-path instrument (`modq` probe, and the memory-type line after a
    // swapchain image import). Neither field participates in creation logic.
    pub pd: VkPhysicalDevice,
    pub mem_props: VkPhysicalDeviceMemoryProperties,
    pub f: RealDevFns,
    pub queues: *mut GnQueue, // wrapper list
    pub cmds: *mut GnCmd,     // wrapper list
    pub next: *mut GnDevice,
    /// Queue family the shim's own command pool belongs to.
    pub queue_family: u32,
    /// The shim's command pool for frame generation, or 0.
    pub cmd_pool: VkCommandPool,
    /// Whether the guest enabled float16, so a lazily opened session picks the
    /// same shader variant the device can run.
    pub fp16: bool,
    /// Swapchain slots are imported as `AHardwareBuffer`s rather than dma-bufs:
    /// the driver has `VK_ANDROID_external_memory_android_hardware_buffer` but
    /// not the dma-buf/modifier pair (the Adreno system driver).
    pub import_ahb: bool,
    /// The settings epoch the session was last attempted at.
    pub lsfg_epoch: u32,
    /// Generation batches in flight, each with its own fence and command buffer:
    /// a batch's command buffer must outlive its submission, and its fence is
    /// what the committed frames of that batch wait on. One shared fence would
    /// make a flush wait on the newest batch, not the one whose frames are due.
    pub gen_batches: Vec<GenBatch>,
    /// The batch slot the next generation records into.
    pub gen_next: usize,
    /// The swapchain the pending batches belong to, or null.
    pub gen_sc: *mut GnSwapchain,
    /// Frames waiting for their slot on the output grid, in commit order: the
    /// generated frames of a source frame first, then the source frame itself.
    /// Committing a batch inline would put source and generated frame inside one
    /// compositor frame, and gamescope composites that once.
    pub gen_due: VecDeque<GenDue>,
    /// The deadline of the last queued frame, so a new batch is scheduled after
    /// it rather than on top of it.
    pub gen_last_deadline: u64,
    /// When the previous source frame reached the presenter, and the smoothed
    /// interval between source frames: the output grid when there is no cap.
    pub gen_last_source_ns: u64,
    pub gen_period_ns: u64,
    // Frame generation for this device, or null. Declared last: `repr(C)` plus
    // the thunks' fixed offset for `scratch`/`real` mean fields may only be
    // appended, never inserted.
    pub lsfg: *mut crate::lsfg::Session,
}

#[repr(C)]
pub struct GnQueue {
    pub scratch: [usize; GN_HANDLE_SCRATCH],
    pub real: VkQueue,
    pub gd: *mut GnDevice,
    pub next: *mut GnQueue,
    /// Queue family this wrapper was handed out for. The shim's own command
    /// pool must belong to the family of the queue it submits on: a batch
    /// recorded in a pool of one family and submitted to a queue of another is
    /// invalid usage, and drivers are free to drop it silently. Appended last so
    /// the fixed offsets the thunks rely on do not move.
    pub family: u32,
}

#[repr(C)]
pub struct GnCmd {
    pub scratch: [usize; GN_HANDLE_SCRATCH],
    pub real: VkCommandBuffer,
    pub gd: *mut GnDevice,
    pub next: *mut GnCmd,
}

pub struct GnSurface {
    pub inst: *mut GnInstance,
    pub dpy: *mut wl_display,         // winewayland's, or our own (X11 mode)
    pub parent: *mut wl_proxy,        // commit target wl_surface
    pub own_surface: *mut wl_proxy,   // X11 mode: shim-created wl_surface
    pub swapchain_obj: *mut wl_proxy, // X11 mode: gamescope_swapchain
    pub x11: *mut crate::x11::X11Present, // X11 route (DRI3+Present) presenter
    // The guest's X window this surface presents for, queried for its live
    // size by `surface_extent`; 0 on the wayland-surface route (no X window).
    pub window: u32,
    pub next: *mut GnSurface,
}

pub struct GnSlot {
    pub owner: *mut GnSwapchain, // release listener -> slot free
    pub image: VkImage,
    pub mem: VkDeviceMemory,
    pub fd: i32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    pub buf: *mut wl_proxy, // created on first present (worker thread)
    pub xpix: u32,          // X11 route: DRI3 pixmap, made once per slot
    pub xfence: u32,        // X11 route: Present idle fence, made once per slot
    pub fence: VkFence,
    pub sem: VkSemaphore, // chains multi-swapchain presents
    pub acquired: bool,   // owned by the app (acquire -> present)
    pub in_flight: bool,  // committed, awaiting wl_buffer.release
    // Guest-side trace: the acquire sequence that filled this slot, so a
    // present can be tied back to the acquire that handed the guest the image.
    pub acq_seq: u64,
    // The daemon's scan-out inode for this slot's buffer, or 0 when the buffer
    // was allocated locally. Identity and logging only: the release floor is
    // this shim's own, one period after the last release, and is not filed
    // against a buffer (see `crate::scanout::note_release`).
    pub ino: u64,
    // The instant the floor for the release landing on this slot expires, in
    // `CLOCK_MONOTONIC` ns, stamped when the release arrived.
    pub floor_ns: u64,
    // Set when the compositor returned the buffer before that instant: the slot
    // stays in flight until the floor passes, and `acquire_take_free` is what
    // hands it back then.
    pub release_held: bool,
    // The slot's `AHardwareBuffer` reference on a device that imports through
    // `VK_ANDROID_external_memory_android_hardware_buffer`, null otherwise.
    pub ahb: *mut c_void,
}

pub struct ScShared {
    pub jobs: VecDeque<i32>,
    /// Single-entry mailbox for the mailbox/immediate modes: a newer present
    /// replaces any pending frame, so the slot map holds at most one entry.
    pub mailbox: Option<i32>,
    pub stopping: bool,
}

pub struct GnSwapchain {
    pub surf: *mut GnSurface,
    pub dev: *mut GnDevice,
    pub format: VkFormat,
    pub extent: VkExtent2D,
    pub fourcc: u32,
    pub usage: VkImageUsageFlags,
    /// The present mode the shim honours for this swapchain (override wins over
    /// the app's request; see `effective_present_mode`). Read by
    /// `present_dispatch` on every present.
    pub present_mode: VkPresentModeKHR,
    /// Every slot the shim owns: the guest's first (`guest_slots` of them), then
    /// the generation destinations, which the guest never sees.
    pub slots: Vec<GnSlot>,
    /// How many leading entries of `slots` are the guest's swapchain images.
    pub guest_slots: u32,
    pub worker: Option<std::thread::JoinHandle<()>>,
    pub shared: Mutex<ScShared>,
    pub cv_put: Condvar,
    pub cv_free: Condvar,
    pub x11_trace: u32,
    pub next: *mut GnSwapchain,
    /// Set once a destination could not be made: the set stops growing.
    pub gen_no_grow: bool,
}

// ---------------------------------------------------------------- wayland

pub struct GnWayland {
    pub dpy: *mut wl_display, // winewayland's, or our own (X11 mode)
    pub comp: *mut wl_proxy,
    pub subcomp: *mut wl_proxy,
    pub dmabuf: *mut wl_proxy,
    pub swapchain_factory: *mut wl_proxy, // gamescope_swapchain_factory_v2
    pub queue: *mut wlproto::wl_event_queue,
    pub dispatch_started: bool,
    pub own_dpy: bool, // X11 mode: we connected (WAYLAND_DISPLAY env)
    pub ready: bool,
    pub failed: bool,
}

// Touched from the setup thread, the dispatch thread and present workers
// (raw wayland pointers — the C's locking model is kept verbatim).
unsafe impl Send for GnWayland {}

static G_WL: Mutex<GnWayland> = Mutex::new(GnWayland {
    dpy: ptr::null_mut(),
    comp: ptr::null_mut(),
    subcomp: ptr::null_mut(),
    dmabuf: ptr::null_mut(),
    swapchain_factory: ptr::null_mut(),
    queue: ptr::null_mut(),
    dispatch_started: false,
    own_dpy: false,
    ready: false,
    failed: false,
});

extern "C" fn registry_global(
    data: *mut c_void,
    reg: *mut wl_proxy,
    id: u32,
    iface: *const c_char,
    version: u32,
) {
    let w = data as *mut GnWayland;
    let iface = unsafe { CStr::from_ptr(iface) }.to_bytes();
    unsafe {
        if iface == b"wl_compositor" && version >= 4 {
            (*w).comp = wlproto::registry_bind(reg, id, &wlproto::wl_compositor_interface, 4);
        } else if iface == b"wl_subcompositor" {
            (*w).subcomp = wlproto::registry_bind(reg, id, &wlproto::wl_subcompositor_interface, 1);
        } else if iface == b"zwp_linux_dmabuf_v1" && version >= 3 {
            (*w).dmabuf =
                wlproto::registry_bind(reg, id, &wlproto::ZWP_LINUX_DMABUF_V1_INTERFACE, 3);
        } else if iface == b"gamescope_swapchain_factory_v2" && !use_guest_surface() {
            // Capability switch (`GN_VKSHIM_GUEST_SURFACE`): the
            // gamescope_swapchain_factory_v2 global is bound only while the
            // override route (the default) is active. The guest-surface
            // (opt-in) route binds exactly the globals it always did and nothing more.
            (*w).swapchain_factory = wlproto::registry_bind(
                reg,
                id,
                &wlproto::GAMESCOPE_SWAPCHAIN_FACTORY_V2_INTERFACE,
                1,
            );
        }
    }
}

extern "C" fn registry_remove(_data: *mut c_void, _reg: *mut wl_proxy, _id: u32) {}

// Listener vtables: libwayland indexes them by event opcode. Stored as raw
// fn pointers — wl_proxy_add_listener takes an untyped implementation array.
type RawListenerFn = extern "C" fn();
static REGISTRY_LISTENER: [RawListenerFn; 2] = [
    unsafe {
        std::mem::transmute::<
            extern "C" fn(*mut c_void, *mut wl_proxy, u32, *const c_char, u32),
            RawListenerFn,
        >(registry_global)
    },
    unsafe {
        std::mem::transmute::<extern "C" fn(*mut c_void, *mut wl_proxy, u32), RawListenerFn>(
            registry_remove,
        )
    },
];
static BUFFER_LISTENER: [RawListenerFn; 1] = [unsafe {
    std::mem::transmute::<extern "C" fn(*mut c_void, *mut wl_proxy), RawListenerFn>(
        on_buffer_release,
    )
}];

fn dispatch_main() {
    let (dpy, queue) = {
        let w = G_WL.lock().unwrap();
        (w.dpy, w.queue)
    };
    let fd = unsafe { wlproto::wl_display_get_fd(dpy) };
    let mut failed = false;
    // Canonical multi-threaded libwayland read loop: flushes go through
    // SEND_LOCK (never concurrent with another sender, see SEND_LOCK), the
    // blocking poll happens with no lock held, and event dispatch runs
    // lock-free on this thread. wl_display_dispatch_queue() cannot be used:
    // its internal pre-poll flush races the present worker's marshal+flush.
    'outer: loop {
        {
            let _sg = SEND_LOCK.lock().unwrap();
            if unsafe { wlproto::wl_display_flush(dpy) } < 0
                && std::io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN)
            {
                failed = true;
                break;
            }
        }
        loop {
            let n = unsafe { wlproto::wl_display_dispatch_queue_pending(dpy, queue) };
            if n <= 0 {
                if n < 0 {
                    failed = true;
                }
                break;
            }
        }
        if failed {
            break;
        }
        while unsafe { wlproto::wl_display_prepare_read_queue(dpy, queue) } != 0 {
            if unsafe { wlproto::wl_display_dispatch_queue_pending(dpy, queue) } < 0 {
                failed = true;
                break 'outer;
            }
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, -1) } < 0 {
            let e = std::io::Error::last_os_error().raw_os_error();
            unsafe { wlproto::wl_display_cancel_read(dpy) };
            if e == Some(libc::EINTR) {
                continue;
            }
            failed = true;
            break;
        }
        if unsafe { wlproto::wl_display_read_events(dpy) } < 0 {
            failed = true;
            break;
        }
    }
    let err = unsafe { wlproto::wl_display_get_error(dpy) };
    G_WL.lock().unwrap().failed = true;
    crate::logf!(gndrm_obf::obf!(
        "wayland dispatch thread exited: {}",
        if err != 0 {
            std::io::Error::from_raw_os_error(err).to_string()
        } else if failed {
            std::io::Error::last_os_error().to_string()
        } else {
            "display closed".to_string()
        }
    ));
}

fn wl_setup(dpy: *mut wl_display) -> bool {
    let mut w = G_WL.lock().unwrap();
    if w.ready || w.failed {
        return !w.failed;
    }
    let wptr: *mut GnWayland = &mut *w;
    unsafe {
        let dpy = if dpy.is_null() {
            let c = wlproto::wl_display_connect(std::ptr::null());
            if c.is_null() {
                crate::logf!(gndrm_obf::obf!(
                    "proxy wayland connect failed (WAYLAND_DISPLAY unset?)"
                ));
                (*wptr).failed = true;
                return false;
            }
            (*wptr).own_dpy = true;
            c
        } else {
            dpy
        };
        (*wptr).dpy = dpy;
        wlproto::wl_log_set_handler_client(gn_wl_log);
        (*wptr).queue = wlproto::wl_display_create_queue(dpy);
        let reg = wlproto::display_get_registry(dpy);
        wlproto::wl_proxy_set_queue(reg, (*wptr).queue);
        wlproto::wl_proxy_add_listener(reg, REGISTRY_LISTENER.as_ptr() as *mut _, wptr.cast());
        if wlproto::wl_display_roundtrip_queue(dpy, (*wptr).queue) < 0 {
            (*wptr).failed = true;
        }
        wlproto::wl_proxy_destroy(reg);
        if !(*wptr).failed && (*wptr).dmabuf.is_null() {
            crate::logf!(gndrm_obf::obf!(
                "compositor misses zwp_linux_dmabuf_v1 v3 — proxy cannot present"
            ));
            (*wptr).failed = true;
        }
        if !(*wptr).failed {
            let spawned = std::thread::Builder::new()
                .name("gn-wl-dispatch".into())
                .spawn(dispatch_main);
            if spawned.is_err() {
                crate::logf!(gndrm_obf::obf!("failed to start wayland dispatch thread"));
                (*wptr).failed = true;
            }
        }
        (*wptr).ready = !(*wptr).failed;
        !(*wptr).failed
    }
}

/// `clock_gettime(CLOCK_MONOTONIC)` in nanoseconds — the domain the daemon's
/// release floor is expressed in, so a floor is compared with no conversion.
fn monotonic_ns() -> u64 {
    // SAFETY: clock_gettime with a valid clock id and a valid out-pointer.
    let ts = unsafe {
        let mut ts: libc::timespec = core::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        ts
    };
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}

extern "C" fn on_buffer_release(data: *mut c_void, _buf: *mut wl_proxy) {
    let slot = data as *mut GnSlot;
    unsafe {
        let sc = (*slot).owner;
        if LOG_RELEASE.fetch_add(1, Ordering::Relaxed) < LOG_FIRST_N {
            crate::logf!(gndrm_obf::obf!("proxy release: slot={slot:?}"));
        }
        // The target may have changed since this shim connected, and a push
        // from the daemon is read here because a release is the one event this
        // shim sees on every frame. Ahead of the floor, so the release it paces
        // is measured against the target it arrived under.
        crate::scanout::poll_notes();
        // The floor this release is held to, measured from this shim's own
        // previous release (see `scanout::note_release`): the instant the slot
        // may go back to the guest.
        let now_ns = monotonic_ns();
        // Destinations are appended after the guest's slots; only a release that
        // frees a guest slot advances the cap's schedule.
        let guest_slot = {
            let guest = (*sc).guest_slots as usize;
            let slots = (*sc).slots.as_ptr();
            ((slot as usize - slots as usize) / core::mem::size_of::<GnSlot>()) < guest
        };
        let floor_ns = crate::scanout::note_release(now_ns, guest_slot);
        let capped = crate::scanout::target_fps() != 0;
        // The floor paces the guest, so only a guest slot is held to it: a
        // generation destination held back is one fewer for the next batch.
        let held = guest_slot && now_ns < floor_ns;
        let _g = (*sc).shared.lock().unwrap();
        if capped {
            REL_CAPPED_N.fetch_add(1, Ordering::Relaxed);
        }
        if !guest_slot {
            GEN_RELEASES.fetch_add(1, Ordering::Relaxed);
        }
        if held {
            // This runs on the Wayland dispatch thread, so the hold is state,
            // not a wait: the slot keeps `in_flight`, and the instant it may be
            // freed is recorded for `acquire_take_free`, which is already
            // running on the guest's blocked acquire. Nothing here can stall
            // the connection's other events on a floor.
            (*slot).floor_ns = floor_ns;
            (*slot).release_held = true;
            let hold_us = ((floor_ns - now_ns) / 1_000) as usize;
            REL_HOLD_N.fetch_add(1, Ordering::Relaxed);
            REL_HOLD_SUM_US.fetch_add(hold_us, Ordering::Relaxed);
            REL_HOLD_MAX_US.fetch_max(hold_us, Ordering::Relaxed);
            if hold_us >= 8_333 {
                REL_HOLD_GE_8333.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            (*slot).in_flight = false;
            (*sc).cv_free.notify_all();
        }
        drop(_g);
        N_REL.fetch_add(1, Ordering::Relaxed);
        if rate_ok(&RATE_REL, RATE_EVERY) {
            crate::logf!(gndrm_obf::obf!(
                "gn-vkshim release: slot={slot:?} {}",
                slot_flags(sc)
            ));
        }
    }
}

// ------------------------------------------------------------- bookkeeping

pub struct Globals {
    pub real_gipa: Option<GipaFn>,
    pub real_handle: *mut c_void,
    pub instances: *mut GnInstance,
    pub devices: *mut GnDevice,
    pub surfaces: *mut GnSurface,
    pub swapchains: *mut GnSwapchain,
}

// All list heads and the real-driver pointers sit behind one mutex (the
// C's g_mtx); raw list pointers are only walked under it.
unsafe impl Send for Globals {}

pub static G: Mutex<Globals> = Mutex::new(Globals {
    real_gipa: None,
    real_handle: ptr::null_mut(),
    instances: ptr::null_mut(),
    devices: ptr::null_mut(),
    surfaces: ptr::null_mut(),
    swapchains: ptr::null_mut(),
});

pub fn set_real_gipa(gipa: GipaFn) {
    G.lock().unwrap().real_gipa = Some(gipa);
}

pub fn set_real_handle(handle: *mut c_void) {
    G.lock().unwrap().real_handle = handle;
}

unsafe fn find_instance(inst: VkInstance) -> *mut GnInstance {
    let g = G.lock().unwrap();
    let mut p = g.instances;
    while !p.is_null() {
        if (*p).real == inst {
            return p;
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

pub(crate) unsafe fn find_instance_by_pd(pd: VkPhysicalDevice) -> *mut GnInstance {
    let g = G.lock().unwrap();
    let mut p = g.instances;
    while !p.is_null() {
        for i in 0..(*p).n_pds as usize {
            if (*p).pds[i] == pd {
                return p;
            }
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

pub(crate) unsafe fn find_device(dev: VkDevice) -> *mut GnDevice {
    let g = G.lock().unwrap();
    let mut p = g.devices;
    while !p.is_null() {
        if p.cast::<c_void>() == dev {
            return p;
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

unsafe fn find_queue(q: VkQueue) -> *mut GnQueue {
    let g = G.lock().unwrap();
    let mut p = g.devices;
    while !p.is_null() {
        let mut gq = (*p).queues;
        while !gq.is_null() {
            if gq.cast::<c_void>() == q {
                return gq;
            }
            gq = (*gq).next;
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

static WRAPPER_HISTORY: Mutex<Vec<usize>> = Mutex::new(Vec::new());

fn noted_wrapper(p: usize) {
    if let Ok(mut v) = WRAPPER_HISTORY.lock() {
        if v.len() < 4096 {
            v.push(p);
        }
    }
}

fn was_wrapper(p: usize) -> bool {
    WRAPPER_HISTORY
        .lock()
        .map(|v| v.contains(&p))
        .unwrap_or(false)
}

/// A dropped call must never be invisible. Every diagnostic line in this shim
/// now follows `GNDRM_LOG_VKSHIM` (via `logf!`, off by default), so a call the
/// shim dropped is named when the gate is on rather than being indistinguishable
/// from one the guest never made. Bounded per site so a storm of misses cannot
/// flood logcat.
///
/// `msg` is a thunk: the gate and the per-site budget are both checked before
/// it runs, so a gated-off or bounded-out site never decodes its message.
const LOUD_MAX: u32 = 6;
fn loud(site: &'static str, msg: impl FnOnce() -> String) {
    if !crate::vkshim_enabled() {
        return;
    }
    static SEEN: Mutex<Vec<(&'static str, u32)>> = Mutex::new(Vec::new());
    if let Ok(mut v) = SEEN.lock() {
        match v.iter_mut().find(|(s, _)| *s == site) {
            Some((_, c)) if *c >= LOUD_MAX => return,
            Some((_, c)) => *c += 1,
            None => v.push((site, 1)),
        }
    }
    let line = msg();
    crate::logf!(gndrm_obf::obf!("{}", line));
}

/// A handle the shim could not resolve, against the wrapper table it searched,
/// so a wrong-handle miss names both values in one line.
unsafe fn handle_note(found: bool, searched: usize, first_wrapper: usize, n: usize) -> String {
    format!(
        "{}",
        gndrm_obf::obf!(
            "found={found} searched={searched:#x} wrappers={n} first={first_wrapper:#x} pid={}",
            std::process::id()
        )
    )
}

/// First wrapper in the device table (0 when empty) and the table size.
unsafe fn device_table() -> (usize, usize) {
    let g = G.lock().unwrap();
    let mut p = g.devices;
    let n = {
        let mut n = 0;
        while !p.is_null() {
            n += 1;
            p = unsafe { (*p).next };
        }
        n
    };
    (g.devices as usize, n)
}

unsafe fn find_cmd_any(cmd: VkCommandBuffer) -> *mut GnCmd {
    let g = G.lock().unwrap();
    let mut d = g.devices;
    while !d.is_null() {
        unsafe {
            let mut gc = (*d).cmds;
            while !gc.is_null() {
                if gc.cast::<c_void>() == cmd {
                    return gc;
                }
                gc = (*gc).next;
            }
            d = (*d).next;
        }
    }
    ptr::null_mut()
}

unsafe fn real_cmd(cmd: VkCommandBuffer) -> VkCommandBuffer {
    let gc = unsafe { find_cmd_any(cmd) };
    if !gc.is_null() {
        return unsafe { (*gc).real };
    }
    // Not in any device's list. Pass it through: a handle the shim never
    // wrapped is the driver's own. But say so when it *was* a wrapper —
    // that case hands the driver a shim pointer and faults inside it.
    if was_wrapper(cmd as usize) {
        crate::logf!(gndrm_obf::obf!(
            "proxy: BUG submit carries an untracked wrapper CB {cmd:?}"
        ));
    }
    cmd
}

// ------------------------------------------- nested wrapped-handle translation
//
// A command's parameters can carry wrapped handles nested inside structs and
// arrays (`VkSubmitInfo::pCommandBuffers`,
// `VkSubmitInfo2::pCommandBufferInfos[].commandBuffer`,
// `vkCmdExecuteCommands::pCommandBuffers`). Argument-0 swapping cannot reach
// them, so the driver would walk a shim wrapper as if it were its own object.
// Which fields these are is a registry property, generated into
// `vk_dispatch_gen::VK_NESTED_HANDLES`; `translate_nested` applies that one
// plan uniformly, replacing every wrapper with the driver's real handle.

/// Owns the translated copies for the duration of a driver call. Pointers into
/// a `Box`'s heap allocation stay valid across `bufs` growth, so a copy already
/// published through an argument word does not move.
pub struct NestedStage {
    bufs: Vec<Box<[u8]>>,
}

impl Default for NestedStage {
    fn default() -> Self {
        Self::new()
    }
}

impl NestedStage {
    pub fn new() -> Self {
        Self { bufs: Vec::new() }
    }

    /// Copy `len` bytes from `src` into a stage-owned buffer and return its
    /// stable pointer.
    unsafe fn copy(&mut self, src: *const u8, len: usize) -> *mut u8 {
        let mut v = vec![0u8; len].into_boxed_slice();
        unsafe { ptr::copy_nonoverlapping(src, v.as_mut_ptr(), len) };
        let p = v.as_mut_ptr();
        self.bufs.push(v);
        p
    }
}

/// The driver's real handle for a shim wrapper of `kind`. A value the shim
/// never wrapped is already the driver's own and passes through unchanged.
unsafe fn map_handle(kind: crate::vk_dispatch_gen::HandleKind, h: usize) -> usize {
    use crate::vk_dispatch_gen::HandleKind;
    let h = h as *mut c_void;
    match kind {
        HandleKind::CommandBuffer => unsafe { real_cmd(h) as usize },
        HandleKind::Queue => {
            let gq = unsafe { find_queue(h as VkQueue) };
            if gq.is_null() {
                h as usize
            } else {
                unsafe { (*gq).real as usize }
            }
        }
        HandleKind::Device => {
            let gd = unsafe { find_device(h as VkDevice) };
            if gd.is_null() {
                h as usize
            } else {
                unsafe { (*gd).real as usize }
            }
        }
    }
}

/// Translate, in place, the wrapped handles `name`'s generated plan marks
/// inside the parameter words `args`, staging every copy in `stage`. Returns
/// the number of handles replaced.
pub unsafe fn translate_nested(
    name: &str,
    args: &mut [usize; 6],
    stage: &mut NestedStage,
) -> usize {
    let mut done = 0usize;
    for nh in crate::vk_dispatch_gen::nested_handles(name) {
        let p = args[nh.param as usize];
        if p == 0 {
            continue;
        }
        if nh.count_param == 255 {
            args[nh.param as usize] = unsafe { map_handle(nh.kind, p) };
            done += 1;
            continue;
        }
        let n = args[nh.count_param as usize];
        if n == 0 {
            continue;
        }
        if nh.hops.is_empty() {
            let copy = unsafe { stage.copy(p as *const u8, n * nh.elem_stride) } as *mut usize;
            for i in 0..n {
                let v = unsafe { *copy.add(i) };
                unsafe { *copy.add(i) = map_handle(nh.kind, v) };
            }
            args[nh.param as usize] = copy as usize;
            done += n;
        } else {
            let copy = unsafe { stage.copy(p as *const u8, n * nh.elem_stride) };
            for i in 0..n {
                let elem = unsafe { copy.add(i * nh.elem_stride) };
                done += unsafe { hop_translate(nh.hops, elem, nh.kind, stage) };
            }
            args[nh.param as usize] = copy as usize;
        }
    }
    if done > 0 {
        note_nested(name, done);
    }
    done
}

/// Walk one hop chain from a top-level element, translating the handle array it
/// reaches. `hops[0]` locates the child pointer, or a single handle value when
/// `count_off` is `usize::MAX`.
unsafe fn hop_translate(
    hops: &[crate::vk_dispatch_gen::Hop],
    elem: *mut u8,
    kind: crate::vk_dispatch_gen::HandleKind,
    stage: &mut NestedStage,
) -> usize {
    let h = hops[0];
    if h.count_off == usize::MAX {
        // A single handle value per element (VkCommandBufferSubmitInfo).
        let slot = unsafe { elem.add(h.off) as *mut usize };
        let v = unsafe { *slot };
        if v == 0 {
            return 0;
        }
        unsafe { *slot = map_handle(kind, v) };
        return 1;
    }
    let child = unsafe { *(elem.add(h.off) as *const usize) };
    let cnt = unsafe { *(elem.add(h.count_off) as *const u32) } as usize;
    if child == 0 || cnt == 0 {
        return 0;
    }
    if hops.len() == 1 {
        let copy = unsafe { stage.copy(child as *const u8, cnt * h.stride) } as *mut usize;
        for i in 0..cnt {
            let v = unsafe { *copy.add(i) };
            unsafe { *copy.add(i) = map_handle(kind, v) };
        }
        unsafe { *(elem.add(h.off) as *mut usize) = copy as usize };
        cnt
    } else {
        let copy = unsafe { stage.copy(child as *const u8, cnt * h.stride) };
        let mut done = 0usize;
        for i in 0..cnt {
            let e = unsafe { copy.add(i * h.stride) };
            done += unsafe { hop_translate(&hops[1..], e, kind, stage) };
        }
        unsafe { *(elem.add(h.off) as *mut usize) = copy as usize };
        done
    }
}

/// Log the first translation of each command name (bounded: one line per name),
/// so a session shows the nested policy engaging.
fn note_nested(name: &str, n: usize) {
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    if let Ok(mut v) = SEEN.lock() {
        if v.len() < 32 && !v.iter().any(|s| s == name) {
            v.push(name.to_string());
            // Always on: "the guest reached the shim's submit/execute path and
            // the shim translated nested wrapper handles" is the fact that
            // settles whether the submit path is live, and the guest process
            // does not set GNDRM_LOG_VKSHIM. Bounded to one line per name.
            crate::logf!(gndrm_obf::obf!(
                "proxy nested translate: {name} ({n} handle(s))"
            ));
        }
    }
}

unsafe fn find_cmd(gd: *mut GnDevice, cmd: VkCommandBuffer) -> *mut GnCmd {
    let mut gc = (*gd).cmds;
    while !gc.is_null() {
        if gc.cast::<c_void>() == cmd {
            return gc;
        }
        gc = (*gc).next;
    }
    ptr::null_mut()
}

unsafe fn find_surface(surface: VkSurfaceKHR) -> *mut GnSurface {
    let g = G.lock().unwrap();
    let mut p = g.surfaces;
    while !p.is_null() {
        if p as usize as VkSurfaceKHR == surface {
            return p;
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

unsafe fn find_swapchain(sc: VkSwapchainKHR) -> *mut GnSwapchain {
    let g = G.lock().unwrap();
    let mut p = g.swapchains;
    while !p.is_null() {
        if p as usize as VkSwapchainKHR == sc {
            return p;
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

pub(crate) unsafe fn gn_real_device_proc(gd: *mut GnDevice, name: &CStr) -> PFN_vkVoidFunction {
    let (handle, gipa, inst) = {
        let g = G.lock().unwrap();
        let inst = if !gd.is_null() {
            (*(*gd).inst).real
        } else if !g.instances.is_null() {
            (*g.instances).real
        } else {
            ptr::null_mut()
        };
        (g.real_handle, g.real_gipa, inst)
    };
    // Which handle a name resolves through decides whether the guest gets the
    // driver's own implementation or the hooks' replacement. The copy/memory
    // entry points are exactly where the guest's calls came back empty, so
    // allow forcing the driver's own (GN_VKSHIM_PREFER_DRIVER_PROC=1).
    let prefer_driver = std::env::var_os("GN_VKSHIM_PREFER_DRIVER_PROC").is_some();
    let lookup = |n: &CStr| -> (PFN_vkVoidFunction, &'static str) {
        if !handle.is_null() && !prefer_driver {
            let sym = unsafe { libc::dlsym(handle, n.as_ptr()) };
            if !sym.is_null() {
                return (
                    Some(unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn()>(sym) }),
                    "dlsym",
                );
            }
        }
        // With a device at hand its own vkGetDeviceProcAddr is final: that is
        // the one resolver the loader would have asked, and the one that knows
        // which extensions this device enabled. The instance resolver is not
        // safe for device-level names: mesa answers them with trampolines
        // whether or not the device has the extension (VK_ARM_tensors on
        // turnip), and for vkCmdEndRendering2EXT its lookup dereferences NULL
        // outright. winevulkan resolves every device entry point by name
        // right after vkCreateDevice, which is how a 32-bit guest ended in a
        // fault inside that call and an abort on assert(!status)
        // (Terraria under gamescope on turnip, second device from the Steam
        // overlay). The instance resolver stays for the device-less callers.
        if !gd.is_null() {
            if let Some(gdp) = unsafe { (*(*gd).inst).f.gdp } {
                let f = unsafe { gdp((*gd).real, n.as_ptr()) };
                return if f.is_some() { (f, "gdpa") } else { (None, "miss") };
            }
        }
        if !inst.is_null() {
            if let Some(gipa) = gipa {
                let f = unsafe { gipa(inst, n.as_ptr()) };
                if f.is_some() {
                    return (f, "gipa");
                }
            }
        }
        (None, "miss")
    };
    let (mut fnptr, mut via) = lookup(name);
    // Core names the driver only exports under an extension spelling. One
    // hop only: alias_of maps both ways, so recursing ping-pongs forever when
    // neither spelling resolves.
    if fnptr.is_none() {
        if let Some(alias) = name
            .to_str()
            .ok()
            .and_then(crate::vk_dispatch_gen::alias_of)
        {
            let c = std::ffi::CString::new(alias).unwrap();
            fnptr = lookup(&c).0;
            if fnptr.is_some() {
                via = "alias";
            }
        }
    }
    // The image/memory entry points are where the guest's calls silently do
    // nothing, so record where each one came from.
    if let Ok(mut seen) = REAL_PROC_SEEN.lock() {
        let n = name.to_string_lossy().into_owned();
        if seen.len() < 128
            && !seen.iter().any(|s| s == &n)
            && (n.contains("Copy")
                || n.contains("Clear")
                || n.contains("Bind")
                || n.contains("Allocate")
                || n.contains("Map")
                || n.contains("CreateImage")
                || n.contains("SubresourceLayout"))
        {
            crate::logf!(gndrm_obf::obf!("proxy: real proc {n} via {via}"));
            seen.push(n);
        }
    }
    fnptr
}

pub(crate) unsafe fn gn_real_inst_proc(inst: VkInstance, name: &CStr) -> PFN_vkVoidFunction {
    let (handle, gipa) = {
        let g = G.lock().unwrap();
        (g.real_handle, g.real_gipa)
    };
    if !handle.is_null() {
        let sym = unsafe { libc::dlsym(handle, name.as_ptr()) };
        if !sym.is_null() {
            return Some(unsafe {
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn()>(sym)
            });
        }
    }
    if let Some(gipa) = gipa {
        return unsafe { gipa(inst, name.as_ptr()) };
    }
    None
}

// Chain-link resolvers for the create-info we hand to the hooks. The outer
// loader appends a VkLayer{Instance,Device}CreateInfo link to the pNext
// chain; forwarded verbatim, the hooks initialize their dispatch tables
// from it — resolving through the outer loader into THIS shim's entry
// points, which lands our gn_* functions in their device/queue slots and
// recurses (our QueueSubmit → hooks forwarder → our GetDeviceQueue → …)
// until the stack overflows. Terminating the chain with resolvers confined
// to the hooks' own namespace keeps every slot they initialize consistent
// with their own CreateDevice.
unsafe extern "C" fn gn_chain_gipa(
    instance: VkInstance,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    let gipa = G.lock().unwrap().real_gipa;
    match gipa {
        Some(gipa) => unsafe { gipa(instance, name) },
        None => None,
    }
}

unsafe extern "C" fn gn_chain_gpdpa(
    instance: VkInstance,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    unsafe { gn_chain_gipa(instance, name) }
}

unsafe extern "C" fn gn_chain_gdpa(_device: VkDevice, name: *const c_char) -> PFN_vkVoidFunction {
    unsafe { gn_real_device_proc(ptr::null_mut(), CStr::from_ptr(name)) }
}

// Statics shared with the loader's chain links (contents never change).
struct SyncLink<T>(T);
unsafe impl<T> Sync for SyncLink<T> {}

static G_CHAIN_INST_LINK: SyncLink<VkLayerInstanceLink> = SyncLink(VkLayerInstanceLink {
    pNext: ptr::null_mut(),
    pfnNextGetInstanceProcAddr: Some(unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction,
            unsafe extern "C" fn(),
        >(gn_chain_gipa)
    }),
    pfnNextGetPhysicalDeviceProcAddr: Some(unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction,
            unsafe extern "C" fn(),
        >(gn_chain_gpdpa)
    }),
});

static G_CHAIN_DEV_LINK: SyncLink<VkLayerDeviceLink> = SyncLink(VkLayerDeviceLink {
    pNext: ptr::null_mut(),
    pfnNextGetInstanceProcAddr: Some(unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction,
            unsafe extern "C" fn(),
        >(gn_chain_gipa)
    }),
    pfnNextGetDeviceProcAddr: Some(unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(VkDevice, *const c_char) -> PFN_vkVoidFunction,
            unsafe extern "C" fn(),
        >(gn_chain_gdpa)
    }),
});

unsafe fn slice_or_empty<'a, T>(p: *const T, n: usize) -> &'a [T] {
    if p.is_null() || n == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(p, n) }
    }
}

// ------------------------------------------------------- prereq probe

const DMA_BUF_EXTS: [&CStr; 4] = [
    c"VK_KHR_external_memory",
    c"VK_KHR_external_memory_fd",
    c"VK_EXT_external_memory_dma_buf",
    c"VK_EXT_image_drm_format_modifier",
];

const AHB_EXTS: [&CStr; 2] = [
    c"VK_KHR_external_memory",
    c"VK_ANDROID_external_memory_android_hardware_buffer",
];

/// How swapchain slots reach the driver, from its device extensions: `Some(false)`
/// for dma-buf import, `Some(true)` for `AHardwareBuffer` import (only when
/// `allow_ahb`), `None` when neither set is complete.
fn import_mode(has: &dyn Fn(&CStr) -> bool, allow_ahb: bool) -> Option<bool> {
    let missing = DMA_BUF_EXTS.iter().copied().find(|e| !has(e));
    let Some(missing) = missing else {
        return Some(false);
    };
    if allow_ahb && AHB_EXTS.iter().all(|e| has(e)) {
        return Some(true);
    }
    crate::logf!(gndrm_obf::obf!(
        "proxy prereq: device lacks {}",
        missing.to_str().unwrap()
    ));
    None
}

fn dev_exts_satisfied(
    enum_de: PFN_vkEnumerateDeviceExtensionProperties,
    pd: VkPhysicalDevice,
    allow_ahb: bool,
) -> bool {
    let mut n: u32 = 0;
    if unsafe { enum_de(pd, ptr::null(), &mut n, ptr::null_mut()) } != VK_SUCCESS {
        return false;
    }
    let mut props = vec![
        VkExtensionProperties {
            extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
            specVersion: 0,
        };
        n as usize
    ];
    if unsafe { enum_de(pd, ptr::null(), &mut n, props.as_mut_ptr()) } != VK_SUCCESS {
        return false;
    }
    let has = |want: &CStr| {
        props[..n as usize].iter().any(|p| {
            let name = unsafe { CStr::from_ptr(p.extensionName.as_ptr()) };
            name == want
        })
    };
    match import_mode(&has, allow_ahb) {
        Some(true) => {
            crate::logf!(gndrm_obf::obf!(
                "proxy prereq: no dma-buf import, using AHardwareBuffer import"
            ));
            true
        }
        Some(false) => true,
        None => false,
    }
}

/// `allow_ahb` admits a driver that can import only `AHardwareBuffer`s. Only a
/// guest process may take that: gamescope itself imports client dma-bufs.
pub fn device_prereqs(gipa: GipaFn, allow_ahb: bool) -> bool {
    let create_inst: Option<PFN_vkCreateInstance> =
        unsafe { std::mem::transmute(gipa(ptr::null_mut(), c"vkCreateInstance".as_ptr())) };
    let Some(create_inst) = create_inst else {
        crate::logf!(gndrm_obf::obf!(
            "proxy prereq: driver exposes no vkCreateInstance"
        ));
        return false;
    };
    let app = VkApplicationInfo {
        sType: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        pNext: ptr::null(),
        pApplicationName: c"gn-vkshim-probe".as_ptr(),
        applicationVersion: 0,
        pEngineName: ptr::null(),
        engineVersion: 0,
        apiVersion: VK_API_VERSION_1_3,
    };
    let ci = VkInstanceCreateInfo {
        sType: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        pNext: ptr::null(),
        flags: 0,
        pApplicationInfo: &app,
        enabledLayerCount: 0,
        ppEnabledLayerNames: ptr::null(),
        enabledExtensionCount: 0,
        ppEnabledExtensionNames: ptr::null(),
    };
    let mut inst: VkInstance = ptr::null_mut();
    let res = unsafe { create_inst(&ci, ptr::null(), &mut inst) };
    if res != VK_SUCCESS {
        crate::logf!(gndrm_obf::obf!(
            "proxy prereq: probe vkCreateInstance failed ({res})"
        ));
        return false;
    }
    // Instance-level procs must be fetched against a live instance: mesa
    // resolves only global functions for a NULL instance.
    let enum_pds: Option<PFN_vkEnumeratePhysicalDevices> =
        unsafe { std::mem::transmute(gipa(inst, c"vkEnumeratePhysicalDevices".as_ptr())) };
    let enum_de: Option<PFN_vkEnumerateDeviceExtensionProperties> = unsafe {
        std::mem::transmute(gipa(inst, c"vkEnumerateDeviceExtensionProperties".as_ptr()))
    };
    let destroy_inst: Option<PFN_vkDestroyInstance> =
        unsafe { std::mem::transmute(gipa(inst, c"vkDestroyInstance".as_ptr())) };
    let mut ok = false;
    match (enum_pds, enum_de, destroy_inst) {
        (Some(enum_pds), Some(enum_de), _) => {
            let mut n: u32 = 0;
            if unsafe { enum_pds(inst, &mut n, ptr::null_mut()) } == VK_SUCCESS && n > 0 {
                let mut pd: VkPhysicalDevice = ptr::null_mut();
                let mut one: u32 = 1;
                if unsafe { enum_pds(inst, &mut one, &mut pd) } == VK_SUCCESS {
                    ok = dev_exts_satisfied(enum_de, pd, allow_ahb);
                }
            } else {
                crate::logf!(gndrm_obf::obf!("proxy prereq: no physical devices"));
            }
        }
        _ => crate::logf!(gndrm_obf::obf!(
            "proxy prereq: instance-level procs unresolved"
        )),
    }
    if let Some(destroy_inst) = destroy_inst {
        unsafe { destroy_inst(inst, ptr::null()) };
    }
    if ok {
        AHB_IMPORT_ALLOWED.store(allow_ahb, Ordering::Relaxed);
    }
    ok
}

// ------------------------------------------------ instance-level functions

unsafe extern "C" fn gn_vkCreateInstance(
    ci: *const VkInstanceCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkInstance,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gipa = G.lock().unwrap().real_gipa;
    let Some(gipa) = gipa else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let real_create: Option<PFN_vkCreateInstance> =
        unsafe { std::mem::transmute(gipa(ptr::null_mut(), c"vkCreateInstance".as_ptr())) };
    let Some(real_create) = real_create else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let mut n: usize = 0;
    let names = unsafe {
        slice_or_empty(
            ci.ppEnabledExtensionNames,
            ci.enabledExtensionCount as usize,
        )
    };
    let mut exts: Vec<*const c_char> = vec![ptr::null(); names.len() + 16];
    for &name in names {
        let n_ = unsafe { CStr::from_ptr(name) };
        if n_ == c"VK_KHR_wayland_surface"
            || n_ == c"VK_KHR_xlib_surface"
            || n_ == c"VK_KHR_xcb_surface"
        {
            continue; // proxied, not real
        }
        if n < exts.len() {
            exts[n] = name;
            n += 1;
        }
    }
    let mut ci2 = VkInstanceCreateInfo { ..*ci };
    ci2.enabledExtensionCount = n as u32;
    ci2.ppEnabledExtensionNames = exts.as_ptr();
    // Confine the loader's chain link to the hooks' namespace (see
    // G_CHAIN_INST_LINK).
    let mut p = ci2.pNext as *mut VkBaseOutStructure;
    while !p.is_null() {
        unsafe {
            if (*p).sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO {
                let lci = p.cast::<VkLayerInstanceCreateInfo>();
                if (*lci).function == VK_LAYER_LINK_INFO {
                    (*lci).u.pLayerInfo = &raw const G_CHAIN_INST_LINK.0 as *mut _;
                }
            }
            p = (*p).pNext;
        }
    }
    let res = unsafe { real_create(&ci2, alloc, out) };
    if res != VK_SUCCESS {
        return res;
    }
    let gi = Box::new(GnInstance {
        real: unsafe { *out },
        f: RealInstFns {
            gdp: unsafe { std::mem::transmute(gn_real_inst_proc(*out, c"vkGetDeviceProcAddr")) },
            enum_pds: unsafe {
                std::mem::transmute(gn_real_inst_proc(*out, c"vkEnumeratePhysicalDevices"))
            },
            create_device: unsafe {
                std::mem::transmute(gn_real_inst_proc(*out, c"vkCreateDevice"))
            },
            destroy_instance: unsafe {
                std::mem::transmute(gn_real_inst_proc(*out, c"vkDestroyInstance"))
            },
        },
        pds: [ptr::null_mut(); 8],
        n_pds: 0,
        next: ptr::null_mut(),
    });
    let gi = Box::into_raw(gi);
    unsafe {
        if let Some(enum_pds) = (*gi).f.enum_pds {
            let mut cnt: u32 = 0;
            if enum_pds(*out, &mut cnt, ptr::null_mut()) == VK_SUCCESS && cnt > 0 {
                if cnt > 8 {
                    cnt = 8;
                }
                if enum_pds(*out, &mut cnt, (*gi).pds.as_mut_ptr()) == VK_SUCCESS {
                    (*gi).n_pds = cnt;
                }
            }
        }
    }
    {
        let mut g = G.lock().unwrap();
        unsafe {
            (*gi).next = g.instances;
            g.instances = gi;
        }
    }
    crate::logf!(gndrm_obf::obf!(
        "proxy instance {:?} (headless WSI backing)",
        unsafe { *out }
    ));
    if !LOUD_INSTANCE.swap(true, Ordering::Relaxed) {
        crate::logf!(gndrm_obf::obf!(
            "proxy: instance created pid={} real={:?} npds={}",
            std::process::id(),
            unsafe { *out },
            unsafe { (*gi).n_pds }
        ));
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkDestroyInstance(
    instance: VkInstance,
    alloc: *const VkAllocationCallbacks,
) {
    let gi = {
        let mut g = G.lock().unwrap();
        let mut pp: *mut *mut GnInstance = &raw mut g.instances;
        unsafe {
            while !(*pp).is_null() && (**pp).real != instance {
                pp = &raw mut (**pp).next;
            }
            let gi = *pp;
            if !gi.is_null() {
                *pp = (*gi).next;
            }
            gi
        }
    };
    if gi.is_null() {
        return;
    }
    unsafe {
        if let Some(destroy) = (*gi).f.destroy_instance {
            destroy(instance, alloc);
        }
        drop(Box::from_raw(gi));
    }
}

unsafe extern "C" fn gn_vkEnumerateInstanceExtensionProperties(
    layer: *const c_char,
    pCount: *mut u32,
    pProps: *mut VkExtensionProperties,
) -> VkResult {
    let gipa = G.lock().unwrap().real_gipa;
    let Some(gipa) = gipa else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let real_enum: Option<PFN_vkEnumerateInstanceExtensionProperties> = unsafe {
        std::mem::transmute(gipa(
            ptr::null_mut(),
            c"vkEnumerateInstanceExtensionProperties".as_ptr(),
        ))
    };
    let Some(real_enum) = real_enum else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    // Extensions the proxy answers itself. The three surface extensions are always
    // hidden from the real driver; `VK_EXT_swapchain_colorspace` is added in an
    // HDR session because it defines the PQ colourspace the surface formats
    // advertise, and a client that does not see the extension must ignore it.
    let names: [&[u8]; 4] = [
        b"VK_KHR_wayland_surface\0",
        b"VK_KHR_xlib_surface\0",
        b"VK_KHR_xcb_surface\0",
        b"VK_EXT_swapchain_colorspace\0",
    ];
    let extra: u32 = 3;
    unsafe {
        if pProps.is_null() {
            let mut n: u32 = 0;
            let res = real_enum(layer, &mut n, ptr::null_mut());
            if res != VK_SUCCESS {
                return res;
            }
            *pCount = n + extra;
            return VK_SUCCESS;
        }
        let cap = *pCount;
        if cap == 0 {
            *pCount = 0;
            return VK_INCOMPLETE;
        }
        let mut n = cap.saturating_sub(extra);
        let res = real_enum(layer, &mut n, pProps);
        if res != VK_SUCCESS && res != VK_INCOMPLETE {
            return res;
        }
        let total = n + extra;
        if cap >= total {
            // The surface extensions are proxied: winewayland clients use
            // the wayland one, winex11 clients (the gamescope path) the xlib
            // one, and Mesa's zink/kopper and EGL x11 the xcb one — the real
            // driver never sees any of them. `VK_EXT_swapchain_colorspace`
            // is NOT advertised: exposing it made vkd3d-proton titles present once
            // per swapchain and then stop, while the PQ surface format alone is
            // enough for DXVK to report an HDR10 output.
            for (i, name) in names[..extra as usize].iter().enumerate() {
                let slot = &mut *pProps.add(n as usize + i);
                slot.specVersion = 6;
                slot.extensionName[..name.len()]
                    .copy_from_slice(std::mem::transmute::<&[u8], &[c_char]>(*name));
            }
            *pCount = total;
            return VK_SUCCESS;
        }
        *pCount = total;
        VK_INCOMPLETE
    }
}

unsafe extern "C" fn gn_vkEnumerateDeviceExtensionProperties(
    pd: VkPhysicalDevice,
    layer: *const c_char,
    count: *mut u32,
    props: *mut VkExtensionProperties,
) -> VkResult {
    let gipa = G.lock().unwrap().real_gipa;
    let Some(gipa) = gipa else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    // Instance-level function: mesa resolves it only against a live instance.
    let gi = unsafe { find_instance_by_pd(pd) };
    let inst = if gi.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*gi).real }
    };
    let real: Option<PFN_vkEnumerateDeviceExtensionProperties> = unsafe {
        std::mem::transmute(gipa(inst, c"vkEnumerateDeviceExtensionProperties".as_ptr()))
    };
    let Some(real) = real else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    unsafe {
        let mut n: u32 = 0;
        let res = real(pd, layer, &mut n, ptr::null_mut());
        if res != VK_SUCCESS {
            return res;
        }
        // Spare slots: the proxy appends `VK_EXT_hdr_metadata` and, for
        // gamescope, `VK_EXT_robustness2` when the driver does not list them.
        let mut buf = vec![
            VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: 0,
            };
            n as usize + 4
        ];
        let mut got = n;
        let res = real(pd, layer, &mut got, buf.as_mut_ptr());
        if res != VK_SUCCESS && res != VK_INCOMPLETE {
            return res;
        }
        let mut kept = 0usize;
        let mut has_hdr_metadata = false;
        let mut has_robustness2 = false;
        let mut has_placed = false;
        let mut has_divisor_ext = false;
        let mut has_divisor_khr = false;
        for i in 0..got as usize {
            let name = CStr::from_ptr(buf[i].extensionName.as_ptr());
            if unimplemented_wsi_ext(name) {
                continue;
            }
            if name == c"VK_EXT_hdr_metadata" {
                has_hdr_metadata = true;
            }
            if name == c"VK_EXT_robustness2" {
                has_robustness2 = true;
            }
            if name == crate::placed::EXT_NAME {
                has_placed = true;
            }
            if name == crate::divisor::EXT_NAME {
                has_divisor_ext = true;
            }
            if name == crate::divisor::KHR_NAME {
                has_divisor_khr = true;
            }
            buf[kept] = buf[i];
            kept += 1;
        }
        if !has_hdr_metadata {
            // The proxy answers `vkSetHdrMetadataEXT` itself, so the extension
            // honours its only command whether or not the driver implements it. A
            // guest that is offered a PQ surface format needs to see this: vkd3d
            // re-enters its swapchain-recreate path and never issues a create when
            // a PQ colours is on offer without it.
            let mut prop = VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: 2,
            };
            let name = b"VK_EXT_hdr_metadata\0";
            for (j, b) in name.iter().enumerate() {
                prop.extensionName[j] = *b as std::os::raw::c_char;
            }
            buf[kept] = prop;
            kept += 1;
            crate::logf!(gndrm_obf::obf!(
                "proxy: advertised VK_EXT_hdr_metadata (driver does not list it)"
            ));
        }
        if !has_robustness2 && layer.is_null() && FAKE_ROBUSTNESS2.load(Ordering::Relaxed) {
            let mut prop = VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: 1,
            };
            let name = b"VK_EXT_robustness2\0";
            for (j, b) in name.iter().enumerate() {
                prop.extensionName[j] = *b as std::os::raw::c_char;
            }
            buf[kept] = prop;
            kept += 1;
            if !ROBUSTNESS2_FAKED.swap(true, Ordering::Relaxed) {
                crate::logf!(gndrm_obf::obf!(
                    "proxy: advertised VK_EXT_robustness2 (driver does not list it)"
                ));
            }
        }
        if !has_placed && layer.is_null() && crate::placed::enabled() {
            let mut prop = VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: 1,
            };
            for (j, b) in crate::placed::EXT_NAME.to_bytes().iter().enumerate() {
                prop.extensionName[j] = *b as std::os::raw::c_char;
            }
            buf[kept] = prop;
            kept += 1;
            crate::placed::note_faked();
        }
        if !has_divisor_ext && has_divisor_khr && layer.is_null() && crate::placed::enabled() {
            let mut prop = VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: 3,
            };
            for (j, b) in crate::divisor::EXT_NAME.to_bytes().iter().enumerate() {
                prop.extensionName[j] = *b as std::os::raw::c_char;
            }
            buf[kept] = prop;
            kept += 1;
            crate::divisor::note_faked();
        }
        if props.is_null() {
            *count = kept as u32;
            return VK_SUCCESS;
        }
        let n_copy = (*count as usize).min(kept);
        ptr::copy_nonoverlapping(buf.as_ptr(), props, n_copy);
        *count = n_copy as u32;
        if n_copy < kept {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        }
    }
}

// ------------------------------------------------- wayland surface (proxy)

unsafe extern "C" fn gn_vkCreateWaylandSurfaceKHR(
    instance: VkInstance,
    ci: *const VkWaylandSurfaceCreateInfoKHR,
    _alloc: *const VkAllocationCallbacks,
    out: *mut VkSurfaceKHR,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gi = unsafe { find_instance(instance) };
    if gi.is_null() {
        crate::logf!(gndrm_obf::obf!(
            "proxy surface create FAILED: instance {instance:?} not one of ours"
        ));
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    if !wl_setup(ci.display.cast()) {
        crate::logf!(gndrm_obf::obf!(
            "proxy surface create FAILED: wayland setup failed"
        ));
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let s = Box::new(GnSurface {
        inst: gi,
        dpy: ci.display.cast(),
        parent: ci.surface.cast(),
        own_surface: ptr::null_mut(),
        swapchain_obj: ptr::null_mut(),
        x11: ptr::null_mut(),
        window: 0,
        next: ptr::null_mut(),
    });
    let s = Box::into_raw(s);
    {
        let mut g = G.lock().unwrap();
        unsafe {
            (*s).next = g.surfaces;
            g.surfaces = s;
        }
    }
    unsafe {
        *out = s as usize as VkSurfaceKHR;
        crate::logf!(gndrm_obf::obf!(
            "proxy surface {:?} (direct attach on wl_surface {:?})",
            s,
            ci.surface
        ));
    }
    VK_SUCCESS
}

// ------------------------------------------------- xlib surface (proxy, X11 mode)
//
// Under winex11.drv wine has no wl_surface for the window. The shim opens its
// own X connection to the Xwayland server (see `crate::x11`) and presents each
// rendered slot straight into the window's X pixmap with DRI3 + Present: the
// compositor's own Xwayland IS the client, so the flip reaches the compositor
// without any gamescope_swapchain override.
//
// This is the only WSI provider in the stack: the Vulkan driver (turnip)
// exports no xlib or wayland WSI entry points at all, so handing an X11
// surface to it cannot work (its ICD carries no vkCreateXlibSurfaceKHR, and
// surface setup then fails VK_ERROR_INITIALIZATION_FAILED). The shim must
// present every window itself, so there is no driver-WSI fallback route.

/// `GN_VKSHIM_GUEST_SURFACE` — the present-route capability switch.
///
/// This shim's DEFAULT (`0`, and every value other than exactly `1`) is the
/// override/Wayland route (`xlib_override_surface`): the guest's Vulkan frames
/// are presented to gamescope's override/Wayland surface, ignoring the guest's
/// window content — Vulkan-only, the cheapest, and independent of Xwayland's
/// DRI3. It is the default because it is the route that keeps presenting here:
/// the splash-clear detection rides the tree walk that runs on BOTH routes (see
/// `maybe_report_game_window`), so the frame path is chosen on its own merits —
/// and the guest-surface route's commit stream stalls when the guest's Vulkan
/// swapchain comes up (see `docs/14-guest-surface-commit-stall.md`).
///
/// Setting `GN_VKSHIM_GUEST_SURFACE=1` opts in to the guest-surface route: the
/// DRI3 1.2 + Present route into wine's own X window, which carries wine's
/// GDI/2D content drawn outside the guest's Vulkan surface as well as the
/// guest's frames. GDI/2D content is the reason to take it.
///
/// This is a deliberate, documented capability switch — not scaffolding. It
/// intentionally deviates from the project's usual "no knobs" rule by the
/// user's decision: `=1` trades the cheaper, Xwayland-independent frame path
/// away for GDI/2D support and the real window.
fn use_guest_surface() -> bool {
    // The DEFAULT is 0 — the override/Wayland route (gamescope's own override
    // surface). Stated as an explicit constant (not implied by the comparison)
    // so the default is a decision in the code: unset, empty, or any value
    // other than exactly "1" is 0, and only the exact value "1" departs from it.
    const DEFAULT_GUEST_SURFACE: u8 = 0;
    let selected = match std::env::var("GN_VKSHIM_GUEST_SURFACE").as_deref() {
        Ok("1") => 1,
        _ => DEFAULT_GUEST_SURFACE,
    };
    selected == 1
}

/// The override/Wayland present route — the DEFAULT (`GN_VKSHIM_GUEST_SURFACE`
/// unset, or any value other than exactly `1`; see `use_guest_surface`).
/// Creates the shim's OWN wl_surface, wraps it in a
/// gamescope_swapchain, and binds it to the guest's X window via
/// override_window_content; the compositor then routes this surface's commits
/// as that window's content. Presenting therefore shows the guest's frames
/// directly — wine's own GDI/2D window content is NOT composited (the known
/// and accepted "chooses, never combines" consequence of this route). That
/// GDI/2D gap is exactly what the guest-surface route (the opt-in) buys.
///
/// Recovered from `cb89a9f^` (the commit whose DRI3 route deleted it); adapted
/// to the current shim (the `x11` field on `GnSurface`, the worker's existing
/// `parent.is_null()` dispatch, and `gn_vkDestroySurfaceKHR`'s proxy teardown).
/// Its only caller is the default (non-`use_guest_surface`) branch of
/// `gn_vkCreateXlibSurfaceKHR`.
unsafe fn xlib_override_surface(
    gi: *mut GnInstance,
    window: u32,
    out: *mut VkSurfaceKHR,
) -> VkResult {
    // The window gamescope will actually paint. gamescope resolves whatever
    // XID the override names to its root-child ancestor, so binding the window
    // winex11 named lands a 1x1 override-redirect wine helper for Dishonored;
    // `resolve_override_target` swaps a degenerate ancestor for the real,
    // painted window and leaves a legitimate one untouched (see x11.rs).
    let bound_window = unsafe { crate::x11::resolve_override_target(window) };
    crate::logf!(gndrm_obf::obf!(
        "override: XID {window:#x} -> gamescope override surface on {bound_window:#x} (default route; guest GDI/2D not composited)"
    ));
    if !wl_setup(ptr::null_mut()) {
        crate::logf!(gndrm_obf::obf!(
            "proxy xlib surface create FAILED: wayland setup failed"
        ));
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let (comp, factory, dpy) = {
        let w = G_WL.lock().unwrap();
        (w.comp, w.swapchain_factory, w.dpy)
    };
    if factory.is_null() {
        crate::logf!(gndrm_obf::obf!(
            "compositor misses gamescope_swapchain_factory_v2 — cannot bind XID"
        ));
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe {
        let _sg = SEND_LOCK.lock().unwrap();
        let surf = wlproto::compositor_create_surface(comp);
        if surf.is_null() {
            crate::logf!(gndrm_obf::obf!(
                "proxy xlib surface create FAILED: wl_surface create failed"
            ));
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        let scobj = wlproto::swapchain_factory_create_swapchain(factory, surf);
        // Single xwayland server (--xwayland-count 1) is always id 0. Bind to
        // the window gamescope paints, not the one wine happened to name.
        wlproto::swapchain_override_window_content(scobj, 0, bound_window);
        wlproto::wl_display_flush(dpy);
        drop(_sg);
        let s = Box::new(GnSurface {
            inst: gi,
            dpy,
            parent: surf,
            own_surface: surf,
            swapchain_obj: scobj,
            x11: ptr::null_mut(),
            // The size gamescope must see this layer at is the window's, not
            // the guest's requested swapchain extent (see the note in caps).
            window: bound_window,
            next: ptr::null_mut(),
        });
        let s = Box::into_raw(s);
        {
            let mut g = G.lock().unwrap();
            (*s).next = g.surfaces;
            g.surfaces = s;
        }
        *out = s as usize as VkSurfaceKHR;
        crate::logf!(gndrm_obf::obf!(
            "override: proxy surface {:?} (own wl_surface {:?} bound to XID {:#x}; Vulkan-only, guest GDI/2D not composited)",
            s,
            surf,
            window
        ));
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkCreateXlibSurfaceKHR(
    instance: VkInstance,
    ci: *const VkXlibSurfaceCreateInfoKHR,
    _alloc: *const VkAllocationCallbacks,
    out: *mut VkSurfaceKHR,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gi = unsafe { find_instance(instance) };
    if gi.is_null() {
        crate::logf!(gndrm_obf::obf!(
            "proxy xlib surface create FAILED: instance {instance:?} not one of ours"
        ));
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe { x11_window_surface(gi, ci.window as u32, out) }
}

/// The X window id is the same number under Xlib and xcb, and both routes open
/// their own X connection, so the caller's Display / xcb_connection_t is unused.
unsafe extern "C" fn gn_vkCreateXcbSurfaceKHR(
    instance: VkInstance,
    ci: *const VkXcbSurfaceCreateInfoKHR,
    _alloc: *const VkAllocationCallbacks,
    out: *mut VkSurfaceKHR,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gi = unsafe { find_instance(instance) };
    if gi.is_null() {
        crate::logf!(gndrm_obf::obf!(
            "proxy xcb surface create FAILED: instance {instance:?} not one of ours"
        ));
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe { x11_window_surface(gi, ci.window, out) }
}

unsafe fn x11_window_surface(gi: *mut GnInstance, window: u32, out: *mut VkSurfaceKHR) -> VkResult {
    // Present-route capability switch (`GN_VKSHIM_GUEST_SURFACE`, see
    // `use_guest_surface`). The DEFAULT (unset, or anything but exactly "1") is
    // the gamescope override/Wayland surface, taken here; `=1` opts in to the
    // DRI3-into-wine's-X-window route below, which carries wine's GDI/2D and
    // hands the shim the real window.
    if !use_guest_surface() {
        return unsafe { xlib_override_surface(gi, window, out) };
    }
    let x11 = if window == 0 {
        crate::logf!(gndrm_obf::obf!(
            "x11 route: XID {:#x} unavailable ({})",
            window,
            crate::x11::RouteUnavailable::NullXid.reason()
        ));
        ptr::null_mut()
    } else {
        match unsafe { crate::x11::X11Present::new(window) } {
            Ok(p) => Box::into_raw(Box::new(p)),
            Err(e) => {
                crate::logf!(gndrm_obf::obf!(
                    "x11 route: XID {:#x} unavailable ({})",
                    window,
                    e.reason()
                ));
                ptr::null_mut()
            }
        }
    };
    let s = Box::new(GnSurface {
        inst: gi,
        dpy: ptr::null_mut(),
        parent: ptr::null_mut(),
        own_surface: ptr::null_mut(),
        swapchain_obj: ptr::null_mut(),
        x11,
        window,
        next: ptr::null_mut(),
    });
    let s = Box::into_raw(s);
    {
        let mut g = G.lock().unwrap();
        unsafe {
            (*s).next = g.surfaces;
            g.surfaces = s;
        }
    }
    unsafe {
        *out = s as usize as VkSurfaceKHR;
        crate::logf!(gndrm_obf::obf!(
            "proxy surface {:?} (X11 window {:#x}, DRI3+Present route)",
            s,
            window
        ));
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceXlibPresentationSupportKHR(
    _pd: VkPhysicalDevice,
    _qf: u32,
    _dpy: *mut c_void,
    _visual: usize,
) -> VkBool32 {
    VK_TRUE
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceXcbPresentationSupportKHR(
    _pd: VkPhysicalDevice,
    _qf: u32,
    _connection: *mut c_void,
    _visual: u32,
) -> VkBool32 {
    VK_TRUE
}

unsafe extern "C" fn gn_vkDestroySurfaceKHR(
    _instance: VkInstance,
    surface: VkSurfaceKHR,
    _alloc: *const VkAllocationCallbacks,
) {
    let s = unsafe { find_surface(surface) };
    if s.is_null() {
        return;
    }
    {
        let mut g = G.lock().unwrap();
        let mut pp: *mut *mut GnSurface = &raw mut g.surfaces;
        unsafe {
            while !(*pp).is_null() && *pp != s {
                pp = &raw mut (**pp).next;
            }
            if !(*pp).is_null() {
                *pp = (*s).next;
            }
        }
    }
    unsafe {
        if !(*s).x11.is_null() {
            drop(Box::from_raw((*s).x11));
        }
        // The override route (the default) owns a wl_surface +
        // gamescope_swapchain. Both are null on the guest-surface DRI3
        // route (the opt-in), so this teardown is inert there.
        if !(*s).swapchain_obj.is_null() || !(*s).own_surface.is_null() {
            let _sg = SEND_LOCK.lock().unwrap();
            if !(*s).swapchain_obj.is_null() {
                wlproto::wl_proxy_destroy((*s).swapchain_obj);
            }
            if !(*s).own_surface.is_null() {
                wlproto::wl_proxy_destroy((*s).own_surface);
            }
            wlproto::wl_display_flush((*s).dpy);
            drop(_sg);
        }
        drop(Box::from_raw(s));
    }
}

// Surface queries are answered locally: the proxy owns the wayland
// presentation contract, and the hooks' surface-query trampolines expect
// loader-wrapped surface objects their VK_EXT_headless_surface path never
// produces (the ICD-level mesa surface's first word is its object-type
// enum, not a dispatch table).
// The advertised surface capabilities (the first query's answer), kept so the
// swapchain slot images the shim creates can be compared against the dimensions
// the guest was told. One line, once, at the first swapchain creation.
#[derive(Clone, Copy)]
struct AdvertisedCaps {
    current: (u32, u32),
    min: (u32, u32),
    max: (u32, u32),
}
static ADVERTISED_CAPS: Mutex<Option<AdvertisedCaps>> = Mutex::new(None);
static CAPS_COMPARE_LOGGED: AtomicBool = AtomicBool::new(false);

/// `vkGetPhysicalDeviceSurfaceCapabilities2KHR`: the `2` form of the query, which
/// D3D12 (VKD3D-Proton) uses where D3D11 (DXVK) uses the plain one.
///
/// It must not fall through to the driver. The `surface` a caller passes here is a
/// shim wrapper, so the driver's own entry point would dereference a handle it
/// never issued — that fault surfaces as an access violation inside the Unix call,
/// which is how a D3D12 title died at surface-capability negotiation. The values
/// come from the same source as the plain KHR query: the proxy owns the wayland
/// presentation contract, so there is no delegate surface to forward from. Only
/// `surfaceCapabilities` is written; any further structure a caller chains is left
/// untouched, which is the contract for structures an implementation does not
/// support.
unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfaceCapabilities2KHR(
    pd: VkPhysicalDevice,
    info: *const VkPhysicalDeviceSurfaceInfo2KHR,
    caps: *mut VkSurfaceCapabilities2KHR,
) -> VkResult {
    HB_CAPS2.fetch_add(1, Ordering::Relaxed);
    wsi_hb();
    if info.is_null() || caps.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: `info` is the caller's in-structure and `caps` its out-structure;
    // the KHR handler writes exactly the field handed to it, and both pointers
    // are checked non-null above.
    unsafe {
        gn_vkGetPhysicalDeviceSurfaceCapabilitiesKHR(
            pd,
            (*info).surface,
            &mut (*caps).surfaceCapabilities,
        )
    }
}

/// The guest's own window size for a surface, or `None` when the surface has
/// no X window (the wayland route) or the window cannot be read.
unsafe fn surface_extent(s: *mut GnSurface) -> Option<(u32, u32)> {
    let w = unsafe { (*s).window };
    if w == 0 {
        return None;
    }
    unsafe { crate::x11::window_extent(w) }
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfaceCapabilitiesKHR(
    _pd: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    caps: *mut VkSurfaceCapabilitiesKHR,
) -> VkResult {
    HB_CAPS.fetch_add(1, Ordering::Relaxed);
    wsi_hb();
    let s = unsafe { find_surface(surface) };
    if s.is_null() {
        crate::logf!(gndrm_obf::obf!(
            "proxy surface caps query: {surface:#x} not one of ours"
        ));
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    // The guest's own window size is what gamescope must see the presented
    // layer at: advertising it as `currentExtent` makes the guest size its
    // swapchain to the container's resolution, so a guest desktop smaller than
    // the output reaches the compositor as a smaller layer and is scaled with
    // the selected filter, instead of a swapchain at the output size that
    // gamescope scans out 1:1. `u32::MAX` ("app picks") is kept only for
    // surfaces with no guest window (the wayland route).
    let window_extent = unsafe { surface_extent(s) };
    let out = VkSurfaceCapabilitiesKHR {
        minImageCount: 2,
        maxImageCount: max_swapchain_images(),
        currentExtent: match window_extent {
            Some((w, h)) => VkExtent2D {
                width: w,
                height: h,
            },
            None => VkExtent2D {
                width: u32::MAX,
                height: u32::MAX,
            },
        },
        minImageExtent: VkExtent2D {
            width: 1,
            height: 1,
        },
        maxImageExtent: VkExtent2D {
            width: 8192,
            height: 8192,
        },
        maxImageArrayLayers: 1,
        supportedTransforms: VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
        currentTransform: VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
        // The same set Mesa/turnip's WSI advertises on the X11 arm. There is
        // no delegate surface to forward from: the proxy owns the wayland
        // presentation contract and the driver exports no WSI entry points
        // (see the xlib-surface and present-modes notes above), and
        // `RealDevFns` carries no surface-capability query. Advertising
        // OPAQUE alone let DXVK force every present opaque, which covered
        // the window's own GDI surface. INHERIT is the honest default for a
        // compositor-managed surface; the pre/post forms let a guest pick
        // its own blending.
        supportedCompositeAlpha: VK_COMPOSITE_ALPHA_INHERIT_BIT_KHR
            | VK_COMPOSITE_ALPHA_PRE_MULTIPLIED_BIT_KHR
            | VK_COMPOSITE_ALPHA_POST_MULTIPLIED_BIT_KHR
            | VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
        // Exactly what the pool's AHardwareBuffer backs, no more. The pool
        // allocates GPU_SAMPLED_IMAGE | GPU_COLOR_OUTPUT | COMPOSER_OVERLAY
        // | CPU_READ_OFTEN and the guest renders straight into it, so
        // SAMPLED_BIT comes with the first flag and COLOR_ATTACHMENT_BIT
        // with the second. The transfer bits are on every image the proxy
        // creates (`GnSwapchain::usage`) and the WSI smoke client actually
        // exercises them: its present readback copies a swapchain image to
        // host memory (TRANSFER_SRC) and its clear scene clears one in
        // TRANSFER_DST_OPTIMAL (TRANSFER_DST). COMPOSER_OVERLAY and
        // CPU_READ_OFTEN are SurfaceFlinger/host usages with no Vulkan
        // equivalent. No STORAGE_BIT: an AHardwareBuffer grants that only
        // through AHARDWAREBUFFER_USAGE_GPU_DATA_BUFFER, which this pool
        // does not set, so advertising it would promise a capability the
        // scan-out buffer cannot back.
        supportedUsageFlags: VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT
            | VK_IMAGE_USAGE_TRANSFER_SRC_BIT
            | VK_IMAGE_USAGE_TRANSFER_DST_BIT
            | VK_IMAGE_USAGE_SAMPLED_BIT,
    };
    if let Ok(mut a) = ADVERTISED_CAPS.lock() {
        if a.is_none() {
            *a = Some(AdvertisedCaps {
                current: (out.currentExtent.width, out.currentExtent.height),
                min: (out.minImageExtent.width, out.minImageExtent.height),
                max: (out.maxImageExtent.width, out.maxImageExtent.height),
            });
        }
    }
    unsafe { *caps = out };
    VK_SUCCESS
}

/// The surface formats the proxy serves: `B8G8R8A8_UNORM` in the scan-out
/// pool's own layout, plus `R8G8B8A8_UNORM` only under the RGBA opt-in (see
/// `advertises_rgba`). Shared by the plain and `2` query forms so both forms
/// advertise exactly the same set.
fn served_surface_formats() -> &'static [VkSurfaceFormatKHR] {
    // The HDR set contains exactly what the create path can present: the
    // scan-out pool's BGRA layout and the 10-bit layout, each with both colour
    // spaces. Two rules fix that shape.
    //
    // Every format is paired with both colours because vkd3d's
    // `dxgi_vk_swap_chain_select_format` demands an entry whose format AND
    // colour space match the request, and its only wildcard fallback exists on
    // the sRGB path: a title that switches to PQ while keeping an 8-bit
    // backbuffer stops creating swapchains entirely without the
    // `(B8G8R8A8_UNORM, HDR10_ST2084)` pair. Order matters to a client that
    // takes the first entry carrying a colour space (DXVK): the 10-bit pair
    // comes first, because 8-bit PQ shreds the tone range.
    //
    // Nothing else may appear. An entry the create path refuses is worse than a
    // missing one — the client takes the offer, the create fails, and it
    // retries forever: measured, advertising `R8G8B8A8_UNORM` without the RGBA
    // opt-in made DXVK loop on an in-game HDR toggle, and advertising FP16 made
    // it take `fmt=97 cs=1000104008` and loop against a refusal, because the
    // scan-out pool cannot serve 16 bits.
    const K_HDR: [VkSurfaceFormatKHR; 9] = [
        VkSurfaceFormatKHR {
            format: VK_FORMAT_A2B10G10R10_UNORM_PACK32,
            colorSpace: VK_COLOR_SPACE_HDR10_ST2084_EXT,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_A2R10G10B10_UNORM_PACK32,
            colorSpace: VK_COLOR_SPACE_HDR10_ST2084_EXT,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_B8G8R8A8_UNORM,
            colorSpace: VK_COLOR_SPACE_HDR10_ST2084_EXT,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_A2B10G10R10_UNORM_PACK32,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_A2R10G10B10_UNORM_PACK32,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_B8G8R8A8_UNORM,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_A2B10G10R10_UNORM_PACK32,
            colorSpace: VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_A2R10G10B10_UNORM_PACK32,
            colorSpace: VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_B8G8R8A8_UNORM,
            colorSpace: VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT,
        },
    ];

    // The RGBA opt-in is the only case where the create path serves
    // `R8G8B8A8_UNORM`, so it is the only case that may advertise it.
    const K_HDR_RGBA: [VkSurfaceFormatKHR; 8] = [
        K_HDR[0],
        K_HDR[1],
        K_HDR[2],
        K_HDR[3],
        K_HDR[4],
        K_HDR[5],
        VkSurfaceFormatKHR {
            format: VK_FORMAT_R8G8B8A8_UNORM,
            colorSpace: VK_COLOR_SPACE_HDR10_ST2084_EXT,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_R8G8B8A8_UNORM,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
    ];
    if hdr_session() {
        return if advertises_rgba() {
            &K_HDR_RGBA
        } else {
            &K_HDR
        };
    }
    const K_FMTS: [VkSurfaceFormatKHR; 2] = [
        VkSurfaceFormatKHR {
            format: VK_FORMAT_B8G8R8A8_UNORM,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_R8G8B8A8_UNORM,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
    ];
    // BGRA_8888 only by default, so the guest renders in the scan-out pool's
    // own layout (see `advertises_rgba` — offering RGBA let Nidhogg render
    // R/B-exchanged into a daemon BGRA buffer, which reached the panel as
    // cyan). RGBA is offered only under `GN_VKSHIM_ALLOW_RGBA`.
    if advertises_rgba() {
        &K_FMTS
    } else {
        &K_FMTS[..1]
    }
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfaceFormatsKHR(
    _pd: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    count: *mut u32,
    formats: *mut VkSurfaceFormatKHR,
) -> VkResult {
    HB_FMT.fetch_add(1, Ordering::Relaxed);
    wsi_hb();
    if unsafe { find_surface(surface) }.is_null() {
        crate::logf!(gndrm_obf::obf!(
            "proxy surface formats query: {surface:#x} not one of ours"
        ));
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    let served = served_surface_formats();
    let n_formats = served.len();
    unsafe {
        if formats.is_null() {
            *count = n_formats as u32;
            return VK_SUCCESS;
        }
        let n = (*count).min(n_formats as u32) as usize;
        ptr::copy_nonoverlapping(served.as_ptr(), formats, n);
        *count = n as u32;
        if n < n_formats {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        }
    }
}

// All four MESA_VK_WSI_PRESENT_MODE names are served; fifo_relaxed sits next to
// fifo as its sibling, so the delivered order of the other three is unchanged.
const SERVED_PRESENT_MODES: [VkPresentModeKHR; 4] = [
    VK_PRESENT_MODE_IMMEDIATE_KHR,
    VK_PRESENT_MODE_MAILBOX_KHR,
    VK_PRESENT_MODE_FIFO_KHR,
    VK_PRESENT_MODE_FIFO_RELAXED_KHR,
];

fn parse_present_mode_override(val: Option<&str>) -> Option<VkPresentModeKHR> {
    let mode = match val? {
        "fifo" => VK_PRESENT_MODE_FIFO_KHR,
        "relaxed" => VK_PRESENT_MODE_FIFO_RELAXED_KHR,
        "mailbox" => VK_PRESENT_MODE_MAILBOX_KHR,
        "immediate" => VK_PRESENT_MODE_IMMEDIATE_KHR,
        _ => {
            crate::logf!(gndrm_obf::obf!("Invalid MESA_VK_WSI_PRESENT_MODE value!"));
            return None;
        }
    };
    Some(mode)
}

/// The present mode the shim honours for a swapchain: the
/// MESA_VK_WSI_PRESENT_MODE override wins over the app's requested
/// `presentMode`, falling back to fifo (mesa wsi_common.c:1521).
fn effective_present_mode(ci: &VkSwapchainCreateInfoKHR) -> VkPresentModeKHR {
    if let Some(m) =
        parse_present_mode_override(std::env::var("MESA_VK_WSI_PRESENT_MODE").ok().as_deref())
    {
        return m;
    }
    match ci.presentMode {
        VK_PRESENT_MODE_IMMEDIATE_KHR
        | VK_PRESENT_MODE_MAILBOX_KHR
        | VK_PRESENT_MODE_FIFO_KHR
        | VK_PRESENT_MODE_FIFO_RELAXED_KHR => ci.presentMode,
        _ => VK_PRESENT_MODE_FIFO_KHR,
    }
}

/// The wire value for `gamescope_swapchain.set_present_mode` that declares
/// `mode` — the mode `present_dispatch` honours — to the compositor.
///
/// gamescope's protocol arg is declared as its own type in
/// `protocol/gamescope-swapchain.xml` (arg name `vk_present_mode`,
/// `summary="VkPresentModeKHR"`), and its handler stores it verbatim as a
/// `VkPresentModeKHR` (`src/wlserver.cpp:1121`:
/// `wl_info->oCurrentPresentMode = VkPresentModeKHR( present_mode );`), then
/// compares it against the Vulkan enumerators in `wlserver_surface_is_fifo()`
/// (`src/wlserver.cpp:3418`, `== VK_PRESENT_MODE_FIFO_KHR`) and
/// `wlserver_surface_is_async()` (`:3401`, `== VK_PRESENT_MODE_IMMEDIATE_KHR
/// || == VK_PRESENT_MODE_MAILBOX_KHR`). So the wire enum IS
/// `VkPresentModeKHR` — immediate=0, mailbox=1, fifo=2, fifo_relaxed=3, the
/// same values `crate::vk` uses — and the map is value-for-value. It is
/// written out explicitly (not `as u32`) so that a future divergence between
/// the two enums is a compile-time edit here rather than a silent mis-pacing
/// on every frame.
fn gamescope_present_mode(mode: VkPresentModeKHR) -> u32 {
    match mode {
        VK_PRESENT_MODE_IMMEDIATE_KHR => 0,
        VK_PRESENT_MODE_MAILBOX_KHR => 1,
        VK_PRESENT_MODE_FIFO_KHR => 2,
        VK_PRESENT_MODE_FIFO_RELAXED_KHR => 3,
        // Unreachable: `(*sc).present_mode` is `effective_present_mode`'s
        // output, which is one of the four served modes above.
        _ => 2,
    }
}

fn offered_present_modes(
    served: &[VkPresentModeKHR],
    override_mode: Option<VkPresentModeKHR>,
) -> Vec<VkPresentModeKHR> {
    let mut v = Vec::with_capacity(served.len() + 1);
    if let Some(m) = override_mode {
        if served.contains(&m) {
            v.push(m);
        } else {
            crate::logf!(gndrm_obf::obf!(
                "Unsupported MESA_VK_WSI_PRESENT_MODE value!"
            ));
        }
    }
    for &m in served {
        if !v.contains(&m) {
            v.push(m);
        }
    }
    if !v.contains(&VK_PRESENT_MODE_FIFO_KHR) {
        v.push(VK_PRESENT_MODE_FIFO_KHR);
    }
    v
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfacePresentModesKHR(
    _pd: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    count: *mut u32,
    modes: *mut VkPresentModeKHR,
) -> VkResult {
    HB_PMODES.fetch_add(1, Ordering::Relaxed);
    wsi_hb();
    if unsafe { find_surface(surface) }.is_null() {
        crate::logf!(gndrm_obf::obf!(
            "proxy surface present-modes query: {surface:#x} not one of ours"
        ));
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    let modes_list = offered_present_modes(
        &SERVED_PRESENT_MODES,
        parse_present_mode_override(std::env::var("MESA_VK_WSI_PRESENT_MODE").ok().as_deref()),
    );
    unsafe {
        if modes.is_null() {
            *count = modes_list.len() as u32;
            return VK_SUCCESS;
        }
        let n = (*count).min(modes_list.len() as u32) as usize;
        ptr::copy_nonoverlapping(modes_list.as_ptr(), modes, n);
        *count = n as u32;
        if n < modes_list.len() {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        }
    }
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfaceSupportKHR(
    _pd: VkPhysicalDevice,
    _qf: u32,
    _surface: VkSurfaceKHR,
    supported: *mut VkBool32,
) -> VkResult {
    unsafe { *supported = VK_TRUE };
    VK_SUCCESS
}

// ------------------------------------------------- WSI dispatch-coverage set
//
// Every entry point here takes a shim-owned wrapper handle (a surface or a
// swapchain) in a position other than argument 0, so the wrapper-swapping thunk
// in `serve_unlisted` cannot serve it: it swaps only argument 0 and hands the
// driver a handle it never issued. Without an entry the driver dereferences the
// shim's surface/swapchain and the call faults with 0xc0000005 (the class the
// `vkGetPhysicalDeviceSurfaceCapabilities2KHR` bug and `vkSetHdrMetadataEXT`
// were both instances of). Each handler validates the wrapper is one the proxy
// owns and answers from the proxy's own presentation contract; there is no
// driver surface or swapchain to forward to, because the proxy creates neither.

/// `VK_DEVICE_GROUP_PRESENT_MODE_LOCAL_BIT_KHR`: the only device-group present
/// mode a single-device implementation offers, and the honest answer here.
const VK_DEVICE_GROUP_PRESENT_MODE_LOCAL_BIT_KHR: u32 = 4;

/// `vkSetHdrMetadataEXT` (VK_EXT_hdr_metadata): the swapchain-count array form.
/// A successful no-op — the compositor owns the output's HDR state, so there is
/// nothing to forward to; forwarding the shim swapchains to the driver is
/// exactly the fault. Each swapchain is validated so a foreign handle is not
/// silently accepted.
unsafe extern "C" fn gn_vkSetHdrMetadataEXT(
    _device: VkDevice,
    swapchain_count: u32,
    swapchains: *const VkSwapchainKHR,
    _metadata: *const c_void,
) -> VkResult {
    if swapchain_count != 0 && swapchains.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    for i in 0..swapchain_count as usize {
        let sc = unsafe { *swapchains.add(i) };
        if unsafe { find_swapchain(sc) }.is_null() {
            crate::logf!(gndrm_obf::obf!(
                "proxy setHdrMetadata: swapchain {sc:#x} not one of ours"
            ));
            return VK_ERROR_INITIALIZATION_FAILED;
        }
    }
    VK_SUCCESS
}

/// `vkSetLocalDimmingAMD`: a display hint with no driver swapchain to carry it.
unsafe extern "C" fn gn_vkSetLocalDimmingAMD(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    _enable: VkBool32,
) -> VkResult {
    if unsafe { find_swapchain(swapchain) }.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    VK_SUCCESS
}

/// `vkGetSwapchainStatusKHR`: the proxy's swapchain is always healthy.
unsafe extern "C" fn gn_vkGetSwapchainStatusKHR(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
) -> VkResult {
    if unsafe { find_swapchain(swapchain) }.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    VK_SUCCESS
}

/// `vkGetSwapchainCounterEXT` (VK_EXT_display_surface_counter): the proxy
/// exposes no surface counters, so the value is 0.
unsafe extern "C" fn gn_vkGetSwapchainCounterEXT(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    _counter: u32,
    value: *mut u64,
) -> VkResult {
    if value.is_null() || unsafe { find_swapchain(swapchain) }.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe { *value = 0 };
    VK_SUCCESS
}

/// `vkGetDeviceGroupSurfacePresentModesKHR` (VK_KHR_device_group): a single
/// device offers only its own local present mode.
unsafe extern "C" fn gn_vkGetDeviceGroupSurfacePresentModesKHR(
    _device: VkDevice,
    surface: VkSurfaceKHR,
    modes: *mut u32,
) -> VkResult {
    if modes.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    if unsafe { find_surface(surface) }.is_null() {
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    unsafe { *modes = VK_DEVICE_GROUP_PRESENT_MODE_LOCAL_BIT_KHR };
    VK_SUCCESS
}

/// `vkAcquireFullScreenExclusiveModeEXT` (VK_EXT_full_screen_exclusive): the
/// compositor owns the display, so exclusive mode is always granted.
unsafe extern "C" fn gn_vkAcquireFullScreenExclusiveModeEXT(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
) -> VkResult {
    if unsafe { find_swapchain(swapchain) }.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    VK_SUCCESS
}

/// `vkReleaseFullScreenExclusiveModeEXT`: the symmetric counterpart.
unsafe extern "C" fn gn_vkReleaseFullScreenExclusiveModeEXT(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
) -> VkResult {
    if unsafe { find_swapchain(swapchain) }.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    VK_SUCCESS
}

/// `vkGetPhysicalDeviceSurfaceFormats2KHR`: the `2` form of the formats query
/// (VKD3D-Proton's form). Forwards onto the plain set so both advertise the same
/// formats; only each element's `surfaceFormat` is written.
unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfaceFormats2KHR(
    _pd: VkPhysicalDevice,
    info: *const VkPhysicalDeviceSurfaceInfo2KHR,
    count: *mut u32,
    formats: *mut VkSurfaceFormat2KHR,
) -> VkResult {
    HB_FMT2.fetch_add(1, Ordering::Relaxed);
    wsi_hb();
    if info.is_null() || count.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let surface = unsafe { (*info).surface };
    if unsafe { find_surface(surface) }.is_null() {
        crate::logf!(gndrm_obf::obf!(
            "proxy surface formats2 query: {surface:#x} not one of ours"
        ));
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    let served = served_surface_formats();
    unsafe {
        if formats.is_null() {
            *count = served.len() as u32;
            return VK_SUCCESS;
        }
        let n = (*count).min(served.len() as u32) as usize;
        for (i, fmt) in served.iter().enumerate().take(n) {
            (*formats.add(i)).surfaceFormat = *fmt;
        }
        *count = n as u32;
        if n < served.len() {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        }
    }
}

/// `vkGetPhysicalDeviceSurfacePresentModes2EXT`: same out layout as the plain
/// form, so it forwards onto the plain handler.
unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfacePresentModes2EXT(
    pd: VkPhysicalDevice,
    info: *const VkPhysicalDeviceSurfaceInfo2KHR,
    count: *mut u32,
    modes: *mut VkPresentModeKHR,
) -> VkResult {
    if info.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe { gn_vkGetPhysicalDeviceSurfacePresentModesKHR(pd, (*info).surface, count, modes) }
}

/// `vkGetPhysicalDeviceSurfaceCapabilities2EXT`: fills only the shared
/// `surfaceCapabilities` member, leaving `supportedSurfaceCounters` at the
/// caller's zero.
unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfaceCapabilities2EXT(
    pd: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    caps: *mut VkSurfaceCapabilities2EXT,
) -> VkResult {
    if caps.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe {
        gn_vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pd, surface, &mut (*caps).surfaceCapabilities)
    }
}

/// `vkGetPhysicalDevicePresentRectanglesKHR` (VK_KHR_device_group): a single
/// device presents the whole surface, so it advertises no present rectangles.
unsafe extern "C" fn gn_vkGetPhysicalDevicePresentRectanglesKHR(
    _pd: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    count: *mut u32,
    _rects: *mut VkRect2D,
) -> VkResult {
    if count.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    if unsafe { find_surface(surface) }.is_null() {
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    unsafe { *count = 0 };
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceWaylandPresentationSupportKHR(
    _pd: VkPhysicalDevice,
    _qf: u32,
    _dpy: *mut wl_display,
) -> VkBool32 {
    VK_TRUE
}

// ------------------------------------------------------- device functions

/// The driver's own device extension names for `pd`, unfiltered.
unsafe fn driver_device_exts(gi: *mut GnInstance, pd: VkPhysicalDevice) -> Vec<std::ffi::CString> {
    let Some(gipa) = G.lock().unwrap().real_gipa else {
        return Vec::new();
    };
    let real: Option<PFN_vkEnumerateDeviceExtensionProperties> = unsafe {
        std::mem::transmute(gipa((*gi).real, c"vkEnumerateDeviceExtensionProperties".as_ptr()))
    };
    let Some(real) = real else {
        return Vec::new();
    };
    let mut n: u32 = 0;
    if unsafe { real(pd, ptr::null(), &mut n, ptr::null_mut()) } != VK_SUCCESS {
        return Vec::new();
    }
    let mut buf = vec![
        VkExtensionProperties {
            extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
            specVersion: 0,
        };
        n as usize
    ];
    let r = unsafe { real(pd, ptr::null(), &mut n, buf.as_mut_ptr()) };
    if r != VK_SUCCESS && r != VK_INCOMPLETE {
        return Vec::new();
    }
    buf[..n as usize]
        .iter()
        .map(|e| unsafe { CStr::from_ptr(e.extensionName.as_ptr()) }.to_owned())
        .collect()
}

/// Did the guest enable `shaderFloat16` on this device?
///
/// The fp16 shader variant needs the feature enabled on the device its pipelines
/// are built on — which is the guest's device, not one we create. Enabling it
/// ourselves is not an option (the device belongs to the guest), so the fp16
/// variant is used only when the guest already asked for it; otherwise the fp32
/// variant, which needs nothing.
unsafe fn guest_enabled_fp16(ci: &VkDeviceCreateInfo) -> bool {
    const SHADER_FLOAT16_INT8_FEATURES: i32 = 1_000_082_000;
    #[repr(C)]
    struct Float16 {
        s_type: i32,
        next: *const c_void,
        shader_float16: u32,
        int8: u32,
    }
    #[repr(C)]
    struct Chain {
        s_type: i32,
        next: *const Chain,
    }
    let mut p = ci.pNext.cast::<Chain>();
    while !p.is_null() {
        unsafe {
            if (*p).s_type == SHADER_FLOAT16_INT8_FEATURES {
                return (*p.cast::<Float16>()).shader_float16 != 0;
            }
            p = (*p).next;
        }
    }
    false
}

unsafe extern "C" fn gn_vkCreateDevice(
    pd: VkPhysicalDevice,
    ci: *const VkDeviceCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkDevice,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gi = unsafe { find_instance_by_pd(pd) };
    if gi.is_null() || unsafe { (*gi).f.create_device.is_none() } {
        loud("create_device.noinst", || {
            format!("{}", gndrm_obf::obf!(
                "proxy: vkCreateDevice NO INSTANCE pd={:#x} gi={} — guest device will be driver-raw (wrong-handle class)",
                pd as usize, gi.is_null()
            ))
        });
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let strip_robustness2 = ROBUSTNESS2_FAKED.load(Ordering::Relaxed);
    let have = unsafe { driver_device_exts(gi, pd) };
    // An unreadable extension list keeps the dma-buf path, as before.
    let import_ahb = !have.is_empty()
        && import_mode(&|e: &CStr| have.iter().any(|h| h.as_c_str() == e), AHB_IMPORT_ALLOWED.load(Ordering::Relaxed))
            == Some(true);
    if import_ahb && !AHB_MODE_LOGGED.swap(true, Ordering::Relaxed) {
        crate::logf!(gndrm_obf::obf!(
            "proxy device: importing swapchain slots as AHardwareBuffers"
        ));
    }
    let add_exts: &[&CStr] = if import_ahb { &AHB_EXTS } else { &DMA_BUF_EXTS };
    let mut n: usize = 0;
    let names = unsafe {
        slice_or_empty(
            ci.ppEnabledExtensionNames,
            ci.enabledExtensionCount as usize,
        )
    };
    let mut exts: Vec<*const c_char> = vec![ptr::null(); names.len() + 16];
    crate::logf!(gndrm_obf::obf!(
        "proxy device: guest enables {} extensions",
        names.len()
    ));
    for &name in names {
        // The guest must never enable a WSI extension the proxy cannot honour
        // (see `unimplemented_wsi_ext`); drop it before the driver sees it.
        let n_ = unsafe { CStr::from_ptr(name) };
        if crate::placed::faked() && n_ == crate::placed::EXT_NAME {
            crate::placed::note_requested();
        }
        if crate::divisor::faked() && n_ == crate::divisor::EXT_NAME {
            let khr_listed = names
                .iter()
                .any(|&o| unsafe { CStr::from_ptr(o) } == crate::divisor::KHR_NAME);
            if !khr_listed && n < exts.len() {
                exts[n] = crate::divisor::KHR_NAME.as_ptr();
                n += 1;
            }
            continue;
        }
        if unimplemented_wsi_ext(n_)
            || (n_ == c"VK_EXT_hdr_metadata"
                && !have.is_empty()
                && !have.iter().any(|h| h.as_c_str() == n_))
            || (strip_robustness2 && n_ == c"VK_EXT_robustness2")
            || (crate::placed::faked() && n_ == crate::placed::EXT_NAME)
        {
            crate::logf!(gndrm_obf::obf!(
                "proxy device: masking {}",
                n_.to_string_lossy()
            ));
            continue;
        }
        if n < exts.len() {
            exts[n] = name;
            n += 1;
        }
    }
    let placed_exts: &[&CStr] = if crate::placed::enabled() {
        &[
            c"VK_ANDROID_external_memory_android_hardware_buffer",
            c"VK_EXT_queue_family_foreign",
        ]
    } else {
        &[]
    };
    'add: for add in add_exts.iter().chain(placed_exts.iter()).copied() {
        for &e in &exts[..n] {
            if unsafe { CStr::from_ptr(e) } == add {
                continue 'add;
            }
        }
        if !have.is_empty() && !have.iter().any(|h| h.as_c_str() == add) {
            crate::logf!(gndrm_obf::obf!(
                "proxy device: driver lacks {}, not adding it",
                add.to_string_lossy()
            ));
            continue;
        }
        if n < exts.len() {
            exts[n] = add.as_ptr();
            n += 1;
        }
    }
    // Optionally mask the pipeline-library extensions (GN_VKSHIM_NO_GPL=1):
    // DXVK then builds monolithic pipelines instead of partial ones it links,
    // which is the A/B for a suspected bad partial/link path in this driver
    // (the X11 vulkan wrapper fakes VK_KHR_pipeline_library, so X11 never uses
    // it). Off by default: without it DXVK keeps its pipeline-compilation
    // optimisation, and masking it means compiling every state monolithically,
    // which is slow enough on this driver to look like a hang.
    if std::env::var_os("GN_VKSHIM_NO_GPL").is_some() {
        for drop in [
            c"VK_EXT_graphics_pipeline_library",
            c"VK_KHR_pipeline_library",
        ] {
            let mut i = 0;
            while i < n {
                if unsafe { CStr::from_ptr(exts[i]) } == drop {
                    crate::logf!(gndrm_obf::obf!(
                        "proxy device: masking {}",
                        drop.to_string_lossy()
                    ));
                    for j in i..n - 1 {
                        exts[j] = exts[j + 1];
                    }
                    n -= 1;
                } else {
                    i += 1;
                }
            }
        }
    }
    let mut ci2 = VkDeviceCreateInfo { ..*ci };
    ci2.enabledExtensionCount = n as u32;
    ci2.ppEnabledExtensionNames = exts.as_ptr();
    // Confine the loader's chain link to the hooks' namespace (see
    // G_CHAIN_DEV_LINK).
    let mut p = ci2.pNext as *mut VkBaseOutStructure;
    while !p.is_null() {
        unsafe {
            if (*p).sType == VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO {
                let lci = p.cast::<VkLayerDeviceCreateInfo>();
                if (*lci).function == VK_LAYER_LINK_INFO {
                    (*lci).u.pLayerDeviceInfo = &raw const G_CHAIN_DEV_LINK.0 as *mut _;
                }
            }
            p = (*p).pNext;
        }
    }
    let mut strip: Vec<VkStructureType> = Vec::new();
    if strip_robustness2 {
        strip.push(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ROBUSTNESS_2_FEATURES_EXT);
    }
    if crate::placed::faked() {
        strip.push(crate::placed::STYPE_FEATURES);
    }
    let mut unlinked: Vec<(*mut VkBaseOutStructure, *mut VkBaseOutStructure)> = Vec::new();
    let mut prev: *mut VkBaseOutStructure = ptr::null_mut();
    let mut p = ci2.pNext as *mut VkBaseOutStructure;
    while !p.is_null() {
        unsafe {
            let next = (*p).pNext;
            if strip.contains(&(*p).sType) {
                if prev.is_null() {
                    ci2.pNext = next as *const c_void;
                } else {
                    (*prev).pNext = next;
                }
                unlinked.push((prev, p));
            } else {
                prev = p;
            }
            p = next;
        }
    }
    let res = unsafe { (*gi).f.create_device.unwrap()(pd, &ci2, alloc, out) };
    for &(prev, p) in unlinked.iter().rev() {
        if !prev.is_null() {
            unsafe { (*prev).pNext = p };
        }
    }
    if res != VK_SUCCESS {
        return res;
    }
    // Read-only capture for the image-path instrument: the physical device and
    // its memory properties, so a later slot import can name the heap the
    // driver placed the image in. `vkGetPhysicalDeviceMemoryProperties` is a
    // pure query; its result is never consulted by any creation decision.
    let mem_props = unsafe { fetch_mem_props(gi, pd) };
    unsafe { log_mem_props_once(pd, &mem_props) };
    let mut gd = Box::new(GnDevice {
        scratch: [0; GN_HANDLE_SCRATCH],
        real: unsafe { *out },
        inst: gi,
        pd,
        mem_props,
        f: RealDevFns::default(),
        queues: ptr::null_mut(),
        cmds: ptr::null_mut(),
        next: ptr::null_mut(),
        queue_family: 0,
        cmd_pool: 0,
        fp16: false,
        import_ahb,
        lsfg_epoch: 0,
        gen_batches: Vec::new(),
        gen_next: 0,
        gen_sc: ptr::null_mut(),
        gen_due: VecDeque::new(),
        gen_last_deadline: 0,
        gen_last_source_ns: 0,
        gen_period_ns: 0,
        lsfg: ptr::null_mut(),
    });
    // Real-device function resolution never uses the hooks'
    // vkGetDeviceProcAddr: its answers are vulkan::driver::* trampolines
    // that index a high dispatch region which isn't valid for this device
    // (GetDeviceQueue through it returned a NULL queue, crashing the
    // trampoline's own handle-magic check). The hooks' exported vk* entries
    // forward through the device's ordinary low dispatch slots — always
    // consistent with the CreateDevice that produced the handle; the
    // instance GIPA falls through to the real driver's name tables for the
    // extension names the hooks don't export.
    let gdp: *mut GnDevice = &mut *gd;
    macro_rules! resolve {
        ($field:ident, $sym:literal) => {
            gd.f.$field = unsafe {
                std::mem::transmute(gn_real_device_proc(
                    gdp,
                    CStr::from_bytes_with_nul_unchecked(concat!($sym, "\0").as_bytes()),
                ))
            };
        };
    }
    resolve!(get_queue, "vkGetDeviceQueue");
    resolve!(create_image, "vkCreateImage");
    resolve!(create_pipelines, "vkCreateGraphicsPipelines");
    resolve!(get_device_queue2, "vkGetDeviceQueue2");
    resolve!(submit2, "vkQueueSubmit2");
    resolve!(wait_semaphores, "vkWaitSemaphoresKHR");
    resolve!(destroy_image, "vkDestroyImage");
    resolve!(get_mem_reqs, "vkGetImageMemoryRequirements");
    resolve!(alloc_mem, "vkAllocateMemory");
    resolve!(free_mem, "vkFreeMemory");
    resolve!(bind_mem, "vkBindImageMemory");
    resolve!(create_fence, "vkCreateFence");
    resolve!(destroy_fence, "vkDestroyFence");
    resolve!(wait_fences, "vkWaitForFences");
    resolve!(reset_fences, "vkResetFences");
    resolve!(get_fence_status, "vkGetFenceStatus");
    resolve!(create_sem, "vkCreateSemaphore");
    resolve!(destroy_sem, "vkDestroySemaphore");
    resolve!(alloc_cmd, "vkAllocateCommandBuffers");
    resolve!(free_cmd, "vkFreeCommandBuffers");
    resolve!(submit, "vkQueueSubmit");
    resolve!(execute_commands, "vkCmdExecuteCommands");
    resolve!(wait_idle, "vkDeviceWaitIdle");
    resolve!(queue_wait_idle, "vkQueueWaitIdle");
    resolve!(destroy_device, "vkDestroyDevice");
    resolve!(create_cmd_pool, "vkCreateCommandPool");
    resolve!(destroy_cmd_pool, "vkDestroyCommandPool");
    resolve!(map_mem, "vkMapMemory");
    resolve!(unmap_mem, "vkUnmapMemory");
    resolve!(cmd_pipeline_barrier, "vkCmdPipelineBarrier");
    resolve!(cmd_clear_color_image, "vkCmdClearColorImage");
    resolve!(cmd_copy_image, "vkCmdCopyImage");
    resolve!(cmd_copy_image2, "vkCmdCopyImage2");
    resolve!(cmd_blit_image, "vkCmdBlitImage");
    resolve!(cmd_blit_image2, "vkCmdBlitImage2");
    resolve!(cmd_copy_buffer_to_image, "vkCmdCopyBufferToImage");
    resolve!(cmd_copy_buffer_to_image2, "vkCmdCopyBufferToImage2");
    resolve!(cmd_pipeline_barrier2, "vkCmdPipelineBarrier2");
    resolve!(cmd_begin_render_pass, "vkCmdBeginRenderPass");
    resolve!(cmd_begin_render_pass2, "vkCmdBeginRenderPass2");
    resolve!(cmd_begin_rendering, "vkCmdBeginRendering");
    resolve!(create_image_view, "vkCreateImageView");
    resolve!(create_framebuffer, "vkCreateFramebuffer");
    resolve!(cmd_set_viewport, "vkCmdSetViewport");
    resolve!(cmd_set_viewport_with_count, "vkCmdSetViewportWithCount");
    resolve!(cmd_set_scissor, "vkCmdSetScissor");
    resolve!(cmd_set_scissor_with_count, "vkCmdSetScissorWithCount");
    resolve!(create_render_pass, "vkCreateRenderPass");
    resolve!(
        create_sampler_ycbcr_conversion,
        "vkCreateSamplerYcbcrConversion"
    );
    let qf = if ci.pQueueCreateInfos.is_null() {
        0
    } else {
        unsafe { (*ci.pQueueCreateInfos).queueFamilyIndex }
    };
    gd.queue_family = qf;
    // A pool of our own: the guest's pools belong to the guest and must not be
    // used for submissions it did not make.
    if let Some(create_pool) = gd.f.create_cmd_pool {
        let mut pci: VkCommandPoolCreateInfo = unsafe { std::mem::zeroed() };
        pci.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO;
        pci.queueFamilyIndex = qf;
        let mut pool: VkCommandPool = 0;
        if unsafe { create_pool(gd.real, &pci, ptr::null(), &mut pool) } == VK_SUCCESS {
            gd.cmd_pool = pool;
        }
    }
    let gd = Box::into_raw(gd);
    // Frame generation is opened on the first present that has settings, not
    // here: the app pushes them through the daemon after this device exists, so
    // an attempt at create time would never see them.
    unsafe { (*gd).fp16 = guest_enabled_fp16(ci) };
    {
        let mut g = G.lock().unwrap();
        unsafe {
            (*gd).next = g.devices;
            g.devices = gd;
        }
    }
    // Hand the loader our own wrapper; gd.real stays private (its first
    // word must keep pointing at the hooks' dispatch table).
    unsafe {
        *out = gd.cast::<c_void>();
        crate::logf!(gndrm_obf::obf!(
            "proxy device {:?} (real {:?}, queue family {qf})",
            gd,
            (*gd).real
        ));
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkDestroyDevice(device: VkDevice, _alloc: *const VkAllocationCallbacks) {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return;
    }
    unsafe {
        if !(*gd).lsfg.is_null() {
            // The engine's context owns pipelines and images on this device, so
            // it is torn down before the device itself.
            (*(*gd).lsfg).close((*gd).real, &(*gd).f);
            drop(Box::from_raw((*gd).lsfg));
            (*gd).lsfg = ptr::null_mut();
        }
        if let Some(wait_idle) = (*gd).f.wait_idle {
            wait_idle((*gd).real);
        }
        for batch in (*gd).gen_batches.drain(..) {
            if batch.fence != 0 {
                if let Some(wait) = (*gd).f.wait_fences {
                    wait((*gd).real, 1, &batch.fence, VK_TRUE, u64::MAX);
                }
                if let Some(destroy_fence) = (*gd).f.destroy_fence {
                    destroy_fence((*gd).real, batch.fence, ptr::null());
                }
            }
            if !batch.cmd.is_null() && (*gd).cmd_pool != 0 {
                if let Some(free) = (*gd).f.free_cmd {
                    free((*gd).real, (*gd).cmd_pool, 1, &batch.cmd);
                }
            }
        }
        (*gd).gen_due.clear();
        if (*gd).cmd_pool != 0 {
            if let Some(destroy_pool) = (*gd).f.destroy_cmd_pool {
                destroy_pool((*gd).real, (*gd).cmd_pool, ptr::null());
            }
            (*gd).cmd_pool = 0;
        }
        if let Some(destroy_device) = (*gd).f.destroy_device {
            destroy_device((*gd).real, ptr::null());
        }
    }
    {
        let mut g = G.lock().unwrap();
        let mut pp: *mut *mut GnDevice = &raw mut g.devices;
        unsafe {
            while !(*pp).is_null() && *pp != gd {
                pp = &raw mut (**pp).next;
            }
            if !(*pp).is_null() {
                *pp = (*gd).next;
            }
            while !(*gd).queues.is_null() {
                let gq = (*gd).queues;
                (*gd).queues = (*gq).next;
                drop(Box::from_raw(gq));
            }
            while !(*gd).cmds.is_null() {
                let gc = (*gd).cmds;
                (*gd).cmds = (*gc).next;
                drop(Box::from_raw(gc));
            }
        }
    }
    unsafe {
        drop(Box::from_raw(gd));
    }
}

unsafe extern "C" fn gn_vkGetDeviceQueue(
    device: VkDevice,
    family: u32,
    index: u32,
    out: *mut VkQueue,
) {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        // Never leave `*out` unwritten: the guest would hold an uninitialised
        // queue and every later submit would be dropped against it. Name the
        // handle that did not match and the table that was searched.
        let (first, n) = unsafe { device_table() };
        loud("get_device_queue.miss", || {
            format!(
                "{}",
                gndrm_obf::obf!(
                    "proxy: gn_vkGetDeviceQueue DEVICE MISS family={family} index={index} {}",
                    unsafe { handle_note(false, device as usize, first, n) }
                )
            )
        });
        unsafe { *out = ptr::null_mut() };
        return;
    }
    let mut gq = Box::new(GnQueue {
        scratch: [0; GN_HANDLE_SCRATCH],
        real: ptr::null_mut(),
        gd,
        next: ptr::null_mut(),
        family,
    });
    unsafe {
        if let Some(get_queue) = (*gd).f.get_queue {
            get_queue((*gd).real, family, index, &mut gq.real);
        }
    }
    let gq = Box::into_raw(gq);
    {
        let _g = G.lock().unwrap();
        unsafe {
            (*gq).next = (*gd).queues;
            (*gd).queues = gq;
        }
    }
    unsafe { *out = gq.cast::<c_void>() }
}

unsafe extern "C" fn gn_vkGetDeviceQueue2(
    device: VkDevice,
    info: *const VkDeviceQueueInfo2,
    out: *mut VkQueue,
) {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        let (first, n) = unsafe { device_table() };
        loud("get_device_queue2.miss", || {
            format!(
                "{}",
                gndrm_obf::obf!("proxy: gn_vkGetDeviceQueue2 DEVICE MISS {}", unsafe {
                    handle_note(false, device as usize, first, n)
                })
            )
        });
        unsafe { *out = ptr::null_mut() };
        return;
    }
    // Per device: these resolved entry points are bound to the device that
    // produced the handle, so a process-global cache hands a later device a
    // function that fails its own handle check (a NULL queue or worse).
    let Some(real_fn) = (unsafe { (*gd).f.get_device_queue2 }) else {
        unsafe { gn_vkGetDeviceQueue(device, (*info).queueFamilyIndex, (*info).queueIndex, out) };
        return;
    };
    let mut gq = Box::new(GnQueue {
        scratch: [0; GN_HANDLE_SCRATCH],
        real: ptr::null_mut(),
        gd,
        next: ptr::null_mut(),
        family: unsafe { (*info).queueFamilyIndex },
    });
    unsafe {
        real_fn((*gd).real, info, &mut gq.real);
    }
    let gq = Box::into_raw(gq);
    {
        let _g = G.lock().unwrap();
        unsafe {
            (*gq).next = (*gd).queues;
            (*gd).queues = gq;
        }
    }
    unsafe { *out = gq.cast::<c_void>() };
}

// ------------------------------------------------- swapchain + present

fn vk_format_to_fourcc(format: VkFormat) -> u32 {
    match format {
        // DRM fourccs name the LE 32-bit word: ARGB8888 = 0xAARRGGBB, i.e.
        // memory bytes B,G,R,A — VK_FORMAT_B8G8R8A8_UNORM's layout. Both of
        // these Vulkan formats carry a real alpha channel, so they map to the
        // alpha-bearing A-formats, not XRGB8888/XBGR8888: the X byte is
        // *ignored* by the compositor, which is what forced a guest's frame
        // opaque. The daemon already reports the same buffer as AR24
        // (`PRIMARY_FORMATS`), so this keeps the two descriptions in agreement.
        VK_FORMAT_B8G8R8A8_UNORM => GN_DRM_FORMAT_ARGB8888,
        VK_FORMAT_R8G8B8A8_UNORM => GN_DRM_FORMAT_ABGR8888,
        // The two 10-bit layouts differ only in which channel sits in the high
        // ten bits, and the fourcc must say which one the buffer actually is:
        // `A2R10G10B10` (58, red high) is the pool's own layout and gamescope
        // reads it back as `ARGB2101010`; `A2B10G10R10` (64, blue high) is the
        // canonical Vulkan layout vkd3d asks for and reads back as
        // `ABGR2101010`. Declaring the wrong one exchanges red and blue —
        // measured on Ori: an HDR frame that should read warm (114,42,28) came
        // out blue (2,51,89).
        VK_FORMAT_A2R10G10B10_UNORM_PACK32 => GN_DRM_FORMAT_ARGB2101010,
        VK_FORMAT_A2B10G10R10_UNORM_PACK32 => GN_DRM_FORMAT_ABGR2101010,
        _ => 0,
    }
}

const SCANOUT_SWAPCHAIN_FORMAT: VkFormat = VK_FORMAT_B8G8R8A8_UNORM;

fn advertises_rgba() -> bool {
    std::env::var_os("GN_VKSHIM_FORCE_BGRA").is_none()
        && std::env::var_os("GN_VKSHIM_ALLOW_RGBA").is_some()
}

fn swapchain_format_allowed(format: VkFormat, allow_rgba: bool) -> bool {
    format == SCANOUT_SWAPCHAIN_FORMAT
        || (allow_rgba && format == VK_FORMAT_R8G8B8A8_UNORM)
        || (hdr_session()
            && (format == VK_FORMAT_A2B10G10R10_UNORM_PACK32
                || format == VK_FORMAT_A2R10G10B10_UNORM_PACK32))
}

/// Whether this session asked for HDR output. The app sets `GNDRM_HDR` in its
/// own process before forking the guest, so the guest — and this shim inside
/// it — sees the flag. It is the same flag the daemon gates the 10-bit pool
/// buffer and the HDR EDID on, which is what keeps the three consistent.
fn hdr_session() -> bool {
    std::env::var_os("GNDRM_HDR").is_some_and(|v| v == "1")
}

unsafe fn slot_release(sc: *mut GnSwapchain, s: *mut GnSlot) {
    let gd = (*sc).dev;
    unsafe {
        // The X11 route's pixmap and idle fence are X server resources, one
        // pair per slot. Free them with the slot or every destroyed swapchain
        // leaks them for the process's life (`destroy_pixmap`/`destroy_fence`
        // otherwise have no caller). A pending present keeps the server's own
        // pixmap reference; `xcb_free_pixmap` only drops ours. The worker has
        // been joined before this runs, so the connection is not in use.
        let surf = (*sc).surf;
        if !surf.is_null() {
            let x11 = (*surf).x11;
            if !x11.is_null() {
                if (*s).xpix != 0 {
                    (*x11).destroy_pixmap((*s).xpix);
                    (*s).xpix = 0;
                }
                if (*s).xfence != 0 {
                    (*x11).destroy_fence((*s).xfence);
                    (*s).xfence = 0;
                }
            }
        }
        if !(*s).buf.is_null() {
            wlproto::wl_proxy_destroy((*s).buf);
            (*s).buf = ptr::null_mut();
        }
        if (*s).fence != 0 {
            if let Some(f) = (*gd).f.destroy_fence {
                f((*gd).real, (*s).fence, ptr::null());
            }
        }
        if (*s).sem != 0 {
            if let Some(f) = (*gd).f.destroy_sem {
                f((*gd).real, (*s).sem, ptr::null());
            }
        }
        if (*s).image != 0 {
            if let Some(f) = (*gd).f.destroy_image {
                f((*gd).real, (*s).image, ptr::null());
            }
        }
        if (*s).mem != 0 {
            if let Some(f) = (*gd).f.free_mem {
                f((*gd).real, (*s).mem, ptr::null());
            }
        }
        if (*s).fd >= 0 {
            libc::close((*s).fd);
        }
        (*s).fd = -1;
        crate::scanout::ahb_release((*s).ahb);
        (*s).ahb = ptr::null_mut();
    }
}

// Present targets are dma-bufs the shim allocates from the system heap and
// IMPORTS into vulkan. The adrenotools driver family cannot export fds
// (vkGetMemoryFdKHR / vkGetImageDrmFormatModifierPropertiesEXT are not
// reachable through its hooks), so the ownership is inverted: the shim owns
// the fd, vulkan borrows it, and the same fd is committed to wayland.
fn dma_heap_open() -> i32 {
    static HEAP_FD: OnceLock<i32> = OnceLock::new();
    *HEAP_FD.get_or_init(|| {
        let path = CString::new("/dev/dma_heap/system").unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            crate::logf!(gndrm_obf::obf!(
                "proxy: /dev/dma_heap/system: {}",
                std::io::Error::last_os_error()
            ));
        }
        fd
    })
}

fn dma_heap_alloc(len: usize) -> i32 {
    let heap = dma_heap_open();
    if heap < 0 {
        return -1;
    }
    // DMA_HEAP_IOCTL_ALLOC: _IOWR('H', 0x0, struct dma_heap_allocation_data)
    #[repr(C)]
    struct dma_heap_allocation_data {
        len: u64,
        fd: u32,
        fd_flags: u32,
        heap_flags: u64,
    }
    // _IOC(_IOC_READ|_IOC_WRITE=3 << 30, size << 16, magic 'H' << 8, nr 0)
    const DMA_HEAP_IOCTL_ALLOC: libc::c_ulong = (3u64 << 30)
        | ((std::mem::size_of::<dma_heap_allocation_data>() as u64) << 16)
        | ((b'H' as u64) << 8);
    let mut data = dma_heap_allocation_data {
        len: len as u64,
        fd: 0,
        // O_RDWR: the fd crosses to gndrm, whose CPU upload path mmaps it
        // PROT_READ — an accmode-less fd fails mmap with EACCES.
        fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
        heap_flags: 0,
    };
    if unsafe { libc::ioctl(heap, DMA_HEAP_IOCTL_ALLOC as _, &mut data) } != 0 {
        crate::logf!(gndrm_obf::obf!(
            "proxy: dma_heap alloc({len}): {}",
            std::io::Error::last_os_error()
        ));
        return -1;
    }
    data.fd as i32
}

unsafe fn slot_init_sync(sc: *mut GnSwapchain, s: *mut GnSlot) -> VkResult {
    let gd = (*sc).dev;
    let dev = unsafe { (*gd).real };
    let f = unsafe { (*gd).f };
    let fci = VkFenceCreateInfo {
        sType: VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
        pNext: ptr::null(),
        flags: 0,
    };
    let sci = VkSemaphoreCreateInfo {
        sType: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
        pNext: ptr::null(),
        flags: 0,
    };
    let mut res = unsafe { f.create_fence.unwrap()(dev, &fci, ptr::null(), &mut (*s).fence) };
    if res == VK_SUCCESS {
        res = unsafe { f.create_sem.unwrap()(dev, &sci, ptr::null(), &mut (*s).sem) };
    }
    res
}

// ------------------------------------------------- image-path instrument
//
// Measurement-only. Every function below is either a log or a read-only
// physical-device query whose result is *discarded* — nothing here alters any
// argument handed to the driver, so image/swapchain creation behaviour is
// unchanged. The one-shot statics bound each trace to a single line per
// process, independent of how many swapchain images the guest creates.
static TRACE_IMAGE_CI_LINEAR: AtomicBool = AtomicBool::new(false);
static TRACE_MOD_QUERY: AtomicBool = AtomicBool::new(false);
static TRACE_MEM_PROPS: AtomicBool = AtomicBool::new(false);

/// Human-readable walk of a `VkImageCreateInfo` pNext chain. Read-only; only
/// the sType list plus the modifier/tiling/handleTypes fields are rendered.
unsafe fn pnext_image_create_summary(mut p: *const c_void) -> String {
    let mut out = String::new();
    let mut n = 0;
    while !p.is_null() && n < 8 {
        let b = p.cast::<VkBaseOutStructure>();
        let st = unsafe { (*b).sType };
        if !out.is_empty() {
            out.push_str(", ");
        }
        match st {
            VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO => {
                let e = p.cast::<VkExternalMemoryImageCreateInfo>();
                out.push_str(&format!(
                    "{}",
                    gndrm_obf::obf!(
                        "ExternalMemoryImageCreateInfo{{handleTypes={:#x}}}",
                        unsafe { (*e).handleTypes }
                    )
                ));
            }
            VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT => {
                let e = p.cast::<VkImageDrmFormatModifierExplicitCreateInfoEXT>();
                let m = unsafe { (*e).drmFormatModifier };
                let cnt = unsafe { (*e).drmFormatModifierPlaneCount };
                let pl = unsafe { (*e).pPlaneLayouts };
                if !pl.is_null() && cnt > 0 {
                    out.push_str(&format!("{}", gndrm_obf::obf!(
                        "ImageDrmFormatModifierExplicitCreateInfoEXT{{mod={m:#x}, planes={cnt}, plane0{{offset={}, rowPitch={}, size={}}}}}",
                        unsafe { (*pl).offset },
                        unsafe { (*pl).rowPitch },
                        unsafe { (*pl).size }
                    )));
                } else {
                    out.push_str(&format!(
                        "{}",
                        gndrm_obf::obf!(
                        "ImageDrmFormatModifierExplicitCreateInfoEXT{{mod={m:#x}, planes={cnt}}}"
                    )
                    ));
                }
            }
            VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT => {
                let e = p.cast::<VkImageDrmFormatModifierListCreateInfoEXT>();
                out.push_str(&format!(
                    "{}",
                    gndrm_obf::obf!(
                        "ImageDrmFormatModifierListCreateInfoEXT{{count={}}}",
                        unsafe { (*e).drmFormatModifierCount }
                    )
                ));
            }
            _ => out.push_str(&format!("{}", gndrm_obf::obf!("sType={st}"))),
        }
        p = unsafe { (*b).pNext as *const c_void };
        n += 1;
    }
    out
}

/// One-shot full dump of the `VkImageCreateInfo` about to be handed to
/// `vkCreateImage` (format/tiling/modifier/usage/extent + pNext walk).
unsafe fn trace_image_create(ci: &VkImageCreateInfo, tag: &str, shot: &AtomicBool) {
    if shot.swap(true, Ordering::Relaxed) {
        return;
    }
    let pnext = unsafe { pnext_image_create_summary(ci.pNext) };
    crate::logf!(gndrm_obf::obf!(
        "{tag}: vkCreateImage fmt={} {}x{}x{} mips={} layers={} samples={:#x} tiling={} usage={:#x} flags={:#x} pNext=[{pnext}]",
        ci.format,
        ci.extent.width,
        ci.extent.height,
        ci.extent.depth,
        ci.mipLevels,
        ci.arrayLayers,
        ci.samples,
        ci.tiling,
        ci.usage,
        ci.flags
    ));
}

/// Pure informational `vkGetPhysicalDeviceImageFormatProperties2` with a
/// `VkPhysicalDeviceImageDrmFormatModifierInfoEXT{ LINEAR }` pNext. The
/// `VkResult` and the returned properties are logged and then thrown away;
/// no caller branches on them. Run for two `tiling` values: the one the
/// create actually uses (LINEAR) and the only tiling for which the spec
/// honours the modifier pNext (DRM_FORMAT_MODIFIER_EXT).
unsafe fn query_linear_modifier_support(
    gd: *mut GnDevice,
    format: VkFormat,
    usage: VkImageUsageFlags,
    create_tiling: VkImageTiling,
) {
    if TRACE_MOD_QUERY.swap(true, Ordering::Relaxed) {
        return;
    }
    let inst = unsafe { (*(*gd).inst).real };
    let pd = unsafe { (*gd).pd };
    let raw = unsafe { gn_real_inst_proc(inst, c"vkGetPhysicalDeviceImageFormatProperties2") };
    let Some(_) = raw else {
        crate::logf!(gndrm_obf::obf!(
            "modq: vkGetPhysicalDeviceImageFormatProperties2 unresolved (driver/loader)"
        ));
        return;
    };
    let query: PFN_vkGetPhysicalDeviceImageFormatProperties2 = unsafe { std::mem::transmute(raw) };
    for tiling in [create_tiling, VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT] {
        let modinfo = VkPhysicalDeviceImageDrmFormatModifierInfoEXT {
            sType: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
            pNext: ptr::null(),
            drmFormatModifier: GN_DRM_MOD_LINEAR,
            sharingMode: VK_SHARING_MODE_EXCLUSIVE,
            queueFamilyIndexCount: 0,
            pQueueFamilyIndices: ptr::null(),
        };
        let fmtinfo = VkPhysicalDeviceImageFormatInfo2 {
            sType: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
            pNext: &modinfo as *const _ as *const c_void,
            format,
            type_: VK_IMAGE_TYPE_2D,
            tiling,
            usage,
            flags: 0,
        };
        let mut mods = [VkDrmFormatModifierPropertiesEXT {
            drmFormatModifier: 0,
            drmFormatModifierPlaneCount: 0,
            drmFormatModifierTilingFeatures: 0,
        }; 8];
        let mut modlist = VkDrmFormatModifierPropertiesListEXT {
            sType: VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,
            pNext: ptr::null_mut(),
            drmFormatModifierCount: mods.len() as u32,
            pDrmFormatModifierProperties: mods.as_mut_ptr(),
        };
        let mut props = VkImageFormatProperties2 {
            sType: VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2,
            pNext: &mut modlist as *mut _ as *mut c_void,
            imageFormatProperties: VkImageFormatProperties {
                maxExtent: VkExtent3D {
                    width: 0,
                    height: 0,
                    depth: 0,
                },
                maxMipLevels: 0,
                maxArrayLayers: 0,
                sampleCounts: 0,
                maxResourceSize: 0,
            },
        };
        let res = unsafe { query(pd, &fmtinfo, &mut props) };
        let ip = props.imageFormatProperties;
        crate::logf!(gndrm_obf::obf!(
            "modq: vkGetPhysicalDeviceImageFormatProperties2 fmt={format} type=2D tiling={tiling} usage={usage:#x} pNext=VkPhysicalDeviceImageDrmFormatModifierInfoEXT{{mod=LINEAR(0), sharing=EXCLUSIVE}} -> res={res} maxExtent={}x{}x{} maxMips={} maxLayers={} samples={:#x} maxRes={} modCount={} mod0={:#x} feat0={:#x} planes0={} [informational only: result not consulted]",
            ip.maxExtent.width,
            ip.maxExtent.height,
            ip.maxExtent.depth,
            ip.maxMipLevels,
            ip.maxArrayLayers,
            ip.sampleCounts,
            ip.maxResourceSize,
            modlist.drmFormatModifierCount,
            mods[0].drmFormatModifier,
            mods[0].drmFormatModifierTilingFeatures,
            mods[0].drmFormatModifierPlaneCount
        ));
    }
}

/// Read-only `vkGetPhysicalDeviceMemoryProperties` capture at device create.
unsafe fn fetch_mem_props(
    gi: *mut GnInstance,
    pd: VkPhysicalDevice,
) -> VkPhysicalDeviceMemoryProperties {
    let mut mp: VkPhysicalDeviceMemoryProperties = unsafe { std::mem::zeroed() };
    let inst = unsafe { (*gi).real };
    let raw = unsafe { gn_real_inst_proc(inst, c"vkGetPhysicalDeviceMemoryProperties") };
    if raw.is_some() {
        let f: PFN_vkGetPhysicalDeviceMemoryProperties = unsafe { std::mem::transmute(raw) };
        unsafe { f(pd, &mut mp) };
    }
    mp
}

/// One-shot summary of the memory types so the slot-import line below can be
/// read against the heaps the driver actually exposes.
unsafe fn log_mem_props_once(pd: VkPhysicalDevice, mp: &VkPhysicalDeviceMemoryProperties) {
    let _ = pd;
    if TRACE_MEM_PROPS.swap(true, Ordering::Relaxed) {
        return;
    }
    let n = (mp.memoryTypeCount as usize).min(VK_MAX_MEMORY_TYPES);
    let summary: Vec<String> = (0..n)
        .map(|i| {
            let t = mp.memoryTypes[i];
            format!(
                "{}",
                gndrm_obf::obf!(
                    "t{i}:heap{}:flags{:#x}{}",
                    t.heapIndex,
                    t.propertyFlags,
                    if t.propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT != 0 {
                        ":LOCAL"
                    } else {
                        ""
                    }
                )
            )
        })
        .collect();
    let heaps: Vec<String> = (0..(mp.memoryHeapCount as usize).min(VK_MAX_MEMORY_HEAPS))
        .map(|i| {
            format!(
                "{}",
                gndrm_obf::obf!(
                    "h{i}:{}B:flags{:#x}",
                    mp.memoryHeaps[i].size,
                    mp.memoryHeaps[i].flags
                )
            )
        })
        .collect();
    crate::logf!(gndrm_obf::obf!(
        "memprops: types={} [{}] heaps={} [{}]",
        mp.memoryTypeCount,
        summary.join(" "),
        mp.memoryHeapCount,
        heaps.join(" ")
    ));
}

/// Read-only description of a chosen `memoryTypeIndex` (heap + DEVICE_LOCAL).
fn mem_type_desc(gd: *mut GnDevice, idx: u32) -> String {
    let mp = unsafe { &(*gd).mem_props };
    if (idx as usize) < mp.memoryTypeCount as usize {
        let t = mp.memoryTypes[idx as usize];
        let local = t.propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT != 0;
        format!(
            "{}",
            gndrm_obf::obf!(
                "mem_type {idx} heap {} flags {:#x} DEVICE_LOCAL={}",
                t.heapIndex,
                t.propertyFlags,
                if local { "yes" } else { "no" }
            )
        )
    } else {
        format!(
            "{}",
            gndrm_obf::obf!(
                "mem_type {idx} (beyond memoryTypeCount {})",
                mp.memoryTypeCount
            )
        )
    }
}

/// Creates one swapchain slot image as a LINEAR, externally-exported image: the
/// guest's `VK_IMAGE_TILING_LINEAR` is the layout contract on its own. No DRM
/// format modifier is named or requested in the `pNext` chain — only
/// `VkExternalMemoryImageCreateInfo{DMA_BUF}` — so the driver always produces
/// plain scan-out pixels (LINEAR forbids UBWC outright) and DRI3 wayland
/// present see a modifier of 0. The image is backed by a dma-buf the shim owns
/// — the daemon's scan-out pool when that channel is up, a local system-heap
/// allocation otherwise — and bound through the imported-fd path.
unsafe fn slot_init(sc: *mut GnSwapchain, s: *mut GnSlot) -> VkResult {
    unsafe { slot_init_with_usage(sc, s, (*sc).usage) }
}

/// `slot_init` with a chosen image usage. A generation destination is written by
/// a blit and sampled by the compositor, which is not what a guest slot asks for.
unsafe fn slot_init_with_usage(
    sc: *mut GnSwapchain,
    s: *mut GnSlot,
    usage: VkImageUsageFlags,
) -> VkResult {
    let gd = (*sc).dev;
    if unsafe { (*gd).import_ahb } {
        return unsafe { slot_init_ahb(sc, s, usage) };
    }
    let dev = unsafe { (*gd).real };
    let f = unsafe { (*gd).f };
    (*s).fd = -1;
    (*s).stride = ((*sc).extent.width * 4 + 255) & !255;
    (*s).offset = 0;
    (*s).modifier = GN_DRM_MOD_LINEAR;
    unsafe {
        if (*s).fd >= 0 {
            libc::close((*s).fd);
        }
    }
    // The swapchain image must be a buffer the daemon owns: the presenter
    // can only publish one of its own `AHardwareBuffer`s. Ask for it over
    // the scan-out channel explicitly rather than relying on the
    // LD_PRELOAD shim to intercept this process's heap allocation. The
    // returned stride is authoritative for the image this fd backs.
    match crate::scanout::allocate((*sc).extent.width, (*sc).extent.height) {
        Some(buf) => {
            (*s).fd = buf.fd;
            // The ino the daemon registered this buffer under is the key its
            // frame-cap note files the release floor by.
            (*s).ino = buf.ino;
            if buf.stride != 0 {
                (*s).stride = buf.stride;
            }
            crate::logf!(gndrm_obf::obf!(
                "proxy slot: pool buffer fd={} ino={} stride={}",
                buf.fd,
                buf.ino,
                (*s).stride
            ));
        }
        None => {
            // No daemon channel, or it refused: a local system-heap
            // allocation, the pre-existing behaviour, which the LD_PRELOAD
            // shim may still redirect.
            (*s).fd = dma_heap_alloc((*s).stride as usize * (*sc).extent.height as usize);
        }
    }
    if (*s).fd < 0 {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let ext_img = VkExternalMemoryImageCreateInfo {
        sType: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        pNext: ptr::null(),
        handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    let ici = VkImageCreateInfo {
        sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        pNext: &ext_img as *const _ as *const c_void,
        flags: 0,
        imageType: VK_IMAGE_TYPE_2D,
        format: (*sc).format,
        extent: VkExtent3D {
            width: (*sc).extent.width,
            height: (*sc).extent.height,
            depth: 1,
        },
        mipLevels: 1,
        arrayLayers: 1,
        samples: VK_SAMPLE_COUNT_1_BIT,
        // TILING_LINEAR forbids UBWC outright, so the fd is always plain
        // scan-out pixels. No explicit DRM modifier is requested: the
        // guest's LINEAR tiling is the layout contract on its own.
        tiling: VK_IMAGE_TILING_LINEAR,
        usage,
        sharingMode: VK_SHARING_MODE_EXCLUSIVE,
        queueFamilyIndexCount: 0,
        pQueueFamilyIndices: ptr::null(),
        initialLayout: VK_IMAGE_LAYOUT_UNDEFINED,
    };
    // One-shot diagnostic dump of the exact create-info (pNext chain,
    // tiling, modifier) plus a pure read-only modifier-support query.
    unsafe { query_linear_modifier_support(gd, (*sc).format, usage, VK_IMAGE_TILING_LINEAR) };
    unsafe { trace_image_create(&ici, "img-create linear", &TRACE_IMAGE_CI_LINEAR) };
    let mut res = unsafe { f.create_image.unwrap()(dev, &ici, ptr::null(), &mut (*s).image) };
    if res != VK_SUCCESS {
        // Drivers that import dma-bufs only through DRM-modifier tiling (Mali):
        // the same LINEAR layout, stated explicitly at the slot's stride.
        let plane = VkSubresourceLayout {
            offset: 0,
            size: 0,
            rowPitch: u64::from((*s).stride),
            arrayPitch: 0,
            depthPitch: 0,
        };
        let explicit = VkImageDrmFormatModifierExplicitCreateInfoEXT {
            sType: VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
            pNext: &ext_img as *const _ as *const c_void,
            drmFormatModifier: GN_DRM_MOD_LINEAR,
            drmFormatModifierPlaneCount: 1,
            pPlaneLayouts: &plane,
        };
        let ici2 = VkImageCreateInfo {
            pNext: &explicit as *const _ as *const c_void,
            tiling: VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
            ..ici
        };
        let r1 = res;
        res = unsafe { f.create_image.unwrap()(dev, &ici2, ptr::null(), &mut (*s).image) };
        crate::logf!(gndrm_obf::obf!(
            "proxy import image: LINEAR tiling {r1}; explicit LINEAR modifier (pitch {}) {res}",
            (*s).stride
        ));
    }
    if res != VK_SUCCESS {
        crate::logf!(gndrm_obf::obf!("proxy import image create failed: {res}"));
        return res;
    }
    let mut reqs = VkMemoryRequirements {
        size: 0,
        alignment: 0,
        memoryTypeBits: 0,
    };
    unsafe { f.get_mem_reqs.unwrap()(dev, (*s).image, &mut reqs) };
    let mut mem_type = u32::MAX;
    for i in 0..32 {
        if reqs.memoryTypeBits & (1 << i) != 0 {
            mem_type = i;
            break;
        }
    }
    if mem_type == u32::MAX {
        crate::logf!(gndrm_obf::obf!("proxy import image: no memory type"));
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    crate::logf!(gndrm_obf::obf!(
        "proxy slot: linear {}x{} reqs {} memTypeBits {:#x} {}",
        (*sc).extent.width,
        (*sc).extent.height,
        reqs.size,
        reqs.memoryTypeBits,
        mem_type_desc(gd, mem_type)
    ));
    let ded = VkMemoryDedicatedAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        pNext: ptr::null(),
        image: (*s).image,
        buffer: 0,
    };
    let import_fd = unsafe { libc::dup((*s).fd) };
    let imp = VkImportMemoryFdInfoKHR {
        sType: VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
        pNext: &ded as *const _ as *const c_void,
        handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        fd: import_fd,
    };
    let mai = VkMemoryAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        pNext: &imp as *const _ as *const c_void,
        allocationSize: reqs.size,
        memoryTypeIndex: mem_type,
    };
    let mut res = unsafe { f.alloc_mem.unwrap()(dev, &mai, ptr::null(), &mut (*s).mem) };
    if res == VK_SUCCESS {
        res = unsafe { f.bind_mem.unwrap()(dev, (*s).image, (*s).mem, 0) };
    } else {
        // A failed import does not consume the descriptor (the driver keeps
        // nothing): close the dup or it leaks with the abandoned slot.
        unsafe { libc::close(import_fd) };
    }
    if res != VK_SUCCESS {
        crate::logf!(gndrm_obf::obf!("proxy import memory failed: {res}"));
        return res;
    }
    slot_init_sync(sc, s)
}

const VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID:
    VkExternalMemoryHandleTypeFlags = 0x400;
const STYPE_AHB_PROPERTIES: VkStructureType = 1000129000;
const STYPE_AHB_FORMAT_PROPERTIES: VkStructureType = 1000129002;
const STYPE_IMPORT_AHB: VkStructureType = 1000129003;

#[repr(C)]
struct AhbProperties {
    s_type: VkStructureType,
    next: *mut c_void,
    allocation_size: u64,
    memory_type_bits: u32,
}

#[repr(C)]
struct AhbFormatProperties {
    s_type: VkStructureType,
    next: *mut c_void,
    format: VkFormat,
    external_format: u64,
    format_features: u32,
    components: [u32; 4],
    ycbcr: [u32; 4],
}

#[repr(C)]
struct ImportAhb {
    s_type: VkStructureType,
    next: *const c_void,
    buffer: *mut c_void,
}

/// `slot_init_with_usage` on a driver that imports only `AHardwareBuffer`s (the
/// Adreno system driver has no dma-buf import). The daemon hands out the same
/// pool buffer as on the dma-buf path, with its `AHardwareBuffer` handle as
/// well: the fd still goes to the compositor, the handle to the driver. Images
/// backed by an `AHardwareBuffer` must use OPTIMAL tiling; the layout is the
/// one gralloc chose for the pool's usage (CPU-readable, so linear), which is
/// what the compositor reads through the fd.
unsafe fn slot_init_ahb(
    sc: *mut GnSwapchain,
    s: *mut GnSlot,
    usage: VkImageUsageFlags,
) -> VkResult {
    let gd = unsafe { (*sc).dev };
    let dev = unsafe { (*gd).real };
    let f = unsafe { (*gd).f };
    unsafe {
        (*s).fd = -1;
        (*s).offset = 0;
        (*s).modifier = GN_DRM_MOD_LINEAR;
        (*s).ahb = ptr::null_mut();
    }
    let (width, height) = unsafe { ((*sc).extent.width, (*sc).extent.height) };
    let Some(buf) = crate::scanout::allocate_ahb(width, height) else {
        crate::logf!(gndrm_obf::obf!(
            "proxy slot: no AHardwareBuffer from the daemon for {width}x{height}"
        ));
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    unsafe {
        (*s).fd = buf.fd;
        (*s).ino = buf.ino;
        (*s).ahb = buf.ahb;
        (*s).stride = if buf.stride != 0 {
            buf.stride
        } else {
            (width * 4 + 255) & !255
        };
    }
    type PropsPfn =
        unsafe extern "C" fn(VkDevice, *const c_void, *mut AhbProperties) -> VkResult;
    let get_props: Option<PropsPfn> = unsafe {
        std::mem::transmute(gn_real_device_proc(
            gd,
            c"vkGetAndroidHardwareBufferPropertiesANDROID",
        ))
    };
    let Some(get_props) = get_props else {
        crate::logf!(gndrm_obf::obf!(
            "proxy slot: driver resolves no vkGetAndroidHardwareBufferPropertiesANDROID"
        ));
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let mut fmt = AhbFormatProperties {
        s_type: STYPE_AHB_FORMAT_PROPERTIES,
        next: ptr::null_mut(),
        format: 0,
        external_format: 0,
        format_features: 0,
        components: [0; 4],
        ycbcr: [0; 4],
    };
    let mut props = AhbProperties {
        s_type: STYPE_AHB_PROPERTIES,
        next: (&raw mut fmt).cast(),
        allocation_size: 0,
        memory_type_bits: 0,
    };
    let res = unsafe { get_props(dev, buf.ahb, &mut props) };
    crate::logf!(gndrm_obf::obf!(
        "proxy slot: AHB fd={} ino={} stride={} size {} memTypeBits {:#x} format {} (swapchain {}): {res}",
        buf.fd,
        buf.ino,
        unsafe { (*s).stride },
        props.allocation_size,
        props.memory_type_bits,
        fmt.format,
        unsafe { (*sc).format }
    ));
    if res != VK_SUCCESS || props.memory_type_bits == 0 {
        return if res != VK_SUCCESS { res } else { VK_ERROR_INITIALIZATION_FAILED };
    }
    let ext_img = VkExternalMemoryImageCreateInfo {
        sType: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        pNext: ptr::null(),
        handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID,
    };
    let ici = VkImageCreateInfo {
        sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        pNext: &ext_img as *const _ as *const c_void,
        flags: 0,
        imageType: VK_IMAGE_TYPE_2D,
        format: unsafe { (*sc).format },
        extent: VkExtent3D {
            width,
            height,
            depth: 1,
        },
        mipLevels: 1,
        arrayLayers: 1,
        samples: VK_SAMPLE_COUNT_1_BIT,
        tiling: VK_IMAGE_TILING_OPTIMAL,
        usage,
        sharingMode: VK_SHARING_MODE_EXCLUSIVE,
        queueFamilyIndexCount: 0,
        pQueueFamilyIndices: ptr::null(),
        initialLayout: VK_IMAGE_LAYOUT_UNDEFINED,
    };
    let res = unsafe { f.create_image.unwrap()(dev, &ici, ptr::null(), &mut (*s).image) };
    if res != VK_SUCCESS {
        crate::logf!(gndrm_obf::obf!("proxy import image (AHB) create failed: {res}"));
        return res;
    }
    let mem_type = (0..32u32)
        .find(|&i| {
            props.memory_type_bits & (1 << i) != 0
                && (i as usize) < unsafe { (*gd).mem_props.memoryTypeCount } as usize
                && unsafe { (*gd).mem_props.memoryTypes[i as usize].propertyFlags } & 0x1 != 0
        })
        .or_else(|| (0..32u32).find(|&i| props.memory_type_bits & (1 << i) != 0))
        .unwrap_or(0);
    let ded = VkMemoryDedicatedAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        pNext: ptr::null(),
        image: unsafe { (*s).image },
        buffer: 0,
    };
    let imp = ImportAhb {
        s_type: STYPE_IMPORT_AHB,
        next: &ded as *const _ as *const c_void,
        buffer: buf.ahb,
    };
    let mai = VkMemoryAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        pNext: &imp as *const _ as *const c_void,
        allocationSize: props.allocation_size,
        memoryTypeIndex: mem_type,
    };
    let mut res = unsafe { f.alloc_mem.unwrap()(dev, &mai, ptr::null(), &mut (*s).mem) };
    if res == VK_SUCCESS {
        res = unsafe { f.bind_mem.unwrap()(dev, (*s).image, (*s).mem, 0) };
    }
    crate::logf!(gndrm_obf::obf!(
        "proxy slot: AHB import {width}x{height} type {} {}: {res}",
        mem_type,
        mem_type_desc(gd, mem_type)
    ));
    if res != VK_SUCCESS {
        return res;
    }
    unsafe { slot_init_sync(sc, s) }
}

unsafe fn slot_make_buffer(sc: *mut GnSwapchain, s: *mut GnSlot) -> *mut wl_proxy {
    let (dmabuf, queue) = {
        let w = G_WL.lock().unwrap();
        (w.dmabuf, w.queue)
    };
    let params = unsafe { wlproto::dmabuf_create_params(dmabuf) };
    if params.is_null() {
        return ptr::null_mut();
    }
    let _sg = SEND_LOCK.lock().unwrap();
    unsafe {
        wlproto::params_add(params, (*s).fd, 0, (*s).offset, (*s).stride, (*s).modifier);
        let buf = wlproto::params_create_immed(
            params,
            (*sc).extent.width as i32,
            (*sc).extent.height as i32,
            (*sc).fourcc,
            0,
        );
        wlproto::wl_proxy_destroy(params);
        if !buf.is_null() {
            wlproto::wl_proxy_add_listener(buf, BUFFER_LISTENER.as_ptr() as *mut _, s.cast());
            wlproto::wl_proxy_set_queue(buf, queue);
        }
        buf
    }
}
