//! wayland WSI proxy for guest vulkan drivers that lack
//! VK_KHR_wayland_surface (the whole adrenotools turnip family: no wayland
//! platform compiled in, so no passthrough can ever present on GameScope's
//! wayland socket).
//!
//! The proxy advertises the surface extensions a guest reaches it with —
//! VK_KHR_wayland_surface, VK_KHR_xlib_surface and VK_KHR_xcb_surface (the
//! last is what Mesa's zink/kopper WSI creates its swapchain from on X11, so
//! a GL guest shares the Vulkan guest's swapchain and its acquire gate) — and
//! implements the whole swapchain contract client-side. Nothing hooks-side may ever touch
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
use gndrm_uapi::wire::ScanoutLayout;
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

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
    crate::logf!("x11 stall ring ({reason}): last {} entry points:",
        r.len());
    let n = r.len();
    for (i, e) in r.iter().enumerate() {
        crate::logf!("x11 stall ring [{}/{}] tid={} {} args={:x} {:x} {:x} {:x} {:x}",
            i + 1,
            n,
            e.tid,
            e.name,
            e.args[0],
            e.args[1],
            e.args[2],
            e.args[3],
            e.args[4]);
    }
}
static LOG_COMMIT: AtomicUsize = AtomicUsize::new(0);

static LOG_RELEASE: AtomicUsize = AtomicUsize::new(0);
const LOG_FIRST_N: usize = 8;

// Swapchain handshake liveness. The stall class that matters — the app stops
// presenting, or the compositor stops releasing — is invisible under first-N
// budgets, so these sites are RATE-LIMITED instead of budgeted, and the idle
// present worker prints the same line as a heartbeat. Every line carries the
// per-slot flags, so a frozen pipeline always leaves a trail.
static N_ACQ: AtomicUsize = AtomicUsize::new(0);
static N_PRES: AtomicUsize = AtomicUsize::new(0);
/// Frames this shim has handed to the guest, reported to the daemon so the app's
/// frame ring follows acquisitions rather than presentations.
static ACQ_COUNT: AtomicU64 = AtomicU64::new(0);
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
/// Time spent inside `vkQueuePresentKHR`, in microseconds: the frame's own
/// window, between the acquire returning and the next one starting. `acqus`
/// covers only the acquire, so a guest that stalls in present showed up as an
/// unexplained gap between two fast acquires.
static PRESENT_US_N: AtomicUsize = AtomicUsize::new(0);
static PRESENT_US_SUM: AtomicUsize = AtomicUsize::new(0);
static PRESENT_US_MAX: AtomicUsize = AtomicUsize::new(0);
/// Presents that took at least one 60 Hz vblank, the scale at which a present
/// stall is visible to the panel.
static PRESENT_US_GE_VBLANK: AtomicUsize = AtomicUsize::new(0);
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
static PRES_IV_LAST_NS: AtomicU64 = AtomicU64::new(0);
// Acquire-gate census, in microseconds, the same shape as the dwell and the
// present interval above and reported on the same lines. The cap is applied in
// `gn_acquire_common`, so its proof lives there and not on the release: a
// release carries no pacing, and a census read on that side reports zeroes for a
// cap that is working. `GATE_HOLD_N` counts the acquires the deadline actually
// delayed, `GATE_HOLD_SUM_US` the total time they were parked, and the max the
// worst single park. Read against `cap=[fps=… notes=… acq_fps=…]`: `notes=0` is
// a target that never arrived; `notes>0` with `gate_holds=0` is a target that
// arrived and never bound, so the guest or the compositor is the slower element;
// `gate_holds>0` with `acq_fps` short of the target is the gate binding and the
// rate still below it, which is a schedule fault rather than a missing target.
static GATE_HOLD_N: AtomicUsize = AtomicUsize::new(0);
/// Acquires that ran with a gate, and acquires that ran without one. The cap
/// can only hold on the gated ones, so the split is the first thing a cap that
/// reads high has to explain.
static ACQ_GATED_N: AtomicUsize = AtomicUsize::new(0);
static ACQ_UNGATED_N: AtomicUsize = AtomicUsize::new(0);
static GATE_HOLD_SUM_US: AtomicUsize = AtomicUsize::new(0);
/// Time the guest actually spent parked by the gate, ns: what the cap costs the
/// producer, as opposed to what it saves the display.
static GATE_WAIT_NS: AtomicU64 = AtomicU64::new(0);
/// Acquire spacing, in buckets of the panel period: `<14 ms`, `14-16.2`,
/// `16.2-17.2`, `17.2-20`, `>20`. A cap that holds shows one bucket at the
/// interval; a cap that leaks shows mass below it.
static ACQ_SPACING: [AtomicUsize; 5] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];
static ACQ_LAST_NS: AtomicU64 = AtomicU64::new(0);
/// The first grant's timestamp, so the census divides frames by elapsed time
/// rather than by the sum of their own waits.
static ACQ_FIRST_NS: AtomicU64 = AtomicU64::new(0);
/// Schedule bookings, for the census's `books=` next to the grant count.
static BOOK_N: AtomicUsize = AtomicUsize::new(0);
/// Acquire *calls* that handed the guest an image, and calls that left without
/// one (`VK_NOT_READY`/`VK_TIMEOUT`). A presentation rate is the first, never
/// the call count: an engine that polls with a zero timeout retries, and a
/// retry is not a frame.
static ACQ_OK_N: AtomicUsize = AtomicUsize::new(0);
static ACQ_RETRY_N: AtomicUsize = AtomicUsize::new(0);
/// First instant this shim sized a swapchain, for the census's session age: a
/// cumulative counter is only a rate once the denominator is known.
static SHIM_T0_NS: AtomicU64 = AtomicU64::new(0);
/// Release notes applied to a parked slot, and slots freed by
/// [`release_note_limit_ns`] instead. The first is the measurement working; the
/// second is a note that never came, and a session where it dominates is one
/// where the daemon is not naming every buffer the guest holds.
static NOTES_APPLIED: AtomicUsize = AtomicUsize::new(0);
static PARK_TIMEOUTS: AtomicUsize = AtomicUsize::new(0);

/// Commits whose slot carried a present id but no wait id: the present armed the
/// id, so something cleared it between the present and the commit. The release
/// handler is the only other writer, and a frame that loses its id this way is
/// one `present::committed` never registers, leaving its wait with no bound.
static NOID_AT_COMMIT: AtomicUsize = AtomicUsize::new(0);

/// How long after its park the note that freed a slot arrived, bucketed at 5 ms,
/// 33 ms (the park bound) and 100 ms. `notes=` alone cannot tell a note that
/// arrived in time from one that arrived after the guest had already gone on.
static NOTELAG: [AtomicUsize; 4] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

fn note_lag_bucket(ns: u64) -> usize {
    match ns {
        0..5_000_000 => 0,
        5_000_000..33_000_000 => 1,
        33_000_000..100_000_000 => 2,
        _ => 3,
    }
}
static GATE_HOLD_MAX_US: AtomicUsize = AtomicUsize::new(0);

/// Record the interval since the previous present. Called once per present at
/// entry; the first present only seeds the stamp, so the delta count is
/// presents - 1 per process.
///
/// The stamp is raw `CLOCK_MONOTONIC` nanoseconds in one atomic rather than a
/// `Mutex<Option<Instant>>`: this runs on every present, and with the stamp
/// swapped in a single instruction the census costs a clock read instead of a
/// lock. Callers pass `monotonic_ns()`'s value.
fn pres_iv_note(now_ns: u64) {
    let prev = PRES_IV_LAST_NS.swap(now_ns, Ordering::Relaxed);
    if prev == 0 {
        return;
    }
    let us = now_ns.saturating_sub(prev) as usize / 1_000;
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
// The present-mode census's counters (see `mode_census`): commits declared to
// gamescope as FIFO and as anything else, and every compositor release. They
// count whether or not `GNDRM_LOG_VKSHIM` is set, unlike the WSI census's, so
// the periodic line reads the session from its start when the switch is on.
static DECL_FIFO_N: AtomicUsize = AtomicUsize::new(0);
static DECL_OTHER_N: AtomicUsize = AtomicUsize::new(0);
static REL_ALL_N: AtomicUsize = AtomicUsize::new(0);
// Gated acquires that found no free image once past the gate and waited on a
// compositor release (the ungated census's `slot_waits=`).
static SLOT_WAIT_N: AtomicUsize = AtomicUsize::new(0);
// Spacing of consecutive guest-frame commits since the last census line, in
// buckets of <15, 15-16.2, 16.2-17.2, 17.2-20 and >20 ms: the guest's own
// cadence as it reaches gamescope (`commit_spacing_bucket`).
static COMMIT_GAPS: [AtomicUsize; 5] = [const { AtomicUsize::new(0) }; 5];
static LAST_COMMIT_NS: AtomicU64 = AtomicU64::new(0);

/// The commit interval, smoothed, in nanoseconds: the cadence whose slowness
/// causes the compositor's hold. Zero until a second commit has been seen.
static COMMIT_SPACING_NS: AtomicU64 = AtomicU64::new(0);

/// The `COMMIT_GAPS` bucket of a spacing of `gap_ns`.
fn commit_spacing_bucket(gap_ns: u64) -> usize {
    match gap_ns {
        0..15_000_000 => 0,
        15_000_000..16_200_000 => 1,
        16_200_000..17_200_000 => 2,
        17_200_000..=20_000_000 => 3,
        _ => 4,
    }
}
static RATE_MODE: Mutex<Option<Instant>> = Mutex::new(None);
const MODE_CENSUS_EVERY: Duration = Duration::from_secs(10);
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
    crate::logf!("proxy hb: fmt={} fmt2={} caps={} caps2={} pmodes={} create={} destroy={} images={} acq={} present={}",
        HB_FMT.load(Ordering::Relaxed),
        HB_FMT2.load(Ordering::Relaxed),
        HB_CAPS.load(Ordering::Relaxed),
        HB_CAPS2.load(Ordering::Relaxed),
        HB_PMODES.load(Ordering::Relaxed),
        HB_CREATE.load(Ordering::Relaxed),
        HB_DESTROY.load(Ordering::Relaxed),
        HB_IMAGES.load(Ordering::Relaxed),
        HB_ACQ.load(Ordering::Relaxed),
        HB_PRESENT.load(Ordering::Relaxed),);
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

/// Slot time, kept apart from the rate's: an acquire that waited for the cap and
/// then for an image used to have the whole span filed under the rate, which read
/// as `gate_avg_us=333460` at a 16.7 ms period — 20 periods of "cap" that were
/// really one image's hold.
/// Parks answered by order rather than by the note's inode (`notelag`'s
/// counterpart): a count above zero means this device does not key notes by the
/// parked buffer, and the order path is what is keeping the pipeline moving.
static NOTES_BY_ORDER: AtomicUsize = AtomicUsize::new(0);

static SLOT_HOLD_N: AtomicUsize = AtomicUsize::new(0);
static SLOT_HOLD_SUM_US: AtomicUsize = AtomicUsize::new(0);
static SLOT_HOLD_MAX_US: AtomicUsize = AtomicUsize::new(0);

/// The mean slot hold per acquire that paid one, or 0.
fn slot_hold_avg_us() -> usize {
    SLOT_HOLD_SUM_US
        .load(Ordering::Relaxed)
        .checked_div(SLOT_HOLD_N.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// The mean park per delayed acquire, or 0 while none has been delayed — the
/// aggregate `gate_us=` divided by `gate_holds=`.
fn gate_hold_avg_us() -> usize {
    GATE_HOLD_SUM_US
        .load(Ordering::Relaxed)
        .checked_div(GATE_HOLD_N.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// Grants per second over the window the census counts: the first grant
/// timestamp and the last one, against how many frames were handed out.
///
/// This used to be `1e6 * n / sum(acq_us)` — the reciprocal of the mean
/// `vkAcquireNextImageKHR` *duration*, which is a service time and not a rate. It
/// read 88.98 on a session whose limit was 30 acquires/s, and I read that as the
/// guest running three times over its cap; the sessions it "measured" as 34.4,
/// 41.7, 49.7 and 64.3 were all that same mistake. A rate has to divide frames by
/// elapsed time, and elapsed time has to come from the clock, not from the thing
/// being measured.
fn acq_fps() -> f64 {
    let n = ACQ_OK_N.load(Ordering::Relaxed);
    let first = ACQ_FIRST_NS.load(Ordering::Relaxed);
    let last = ACQ_LAST_NS.load(Ordering::Relaxed);
    if n < 2 || first == 0 || last <= first {
        return 0.0;
    }
    1_000_000_000.0 * (n - 1) as f64 / (last - first) as f64
}

/// The generation ring, as `in_flight/total` over the slots behind the guest's
/// images, plus how many of them are parked waiting for a release note. A
/// destination that is written, committed and never released reads `n/n parked=n`
/// and is the reason a batch finds nothing free while the cap says it may grow.
fn dest_state(sc: *mut GnSwapchain) -> String {
    let Ok(g) = (unsafe { (*sc).shared.lock() }) else {
        return String::from("<locked>");
    };
    let guest = unsafe { (*sc).guest_slots as usize };
    let mut total = 0usize;
    let mut in_flight = 0usize;
    let mut parked = 0usize;
    for s in unsafe { (*sc).slots.iter().skip(guest) } {
        total += 1;
        if s.in_flight {
            in_flight += 1;
        }
        if s.parked_ns != 0 {
            parked += 1;
        }
    }
    drop(g);
    format!("{in_flight}/{total} parked={parked}")
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
        format_args!("acq={} pres={} rel={} commit={} starved={} in_flight={in_flight} slots=[{}] dests=[{}] gendrel={} acqus=[n={} sum={} max={} nz={} ge8={} ge16={}] presiv=[n={} sum={} min={} max={} ge16667={} ge33333={}] cap=[fps={} notes={} gate_holds={} gate_us={} gate_avg_us={} gate_max_us={} slot_avg_us={} by_order={} acq_fps={:.2}]",
            N_ACQ.load(Ordering::Relaxed),
            N_PRES.load(Ordering::Relaxed),
            N_REL.load(Ordering::Relaxed),
            N_COMMIT_OK.load(Ordering::Relaxed),
            N_STARVE.load(Ordering::Relaxed),
            flags.trim_end(),
            dest_state(sc),
            GEN_RELEASES.load(Ordering::Relaxed),
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
                GATE_HOLD_N.load(Ordering::Relaxed),
                GATE_HOLD_SUM_US.load(Ordering::Relaxed),
                gate_hold_avg_us(),
                GATE_HOLD_MAX_US.load(Ordering::Relaxed),
                slot_hold_avg_us(),
                NOTES_BY_ORDER.load(Ordering::Relaxed),
                acq_fps(),)
    )
}

/// Counters only — safe to call while `shared` is held.
fn counters() -> String {
    format!(
        "{}",
        format_args!("acq={} pres={} rel={} commit={} starved={} gendrel={} maint1=[fences={} released={}] acqus=[n={} sum={} max={} nz={} ge8={} ge16={}] presiv=[n={} sum={} min={} max={} ge16667={} ge33333={}] cap=[fps={} notes={} acq_fps={:.2} books={} gate_holds={} gate_us={} gate_avg_us={} gate_max_us={} slot_holds={} slot_us={} slot_avg_us={} slot_max_us={} by_order={}] presus=[n={} sum={} max={} ge16667={}]",
            N_ACQ.load(Ordering::Relaxed),
            N_PRES.load(Ordering::Relaxed),
            N_REL.load(Ordering::Relaxed),
            N_COMMIT_OK.load(Ordering::Relaxed),
            N_STARVE.load(Ordering::Relaxed),
            GEN_RELEASES.load(Ordering::Relaxed),
            N_PRESENT_FENCE.load(Ordering::Relaxed),
            N_RELEASED_IMAGES.load(Ordering::Relaxed),
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
                acq_fps(),
                BOOK_N.load(Ordering::Relaxed),
                GATE_HOLD_N.load(Ordering::Relaxed),
                GATE_HOLD_SUM_US.load(Ordering::Relaxed),
                gate_hold_avg_us(),
                GATE_HOLD_MAX_US.load(Ordering::Relaxed),
                SLOT_HOLD_N.load(Ordering::Relaxed),
                SLOT_HOLD_SUM_US.load(Ordering::Relaxed),
                slot_hold_avg_us(),
                SLOT_HOLD_MAX_US.load(Ordering::Relaxed),
                NOTES_BY_ORDER.load(Ordering::Relaxed),
                PRESENT_US_N.load(Ordering::Relaxed),
                PRESENT_US_SUM.load(Ordering::Relaxed),
                PRESENT_US_MAX.load(Ordering::Relaxed),
                PRESENT_US_GE_VBLANK.load(Ordering::Relaxed),)
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
    crate::logf!("libwayland: {}", msg.trim_end());
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
        crate::logf!("x11 trace {trace_n}: sc={sc:?} idx={idx} acq#{acq} {}x{} alpha0={a0}/{} nz={nz} px0=({},{},{},{})",
            unsafe { (*sc).extent.width },
            unsafe { (*sc).extent.height },
            unsafe { (*sc).extent.width as usize * (*sc).extent.height as usize },
            px[0],
            px[1],
            px[2],
            px[3]);
        // Bounded one-shot two-view probe (first presents): the shim's own
        // linear mmap above vs the driver's own mapping of the same dma-buf
        // (vkMapMemory of the slot's imported memory). They only differ if the
        // image's real layout/stride is not what the shim advertises/reads.
        if trace_n < X11_VIEW_N {
            if let Some((va0, vnz, vpx)) = unsafe { slot_mem_signature(sc, s) } {
                crate::logf!("x11 view {trace_n}: idx={idx} driver(mapmem) alpha0={va0}/{} nz={vnz} px0=({},{},{},{})",
                    unsafe { (*sc).extent.width as usize * (*sc).extent.height as usize },
                    vpx[0],
                    vpx[1],
                    vpx[2],
                    vpx[3]);
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
                crate::logf!("x11: slot {idx} has no pixmap - frame dropped");
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
                crate::logf!("x11 present: sc={sc:?} idx={idx} pix={} serial={serial} {}x{} {}",
                    unsafe { (*s).xpix },
                    unsafe { (*sc).extent.width },
                    unsafe { (*sc).extent.height },
                    counters());
            }
        }
        None => {
            crate::logf!("x11: slot {idx} present refused - frame dropped");
            return false;
        }
    }
    true
}

/// Frees every X11 slot whose idle fence has triggered - the server is done
/// with its pixmap, the slot-reuse contract the wayland arm gets from
/// wl_buffer.release.
unsafe fn x11_reap(sc: *mut GnSwapchain) {
    let present = (*(*sc).surf).x11;
    if present.is_null() {
        return;
    }
    // The release census is read under the log gate only.
    let logging = crate::vkshim_enabled();
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
        if unsafe { (*present).fence_idle(fence) } {
            // The server is done with the pixmap: the slot is free, and the cap
            // (if any) is applied when the guest asks for it again.
            let _g = unsafe { (*sc).shared.lock().unwrap() };
            let wait_id = unsafe {
                (*s).in_flight = false;
                (*sc).cv_free.notify_all();
                core::mem::replace(&mut (*s).wait_id, 0)
            };
            drop(_g);
            if wait_id != 0 {
                crate::present::complete_on_release(wait_id, sc as usize, monotonic_ns());
            }
            if logging {
                N_REL.fetch_add(1, Ordering::Relaxed);
                if rate_ok(&RATE_REL, RATE_EVERY) {
                    crate::logf!("x11 release: slot={i} {}", slot_flags(sc));
                }
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

/// Report one generated frame the moment it is on the compositor's surface.
///
/// The daemon counts produced frames from the deltas of the two totals an
/// acquire message carries, and that message is sent once per *acquired* frame.
/// A generated frame therefore reached the app's frame ring only when the guest
/// next acquired: up to a multiplier's worth late, coalesced into a burst, and
/// then spread over the interval by `note_produced`. The ring's own reading of
/// that was a HUD that jumped by several fps while the session's rate did not
/// move (`acq_fps=29.00, 29.01, 29.00` over 7 000 frames).
///
/// Sent here instead — one message per generated frame, acquire total unchanged —
/// so `note_produced` sees a delta of exactly one generated frame and calls
/// `notify_frame_published` once per frame, at the frame's own commit.
///
/// `send_frame_nb` is non-blocking and the totals are cumulative, so a refused
/// message costs one ring sample and carries the frame on the next one.
fn note_frame_generated() {
    let generated = GEN_COMMITS.fetch_add(1, Ordering::Relaxed) + 1;
    let acquired = ACQ_COUNT.load(Ordering::Relaxed);
    crate::scanout::send_frame_acquired(acquired, u64::from(generated));
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
            // A destination's note is matched against its commit too, and a zero
            // here would make `slot_just_freed`'s anchor `min(0, parked) = 0` —
            // i.e. accept any note ever sent for this ino, including one for an
            // earlier frame of the same buffer.
            (*s).commit_ns = monotonic_ns();
        }
    }
    if unsafe { (*(*sc).surf).parent.is_null() } {
        unsafe { x11_present_slot(sc, s, slot_index as i32) };
        note_frame_generated();
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
        bind_window_at_first_commit(sc);
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
    note_frame_generated();
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
    let guest = unsafe { gen_first_slot(sc) };
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
                present_id: 0,
                wait_id: 0,
                wait_registered: false,
                fence: 0,
                sem: 0,
                fence_sem: 0,
                acquired: false,
                in_flight: false,
                parked_ns: 0,
                commit_ns: 0,
                acq_seq: 0,
                acquired_ns: 0,
                ino: 0,
                ahb: ptr::null_mut(),
                spare: NO_SPARE,
            });
        }
        let res =
            unsafe { slot_init_with_usage(sc, (*sc).slots.as_mut_ptr().add(index), GEN_USAGE) };
        if res != VK_SUCCESS {
            crate::logf!("lsfg: destination slot init failed: {res}");
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

/// The first slot of the ring behind the guest's images.
///
/// One ring serves both consumers: a strict game's hidden copies and frame
/// generation's destinations. They never run together — [`spare_in_use`] is off
/// while generation is live — so a slot is only ever one of the two at any
/// moment, and each allocator skips what the other still has out (`in_flight`
/// for a slot at the compositor, `gen_due` for a batch queued to be written).
/// Sizing one ring at `max(hidden, destinations)` instead of the sum is what
/// keeps a listed game from paying for both: 4 + 5 slots became 5.
unsafe fn gen_first_slot(sc: *mut GnSwapchain) -> usize {
    unsafe { (*sc).guest_slots as usize }
}

/// The slots one ring behind the guest's images needs: the larger of the two
/// consumers, never their sum.
fn dest_ring(hidden: usize, destinations: usize) -> usize {
    destinations.max(hidden)
}

/// A guest slot whose frame goes out as it is, without a hidden copy.
const NO_SPARE: i32 = -1;

/// The command buffers that copy a strict game's frames to its hidden slots, one
/// per guest image. Used by the guest's present and by the destroy, which the
/// guest must already synchronise, so nothing locks them.
pub struct SpareCopy {
    pool: VkCommandPool,
    family: u32,
    cmds: Vec<VkCommandBuffer>,
    begin: unsafe extern "C" fn(VkCommandBuffer, *const VkCommandBufferBeginInfo) -> VkResult,
    end: unsafe extern "C" fn(VkCommandBuffer) -> VkResult,
}

/// Whether this swapchain's frames go through its hidden slots now. Generation
/// reads the guest's image itself and has its own destinations, so a strict game
/// that turns it on is back on the direct path until it turns it off.
unsafe fn spare_in_use(sc: *mut GnSwapchain) -> bool {
    unsafe { (*sc).spare_slots != 0 && !crate::lsfg::live().0 }
}

/// The pool and command buffers for a queue of `family`, made at the swapchain's
/// first present because only then is the present queue known. A queue of another
/// family later gets `None`: a guest image is owned by its present queue's family
/// and these buffers can only run there.
unsafe fn spare_copy_state(sc: *mut GnSwapchain, family: u32) -> Option<*const SpareCopy> {
    if let Some(copy) = unsafe { (*sc).spare.as_ref() } {
        return (copy.family == family).then_some(copy as *const SpareCopy);
    }
    let gd = unsafe { (*sc).dev };
    let f = unsafe { (*gd).f };
    let begin = unsafe { gn_real_device_proc(gd, c"vkBeginCommandBuffer") }?;
    let end = unsafe { gn_real_device_proc(gd, c"vkEndCommandBuffer") }?;
    let mut pci: VkCommandPoolCreateInfo = unsafe { std::mem::zeroed() };
    pci.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO;
    pci.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
    pci.queueFamilyIndex = family;
    let mut pool: VkCommandPool = 0;
    if unsafe { f.create_cmd_pool?((*gd).real, &pci, ptr::null(), &mut pool) } != VK_SUCCESS {
        return None;
    }
    let count = unsafe { (*sc).guest_slots };
    let mut cmds = vec![ptr::null_mut(); count as usize];
    let mut cbi: VkCommandBufferAllocateInfo = unsafe { std::mem::zeroed() };
    cbi.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO;
    cbi.commandPool = pool;
    cbi.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    cbi.commandBufferCount = count;
    if unsafe { f.alloc_cmd?((*gd).real, &cbi, cmds.as_mut_ptr()) } != VK_SUCCESS {
        if let Some(destroy) = f.destroy_cmd_pool {
            unsafe { destroy((*gd).real, pool, ptr::null()) };
        }
        return None;
    }
    let copy = SpareCopy {
        pool,
        family,
        cmds,
        begin: unsafe { std::mem::transmute(begin) },
        end: unsafe { std::mem::transmute(end) },
    };
    Some(unsafe { (*sc).spare.insert(copy) } as *const SpareCopy)
}

/// Record the copy of a presented guest image to a hidden slot. It rides on the
/// present's own submit, after the guest's wait semaphores, so `cmd` runs with
/// the frame finished. Everything the guest submitted earlier without a semaphore
/// is covered by the first barrier's source stage.
unsafe fn spare_record(
    sc: *mut GnSwapchain,
    copy: *const SpareCopy,
    cmd: VkCommandBuffer,
    guest: VkImage,
    hidden: VkImage,
) -> bool {
    let f = unsafe { (*(*sc).dev).f };
    let (Some(barrier), Some(copy_image)) = (f.cmd_pipeline_barrier, f.cmd_copy_image) else {
        return false;
    };
    let mut bi: VkCommandBufferBeginInfo = unsafe { std::mem::zeroed() };
    bi.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
    bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    if unsafe { ((*copy).begin)(cmd, &bi) } != VK_SUCCESS {
        return false;
    }
    let before = [
        lsfg_barrier(
            guest,
            VK_ACCESS_MEMORY_WRITE_BIT,
            VK_ACCESS_TRANSFER_READ_BIT,
            VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
            VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
        ),
        lsfg_barrier(
            hidden,
            0,
            VK_ACCESS_TRANSFER_WRITE_BIT,
            VK_IMAGE_LAYOUT_UNDEFINED,
            VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
        ),
    ];
    unsafe {
        barrier(
            cmd,
            VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
            VK_PIPELINE_STAGE_TRANSFER_BIT,
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            2,
            before.as_ptr(),
        )
    };
    let mut region: VkImageCopy = unsafe { std::mem::zeroed() };
    region.srcSubresource.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
    region.srcSubresource.layerCount = 1;
    region.dstSubresource = region.srcSubresource;
    region.extent = VkExtent3D {
        width: unsafe { (*sc).extent.width },
        height: unsafe { (*sc).extent.height },
        depth: 1,
    };
    unsafe {
        copy_image(
            cmd,
            guest,
            VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
            hidden,
            VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            1,
            &region,
        )
    };
    let after = [
        lsfg_barrier(
            guest,
            0,
            0,
            VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
            VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
        ),
        lsfg_barrier(
            hidden,
            VK_ACCESS_TRANSFER_WRITE_BIT,
            0,
            VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
        ),
    ];
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
            2,
            after.as_ptr(),
        )
    };
    unsafe { ((*copy).end)(cmd) == VK_SUCCESS }
}

/// Claim a hidden slot for the frame in guest slot `idx` and record its copy, the
/// command buffer for the present's submit. Null when the frame goes out directly
/// on the guest's image: the slot then returns on the compositor's release, so a
/// compositor holding every hidden slot makes the guest wait at its next acquire,
/// as it would with no hidden slots at all, and no frame is lost.
unsafe fn spare_begin(
    sc: *mut GnSwapchain,
    slot: *mut GnSlot,
    idx: usize,
    queue: *mut GnQueue,
) -> VkCommandBuffer {
    unsafe { (*slot).spare = NO_SPARE };
    if !unsafe { spare_in_use(sc) } || queue.is_null() {
        return ptr::null_mut();
    }
    let Some(copy) = (unsafe { spare_copy_state(sc, (*queue).family) }) else {
        return ptr::null_mut();
    };
    let first = unsafe { (*sc).guest_slots as usize };
    let end = first + unsafe { (*sc).spare_slots } as usize;
    let gd = unsafe { (*sc).dev };
    // A batch generation has queued will be written into its slots; after the
    // user turns generation off mid-session the copy path comes live at once,
    // and picking one of those slots would tear the frame it is about to draw.
    // The queue belongs to a swapchain, so only this one's entries count.
    let gen_queued = unsafe { (*gd).gen_sc == sc };
    let hidden = {
        let _g = unsafe { (*sc).shared.lock().unwrap() };
        let free = (first..end).find(|&i| {
            let s = unsafe { (*sc).slots.as_ptr().add(i) };
            !unsafe { (*s).in_flight }
                && !(gen_queued
                    && unsafe { (*gd).gen_due.iter().any(|d| d.slot as usize == i) })
        });
        if let Some(i) = free {
            unsafe { (*(*sc).slots.as_mut_ptr().add(i)).in_flight = true };
        }
        free
    };
    let Some(hidden) = hidden else {
        return ptr::null_mut();
    };
    let cmd = unsafe { (&(*copy).cmds)[idx] };
    let hidden_image = unsafe { (*(*sc).slots.as_ptr().add(hidden)).image };
    if !unsafe { spare_record(sc, copy, cmd, (*slot).image, hidden_image) } {
        unsafe { spare_unclaim(sc, hidden) };
        return ptr::null_mut();
    }
    unsafe { (*slot).spare = hidden as i32 };
    cmd
}

unsafe fn spare_unclaim(sc: *mut GnSwapchain, hidden: usize) {
    let _g = unsafe { (*sc).shared.lock().unwrap() };
    unsafe {
        (*(*sc).slots.as_mut_ptr().add(hidden)).in_flight = false;
        (*sc).cv_free.notify_all();
    }
}

/// The copy in guest slot `idx` has finished: the guest gets its image back and
/// the frame's identity moves to the hidden slot, which is what the compositor
/// will see and release. Returns that slot.
unsafe fn spare_handover(sc: *mut GnSwapchain, idx: usize) -> usize {
    let slots = unsafe { (*sc).slots.as_mut_ptr() };
    let guest = unsafe { slots.add(idx) };
    let g = unsafe { (*sc).shared.lock().unwrap() };
    let hidden = unsafe { core::mem::replace(&mut (*guest).spare, NO_SPARE) } as usize;
    let dest = unsafe { slots.add(hidden) };
    let stale_id = unsafe {
        (*dest).present_id = core::mem::replace(&mut (*guest).present_id, 0);
        (*dest).acquired_ns = core::mem::replace(&mut (*guest).acquired_ns, 0);
        (*guest).in_flight = false;
        (*sc).cv_free.notify_all();
        core::mem::replace(&mut (*dest).wait_id, core::mem::replace(&mut (*guest).wait_id, 0))
    };
    drop(g);
    // Left by a frame on this slot that could not be committed.
    if stale_id != 0 {
        crate::present::discard(stale_id, sc as usize, monotonic_ns());
    }
    hidden
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
/// No drought counter: parking is unconditional for a synced mode, and
/// [`slot_just_freed`]'s bound is what stops a missing note from stranding a
/// slot. A heuristic on the parks-since-the-last-note was tried and removed —
/// parks arrive per release event while notes are drained per poll, so nine
/// releases can legitimately precede one note. It tripped at `notes=5754` on a
/// channel that went on to deliver 9 811 (`rnotes=9811`), and being sticky it
/// turned the tear fix off for the rest of the session: `park=off`,
/// `acq_fps=64.33` on a 60 Hz panel, visible tearing.
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

/// How long the present worker waits on a guest frame's fence between looks at
/// the queued frames, while any are queued.
const GEN_WAIT_SLICE_NS: u64 = 1_000_000;

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
    // One batch per source frame. The guest's presents are not the cap — a
    // mailbox guest presents again as soon as a buffer frees — so a batch per
    // present would generate for frames that were never acquired and put the
    // output above `cap × multiplier`.
    let acquired = ACQ_COUNT.load(Ordering::Relaxed);
    if acquired == unsafe { (*gd).gen_last_acq } {
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
    let (guest_w, guest_h) = unsafe {
        (
            (*gd).guest_peak.0.load(Ordering::Relaxed),
            (*gd).guest_peak.1.load(Ordering::Relaxed),
        )
    };
    if guest_w != 0 && guest_h != 0 {
        unsafe { (*session).set_guest_extent(guest_w, guest_h) };
    }
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
    // The engine plans by the multiplier; the session's generation limit is a
    // rate, so a panel that cannot show `acquire × multiplier` frames leaves
    // fewer per source frame ([`crate::limits::Limits::generate_per_source`]).
    let planned = unsafe { (*session).plan(capacity) };
    let allowed = session_limits(sc).generate_per_source(multiplier);
    let planned = match allowed {
        Some(per) => planned.min(per),
        None => 0,
    };
    let guest = unsafe { gen_first_slot(sc) };
    // A destination is free once the compositor has released it — or once its
    // note is overdue — and nothing queued still points at it. The unpark check
    // is the same one the guest's acquire uses: this path had none, so every
    // destination it parked left the ring for good (`slots=[5/5 parked=5]`).
    let mut free: Vec<usize> = (guest..unsafe { (*sc).slots.len() })
        .filter(|&i| {
            let s = unsafe { (*sc).slots.as_mut_ptr().add(i) };
            let landed = unsafe { !(*s).in_flight || slot_just_freed(s, monotonic_ns()) };
            landed
                && !unsafe { (*gd).gen_due.iter().any(|d| d.slot as usize == i) }
        })
        .collect();
    // Identity found too few: spend notes by order and look again, so a device
    // whose notes name another buffer still gets its destinations back.
    if free.len() < planned as usize
        && unsafe { answer_park_from_notes(sc, guest, (*sc).slots.len(), false) } >= 0
    {
        free = (guest..unsafe { (*sc).slots.len() })
            .filter(|&i| {
                let s = unsafe { (*sc).slots.as_mut_ptr().add(i) };
                let landed = unsafe { !(*s).in_flight || slot_just_freed(s, monotonic_ns()) };
                landed && !unsafe { (*gd).gen_due.iter().any(|d| d.slot as usize == i) }
            })
            .collect();
    }
    if free.len() < planned as usize {
        GEN_SHORT_FREE.fetch_add(1, Ordering::Relaxed);
        // The compositor returns buffers later than the ring assumed: add what
        // this batch is short, within the cap. A new slot is free by definition.
        let total = unsafe { (*sc).slots.len() } - guest;
        let cap = gen_destination_cap(capacity);
        let grow = (planned as usize - free.len()).min(cap.saturating_sub(total));
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
        // session's acquire limit, or the measured one when the guest runs
        // slower than that limit.
        let cap_period = match session_limits(sc).acquire_per_s {
            Some(acquire) if acquire > 0 => 1_000_000_000u64 / u64::from(acquire),
            _ => 0,
        };
        let source_period = match (cap_period, period) {
            (0, 0) => 16_666_667,
            (c, 0) => c,
            (0, p) => p,
            (c, p) => c.max(p),
        };
        // On the panel's latch grid when the batch fits it: the frames take
        // consecutive latch points from the one after the previous queued
        // frame's (or the first a commit can still reach, when that has
        // passed), so one frame lands per panel period and none replaces
        // another unshown. A batch with more frames than the panel has points
        // in a source period goes out on this clock spread over the period
        // instead: on the grid the source frame would follow the batch's last
        // point, the queue would carry a period per extra frame and the
        // multiplier the user asked for would never arrive. What the panel
        // cannot latch is the compositor's to drop.
        let grid = crate::lsfg::latch_grid().filter(|g| {
            g.period_ns == 0 || (built as u64 + 1) * g.period_ns <= source_period
        });
        let step = source_period / (built as u64 + 1);
        let base = now.max(unsafe { (*gd).gen_last_deadline });
        let first_latch = grid.map(|g| g.first_latch(now, unsafe { (*gd).gen_last_latch_ns }));
        let place = |k: u64| -> (u64, u64) {
            match (grid, first_latch) {
                (Some(g), Some(first)) => {
                    let latch = first + g.period_ns * (k - 1);
                    (g.commit_for(latch), latch)
                }
                _ => (base + step * k, 0),
            }
        };
        for (k, index) in destinations.iter().take(built).enumerate() {
            let (deadline, _) = place(k as u64 + 1);
            unsafe {
                // Taking a destination clears its park as well as its flight:
                // `slot_just_freed` says the display is done with it, and the
                // generator is about to write the whole image again.
                let s = (*sc).slots.as_mut_ptr().add(*index);
                (*s).parked_ns = 0;
                (*s).in_flight = true;
                (*gd).gen_due.push_back(GenDue {
                    deadline,
                    slot: *index as i32,
                    batch,
                    drop_at: deadline + GEN_GRACE_NS,
                })
            };
        }
        GEN_QUEUED.fetch_add(built as u32, Ordering::Relaxed);
        let (real_at, real_latch) = place(built as u64 + 1);
        unsafe {
            (*gd).gen_due.push_back(GenDue {
                deadline: real_at,
                slot: src_idx,
                batch: GEN_NO_BATCH,
                drop_at: u64::MAX,
            });
            (*gd).gen_last_deadline = real_at;
            (*gd).gen_last_latch_ns = real_latch;
            (*gd).gen_last_acq = acquired;
        }
        queued = true;
    }
    // Health, not a trace: one line every 600 batches.
    static BATCHES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if BATCHES
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .is_multiple_of(600)
    {
        crate::logf!("lsfg: generated {} (skips {}, queued {}, short: free {} plan {}, late {}, released {})",
            GEN_COMMITS.load(std::sync::atomic::Ordering::Relaxed),
            GEN_SKIPS.load(std::sync::atomic::Ordering::Relaxed),
            GEN_QUEUED.load(std::sync::atomic::Ordering::Relaxed),
            GEN_SHORT_FREE.load(std::sync::atomic::Ordering::Relaxed),
            GEN_SHORT_PLAN.load(std::sync::atomic::Ordering::Relaxed),
            GEN_LATE.load(std::sync::atomic::Ordering::Relaxed),
            GEN_RELEASES.load(std::sync::atomic::Ordering::Relaxed));
    }
    queued
}

/// Queue a source frame that got no generations of its own behind the frames
/// already queued, on its own latch point. Returns whether it was queued; with
/// nothing queued the caller commits it inline, which is the same order.
///
/// Committed inline while frames are still queued, it would show before the
/// previous source frame and that frame's generations: time would run backwards
/// on the panel for a few frames every time a batch is skipped.
unsafe fn lsfg_queue_real(sc: *mut GnSwapchain, src_idx: i32) -> bool {
    let gd = unsafe { (*sc).dev };
    if unsafe { (*gd).gen_sc } != sc || unsafe { (*gd).gen_due.is_empty() } {
        return false;
    }
    let now = monotonic_ns();
    let grid = crate::lsfg::latch_grid();
    // One source period on this clock: the cap's, else the measured one, else
    // the panel's.
    let source_period = {
        let measured = unsafe { (*gd).gen_period_ns };
        match session_limits(sc).acquire_per_s {
            Some(cap) if cap > 0 => (1_000_000_000 / u64::from(cap)).max(measured),
            _ if measured != 0 => measured,
            _ => grid.map_or(16_666_667, |g| g.period_ns),
        }
    };
    // A source frame already waiting that is not due for a source period yet:
    // the guest is ahead of the output, and queueing behind it would stack a
    // frame of latency per presented frame, up to the guest's slots. That frame
    // has not been shown and this one is newer, so this one takes its place and
    // its slot goes straight back to the guest.
    if let Some(tail) = unsafe { (*gd).gen_due.back_mut() } {
        if tail.batch == GEN_NO_BATCH && tail.deadline > now + source_period {
            let superseded = tail.slot;
            tail.slot = src_idx;
            GEN_SKIPS.fetch_add(1, Ordering::Relaxed);
            unsafe {
                let s = (*sc).slots.as_mut_ptr().add(superseded as usize);
                let _g = (*sc).shared.lock().unwrap();
                (*s).in_flight = false;
                (*sc).cv_free.notify_all();
            }
            return true;
        }
    }
    let prev_latch = unsafe { (*gd).gen_last_latch_ns };
    let (deadline, latch) = match grid {
        Some(grid) => {
            let latch = grid.first_latch(now, prev_latch);
            (grid.commit_for(latch), latch)
        }
        // One output step after the last queued frame, like the frames of a
        // batch, so the two are not composited as one.
        None => {
            let (_, multiplier, _) = crate::lsfg::live();
            let step = source_period / u64::from(multiplier.max(1));
            (
                now.max(unsafe { (*gd).gen_last_deadline }.saturating_add(step)),
                prev_latch,
            )
        }
    };
    unsafe {
        (*gd).gen_due.push_back(GenDue {
            deadline,
            slot: src_idx,
            batch: GEN_NO_BATCH,
            drop_at: u64::MAX,
        });
        (*gd).gen_last_deadline = deadline;
        (*gd).gen_last_latch_ns = latch;
    }
    true
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
            // `commit_generated` counts the frame itself, one message per frame,
            // so the batch adds nothing here: a batch-level total would be a
            // second, duplicate report for every frame it contains.
            unsafe { commit_generated(sc, slot as usize) };
        }
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
                crate::logf!("gn-vkshim idle: present worker has no work {}",
                    slot_flags(sc));
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
                // In slices while frames are queued: a queued frame that falls
                // due goes out on time whatever this frame's GPU time, instead
                // of waiting behind its fence.
                let gd = (*sc).dev;
                loop {
                    let queued = (*gd).gen_sc == sc && !(*gd).gen_due.is_empty();
                    let timeout = if queued { GEN_WAIT_SLICE_NS } else { u64::MAX };
                    if wait_fences((*gd).real, 1, &(*s).fence, VK_TRUE, timeout) != VK_TIMEOUT {
                        break;
                    }
                    flush_due(sc);
                }
            }
            let fence_ms = t0.elapsed().as_millis();
            if (*s).spare != NO_SPARE {
                let hidden = spare_handover(sc, idx as usize);
                commit_real(sc, hidden as i32, fence_ms);
                continue;
            }
            // Before the commit, so the capture reads the frame the guest just
            // finished while the slot is still ours. When the frame's generations
            // were queued, the frame itself is queued behind them; a frame that
            // got none still queues behind whatever is waiting, so frames go out
            // in the order they were presented.
            if lsfg_generate(sc, idx) || lsfg_queue_real(sc, idx) {
                continue;
            }
            commit_real(sc, idx, fence_ms);
        }
    }
}

/// Binds the swapchain's override surface to its X window, once per swapchain,
/// right before its first commit. gamescope keeps one surface per window, so a
/// swapchain that never presents (the Epic overlay's Present probe on a child
/// of the game's window) must not take the window from the one that does.
/// Called with `SEND_LOCK` held.
unsafe fn bind_window_at_first_commit(sc: *mut GnSwapchain) {
    unsafe {
        let surf = (*sc).surf;
        if (*sc).window_bound || (*surf).swapchain_obj.is_null() {
            return;
        }
        (*sc).window_bound = true;
        // The resolution at surface creation had to read a tree the guest had
        // not drawn into yet. On the fallback path that answer is a guess about
        // which window is the game's, and this is the first instant there is
        // evidence: the swapchain it is on is committing a frame. `wine
        // explorer /desktop=shell` never takes the fallback (its root child is
        // the full-size desktop window), so this costs one X round trip only on
        // the guests that needed the fallback in the first place.
        if (*surf).window_provisional {
            let resolved = crate::x11::resolve_override_target((*surf).named_window);
            if resolved.window != (*surf).window {
                crate::logf!("override: XID {:#x} re-resolved at first commit: {:#x} -> {:#x}",
                    (*surf).named_window,
                    (*surf).window,
                    resolved.window);
            }
            (*surf).window = resolved.window;
            (*surf).window_provisional = resolved.provisional;
        }
        // Single xwayland server (--xwayland-count 1) is always id 0.
        wlproto::swapchain_override_window_content((*surf).swapchain_obj, 0, (*surf).window);
        crate::logf!("override: swapchain {sc:?} binds surface {surf:?} to XID {:#x} at its first commit",
            (*surf).window);
    }
}

/// Commit a guest frame: into its X11 window on that route, otherwise attached
/// to the surface and committed. The slot's own fence has already been waited.
unsafe fn commit_real(sc: *mut GnSwapchain, idx: i32, fence_ms: u128) {
    unsafe {
        let s = (*sc).slots.as_mut_ptr().add(idx as usize);
        let now = monotonic_ns();
        // The instant a release note for this frame will be about. Set here, not
        // at the park: the note can precede the park (see `commit_ns`).
        (*s).commit_ns = now;
        let last = LAST_COMMIT_NS.swap(now, Ordering::Relaxed);
        if last != 0 && now > last {
            let gap = now - last;
            COMMIT_GAPS[commit_spacing_bucket(gap)].fetch_add(1, Ordering::Relaxed);
            let est = COMMIT_SPACING_NS.load(Ordering::Relaxed);
            COMMIT_SPACING_NS.store(
                crate::vsync_gate::smooth_latency(est, gap),
                Ordering::Relaxed,
            );
        }
        // How long the frame took from the gate's release to here, its render
        // fence waited, for the census.
        if (*s).acquired_ns != 0 {
            let took = monotonic_ns().saturating_sub(core::mem::replace(&mut (*s).acquired_ns, 0));
            let est = (*sc).commit_latency_ns.load(Ordering::Relaxed);
            (*sc).commit_latency_ns.store(
                crate::vsync_gate::smooth_latency(est, took),
                Ordering::Relaxed,
            );
        }
        // Tell the daemon which present id this frame carries, before the commit
        // that names its buffer in an atomic commit. The worker may wait for an
        // allocation exchange in flight, which is bounded by that exchange's own
        // timeout; a mark that never arrives would leave a wait hanging.
        // Both ids this commit acts on are read here, once: `send_present_mark`
        // below is a blocking send, and the release handler on the dispatch
        // thread writes `wait_id` while it blocks.
        let presented_id = (*s).present_id;
        let wait_id = {
            let _g = (*sc).shared.lock().unwrap();
            (*s).wait_id
        };
        // This frame's present wait, registered before anything that can block.
        // The bound that answers a wait nobody else answers is measured from
        // this commit instant, so registering after the mark would put the whole
        // mark's latency into every frame the guest is waiting on.
        let mut feedback_token = 0u64;
        if wait_id != 0 {
            // A guest that asked for mailbox or immediate does not wait, and
            // neither would one whose frames go out as mailbox or immediate
            // (the container's mode, or frame generation): both are answered
            // here. A FIFO guest declared FIFO keeps the release-or-grace answer.
            // A cap-rate-vsync frame's wait was answered when the guest queued
            // it (`present_dispatch`), so one still on the slot here is a frame
            // whose declaration changed since; it is answered now.
            let (live, _why) = live_choice(sc);
            let at_commit = present_wait_answer(
                !(*(*sc).surf).swapchain_obj.is_null(),
                acquire_pacing(sc) != crate::vsync_gate::Pacing::Off,
                (*sc).present_mode,
                live,
            ) != WaitAnswer::Later;
            if at_commit {
                let _g = (*sc).shared.lock().unwrap();
                (*s).wait_id = 0;
                (*s).wait_registered = false;
            } else {
                // A FIFO wait on the override route is answered by gamescope's
                // presentation feedback, as in Mesa's Wayland WSI, and by the
                // grace behind it. Marked registered first, so the release that
                // follows the commit may answer it and the one that preceded
                // the commit could not.
                let _g = (*sc).shared.lock().unwrap();
                (*s).wait_registered = true;
                drop(_g);
                if !(*(*sc).surf).swapchain_obj.is_null()
                    && !PRESENTATION.load(Ordering::Acquire).is_null()
                {
                    feedback_token = crate::present::expect_feedback(wait_id, sc as usize);
                }
            }
            crate::present::committed(wait_id, sc as usize, now, at_commit);
        }
        if presented_id != 0 && wait_id == 0 {
            // The present sets both ids, and only a commit takes them again
            // (`at_commit`) or a re-present of the slot discards them — so a
            // frame that arrives here with a present id and no wait id lost it
            // on the way. Counted because the loss leaves its wait with no
            // commit to bound it, which is what made the compositor's feedback
            // the only answer: 2975 of 3407 waits before this was fixed.
            NOID_AT_COMMIT.fetch_add(1, Ordering::Relaxed);
        }
        if (*s).present_id != 0 && (*s).ino != 0 {
            crate::scanout::send_present_mark((*s).ino, (*s).present_id);
            // One mark per frame: a guest that stops chaining ids must not keep
            // marking later frames with the last one it sent.
            (*s).present_id = 0;
        }
        // The frame goes to the compositor now. Under FIFO a wait on it is
        // answered at the latest when the refresh after this commit has
        // passed: a frame that nothing replaces is never released, and the
        // guest may wait on it before presenting again (vkd3d-proton does, on
        // its first frame). Its wait was registered above, and the feedback
        // request a `feedback_token` names rides the commit below.
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
                x11_reap(sc);
            } else {
                if !X11_DROP_LOGGED.swap(true, Ordering::Relaxed) {
                    crate::logf!("x11 route: present unavailable - frames dropped, slots kept free (guest keeps running)");
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
            crate::logf!("proxy: wl_buffer creation failed — dropping frame");
            crate::present::cancel_feedback(feedback_token);
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
            // The guest-surface route has no gamescope swapchain to declare to.
            if !(*(*sc).surf).swapchain_obj.is_null() {
                wlproto::swapchain_set_present_mode(
                    (*(*sc).surf).swapchain_obj,
                    gamescope_present_mode(declared_present_mode(sc)),
                );
            }
            bind_window_at_first_commit(sc);
            wlproto::surface_attach(parent, (*s).buf, 0, 0);
            wlproto::surface_damage_buffer(
                parent,
                0,
                0,
                (*sc).extent.width as i32,
                (*sc).extent.height as i32,
            );
            if feedback_token != 0 {
                let fb = wlproto::presentation_feedback(
                    PRESENTATION.load(Ordering::Acquire),
                    parent,
                );
                if fb.is_null() {
                    crate::present::cancel_feedback(feedback_token);
                } else {
                    // The token rides as the listener's data: no allocation
                    // to free when the event spends the object.
                    wlproto::wl_proxy_add_listener(
                        fb,
                        FEEDBACK_LISTENER.as_ptr() as *mut _,
                        feedback_token as usize as *mut c_void,
                    );
                }
            }
            wlproto::surface_commit(parent);
            wlproto::wl_display_flush((*(*sc).surf).dpy)
        };
        let commit_ms = t1.elapsed().as_millis();
        let n = LOG_COMMIT.fetch_add(1, Ordering::Relaxed);
        let done = N_COMMIT_OK.fetch_add(1, Ordering::Relaxed);
        if n < LOG_FIRST_N || n.is_multiple_of(120) || fence_ms >= 100 {
            crate::logf!("proxy commit: sc={sc:?} idx={idx} {}x{} flush_rc={flush_rc} dpy_err={} fence={fence_ms}ms commit={commit_ms}ms #{} {}",
                (*sc).extent.width,
                (*sc).extent.height,
                wlproto::wl_display_get_error((*(*sc).surf).dpy),
                done + 1,
                counters(),);
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
    crate::logf!("proxy swapchain create: device={device:?} surface={:#x} {}x{} fmt={} cs={} min={} mode={} old={:#x} -> gd={:?} surf={:?}",
        ci.surface,
        ci.imageExtent.width,
        ci.imageExtent.height,
        ci.imageFormat,
        ci.imageColorSpace,
        ci.minImageCount,
        ci.presentMode,
        ci.oldSwapchain as usize,
        gd,
        surf);
    if gd.is_null() || surf.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // `VK_KHR_swapchain_maintenance1` creation structs (`crate::maint1`): the
    // modes list is accepted (a switch to another listed mode is not served
    // yet, see the module's TODO), the scaling struct too (no scaling is
    // offered, so a conforming guest passes zeros), and the deferred-allocation
    // bit is a hint the shim ignores: every slot is allocated here.
    {
        let modes = unsafe { crate::maint1::create_modes(ci.pNext) };
        let scaling = unsafe { crate::maint1::create_scaling(ci.pNext) };
        let deferred = ci.flags & crate::maint1::CREATE_DEFERRED_MEMORY_ALLOCATION_BIT != 0;
        if modes.is_some() || scaling.is_some() || deferred {
            crate::logf!("proxy swapchain create: maintenance1 modes={modes:?} scaling={scaling:?} deferred_alloc={deferred}");
        }
    }
    let fourcc = vk_format_to_fourcc(ci.imageFormat);
    if fourcc == 0 {
        crate::logf!("proxy: unsupported swapchain format {}",
            ci.imageFormat);
        return VK_ERROR_FORMAT_NOT_SUPPORTED;
    }
    // Only the scan-out pool's own layout can be served end to end (see
    // `allows_other_eight_bit`): the guest often renders straight into a
    // daemon-owned buffer, so a swapchain in the other channel order would
    // publish R/B-exchanged pixels. Refuse it loudly rather than render wrong
    // colours.
    let layout = crate::scanout::layout();
    if !swapchain_format_allowed(ci.imageFormat, layout, allows_other_eight_bit(layout)) {
        crate::logf!("proxy: refusing swapchain format {} cs={} (scan-out pool is format {}; set {} to opt in)",
            ci.imageFormat,
            ci.imageColorSpace,
            scanout_swapchain_format(layout),
            other_eight_bit_env(layout));
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
            crate::logf!("proxy: no gamescope_swapchain to declare format {} colourspace {} on",
                ci.imageFormat,
                colorspace);
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
            crate::logf!("proxy: declared swapchain format {} colourspace {} to gamescope",
                ci.imageFormat,
                colorspace);
        }
    }
    // The guest's requested mode is the one this swapchain keeps; what each
    // commit declares is decided per present (`live_present_mode`).
    let gamescope_route = unsafe { !(*surf).swapchain_obj.is_null() };
    let mode = guest_present_mode(ci);
    // The count is the guest's, on Mesa's convention: the capabilities carry
    // the depth ([`SC_DEFAULT_IMAGES`]) and creation honours the ask, with the
    // mailbox floor as the only bump ([`swapchain_slots`]). What a commit
    // declares per present (`live_present_mode`) is pacing and does not move
    // the count, so there is nothing to resolve here about the container's
    // mode: a swapchain made at one count keeps it until the guest recreates it.
    //
    // A game on Mesa's strict list gets exactly its ask and no bump at all —
    // the mailbox floor included — because any image it did not ask for is what
    // it fails on (`crate::image_count`).
    let strict_count = crate::image_count::strict();
    let ceiling = max_swapchain_images();
    let mut n_slots = if strict_count {
        ci.minImageCount.clamp(2, ceiling.max(2))
    } else {
        swapchain_slots(ci.minImageCount, ceiling, count_present_mode(mode))
    };
    // Frame generation needs the source, its destination and the one the
    // compositor holds. This floor is not what sizes the generation ring — that
    // is `lsfg_destinations`, grown behind the guest's images — and a run with 4
    // here generated exactly as many frames as one with 3. The starvation it was
    // meant to fix measured `slots=[5/5 parked=5]` with `notes=1`: every
    // destination held for a note that never came. Never for a strict game, which
    // cannot be given a slot it did not ask for.
    if !strict_count && crate::lsfg::live().0 {
        n_slots = n_slots.max(SC_FRAMEGEN_IMAGES.min(ceiling.max(2)));
    }
    // The count is decided once, here, and cannot follow the settings afterwards:
    // the app's frame can arrive after the swapchain exists. Printed at the
    // default level because the session's whole depth follows from it — a session
    // whose settings came late ran on three images and could not reach its cap,
    // and the only way to see that was to subtract the generation ring from a
    // slot count in the census.
    crate::logi!(
        "proxy swapchain create: {} images for asked={} ceiling={} declared={} counted={} live={} generating={} strict={}",
        n_slots,
        ci.minImageCount,
        ceiling,
        mode,
        count_present_mode(mode),
        crate::lsfg::live().0,
        crate::lsfg::wanted().is_some(),
        strict_count
    );
    if strict_count {
        crate::logi!("proxy swapchain create: strict image count: requested {} gives {n_slots} images, mode={}",
            ci.minImageCount,
            ci.presentMode);
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
        spare_slots: 0,
        spare: None,
        gen_no_grow: false,
        release: crate::vsync_gate::Schedule::default(),
        commit_latency_ns: AtomicU64::new(0),
        worker: None,
        shared: Mutex::new(ScShared {
            jobs: VecDeque::with_capacity(GN_JOB_CAP),
            mailbox: None,
            stopping: false,
        }),
        cv_put: Condvar::new(),
        cv_free: Condvar::new(),
        x11_trace: 0,
        window_bound: false,
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
            present_id: 0,
            wait_id: 0,
            wait_registered: false,
            fence: 0,
            sem: 0,
            fence_sem: 0,
            acquired: false,
            in_flight: false,
            parked_ns: 0,
                commit_ns: 0,
            acq_seq: 0,
            acquired_ns: 0,
            ino: 0,
            ahb: ptr::null_mut(),
            spare: NO_SPARE,
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
            crate::logf!("proxy: slot init failed: {res}");
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
    // Hidden slots only on gamescope's route, the one where a slot comes back on
    // the compositor's release. The guest never sees them (`spare_begin`).
    let hidden = if strict_count && gamescope_route {
        crate::image_count::hidden_slot_count(n_slots) as usize
    } else {
        0
    };
    let capacity = crate::lsfg::wanted().unwrap_or(1).saturating_sub(1);
    let dest_cap = if capacity == 0 {
        0
    } else {
        gen_destination_cap(capacity)
    };
    // Both consumers take their images from the same ring past the guest's own
    // ([`gen_first_slot`]), so it is sized for the larger of them and not their
    // sum. The reserve covers the growth the destination allocator may ask for
    // later: pushing past the vector's capacity would move every slot while the
    // compositor holds raw pointers into them (see the note above).
    let ring = dest_ring(hidden, dest_cap);
    sc.slots.reserve(ring.max(gen_destination_cap(3)));
    if ring > 0 {
        unsafe { lsfg_destinations(scp, ring) };
    }
    sc.spare_slots = hidden as u32;
    if hidden != 0 {
        crate::logi!("proxy swapchain create: strict image count: {} images, {} of {} ring slot(s) for hidden copies",
            n_slots,
            hidden,
            ring);
    }

    // What the guest was told the surface can be, next to what the slot images
    // actually are. One line, once: a disagreement here names a guest that
    // sized its viewport to a different image than the shim renders into.
    if !CAPS_COMPARE_LOGGED.swap(true, Ordering::Relaxed) {
        if let Ok(adv) = ADVERTISED_CAPS.lock() {
            if let Some(a) = *adv {
                let stride = sc.slots.first().map_or(0, |s| s.stride);
                crate::logf!("x11 caps vs slot: advertised current={}x{} min={}x{} max={}x{} -> image {}x{} stride={} slots={}",
                    a.current.0,
                    a.current.1,
                    a.min.0,
                    a.min.1,
                    a.max.0,
                    a.max.1,
                    ci.imageExtent.width,
                    ci.imageExtent.height,
                    stride,
                    sc.slots.len());
            }
        }
    }
    let sc_addr = scp as usize;
    if !unsafe { (*(*scp).surf).x11 }.is_null() {
        crate::logf!("X11 present engages: swapchain {scp:?} {}x{} presents through DRI3 + Present",
            ci.imageExtent.width,
            ci.imageExtent.height);
    }
    let worker = std::thread::Builder::new()
        .name("gn-present".into())
        .spawn(move || worker_main(sc_addr as *mut GnSwapchain));
    let Ok(worker) = worker else {
        for j in 0..sc.slots.len() {
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
        crate::logf!("proxy swapchain {:?}: {}x{} fmt {} -> fourcc {:#x}, {} images; requested usage={:#x} minImages={} presentMode={} compositeAlpha={:#x} colorspace={} clipped={} tiling=LINEAR effective_mode={} gamescope_mode={}",
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
            gamescope_present_mode(live_present_mode(sc)));
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
            crate::logf!("x11 scimages: sc={sc:?} n={n} imgs={hs:?}");
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
                (*gd).gen_last_latch_ns = 0;
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
    // Ids armed on this swapchain can no longer complete: its buffers are not
    // coming back. A wait on one (vkd3d-proton waits on the last id of a chain
    // it has just replaced) would sit at the bound instead.
    crate::present::forget_swapchain(sc as usize);
    {
        // One line per destroyed swapchain: a swapchain that goes away while the
        // guest keeps rendering is otherwise invisible, and nothing downstream
        // can tell it from a guest that simply stopped presenting.
        crate::logf!("proxy swapchain destroyed: sc={sc:?} {}x{} presents={} commits={}",
            unsafe { (*sc).extent.width },
            unsafe { (*sc).extent.height },
            N_PRES.load(Ordering::Relaxed),
            N_COMMIT_OK.load(Ordering::Relaxed));
    }
    if let Some(worker) = unsafe { (*sc).worker.take() } {
        let _ = worker.join();
    }
    // The worker waited every queued frame's fence, so no copy is still running.
    if let Some(copy) = unsafe { (*sc).spare.take() } {
        let gd = unsafe { (*sc).dev };
        if let Some(destroy) = unsafe { (*gd).f.destroy_cmd_pool } {
            unsafe { destroy((*gd).real, copy.pool, ptr::null()) };
        }
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

/// How long a slot the compositor has released may wait for the daemon's own
/// release note before it is taken anyway, in nanoseconds.
///
/// The note is a fence signal, so it arrives in microseconds once SurfaceFlinger
/// releases the buffer. The bound exists for the case where it never does: the
/// daemon emits a note for a buffer that is *replaced* on the layer, so a buffer
/// its publish path coalesced away is never replaced and never gets one.
///
/// Two display periods, because that is the scale of the hold the note reports
/// (measured 1.5-2.5 periods). A longer bound is not a safety net, it is a
/// pacing rule: at 100 ms four slots cap the session at 40/s, and the guest's
/// own over-production is what decides how many of those it pays.
/// How long a parked slot waits for the daemon's release note before it is taken
/// anyway.
///
/// How long a parked slot waits for the daemon's release note before it is taken
/// anyway: two producer intervals, kept between two and four panel periods.
///
/// The producer interval is the right scale for the *hold* — a buffer stays on
/// the layer until the next frame replaces it, so a bound measured in panel
/// periods alone expires before every note for a slow producer (40 fps on 60 Hz
/// held ~66 ms against a 33 ms bound; `park_timeouts` read 12 203 against 3 623
/// slot waits).
///
/// It must not be the only scale, though, because the bound then depends on the
/// interval it governs. A session that has already slowed lengthens the hold that
/// keeps it slow — measured on Witcher 3 under Odin 3, where no note matched its
/// slot: 3 fps → 318 ms spacing → 636 ms bound → three images per 0.64 s → 3 fps.
/// Four panel periods is the ceiling; it is still twice the 1.5-2.5-period hold
/// SurfaceFlinger was measured to take, so a working note path never reaches it.
fn release_note_limit_ns() -> u64 {
    let hz = match crate::lsfg::panel_hz() {
        0 => 60,
        hz => u64::from(hz),
    };
    let period = 1_000_000_000 / hz;
    let floor = 2 * period;
    let ceiling = 4 * period;
    let spacing = COMMIT_SPACING_NS.load(Ordering::Relaxed);
    spacing.saturating_mul(2).clamp(floor, ceiling)
}

/// How long an acquire may sleep while a slot is parked on a release note.
///
/// The note is read by whoever next drains the channel, including this loop, so a
/// parked wait is a poll of the channel and not a sleep: a slice longer than the
/// note's own latency adds that latency to every frame the guest is waiting for.
/// At a 120 Hz acquire interval a 4 ms slice is half a frame, measured as a fifth
/// of the panel's frames repeated.
const PARKED_SLICE: Duration = Duration::from_micros(250);

/// Whether any slot of this swapchain is waiting on the daemon's release note.
unsafe fn any_parked(sc: *mut GnSwapchain) -> bool {
    for k in 0..unsafe { (*sc).guest_slots as usize } {
        let s = unsafe { (*sc).slots.as_ptr().add(k) };
        if unsafe { (*s).parked_ns != 0 } {
            return true;
        }
    }
    false
}

/// Claim the first swapchain slot that is neither held by the app nor awaiting a
/// compositor release. On success `idx` is set to the slot (marked acquired);
/// when none is free `idx` is left unchanged.
///
/// Withholding an in-flight slot is the reuse contract, not the cap: the cap is
/// applied by the caller's schedule before a slot is taken. Letting the app
/// acquire a slot the compositor still holds is not a safe way to decouple the
/// guest's rate: this scan hands out the *first* free slot, so the app takes slot
/// 0, presents it, re-acquires slot 0 and presents it
/// again — the compositor is handed the same buffer repeatedly and starves, which
/// measured as 8 commits against 5334 for the gated path on the same title. Any
/// free-running scheme needs a rotating slot choice (so each commit carries a
/// newer buffer) and its own back-pressure, not just this condition relaxed.
/// Free the oldest parked slot if a release note older than nothing is in hand.
///
/// The order fallback, for a device whose notes name a different buffer than the
/// one parked (see `scanout::take_any_released`). It lives here rather than in the
/// per-slot scan because that scan visits slots by index, not by age: with three
/// parks it would free slot 0 while the oldest frame sits on slot 2, and the note
/// would then be spent on the wrong frame.
///
/// Returns the slot freed, or -1. Called before the acquire scan and before the
/// destination search, so both consumers see the freed slot without knowing about
/// notes.
unsafe fn answer_park_from_notes(
    sc: *mut GnSwapchain,
    first: usize,
    end: usize,
    locked: bool,
) -> i32 {
    let mut oldest = -1i32;
    let mut oldest_commit = u64::MAX;
    {
        let slots = unsafe { (*sc).slots.as_ptr() };
        for k in first..end {
            let s = unsafe { slots.add(k) };
            let parked = unsafe { (*s).parked_ns };
            // Only a slot whose own identity did not match is a candidate: the
            // identity path has already had its turn.
            if parked == 0 {
                continue;
            }
            let commit = unsafe { (*s).commit_ns };
            if commit < oldest_commit {
                oldest_commit = commit;
                oldest = k as i32;
            }
        }
    }
    if oldest < 0 {
        return -1;
    }
    if !crate::scanout::take_any_released(oldest_commit) {
        return -1;
    }
    NOTES_APPLIED.fetch_add(1, Ordering::Relaxed);
    NOTES_BY_ORDER.fetch_add(1, Ordering::Relaxed);
    // `shared` is not reentrant: the acquire path already holds it, the present
    // worker's destination search does not. Taking it twice hangs the acquire
    // thread inside `vkAcquireNextImageKHR`, which is what a session reads as a
    // frozen counter (`acq=5` for ten seconds, guest waiting on a semaphore).
    let _g = if locked {
        None
    } else {
        Some(unsafe { (*sc).shared.lock().unwrap() })
    };
    let s = unsafe { (*sc).slots.as_mut_ptr().add(oldest as usize) };
    unsafe {
        (*s).parked_ns = 0;
        (*s).in_flight = false;
    }
    unsafe { (*sc).cv_free.notify_all() };
    drop(_g);
    oldest
}

/// Whether a slot that the compositor has let go may be used again.
///
/// A parked slot is one the compositor released but the *display* may still be
/// reading, so it waits for the daemon's note. That note must not be the only way
/// out: a note that never arrives strands the slot for the session. The measured
/// case is `slots=[5/5 parked=5] gendrel=6` with `notes=1` — five generation
/// destinations parked, one note ever, `generated 6` for a whole session while
/// `short: free` counted 4 790 refusals.
///
/// Shared by the guest's acquire and the generator's search for a destination.
/// It was inline in the first, and the second had no unpark path at all: the only
/// release check in the shim scanned `0..guest_slots`, so a parked destination was
/// never released, never timed out, and never counted — it just left the ring.
unsafe fn slot_just_freed(s: *mut GnSlot, now: u64) -> bool {
    let parked = unsafe { (*s).parked_ns };
    if parked == 0 {
        return false;
    }
    // A note describes the frame that was committed, and it can arrive before the
    // compositor's release that parks the slot. Anchoring on the commit is what
    // pairs them; anchoring on the park dropped the note for this very frame as
    // stale and left every slot on the bound.
    let since = unsafe { (*s).commit_ns }.min(parked);
    if unsafe { crate::scanout::take_released((*s).ino, since) } {
        NOTES_APPLIED.fetch_add(1, Ordering::Relaxed);
        let lag = now.saturating_sub(parked);
        NOTELAG[note_lag_bucket(lag)].fetch_add(1, Ordering::Relaxed);
        return true;
    }

    if now.saturating_sub(parked) >= release_note_limit_ns() {
        PARK_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        // Once per process: the slot's identity beside the notes the daemon sent.
        // A session that reads `park_timeouts == acquires` with `notelag` empty
        // needs this line to say whether the identities differ or the note list
        // was empty when the park expired.
        static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !REPORTED.swap(true, Ordering::Relaxed) {
            crate::logf!(
                "proxy park timeout: slot ino={} parked {} us ago; released=[{}]",
                unsafe { (*s).ino },
                now.saturating_sub(parked) / 1_000,
                crate::scanout::released_snapshot()
            );
        }
        return true;
    }
    false
}

/// Called with `shared` **held**: nothing reached from here may lock it again.
///
/// That is not a style rule — `std::sync::Mutex` is not reentrant, so a second
/// `lock()` on the same guard hangs the acquire thread inside
/// `vkAcquireNextImageKHR`. Measured: a session froze at `acq=5` with the guest
/// parked on a timeline semaphore and `park_timeouts=0`, because the order
/// fallback locked `shared` while the caller already held it.
unsafe fn acquire_take_free(sc: *mut GnSwapchain, idx: &mut i32) {
    // Identity first: a note that names this slot is the sharper pairing, and on a
    // device where inodes line up it is the only path that runs.
    unsafe { acquire_scan_identity(sc, idx) };
    if *idx >= 0 {
        return;
    }
    // Nothing matched by identity. A note that names no parked buffer may still
    // answer the oldest park, so spend one that way and look again.
    if unsafe { answer_park_from_notes(sc, 0, (*sc).guest_slots as usize, true) } >= 0 {
        unsafe { acquire_scan_identity(sc, idx) };
    }
}

unsafe fn acquire_scan_identity(sc: *mut GnSwapchain, idx: &mut i32) {
    let now = monotonic_ns();
    for k in 0..unsafe { (*sc).guest_slots as usize } {
        let s = unsafe { (*sc).slots.as_mut_ptr().add(k) };
        let released = unsafe { slot_just_freed(s, now) };
        let free = unsafe { !(*s).acquired && (!(*s).in_flight || released) };
        if free {
            *idx = k as i32;
            unsafe {
                (*s).acquired = true;
                (*s).in_flight = false;
                (*s).parked_ns = 0;
            }
            let seq = ACQ_SEQ.fetch_add(1, Ordering::Relaxed);
            unsafe {
                (*s).acq_seq = seq as u64;
            }
            if seq < X11_TRACE_N {
                crate::logf!("x11 acquire {seq}: sc={sc:?} idx={} img={:#x}",
                    *idx,
                    unsafe { (*s).image } as usize);
            }
            return;
        }
    }
}

/// What a wait ended as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waited {
    /// The span elapsed (or something woke it).
    Slept,
    /// The caller's own `timeout` ran out first.
    Expired,
}

/// Why an acquire is blocked, for the census.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Blocked {
    /// It was not blocked: the image was free and the schedule allowed it.
    None,
    /// The cap: the schedule had not reached this frame's release point.
    Rate,
    /// The pipeline: every image was held by the compositor.
    Slot,
}

/// The one sleep in the acquire: `ns` on the swapchain's free-slot condition, or
/// until the caller's `deadline` if that comes first.
///
/// Both conditions use it, so the deadline arithmetic exists once and a wake-up
/// can never be mistaken for a decision.
unsafe fn sleep_ns(
    sc: *mut GnSwapchain,
    g: MutexGuard<'_, ScShared>,
    ns: u64,
    deadline: Option<Instant>,
) -> (MutexGuard<'_, ScShared>, Waited) {
    let mut wait = Duration::from_nanos(ns);
    if let Some(dl) = deadline {
        let left = dl.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return (g, Waited::Expired);
        }
        wait = wait.min(left);
    }
    // SAFETY: `sc` is live for the call and `g` is the guard being released.
    let (guard, _) = unsafe { (*sc).cv_free.wait_timeout(g, wait) }.unwrap();
    (guard, Waited::Slept)
}

/// One acquired frame: the totals and the cadence the census reads.
fn note_acquire_granted(now_ns: u64) {
    ACQ_OK_N.fetch_add(1, Ordering::Relaxed);
    let _ = ACQ_FIRST_NS.compare_exchange(
        0,
        now_ns,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    let last = ACQ_LAST_NS.swap(now_ns, Ordering::Relaxed);
    if last == 0 {
        return;
    }
    let gap = now_ns.saturating_sub(last);
    let bucket = match gap {
        0..=13_999_999 => 0,
        14_000_000..=16_199_999 => 1,
        16_200_000..=17_199_999 => 2,
        17_200_000..=19_999_999 => 3,
        _ => 4,
    };
    ACQ_SPACING[bucket].fetch_add(1, Ordering::Relaxed);
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
    let _ = SHIM_T0_NS.compare_exchange(
        0,
        monotonic_ns(),
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    ring_push(
        "vkAcquireNextImageKHR",
        [timeout as usize, sem as usize, fence as usize, 0, 0],
    );
    let deadline = (timeout != u64::MAX).then(|| Instant::now() + Duration::from_nanos(timeout));
    // Two things hold a frame, and they are independent: the schedule (the cap)
    // and an image being free (the pipeline). The loop asks each in turn, in that
    // order, and sleeps in exactly one place.
    let pacing = unsafe { acquire_pacing(sc) };
    let gated = pacing != crate::vsync_gate::Pacing::Off;
    if gated {
        ACQ_GATED_N.fetch_add(1, Ordering::Relaxed);
    } else {
        ACQ_UNGATED_N.fetch_add(1, Ordering::Relaxed);
    }
    let mut idx: i32 = -1;
    // Why this acquire waited, and since when: one census sample per acquire,
    // never one per turn of the loop.
    let mut blocked = Blocked::None;
    let mut blocked_at = 0u64;
    // When the rate condition was satisfied, 0 while it still is not.
    let mut rate_done_ns = 0u64;
    {
        let mut g = (*sc).shared.lock().unwrap();
        loop {
            if idx >= 0 || g.stopping {
                break;
            }
            // 1. The cap. A frame may not start before the schedule says.
            if gated {
                let due = (*sc).release.next_ns;
                let now = monotonic_ns();
                if due != 0 && now < due {
                    if timeout == 0 {
                        break;
                    }
                    if blocked == Blocked::None {
                        blocked = Blocked::Rate;
                        blocked_at = now;
                    }
                    let (guard, waited) =
                        unsafe { sleep_ns(sc, g, due.saturating_sub(now), deadline) };
                    g = guard;
                    match waited {
                        Waited::Slept => continue,
                        Waited::Expired => break,
                    }
                }
            }
            // 1 ended: if this frame was held for the cap, its rate time stops
            // here. Recorded once, so the slot wait that follows is counted as a
            // slot wait instead of being filed under the rate.
            if blocked == Blocked::Rate && rate_done_ns == 0 {
                rate_done_ns = monotonic_ns();
            }
            // 2. An image.
            unsafe { acquire_take_free(sc, &mut idx) };
            if idx >= 0 {
                break;
            }
            if blocked == Blocked::None {
                blocked = Blocked::Slot;
                blocked_at = monotonic_ns();
                SLOT_WAIT_N.fetch_add(1, Ordering::Relaxed);
            }
            if timeout == 0 {
                break;
            }
            // Nothing free: let the release side make one free, then sleep a
            // slice on the event that will.
            drop(g);
            unsafe { x11_reap(sc) };
            crate::scanout::poll_notes();
            g = (*sc).shared.lock().unwrap();
            if g.stopping {
                break;
            }
            // The release side has had its turn: a slot it freed is taken here,
            // without paying for another trip round the loop.
            unsafe { acquire_take_free(sc, &mut idx) };
            if idx >= 0 {
                break;
            }
            // A parked slot is freed by a note, so that wait is a poll of the
            // channel at the note's own scale; otherwise the idle slice.
            let slice = if unsafe { any_parked(sc) } {
                PARKED_SLICE
            } else {
                REAP_SLICE
            };
            let (guard, waited) = unsafe { sleep_ns(sc, g, slice.as_nanos() as u64, deadline) };
            g = guard;
            if waited == Waited::Expired {
                break;
            }
        }
        if g.stopping && idx < 0 {
            drop(g);
            return VK_ERROR_OUT_OF_DATE_KHR;
        }
        drop(g);
    }
    if idx >= 0 {
        let now = monotonic_ns();
        // What this acquire produced, and the cadence it produced it at.
        note_acquire_granted(now);
        // One timeline for the whole swapchain: the next frame may not start
        // before one period after this one *actually* started.
        if gated {
            (*sc).release.book_paced(now, pacing);
            // Counted against the grants to answer one question the census could
            // not: a schedule that books once per grant can never let a grant see
            // its deadline already past, so `books` equal to the grant count with
            // ungated-looking acquires means the schedule is being re-anchored,
            // and `books` short of it means some grants skip the book.
            BOOK_N.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: `idx` is a slot of this swapchain.
        (*(*sc).slots.as_mut_ptr().add(idx as usize)).acquired_ns = now;
        // Two waits, two totals. A frame held by the cap and then by an image is
        // charged to each for the part it actually spent there.
        let rate_end = if rate_done_ns != 0 { rate_done_ns } else { now };
        if blocked == Blocked::Rate {
            let us = rate_end.saturating_sub(blocked_at) as usize / 1_000;
            GATE_HOLD_N.fetch_add(1, Ordering::Relaxed);
            GATE_HOLD_SUM_US.fetch_add(us, Ordering::Relaxed);
            GATE_HOLD_MAX_US.fetch_max(us, Ordering::Relaxed);
        }
        if blocked != Blocked::None {
            let slot_us = now.saturating_sub(rate_end) as usize / 1_000;
            if slot_us > 0 {
                SLOT_HOLD_N.fetch_add(1, Ordering::Relaxed);
                SLOT_HOLD_SUM_US.fetch_add(slot_us, Ordering::Relaxed);
                SLOT_HOLD_MAX_US.fetch_max(slot_us, Ordering::Relaxed);
            }
        }
    } else {
        ACQ_RETRY_N.fetch_add(1, Ordering::Relaxed);
    }
    if idx < 0 {
        // Every slot is held by the compositor. This is the signature to look
        // for when the game goes silent: an app that spins here is waiting for
        // a wl_buffer.release that never comes.
        N_STARVE.fetch_add(1, Ordering::Relaxed);
        if rate_ok(&RATE_STALL, RATE_EVERY) {
            crate::logf!("gn-vkshim stall: acquire found no free slot (timeout={timeout}) {}",
                slot_flags(sc));
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
    // One line per frame for the app's frame ring: the count is cumulative, so
    // a message the socket refuses is carried by the next one.
    let acquired = ACQ_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    // With what generation has inserted so far: the pair is what the session has
    // produced, and the daemon turns the deltas into the app's frame ring. Sent
    // once per acquired frame, so the ring's granularity is one frame.
    crate::scanout::send_frame_acquired(
        acquired,
        GEN_COMMITS.load(Ordering::Relaxed).into(),
    );
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
        crate::logf!("proxy acquire: sc={sc:?} slot={idx} wait={acq_ms}ms");
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
        // The replaced frame is never committed, released or latched, so a
        // wait on its present id is answered here (wp_presentation's
        // "discarded"); otherwise the guest waits out present::wait's bound.
        let mut replaced_id = 0;
        if let Some(old) = g.mailbox.replace(idx) {
            let os = (*sc).slots.as_mut_ptr().add(old as usize);
            (*os).in_flight = false;
            (*os).present_id = 0;
            (*os).wait_registered = false;
            replaced_id = core::mem::replace(&mut (*os).wait_id, 0);
        }
        (*sc).cv_free.notify_all();
        (*sc).cv_put.notify_one();
        drop(g);
        if replaced_id != 0 {
            crate::present::discard(replaced_id, sc as usize, monotonic_ns());
        }
    }
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
                // Declare the mode in force, as on the worker path above
                // (gamescope reads the mode once per commit).
                if !(*(*sc).surf).swapchain_obj.is_null() {
                    wlproto::swapchain_set_present_mode(
                        (*(*sc).surf).swapchain_obj,
                        gamescope_present_mode(declared_present_mode(sc)),
                    );
                }
                bind_window_at_first_commit(sc);
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
    }
}

/// When a present wait on a frame is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitAnswer {
    /// When the guest queues the present (`present_dispatch`).
    AtQueue,
    /// When the frame is committed, its render fence waited (`commit_real`).
    AtCommit,
    /// By the compositor: presentation feedback, the release, or the grace.
    Later,
}

/// [`WaitAnswer`] for a frame of a swapchain the guest made for `requested`
/// that goes out as `live`, declared for the reason `why`.
///
/// One case is answered when the guest queues the present, before the frame's
/// render fence: gamescope's route and a FIFO-relaxed guest whose rate the
/// acquire gate holds. That guest waits on every frame (DXVK with VSync on),
/// and answered after the fence its CPU and GPU alternated; at queue, as the
/// X11 route answers (its X server sends PresentCompleteNotify while handling
/// the PresentPixmap request), they overlap and the gate holds the rate.
/// Everything else keeps the answer it had: a frame going out as mailbox or
/// immediate, or from a guest that asked for either, at the commit; FIFO by
/// the compositor.
fn present_wait_answer(
    gamescope_route: bool,
    paced: bool,
    requested: VkPresentModeKHR,
    live: VkPresentModeKHR,
) -> WaitAnswer {
    // The override route answers at the commit, for every mode: FIFO's throttle in
    // native Vulkan is inside `vkQueuePresentKHR`, and `VK_KHR_present_wait` is a
    // report of scan-out that the client paces itself with. Making the report the
    // throttle cost a full latch inside every frame — Dave at 40 fps spent ~11 ms
    // of 24.6 blocked here. The X11 route answers at queue, where its X server
    // sends PresentCompleteNotify while handling the request.
    let _ = (paced, requested, live);
    if gamescope_route {
        WaitAnswer::AtCommit
    } else {
        WaitAnswer::AtQueue
    }
}

/// Route a present to the discipline its effective present mode names.
unsafe fn present_dispatch(sc: *mut GnSwapchain, slot: *mut GnSlot, idx: i32) {
    mode_census(sc);
    let (live, _why) = unsafe { live_choice(sc) };
    let answer = present_wait_answer(
        unsafe { !(*(*sc).surf).swapchain_obj.is_null() },
        unsafe { acquire_pacing(sc) } != crate::vsync_gate::Pacing::Off,
        unsafe { (*sc).present_mode },
        live,
    );
    if answer == WaitAnswer::AtQueue {
        // Taken off the slot, so neither the commit, a replacement nor the
        // release answers it again; the daemon's later latch report finds it
        // no longer armed.
        let id = {
            let _g = unsafe { (*sc).shared.lock().unwrap() };
            unsafe { core::mem::replace(&mut (*slot).wait_id, 0) }
        };
        if id != 0 {
            crate::present::answered_at_queue(id, sc as usize, monotonic_ns());
        }
    }
    // In present order, whatever the mode: a copy under way cannot be replaced,
    // and at most `guest_slots` frames can wait. gamescope reads the declared mode
    // and replaces a frame it has not latched, so mailbox still means mailbox.
    if unsafe { (*slot).spare != NO_SPARE || spare_in_use(sc) } {
        unsafe { present_immediate(sc, slot, idx) };
        return;
    }
    match unsafe { live_present_mode(sc) } {
        VK_PRESENT_MODE_IMMEDIATE_KHR => unsafe { present_immediate(sc, slot, idx) },
        VK_PRESENT_MODE_MAILBOX_KHR => unsafe { present_mailbox(sc, slot, idx) },
        VK_PRESENT_MODE_FIFO_RELAXED_KHR => unsafe { present_fifo_relaxed(sc, slot, idx) },
        _ => unsafe { present_fifo(sc, slot, idx) },
    }
}

// Present: the app's render completion
/// Once per process: the sTypes chained on the first present, and the id read
/// from them. Says whether a VkPresentIdKHR the guest chains reaches this call.
fn log_present_chain_once(p_next: *const c_void, id: u64) {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    #[repr(C)]
    struct Head {
        s_type: i32,
        p_next: *const c_void,
    }
    let mut types = Vec::new();
    let mut head = p_next.cast::<Head>();
    for _ in 0..64 {
        if head.is_null() {
            break;
        }
        // SAFETY: a live VkPresentInfoKHR chain; only the two leading members are read.
        let node = unsafe { &*head };
        types.push(node.s_type);
        head = node.p_next.cast::<Head>();
    }
    crate::logf!("proxy present: first pNext chain sTypes={:?} present_id={}",
        types,
        id);
}

/// Logged once per process: a maintenance1 mode switch the shim does not serve.
static MODE_SWITCH_NOTED: AtomicBool = AtomicBool::new(false);
/// Present fences signalled (`VkSwapchainPresentFenceInfo`), for the census.
static N_PRESENT_FENCE: AtomicUsize = AtomicUsize::new(0);
/// Images handed back by `vkReleaseSwapchainImagesKHR`, for the census.
static N_RELEASED_IMAGES: AtomicUsize = AtomicUsize::new(0);

/// `vkReleaseSwapchainImagesKHR` (`VK_KHR_swapchain_maintenance1`): the guest
/// hands back images it acquired and will not present. Each goes straight back
/// to the free list; the semaphore and fence of its acquire were signalled at
/// the acquire (`gn_acquire_common`), and the extension makes the guest wait
/// for them before calling this.
///
/// The acquire gate booked a frame for each of these acquires
/// (`Schedule::book_paced`); the booking stands, so a guest that releases
/// instead of presenting has spent that period. That is the cap doing its job
/// (a released image is a frame the guest chose not to show), not a gap.
unsafe extern "C" fn gn_vkReleaseSwapchainImagesKHR(
    _device: VkDevice,
    info: *const c_void,
) -> VkResult {
    let (swapchain, indices) = unsafe { crate::maint1::release_info(info) };
    let sc = unsafe { find_swapchain(swapchain) };
    if sc.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let mut freed = 0usize;
    {
        let _g = unsafe { (*sc).shared.lock().unwrap() };
        for &i in &indices {
            if i >= unsafe { (*sc).guest_slots } {
                continue;
            }
            let s = unsafe { (*sc).slots.as_mut_ptr().add(i as usize) };
            // Only a slot the guest holds: one in flight was presented, and a
            // free one is not the guest's to release.
            if unsafe { (*s).acquired && !(*s).in_flight } {
                unsafe {
                    (*s).acquired = false;
                    (*s).acquired_ns = 0;
                }
                freed += 1;
            }
        }
        if freed > 0 {
            unsafe { (*sc).cv_free.notify_all() };
        }
    }
    if crate::vkshim_enabled() {
        N_RELEASED_IMAGES.fetch_add(freed, Ordering::Relaxed);
        crate::logf!("proxy release images: sc={sc:?} asked={:?} freed={freed}",
            indices);
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkQueuePresentKHR(queue: VkQueue, pi: *const VkPresentInfoKHR) -> VkResult {
    // The present's own duration, the analogue of `acqus`: the acquire is
    // microseconds, so where a frame actually goes is this.
    if !crate::vkshim_enabled() {
        return unsafe { present_body(queue, pi) };
    }
    let t0 = Instant::now();
    let res = unsafe { present_body(queue, pi) };
    let us = t0.elapsed().as_micros() as usize;
    PRESENT_US_N.fetch_add(1, Ordering::Relaxed);
    PRESENT_US_SUM.fetch_add(us, Ordering::Relaxed);
    PRESENT_US_MAX.fetch_max(us, Ordering::Relaxed);
    if us >= 16_667 {
        PRESENT_US_GE_VBLANK.fetch_add(1, Ordering::Relaxed);
    }
    res
}

unsafe fn present_body(queue: VkQueue, pi: *const VkPresentInfoKHR) -> VkResult {
    // The WSI census exists to be read under `GNDRM_LOG_VKSHIM`, and every line
    // that prints it is gated the same way, so its accounting is skipped when
    // the gate is off: a present would otherwise be a clock read and five
    // read-modify-writes for numbers nobody can print.
    if crate::vkshim_enabled() {
        HB_PRESENT.fetch_add(1, Ordering::Relaxed);
        wsi_hb();
        N_PRES.fetch_add(1, Ordering::Relaxed);
        // The guest's frame period as the shim sees it: stamped at present
        // entry, before any work, so the census is the client's own cadence,
        // not ours.
        pres_iv_note(monotonic_ns());
    }
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
                format_args!("proxy: gn_vkQueuePresentKHR QUEUE MISS q={:#x} {}",
                    queue as usize,
                    unsafe { handle_note(false, queue as usize, first, n) })
            )
        });
        gd = G.lock().unwrap().devices;
    }
    if gd.is_null() {
        loud("present.device_miss", || {
            format!(
                "{}",
                format_args!("proxy: gn_vkQueuePresentKHR DEVICE MISS q={:#x} — no device wrapper at all",
                    queue as usize)
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
        // The present id the guest chained for this swapchain, if any: the
        // frame is waited on by it, and the daemon is told at the commit.
        let present_id = unsafe { crate::present::present_id(pi.pNext, i) };
        if i == 0 {
            log_present_chain_once(pi.pNext, present_id);
        }
        // The guest's present fence for this swapchain, if any
        // (`VkSwapchainPresentFenceInfo`): signalled once the wait semaphores
        // below have been consumed, which is what the fence promises.
        let guest_fence = unsafe { crate::maint1::present_fence(pi.pNext, i) };
        if let Some(mode) = unsafe { crate::maint1::present_mode_request(pi.pNext, i) } {
            // TODO(maintenance1 follow-up): per-present mode switching. The
            // compatibility answer lists one mode, so a conforming guest never
            // names another; one that does is noted once and keeps the
            // swapchain's mode (`crate::maint1`).
            if mode != unsafe { (*sc).present_mode }
                && !MODE_SWITCH_NOTED.swap(true, Ordering::Relaxed)
            {
                crate::logf!("proxy present: maintenance1 mode switch to {mode} requested (swapchain created with {}); not served, mode kept",
                    unsafe { (*sc).present_mode });
            }
        }
        // A slot presented again while it still carries an armed id: that
        // frame will never be committed, released or latched under its id
        // now, so the wait on it is answered first.
        let stale_id = unsafe {
            let _g = (*sc).shared.lock().unwrap();
            if (*slot).wait_id != 0 && (*slot).wait_id != present_id {
                (*slot).wait_registered = false;
                core::mem::replace(&mut (*slot).wait_id, 0)
            } else {
                0
            }
        };
        if stale_id != 0 {
            crate::present::discard(stale_id, sc as usize, monotonic_ns());
        }
        if present_id != 0 {
            unsafe {
                (*slot).present_id = present_id;
                (*slot).wait_id = present_id;
                (*slot).wait_registered = false;
            }
            crate::present::arm(present_id, sc as usize);
        }
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
        // Signals: the chain semaphore when a later present waits it, and the
        // slot's fence semaphore when the guest chained a present fence. Both
        // are binary and each is waited exactly once.
        let mut signals: [VkSemaphore; 2] = [0; 2];
        let mut n_signals = 0u32;
        if signal_chain {
            signals[n_signals as usize] = unsafe { (*slot).sem };
            n_signals += 1;
        }
        if guest_fence != 0 {
            signals[n_signals as usize] = unsafe { (*slot).fence_sem };
            n_signals += 1;
        }
        let spare_cmd = if unsafe { (*sc).spare_slots } == 0 {
            ptr::null_mut()
        } else {
            unsafe { spare_begin(sc, slot, indices[i] as usize, gq) }
        };
        let si = VkSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            pNext: ptr::null(),
            waitSemaphoreCount: n_waits,
            pWaitSemaphores: waits,
            pWaitDstStageMask: wait_stages.as_mut_ptr(),
            commandBufferCount: u32::from(!spare_cmd.is_null()),
            pCommandBuffers: &spare_cmd,
            signalSemaphoreCount: n_signals,
            pSignalSemaphores: if n_signals > 0 {
                signals.as_ptr()
            } else {
                ptr::null()
            },
        };
        // The guest's present fence rides on a second, empty submit that waits
        // the fence semaphore: it completes once the present's wait semaphores
        // have been consumed and no sooner, which is the fence's contract
        // (`VK_KHR_swapchain_maintenance1`: the wait semaphores may be reused or
        // destroyed once it signals). The slot's own fence stays on the first
        // submit for the present worker.
        let fence_stage = [VK_PIPELINE_STAGE_ALL_COMMANDS_BIT];
        let fence_si = VkSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            pNext: ptr::null(),
            waitSemaphoreCount: 1,
            pWaitSemaphores: unsafe { &(*slot).fence_sem },
            pWaitDstStageMask: fence_stage.as_ptr(),
            commandBufferCount: 0,
            pCommandBuffers: ptr::null(),
            signalSemaphoreCount: 0,
            pSignalSemaphores: ptr::null(),
        };
        let res = {
            let _submit = SUBMIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let res = unsafe { (*gd).f.submit.unwrap()(real_q, 1, &si, (*slot).fence) };
            if res == VK_SUCCESS && guest_fence != 0 {
                let r2 = unsafe { (*gd).f.submit.unwrap()(real_q, 1, &fence_si, guest_fence) };
                if r2 != VK_SUCCESS {
                    crate::logf!("proxy: present fence submit failed: {r2}");
                } else if crate::vkshim_enabled() {
                    N_PRESENT_FENCE.fetch_add(1, Ordering::Relaxed);
                }
            }
            res
        };
        if res != VK_SUCCESS {
            crate::logf!("proxy: present wait submit failed: {res}");
            let hidden = unsafe { core::mem::replace(&mut (*slot).spare, NO_SPARE) };
            if hidden != NO_SPARE {
                unsafe { spare_unclaim(sc, hidden as usize) };
            }
            return res;
        }
        if signal_chain {
            chain_prev = unsafe { (*slot).sem };
        }
        unsafe { present_dispatch(sc, slot, indices[i] as i32) };
        if LOG_PRESENT.fetch_add(1, Ordering::Relaxed) < LOG_FIRST_N {
            crate::logf!("proxy present: sc={sc:?} idx={} {}",
                indices[i],
                counters());
        }
        if LOUD_PRESENT.fetch_add(1, Ordering::Relaxed) < 4 {
            crate::logf!("proxy: present flow pid={} sc={sc:?} idx={} {}",
                std::process::id(),
                indices[i],
                counters());
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
                format_args!("proxy: vkQueueSubmit QUEUE MISS q={:#x} {} — submitting guest CBs directly",
                    queue as usize,
                    unsafe { handle_note(false, queue as usize, first, n) })
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
    crate::logf!("proxy submit #{seq}: n={count} q={queue:?} entries={:?} fence={fence:?}",
        unsafe { slice_or_empty(submits, count as usize) }
            .iter()
            .map(|s| s.commandBufferCount)
            .collect::<Vec<_>>());
    if LOUD_SUBMIT.fetch_add(1, Ordering::Relaxed) < 4 {
        crate::logf!("proxy: submit flow #{seq} pid={} q={queue:?} real_q={:?} n={count} ncmd={:?}",
            std::process::id(),
            unsafe { (*gq).real },
            unsafe { slice_or_empty(submits, count as usize) }
                .iter()
                .map(|s| s.commandBufferCount)
                .collect::<Vec<_>>());
    }
    let _submit = SUBMIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { (*gd).f.submit.unwrap()((*gq).real, count, args[2] as *const VkSubmitInfo, fence) }
}

/// GPU waits. The guest blocks here when submitted work never completes, so log
/// what it wants against the last value it asked us to signal. The line is
/// printed BEFORE the wait, so if the wait never returns, the last line in the
/// log *is* the block; a returned wait longer than a second is logged too.
unsafe extern "C" fn gn_vkWaitForPresentKHR(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    present_id: u64,
    timeout: u64,
) -> VkResult {
    // The guest must have enabled the extension it is calling into: the state
    // is recorded at device creation, before the feature structs are stripped.
    if !crate::present::enabled() {
        return VK_ERROR_EXTENSION_NOT_PRESENT;
    }
    let sc = unsafe { find_swapchain(swapchain) };
    if sc.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    if crate::present::wait(present_id, sc as usize, timeout) {
        VK_SUCCESS
    } else {
        VK_TIMEOUT
    }
}

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
        crate::logf!("proxy wait: n={n} want=[{wants}] last_signalled={last} timeout={timeout}");
    }
    let t0 = Instant::now();
    let res = unsafe { f((*gd).real, wi, timeout) };
    let ms = t0.elapsed().as_millis();
    if ms >= 1000 {
        crate::logf!("proxy wait done: res={res} after={ms}ms");
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
                format_args!("proxy: vkQueueSubmit2 QUEUE MISS q={:#x} {} — batch dropped",
                    queue as usize,
                    unsafe { handle_note(false, queue as usize, first, n) })
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
        crate::logf!("proxy: submit2 flow pid={} q={queue:?} real_q={:?} n={count} ncmd={:?}",
            std::process::id(),
            unsafe { (*gq).real },
            unsafe { slice_or_empty(submits, count as usize) }
                .iter()
                .map(|s| s.commandBufferInfoCount)
                .collect::<Vec<_>>());
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
                        format_args!("W{:?}@{}", s.semaphore, s.value)
                    ));
                }
                let ns = si.signalSemaphoreInfoCount as usize;
                let ss = unsafe {
                    slice_or_empty(si.pSignalSemaphoreInfos as *const VkSemaphoreSubmitInfo, ns)
                };
                for s in ss.iter().take(3) {
                    sems.push(format!(
                        "{}",
                        format_args!("S{:?}@{}", s.semaphore, s.value)
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
            crate::logf!("proxy submit2 #{seq}: n={count} q={queue:?} real_q={:?} entries={:?} cbs={cbs:?} sems={sems:?} fence={fence:?}",
                unsafe { (*gq).real },
                unsafe { slice_or_empty(submits, count as usize) }
                    .iter()
                    .map(|s| s.commandBufferInfoCount)
                    .collect::<Vec<_>>());
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
                format_args!("proxy: vkAllocateCommandBuffers DEVICE MISS {} — guest CBs will be driver-raw",
                    unsafe { handle_note(false, device as usize, first, n) })
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
                crate::logf!("proxy alloc cmd: wrapper={:?} real={real:?}",
                    gc.cast::<c_void>());
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
        crate::logf!("proxy: winding compensation skipped on {culled}/{count} culling pipeline(s)");
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
    if let Some(r) = unsafe { crate::export::try_create_import_image(gd, ci, alloc, out) } {
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
            crate::logf!("guest image: fmt={} {}x{}x{} type={} mips={} layers={} samples={:#x} tiling={} usage={:#x} flags={:#x} pNext=[{pnext}]",
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
                ci_ref.flags);
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
        crate::logf!("x11 cmd {n}: {name} cb={cmd:?} {hits}");
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
            crate::logf!("x11 cmd-seen: {name} (guest records it through the shim)");
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
        Some((sc, i)) => format!("{}", format_args!("SLOT sc={sc:?} slot={i}")),
        None => "guest".into(),
    }
}

/// Note the area the guest draws at. DXVK scales its backbuffer into a
/// window-sized swapchain, so a present transfer's source extent is the
/// resolution the guest actually rendered; the engine sizes its working
/// resolution against it, so a title drawing below the window's size costs less
/// per generated frame. A peak, because a title that once drew larger keeps the
/// larger chain.
fn note_guest_area(gd: *mut GnDevice, width: u32, height: u32) {
    if gd.is_null() || width == 0 || height == 0 {
        return;
    }
    let peak = unsafe { &(*gd).guest_peak };
    peak.0.fetch_max(width, Ordering::Relaxed);
    peak.1.fetch_max(height, Ordering::Relaxed);
}

/// Source extent of the first blit region: the size of the image scaled into a
/// swapchain slot.
fn blit_source(n: u32, regions: *const VkImageBlit) -> (u32, u32) {
    if n == 0 || regions.is_null() {
        return (0, 0);
    }
    let r = unsafe { &*regions };
    (
        r.srcOffsets[1].x.abs_diff(r.srcOffsets[0].x),
        r.srcOffsets[1].y.abs_diff(r.srcOffsets[0].y),
    )
}

/// Source extent of the first blit region of a v2 `VkImageBlit2` array.
fn blit_source2(n: u32, regions: *const VkImageBlit2) -> (u32, u32) {
    if n == 0 || regions.is_null() {
        return (0, 0);
    }
    let r = unsafe { &*regions };
    (
        r.srcOffsets[1].x.abs_diff(r.srcOffsets[0].x),
        r.srcOffsets[1].y.abs_diff(r.srcOffsets[0].y),
    )
}

/// Extent of the first copy region, as a pair.
fn copy_size(n: u32, regions: *const VkImageCopy) -> (u32, u32) {
    if n == 0 || regions.is_null() {
        return (0, 0);
    }
    let e = unsafe { (*regions).extent };
    (e.width, e.height)
}

/// Extent of the first copy region of a v2 `VkImageCopy2` array, as a pair.
fn copy_size2(n: u32, regions: *const VkImageCopy2) -> (u32, u32) {
    if n == 0 || regions.is_null() {
        return (0, 0);
    }
    let e = unsafe { (*regions).extent };
    (e.width, e.height)
}

/// A transfer region's extent, as a log field.
fn extent_note(e: VkExtent3D) -> String {
    format!("{}", format_args!("r0={}x{}", e.width, e.height))
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
        format_args!("blit={}x{} -> {}x{}",
            r.srcOffsets[1].x - r.srcOffsets[0].x,
            r.srcOffsets[1].y - r.srcOffsets[0].y,
            r.dstOffsets[1].x - r.dstOffsets[0].x,
            r.dstOffsets[1].y - r.dstOffsets[0].y)
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
    crate::logf!("x11 {name}: call={n} count={count} vp0 x={} y={} w={} h={} depth={}..{}",
        v.x,
        v.y,
        v.width,
        v.height,
        v.minDepth,
        v.maxDepth);
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
    crate::logf!("x11 {name}: call={n} count={count} sc0 off=({},{}) ext={}x{}",
        r.offset.x,
        r.offset.y,
        r.extent.width,
        r.extent.height);
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
                            format!("{}", format_args!("SLOT sc={sc:?} slot={si}"))
                        }
                        None => format!("{}", format_args!("img={:#x}", i as usize)),
                    },
                    None => format!("{}", format_args!("view={:#x}", a.imageView as usize)),
                };
                let c = a.clearValue.float32;
                parts.push(format!(
                    "{}",
                    format_args!("a{k}[{note} layout={} load={} store={} clear=({},{},{},{})]",
                        a.imageLayout,
                        a.loadOp,
                        a.storeOp,
                        c[0],
                        c[1],
                        c[2],
                        c[3])
                ));
            }
            crate::logf!("x11 rndr {n}: {}x{} colors={na} {}",
                ri.renderArea.extent.width,
                ri.renderArea.extent.height,
                parts.join(" "));
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
            crate::logf!("x11 rpb2 {n}: {}x{} off=({},{}) fb={:#x} rp={:#x} clears={} clear0=({},{},{},{}) rpOps={} slots={slots:?}",
                bi.renderArea.extent.width,
                bi.renderArea.extent.height,
                bi.renderArea.offset.x,
                bi.renderArea.offset.y,
                bi.framebuffer as usize,
                bi.renderPass as usize,
                bi.clearValueCount,
                c[0], c[1], c[2], c[3],
                rp_ops_note(bi.renderPass));
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
            crate::logf!("x11 rpb {n}: {}x{} off=({},{}) fb={:#x} rp={:#x} clears={} clear0=({},{},{},{}) contents={contents} rpOps={} slots={slots:?}",
                bi.renderArea.extent.width,
                bi.renderArea.extent.height,
                bi.renderArea.offset.x,
                bi.renderArea.offset.y,
                bi.framebuffer as usize,
                bi.renderPass as usize,
                bi.clearValueCount,
                c[0], c[1], c[2], c[3],
                rp_ops_note(bi.renderPass));
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
                crate::logf!("x11 any clear: img={:#x} layout={layout}",
                    image as usize);
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
                    format_args!("SLOT sc={sc:?} slot={i} layout={layout} n={n} clear=({},{},{},{})",
                        c[0],
                        c[1],
                        c[2],
                        c[3])
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
        if d.is_some() {
            let (w, h) = copy_size(n, regions);
            note_guest_area(gd, w, h);
        }
        if s.is_some() || d.is_some() {
            trace_sc_cmd(
                "vkCmdCopyImage",
                cmd,
                &format!("{}", format_args!("src={:#x}({}) dst={:#x}({}) srcLayout={src_layout} dstLayout={dst_layout} n={n} {}",
                    src as usize,
                    slot_note(s),
                    dst as usize,
                    slot_note(d),
                    copy_extent(n, regions))),
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
                crate::logf!("x11 any copyimg2: src={:#x} dst={:#x} {}x{}",
                    ci.srcImage as usize,
                    ci.dstImage as usize,
                    ci.regionCount,
                    if ci.regionCount > 0 && !ci.pRegions.is_null() {
                        unsafe { (*ci.pRegions).extent.width }
                    } else {
                        0
                    });
            }
        }
        let s = unsafe { sc_slot_of(gd, ci.srcImage) };
        let d = unsafe { sc_slot_of(gd, ci.dstImage) };
        if d.is_some() {
            let (w, h) = copy_size2(ci.regionCount, ci.pRegions);
            note_guest_area(gd, w, h);
        }
        if s.is_some() || d.is_some() {
            trace_sc_cmd(
                "vkCmdCopyImage2",
                cmd,
                &format!(
                    "{}",
                    format_args!("src={:#x}({}) dst={:#x}({}) srcLayout={} dstLayout={} n={} {}",
                        ci.srcImage as usize,
                        slot_note(s),
                        ci.dstImage as usize,
                        slot_note(d),
                        ci.srcImageLayout,
                        ci.dstImageLayout,
                        ci.regionCount,
                        copy_extent2(ci.regionCount, ci.pRegions))
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
        if d.is_some() {
            let (w, h) = blit_source(n, regions);
            note_guest_area(gd, w, h);
        }
        if s.is_some() || d.is_some() {
            trace_sc_cmd(
                "vkCmdBlitImage",
                cmd,
                &format!("{}", format_args!("src={:#x}({}) dst={:#x}({}) srcLayout={src_layout} dstLayout={dst_layout} n={n} filter={filter} {}",
                    src as usize,
                    slot_note(s),
                    dst as usize,
                    slot_note(d),
                    blit_extent(n, regions))),
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
        if d.is_some() {
            let (w, h) = blit_source2(ci.regionCount, ci.pRegions);
            note_guest_area(gd, w, h);
        }
        if s.is_some() || d.is_some() {
            trace_sc_cmd(
                "vkCmdBlitImage2",
                cmd,
                &format!(
                    "{}",
                    format_args!("src={:#x}({}) dst={:#x}({}) srcLayout={} dstLayout={} n={} filter={}",
                        ci.srcImage as usize,
                        slot_note(s),
                        ci.dstImage as usize,
                        slot_note(d),
                        ci.srcImageLayout,
                        ci.dstImageLayout,
                        ci.regionCount,
                        ci.filter)
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
                        crate::logf!("x11 any bar2: img={:#x} srcStage={:#x} dstStage={:#x}",
                            b.image as usize,
                            b.srcStageMask,
                            b.dstStageMask);
                    }
                }
                if let Some((sc, i)) = unsafe { sc_slot_of(gd, b.image) } {
                    trace_sc_cmd(
                        "vkCmdPipelineBarrier2(image)",
                        cmd,
                        &format!("{}", format_args!("SLOT sc={sc:?} slot={i} srcStage={:#x} srcAccess={:#x} dstStage={:#x} dstAccess={:#x}",
                            b.srcStageMask, b.srcAccessMask, b.dstStageMask, b.dstAccessMask)),
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
                    format_args!("buf={src_buffer:#x} -> SLOT sc={sc:?} slot={i} dstLayout={dst_layout} n={n}")
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
                    format_args!("buf={:#x} -> SLOT sc={sc:?} slot={i} dstLayout={} n={}",
                        ci.srcBuffer as usize,
                        ci.dstImageLayout,
                        ci.regionCount)
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
                    &format!("{}", format_args!("SLOT sc={sc:?} slot={i} {}->{} srcStage={src_stage:#x} dstStage={dst_stage:#x}",
                        b.oldLayout, b.newLayout)),
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
            format!("{}", format_args!("proxy: vkCmdExecuteCommands CB MISS cmd={:#x} count={count} — secondaries not translated",
                cmd as usize))
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
    (c"vkWaitForPresentKHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkWaitForPresentKHR
                as unsafe extern "C" fn(VkDevice, VkSwapchainKHR, u64, u64) -> VkResult,
        )
    }),
    (c"vkReleaseSwapchainImagesKHR", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkReleaseSwapchainImagesKHR
                as unsafe extern "C" fn(VkDevice, *const c_void) -> VkResult,
        )
    }),
    (c"vkReleaseSwapchainImagesEXT", HandlerScope::Always, unsafe {
        std::mem::transmute(
            gn_vkReleaseSwapchainImagesKHR
                as unsafe extern "C" fn(VkDevice, *const c_void) -> VkResult,
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
        c"vkGetPhysicalDeviceProperties",
        HandlerScope::ProxyOnly,
        unsafe {
            std::mem::transmute(
                crate::placed::gn_vkGetPhysicalDeviceProperties
                    as unsafe extern "C" fn(VkPhysicalDevice, *mut VkPhysicalDeviceProperties),
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
    // The features query is served on every route: besides the placed-memory
    // bits (patched only while that emulation is on) it reports the present-id
    // and present-wait features the shim serves for its own swapchain, which
    // the driver underneath answers for a swapchain the guest never uses.
    // Gated behind placed memory, a Turnip guest read the driver's zeros, so
    // DXVK never enabled the extensions the shim had advertised.
    if name == c"vkGetPhysicalDeviceFeatures2"
        || name == c"vkGetPhysicalDeviceFeatures2KHR"
        || name == c"vkGetPhysicalDeviceProperties2"
        || name == c"vkGetPhysicalDeviceProperties2KHR"
    {
        return false;
    }
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
    // then iterates without ever presenting. Which commands those are is the
    // WSI policy's (`crate::wsi_policy`): the hidden extensions' commands.
    if crate::wsi_policy::refused_command(name) {
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

/// Names already logged, per guest device handle. A process can create
/// several devices at different API versions and extension sets, and the same
/// name resolves differently on each, so once per process hides the device
/// that matters.
pub struct PerDeviceLog(Mutex<Vec<(usize, Vec<String>)>>);

const PER_DEVICE_LOG_CAP: usize = 1024;

impl Default for PerDeviceLog {
    fn default() -> Self {
        Self::new()
    }
}

impl PerDeviceLog {
    pub const fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    /// True the first time `name` is seen for `dev`, up to the per-device cap.
    pub fn first(&self, dev: usize, name: &str) -> bool {
        let Ok(mut all) = self.0.lock() else {
            return false;
        };
        let idx = match all.iter().position(|(d, _)| *d == dev) {
            Some(i) => i,
            None => {
                all.push((dev, Vec::new()));
                all.len() - 1
            }
        };
        let seen = &mut all[idx].1;
        if seen.len() >= PER_DEVICE_LOG_CAP || seen.iter().any(|s| s == name) {
            return false;
        }
        seen.push(name.to_owned());
        true
    }

    /// Drop a destroyed device's names, so a new device at the same address
    /// logs afresh.
    pub fn forget(&self, dev: usize) {
        if let Ok(mut all) = self.0.lock() {
            all.retain(|(d, _)| *d != dev);
        }
    }
}

pub static GDPA_MISSED: PerDeviceLog = PerDeviceLog::new();
/// Entry points the *driver* has no implementation for, asked through the
/// instance and physical-device paths. Named once each, like `GDPA_MISSED`:
/// the loader resolves device functions through these paths too, so a name the
/// driver lacks here is a name a client receives as a no-op.
pub static GIPA_MISSED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Record an unresolvable entry point asked through the instance or
/// physical-device path. Names the shim masks deliberately are not misses —
/// they are refusals, and `crate::wsi_policy` already says why.
pub fn note_instance_miss(name: &CStr, driver_has_none: bool) {
    if !driver_has_none || crate::wsi_policy::refused_command(name) {
        return;
    }
    if let Ok(mut missed) = GIPA_MISSED.lock() {
        let n = name.to_string_lossy().into_owned();
        if missed.len() < 64 && !missed.contains(&n) {
            crate::logf!("proxy: instance-path MISS (driver has no entry point): {n}");
            missed.push(n);
        }
    }
}
static REAL_PROC_SEEN: PerDeviceLog = PerDeviceLog::new();

unsafe extern "C" fn gn_vkGetDeviceProcAddr(
    device: VkDevice,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    let name = unsafe { CStr::from_ptr(name) };
    if let Some(f) = device_func(name) {
        return Some(f);
    }
    if crate::wsi_policy::refused_command(name) {
        // The proxy owns the swapchain: never hand the driver one of these with
        // a shim swapchain handle it has never seen. A hidden extension's
        // command the shim intercepts itself (the WSI dispatch-coverage set)
        // was answered above, as before.
        return None;
    }
    // Never the hooks' vkGetDeviceProcAddr (see gn_real_device_proc). The
    // loader will call whatever we return with the shim's wrapper handle, so
    // the rest goes through the shared unlisted-name policy: device context,
    // which always swaps.
    let target = unsafe { gn_real_device_proc(find_device(device), name) };
    if target.is_none() {
        // A NULL from here reaches the app as NULL from the loader's
        // vkGetDeviceProcAddr. What jumps through an empty slot is a loader
        // instance trampoline, resolved by name through the instance path
        // instead. Name every miss once per device.
        let n = name.to_string_lossy();
        if GDPA_MISSED.first(device as usize, &n) {
            crate::logf!("proxy: vkGetDeviceProcAddr MISS dev={:#x} (NULL to the app): {n}",
                device as usize);
        }
    }
    serve_unlisted(name, target, DispatchCtx::Device)
}

#[cfg(test)]
mod swapchain_slots_tests {
    use super::*;

    /// One ring for both consumers: a listed game with generation on pays for
    /// the larger of the two, not for both. This is the difference between 5
    /// slots and 9 at 720p, and between 9 and 20 past guest images at x4.
    #[test]
    fn the_ring_is_sized_for_the_larger_consumer() {
        // No generation: the hidden set is all the ring is.
        assert_eq!(dest_ring(4, 0), 4);
        // No hidden set: the destinations are.
        assert_eq!(dest_ring(0, 5), 5);
        // Both, which is the case that must not add: 4 + 5 was 9.
        assert_eq!(dest_ring(4, 5), 5);
        assert_eq!(dest_ring(4, 11), 11);
        // Neither.
        assert_eq!(dest_ring(0, 0), 0);
    }

    /// The ask is honoured: this is the whole point of the convention, and what
    /// a guest that sizes its per-image state from it depends on.
    #[test]
    fn the_ask_is_the_count() {
        assert_eq!(swapchain_slots(2, 8, VK_PRESENT_MODE_FIFO_KHR), 2);
        assert_eq!(swapchain_slots(3, 8, VK_PRESENT_MODE_FIFO_KHR), 3);
        assert_eq!(swapchain_slots(4, 8, VK_PRESENT_MODE_FIFO_RELAXED_KHR), 4);
        assert_eq!(swapchain_slots(5, 8, VK_PRESENT_MODE_IMMEDIATE_KHR), 5);
    }

    /// Mailbox is the one bump, and it is a floor, not an addend: a guest that
    /// already asked for four does not become five.
    #[test]
    fn mailbox_is_a_floor_of_four() {
        assert_eq!(swapchain_slots(2, 8, VK_PRESENT_MODE_MAILBOX_KHR), 4);
        assert_eq!(swapchain_slots(3, 8, VK_PRESENT_MODE_MAILBOX_KHR), 4);
        assert_eq!(swapchain_slots(4, 8, VK_PRESENT_MODE_MAILBOX_KHR), 4);
        assert_eq!(swapchain_slots(6, 8, VK_PRESENT_MODE_MAILBOX_KHR), 6);
    }

    /// The count follows the mode that will be *served*, so a generation session
    /// gets the mailbox depth even when the guest asks FIFO — the case Witcher 3
    /// measured at 3 guest slots against a 1.5-2.5-period hold.
    #[test]
    fn the_count_is_sized_for_the_served_mode() {
        // Not generating: the declaration decides, with immediate served as
        // mailbox, so an immediate guest gets four rather than its own two.
        assert_eq!(count_present_mode(VK_PRESENT_MODE_IMMEDIATE_KHR), VK_PRESENT_MODE_MAILBOX_KHR);
        assert_eq!(count_present_mode(VK_PRESENT_MODE_FIFO_KHR), VK_PRESENT_MODE_FIFO_KHR);
        // The mailbox floor then applies to the served mode, which is the whole
        // point: the same guest ask yields four images.
        assert_eq!(
            swapchain_slots(3, 8, count_present_mode(VK_PRESENT_MODE_IMMEDIATE_KHR)),
            4
        );
    }

    /// The park bound may not be derived from the interval it governs: a session
    /// that has slowed lengthens the hold that keeps it slow. Measured on Witcher
    /// 3 under Odin 3 at 3 fps — spacing 318 ms, bound 636 ms, three images per
    /// 0.64 s, 3 fps.
    #[test]
    fn the_park_bound_is_clamped_to_the_panel() {
        // The floor calibrates the test to whatever rate this process reports,
        // rather than assuming 60 Hz: with no spacing the bound is two panel
        // periods by definition.
        COMMIT_SPACING_NS.store(0, Ordering::Relaxed);
        let floor = release_note_limit_ns();
        let period = floor / 2;
        assert!(period > 0, "no panel period to work from");
        // A pathological spacing cannot stretch the bound past four periods.
        COMMIT_SPACING_NS.store(400_000_000, Ordering::Relaxed);
        assert_eq!(release_note_limit_ns(), 4 * period);
        // A fast producer is floored at two periods: the hold is a latch gap, and
        // a bound below it would expire before every note.
        COMMIT_SPACING_NS.store(1_000_000, Ordering::Relaxed);
        assert_eq!(release_note_limit_ns(), floor);
        // In between, the producer's own interval decides: a spacing of 1.5
        // periods doubles to 3, which sits inside the clamp and is returned as is.
        let spacing = period + period / 2;
        COMMIT_SPACING_NS.store(spacing, Ordering::Relaxed);
        assert_eq!(release_note_limit_ns(), spacing * 2);
        COMMIT_SPACING_NS.store(0, Ordering::Relaxed);
    }

    /// The census's rate is frames over elapsed time, and a bare sum of waits
    /// cannot supply it: 89 "fps" on a 30 cap was the reciprocal of a service
    /// time. This pins the arithmetic on the counters it now reads.
    #[test]
    fn the_census_rate_divides_frames_by_elapsed_time() {
        // Three grants 16.667 ms apart: two intervals over 33.333 ms = 60/s.
        ACQ_OK_N.store(3, Ordering::Relaxed);
        ACQ_FIRST_NS.store(1_000_000_000, Ordering::Relaxed);
        ACQ_LAST_NS.store(1_033_333_333, Ordering::Relaxed);
        let fps = acq_fps();
        assert!((fps - 60.0).abs() < 0.01, "{fps}");
        // One grant is not a rate, and neither is a window that never advanced.
        ACQ_OK_N.store(1, Ordering::Relaxed);
        assert_eq!(acq_fps(), 0.0);
        ACQ_OK_N.store(2, Ordering::Relaxed);
        ACQ_LAST_NS.store(1_000_000_000, Ordering::Relaxed);
        assert_eq!(acq_fps(), 0.0);
    }

    /// Two is the floor and the ceiling is the ceiling, whatever the ask.
    #[test]
    fn stays_inside_two_and_the_ceiling() {
        assert_eq!(swapchain_slots(0, 8, VK_PRESENT_MODE_FIFO_KHR), 2);
        assert_eq!(swapchain_slots(1, 8, VK_PRESENT_MODE_MAILBOX_KHR), 4);
        assert_eq!(swapchain_slots(9, 8, VK_PRESENT_MODE_FIFO_KHR), 8);
        // A ceiling under the mailbox floor cannot be honoured; the guest gets
        // the ceiling rather than an image count the surface refused.
        assert_eq!(swapchain_slots(2, 3, VK_PRESENT_MODE_MAILBOX_KHR), 3);
        assert_eq!(swapchain_slots(2, 1, VK_PRESENT_MODE_FIFO_KHR), 2);
    }

    /// The advertised minimum is the default, so a conforming guest and the
    /// create path agree on 3 without any bump carrying the depth.
    #[test]
    fn the_advertised_minimum_is_the_default() {
        assert_eq!(SC_DEFAULT_IMAGES, 3);
        assert_eq!(
            swapchain_slots(SC_DEFAULT_IMAGES, 8, VK_PRESENT_MODE_FIFO_KHR),
            SC_DEFAULT_IMAGES
        );
        // The floor cannot exceed the advertised depth, or a conforming guest
        // asking for the minimum would be bumped into a count it never chose.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_spellings_cover_the_whole_alias_group() {
        let mut s: Vec<&str> = other_spellings("vkCmdSetLineStipple").collect();
        s.sort_unstable();
        assert_eq!(s, ["vkCmdSetLineStippleEXT", "vkCmdSetLineStippleKHR"]);
        let mut s: Vec<&str> = other_spellings("vkCmdSetLineStippleKHR").collect();
        assert_eq!(s.remove(0), "vkCmdSetLineStipple");
        assert_eq!(s, ["vkCmdSetLineStippleEXT"]);
        assert_eq!(other_spellings("vkCreateDevice").count(), 0);
    }

    #[test]
    fn instance_names_the_system_loader_lacks_are_dropped() {
        let known = [CString::from(c"VK_KHR_surface"), CString::from(c"VK_EXT_debug_report")];
        let req = [c"VK_KHR_surface", c"VK_EXT_swapchain_colorspace"];
        let (keep, drop) = split_known(&req, Some(&known));
        assert_eq!(keep, vec![c"VK_KHR_surface"]);
        assert_eq!(drop, vec![c"VK_EXT_swapchain_colorspace"]);
        let (keep, drop) = split_known(&req, None);
        assert_eq!(keep, req.to_vec());
        assert!(drop.is_empty());
        let (keep, drop) = split_known(&[], Some(&known));
        assert!(keep.is_empty() && drop.is_empty());
    }

    #[test]
    fn api_version_is_clamped_only_for_a_1_0_loader() {
        let v1_4 = 1 << 22 | 4 << 12;
        assert_eq!(forward_api_version(VK_API_VERSION_1_3, v1_4), VK_API_VERSION_1_3);
        assert_eq!(forward_api_version(VK_API_VERSION_1_3, VK_API_VERSION_1_1), VK_API_VERSION_1_3);
        assert_eq!(forward_api_version(v1_4 | 354, VK_API_VERSION_1_3 | 280), v1_4 | 354);
        assert_eq!(forward_api_version(VK_API_VERSION_1_3, VK_API_VERSION_1_0), VK_API_VERSION_1_0);
        assert_eq!(forward_api_version(VK_API_VERSION_1_0 | 5, VK_API_VERSION_1_0), VK_API_VERSION_1_0 | 5);
        assert_eq!(forward_api_version(0, VK_API_VERSION_1_0), 0);
    }

    #[test]
    fn external_memory_counts_as_present_once_it_is_core() {
        let listed = |e: &CStr| DMA_BUF_EXTS[1..].contains(&e);
        assert_eq!(import_mode(&listed, false), None);
        EXTERNAL_MEMORY_CORE.store(true, Ordering::Relaxed);
        assert_eq!(import_mode(&listed, false), Some(false));
        EXTERNAL_MEMORY_CORE.store(false, Ordering::Relaxed);
    }

    #[test]
    fn api_versions_print_as_major_minor_patch() {
        assert_eq!(api_str(1 << 22 | 3 << 12 | 288), "1.3.288");
        assert_eq!(api_str(1 << 22 | 4 << 12), "1.4.0");
    }

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
                .unwrap_or_else(|| panic!("{}", format_args!("{name} missing from registry")));
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
                .unwrap_or_else(|| panic!("{}", format_args!("alias_of({core}) is empty")));
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

    /// Only the scan-out pool's layout is accepted by default, so a guest cannot
    /// render one channel order into a buffer the daemon publishes in the
    /// other. The other order is reachable only under the explicit opt-in.
    #[test]
    fn only_the_scanout_layout_is_accepted_by_default() {
        use gndrm_uapi::wire::ScanoutLayout::{Bgra8888, Rgba8888};
        for (layout, own, other) in [
            (Bgra8888, VK_FORMAT_B8G8R8A8_UNORM, VK_FORMAT_R8G8B8A8_UNORM),
            (Rgba8888, VK_FORMAT_R8G8B8A8_UNORM, VK_FORMAT_B8G8R8A8_UNORM),
        ] {
            assert_eq!(scanout_swapchain_format(layout), own);
            assert!(swapchain_format_allowed(own, layout, false));
            assert!(swapchain_format_allowed(own, layout, true));
            assert!(!swapchain_format_allowed(other, layout, false));
            assert!(swapchain_format_allowed(other, layout, true));
        }
        assert_eq!(other_eight_bit_env(Bgra8888), "GN_VKSHIM_ALLOW_RGBA");
        assert_eq!(other_eight_bit_env(Rgba8888), "GN_VKSHIM_ALLOW_BGRA");
    }

    /// Each layout serves its own 8-bit format, and the other one only under
    /// the opt-in.
    #[test]
    fn surface_formats_follow_the_layout() {
        use gndrm_uapi::wire::ScanoutLayout::{Bgra8888, Rgba8888};
        for (layout, own, other) in [
            (Bgra8888, VK_FORMAT_B8G8R8A8_UNORM, VK_FORMAT_R8G8B8A8_UNORM),
            (Rgba8888, VK_FORMAT_R8G8B8A8_UNORM, VK_FORMAT_B8G8R8A8_UNORM),
        ] {
            let sdr = surface_formats_for(layout, false, false);
            assert_eq!(sdr.len(), 1);
            assert_eq!(sdr[0].format, own);
            let sdr = surface_formats_for(layout, false, true);
            assert_eq!(
                sdr.iter().map(|f| f.format).collect::<Vec<_>>(),
                [own, other]
            );
            let hdr = surface_formats_for(layout, true, false);
            assert_eq!(hdr.len(), 9);
            assert!(hdr.iter().all(|f| f.format != other));
            assert_eq!(hdr.iter().filter(|f| f.format == own).count(), 3);
            let hdr = surface_formats_for(layout, true, true);
            assert_eq!(hdr.len(), 8);
            assert_eq!(hdr.iter().filter(|f| f.format == other).count(), 2);
        }
    }

    #[test]
    fn the_declaration_is_the_guest_mode_except_for_generation() {
        let (m, f, i, r) = (
            VK_PRESENT_MODE_MAILBOX_KHR,
            VK_PRESENT_MODE_FIFO_KHR,
            VK_PRESENT_MODE_IMMEDIATE_KHR,
            VK_PRESENT_MODE_FIFO_RELAXED_KHR,
        );
        // Nothing rewrites the request: not the cap, not the panel, not the
        // lead. Two exceptions, neither of them the guest's present as asked: a
        // generated frame, and `IMMEDIATE`, which is served as MAILBOX so a frame
        // that cannot be shown is dropped rather than scanned out mid-flip.
        for mode in [m, f, r] {
            assert_eq!(choose_live_mode(mode, false), (mode, Declared::Mode));
        }
        assert_eq!(
            choose_live_mode(i, false),
            (VK_PRESENT_MODE_MAILBOX_KHR, Declared::Mode)
        );
        for mode in [m, f, i, r] {
            assert_eq!(
                choose_live_mode(mode, true),
                (VK_PRESENT_MODE_MAILBOX_KHR, Declared::Generating)
            );
        }
    }

    #[test]
    fn the_gate_follows_the_acquire_limit_whatever_the_declaration() {
        use crate::vsync_gate::{GateClock, Pacing};
        // Every declaration the shim makes is gated: no mode is a pacer on this
        // route (see `gamescope_route_pacing`), and the period is the cap's own
        // whatever the panel is doing.
        for declared in [Declared::Mode, Declared::Generating] {
            assert_eq!(
                gamescope_route_pacing(declared, 60),
                Pacing::Clock {
                    period_ns: 16_666_666
                }
            );
        }
        // The cell gamescope's integer divisor cannot express: 60/40 truncates to
        // 1, so nothing limits 40 on a 60 Hz panel but this gate.
        assert_eq!(
            gamescope_route_pacing(Declared::Mode, 40),
            Pacing::Clock {
                period_ns: 25_000_000
            }
        );
        // And a cap above the panel is the same rule, not a special case.
        assert_eq!(gamescope_route_pacing(Declared::Mode, 120).clock(), GateClock::Clock);
    }

    #[test]
    fn the_present_wait_is_answered_at_the_commit_on_the_override_route() {
        let (m, f, i, r) = (
            VK_PRESENT_MODE_MAILBOX_KHR,
            VK_PRESENT_MODE_FIFO_KHR,
            VK_PRESENT_MODE_IMMEDIATE_KHR,
            VK_PRESENT_MODE_FIFO_RELAXED_KHR,
        );
        // Every mode on the override route, and with or without a gate: the wait
        // reports the handover, and the gate and the ring are what pace.
        for mode in [m, f, i, r] {
            for paced in [true, false] {
                assert_eq!(
                    present_wait_answer(true, paced, mode, mode),
                    WaitAnswer::AtCommit
                );
            }
        }
        // The X11 route answers when the guest queues, where its X server already
        // sends PresentCompleteNotify.
        for mode in [m, f, i, r] {
            assert_eq!(
                present_wait_answer(false, false, mode, mode),
                WaitAnswer::AtQueue
            );
        }
    }

    #[test]
    fn present_wait_is_answered_at_commit_only_for_unpaced_modes() {
    }

    /// The offered list keeps the served order and always offers FIFO, once.
    #[test]
    fn offered_list_keeps_the_served_order_and_always_offers_fifo() {
        let (m, f, i, r) = (
            VK_PRESENT_MODE_MAILBOX_KHR,
            VK_PRESENT_MODE_FIFO_KHR,
            VK_PRESENT_MODE_IMMEDIATE_KHR,
            VK_PRESENT_MODE_FIFO_RELAXED_KHR,
        );
        assert_eq!(offered_present_modes(&SERVED_PRESENT_MODES), [i, m, f, r]);
        // FIFO is appended when the served set lacks it, and not duplicated
        // when it has it.
        assert_eq!(offered_present_modes(&[m, i]), [m, i, f]);
        assert_eq!(offered_present_modes(&[f]), [f]);
        assert_eq!(offered_present_modes(&[r, r]), [r, f]);
    }

    /// The area the frame generator is told about is the guest's side of a
    /// transfer into a swapchain image, whichever way the offsets run.
    #[test]
    fn guest_area_comes_from_the_source_side_of_a_transfer() {
        let mut blit: VkImageBlit = unsafe { std::mem::zeroed() };
        blit.srcOffsets[0] = VkOffset3D { x: 0, y: 0, z: 0 };
        blit.srcOffsets[1] = VkOffset3D { x: 1280, y: 720, z: 1 };
        assert_eq!(blit_source(1, &blit), (1280, 720));
        blit.srcOffsets[0] = VkOffset3D { x: 640, y: 360, z: 0 };
        blit.srcOffsets[1] = VkOffset3D { x: 0, y: 0, z: 1 };
        assert_eq!(blit_source(1, &blit), (640, 360));
        assert_eq!(blit_source(0, &blit), (0, 0));
        assert_eq!(blit_source(1, std::ptr::null()), (0, 0));

        let mut copy: VkImageCopy = unsafe { std::mem::zeroed() };
        copy.extent = VkExtent3D {
            width: 960,
            height: 540,
            depth: 1,
        };
        assert_eq!(copy_size(1, &copy), (960, 540));
        assert_eq!(copy_size(0, &copy), (0, 0));
        assert_eq!(copy_size(1, std::ptr::null()), (0, 0));
    }
}

// ---- reconstructed from HEAD (items missing after the damaged edit) ----

// First-N diagnostics for the present pipeline handoffs (acquire ->
// present -> worker commit -> compositor release). Bounded counters, so
// steady-state frame pacing stays silent in logcat.
use std::ptr;
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

pub const GN_MAX_SC_IMAGES: usize = 8;
pub const GN_MAX_PRESENT: usize = 8; // swapchainCount handled per vkQueuePresentKHR
pub const GN_JOB_CAP: usize = 16;

/// What the surface capabilities advertise as `minImageCount`, so a guest that
/// honours them asks for a count with the depth it needs and reads back exactly
/// that. Mesa's Wayland WSI: "3 images is a sound default for everything except
/// MAILBOX" (`WSI_WL_DEFAULT_NUM_IMAGES`).
pub const SC_DEFAULT_IMAGES: u32 = 3;
/// Mesa's `WSI_WL_BUMPED_NUM_IMAGES`: "For true mailbox mode, we need at least
/// 4 images: one to scan out from, one queued for scan-out, one held by the
/// compositor, one to render to." Creation raises a mailbox swapchain to this.
pub const SC_MAILBOX_IMAGES: u32 = 4;
/// The source images a generation session gets when the guest asks for fewer:
/// the source, its destination, and the one the compositor still holds.
///
/// Not the size of the generation ring. That ring is the slots *behind* the
/// guest's images, grown on demand by `lsfg_destinations` up to
/// `gen_destination_cap`, and raising this floor changes nothing about it — the
/// run that raised it to 4 measured `generated 8` either way. What starved
/// generation was a parked destination, never a missing source slot.
pub const SC_FRAMEGEN_IMAGES: u32 = 3;

/// The images a swapchain gets for a guest that asked `asked`, kept legal (at
/// least two) and inside `ceiling`.
///
/// Mesa's convention, followed here because the shim is the one writing the
/// capabilities: the depth lives in `minImageCount` ([`SC_DEFAULT_IMAGES`]) and
/// creation honours the ask. The only bump is the mailbox one
/// ([`SC_MAILBOX_IMAGES`]) — Mesa makes exactly that one, and only when its
/// wayland FIFO manager is present, which is the case its own create path
/// matches. A guest that sizes its per-image state from the ask therefore gets
/// its own number back from `vkGetSwapchainImagesKHR`; that mismatch is what
/// Mesa's `vk_x11_strict_image_count` list exists to avoid, and it is why the
/// count is no longer asked-for-plus-one.
fn swapchain_slots(asked: u32, ceiling: u32, mode: VkPresentModeKHR) -> u32 {
    let base = asked.clamp(2, ceiling.max(2));
    if mode == VK_PRESENT_MODE_MAILBOX_KHR {
        base.max(SC_MAILBOX_IMAGES.min(ceiling.max(2)))
    } else {
        base
    }
}

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
/// The WSI policy's line, once per process for devices and for instances.
/// Printed whatever `GNDRM_LOG_VKSHIM` says (`logi!`), like the
/// driver-selection lines: the gamescope session never carries that variable.
static WSI_DEVICE_LOGGED: AtomicBool = AtomicBool::new(false);
static WSI_INSTANCE_LOGGED: AtomicBool = AtomicBool::new(false);

static ROBUSTNESS2_FAKED: AtomicBool = AtomicBool::new(false);

/// gamescope's DRM nodes are the daemon's, so it must not see the real ones a
/// driver reports through `VK_EXT_physical_device_drm` (PowerVR does): it
/// would open the real render node through the preload shim, which only
/// serves the daemon's.
static HIDE_DEVICE_DRM: AtomicBool = AtomicBool::new(false);

pub fn set_hide_device_drm(on: bool) {
    HIDE_DEVICE_DRM.store(on, Ordering::Relaxed);
}

pub fn set_fake_robustness2(on: bool) {
    FAKE_ROBUSTNESS2.store(on, Ordering::Relaxed);
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
    /// The apiVersion the app asked for (`VkApplicationInfo`), 0 when it gave none.
    pub api_version: u32,
    /// The apiVersion the shim handed the system loader (`api=` on the
    /// instance-create line), 0 when the app gave none.
    pub forwarded_api: u32,
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
    /// The largest area the guest has blitted into a swapchain image, so the
    /// engine can size its working resolution to what the guest draws rather
    /// than to the window it draws into. Zero until the first present blit.
    pub guest_peak: (AtomicU32, AtomicU32),
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
    /// The panel latch point the last queued frame was placed on, so the next
    /// one takes a later point; 0 before any frame was placed on the grid.
    pub gen_last_latch_ns: u64,
    /// The acquire count when the last batch ran. A batch belongs to a source
    /// frame, so a guest presenting faster than its cap must not add batches of
    /// its own: those would multiply the surplus instead of the cap.
    pub gen_last_acq: u64,
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
    // The XID winex11 named, kept for the re-ask in
    // `bind_window_at_first_commit`: the resolution rule reads the tree, not the
    // id, so asking again needs the window the guest pointed at rather than the
    // answer it produced.
    pub named_window: u32,
    // `window` came out of the resolution rule's fallback, which reads the tree
    // as it is at surface creation. A legitimate root-child ancestor is not a
    // guess (`false`), and is never re-resolved.
    pub window_provisional: bool,
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
    // Signalled by the present's own submit and waited by the one that signals
    // the guest's present fence (`VkSwapchainPresentFenceInfo`, `crate::maint1`).
    pub fence_sem: VkSemaphore,
    pub acquired: bool, // owned by the app (acquire -> present)
    pub in_flight: bool,  // committed, awaiting wl_buffer.release
    // When the compositor released the buffer and the daemon had not yet
    // reported SurfaceFlinger's release, `CLOCK_MONOTONIC` ns; 0 when the slot is
    // not waiting on that. The compositor is done *compositing* with it, which is
    // before the display is done *scanning* it out (see `crate::scanout`), so the
    // slot stays busy until the note arrives or this ages out.
    pub parked_ns: u64,
    /// When this slot's frame was committed to the compositor, mono ns.
    ///
    /// The anchor a release note is matched against. Not `parked_ns`: the park is
    /// taken on the compositor's own release, while the daemon's note comes from
    /// SurfaceFlinger's previous-release fence, which signals when the *next*
    /// buffer latches — before the compositor tells us it is done with this one.
    /// Measured on Witcher 3 under Odin 3: notes and park timeouts paired 1:1
    /// (`rnotes=707`, `park_timeouts=705`) with `notelag` empty on every bucket,
    /// i.e. the note describing each frame arrived before the park and was dropped
    /// as stale, leaving every slot to wait out the bound (23.6 fps, slot wait
    /// 25 ms per frame).
    pub commit_ns: u64,
    // Guest-side trace: the acquire sequence that filled this slot, so a
    // present can be tied back to the acquire that handed the guest the image.
    pub acq_seq: u64,
    // When the gate let the guest's acquire of this slot go, `CLOCK_MONOTONIC`
    // ns; 0 when no gate ran. Read once at the commit, for the census's
    // acquire-to-commit latency.
    pub acquired_ns: u64,
    // The guest's present id for the frame currently in this slot, 0 when it
    // chained none: read at the commit to mark the frame for the daemon, which
    // reports it back once the transaction presenting it latched.
    pub present_id: u64,
    // The same id, kept until the slot comes back: the compositor's release is
    // the one completion every route observes, so a wait on it is answered
    // there when the daemon's latch report has not arrived first.
    pub wait_id: u64,
    // Whether the commit that carries `wait_id` has registered it with
    // `crate::present` yet. The compositor's release of this slot's buffer can
    // arrive between the present that armed the id and the commit that
    // registers it — it may belong to an earlier use of the same buffer — and
    // consuming the id there left the wait with no commit, no grace and no bound.
    pub wait_registered: bool,
    // The daemon's scan-out inode for this slot's buffer, or 0 when the buffer
    // was allocated locally. Identity and logging only: the release floor is
    // this shim's own, one period after the last release, and is not filed
    // against a buffer (see `crate::scanout::note_release`).
    pub ino: u64,

    // The slot's `AHardwareBuffer` reference on a device that imports through
    // `VK_ANDROID_external_memory_android_hardware_buffer`, null otherwise.
    pub ahb: *mut c_void,

    // The hidden slot this guest slot's frame is being copied to, `NO_SPARE` when
    // none. Set by the present, cleared by the worker when the copy has finished.
    pub spare: i32,
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
    /// The present mode the guest asked for, normalised to the four served
    /// modes ([`guest_present_mode`]). Read by `present_dispatch` on every
    /// present.
    pub present_mode: VkPresentModeKHR,
    /// Every slot the shim owns: the guest's first (`guest_slots` of them), then
    /// the guest's own images, then one ring behind them shared by a strict
    /// game's hidden copies and generation's destinations (`dest_ring`). The
    /// guest sees only its own.
    pub slots: Vec<GnSlot>,
    /// How many leading entries of `slots` are the guest's swapchain images.
    pub guest_slots: u32,
    /// How many ring slots the copy path may use, counted from `guest_slots`
    /// (`crate::image_count::hidden_slot_count`); 0 when there are none. The
    /// ring itself may be larger when generation's destinations need more.
    pub spare_slots: u32,
    /// Made at the first present of a swapchain with hidden slots.
    pub spare: Option<SpareCopy>,
    pub worker: Option<std::thread::JoinHandle<()>>,
    pub shared: Mutex<ScShared>,
    pub cv_put: Condvar,
    pub cv_free: Condvar,
    pub x11_trace: u32,
    /// Set by `bind_window_at_first_commit`, under `SEND_LOCK`.
    pub window_bound: bool,
    pub next: *mut GnSwapchain,
    /// Set once a destination could not be made: the set stops growing.
    pub gen_no_grow: bool,
    /// When the guest may start its next frame (`release.next_ns`,
    /// `CLOCK_MONOTONIC` ns, 0 before the first), advanced by each gated
    /// acquire. One timeline per swapchain, not per slot — a slot-local schedule
    /// would let a ring of N images run N frames back to back.
    pub release: crate::vsync_gate::Schedule,
    /// The guest's acquire-to-commit time, smoothed over gated frames
    /// (`crate::vsync_gate::smooth_latency`); 0 until one was measured. Written
    /// by the present worker, read by the acquire.
    pub commit_latency_ns: AtomicU64,
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
        } else if iface == b"wp_presentation" && !use_guest_surface() {
            // gamescope's presentation feedback, the present-wait answer on the
            // override route (see `crate::present`). Bound only there, like the
            // swapchain factory.
            let p = wlproto::registry_bind(reg, id, &wlproto::WP_PRESENTATION_INTERFACE, 1);
            if !p.is_null() {
                wlproto::wl_proxy_add_listener(
                    p,
                    PRESENTATION_LISTENER.as_ptr() as *mut _,
                    ptr::null_mut(),
                );
                PRESENTATION.store(p, Ordering::Release);
            }
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
/// The bound `wp_presentation`, null while the compositor offers none (or the
/// guest-surface route is in use): then present waits keep the release and grace
/// answers.
static PRESENTATION: AtomicPtr<wl_proxy> = AtomicPtr::new(ptr::null_mut());
/// The clock `wp_presentation` stamps with (`CLOCK_MONOTONIC` = 1 on gamescope);
/// `u32::MAX` until announced.
static PRESENTATION_CLOCK: AtomicU32 = AtomicU32::new(u32::MAX);

extern "C" fn presentation_clock_id(_data: *mut c_void, _p: *mut wl_proxy, clk: u32) {
    PRESENTATION_CLOCK.store(clk, Ordering::Relaxed);
}

static PRESENTATION_LISTENER: [RawListenerFn; 1] = [unsafe {
    std::mem::transmute::<extern "C" fn(*mut c_void, *mut wl_proxy, u32), RawListenerFn>(
        presentation_clock_id,
    )
}];

extern "C" fn feedback_sync_output(_data: *mut c_void, _fb: *mut wl_proxy, _output: *mut wl_proxy) {}

/// `presented`: the frame whose feedback token is `data` was latched for
/// the refresh at `tv`. The feedback object is spent either way.
#[allow(clippy::too_many_arguments)] // the event's own arity
extern "C" fn feedback_presented(
    data: *mut c_void,
    fb: *mut wl_proxy,
    tv_sec_hi: u32,
    tv_sec_lo: u32,
    tv_nsec: u32,
    _refresh: u32,
    _seq_hi: u32,
    _seq_lo: u32,
    _flags: u32,
) {
    let token = data as usize as u64;
    let ns = if PRESENTATION_CLOCK.load(Ordering::Relaxed) == libc::CLOCK_MONOTONIC as u32 {
        ((u64::from(tv_sec_hi) << 32 | u64::from(tv_sec_lo)) * 1_000_000_000)
            .saturating_add(u64::from(tv_nsec))
    } else {
        monotonic_ns()
    };
    crate::present::feedback_presented(token, ns);
    unsafe { wlproto::wl_proxy_destroy(fb) };
}

/// `discarded`: the frame whose feedback token is `data` will never be
/// shown; its wait is answered now.
extern "C" fn feedback_discarded(data: *mut c_void, fb: *mut wl_proxy) {
    crate::present::feedback_discarded(data as usize as u64, monotonic_ns());
    unsafe { wlproto::wl_proxy_destroy(fb) };
}

static FEEDBACK_LISTENER: [RawListenerFn; 3] = [
    unsafe {
        std::mem::transmute::<extern "C" fn(*mut c_void, *mut wl_proxy, *mut wl_proxy), RawListenerFn>(
            feedback_sync_output,
        )
    },
    unsafe {
        std::mem::transmute::<
            extern "C" fn(*mut c_void, *mut wl_proxy, u32, u32, u32, u32, u32, u32, u32),
            RawListenerFn,
        >(feedback_presented)
    },
    unsafe {
        std::mem::transmute::<extern "C" fn(*mut c_void, *mut wl_proxy), RawListenerFn>(
            feedback_discarded,
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
    crate::logf!("wayland dispatch thread exited: {}",
        if err != 0 {
            std::io::Error::from_raw_os_error(err).to_string()
        } else if failed {
            std::io::Error::last_os_error().to_string()
        } else {
            "display closed".to_string()
        });
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
                crate::logf!("proxy wayland connect failed (WAYLAND_DISPLAY unset?)");
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
            crate::logf!("compositor misses zwp_linux_dmabuf_v1 v3 — proxy cannot present");
            (*wptr).failed = true;
        }
        if !(*wptr).failed {
            let spawned = std::thread::Builder::new()
                .name("gn-wl-dispatch".into())
                .spawn(dispatch_main);
            if spawned.is_err() {
                crate::logf!("failed to start wayland dispatch thread");
                (*wptr).failed = true;
            }
        }
        (*wptr).ready = !(*wptr).failed;
        !(*wptr).failed
    }
}

/// `clock_gettime(CLOCK_MONOTONIC)` in nanoseconds — the domain the daemon's
/// release floor is expressed in, so a floor is compared with no conversion.
pub(crate) fn monotonic_ns() -> u64 {
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
        let logging = crate::vkshim_enabled();
        if logging && LOG_RELEASE.fetch_add(1, Ordering::Relaxed) < LOG_FIRST_N {
            crate::logf!("proxy release: slot={slot:?}");
        }
        // The target may have changed since this shim connected, and a push
        // from the daemon is read here because a release is the one event this
        // shim sees on every frame.
        crate::scanout::poll_notes();
        // A release only frees the slot: the cap is applied when the guest asks
        // for the image again, so nothing here can delay a frame that is already
        // on its way to the panel.
        let guest_slot = {
            let guest = (*sc).guest_slots as usize;
            let slots = (*sc).slots.as_ptr();
            ((slot as usize - slots as usize) / core::mem::size_of::<GnSlot>()) < guest
        };
        let _g = (*sc).shared.lock().unwrap();
        // Counted whether or not verbose logging is on: this is the denominator
        // for a destination that never comes back. A destination is written by
        // the generator and handed to the compositor like any other frame, so it
        // must release like one; a census that shows `dests` pinned at its cap
        // with `gendrel` standing still is a destination leaked into `in_flight`,
        // not a ring that is merely too small.
        if !guest_slot {
            GEN_RELEASES.fetch_add(1, Ordering::Relaxed);
        }
        // The compositor has finished compositing with the buffer. That is not
        // the same as the display finishing reading it, which is what a producer
        // has to wait for: a buffer the daemon allocated is held again here until
        // the daemon reports the scan-out release, and freed on that note. A
        // buffer the daemon does not track (a local allocation, or a daemon too
        // old to send notes) is freed here, as before.
        //
        // Only for a synced declaration. `VkPresentModeKHR` is a contract about
        // when an image comes back, and FIFO is the only one that promises the
        // client a display-bound return: IMMEDIATE returns the image once it is
        // handed over, MAILBOX as soon as a newer one replaces it. Parking those
        // applies FIFO's lifetime to a mode that promised none, and the cost is
        // measurable — an uncapped immediate session on a 120 Hz panel ran at
        // `depth x refresh / hold` = 106 instead of free, with `slot_waits` at
        // 99 % and `in_flight` pinned at the depth. FIFO_RELAXED stays synced:
        // the spec lets it return a late frame early, and holding it to the note
        // is the conservative side of a row the table already calls synced.
        // The *served* mode, not the declared one: an immediate guest is served as
        // mailbox above, and a mailbox frame is replaced at a vblank rather than
        // shown mid-scan — so its buffer still has to survive its scan-out, exactly
        // as a FIFO frame's does. Keying this on the declaration would free the slot
        // at the compositor's release again and let the guest write into a buffer
        // the display is reading.
        let served = live_choice(sc).0;
        let synced = matches!(
            served,
            VK_PRESENT_MODE_FIFO_KHR
                | VK_PRESENT_MODE_FIFO_RELAXED_KHR
                | VK_PRESENT_MODE_MAILBOX_KHR
        );
        // A synced slot waits for the display's own release, which is the note.
        // No capability check stands in front of it: the note may never come (a
        // buffer the daemon's publish path coalesced away is never replaced), and
        // `slot_just_freed`'s bound is what answers that case — for the guest's
        // acquire and for the generator's destinations alike. A latch that
        // enabled parking only after one note arrived was removed for the same
        // reason: it turned a one-off note into a session-long decision.
        if synced && (*slot).ino != 0 {
            (*slot).parked_ns = monotonic_ns();
        } else {
            (*slot).in_flight = false;
        }
        (*sc).cv_free.notify_all();
        // Only a frame whose commit has registered its wait is answered here.
        // This release may belong to an earlier use of the same buffer: it can
        // land between the present arming the id and the worker registering it,
        // and consuming the id there is what left 87 % of waits with no commit
        // to bound them — the guest then waited out the compositor's feedback.
        let wait_id = if (*slot).wait_registered {
            (*slot).wait_registered = false;
            core::mem::replace(&mut (*slot).wait_id, 0)
        } else {
            0
        };
        drop(_g);
        REL_ALL_N.fetch_add(1, Ordering::Relaxed);
        if wait_id != 0 {
            crate::present::complete_on_release(wait_id, sc as usize, monotonic_ns());
        }
        if logging {
            N_REL.fetch_add(1, Ordering::Relaxed);
            if rate_ok(&RATE_REL, RATE_EVERY) {
                crate::logf!("gn-vkshim release: slot={slot:?} {}",
                    slot_flags(sc));
            }
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
    crate::logf!("{}", line);
}

/// A handle the shim could not resolve, against the wrapper table it searched,
/// so a wrong-handle miss names both values in one line.
unsafe fn handle_note(found: bool, searched: usize, first_wrapper: usize, n: usize) -> String {
    format!(
        "{}",
        format_args!("found={found} searched={searched:#x} wrappers={n} first={first_wrapper:#x} pid={}",
            std::process::id())
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
        crate::logf!("proxy: BUG submit carries an untracked wrapper CB {cmd:?}");
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
            crate::logf!("proxy nested translate: {name} ({n} handle(s))");
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

/// The existence gate every device-level lookup goes through: the real
/// instance's `vkGetDeviceProcAddr` on the real device. NULL here is NULL to
/// the guest. The resolve probe (`crate::resolve_probe`) asks the same gate on
/// its own device, so what it finds missing is what the guest would.
pub(crate) unsafe fn device_gate(
    gdp: PFN_vkGetDeviceProcAddr,
    dev: VkDevice,
    name: &CStr,
) -> PFN_vkVoidFunction {
    unsafe { gdp(dev, name.as_ptr()) }
}

/// The other registry spellings of `name`: the alias the shim has always
/// tried first (`vk_dispatch_gen::alias_of`), then the rest of its alias group
/// (the core name and every extension spelling of it). The registry has no
/// alias chains, so one level is the whole group.
fn other_spellings(name: &str) -> impl Iterator<Item = &'static str> {
    use crate::vk_dispatch_gen::VK_ALIASES;
    let target = VK_ALIASES
        .binary_search_by(|(n, _)| n.cmp(&name))
        .ok()
        .map(|i| VK_ALIASES[i].1);
    let root = target.unwrap_or(name);
    let mut group: Vec<&'static str> = target.into_iter().collect();
    for (n, t) in VK_ALIASES {
        if *t == root && *n != name {
            group.push(n);
        }
    }
    // `alias_of` first, so the guest path tries what it has always tried.
    if let Some(f) = crate::vk_dispatch_gen::alias_of(name) {
        if let Some(i) = group.iter().position(|g| *g == f) {
            group.remove(i);
            group.insert(0, f);
        }
    }
    group.into_iter()
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
        // With a device at hand its own vkGetDeviceProcAddr decides whether a
        // name exists: that is the one resolver the loader would have asked,
        // and the one that knows which extensions and core version this
        // device enabled. The instance resolver is not safe for device-level
        // names: mesa answers them with trampolines whether or not the device
        // has the extension (VK_ARM_tensors on turnip), and for
        // vkCmdEndRendering2EXT its lookup dereferences NULL outright.
        // winevulkan resolves every device entry point by name right after
        // vkCreateDevice, which is how a 32-bit guest ended in a fault inside
        // that call and an abort on assert(!status) (Terraria under gamescope
        // on turnip, second device from the Steam overlay). dlsym is no
        // better as an existence check: Android 16's libvulkan.so exports the
        // 1.4 core names, so vkMapMemory2 resolved to a trampoline into an
        // empty dispatch slot on a 1.3 device. The device only gates; which
        // implementation is handed back still prefers dlsym for the hooks.
        // The instance resolver stays for the device-less callers.
        let mut from_device = None;
        if !gd.is_null() {
            if let Some(gdp) = unsafe { (*(*gd).inst).f.gdp } {
                from_device = unsafe { device_gate(gdp, (*gd).real, n) };
                if from_device.is_none() {
                    return (None, "miss");
                }
            }
        }
        if !handle.is_null() && !prefer_driver {
            let sym = unsafe { libc::dlsym(handle, n.as_ptr()) };
            if !sym.is_null() {
                return (
                    Some(unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn()>(sym) }),
                    "dlsym",
                );
            }
        }
        if from_device.is_some() {
            return (from_device, "gdpa");
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
    // Core names the driver only exports under an extension spelling, and the
    // reverse. The registry's alias group, one level (the registry has no
    // alias chains): `alias_of` first, as always, then the other spellings
    // (vkCmdSetLineStipple has a KHR and an EXT one). Never recursive:
    // alias_of maps both ways, so recursing ping-pongs forever when no
    // spelling resolves.
    if fnptr.is_none() {
        if let Ok(n) = name.to_str() {
            for alias in other_spellings(n) {
                let c = std::ffi::CString::new(alias).unwrap();
                fnptr = lookup(&c).0;
                if fnptr.is_some() {
                    via = "alias";
                    break;
                }
            }
        }
    }
    // The image/memory entry points are where the guest's calls silently do
    // nothing, so record where each one came from.
    let n = name.to_string_lossy();
    if (n.contains("Copy")
        || n.contains("Clear")
        || n.contains("Bind")
        || n.contains("Allocate")
        || n.contains("Map")
        || n.contains("CreateImage")
        || n.contains("SubresourceLayout"))
        && REAL_PROC_SEEN.first(gd as usize, &n)
    {
        crate::logf!("proxy: real proc {n} via {via} dev={:#x}",
            gd as usize);
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

/// The chain link the guest's device creates carry to the hooks (see
/// `G_CHAIN_DEV_LINK`); the resolve probe's device carries the same one.
pub(crate) fn chain_dev_link() -> *mut VkLayerDeviceLink {
    &raw const G_CHAIN_DEV_LINK.0 as *mut _
}

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

/// Set when the probed device is Vulkan 1.1+ and does not list
/// `VK_KHR_external_memory`: the extension is core there, so its absence from
/// the list does not mean the driver lacks it.
static EXTERNAL_MEMORY_CORE: AtomicBool = AtomicBool::new(false);

/// How swapchain slots reach the driver, from its device extensions: `Some(false)`
/// for dma-buf import, `Some(true)` for `AHardwareBuffer` import (only when
/// `allow_ahb`), `None` when neither set is complete.
fn import_mode(has: &dyn Fn(&CStr) -> bool, allow_ahb: bool) -> Option<bool> {
    let core = EXTERNAL_MEMORY_CORE.load(Ordering::Relaxed);
    let has = |e: &CStr| has(e) || (core && e == c"VK_KHR_external_memory");
    let missing = DMA_BUF_EXTS.iter().copied().find(|e| !has(e));
    let Some(missing) = missing else {
        return Some(false);
    };
    if allow_ahb && AHB_EXTS.iter().all(|e| has(e)) {
        return Some(true);
    }
    crate::log_reason!("proxy prereq: device lacks {}",
        missing.to_str().unwrap());
    None
}

fn dev_exts_satisfied(
    enum_de: PFN_vkEnumerateDeviceExtensionProperties,
    pd: VkPhysicalDevice,
    device_api: u32,
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
    if device_api >= VK_API_VERSION_1_1 && !has(c"VK_KHR_external_memory") {
        EXTERNAL_MEMORY_CORE.store(true, Ordering::Relaxed);
        crate::logi!("proxy prereq: VK_KHR_external_memory not listed, core in device api {}",
            api_str(device_api));
    }
    match import_mode(&has, allow_ahb) {
        Some(true) => {
            crate::logi!("proxy prereq: no dma-buf import, using AHardwareBuffer import");
            true
        }
        Some(false) => true,
        None => false,
    }
}

/// `allow_ahb` admits a driver that can import only `AHardwareBuffer`s. Only a
/// guest process may take that: gamescope itself imports client dma-bufs.
pub fn device_prereqs(gipa: GipaFn, allow_ahb: bool) -> bool {
    log_real_instance_surface(gipa);
    let create_inst: Option<PFN_vkCreateInstance> =
        unsafe { std::mem::transmute(gipa(ptr::null_mut(), c"vkCreateInstance".as_ptr())) };
    let Some(create_inst) = create_inst else {
        crate::log_reason!("proxy prereq: driver exposes no vkCreateInstance");
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
    crate::logi!("proxy prereq: probe vkCreateInstance api={} layers=[] exts=[] -> {res}",
        api_str(app.apiVersion));
    if res != VK_SUCCESS {
        crate::log_reason!("proxy prereq: probe vkCreateInstance failed ({res})");
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
    let get_props: Option<PFN_vkGetPhysicalDeviceProperties> =
        unsafe { std::mem::transmute(gipa(inst, c"vkGetPhysicalDeviceProperties".as_ptr())) };
    let mut ok = false;
    match (enum_pds, enum_de, destroy_inst) {
        (Some(enum_pds), Some(enum_de), _) => {
            let mut n: u32 = 0;
            if unsafe { enum_pds(inst, &mut n, ptr::null_mut()) } == VK_SUCCESS && n > 0 {
                let mut pd: VkPhysicalDevice = ptr::null_mut();
                let mut one: u32 = 1;
                let r = unsafe { enum_pds(inst, &mut one, &mut pd) };
                if r == VK_SUCCESS || r == VK_INCOMPLETE {
                    let device_api = get_props.map_or(0, |f| unsafe {
                        let mut p: VkPhysicalDeviceProperties = std::mem::zeroed();
                        f(pd, &mut p);
                        crate::logi!("proxy prereq: device '{}' api {} vendor {:#x} device {:#x} driver {:#x} ({n} physical devices)",
                            CStr::from_ptr(p.deviceName.as_ptr()).to_string_lossy(),
                            api_str(p.apiVersion),
                            p.vendorID,
                            p.deviceID,
                            p.driverVersion);
                        p.apiVersion
                    });
                    ok = dev_exts_satisfied(enum_de, pd, device_api, allow_ahb);
                } else {
                    crate::log_reason!("proxy prereq: vkEnumeratePhysicalDevices failed ({r})");
                }
            } else {
                crate::log_reason!("proxy prereq: no physical devices");
            }
        }
        _ => crate::log_reason!("proxy prereq: instance-level procs unresolved"),
    }
    if let Some(destroy_inst) = destroy_inst {
        unsafe { destroy_inst(inst, ptr::null()) };
    }
    if ok {
        AHB_IMPORT_ALLOWED.store(allow_ahb, Ordering::Relaxed);
    }
    crate::logi!("proxy prereq: {}",
        if ok {
            "passed".to_string()
        } else {
            format!("FAILED ({})", crate::reason())
        });
    ok
}

// ------------------------------------------- the real loader's instance surface

fn api_str(v: u32) -> String {
    format!("{}.{}.{}", (v >> 22) & 0x7F, (v >> 12) & 0x3FF, v & 0xFFF)
}

fn join_names<S: AsRef<CStr>>(names: &[S]) -> String {
    names
        .iter()
        .map(|n| n.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(",")
}

/// The real loader's instance version; 1.0 when it has no
/// `vkEnumerateInstanceVersion`.
fn real_instance_version(gipa: GipaFn) -> u32 {
    let f: Option<PFN_vkEnumerateInstanceVersion> = unsafe {
        std::mem::transmute(gipa(ptr::null_mut(), c"vkEnumerateInstanceVersion".as_ptr()))
    };
    let Some(f) = f else {
        return VK_API_VERSION_1_0;
    };
    let mut v: u32 = 0;
    if unsafe { f(&mut v) } == VK_SUCCESS && v != 0 {
        v
    } else {
        VK_API_VERSION_1_0
    }
}

/// The real loader's global instance extensions, `None` when it cannot list them.
fn real_instance_exts(gipa: GipaFn) -> Option<Vec<CString>> {
    let f: Option<PFN_vkEnumerateInstanceExtensionProperties> = unsafe {
        std::mem::transmute(gipa(
            ptr::null_mut(),
            c"vkEnumerateInstanceExtensionProperties".as_ptr(),
        ))
    };
    let f = f?;
    let mut n: u32 = 0;
    if unsafe { f(ptr::null(), &mut n, ptr::null_mut()) } != VK_SUCCESS {
        return None;
    }
    let mut props = vec![
        VkExtensionProperties {
            extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
            specVersion: 0,
        };
        n as usize
    ];
    let r = unsafe { f(ptr::null(), &mut n, props.as_mut_ptr()) };
    if r != VK_SUCCESS && r != VK_INCOMPLETE {
        return None;
    }
    Some(
        props[..n as usize]
            .iter()
            .map(|p| unsafe { CStr::from_ptr(p.extensionName.as_ptr()) }.to_owned())
            .collect(),
    )
}

/// The real loader's instance layers, `None` when it cannot list them.
fn real_instance_layers(gipa: GipaFn) -> Option<Vec<CString>> {
    let f: Option<PFN_vkEnumerateInstanceLayerProperties> = unsafe {
        std::mem::transmute(gipa(
            ptr::null_mut(),
            c"vkEnumerateInstanceLayerProperties".as_ptr(),
        ))
    };
    let f = f?;
    let mut n: u32 = 0;
    if unsafe { f(&mut n, ptr::null_mut()) } != VK_SUCCESS {
        return None;
    }
    let mut props = vec![
        VkLayerProperties {
            layerName: [0; VK_MAX_EXTENSION_NAME_SIZE],
            specVersion: 0,
            implementationVersion: 0,
            description: [0; VK_MAX_DESCRIPTION_SIZE],
        };
        n as usize
    ];
    let r = unsafe { f(&mut n, props.as_mut_ptr()) };
    if r != VK_SUCCESS && r != VK_INCOMPLETE {
        return None;
    }
    Some(
        props[..n as usize]
            .iter()
            .map(|p| unsafe { CStr::from_ptr(p.layerName.as_ptr()) }.to_owned())
            .collect(),
    )
}

fn log_real_instance_surface(gipa: GipaFn) {
    crate::logi!("system loader: instance version {}",
        api_str(real_instance_version(gipa)));
    match real_instance_exts(gipa) {
        Some(e) => crate::logi!("system loader: {} instance extensions: {}",
            e.len(),
            join_names(&e)),
        None => crate::logi!("system loader: instance extensions unreadable"),
    }
    match real_instance_layers(gipa) {
        Some(l) => crate::logi!("system loader: {} instance layers: {}",
            l.len(),
            join_names(&l)),
        None => crate::logi!("system loader: instance layers unreadable"),
    }
}

/// Splits `requested` into the names `known` lists and the ones it does not.
/// An unreadable list (`None`) keeps every name.
fn split_known<'a>(requested: &[&'a CStr], known: Option<&[CString]>) -> (Vec<&'a CStr>, Vec<&'a CStr>) {
    let Some(known) = known else {
        return (requested.to_vec(), Vec::new());
    };
    requested
        .iter()
        .partition(|r| known.iter().any(|k| k.as_c_str() == **r))
}

/// The apiVersion to hand a loader whose own instance version is `loader`. A
/// 1.0 implementation may refuse anything above 1.0 with
/// VK_ERROR_INCOMPATIBLE_DRIVER; from 1.1 on any value must be accepted, so it
/// is passed through unchanged.
fn forward_api_version(requested: u32, loader: u32) -> u32 {
    if loader & !0xFFF < VK_API_VERSION_1_1 && requested & !0xFFF > VK_API_VERSION_1_0 {
        VK_API_VERSION_1_0
    } else {
        requested
    }
}

/// The pNext chain's sTypes (and the loader-info `function` where there is
/// one), for the create-info log line.
unsafe fn pnext_str(mut p: *const VkBaseOutStructure) -> String {
    let mut out = Vec::new();
    while !p.is_null() && out.len() < 32 {
        unsafe {
            if (*p).sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO {
                let lci = p.cast::<VkLayerInstanceCreateInfo>();
                out.push(format!("{}/{}", (*p).sType, (*lci).function));
            } else {
                out.push(format!("{}", (*p).sType));
            }
            p = (*p).pNext;
        }
    }
    out.join(",")
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
    let real_exts = real_instance_exts(gipa);
    if !WSI_INSTANCE_LOGGED.swap(true, Ordering::Relaxed) {
        crate::logi!("proxy instance: WSI extensions hidden from the guest: [{}] requested and masked: [{}]",
            crate::wsi_policy::hidden_names(
                real_exts.iter().flatten().map(|e| e.as_c_str())
            ),
            crate::wsi_policy::hidden_names(names.iter().map(|&o| unsafe { CStr::from_ptr(o) })));
    }
    for &name in names {
        let n_ = unsafe { CStr::from_ptr(name) };
        // Proxied, not real (the shim's surfaces), or hidden by the WSI
        // policy (`crate::wsi_policy`).
        if crate::wsi_policy::driver_copy_hidden(n_) {
            continue;
        }
        if n < exts.len() {
            exts[n] = name;
            n += 1;
        }
    }
    // Forward only what the real loader lists: an extension the proxy answers
    // itself (or a layer the outer loader knows and the system one does not)
    // would make it refuse the whole instance.
    let wanted: Vec<&CStr> = exts[..n].iter().map(|&e| unsafe { CStr::from_ptr(e) }).collect();
    let (fwd_exts, dropped_exts) = split_known(&wanted, real_exts.as_deref());
    let layers: Vec<&CStr> = unsafe {
        slice_or_empty(ci.ppEnabledLayerNames, ci.enabledLayerCount as usize)
    }
    .iter()
    .map(|&l| unsafe { CStr::from_ptr(l) })
    .collect();
    let real_layers = if layers.is_empty() {
        None
    } else {
        real_instance_layers(gipa)
    };
    let (fwd_layers, dropped_layers) = split_known(&layers, real_layers.as_deref());
    let ext_ptrs: Vec<*const c_char> = fwd_exts.iter().map(|e| e.as_ptr()).collect();
    let layer_ptrs: Vec<*const c_char> = fwd_layers.iter().map(|l| l.as_ptr()).collect();
    let mut ci2 = VkInstanceCreateInfo { ..*ci };
    ci2.enabledExtensionCount = ext_ptrs.len() as u32;
    ci2.ppEnabledExtensionNames = if ext_ptrs.is_empty() { ptr::null() } else { ext_ptrs.as_ptr() };
    ci2.enabledLayerCount = layer_ptrs.len() as u32;
    ci2.ppEnabledLayerNames = if layer_ptrs.is_empty() { ptr::null() } else { layer_ptrs.as_ptr() };
    let app_api = if ci.pApplicationInfo.is_null() {
        0
    } else {
        unsafe { (*ci.pApplicationInfo).apiVersion }
    };
    unsafe { crate::image_count::note_instance(ci.pApplicationInfo) };
    let loader_api = real_instance_version(gipa);
    let fwd_api = forward_api_version(app_api, loader_api);
    let app2;
    if fwd_api != app_api {
        let a = unsafe { &*ci.pApplicationInfo };
        app2 = VkApplicationInfo {
            sType: a.sType,
            pNext: a.pNext,
            pApplicationName: a.pApplicationName,
            applicationVersion: a.applicationVersion,
            pEngineName: a.pEngineName,
            engineVersion: a.engineVersion,
            apiVersion: fwd_api,
        };
        ci2.pApplicationInfo = &app2;
    }
    if !dropped_exts.is_empty() || !dropped_layers.is_empty() {
        crate::logi!("proxy instance create: dropping exts=[{}] layers=[{}] the system loader does not list",
            join_names(&dropped_exts),
            join_names(&dropped_layers));
    }
    crate::logi!("proxy instance create: api={} (app {}, system loader {}) flags={:#x} layers=[{}] exts=[{}] pNext=[{}]",
        api_str(fwd_api),
        api_str(app_api),
        api_str(loader_api),
        ci.flags,
        join_names(&fwd_layers),
        join_names(&fwd_exts),
        unsafe { pnext_str(ci.pNext.cast()) });
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
    crate::logi!("proxy instance create: system loader returned {res}");
    if res != VK_SUCCESS {
        return res;
    }
    let api_version = app_api;
    let gi = Box::new(GnInstance {
        real: unsafe { *out },
        api_version,
        forwarded_api: fwd_api,
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
    crate::logf!("proxy instance {:?} (headless WSI backing)",
        unsafe { *out });
    if !LOUD_INSTANCE.swap(true, Ordering::Relaxed) {
        crate::logf!("proxy: instance created pid={} real={:?} npds={}",
            std::process::id(),
            unsafe { *out },
            unsafe { (*gi).n_pds });
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
    // Extensions the proxy answers itself (`crate::wsi_policy`): the three
    // surface extensions — winewayland clients use the wayland one, winex11
    // clients (the gamescope path) the xlib one, and Mesa's zink WSI the xcb
    // one — and the surface-maintenance pair `VK_KHR_swapchain_maintenance1`
    // depends on (the shim's own surface-capability query answers its structs,
    // `crate::maint1::surface_caps2`). The real driver never sees them; its
    // copies are dropped, together with every WSI extension the policy hides.
    // `VK_EXT_swapchain_colorspace` is the driver's copy only, never added: it
    // defines the PQ colourspace the surface formats advertise in an HDR
    // session, but exposing the shim's copy made vkd3d-proton titles present
    // once per swapchain and then stop, while the PQ surface format alone is
    // enough for DXVK to report an HDR10 output.
    let added: Vec<(&CStr, u32)> =
        crate::wsi_policy::shim_advertised(crate::vk_dispatch_gen::ExtKind::Instance).collect();
    unsafe {
        let mut n: u32 = 0;
        let res = real_enum(layer, &mut n, ptr::null_mut());
        if res != VK_SUCCESS {
            return res;
        }
        let mut buf = vec![
            VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: 0,
            };
            n as usize + added.len()
        ];
        let mut got = n;
        let res = real_enum(layer, &mut got, buf.as_mut_ptr());
        if res != VK_SUCCESS && res != VK_INCOMPLETE {
            return res;
        }
        let mut kept = 0usize;
        for i in 0..(got as usize).min(n as usize) {
            let name = CStr::from_ptr(buf[i].extensionName.as_ptr());
            if crate::wsi_policy::driver_copy_hidden(name) {
                continue;
            }
            buf[kept] = buf[i];
            kept += 1;
        }
        for (name, spec) in &added {
            let mut prop = VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: *spec,
            };
            for (j, b) in name.to_bytes().iter().enumerate() {
                prop.extensionName[j] = *b as c_char;
            }
            buf[kept] = prop;
            kept += 1;
        }
        if pProps.is_null() {
            *pCount = kept as u32;
            return VK_SUCCESS;
        }
        let n_copy = (*pCount as usize).min(kept);
        ptr::copy_nonoverlapping(buf.as_ptr(), pProps, n_copy);
        *pCount = n_copy as u32;
        if n_copy < kept {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        }
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
        // gamescope, `VK_EXT_robustness2` when the driver does not list them,
        // plus the WSI extensions it serves itself (`crate::wsi_policy`) and
        // the placed-memory and divisor names.
        let shim_wsi: Vec<(&CStr, u32)> =
            crate::wsi_policy::shim_advertised(crate::vk_dispatch_gen::ExtKind::Device).collect();
        let mut buf = vec![
            VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: 0,
            };
            n as usize + 4 + shim_wsi.len()
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
            // The WSI policy: hidden names, and the driver's copies of the
            // names the shim advertises itself.
            if crate::wsi_policy::driver_copy_hidden(name) {
                continue;
            }
            if name == c"VK_EXT_physical_device_drm" && HIDE_DEVICE_DRM.load(Ordering::Relaxed) {
                crate::logf!("proxy: hiding VK_EXT_physical_device_drm from gamescope");
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
            crate::logf!("proxy: advertised VK_EXT_hdr_metadata (driver does not list it)");
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
                crate::logf!("proxy: advertised VK_EXT_robustness2 (driver does not list it)");
            }
        }
        // Present ids, present waits and swapchain maintenance are served by the
        // shim for its own swapchain: the driver's swapchain is not the one the
        // guest presents through, so its support says nothing. The driver's
        // copies were skipped above; the shim's are advertised here.
        for &(name, spec) in &shim_wsi {
            if !layer.is_null() {
                continue;
            }
            let mut prop = VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: spec,
            };
            for (j, b) in name.to_bytes().iter().enumerate() {
                prop.extensionName[j] = *b as std::os::raw::c_char;
            }
            buf[kept] = prop;
            kept += 1;
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
        crate::logf!("proxy surface create FAILED: instance {instance:?} not one of ours");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    if !wl_setup(ci.display.cast()) {
        crate::logf!("proxy surface create FAILED: wayland setup failed");
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
        named_window: 0,
        window_provisional: false,
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
        crate::logf!("proxy surface {:?} (direct attach on wl_surface {:?})",
            s,
            ci.surface);
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
/// the guest-surface route's commit stream stalls when the guest's Vulkan
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
/// Creates the shim's OWN wl_surface and wraps it in a
/// gamescope_swapchain; a swapchain's first commit binds it to the guest's X
/// window via override_window_content (`bind_window_at_first_commit`), and the
/// compositor then routes this surface's commits as that window's content. Presenting therefore shows the guest's frames
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
    let target = unsafe { crate::x11::resolve_override_target(window) };
    crate::logf!("override: XID {window:#x} -> gamescope override surface on {:#x}{} (default route; guest GDI/2D not composited)",
        target.window,
        if target.provisional { ", provisional" } else { "" });
    if !wl_setup(ptr::null_mut()) {
        crate::logf!("proxy xlib surface create FAILED: wayland setup failed");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let (comp, factory, dpy) = {
        let w = G_WL.lock().unwrap();
        (w.comp, w.swapchain_factory, w.dpy)
    };
    if factory.is_null() {
        crate::logf!("compositor misses gamescope_swapchain_factory_v2 — cannot bind XID");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe {
        let _sg = SEND_LOCK.lock().unwrap();
        let surf = wlproto::compositor_create_surface(comp);
        if surf.is_null() {
            crate::logf!("proxy xlib surface create FAILED: wl_surface create failed");
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        let scobj = wlproto::swapchain_factory_create_swapchain(factory, surf);
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
            window: target.window,
            named_window: window,
            window_provisional: target.provisional,
            next: ptr::null_mut(),
        });
        let s = Box::into_raw(s);
        {
            let mut g = G.lock().unwrap();
            (*s).next = g.surfaces;
            g.surfaces = s;
        }
        *out = s as usize as VkSurfaceKHR;
        crate::logf!("override: proxy surface {:?} (own wl_surface {:?} for XID {:#x}, bound at its first present; Vulkan-only, guest GDI/2D not composited)",
            s,
            surf,
            window);
    }
    VK_SUCCESS
}

/// The surface both X11 create functions produce. `VK_KHR_xlib_surface` and
/// `VK_KHR_xcb_surface` differ only in the connection type and neither route
/// reads it — the override route binds the XID to a gamescope surface, and the
/// DRI3 route opens its own connection from `DISPLAY`.
unsafe fn x11_surface(gi: *mut GnInstance, window: u32, out: *mut VkSurfaceKHR) -> VkResult {
    // Present-route capability switch (`GN_VKSHIM_GUEST_SURFACE`, see
    // `use_guest_surface`). The DEFAULT (unset, or anything but exactly "1") is
    // the gamescope override/Wayland surface, taken here; `=1` opts in to the
    // DRI3-into-wine's-X-window route below, which carries wine's GDI/2D and
    // hands the shim the real window.
    if !use_guest_surface() {
        return unsafe { xlib_override_surface(gi, window, out) };
    }
    let x11 = if window == 0 {
        crate::logf!("x11 route: XID {:#x} unavailable ({})",
            window,
            crate::x11::RouteUnavailable::NullXid.reason());
        ptr::null_mut()
    } else {
        match unsafe { crate::x11::X11Present::new(window) } {
            Ok(p) => Box::into_raw(Box::new(p)),
            Err(e) => {
                crate::logf!("x11 route: XID {:#x} unavailable ({})",
                    window,
                    e.reason());
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
        named_window: window,
        window_provisional: false,
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
        crate::logf!("proxy surface {:?} (X11 window {:#x}, DRI3+Present route)",
            s,
            window);
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
        crate::logf!("proxy xlib surface create FAILED: instance {instance:?} not one of ours");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe { x11_surface(gi, ci.window as u32, out) }
}

/// `VK_KHR_xcb_surface`: what Mesa's zink (kopper) WSI creates its guest
/// swapchain from on X11. Without it a GL guest cannot reach this swapchain and
/// so never meets the acquire cap; with it, GL takes the same route and the same
/// gate as a Vulkan guest.
unsafe extern "C" fn gn_vkCreateXcbSurfaceKHR(
    instance: VkInstance,
    ci: *const VkXcbSurfaceCreateInfoKHR,
    _alloc: *const VkAllocationCallbacks,
    out: *mut VkSurfaceKHR,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gi = unsafe { find_instance(instance) };
    if gi.is_null() {
        crate::logf!("proxy xcb surface create FAILED: instance {instance:?} not one of ours");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe { x11_surface(gi, ci.window, out) }
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
    _visual_id: u32,
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
                wlproto::swapchain_destroy((*s).swapchain_obj);
            }
            if !(*s).own_surface.is_null() {
                wlproto::surface_destroy((*s).own_surface);
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
/// `surfaceCapabilities` is written, plus the surface-maintenance structs the
/// shim serves (`crate::maint1::surface_caps2`); any other structure a caller
/// chains is left untouched, which is the contract for structures an
/// implementation does not support.
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
    let res = unsafe {
        gn_vkGetPhysicalDeviceSurfaceCapabilitiesKHR(
            pd,
            (*info).surface,
            &mut (*caps).surfaceCapabilities,
        )
    };
    if res == VK_SUCCESS {
        // SAFETY: both chains are the caller's, live for the call.
        unsafe {
            crate::maint1::surface_caps2(
                (*info).pNext,
                (*caps).pNext,
                (*caps).surfaceCapabilities.minImageExtent,
                (*caps).surfaceCapabilities.maxImageExtent,
            )
        };
    }
    res
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
        crate::logf!("proxy surface caps query: {surface:#x} not one of ours");
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
        minImageCount: SC_DEFAULT_IMAGES,
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

/// The surface formats the proxy serves: the scan-out pool layout's own 8-bit
/// format, plus the other 8-bit format only under the opt-in (see
/// `allows_other_eight_bit`). Shared by the plain and `2` query forms so both
/// forms advertise exactly the same set.
fn served_surface_formats(layout: ScanoutLayout) -> &'static [VkSurfaceFormatKHR] {
    surface_formats_for(layout, hdr_session(), allows_other_eight_bit(layout))
}

/// Pure core of [`served_surface_formats`].
fn surface_formats_for(
    layout: ScanoutLayout,
    hdr: bool,
    allow_other: bool,
) -> &'static [VkSurfaceFormatKHR] {
    // The HDR set contains exactly what the create path can present: the
    // scan-out pool's 8-bit layout and the 10-bit layout, each with both colour
    // spaces. Two rules fix that shape.
    //
    // Every format is paired with both colours because vkd3d's
    // `dxgi_vk_swap_chain_select_format` demands an entry whose format AND
    // colour space match the request, and its only wildcard fallback exists on
    // the sRGB path: a title that switches to PQ while keeping an 8-bit
    // backbuffer stops creating swapchains entirely without the
    // `(<8-bit>, HDR10_ST2084)` pair. Order matters to a client that
    // takes the first entry carrying a colour space (DXVK): the 10-bit pair
    // comes first, because 8-bit PQ shreds the tone range.
    //
    // Nothing else may appear. An entry the create path refuses is worse than a
    // missing one — the client takes the offer, the create fails, and it
    // retries forever: measured, advertising an 8-bit format the create path
    // refused made DXVK loop on an in-game HDR toggle, and advertising FP16 made
    // it take `fmt=97 cs=1000104008` and loop against a refusal, because the
    // scan-out pool cannot serve 16 bits.
    const fn f(format: VkFormat, color_space: VkColorSpaceKHR) -> VkSurfaceFormatKHR {
        VkSurfaceFormatKHR {
            format,
            colorSpace: color_space,
        }
    }
    const fn hdr_set(eight_bit: VkFormat) -> [VkSurfaceFormatKHR; 9] {
        const A2B10: VkFormat = VK_FORMAT_A2B10G10R10_UNORM_PACK32;
        const A2R10: VkFormat = VK_FORMAT_A2R10G10B10_UNORM_PACK32;
        const PQ: VkColorSpaceKHR = VK_COLOR_SPACE_HDR10_ST2084_EXT;
        const SRGB: VkColorSpaceKHR = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR;
        const LINEAR: VkColorSpaceKHR = VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT;
        [
            f(A2B10, PQ),
            f(A2R10, PQ),
            f(eight_bit, PQ),
            f(A2B10, SRGB),
            f(A2R10, SRGB),
            f(eight_bit, SRGB),
            f(A2B10, LINEAR),
            f(A2R10, LINEAR),
            f(eight_bit, LINEAR),
        ]
    }

    // The opt-in is the only case where the create path serves the other
    // 8-bit format, so it is the only case that may advertise it.
    const fn hdr_opt_in(set: [VkSurfaceFormatKHR; 9], other: VkFormat) -> [VkSurfaceFormatKHR; 8] {
        [
            set[0],
            set[1],
            set[2],
            set[3],
            set[4],
            set[5],
            f(other, VK_COLOR_SPACE_HDR10_ST2084_EXT),
            f(other, VK_COLOR_SPACE_SRGB_NONLINEAR_KHR),
        ]
    }

    const B8: VkFormat = VK_FORMAT_B8G8R8A8_UNORM;
    const R8: VkFormat = VK_FORMAT_R8G8B8A8_UNORM;
    const K_HDR_BGRA: [VkSurfaceFormatKHR; 9] = hdr_set(B8);
    const K_HDR_RGBA: [VkSurfaceFormatKHR; 9] = hdr_set(R8);
    const K_HDR_BGRA_OPT_IN: [VkSurfaceFormatKHR; 8] = hdr_opt_in(K_HDR_BGRA, R8);
    const K_HDR_RGBA_OPT_IN: [VkSurfaceFormatKHR; 8] = hdr_opt_in(K_HDR_RGBA, B8);
    const K_FMTS_BGRA: [VkSurfaceFormatKHR; 2] = [K_HDR_BGRA[5], K_HDR_RGBA[5]];
    const K_FMTS_RGBA: [VkSurfaceFormatKHR; 2] = [K_HDR_RGBA[5], K_HDR_BGRA[5]];

    // Only the pool layout's own 8-bit format by default, so the guest renders
    // in the scan-out pool's own layout (see `allows_other_eight_bit` — a
    // second 8-bit layout lets a guest render R/B-exchanged into a daemon
    // buffer).
    match (layout, hdr, allow_other) {
        (ScanoutLayout::Bgra8888, true, false) => &K_HDR_BGRA,
        (ScanoutLayout::Bgra8888, true, true) => &K_HDR_BGRA_OPT_IN,
        (ScanoutLayout::Rgba8888, true, false) => &K_HDR_RGBA,
        (ScanoutLayout::Rgba8888, true, true) => &K_HDR_RGBA_OPT_IN,
        (ScanoutLayout::Bgra8888, false, true) => &K_FMTS_BGRA,
        (ScanoutLayout::Bgra8888, false, false) => &K_FMTS_BGRA[..1],
        (ScanoutLayout::Rgba8888, false, true) => &K_FMTS_RGBA,
        (ScanoutLayout::Rgba8888, false, false) => &K_FMTS_RGBA[..1],
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
        crate::logf!("proxy surface formats query: {surface:#x} not one of ours");
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    let served = served_surface_formats(crate::scanout::layout());
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

// The four present modes the shim serves, in the order the guest sees them;
// fifo_relaxed sits next to fifo as its sibling.
const SERVED_PRESENT_MODES: [VkPresentModeKHR; 4] = [
    VK_PRESENT_MODE_IMMEDIATE_KHR,
    VK_PRESENT_MODE_MAILBOX_KHR,
    VK_PRESENT_MODE_FIFO_KHR,
    VK_PRESENT_MODE_FIFO_RELAXED_KHR,
];

/// The guest's requested `presentMode`, falling back to fifo (mesa
/// wsi_common.c:1521). Every swapchain the shim creates carries this mode: the
/// shim does not override it, so what the guest asks for is what it gets.
fn guest_present_mode(ci: &VkSwapchainCreateInfoKHR) -> VkPresentModeKHR {
    match ci.presentMode {
        VK_PRESENT_MODE_IMMEDIATE_KHR
        | VK_PRESENT_MODE_MAILBOX_KHR
        | VK_PRESENT_MODE_FIFO_KHR
        | VK_PRESENT_MODE_FIFO_RELAXED_KHR => ci.presentMode,
        _ => VK_PRESENT_MODE_FIFO_KHR,
    }
}


/// Why a present declares the mode it does, on the override route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Declared {
    /// The guest's own mode ([`guest_present_mode`]).
    Mode,
    /// Mailbox for frame generation (`source=generating`).
    Generating,
}

/// The mode a present on `sc` declares to gamescope.
///
/// gamescope is told what the guest asked for: the container cannot override it
/// (`MESA_VK_WSI_PRESENT_MODE` is not read anywhere), and neither can the cap,
/// the panel's refresh or frame generation. What those decide is the rate the
/// guest may acquire at and the frames the generator may add, which is
/// [`crate::limits`] — one rule for both.
///
/// The single exception is a generated frame: it declares mailbox, because
/// generated commits carry no declaration of their own and a real frame
/// declared FIFO would queue behind them in gamescope's limiter.
///
/// The rule is read per present, so a cap, a multiplier or a panel rate that
/// changes mid-session moves the declaration on the next commit. Off the
/// override route there is nothing to declare it to, and the mode is the one
/// the swapchain was made with.
pub(crate) unsafe fn live_present_mode(sc: *mut GnSwapchain) -> VkPresentModeKHR {
    unsafe { live_choice(sc) }.0
}

/// [`live_present_mode`] and why.
unsafe fn live_choice(sc: *mut GnSwapchain) -> (VkPresentModeKHR, Declared) {
    let mode = unsafe { (*sc).present_mode };
    if unsafe { (*(*sc).surf).swapchain_obj.is_null() } {
        return (mode, Declared::Mode);
    }
    choose_live_mode(mode, crate::lsfg::wanted().is_some())
}

/// [`live_choice`]'s rule: the guest's mode, except for a generated frame, and
/// except that `IMMEDIATE` is served as `MAILBOX`.
///
/// `IMMEDIATE` and `MAILBOX` differ only in what happens to a frame that cannot
/// be shown: immediate presents it anyway and tears, mailbox replaces the pending
/// one. A guest that asks immediate therefore gets its frames dropped for it
/// instead of being shown mid-scan, which is what the container used to do with
/// `MESA_VK_WSI_PRESENT_MODE` and no longer may. The declaration follows the same
/// mode, so gamescope is told what is actually served.
/// The mode a swapchain's image count is sized for: the one this session will
/// serve, not the one the guest declared.
///
/// The two differ in exactly two cases, and both are ours: a generation session
/// is served mailbox or fifo from the QuickMenu whatever the guest asked, and a
/// declared IMMEDIATE is served as mailbox. Sizing the count from the declaration
/// therefore sizes it for a mode nobody runs — measured on Witcher 3, whose guest
/// asks FIFO (`min=3 mode=2`) while it is served and held as mailbox:
///
/// ```text
/// slots=[3 guest + 8 destinations]  sf hold 1.5-2.5 periods  acq_fps 49.7
/// ```
///
/// Three guest slots under a two-period hold cap at `depth x refresh / hold` = 54,
/// and the mailbox floor of four — the whole reason `SC_MAILBOX_IMAGES` exists —
/// never applied. The hold already follows the served mode (`on_buffer_release`);
/// this makes the depth follow it too.
fn count_present_mode(declared: VkPresentModeKHR) -> VkPresentModeKHR {
    choose_live_mode(declared, crate::lsfg::live().0).0
}

fn choose_live_mode(mode: VkPresentModeKHR, generating: bool) -> (VkPresentModeKHR, Declared) {
    if generating {
        // The QuickMenu offers mailbox and fifo for generation and nothing else,
        // and the note carries the choice (`LsfgConfig::present_mode`); anything
        // unknown is mailbox.
        let served = if crate::lsfg::generating_present_mode() == 1 {
            VK_PRESENT_MODE_FIFO_KHR
        } else {
            VK_PRESENT_MODE_MAILBOX_KHR
        };
        (served, Declared::Generating)
    } else if mode == VK_PRESENT_MODE_IMMEDIATE_KHR {
        (VK_PRESENT_MODE_MAILBOX_KHR, Declared::Mode)
    } else {
        (mode, Declared::Mode)
    }
}

/// What this swapchain's session allows ([`crate::limits::limits`]): the
/// guest's own mode decides whether the panel is a ceiling, the daemon's cap
/// note and the generator's multiplier decide the rest.
fn session_limits(sc: *mut GnSwapchain) -> crate::limits::Limits {
    let mode = unsafe { (*sc).present_mode };
    let vsync = matches!(
        mode,
        VK_PRESENT_MODE_FIFO_KHR | VK_PRESENT_MODE_FIFO_RELAXED_KHR
    );
    let cap = crate::scanout::cap_fps_note();
    crate::limits::limits(
        vsync,
        (cap != gndrm_uapi::wire::FPS_UNCAPPED).then_some(cap),
        crate::lsfg::wanted(),
        crate::lsfg::refresh_hz(),
    )
}

/// How the acquire gate paces `sc` now: to the session's acquire limit
/// ([`session_limits`]), on whichever clock can hold it.
///
/// - Off gamescope's override route (the X11 Present route) nothing else holds
///   the cap, so the gate does, on its own clock.
/// - **Every declaration is gated.** gamescope's limiter withholds a window's
///   frame callback, and this shim paces on its own schedule and the buffer
///   release, never on that callback — so a FIFO declaration used to leave the
///   guest with no cap at all: `steamcompmgr_should_vblank_window` acts only
///   when the cap is below the refresh AND divides it, so 60 on 60 Hz and 40 on
///   60 Hz were both unlimited, measured as a 26 fps session whose cap did
///   nothing.
/// - With generation on the gate holds the source rate on its own clock, and
///   the batches are placed on the latch grid by the generator itself.
/// - Otherwise the gate holds the acquire limit on the display's vblank grid
///   (`crate::vsync_gate`), snapping to a whole number of vblanks when it can.
unsafe fn acquire_pacing(sc: *mut GnSwapchain) -> crate::vsync_gate::Pacing {
    let limits = session_limits(sc);
    let Some(acquire) = limits.acquire_per_s else {
        return crate::vsync_gate::Pacing::Off;
    };
    if unsafe { (*(*sc).surf).swapchain_obj.is_null() } {
        return crate::vsync_gate::Pacing::on_clock(acquire);
    }
    let (_, declared) = unsafe { live_choice(sc) };
    gamescope_route_pacing(declared, acquire)
}

/// [`acquire_pacing`]'s rule on gamescope's override route, for what the commit
/// declares ([`choose_live_mode`]), the session's acquire limit and the
/// display's vblank grid.
fn gamescope_route_pacing(declared: Declared, acquire: u32) -> crate::vsync_gate::Pacing {
    match declared {
        // Generated frames are placed on the latch grid by the generator, so
        // the gate holds the source rate on its own clock.
        Declared::Generating => crate::vsync_gate::Pacing::on_clock(acquire),
        // Whatever the guest declared. A FIFO declaration is not a pacer:
        // gamescope withholds the window's frame callback and this shim never
        // waits on one, and its divisor only bites when the cap is below the
        // refresh AND divides it. Standing down here is what left a 60/60 and a
        // 40/60 session with no cap at all.
        Declared::Mode => crate::vsync_gate::pacing(acquire),
    }
}

/// The mode a commit on `sc` declares to gamescope, counted for the census.
unsafe fn declared_present_mode(sc: *mut GnSwapchain) -> VkPresentModeKHR {
    let mode = unsafe { live_present_mode(sc) };
    if mode == VK_PRESENT_MODE_FIFO_KHR {
        DECL_FIFO_N.fetch_add(1, Ordering::Relaxed);
    } else {
        DECL_OTHER_N.fetch_add(1, Ordering::Relaxed);
    }
    mode
}

fn mode_name(mode: VkPresentModeKHR) -> &'static str {
    match mode {
        VK_PRESENT_MODE_IMMEDIATE_KHR => "immediate",
        VK_PRESENT_MODE_MAILBOX_KHR => "mailbox",
        VK_PRESENT_MODE_FIFO_KHR => "fifo",
        VK_PRESENT_MODE_FIFO_RELAXED_KHR => "fifo_relaxed",
        _ => "?",
    }
}

/// What the ungated `present mode:` line reports: the mode declared to
/// gamescope and why, the cap and what holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModeLine {
    declared: VkPresentModeKHR,
    source: &'static str,
    gamescope_route: bool,
    cap: u32,
    gate: &'static str,
    lsfg: bool,
    /// The session's limits ([`crate::limits`]), `FPS_UNCAPPED`/`0` for none:
    /// the acquire ceiling and the frames the generator may add, in frames per
    /// second. Printed because the declaration no longer says what the cap did.
    acquire: u32,
    generate: u32,
}

/// The last [`ModeLine`] printed.
static MODE_LINE: Mutex<Option<ModeLine>> = Mutex::new(None);

/// The `present mode:` census.
///
/// Ungated, once when frames first flow and again whenever the declared mode,
/// its source, the cap or the gate changes: a shipped session's log says which
/// path ran without repeating it. The counters a device run checks the pacing
/// against repeat every 10 s with `GNDRM_LOG_VKSHIM=1`, as the shim's stats
/// line does.
unsafe fn mode_census(sc: *mut GnSwapchain) {
    let route = unsafe { !(*(*sc).surf).swapchain_obj.is_null() };
    let (declared, why) = unsafe { live_choice(sc) };
    let limits = session_limits(sc);
    let line = ModeLine {
        declared,
        source: match why {
            Declared::Generating => "generating",
            Declared::Mode => "guest",
        },
        gamescope_route: route,
        cap: crate::scanout::cap_fps_note(),
        gate: unsafe { acquire_pacing(sc) }.clock().name(),
        lsfg: crate::lsfg::live().0,
        acquire: limits
            .acquire_per_s
            .unwrap_or(gndrm_uapi::wire::FPS_UNCAPPED),
        generate: limits.generate_per_s.unwrap_or(0),
    };
    {
        let mut last = MODE_LINE.lock().unwrap_or_else(|e| e.into_inner());
        if *last != Some(line) {
            *last = Some(line);
            crate::logi!("present mode: declared={} source={} route={} cap={} gate={} lsfg={} acquire={} generate={}",
                mode_name(line.declared),
                line.source,
                if route { "gamescope" } else { "x11" },
                line.cap,
                line.gate,
                u8::from(line.lsfg),
                line.acquire,
                line.generate);
        }
    }
    if !crate::vkshim_enabled() || !rate_ok(&RATE_MODE, MODE_CENSUS_EVERY) {
        return;
    }
    let (presented, discarded, fallback) = crate::present::feedback_counts();
    let wait_census = crate::present::wait_census();
    let (late, skipped) = crate::vsync_gate::late_counts();
    // A cumulative counter is a rate only with its denominator: the session's
    // own age, from the first acquire.
    let t0 = SHIM_T0_NS.load(Ordering::Relaxed);
    let age_s = if t0 == 0 {
        0
    } else {
        monotonic_ns().saturating_sub(t0) / 1_000_000_000
    };
    // Since the last line, so each line reads one window's cadence.
    let gaps = COMMIT_GAPS.each_ref().map(|b| b.swap(0, Ordering::Relaxed));
    crate::logf!("present mode: declared={} source={} route={} cap={} gate={} lsfg={} acquire={} generate={} acquires={} commits={} declared_fifo={} declared_other={} releases={} feedback={} presented={} discarded={} fallback={} at_queue={} gate_holds={} gate_late={} skipped={} slot_waits={} commit_lat_us={} spacing=[<15:{} 15-16.2:{} 16.2-17.2:{} 17.2-20:{} >20:{}] gated={} ungated={} gate_wait_us={} acq_spacing=[<14:{} 14-16.2:{} 16.2-17.2:{} 17.2-20:{} >20:{}] age_s={} calls={} ok={} retry={} notes={} park_timeouts={} rnotes={} noid={} notelag=[<5:{} <33:{} <100:{} >100:{}] waitans=[n:{} q:{} commit:{} fb:{} daemon:{} grace:{} release:{} discard:{} unarmed:{} timeout:{} nocommit:{}]",
        mode_name(line.declared),
        line.source,
        if route { "gamescope" } else { "x11" },
        line.cap,
        line.gate,
        u8::from(line.lsfg),
        line.acquire,
        line.generate,
        N_ACQ.load(Ordering::Relaxed),
        N_COMMIT_OK.load(Ordering::Relaxed),
        DECL_FIFO_N.load(Ordering::Relaxed),
        DECL_OTHER_N.load(Ordering::Relaxed),
        REL_ALL_N.load(Ordering::Relaxed),
        if PRESENTATION.load(Ordering::Relaxed).is_null() { "off" } else { "on" },
        presented,
        discarded,
        fallback,
        crate::present::at_queue_count(),
        GATE_HOLD_N.load(Ordering::Relaxed),
        late,
        skipped,
        SLOT_WAIT_N.load(Ordering::Relaxed),
        unsafe { (*sc).commit_latency_ns.load(Ordering::Relaxed) } / 1_000,
        gaps[0],
        gaps[1],
        gaps[2],
        gaps[3],
        gaps[4],
        ACQ_GATED_N.load(Ordering::Relaxed),
        ACQ_UNGATED_N.load(Ordering::Relaxed),
        GATE_WAIT_NS.load(Ordering::Relaxed) / 1_000,
        ACQ_SPACING[0].load(Ordering::Relaxed),
        ACQ_SPACING[1].load(Ordering::Relaxed),
        ACQ_SPACING[2].load(Ordering::Relaxed),
        ACQ_SPACING[3].load(Ordering::Relaxed),
        ACQ_SPACING[4].load(Ordering::Relaxed),
        age_s,
        N_ACQ.load(Ordering::Relaxed),
        ACQ_OK_N.load(Ordering::Relaxed),
        ACQ_RETRY_N.load(Ordering::Relaxed),
        NOTES_APPLIED.load(Ordering::Relaxed),
        PARK_TIMEOUTS.load(Ordering::Relaxed),
        crate::scanout::release_notes_received(),
        NOID_AT_COMMIT.load(Ordering::Relaxed),
        NOTELAG[0].load(Ordering::Relaxed),
        NOTELAG[1].load(Ordering::Relaxed),
        NOTELAG[2].load(Ordering::Relaxed),
        NOTELAG[3].load(Ordering::Relaxed),
        wait_census[0],
        wait_census[1],
        wait_census[2],
        wait_census[3],
        wait_census[4],
        wait_census[5],
        wait_census[6],
        wait_census[7],
        wait_census[8],
        wait_census[9],
        wait_census[10],);
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
        // Unreachable: `(*sc).present_mode` is `guest_present_mode`'s output,
        // which is one of the four served modes above.
        _ => 2,
    }
}

/// The modes the guest is offered: the served set in its own order, with FIFO
/// appended when it is missing (a swapchain must be creatable in FIFO).
fn offered_present_modes(served: &[VkPresentModeKHR]) -> Vec<VkPresentModeKHR> {
    let mut v = Vec::with_capacity(served.len() + 1);
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
    let surf = unsafe { find_surface(surface) };
    if surf.is_null() {
        crate::logf!("proxy surface present-modes query: {surface:#x} not one of ours");
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    let modes_list = offered_present_modes(&SERVED_PRESENT_MODES);
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
            crate::logf!("proxy setHdrMetadata: swapchain {sc:#x} not one of ours");
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
        crate::logf!("proxy surface formats2 query: {surface:#x} not one of ours");
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    let served = served_surface_formats(crate::scanout::layout());
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
            format!("{}", format_args!("proxy: vkCreateDevice NO INSTANCE pd={:#x} gi={} — guest device will be driver-raw (wrong-handle class)",
                pd as usize, gi.is_null()))
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
        crate::logf!("proxy device: importing swapchain slots as AHardwareBuffers");
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
    // An absent or zero apiVersion means 1.0.
    let api = match unsafe { (*gi).api_version } {
        0 => 1 << 22,
        v => v,
    };
    crate::logf!("proxy device: guest enables {} extensions api={}.{}",
        names.len(),
        (api >> 22) & 0x7F,
        (api >> 12) & 0x3FF);
    if !WSI_DEVICE_LOGGED.swap(true, Ordering::Relaxed) {
        crate::logi!("proxy device: WSI extensions hidden from the guest: [{}] requested and masked: [{}]",
            crate::wsi_policy::hidden_names(have.iter().map(|h| h.as_c_str())),
            crate::wsi_policy::hidden_names(names.iter().map(|&o| unsafe { CStr::from_ptr(o) })));
    }
    for &name in names {
        // The guest must never enable a WSI extension the proxy cannot honour,
        // nor hand the driver one the shim serves itself (`crate::wsi_policy`);
        // drop it before the driver sees it.
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
        if crate::wsi_policy::driver_copy_hidden(n_)
            || (n_ == c"VK_EXT_hdr_metadata"
                && !have.is_empty()
                && !have.iter().any(|h| h.as_c_str() == n_))
            || (strip_robustness2 && n_ == c"VK_EXT_robustness2")
            || (crate::placed::faked() && n_ == crate::placed::EXT_NAME)
        {
            crate::logf!("proxy device: masking {}",
                n_.to_string_lossy());
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
    } else if crate::export::enabled() || AHB_IMPORT_ALLOWED.load(Ordering::Relaxed) {
        &[c"VK_ANDROID_external_memory_android_hardware_buffer"]
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
            crate::logf!("proxy device: driver lacks {}, not adding it",
                add.to_string_lossy());
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
                    crate::logf!("proxy device: masking {}",
                        drop.to_string_lossy());
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
    // The present-id/wait features belong to the shim's swapchain, not the
    // driver's, so the structs are stripped from the driver's chain whatever the
    // guest asked for. They are recorded here: the entry points answer only when
    // the guest enabled them.
    let guest_id = names
        .iter()
        .any(|&o| unsafe { CStr::from_ptr(o) } == crate::present::EXT_PRESENT_ID);
    let guest_wait = names
        .iter()
        .any(|&o| unsafe { CStr::from_ptr(o) } == crate::present::EXT_PRESENT_WAIT);
    crate::present::note_enabled(guest_id, guest_wait);

    // Swapchain maintenance is the shim's too (`crate::maint1`). The policy
    // (`crate::wsi_policy`) strips the feature struct of every WSI extension
    // whose driver copy the guest does not see: the shim's own features are its
    // answer, and a hidden extension's struct (the v2 present pair, present
    // timing, ...) names a feature the driver must not be asked for.
    let mut strip: Vec<VkStructureType> = crate::wsi_policy::stripped_feature_stypes()
        .map(|s| s as VkStructureType)
        .collect();
    if strip_robustness2 {
        strip.push(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ROBUSTNESS_2_FEATURES_EXT);
    }
    if crate::placed::faked() {
        strip.push(crate::placed::STYPE_FEATURES);
    }
    // The create chain as the driver will see it: a `VK_ERROR_FEATURE_NOT_PRESENT`
    // names no feature, and everything the guest asks for lives in one of these
    // structs — sType plus the first six words says which and which bit.
    // Built only when the trace is on: it allocates, and the answer is the same
    // every create in a session.
    if crate::vkshim_enabled() {
        let mut chain: Vec<String> = Vec::new();
        unsafe {
            let mut w = ci2.pNext as *mut VkBaseOutStructure;
            while !w.is_null() {
                let words = (w as *const u32).add(2);
                let mut s = format!("{}:", (*w).sType);
                for k in 0..6 {
                    s.push_str(&format!("{:x},", *words.add(k)));
                }
                chain.push(s);
                w = (*w).pNext;
            }
        }
        if !chain.is_empty() {
            crate::logf!("proxy device: create chain [{}]",
                chain.join(" "));
        }
    }
    // The feature lift is advertised to the guest, not asked of the driver
    // (`advertise`): a driver that gates a feature it implements answers
    // VK_ERROR_FEATURE_NOT_PRESENT for a create that requests the bit, so each
    // lifted bit is ANDed back to the driver's own answer here — the wrapper's
    // DISABLE_FEATURE. Restored after the call, as the unlinked structs are.
    let base = unsafe { crate::advertise::driver_core(gi, pd) };
    let mut masked: Vec<(*mut crate::advertise::CoreFeatures, crate::advertise::CoreFeatures)> =
        Vec::new();
    // No base, no masking: a driver that answers neither features query is not a
    // driver that supports nothing, and cutting the guest's request against
    // zeros would take bits this driver does have out of the create.
    if let Some(base) = base.as_ref() {
        // Before masking: the guest's request against the driver's answer, which
        // is the bit a `VK_ERROR_FEATURE_NOT_PRESENT` create will refuse.
        if !ci2.pEnabledFeatures.is_null() {
            unsafe {
                crate::advertise::request_mismatch(
                    &*(ci2.pEnabledFeatures as *const crate::advertise::CoreFeatures),
                    base,
                );
            }
            let c = ci2.pEnabledFeatures as *mut crate::advertise::CoreFeatures;
            let before = unsafe { *c };
            unsafe { crate::advertise::mask(&mut *c, base) };
            masked.push((c, before));
        }
    }
    let mut unlinked: Vec<(*mut VkBaseOutStructure, *mut VkBaseOutStructure)> = Vec::new();
    let mut prev: *mut VkBaseOutStructure = ptr::null_mut();
    let mut p = ci2.pNext as *mut VkBaseOutStructure;
    while !p.is_null() {
        unsafe {
            let next = (*p).pNext;
            if let (true, Some(base)) = (
                (*p).sType == VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
                base.as_ref(),
            ) {
                let c = (p as *mut u8).add(16) as *mut crate::advertise::CoreFeatures;
                let before = *c;
                crate::advertise::request_mismatch(&*c, base);
                crate::advertise::mask(&mut *c, base);
                masked.push((c, before));
            }
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
    if res != VK_SUCCESS {
        // The guest's own log hides this behind a generic E_FAIL; the driver's
        // code names the cause (VK_ERROR_FEATURE_NOT_PRESENT, EXTENSION_NOT_PRESENT,
        // INITIALIZATION_FAILED).
        crate::logf!("proxy device: driver create failed res={}", res);
    }
    for &(prev, p) in unlinked.iter().rev() {
        if !prev.is_null() {
            unsafe { (*prev).pNext = p }
        }
    }
    for (c, before) in masked {
        unsafe { *c = before };
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
        guest_peak: (AtomicU32::new(0), AtomicU32::new(0)),
        gen_batches: Vec::new(),
        gen_next: 0,
        gen_sc: ptr::null_mut(),
        gen_due: VecDeque::new(),
        gen_last_deadline: 0,
        gen_last_source_ns: 0,
        gen_period_ns: 0,
        lsfg: ptr::null_mut(),
        gen_last_latch_ns: 0,
        gen_last_acq: 0,
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
        crate::logf!("proxy device {:?} (real {:?}, queue family {qf})",
            gd,
            (*gd).real);
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
    GDPA_MISSED.forget(gd as usize);
    REAL_PROC_SEEN.forget(gd as usize);
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
                format_args!("proxy: gn_vkGetDeviceQueue DEVICE MISS family={family} index={index} {}",
                    unsafe { handle_note(false, device as usize, first, n) })
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
                format_args!("proxy: gn_vkGetDeviceQueue2 DEVICE MISS {}", unsafe {
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
        // opaque. The daemon already reports the same buffer as AR24 or AB24
        // (`PRIMARY_FORMATS`), so this keeps the two descriptions in agreement.
        VK_FORMAT_B8G8R8A8_UNORM => GN_DRM_FORMAT_ARGB8888,
        VK_FORMAT_R8G8B8A8_UNORM => GN_DRM_FORMAT_ABGR8888,
        // The two 10-bit layouts differ only in which channel sits in the high
        // ten bits, and the fourcc must say which one the buffer actually is:
        // `A2R10G10B10` (58, red high) is read back by gamescope as
        // `ARGB2101010`; `A2B10G10R10` (64, blue high) is the canonical Vulkan
        // layout vkd3d asks for and reads back as `ABGR2101010`. Declaring the
        // wrong one exchanges red and blue — measured on Ori: an HDR frame that
        // should read warm (114,42,28) came out blue (2,51,89).
        VK_FORMAT_A2R10G10B10_UNORM_PACK32 => GN_DRM_FORMAT_ARGB2101010,
        VK_FORMAT_A2B10G10R10_UNORM_PACK32 => GN_DRM_FORMAT_ABGR2101010,
        _ => 0,
    }
}

/// The swapchain format in the scan-out pool's `layout`.
const fn scanout_swapchain_format(layout: ScanoutLayout) -> VkFormat {
    match layout {
        ScanoutLayout::Bgra8888 => VK_FORMAT_B8G8R8A8_UNORM,
        ScanoutLayout::Rgba8888 => VK_FORMAT_R8G8B8A8_UNORM,
    }
}

/// The 8-bit swapchain format in the other channel order.
const fn other_eight_bit_format(layout: ScanoutLayout) -> VkFormat {
    match layout {
        ScanoutLayout::Bgra8888 => VK_FORMAT_R8G8B8A8_UNORM,
        ScanoutLayout::Rgba8888 => VK_FORMAT_B8G8R8A8_UNORM,
    }
}

/// The opt-in that allows [`other_eight_bit_format`] for `layout`.
const fn other_eight_bit_env(layout: ScanoutLayout) -> &'static str {
    match layout {
        ScanoutLayout::Bgra8888 => "GN_VKSHIM_ALLOW_RGBA",
        ScanoutLayout::Rgba8888 => "GN_VKSHIM_ALLOW_BGRA",
    }
}

fn allows_other_eight_bit(layout: ScanoutLayout) -> bool {
    std::env::var_os(other_eight_bit_env(layout)).is_some_and(|v| v == "1")
}

fn swapchain_format_allowed(format: VkFormat, layout: ScanoutLayout, allow_other: bool) -> bool {
    format == scanout_swapchain_format(layout)
        || (allow_other && format == other_eight_bit_format(layout))
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
        if (*s).fence_sem != 0 {
            if let Some(f) = (*gd).f.destroy_sem {
                f((*gd).real, (*s).fence_sem, ptr::null());
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
            crate::logf!("proxy: /dev/dma_heap/system: {}",
                std::io::Error::last_os_error());
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
        crate::logf!("proxy: dma_heap alloc({len}): {}",
            std::io::Error::last_os_error());
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
    if res == VK_SUCCESS {
        res = unsafe { f.create_sem.unwrap()(dev, &sci, ptr::null(), &mut (*s).fence_sem) };
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
                    format_args!("ExternalMemoryImageCreateInfo{{handleTypes={:#x}}}",
                        unsafe { (*e).handleTypes })
                ));
            }
            VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT => {
                let e = p.cast::<VkImageDrmFormatModifierExplicitCreateInfoEXT>();
                let m = unsafe { (*e).drmFormatModifier };
                let cnt = unsafe { (*e).drmFormatModifierPlaneCount };
                let pl = unsafe { (*e).pPlaneLayouts };
                if !pl.is_null() && cnt > 0 {
                    out.push_str(&format!("{}", format_args!("ImageDrmFormatModifierExplicitCreateInfoEXT{{mod={m:#x}, planes={cnt}, plane0{{offset={}, rowPitch={}, size={}}}}}",
                        unsafe { (*pl).offset },
                        unsafe { (*pl).rowPitch },
                        unsafe { (*pl).size })));
                } else {
                    out.push_str(&format!(
                        "{}",
                        format_args!("ImageDrmFormatModifierExplicitCreateInfoEXT{{mod={m:#x}, planes={cnt}}}")
                    ));
                }
            }
            VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT => {
                let e = p.cast::<VkImageDrmFormatModifierListCreateInfoEXT>();
                out.push_str(&format!(
                    "{}",
                    format_args!("ImageDrmFormatModifierListCreateInfoEXT{{count={}}}",
                        unsafe { (*e).drmFormatModifierCount })
                ));
            }
            _ => out.push_str(&format!("{}", format_args!("sType={st}"))),
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
    crate::logf!("{tag}: vkCreateImage fmt={} {}x{}x{} mips={} layers={} samples={:#x} tiling={} usage={:#x} flags={:#x} pNext=[{pnext}]",
        ci.format,
        ci.extent.width,
        ci.extent.height,
        ci.extent.depth,
        ci.mipLevels,
        ci.arrayLayers,
        ci.samples,
        ci.tiling,
        ci.usage,
        ci.flags);
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
        crate::logf!("modq: vkGetPhysicalDeviceImageFormatProperties2 unresolved (driver/loader)");
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
        crate::logf!("modq: vkGetPhysicalDeviceImageFormatProperties2 fmt={format} type=2D tiling={tiling} usage={usage:#x} pNext=VkPhysicalDeviceImageDrmFormatModifierInfoEXT{{mod=LINEAR(0), sharing=EXCLUSIVE}} -> res={res} maxExtent={}x{}x{} maxMips={} maxLayers={} samples={:#x} maxRes={} modCount={} mod0={:#x} feat0={:#x} planes0={} [informational only: result not consulted]",
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
            mods[0].drmFormatModifierPlaneCount);
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
                format_args!("t{i}:heap{}:flags{:#x}{}",
                    t.heapIndex,
                    t.propertyFlags,
                    if t.propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT != 0 {
                        ":LOCAL"
                    } else {
                        ""
                    })
            )
        })
        .collect();
    let heaps: Vec<String> = (0..(mp.memoryHeapCount as usize).min(VK_MAX_MEMORY_HEAPS))
        .map(|i| {
            format!(
                "{}",
                format_args!("h{i}:{}B:flags{:#x}",
                    mp.memoryHeaps[i].size,
                    mp.memoryHeaps[i].flags)
            )
        })
        .collect();
    crate::logf!("memprops: types={} [{}] heaps={} [{}]",
        mp.memoryTypeCount,
        summary.join(" "),
        mp.memoryHeapCount,
        heaps.join(" "));
}

/// Read-only description of a chosen `memoryTypeIndex` (heap + DEVICE_LOCAL).
fn mem_type_desc(gd: *mut GnDevice, idx: u32) -> String {
    let mp = unsafe { &(*gd).mem_props };
    if (idx as usize) < mp.memoryTypeCount as usize {
        let t = mp.memoryTypes[idx as usize];
        let local = t.propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT != 0;
        format!(
            "{}",
            format_args!("mem_type {idx} heap {} flags {:#x} DEVICE_LOCAL={}",
                t.heapIndex,
                t.propertyFlags,
                if local { "yes" } else { "no" })
        )
    } else {
        format!(
            "{}",
            format_args!("mem_type {idx} (beyond memoryTypeCount {})",
                mp.memoryTypeCount)
        )
    }
}

/// Creates one swapchain slot image as a LINEAR, externally-exported image: the
/// guest's `VK_IMAGE_TILING_LINEAR` is the layout contract when the driver's
/// row pitch lands on the backing buffer's stride; otherwise the same LINEAR
/// layout is restated through an explicit DRM-modifier create at that stride.
/// Either way the driver produces plain scan-out pixels (LINEAR forbids UBWC
/// outright) and DRI3 wayland present see a modifier of 0. The image is backed by a dma-buf the shim owns
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
    if !SLOT_DMABUF_REFUSED.load(Ordering::Relaxed)
        && std::env::var_os("GNDRM_EXPORT_AHB").is_some_and(|v| v == "1")
    {
        SLOT_DMABUF_REFUSED.store(true, Ordering::Relaxed);
        crate::logf!("proxy slot: GNDRM_EXPORT_AHB=1, importing pool buffers as AHardwareBuffers from the start");
    }
    if unsafe { (*gd).import_ahb } || SLOT_DMABUF_REFUSED.load(Ordering::Relaxed) {
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
            crate::logf!("proxy slot: pool buffer fd={} ino={} stride={}",
                buf.fd,
                buf.ino,
                (*s).stride);
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
    let mut explicit_layout = false;
    if res != VK_SUCCESS {
        explicit_layout = true;
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
        crate::logf!("proxy import image: LINEAR tiling {r1}; explicit LINEAR modifier (pitch {}) {res}",
            (*s).stride);
    }
    if res != VK_SUCCESS {
        crate::logf!("proxy import image create failed: {res}");
        return res;
    }
    // A plain LINEAR image's row pitch is the driver's choice, and the pool
    // buffer is scanned out and sampled at its gralloc stride: a driver that
    // packs the rows tighter (PowerVR, 632 px -> 2528 B against a 2560 B
    // stride) shears every frame. Such a slot takes the AHardwareBuffer route,
    // where the driver owns the layout.
    if !explicit_layout {
        if let Some(gsl) = unsafe { gn_real_device_proc(gd, c"vkGetImageSubresourceLayout") } {
            let gsl: PFN_vkGetImageSubresourceLayout = unsafe { std::mem::transmute(gsl) };
            let sub = VkImageSubresource {
                aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                mipLevel: 0,
                arrayLayer: 0,
            };
            let mut l = VkSubresourceLayout {
                offset: 0,
                size: 0,
                rowPitch: 0,
                arrayPitch: 0,
                depthPitch: 0,
            };
            unsafe { gsl(dev, (*s).image, &sub, &mut l) };
            if l.rowPitch != 0 && (l.rowPitch != u64::from((*s).stride) || l.offset != 0) {
                // The pool's rows are gralloc's, padded for widths it does not
                // like (1024 px -> 5120 B on Adreno), and plain LINEAR let the
                // driver pick width*4. The same LINEAR layout stated explicitly
                // at the pool's stride is what a driver with DRM-modifier
                // import (Turnip) takes; one that refuses it goes to the
                // AHardwareBuffer route below, where the driver owns the layout.
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
                let mut img2: VkImage = 0;
                let r2 = unsafe { f.create_image.unwrap()(dev, &ici2, ptr::null(), &mut img2) };
                crate::logf!("proxy slot: LINEAR pitch {} != pool stride {} for {}x{}; explicit LINEAR modifier at the pool stride: {r2}",
                    l.rowPitch,
                    (*s).stride,
                    (*sc).extent.width,
                    (*sc).extent.height);
                if r2 == VK_SUCCESS && img2 != 0 {
                    unsafe {
                        if let Some(destroy) = f.destroy_image {
                            destroy(dev, (*s).image, ptr::null());
                        }
                        (*s).image = img2;
                    }
                    explicit_layout = true;
                }
            }
            if !explicit_layout && l.rowPitch != 0 && (l.rowPitch != u64::from((*s).stride) || l.offset != 0) {
                let can_ahb = unsafe {
                    gn_real_device_proc(gd, c"vkGetAndroidHardwareBufferPropertiesANDROID")
                }
                .is_some();
                crate::logf!("proxy slot: LINEAR pitch {} != pool stride {} for {}x{}; {}",
                    l.rowPitch,
                    (*s).stride,
                    (*sc).extent.width,
                    (*sc).extent.height,
                    if can_ahb { "importing pool buffer as AHardwareBuffer" } else { "no AHardwareBuffer import on this driver, frames will shear" });
                if can_ahb {
                    SLOT_DMABUF_REFUSED.store(true, Ordering::Relaxed);
                    unsafe {
                        if let Some(destroy) = f.destroy_image {
                            destroy(dev, (*s).image, ptr::null());
                        }
                        (*s).image = 0;
                        libc::close((*s).fd);
                        (*s).fd = -1;
                    }
                    return unsafe { slot_init_ahb(sc, s, usage) };
                }
            }
        }
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
        crate::logf!("proxy import image: no memory type");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    crate::logf!("proxy slot: linear {}x{} reqs {} memTypeBits {:#x} {}",
        (*sc).extent.width,
        (*sc).extent.height,
        reqs.size,
        reqs.memoryTypeBits,
        mem_type_desc(gd, mem_type));
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
        if unsafe { gn_real_device_proc(gd, c"vkGetAndroidHardwareBufferPropertiesANDROID") }
            .is_some()
        {
            crate::logf!("proxy slot: dma-buf import failed ({res}), importing pool buffer as AHardwareBuffer");
            SLOT_DMABUF_REFUSED.store(true, Ordering::Relaxed);
            unsafe {
                if let Some(destroy) = f.destroy_image {
                    destroy(dev, (*s).image, ptr::null());
                }
                (*s).image = 0;
                (*s).mem = 0;
                libc::close((*s).fd);
                (*s).fd = -1;
            }
            return unsafe { slot_init_ahb(sc, s, usage) };
        }
    }
    if res != VK_SUCCESS {
        crate::logf!("proxy import memory failed: {res}");
        return res;
    }
    slot_init_sync(sc, s)
}

/// Set once the driver refuses a pool buffer's dma-buf although it advertises
/// dma-buf import (the Mali-G720): later slots import the `AHardwareBuffer`.
static SLOT_DMABUF_REFUSED: AtomicBool = AtomicBool::new(false);

pub(crate) const VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID:
    VkExternalMemoryHandleTypeFlags = 0x400;
pub(crate) const STYPE_AHB_PROPERTIES: VkStructureType = 1000129000;
pub(crate) const STYPE_AHB_FORMAT_PROPERTIES: VkStructureType = 1000129002;
pub(crate) const STYPE_IMPORT_AHB: VkStructureType = 1000129003;

#[repr(C)]
pub(crate) struct AhbProperties {
    pub s_type: VkStructureType,
    pub next: *mut c_void,
    pub allocation_size: u64,
    pub memory_type_bits: u32,
}

#[repr(C)]
pub(crate) struct AhbFormatProperties {
    pub s_type: VkStructureType,
    pub next: *mut c_void,
    pub format: VkFormat,
    pub external_format: u64,
    pub format_features: u32,
    pub components: [u32; 4],
    pub ycbcr: [u32; 4],
}

#[repr(C)]
pub(crate) struct ImportAhb {
    pub s_type: VkStructureType,
    pub next: *const c_void,
    pub buffer: *mut c_void,
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
        crate::logf!("proxy slot: no AHardwareBuffer from the daemon for {width}x{height}");
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
        crate::logf!("proxy slot: driver resolves no vkGetAndroidHardwareBufferPropertiesANDROID");
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
    crate::logf!("proxy slot: AHB fd={} ino={} stride={} size {} memTypeBits {:#x} format {} (swapchain {}): {res}",
        buf.fd,
        buf.ino,
        unsafe { (*s).stride },
        props.allocation_size,
        props.memory_type_bits,
        fmt.format,
        unsafe { (*sc).format });
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
        crate::logf!("proxy import image (AHB) create failed: {res}");
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
    crate::logf!("proxy slot: AHB import {width}x{height} type {} {}: {res}",
        mem_type,
        mem_type_desc(gd, mem_type));
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
