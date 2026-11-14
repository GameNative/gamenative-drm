//! Native SurfaceControl presenter — the publish half of route B (doc
//! `android/06`).
//!
//! The buffer gamescope renders into is an `AHardwareBuffer` the daemon owns,
//! so presentation is a `SurfaceControl` + `SurfaceTransaction` publish of
//! that very buffer: `setBuffer` + `setGeometry` + `apply`, straight to
//! SurfaceFlinger. The publish path has no separate GPU context, no second
//! buffer queue and no copy anywhere in it.
//!
//! Ownership runs this way round because the daemon is the process that must
//! call `ASurfaceTransaction_setBuffer`, and an `AHardwareBuffer` cannot be
//! obtained from a bare dma-buf fd on API 33 — whoever publishes it has to be
//! whoever allocated it. So the shim asks for a scan-out buffer over a
//! dedicated Unix socket ([`serve_scanout`]) — by geometry when it matched a
//! connector mode, or by allocation length when it did not, in which case the
//! daemon resolves the length to a geometry it knows (a mode, a committed
//! framebuffer size, or a geometry already in its pool) — and receives a
//! dma-buf fd back; gamescope renders into that fd exactly as it would into a
//! dma-heap allocation. The registration key is the fd's `stat.st_ino`, the
//! same identity the GEM store dedups `PRIME_FD_TO_HANDLE` imports by, so a
//! committed framebuffer resolves to its buffer with no extra bookkeeping.
//!
//! The buffer pool is a separate lock ([`ScanoutPool`]) from the publish
//! state: the present path holds its state lock across a blocking
//! `ASurfaceTransaction_apply` (a SurfaceFlinger round-trip), and the
//! allocator runs on its own thread with the shim's 250 ms reply budget, so
//! sharing one lock would let a compositor stall starve a scan-out request.
//! That lock also guards bookkeeping only — every gralloc call
//! (`AHardwareBuffer_allocate`/`_release`/`_getNativeHandle`) and every
//! blocking wait runs with it dropped — and the pool holds a pre-allocated
//! spare ([`WarmDoorbell`]) so a request is normally answered without any
//! gralloc call at all. Retirements arrive in bursts, and their releases must
//! never be able to deliver a reply late.
//!
//! A retired buffer is not freed: it is kept in a reuse cache
//! ([`gndrm_daemon::scanout::ReusePool`]) and handed back out on a later
//! request of the same geometry, the way a kernel DRM driver recycles GEM
//! buffers. The steady state therefore allocates nothing — the pool settles at
//! the session's working set and the gralloc allocation cost leaves the
//! request path entirely. The reuse rule is the flip event: a buffer is only
//! cached once the presenter has reported its completion, so SurfaceFlinger
//! is never handed a buffer it may still be compositing. The warm spare stays
//! as the allocate-latency cover for the case where the cache is genuinely
//! empty (the first frames, or a geometry change).
//!
//! The sole presentation path (doc `android/06`). When the API-29 publish
//! surface is absent (or a layer cannot be created) the presenter logs once
//! and drops frames: the daemon keeps serving gamescope regardless.
//!
//! Back-pressure is SurfaceFlinger's own release fence. With no copy in the
//! path the published buffer IS the one being scanned out, so re-publishing it
//! before SurfaceFlinger has finished would tear the frame gamescope is still
//! drawing into; the completion callback captures that fence and
//! [`AscState::publish`] waits on it before reusing the same buffer.

use core::ffi::{c_int, c_void};
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::RawFd;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use gndrm_daemon::diag::{self, Diag, Reply, Role, Stage, State, TrackedMutex, LOCK_SCANOUT_POOL};
use gndrm_daemon::scanout::{
    serves_len, Geometry, Removed, Reservation as CacheReservation, ReusePool,
};
use gndrm_daemon::{CursorUpdate, FrameCap, Framebuffer, PlaneState, Presenter};
use gndrm_uapi::wire::{
    scanout_ahb_format, BufferReleased, FrameAcquired, FrameCapSubscribe, LsfgConfig, PresentDone,
    PresentMark, ScanoutAllocReq, ScanoutAllocResp, CAP_FRAME_CAP, CAP_PRESENT_ID,
    CAP_RELEASE_NOTE, CAP_VSYNC_NOTE,
};

use crate::ffi;
use crate::foreign::{CursorStage, ForeignCopy, Layout};
use crate::{log_always, log_info, log_warn};

/// Budget for the previous release fence (a 30 Hz frame). A fence that never
/// signals must not stall the flip loop, so the frame is published anyway and
/// the timeout is logged once.
const RELEASE_FENCE_TIMEOUT_MS: c_int = 33;

/// Budget for a foreign frame's acquire fence before its pixels are read on
/// the CPU. A fence that never signals copies whatever is there rather than
/// stall the flip loop.
const ACQUIRE_FENCE_TIMEOUT_MS: c_int = 100;

/// Always-on foreign-buffer lines per session (first copy or failure per
/// inode), so a client that churns buffers cannot flood logcat.
const FOREIGN_LOG_BUDGET: u32 = 32;

/// How long the spare worker waits before retrying a failed refill. A gralloc
/// failure is transient (another process holding the mapper), so the retry
/// keeps the session's spare; the pause stops a persistent failure spinning.
const WARM_RETRY_MS: u64 = 100;

/// Largest scan-out geometry the allocator will create (matches the panel
/// ceiling the emulated device advertises) and the most buffers it will own
/// at once. gamescope's scan-out swapchain is a handful of mode-sized buffers;
/// the cap bounds what a misbehaving client can pin.
const MAX_SCANOUT_DIM: u32 = 8192;
/// Cap on the buffers the pool owns: it bounds `live + free + reserved`.
/// The guest's swapchain images are imported for the whole session and are
/// never retired, so they hold slots permanently; on top of those sit the
/// in-flight scan-out set, one spare, and a free list per geometry in use.
const MAX_SCANOUT_POOL: usize = 64;

/// Panel-dump rate limit: the first [`DUMP_FIRST`] published frames in full,
/// then every [`DUMP_EVERY`]-th, and one immediately on a geometry change (a
/// resized scan-out must not wait for the next sample). Matches the daemon's
/// other periodic diagnostics.
const DUMP_FIRST: u64 = 10;
const DUMP_EVERY: u64 = 120;

/// Publish-probe period, in published frames. The probe also prints on a change
/// of the decision or either input, so this exists to keep it in a capture that
/// starts after the transition — the probe's earliest line arrives at session
/// start, long before any post-launch capture begins. 121 is coprime with the
/// two-buffer scan-out pool: a multiple of 120 always lands on the same buffer,
/// which hides whether the published inode is cycling.
const PUBLISH_LOG_EVERY: u64 = 121;

/// `GNDRM_DUMP_DIR`, read once: the directory the panel dump is written to.
/// `None` (unset or empty) disables the dump entirely, so it costs nothing on
/// the publish path when dumps are off.
static DUMP_DIR: OnceLock<Option<std::ffi::OsString>> = OnceLock::new();

/// Daemon diagnostics are opt-in per session (`GNDRM_LOG_DAEMON`, set by the
/// launcher in this process): the daemon shares the app's address space, so a
/// container's `GNDRM_LOG_*` cannot reach it, and a shipped session must not
/// describe the presenter in logcat.
macro_rules! asc_log {
    ($($arg:tt)*) => {
        if gndrm_daemon::verbose_logging() {
            $crate::log_always!($($arg)*)
        }
    };
}

/// Whether the HDR static metadata has been set on the current PQ layer. It
/// describes the panel, not the frame, so it is issued once per entry into PQ
/// rather than once per commit; the flag clears when the layer leaves PQ.
static PQ_METADATA_SET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// ---------------------------------------------------------------------------
// the release hold probe
//
// How long does SurfaceFlinger keep a buffer after showing it? The publisher
// only learns it where it waits for the previous release fence before reusing a
// registration, and the answer decides which release rule is safe: gamescope's
// page-flip event is emitted at publish, so a producer given a buffer at that
// event may still be writing into a buffer on the panel. Holding the release
// one flip longer (gamescope A56) fixes that by taking a buffer away from every
// producer — a 3-image swapchain loses half its depth. This measures the real
// number instead: hold / one display period. Below one period, that deferral is
// sufficient; above it, only SurfaceFlinger's own fence is.
// ---------------------------------------------------------------------------

/// The last latch the completion callback reported, for the flip period.
static HOLD_LAST_LATCH_NS: AtomicU64 = AtomicU64::new(0);
/// The last [`HOLD_GAPS`] accepted latch gaps. One gap is a sample of the
/// display, but a burst of callbacks, or a second swapchain publishing between
/// two latches, makes it far too small — and a period that small turns every
/// hold into an outlier. The median of the last few is what the buckets are
/// percentages of.
const HOLD_GAPS: usize = 9;
/// A latch gap outside this band is not a display period: below the fastest
/// panel this ships on, above a stall.
const HOLD_GAP_MIN_NS: u64 = 3_000_000;
const HOLD_GAP_MAX_NS: u64 = 50_000_000;
static HOLD_GAP_RING: Mutex<[u64; HOLD_GAPS]> = Mutex::new([0; HOLD_GAPS]);
static HOLD_GAP_NEXT: AtomicU64 = AtomicU64::new(0);
/// One display period as the latch grid delivered it, 0 until two latches.
static HOLD_PERIOD_NS: AtomicU64 = AtomicU64::new(0);
/// Release-fence waits that produced a sample.
static HOLD_N: AtomicU64 = AtomicU64::new(0);
/// Waits that hit `RELEASE_FENCE_TIMEOUT_MS`: no hold, no sample.
static HOLD_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static HOLD_MAX_US: AtomicU64 = AtomicU64::new(0);
/// Hold as a percentage of one period, bucketed: <50, 50-100, 100-150,
/// 150-250, >250.
static HOLD_BUCKETS: [AtomicU64; 5] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static HOLD_LAST_REPORT_NS: AtomicU64 = AtomicU64::new(0);

/// One measured hold: `latch_ns` is when SurfaceFlinger showed the frame the
/// buffer carried, `released_ns` when its release fence signalled.
fn note_hold(latch_ns: u64, released_ns: u64) {
    let hold_ns = released_ns.saturating_sub(latch_ns);
    let hold_us = hold_ns / 1000;
    HOLD_N.fetch_add(1, Ordering::Relaxed);
    HOLD_MAX_US.fetch_max(hold_us, Ordering::Relaxed);
    let period_ns = HOLD_PERIOD_NS.load(Ordering::Relaxed);
    // Nothing to compare against until a few gaps have been seen: before the
    // ring fills, a burst can still pass as a period and every hold then reads
    // as an outlier.
    if period_ns == 0 || HOLD_GAP_NEXT.load(Ordering::Relaxed) < HOLD_GAPS as u64 / 2 {
        return;
    }
    let percent = hold_ns.saturating_mul(100) / period_ns;
    let bucket = match percent {
        0..=49 => 0,
        50..=99 => 1,
        100..=149 => 2,
        150..=249 => 3,
        _ => 4,
    };
    HOLD_BUCKETS[bucket].fetch_add(1, Ordering::Relaxed);
}

/// The release-fence wait ran out of budget, so this frame has no hold sample.
fn note_hold_timeout() {
    HOLD_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
}

/// One line per 10 s naming SurfaceFlinger's hold distribution. Counts, not
/// percentages, so a run can be added up; `period` is the latch grid's own, and
/// the buckets are in halves of it.
fn report_hold(now_ns: u64) {
    let last = HOLD_LAST_REPORT_NS.load(Ordering::Relaxed);
    if last != 0 && now_ns.saturating_sub(last) < 10_000_000_000 {
        return;
    }
    if HOLD_LAST_REPORT_NS
        .compare_exchange(last, now_ns, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    // Marks, before the hold: a session whose marks do not resolve sends notes
    // with no ticket, and that is invisible in every other counter.
    let settled = MARK_HIT.load(Ordering::Relaxed)
        + MARK_ORDER.load(Ordering::Relaxed)
        + MARK_MISS.load(Ordering::Relaxed)
        + MARK_STALE.load(Ordering::Relaxed)
        + MARK_RESYNC.load(Ordering::Relaxed)
        + MARK_SKEW.load(Ordering::Relaxed);
    if settled != MARK_REPORTED.swap(settled, Ordering::Relaxed) {
        let depth = PRESENT_PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        log_always!(
            "{}",
            format_args!(
                "sf mark: hit={} order={} miss={} stale={} resync={} skew={} pending={} last={}",
                MARK_HIT.load(Ordering::Relaxed),
                MARK_ORDER.load(Ordering::Relaxed),
                MARK_MISS.load(Ordering::Relaxed),
                MARK_STALE.load(Ordering::Relaxed),
                MARK_RESYNC.load(Ordering::Relaxed),
                MARK_SKEW.load(Ordering::Relaxed),
                depth,
                MARK_LAST_TICKET.load(Ordering::Relaxed)
            )
        );
    }
    let n = HOLD_N.load(Ordering::Relaxed);
    if n == 0 {
        return;
    }
    let b = |i: usize| HOLD_BUCKETS[i].load(Ordering::Relaxed);
    // Ungated: a shipped session cannot turn the daemon's gated channels on
    // (`GNDRM_LOG_*` is set by the launcher in this process, not by a
    // container), and this is the number that decides which release rule is
    // safe. One line per 10 s is the same cadence as the shim's census.
    log_always!(
        "{}",
        format_args!(
            "sf hold: n={} <half={} half-one={} one-1.5={} 1.5-2.5={} over-2.5={} max={}us period={}us timeouts={}",
            n,
            b(0),
            b(1),
            b(2),
            b(3),
            b(4),
            HOLD_MAX_US.load(Ordering::Relaxed),
            HOLD_PERIOD_NS.load(Ordering::Relaxed) / 1000,
            HOLD_TIMEOUTS.load(Ordering::Relaxed)
        )
    );
}

/// The dump directory if the environment requests one.
fn dump_dir() -> Option<&'static std::ffi::OsString> {
    DUMP_DIR
        .get_or_init(|| std::env::var_os("GNDRM_DUMP_DIR").filter(|v| !v.is_empty()))
        .as_ref()
}

/// Debug name of the layer; shows up in SurfaceFlinger dumps.
const SC_DEBUG_NAME: &core::ffi::CStr = c"gndrm-scanout";
/// Debug name of the cursor layer (`dumpsys SurfaceFlinger`).
const SC_CURSOR_DEBUG_NAME: &core::ffi::CStr = c"gndrm-cursor";
/// Z-order of the scan-out and cursor layers among the window's children.
const Z_CURSOR: i32 = 1;
/// Always-on cursor-image lines printed per session.
const CURSOR_LOG_BUDGET: u32 = 8;
/// Always-on cursor-presentation lines printed per session.
const CURSOR_STAGE_LOG_BUDGET: u32 = 64;

/// A system property, or an empty string when it is unset.
fn property(name: &core::ffi::CStr) -> String {
    let mut value = [0u8; 92];
    // SAFETY: `name` is NUL-terminated and `value` is PROP_VALUE_MAX bytes.
    let n = unsafe {
        libc::__system_property_get(name.as_ptr(), value.as_mut_ptr().cast::<libc::c_char>())
    };
    let n = usize::try_from(n).unwrap_or(0).min(value.len());
    String::from_utf8_lossy(&value[..n]).trim().to_owned()
}

/// Whether the panel is taken to be landscape in its natural orientation:
/// tablets (`ro.build.characteristics` lists `tablet`); phones and handhelds
/// are portrait panels. See [`gndrm_daemon::cursor::derive_rotation`].
fn natural_landscape() -> bool {
    static NATURAL_LANDSCAPE: OnceLock<bool> = OnceLock::new();
    *NATURAL_LANDSCAPE.get_or_init(|| {
        property(c"ro.build.characteristics")
            .split(',')
            .any(|c| c.trim() == "tablet")
    })
}

/// The cursor layer's debug overrides, read on every cursor show/move/image so
/// they can be switched live:
/// - `debug.gndrm.cursor_mode`: `0` hands the padded cursor buffer to
///   SurfaceFlinger as KMS describes it (scaled by the composer, buffer
///   transform 0); unset or anything else stages it (the default, see
///   [`gndrm_daemon::cursor`]).
/// - `debug.gndrm.cursor_rot`: the staging rotation in degrees clockwise
///   (0/90/180/270) instead of the derived one.
///
/// Returns `(staged, rotation override)`.
fn cursor_overrides() -> (bool, Option<u32>) {
    let staged = property(c"debug.gndrm.cursor_mode").parse::<u32>() != Ok(0);
    let rot = property(c"debug.gndrm.cursor_rot")
        .parse::<u32>()
        .ok()
        .map(gndrm_daemon::cursor::normalize_rotation);
    (staged, rot)
}

/// What a staged cursor image was built from.
#[derive(Clone, Copy, PartialEq, Eq)]
struct StageKey {
    images: u64,
    rot: u32,
    crop: (u32, u32, u32, u32),
    out: (u32, u32),
}

/// One scan-out buffer the daemon allocated, on its way to the client.
pub struct ScanoutBuffer {
    /// Fresh descriptor the caller owns and the shim hands to gamescope.
    pub fd: RawFd,
    /// `stat.st_ino` of `fd`: the identity a committed framebuffer resolves to.
    pub ino: u64,
    /// Row stride in bytes (`AHardwareBuffer_Desc::stride` counts pixels for a
    /// 32-bit format, and DRM pitches are bytes).
    pub stride: u32,
    /// Real allocation length of the backing dma-buf (`stat.st_size`), row
    /// padded and page rounded — the length a length-only scan-out request
    /// carries. A compressed (UBWC) gralloc buffer carries its metadata region
    /// past the pixels, so this is longer than the tight `width * height * 4`;
    /// the pool registers it so a later request for that exact length resolves.
    pub size: u64,
}

/// Completion-callback mailbox, one per registered dma-buf inode.
///
/// The callback runs on whatever thread SurfaceFlinger completes the
/// transaction on, so it can only reach this through a raw pointer; the
/// presenter leaks it (never frees it) because there is no point at which
/// "no callback can still be in flight" is knowable. It is a few words per
/// registered buffer, and registrations are bounded by the shim's pool.
#[repr(C)]
struct ReleaseCtx {
    /// The two stats functions the completion callback needs, copied in because
    /// the API table is not reachable from the SurfaceFlinger callback thread.
    /// The callback asks the stats for the controls the reported transaction
    /// itself updated and queries the fence on one of those, so the mailbox
    /// holds no layer handle at all and none can go stale across a retarget.
    stats_get_controls: ffi::FnStStatsGetASurfaceControls,
    stats_release_controls: ffi::FnStStatsReleaseASurfaceControls,
    previous_release: ffi::FnStStatsGetPreviousReleaseFenceFd,
    /// fd of the previous release fence, `-1` when none is waiting.
    release_fd: AtomicI32,
    /// The latch-time query, when the platform has it.
    latch_time: Option<ffi::FnStStatsGetLatchTime>,
    /// The latch SurfaceFlinger took the frame in this registration at,
    /// `CLOCK_MONOTONIC` ns; 0 before the first. The publish that reuses the
    /// registration reads it to measure how long SurfaceFlinger held the buffer
    /// after showing it (see [`note_hold`]).
    latch_ns: AtomicU64,
    /// When the daemon's apply of the transaction this registration was last
    /// published in returned, `CLOCK_MONOTONIC` ns; 0 before the first.
    applied_ns: AtomicU64,
    /// The guest's present id for the frame currently in this registration, 0
    /// when the app did not chain one. Written by the publish path, consumed by
    /// the completion callback, which reports it with the latch time.
    present_id: AtomicU64,
    /// The shim's ticket for the frame currently in this registration: the key
    /// its release note is filed under, and 0 before the first publish. This is
    /// what makes a release note exact — the ticket is minted by the process
    /// that owns the slot, so it cannot be ambiguous the way a dma-buf inode can.
    ticket: AtomicU64,
    /// The ticket this registration carried *when the transaction that replaced it
    /// was applied* — the frame the release fence actually describes.
    ///
    /// Not `ticket`: that field is overwritten by the buffer's next publish, and the
    /// completion callback runs on SurfaceFlinger's binder thread some time after the
    /// apply. Normally long before the buffer is reused, but a delayed callback (a
    /// stalled SF, a suspend) can arrive after the buffer has been published again —
    /// and the note would then name the *newer* frame, freeing the slot that holds it
    /// while the display may still be reading it. Snapshotting at the apply removes
    /// the window: between the apply and the callback this buffer is on the layer and
    /// cannot be published again.
    release_ticket: AtomicU64,
    /// Set by the completion callback: SurfaceFlinger reported the flip event
    /// for this registration, so the buffer is safe to hand back out (the
    /// reuse cache keys on it). Never cleared: the flag belongs to this
    /// registration, and a registration is not reused.
    latched: Arc<AtomicBool>,
    /// The scan-out inode this mailbox belongs to: the identity a release note
    /// names, and 0 for a foreign slot the pool does not own.
    ino: u64,
    /// The mailbox of the buffer this registration's buffer replaced on the
    /// layer, or null when it replaced nothing.
    ///
    /// This is what makes SurfaceFlinger's release fence attributable. Its
    /// `getPreviousReleaseFenceFd` describes the buffer the transaction
    /// *replaced*, not the one it published — so the fence a completion reports
    /// belongs to `replaced_ctx`, and is filed there for that buffer's own
    /// reuse and its own release note.
    replaced_ctx: AtomicPtr<ReleaseCtx>,
}

// SAFETY: after construction the only mutations are the atomic and the release
// fd, and the other fields are immutable — so the mailbox is safe to share with
// the callback thread.
unsafe impl Send for ReleaseCtx {}
unsafe impl Sync for ReleaseCtx {}

// ---------------------------------------------------------------- present ids
//
// The guest can ask when a frame reached the panel: it chains `VkPresentIdKHR`
// in its present, and waits on the id through `vkWaitForPresentKHR`. Nothing in
// Vulkan carries that id to the display side, so the shim marks the frame on the
// scan-out channel and this daemon answers when the transaction that carried it
// completes — with the latch time it already samples for the vsync grid.

/// Marks received from the shim, by scan-out inode, waiting for the commit that
/// publishes them. One frame per buffer is in flight at a time, so a plain map
/// is enough; it is bounded because a mark for a buffer that never gets
/// committed must not accumulate.
static PRESENT_PENDING: Mutex<Vec<(u64, u64, u64)>> = Mutex::new(Vec::new());

/// Completions not yet written to the shim: the callback runs on SurfaceFlinger's
/// thread, the channel on the scan-out thread, and only the latter may send.
static PRESENT_DONE: Mutex<Vec<PresentDone>> = Mutex::new(Vec::new());

/// Frames acquired as last reported by the shim: the app's frame ring is fed the
/// difference, so it follows the rate the limiter controls rather than the rate
/// the compositor shows.
static ACQUIRED_SEEN: AtomicU64 = AtomicU64::new(0);

/// Frames frame generation had inserted when the producer last reported, so the
/// delta the app's ring sees includes them: they are produced, never acquired.
static GENERATED_SEEN: AtomicU64 = AtomicU64::new(0);

/// The most produced frames one report may add to the ring. One report is sent
/// per acquired frame, so a healthy delta is one frame plus the multiplier; this
/// only bounds what a corrupt count could make the loop do.
const MAX_PRODUCED_PER_MESSAGE: u64 = 4096;
/// Monotonic time of the last producer report, in ms since the daemon's epoch
/// (0 = never). A session whose frames never go through the ICD shim (Xwayland
/// GL, GDI, software rendering) sends no reports, so the publish path feeds the
/// frame ring for it instead; once reports flow, the publish path goes quiet.
static LAST_PRODUCER_REPORT_MS: AtomicU64 = AtomicU64::new(0);
/// How long without a producer report before publishes count again.
const PRODUCER_REPORT_GRACE_MS: u64 = 1000;

fn monotonic_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = *EPOCH.get_or_init(Instant::now);
    // Never 0: 0 means "no report yet" in LAST_PRODUCER_REPORT_MS.
    Instant::now().duration_since(epoch).as_millis() as u64 + 1
}

/// Whether the frame ring is currently fed by producer reports.
fn producer_reports_live() -> bool {
    let last = LAST_PRODUCER_REPORT_MS.load(Ordering::Relaxed);
    last != 0 && monotonic_ms().saturating_sub(last) <= PRODUCER_REPORT_GRACE_MS
}

/// Eventfd the completion callback writes to wake the scan-out thread. -1 until
/// the channel is served, which is before any marked frame can exist.
static PRESENT_FD: AtomicI32 = AtomicI32::new(-1);

/// Whether the connection reading this channel asked for present-done notes.
static PRESENT_AWARE: AtomicBool = AtomicBool::new(false);

/// How many marks and completions are held, oldest dropped first. A guest that
/// marks frames and never waits cannot grow either list without bound.
const PRESENT_QUEUE_CAP: usize = 64;

/// How marks resolve when a registration is published.
///
/// `hit` matched by inode; `order` matched the oldest unclaimed mark because the
/// inode did not; `miss` found no mark at all; `skew` found one that would move
/// the ticket backwards and refused it. The first two are the pairing rule, the
/// last two are the two ways it can fail — and until they were counted, a session
/// that sent notes with `ticket: 0` was indistinguishable from one that sent no
/// notes: RP6 ran sixty seconds at 60 fps with `tick=[n:0]`, every release note
/// carrying no ticket because this lookup missed.
static MARK_HIT: AtomicU64 = AtomicU64::new(0);
static MARK_ORDER: AtomicU64 = AtomicU64::new(0);
static MARK_MISS: AtomicU64 = AtomicU64::new(0);
static MARK_SKEW: AtomicU64 = AtomicU64::new(0);
/// Marks dropped because their ticket was already published. An identity hit can
/// remove a mark from the middle of the queue, leaving older ones behind that can
/// never be assigned — discarding them is what stops the queue wedging on a head
/// no publish will ever match.
static MARK_STALE: AtomicU64 = AtomicU64::new(0);
/// Fallbacks that accepted a small forward gap: the marks in between were lost, not
/// mis-ordered. This is the counter that says whether `sendmsg` is dropping under
/// load, which nothing measured before.
static MARK_RESYNC: AtomicU64 = AtomicU64::new(0);

/// The largest forward gap the order fallback will re-seed across. One lost mark is
/// a dropped send; four is a burst; more than that is a queue this code does not
/// understand, and the ticket path stands down until identity carries it.
const MARK_RESYNC_GAP: u64 = 4;
/// The highest ticket assigned to a registration: the order fallback may only
/// move it forward, so a lost or late mark cannot pair every later frame with the
/// one before it.
static MARK_LAST_TICKET: AtomicU64 = AtomicU64::new(0);
/// Marks reported so far, for the once-every-ten-seconds line.
static MARK_REPORTED: AtomicU64 = AtomicU64::new(0);

/// Record a frame's present id against the buffer carrying it.
fn note_present_mark(mark: PresentMark) {
    // A mark with no ticket and no guest id says nothing: the shim sends one per
    // commit now, so the guest id is usually 0 and only the ticket is present.
    if (mark.id == 0 && mark.ticket == 0) || mark.ino == 0 {
        return;
    }
    let mut pending = PRESENT_PENDING.lock().unwrap_or_else(|e| e.into_inner());
    pending.retain(|&(ino, _, _)| ino != mark.ino);
    if pending.len() >= PRESENT_QUEUE_CAP {
        pending.remove(0);
    }
    pending.push((mark.ino, mark.id, mark.ticket));
}

/// Take the `(id, ticket)` marked for `ino`: the commit naming that buffer is the
/// frame they belong to.
///
/// Identity first, then order. The inode is the sharper match and is all that runs
/// where the shim's slot inode and this registration's inode are the same object —
/// true for Portal 2 on Nova, where only 6 % of parks needed a fallback.
///
/// It is not available everywhere: the same device needed it for 84 % of Dave's
/// parks, and RP6 and Odin 3 needed it for all of them, so the mark that carries
/// the ticket would be dropped on exactly the devices the ticket exists for. Both
/// ends see the same commit sequence, so the oldest unclaimed mark belongs to the
/// publish being made now — no window, no floor, no reordering, unlike the shim's
/// own order arm, which pairs frames that have already been shown.
///
/// The guard on the fallback is exactness, not monotonicity: the next publish must
/// take the *next* ticket, because one mark is sent per commit in order. A permissive
/// "greater than the last" rule accepts a mis-assigned ticket — a mark that belongs to
/// a different frame gets bound to this registration, and when that buffer's release
/// fires the shim frees the slot holding somebody else's frame. That is a wrong-slot
/// free, which is a tear, not a reordering: the present path never consults a ticket.
///
/// The three outcomes below are what keeps the rule usable:
///
/// - **stale** — the head's ticket was already published. Discarded and the queue
///   re-examined, because an identity hit can remove a mark from the middle and leave
///   older ones queued that no publish will ever match. Refusing instead would wedge
///   the head forever.
/// - **resync** — a small forward gap, so the marks in between were lost rather than
///   mis-ordered. Accepted and counted: this is the only measurement of `sendmsg`
///   dropping marks under load, which nothing had.
/// - **skew** — a gap beyond [`MARK_RESYNC_GAP`]. The queue and the publisher disagree
///   about where they are, so the ticket path stands down and identity carries it.
fn take_present_mark(ino: u64) -> (u64, u64) {
    let mut pending = PRESENT_PENDING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = pending.iter().position(|&(marked, _, _)| marked == ino) {
        let (_, id, ticket) = pending.remove(i);
        MARK_HIT.fetch_add(1, Ordering::Relaxed);
        MARK_LAST_TICKET.fetch_max(ticket, Ordering::Relaxed);
        return (id, ticket);
    }
    loop {
        let last = MARK_LAST_TICKET.load(Ordering::Relaxed);
        let Some(&(_, id, ticket)) = pending.first() else {
            MARK_MISS.fetch_add(1, Ordering::Relaxed);
            return (0, 0);
        };
        if ticket == 0 || ticket <= last {
            pending.remove(0);
            MARK_STALE.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if ticket > last + MARK_RESYNC_GAP {
            MARK_SKEW.fetch_add(1, Ordering::Relaxed);
            return (0, 0);
        }
        if ticket > last + 1 {
            MARK_RESYNC.fetch_add(1, Ordering::Relaxed);
        }
        pending.remove(0);
        MARK_ORDER.fetch_add(1, Ordering::Relaxed);
        MARK_LAST_TICKET.store(ticket, Ordering::Relaxed);
        return (id, ticket);
    }
}

/// Forget every mark and the ticket order they were checked against.
///
/// Tickets are minted per shim process and start again at 1 in the next one, so
/// the previous process's highest ticket would refuse every order match of the
/// new one as skew, and its unclaimed marks would sit at the head of the queue.
/// A reconnect of the same process loses at most the marks in flight, whose notes
/// then carry no ticket.
fn reset_present_marks() {
    let mut pending = PRESENT_PENDING.lock().unwrap_or_else(|e| e.into_inner());
    pending.clear();
    MARK_LAST_TICKET.store(0, Ordering::Relaxed);
}

/// Report a completed present to the shim: queued here, written by the channel.
fn note_present_done(id: u64, latch_ns: u64) {
    if id == 0 {
        return;
    }
    {
        let mut done = PRESENT_DONE.lock().unwrap_or_else(|e| e.into_inner());
        if done.len() >= PRESENT_QUEUE_CAP {
            done.remove(0);
        }
        done.push(PresentDone { id, latch_ns });
    }
    let fd = PRESENT_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: `fd` is the eventfd this channel created; writing a count is
        // the whole protocol.
        unsafe { libc::write(fd, (&1u64 as *const u64).cast(), 8) };
    }
}

/// `ASurfaceTransaction_setOnComplete`: capture the fence that says
/// SurfaceFlinger has finished with the buffer published in the previous
/// transaction. First writer wins; a second fence arriving before the present
/// path consumed the first describes the same slot and is closed.
unsafe extern "C" fn on_complete(context: *mut c_void, stats: *mut ffi::ASurfaceTransactionStats) {
    if context.is_null() {
        return;
    }
    // Publish the callback's activity: the completion is SurfaceFlinger's
    // release signal for this frame, so a snapshot must be able to say whether
    // a callback was running when a request stalled.
    diag::diag().set_role(Role::PresenterCallback, State::PresenterCallback);
    // SAFETY: `context` is the leaked ReleaseCtx this presenter handed to
    // setOnComplete; it outlives every callback by construction.
    let ctx = unsafe { &*context.cast::<ReleaseCtx>() };
    // Ask `stats` for the controls this transaction itself updated: a publish
    // transaction updates exactly one control — the layer it published on — so
    // the entry names the layer of *this* transaction. A control taken from
    // `stats` is by construction one of `stats`' own entries, so the fence query
    // below can never be handed a handle that belongs to a different
    // transaction. This is the invariant the callback keeps: no callback ever
    // passes a handle that could belong to a different transaction. The mailbox
    // holds no layer handle, so a retarget cannot make it point at another
    // transaction's layer.
    let mut controls: *mut *mut ffi::ASurfaceControl = ptr::null_mut();
    let mut count: usize = 0;
    // SAFETY: `stats` belongs to this completion; the out-parameters are valid
    // for the two values the call stores.
    unsafe { (ctx.stats_get_controls)(stats, &mut controls, &mut count) };
    let fd = if count >= 1 && !controls.is_null() {
        // SAFETY: `controls` is an array of `count` controls, each an entry of
        // `stats`.
        let sc = unsafe { *controls };
        // SAFETY: `sc` is one of `stats`' own controls, which is exactly the
        // handle this query requires.
        unsafe { (ctx.previous_release)(stats, sc) }
    } else {
        // The stats name no control (an empty update): there is no previous
        // release fence to take this time. A normal outcome — the publish
        // completes without a fence instead of the query asserting.
        -1
    };
    if !controls.is_null() {
        // SAFETY: `controls` is the array the get call above returned, released
        // exactly once here.
        unsafe { (ctx.stats_release_controls)(controls) };
    }
    // The fence describes the buffer this transaction *replaced*, not the one it
    // published: SurfaceFlinger reports the release of whatever the new buffer
    // took off the layer. So it is filed on the replaced buffer's mailbox — the
    // one whose next publish waits on it, and whose scan-out inode names it to
    // the producer — and a copy goes to the watcher that reports the release.
    let prev = ctx.replaced_ctx.load(Ordering::Acquire);
    if fd >= 0 {
        if prev.is_null() {
            // Nothing was on the layer before this transaction: there is no
            // buffer whose release the fence could describe.
            // SAFETY: `fd` is a valid descriptor owned by this call.
            unsafe { libc::close(fd) };
        } else {
            // Its own copy first: the mailbox's descriptor is consumed by that
            // buffer's next publish (waited on, then closed), so the watcher
            // must not poll a descriptor another thread may close under it.
            // SAFETY: `prev` is a leaked mailbox, live for the session.
            unsafe {
                let watch = libc::dup(fd);
                if watch >= 0 {
                    watch_release(
                        (*prev).ino,
                        (*prev).release_ticket.load(Ordering::Acquire),
                        watch,
                        (*prev).latch_ns.load(Ordering::Relaxed),
                    );
                }
                if (*prev)
                    .release_fd
                    .compare_exchange(-1, fd, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    // A fence was already waiting there: the older one describes
                    // an earlier release of the same buffer and is superseded.
                    libc::close(fd);
                }
            }
        }
    }
    // The latch time is a sample of the panel's latch grid, which the presenter
    // places generated frames on — and the answer a guest waiting on this
    // frame's present id is given.
    let mut latch_ns = 0u64;
    if let Some(latch_time) = ctx.latch_time {
        // SAFETY: `stats` belongs to this completion.
        let latch = unsafe { latch_time(stats) };
        if latch > 0 {
            latch_ns = latch as u64;
            crate::vsync::sample(latch_ns, ctx.applied_ns.load(Ordering::Relaxed));
            // The flip cadence, for the release-hold probe: the gap between
            // consecutive latches is one display period, which is what a hold
            // is measured against. Gaps outside `HOLD_GAP_MIN_NS..=MAX` are a
            // burst or a stall rather than a period, and the period kept is the
            // median of the last few, because one gap is not a measurement.
            let prev = HOLD_LAST_LATCH_NS.swap(latch_ns, Ordering::Relaxed);
            if prev != 0 && latch_ns > prev {
                let gap = latch_ns - prev;
                if (HOLD_GAP_MIN_NS..=HOLD_GAP_MAX_NS).contains(&gap) {
                    let slot = (HOLD_GAP_NEXT.fetch_add(1, Ordering::Relaxed)
                        % HOLD_GAPS as u64) as usize;
                    let mut ring = HOLD_GAP_RING.lock().unwrap_or_else(|e| e.into_inner());
                    ring[slot] = gap;
                    let mut vals: Vec<u64> = ring.iter().copied().filter(|g| *g != 0).collect();
                    drop(ring);
                    vals.sort_unstable();
                    if let Some(mid) = vals.get(vals.len() / 2) {
                        HOLD_PERIOD_NS.store(*mid, Ordering::Relaxed);
                    }
                }
            }
            ctx.latch_ns.store(latch_ns, Ordering::Relaxed);
        }
    }
    note_present_done(ctx.present_id.swap(0, Ordering::AcqRel), latch_ns);
    // The transaction completed: SurfaceFlinger has reported this publish's
    // completion. This is the frame's release signal — the return path the reuse
    // cache and the next publish's release-fence wait key on.
    ctx.latched.store(true, Ordering::Release);
    diag::diag().set_role(Role::PresenterCallback, State::Idle);
}

// ------------------------------------------------------- scan-out releases
//
// A compositor hands a buffer back when it has finished *compositing* with it.
// The display may still be reading it: on this path the consumer is
// SurfaceFlinger, which reports a buffer's true release a transaction later, as
// the previous-release fence of whoever replaced it. A producer that took the
// compositor's release as its own therefore wrote into memory that was still
// being scanned out, and the panel showed two frames at once for however long
// the overlap lasted.
//
// The completion callback already receives that fence, attributed to the buffer
// the transaction replaced (see [`ReleaseCtx::replaced_ctx`]). It is waited on
// here, off the callback thread, and reported to the producer as a
// [`BufferReleased`] note — the one fact no compositor can give a producer.

/// Release fences waiting for the watcher, `(ino, fd, latch_ns of that buffer)`.
/// Added by the completion callback, which must not block: a fence is waited on
/// here instead, and the latch travels with it so the hold is measured where it
/// ends rather than when the buffer happens to come round again.
static RELEASE_WATCH: Mutex<Vec<(u64, u64, RawFd, u64)>> = Mutex::new(Vec::new());

/// Eventfd the callback writes to wake the watcher, so a fence added between
/// polls is picked up at once rather than at the end of one. -1 until the
/// watcher has started; a fence queued before that is drained by its first pass.
static RELEASE_WAKE: AtomicI32 = AtomicI32::new(-1);

/// Whether the watcher thread exists.
static RELEASE_THREAD: AtomicBool = AtomicBool::new(false);

/// How long one fence may stay unsignalled before the release is reported
/// anyway. A fence SurfaceFlinger never signals means the compositor is stuck;
/// holding the producer behind it would turn that into a hang, so the buffer is
/// reported released and the case is counted.
const RELEASE_WATCH_MAX: Duration = Duration::from_millis(500);

/// Fences the watcher may hold at once. A session has one per buffer in the
/// pool; a producer that stopped presenting leaves its last ones behind, and
/// they must not accumulate.
const RELEASE_WATCH_CAP: usize = 32;

/// Hand one release fence to the watcher. `ino` 0 is a buffer the pool copied
/// into (nothing outside holds it), so its fence is only ever closed.
fn watch_release(ino: u64, ticket: u64, fd: RawFd, latch_ns: u64) {
    // Nobody reads release notes: the producer is not a shim that gates on them,
    // so the fence is worth nothing here. Checked per fence rather than per
    // session, because the subscriber that matters may connect after the first
    // buffers are already in flight.
    if ino == 0 || !gndrm_daemon::pushes::wants(CAP_RELEASE_NOTE) {
        // SAFETY: the descriptor was duplicated for this call.
        unsafe { libc::close(fd) };
        return;
    }
    if !RELEASE_THREAD.swap(true, Ordering::Relaxed) {
        let _ = thread::Builder::new()
            .name("gn-release".to_string())
            .spawn(release_watcher);
    }
    RELEASE_WATCH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((ino, ticket, fd, latch_ns));
    let wake = RELEASE_WAKE.load(Ordering::Relaxed);
    if wake >= 0 {
        // SAFETY: `wake` is this module's eventfd, created non-blocking; an
        // 8-byte counter write is the whole protocol.
        unsafe {
            let one = 1u64;
            libc::write(wake, (&raw const one).cast(), 8);
        }
    }
}

/// Report one buffer as released to the producers that gate on it, and close its
/// fence. Every path that retires a watch goes through here, so exactly one note
/// is sent per fence.
fn report_release(ino: u64, ticket: u64, fd: RawFd, latch_ns: u64, source: u64) {
    // SAFETY: the descriptor was created for the watcher and is closed once.
    unsafe { libc::close(fd) };
    // The hold: from the panel taking this buffer to SurfaceFlinger letting it
    // go. Sampled here rather than at the buffer's next publish, which may be
    // seconds later and would measure the pool's rotation instead.
    let now = crate::vsync::now_ns();
    if latch_ns != 0 {
        note_hold(latch_ns, now);
    }
    report_hold(now);
    gndrm_daemon::pushes::push(
        CAP_RELEASE_NOTE,
        &BufferReleased {
            ticket,
            ino,
            latch_ns,
            source,
        }
        .encode(),
    );
}

/// Wait on the release fences the producers are owed, and report each one as it
/// signals.
///
/// One thread for the session, mostly asleep: it wakes on a new fence, on the
/// oldest one ageing out, or on the poll tick. Nothing else waits on it, so a
/// slow fence cannot delay a publish.
fn release_watcher() {
    let wake = eventfd_new();
    RELEASE_WAKE.store(wake, Ordering::Relaxed);
    let mut live: Vec<(u64, u64, RawFd, u64, Instant)> = Vec::new();
    loop {
        {
            let mut queued = RELEASE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
            for (ino, ticket, fd, latch_ns) in queued.drain(..) {
                if live.len() >= RELEASE_WATCH_CAP {
                    // Evicted, not signalled: the same guarantee as a timeout,
                    // because the buffer is being handed back without its fence.
                    let (old_ino, old_ticket, old_fd, old_latch, _) = live.remove(0);
                    report_release(
                        old_ino,
                        old_ticket,
                        old_fd,
                        old_latch,
                        gndrm_uapi::wire::RELEASE_FENCE_TIMEOUT,
                    );
                }
                live.push((ino, ticket, fd, latch_ns, Instant::now()));
            }
        }
        if live.is_empty() {
            // Nothing owed: wait for a fence to be handed over. The wake fd is
            // drained here, so the next `poll` starts clean.
            let mut p = libc::pollfd {
                fd: wake,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd, no timeout.
            unsafe { libc::poll((&raw mut p).cast(), 1, -1) };
            drain_eventfd(wake);
            continue;
        }
        let mut pfds: Vec<libc::pollfd> = Vec::with_capacity(live.len() + 1);
        pfds.push(libc::pollfd {
            fd: wake,
            events: libc::POLLIN,
            revents: 0,
        });
        pfds.extend(live.iter().map(|&(_, _, fd, _, _)| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }));
        // SAFETY: `pfds` holds one entry per live fence plus the wake fd, all
        // valid for the call; the timeout bounds a fence that never signals.
        // One latch period, not a fixed 50 ms: the slice is how late a note can be
        // after its fence has already signalled, so a constant puts that constant
        // into every producer's park. The panel's own period is the finest useful
        // granularity — a note is worth nothing sooner than the buffer changes.
        let slice_ms = (crate::vsync::period_now_ns() / 1_000_000).clamp(1, 50) as c_int;
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, slice_ms) };
        if pfds[0].revents != 0 {
            drain_eventfd(wake);
        }
        let now = Instant::now();
        // Backwards, so the indices still to visit are unaffected by a removal.
        for i in (0..live.len()).rev() {
            let mut ready = n > 0 && pfds[i + 1].revents != 0;
            let mut timed_out = false;
            if !ready && now.duration_since(live[i].4) >= RELEASE_WATCH_MAX {
                ready = true;
                timed_out = true;
            }
            if ready {
                let (ino, ticket, fd, latch_ns, _) = live.swap_remove(i);
                if timed_out {
                    note_hold_timeout();
                }
                let source = if timed_out {
                    gndrm_uapi::wire::RELEASE_FENCE_TIMEOUT
                } else {
                    gndrm_uapi::wire::RELEASE_FENCE
                };
                report_release(ino, ticket, fd, latch_ns, source);
            }
        }
    }
}

/// Payload the reuse cache stores for one registered scan-out buffer: the
/// daemon's gralloc reference, this registration's completion mailbox, and the
/// row stride to report to the client. All three are copied out; the raw
/// pointers are borrowed for as long as the registration is live.
#[derive(Clone, Copy)]
struct PoolBuf {
    ahb: *mut ffi::AHardwareBuffer,
    ctx: *mut ReleaseCtx,
    stride: u32,
}

/// A buffer taken out of the pool to be released: its `AHardwareBuffer`
/// reference and any descriptor that came with it. Both are dropped outside
/// the pool lock, because a gralloc release has no latency bound.
struct Retired {
    ahb: *mut ffi::AHardwareBuffer,
    /// Descriptor to close alongside the buffer (a stale release fence, or a
    /// spare's client descriptor); `-1` when there is none.
    fd: RawFd,
}

/// A pre-allocated buffer held ready to satisfy the next scan-out request.
///
/// `AHardwareBuffer_allocate` and `AHardwareBuffer_release` are gralloc calls
/// with no latency bound, and retirements arrive in bursts (five were measured
/// inside ~100 ms). The shim answers a request on a 250 ms budget, so a request
/// that had to allocate on the reply path can lose that race outright. Holding
/// a matching spare moves the allocation off the reply path: the request takes
/// the ready buffer, and the worker replaces it afterwards. With the reuse
/// cache covering the steady state this spare is the cover for a genuinely
/// empty cache (the first frames, or a geometry change), not the normal path.
struct Warm {
    width: u32,
    height: u32,
    format: u32,
    /// Client descriptor, already duplicated; a reply hands it over.
    buffer: ScanoutBuffer,
    ahb: *mut ffi::AHardwareBuffer,
}

/// What a scan-out request reserved under the pool lock.
enum Reservation {
    /// A ready spare matching the request: no gralloc work needed.
    Warm(Warm),
    /// A retired buffer matching the request: handed back out with only a
    /// client-descriptor duplicate — no allocation. `ino` is the identity the
    /// free-list entry was registered under, carried out so the handout can
    /// verify the fresh descriptor keeps it and re-register if it moved.
    Reuse { ino: u64, payload: PoolBuf },
    /// Nothing matched; the caller allocates outside the lock. `evicted` is a
    /// free buffer the cache dropped to stay under the cap and the caller must
    /// release.
    Cold { evicted: Option<PoolBuf> },
}

/// The scan-out buffer pool, on its own lock.
///
/// Split from [`AscState`] because the two paths that reach it have very
/// different timing: the allocator runs on a dedicated thread under the shim's
/// 250 ms reply budget, while the present path holds its own lock across a
/// blocking `ASurfaceTransaction_apply` (a SurfaceFlinger round-trip) and the
/// release-fence wait. On a shared lock a compositor stall would queue the
/// allocator behind that round-trip, the shim would time out with `EAGAIN`,
/// and the session would lose a publishable buffer.
///
/// This lock guards bookkeeping only. Every gralloc call
/// (`AHardwareBuffer_allocate`, `AHardwareBuffer_release`,
/// `AHardwareBuffer_getNativeHandle`) and every blocking wait runs with it
/// dropped: a request reserves capacity and resolves handles under the lock,
/// and the slow work happens outside. A burst of retirements therefore cannot
/// deliver a reply late — and with the reuse cache a retirement is a free-list
/// insert, not a release at all.
///
/// The free-list policy (which retired buffer a request may be answered from,
/// and the flip-event rule that gates it) lives in
/// [`gndrm_daemon::scanout::ReusePool`] so it is host-testable.
pub struct ScanoutPool {
    /// dlopen'd publish surface; `None` disables publishing (symbols missing).
    api: Option<ffi::ScApi>,
    /// Live buffers, retired buffers held for reuse, and the cap.
    cache: ReusePool<PoolBuf>,
    /// Buffer held ready for the next request (see [`Warm`]).
    warm: Option<Warm>,
}

// SAFETY: reached only through the `Arc<Mutex<_>>` shared by the present path
// and the scan-out channel thread. The raw pointers it holds are
// SurfaceFlinger objects (thread-safe by contract) and leaked mailboxes that
// are only touched atomically.
unsafe impl Send for ScanoutPool {}

impl ScanoutPool {
    fn new(api: Option<ffi::ScApi>) -> Self {
        Self {
            api,
            cache: ReusePool::new(MAX_SCANOUT_POOL),
            warm: None,
        }
    }

    /// The publish API, copied out so the caller can run gralloc work without
    /// holding the lock. `None` while publishing is disabled.
    fn api(&self) -> Option<ffi::ScApi> {
        self.api
    }

    /// Gralloc allocations the pool owns: handed out, held for reuse, and cold
    /// allocations in flight. The ready spare is deliberately not counted — it
    /// is one allocation held beyond the cap so that taking it never has to
    /// evict a buffer the client still holds.
    fn owned(&self) -> usize {
        self.cache.owned()
    }

    /// Validate a request and reserve its answer — all under the lock, with no
    /// gralloc call.
    ///
    /// A retired buffer of the same geometry is preferred (it costs only a
    /// descriptor duplicate), then the ready spare (the allocation-free
    /// answer), and only then capacity for a cold allocation. A
    /// [`Reservation::Cold`] is finished with [`Self::commit`] (or given back
    /// with [`Self::abort_alloc`]) once the allocation has run outside the
    /// lock; the others are finished with [`Self::commit`] too.
    ///
    /// # Errors
    /// `EINVAL` for a geometry or format the pool does not serve, `ENOSYS`
    /// when publishing is disabled (there is no point owning a buffer nothing
    /// can publish), and `ENOSPC` when the cap is reached.
    fn begin_alloc(&mut self, width: u32, height: u32, format: u32) -> io::Result<Reservation> {
        check_request_format(format)?;
        if width == 0 || height == 0 || width > MAX_SCANOUT_DIM || height > MAX_SCANOUT_DIM {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        if self.api.is_none() {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }
        let geom = Geometry {
            width,
            height,
            format,
        };
        // The steady-state answer: a retired buffer of this geometry, handed
        // back out with no allocation at all.
        if let Some((ino, payload)) = self.cache.take_reusable(geom) {
            return Ok(Reservation::Reuse { ino, payload });
        }
        // Then the pre-allocated spare, so the request does not allocate
        // synchronously (the worker rebuilds the spare afterwards).
        let warm_matches = self
            .warm
            .as_ref()
            .is_some_and(|w| w.width == width && w.height == height && w.format == format);
        if warm_matches {
            if let Some(warm) = self.warm.take() {
                return Ok(Reservation::Warm(warm));
            }
        }
        // Otherwise reserve capacity for a cold allocation. `reserve` evicts
        // the longest-free buffer when the cap is reached, so a geometry
        // change does not wedge the pool.
        match self.cache.reserve() {
            CacheReservation::Cold { evicted } => Ok(Reservation::Cold { evicted }),
            CacheReservation::Reuse { ino, payload } => Ok(Reservation::Reuse { ino, payload }),
            CacheReservation::Full => Err(io::Error::from_raw_os_error(libc::ENOSPC)),
        }
    }

    /// Give back the capacity reserved by a [`Self::begin_alloc`] whose gralloc
    /// work failed.
    fn abort_alloc(&mut self) {
        self.cache.abort_cold();
    }

    /// Reserve capacity for one cold allocation, for a request whose first
    /// answer (a reused buffer or the spare) could not satisfy it. Same policy
    /// as the tail of [`Self::begin_alloc`]: evict the longest-free buffer at
    /// the cap, `ENOSPC` only when nothing free can be evicted.
    fn begin_cold(&mut self) -> io::Result<Reservation> {
        match self.cache.reserve() {
            CacheReservation::Cold { evicted } => Ok(Reservation::Cold { evicted }),
            CacheReservation::Reuse { ino, payload } => Ok(Reservation::Reuse { ino, payload }),
            CacheReservation::Full => Err(io::Error::from_raw_os_error(libc::ENOSPC)),
        }
    }

    /// Put the spare back after it could not satisfy a request, so it is not
    /// lost when the cold fall-through runs in its place.
    fn put_warm(&mut self, warm: Warm) {
        self.warm.replace(warm);
    }

    /// Resolve a length-only request to a geometry the pool knows, then reserve
    /// its answer. `EINVAL` when the length resolves to no known geometry (not
    /// a scan-out allocation: the shim leaves it to the kernel), and whatever
    /// [`Self::begin_alloc`] returns otherwise.
    ///
    /// # Errors
    /// `EINVAL` for an unknown length or a format the pool does not serve,
    /// `ENOSYS`/`ENOSPC` as [`Self::begin_alloc`].
    fn begin_alloc_len(&mut self, len: u64, format: u32) -> io::Result<(u32, u32, Reservation)> {
        check_request_format(format)?;
        let Some(geom) = self.cache.resolve_len(len, format) else {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        };
        let reservation = self.begin_alloc(geom.width, geom.height, format)?;
        Ok((geom.width, geom.height, reservation))
    }

    /// Record a framebuffer's geometry, so a length-only request for the same
    /// window size resolves to it. `alloc_len` is the backing GEM's real
    /// allocation length (`st_size`): row-padded and page-rounded, which is the
    /// length the shim's `DMA_HEAP_IOCTL_ALLOC` request actually carries. Fed
    /// from the daemon's `ADDFB2` path, which reaches this for every
    /// registered framebuffer — including those never published as the primary
    /// plane — and earlier than any commit.
    pub(crate) fn note_framebuffer(&mut self, width: u32, height: u32, alloc_len: u64) {
        self.cache.note_geometry(
            Geometry {
                width,
                height,
                format: session_format(),
            },
            alloc_len,
        );
    }

    /// Put a reused buffer back after its client descriptor could not be
    /// duplicated, so the pool does not lose the buffer. `ino` is the identity
    /// the free-list entry was registered under.
    fn recycle(&mut self, geom: Geometry, ino: u64, payload: PoolBuf) {
        self.cache.recycle(geom, ino, payload);
    }

    /// A fresh completion mailbox for one registration, with the reuse gate
    /// the completion callback publishes into.
    fn new_ctx(&self, ino: u64) -> (*mut ReleaseCtx, Arc<AtomicBool>) {
        let api = self.api.expect(&"a slot is only committed when the presenter is enabled");
        new_release_ctx(&api, ino)
    }

    /// Register a handed-out buffer as `ino`'s slot, logging how it was
    /// obtained (`origin` is `"allocated"`, `"reused"` or `"from spare"`).
    /// `cold` releases the reservation [`Self::begin_alloc`] took. The map work
    /// is all that runs here; a replaced slot is returned for release outside
    /// the lock (a live slot with the same inode cannot exist, but a collision
    /// must not leak the old reference). The single per-hand-out line names
    /// the origin and the pool count, so the steady state reads as reuse with
    /// a settled pool and no "allocated" lines at all.
    fn commit(
        &mut self,
        width: u32,
        height: u32,
        buffer: &ScanoutBuffer,
        ahb: *mut ffi::AHardwareBuffer,
        cold: bool,
        origin: &str,
    ) -> Option<PoolBuf> {
        let geom = Geometry {
            width,
            height,
            format: session_format(),
        };
        let (ctx, latched) = self.new_ctx(buffer.ino);
        let replaced = self.cache.commit(
            buffer.ino,
            geom,
            PoolBuf {
                ahb,
                ctx,
                stride: buffer.stride,
            },
            latched,
            cold,
        );
        // Register the buffer's real allocation length too, so a length-only
        // scan-out request for a compressed (UBWC) buffer — whose length is
        // longer than the tight `width * height * 4` the cache notes itself —
        // resolves to this geometry. Idempotent: the length index keeps the
        // first geometry noted for a (length, format) pair.
        self.cache.note_geometry(geom, buffer.size);
        log_info!("{}", format_args!("asc: scan-out {width}x{height} {origin} (ino={}, stride={}, pool={}/{MAX_SCANOUT_POOL})",
            buffer.ino,
            buffer.stride,
            self.owned()));
        replaced
    }

    /// Take `ino` back out of the pool (a reply that could not be delivered):
    /// its capacity is freed and it is kept for reuse or handed back for
    /// release.
    fn take(&mut self, ino: u64) -> Option<Removed<PoolBuf>> {
        self.cache.take(ino)
    }

    /// Buffers whose GEM object is gone, taken out of the pool under the lock.
    /// A buffer whose flip event has been reported is kept in the reuse cache;
    /// one that has not is returned for release. `live_inos` and `keep` are
    /// exactly as they were: only a buffer the client stopped importing (and
    /// that the layer does not still fall back to) is considered.
    fn retire(&mut self, live_inos: &HashSet<u64>, keep: Option<u64>) -> Vec<Removed<PoolBuf>> {
        self.cache.retire(live_inos, keep)
    }

    /// Put a freshly built spare in the pool, returning the one it replaces
    /// (released outside the lock). The spare's real allocation length is
    /// registered first, so a length-only request for a compressed buffer
    /// resolves even before the spare is first handed out.
    fn install_warm(&mut self, warm: Warm) -> Option<Warm> {
        self.cache.note_geometry(
            Geometry {
                width: warm.width,
                height: warm.height,
                format: warm.format,
            },
            warm.buffer.size,
        );
        self.warm.replace(warm)
    }

    /// Buffers the pool owns (handed out, held for reuse, being allocated).
    #[must_use]
    pub fn pool_owned(&self) -> usize {
        self.cache.owned()
    }

    /// Buffers currently handed out (registered with the presenter).
    #[must_use]
    pub fn pool_live(&self) -> usize {
        self.cache.live_len()
    }

    /// Retired buffers held in the reuse cache.
    #[must_use]
    pub fn pool_free(&self) -> usize {
        self.cache.free_len()
    }

    /// Take every buffer the pool owns, for teardown. The caller releases the
    /// `AHardwareBuffer`s outside the lock.
    fn take_all(&mut self) -> Vec<Retired> {
        let mut out: Vec<Retired> = self
            .cache
            .drain()
            .into_iter()
            .map(|p| Retired {
                ahb: p.ahb,
                fd: ctx_release_fd(&p),
            })
            .collect();
        if let Some(warm) = self.warm.take() {
            out.push(Retired {
                ahb: warm.ahb,
                fd: warm.buffer.fd,
            });
        }
        out
    }

    /// Resolve `ino` to its registered buffer and mark it live. `None` when the
    /// inode was never registered (the commit must fall back to the last
    /// published buffer), and the miss is counted.
    fn resolve(&mut self, ino: u64) -> Option<PoolBuf> {
        self.cache.resolve(ino)
    }

    /// Commits this session that resolved to no registration (the presenter
    /// had to fall back), i.e. buffers the presenter never handed out.
    #[must_use]
    pub fn pool_misses(&self) -> u64 {
        self.cache.misses()
    }

    /// Resolve `ino` to its registered buffer without marking it live.
    fn get(&self, ino: u64) -> Option<PoolBuf> {
        self.cache.get(ino)
    }
}

/// The release-fence descriptor a registration's mailbox still holds, taken so
/// it is closed exactly once (`-1` when none). The mailbox itself is leaked
/// (see [`ReleaseCtx`]).
fn ctx_release_fd(buf: &PoolBuf) -> RawFd {
    // SAFETY: `buf.ctx` is a leaked mailbox owned by the pool for the session.
    unsafe { (*buf.ctx).release_fd.swap(-1, Ordering::AcqRel) }
}

/// A fresh completion mailbox for one registration, with the reuse gate the
/// completion callback publishes into. Leaked (see [`ReleaseCtx`]).
fn new_release_ctx(api: &ffi::ScApi, ino: u64) -> (*mut ReleaseCtx, Arc<AtomicBool>) {
    let latched = Arc::new(AtomicBool::new(false));
    let ctx = Box::into_raw(Box::new(ReleaseCtx {
        stats_get_controls: api.st_stats_get_controls,
        stats_release_controls: api.st_stats_release_controls,
        previous_release: api.st_stats_previous_release_fence_fd,
        release_fd: AtomicI32::new(-1),
        latch_time: api.st_stats_latch_time,
        latch_ns: AtomicU64::new(0),
        ticket: AtomicU64::new(0),
        release_ticket: AtomicU64::new(0),
        applied_ns: AtomicU64::new(0),
        present_id: AtomicU64::new(0),
        latched: Arc::clone(&latched),
        ino,
        replaced_ctx: AtomicPtr::new(ptr::null_mut()),
    }));
    (ctx, latched)
}

/// A committed dma-buf the pool does not own, and the copy target its frames
/// are published through. `copy` is `None` when the copy is not possible: the
/// failure was logged and the inode's frames are dropped as before.
struct ForeignSlot {
    layout: Layout,
    copy: Option<ForeignCopy>,
    ctx: *mut ReleaseCtx,
}

impl ForeignSlot {
    fn buf(&self) -> Option<PoolBuf> {
        self.copy.as_ref().map(|c| PoolBuf {
            ahb: c.ahb,
            ctx: self.ctx,
            stride: c.stride,
        })
    }
}

impl Drop for ForeignSlot {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            // SAFETY: `ctx` is this slot's leaked mailbox.
            let fd = unsafe { (*self.ctx).release_fd.swap(-1, Ordering::AcqRel) };
            if fd >= 0 {
                // SAFETY: the mailbox owned this release fence.
                unsafe { libc::close(fd) };
            }
        }
    }
}

/// The cursor image on the cursor layer: a daemon-owned copy of the cursor
/// framebuffer in the session's scan-out layout (`ForeignCopy` follows the
/// layout rule, so a GLES sdk>=36 session gets an RGBA layer here too).
struct CursorSlot {
    ino: u64,
    layout: Layout,
    fb_id: u32,
    copy: ForeignCopy,
}

/// The cursor layer: a sibling of the scan-out layer under the app's window,
/// above it, placed in window pixels by the same fit as the scan-out layer.
struct CursorLayer {
    sc: *mut ffi::ASurfaceControl,
    slot: Option<CursorSlot>,
    /// The last update applied, so a rebuilt layer can be given it back.
    last: Option<CursorUpdate>,
    visible: bool,
    images: u64,
    moves: u64,
    hides: u64,
    failures: u64,
    logged: u32,
    /// The staged image on the layer, and what it was built from.
    stage: Option<(StageKey, CursorStage)>,
    /// The current image's pixels (`images`, pixels, width, format) and its
    /// opaque extent for the last source rect, so a motion costs no read.
    pixels: Option<(u64, Vec<u8>, u32, u32)>,
    extent: Option<((u64, gndrm_daemon::scanout::Edges), (u32, u32))>,
    /// How the layer was last shown: staged, and with which rotation (`None`:
    /// the padded buffer as KMS describes it).
    shown_as: Option<Option<u32>>,
    stage_logged: u32,
}

impl CursorLayer {
    fn new(sc: *mut ffi::ASurfaceControl) -> Self {
        Self {
            sc,
            slot: None,
            last: None,
            visible: false,
            stage: None,
            pixels: None,
            extent: None,
            shown_as: None,
            stage_logged: 0,
            images: 0,
            moves: 0,
            hides: 0,
            failures: 0,
            logged: 0,
        }
    }
}

/// The layer-rect probe's change key: the published source and destination
/// rects plus the mode and window the destination is bounded by. A plain tuple,
/// so the probe has a `Copy + PartialEq` value to compare against the last one
/// and prints on a change instead of every frame.
type RectLogKey = (
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
);

/// The last buffer actually put on the layer, with the geometry it went out
/// with. A commit for an inode that is not registered re-publishes this rather
/// than leaving the layer empty: SurfaceFlinger compositing nothing drops the
/// app's own background through, which reads as a blank panel.
#[derive(Clone, Copy)]
struct LastPublish {
    ino: u64,
    src: ffi::ARect,
    dst: ffi::ARect,
    /// Dataspace the buffer was published with, so a republish (a rebuilt
    /// layer) restores the same HDR/SDR treatment rather than guessing.
    dataspace: c_int,
}

/// The presenter's state, shared between the server thread that presents and
/// the scan-out channel thread that allocates buffers.
pub struct AscState {
    /// dlopen'd publish surface; `None` disables publishing (symbols missing).
    api: Option<ffi::ScApi>,
    /// Child layer of the app's window; null while publishing is disabled.
    sc: *mut ffi::ASurfaceControl,
    /// Attached lazily on the thread that publishes — the daemon's commit
    /// worker — so the looper always belongs to the thread that pumps it. See
    /// [`AscState::publish`] for why that matters and why the SurfaceFlinger
    /// completion callback does not depend on it.
    looper: Option<ffi::LooperApi>,
    /// The session's mode: the scan-out size the connector advertises and the
    /// pool's buffers are allocated at.
    mode_w: c_int,
    mode_h: c_int,
    /// The app's window — the display's own pixels — which the layer is
    /// composited in. Equal to the mode unless the app asked for a smaller
    /// scan-out size, in which case the window's compositor does the upscale.
    win_w: c_int,
    win_h: c_int,
    /// The scan-out buffer pool, on its own lock (see [`ScanoutPool`]). The
    /// lock is tracked (see `gndrm_daemon::diag`): a request that stalls on it
    /// must be able to name the thread holding it.
    pool: Arc<TrackedMutex<ScanoutPool>>,
    last_publish: Option<LastPublish>,
    /// The mailbox of the buffer currently on the layer, or null before the
    /// first show. Written for the next publish to record what it replaced, so
    /// a completion's release fence can be filed against the right buffer.
    last_shown_ctx: *mut ReleaseCtx,
    frames: u64,
    warned_fence_timeout: bool,
    /// Inodes already reported as having no registered buffer (log once each).
    logged_unregistered: Vec<u64>,
    /// Inodes already reported as re-publishing a stale buffer (log once each).
    logged_stale: Vec<u64>,
    /// Last `(src, dst, panel)` tuple the layer-rect probe printed, so the probe
    /// prints on a geometry change rather than every frame.
    last_rect_log: Option<RectLogKey>,
    /// Last `(fourcc, buffer_is_ten_bit, connector_hdr)` triple the HDR probe
    /// printed, so it prints when the decision or either of its inputs changes
    /// rather than every frame — and every [`PUBLISH_LOG_EVERY`]-th frame
    /// regardless, so a capture that starts after the transition still sees it.
    last_hdr_log: Option<(u32, bool, bool)>,
    /// Whether the layer is currently hidden for a non-drawable plane, so the
    /// hide/show probe prints on the transition instead of every frame.
    hidden: bool,
    /// Frames the panel dump has considered, and the geometry it last dumped
    /// (see [`AscState::dump_published`]).
    dump_n: u64,
    dump_geom: Option<(u32, u32)>,
    /// Committed dma-bufs the pool does not own, by inode, with the buffer
    /// their frames are copied into (see [`AscState::foreign_target`]).
    foreign: HashMap<u64, ForeignSlot>,
    /// Always-on foreign-buffer lines printed (bounded by
    /// [`FOREIGN_LOG_BUDGET`]).
    foreign_logged: u32,
    /// Session totals for the teardown summary.
    foreign_frames: u64,
    foreign_buffers: u64,
    foreign_failures: u64,
    /// The cursor layer (see [`AscState::publish_cursor`]).
    cursor: CursorLayer,
}

// SAFETY: the state is only ever reached through the `Arc<Mutex<_>>` shared by
// the present path and the scan-out channel thread. The raw pointers it holds
// are SurfaceFlinger objects (thread-safe by contract) and the native window is
// not kept — it is used once, on the thread that owns it, to create the child
// layer.
unsafe impl Send for AscState {}

impl AscState {
    /// Create the child layer under `window`, presenting the session's `mode_w`x`mode_h`
    /// scan-out into the `win_w`x`win_h` window. Never fails: a missing publish
    /// surface or a null layer leaves the presenter disabled, which drops frames
    /// instead of failing the session.
    #[must_use]
    pub fn new(
        window: *mut ffi::ANativeWindow,
        mode_w: i32,
        mode_h: i32,
        win_w: i32,
        win_h: i32,
    ) -> Self {
        let loaded = ffi::ScApi::load();
        // SAFETY: `window` is a live ANativeWindow owned by the caller for the
        // whole session; createFromWindow only reads it.
        let sc = loaded.map_or(ptr::null_mut(), |api| unsafe {
            (api.sc_create_from_window)(window, SC_DEBUG_NAME.as_ptr())
        });
        let api = if sc.is_null() { None } else { loaded };
        // The cursor layer: created with the scan-out layer from the same
        // window, empty (it composites nothing) until a client driving the
        // cursor plane shows a cursor on it.
        let cursor_sc = api.map_or(ptr::null_mut(), |api| unsafe {
            // SAFETY: as above.
            (api.sc_create_from_window)(window, SC_CURSOR_DEBUG_NAME.as_ptr())
        });
        if loaded.is_none() {
            log_warn!(
                "asc: SurfaceControl API unavailable (pre-API-29?); publishing disabled"
            );
        } else if sc.is_null() {
            log_warn!(
                "asc: ASurfaceControl_createFromWindow failed; publishing disabled"
            );
        } else {
            log_info!(
                "{}",
                format_args!("asc: SurfaceControl layer created (mode {mode_w}x{mode_h} in a {win_w}x{win_h} window)")
            );
        }
        // The `ALooper` is deliberately *not* prepared here. This function runs
        // on the serve thread, but `ALooper_pollOnce` must be called from the
        // thread the looper was prepared on, and the polling happens in
        // `publish` on the daemon's commit worker. Preparing it here was the
        // on-device bug: the worker's poll then failed with
        // "ALooper_pollOnce: No looper for this thread!". `publish` attaches it
        // lazily on the publishing thread instead.
        let mut state = Self {
            api,
            sc,
            looper: None,
            mode_w,
            mode_h,
            win_w,
            win_h,
            pool: Arc::new(TrackedMutex::new(
                diag::diag(),
                LOCK_SCANOUT_POOL,
                ScanoutPool::new(api),
            )),
            last_publish: None,
            last_shown_ctx: ptr::null_mut(),
            frames: 0,
            warned_fence_timeout: false,
            logged_unregistered: Vec::new(),
            logged_stale: Vec::new(),
            last_rect_log: None,
            last_hdr_log: None,
            hidden: false,
            dump_n: 0,
            dump_geom: None,
            foreign: HashMap::new(),
            foreign_logged: 0,
            foreign_frames: 0,
            foreign_buffers: 0,
            foreign_failures: 0,
            cursor: CursorLayer::new(cursor_sc),
        };
        // A layer is created empty, and an empty layer makes SurfaceFlinger
        // composite nothing. Everything this session knows lives in the
        // allocator pool, which is empty at this point, so there is normally
        // nothing to put up yet — re-publishing is a no-op here and only does
        // work on a path that re-creates the layer over an existing table.
        // The mode is a geometry the daemon can serve from a bare length, so a
        // mode-sized scan-out allocation resolves by length even before a
        // framebuffer of that size is committed.
        if mode_w > 0 && mode_h > 0 {
            state.pool.lock(Role::Commit).note_framebuffer(
                mode_w as u32,
                mode_h as u32,
                mode_w as u64 * mode_h as u64 * 4,
            );
        }
        state.republish_last();
        state
    }

    /// The allocator's handle on the buffer pool. Cloned once at bring-up so
    /// the scan-out thread never touches the publish state's lock.
    #[must_use]
    pub fn pool(&self) -> Arc<TrackedMutex<ScanoutPool>> {
        Arc::clone(&self.pool)
    }

    /// Real frames published to the layer (observability).
    #[must_use]
    pub fn frames_presented(&self) -> u64 {
        self.frames
    }

    /// Publish the committed primary-plane framebuffer to the layer. The
    /// presenter does not hand the completion back to its caller: the commit
    /// worker never waits on it, and the buffer is released by the compositor's
    /// own release signal (the release fence the next publish waits on).
    ///
    /// `ino` is the buffer identity the device resolved from the object store;
    /// `live_inos` is the set of still-imported identities the pool retire keys
    /// on. `src_fd` is that buffer's dma-buf (borrowed), read when the pool does
    /// not own it.
    pub fn publish(
        &mut self,
        fb: &Framebuffer,
        plane: &PlaneState,
        ino: Option<u64>,
        src_fd: Option<RawFd>,
        live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    ) {
        // UNGATED one-shot at the presenter's entry. The daemon's own commit
        // probe says whether a commit arrived with a publishable framebuffer;
        // this says whether the presenter was handed it, and names the inputs
        // its early exits test — so a run that shows commits arriving and still
        // no published frame points at one exit instead of at the whole path.
        // Latched: one relaxed swap per publish and nothing formats after the
        // first call.
        {
            static PUBLISH_SEEN: AtomicBool = AtomicBool::new(false);
            if !PUBLISH_SEEN.swap(true, Ordering::Relaxed) {
                asc_log!(
                    "{}",
                    format_args!("gndrm-daemon: publish entry fb={} ino_set={} api_set={} sc_set={}",
                        fb.id,
                        ino.is_some(),
                        self.api.is_some(),
                        !self.sc.is_null())
                );
            }
        }
        let Some(api) = self.api else {
            close_fence(fence);
            return;
        };
        if self.sc.is_null() {
            close_fence(fence);
            return;
        }
        let Some(ino) = ino else {
            close_fence(fence);
            return;
        };

        // The FB's SRC rect (16.16 fixed point) is the crop inside the buffer,
        // which the mode's coordinates then scale into the window.
        let src = ffi::ARect {
            left: (plane.src_x >> 16) as c_int,
            top: (plane.src_y >> 16) as c_int,
            right: ((plane.src_x + plane.src_w) >> 16) as c_int,
            bottom: ((plane.src_y + plane.src_h) >> 16) as c_int,
        };
        // KMS clips the plane to the mode, and the layer is composited in the
        // window: the CRTC rect (mode pixels) is fitted into the window, so a
        // scan-out size below the display's is upscaled by the display's own
        // compositor rather than by gamescope's composite, which stops at the
        // mode. The output is black outside the rect, and the layer is sized to
        // it only, so whatever lies outside is the app's window showing through;
        // the layer's background covers transparent regions inside it
        // (gamescope letterboxes with transparent bars).
        let (dst_left, dst_top, dst_right, dst_bottom) = gndrm_daemon::scanout::layer_rect(
            (plane.crtc_x, plane.crtc_y, plane.crtc_w, plane.crtc_h),
            (self.mode_w as u32, self.mode_h as u32),
            (self.win_w as u32, self.win_h as u32),
        );
        let dst = ffi::ARect {
            left: dst_left,
            top: dst_top,
            right: dst_right,
            bottom: dst_bottom,
        };
        let drawable = src.right > src.left
            && src.bottom > src.top
            && dst.right > dst.left
            && dst.bottom > dst.top;
        let keep = self.last_publish.map(|lp| lp.ino);

        // UNGATED, change-latched: the rect this commit publishes the layer
        // with, against the window it is composited in. The layer is a child of
        // the app's window and is sized to the CRTC rect only, so whatever lies
        // inside the window but outside that rect is the app's own window
        // showing through — the bands this probe exists to measure. Prints on a
        // rect, mode or window change and once at session start, never per
        // frame.
        let key: RectLogKey = (
            src.left,
            src.top,
            src.right,
            src.bottom,
            dst.left,
            dst.top,
            dst.right,
            dst.bottom,
            self.mode_w,
            self.mode_h,
            self.win_w,
            self.win_h,
        );
        if self.last_rect_log != Some(key) {
            self.last_rect_log = Some(key);
            asc_log!(
                "{}",
                format_args!("gndrm-daemon: asc layer rect src=({},{})-({},{}) dst=({},{})-({},{}) mode={}x{} window={}x{} band left={} top={} right={} bottom={} drawable={}",key.0,
                key.1,
                key.2,
                key.3,
                key.4,
                key.5,
                key.6,
                key.7,
                key.8,
                key.9,
                key.10,
                key.11,
                dst.left,
                dst.top,
                self.win_w - dst.right,
                self.win_h - dst.bottom,
                drawable,)
            );
        }

        // The pool lock is held only for map work: retiring the buffers the
        // client stopped importing (into the reuse cache) and resolving the
        // payload this commit publishes. Every gralloc call, the looper/fence
        // service and the transaction build/apply run after it is dropped, so a
        // commit — including a retirement burst — can never queue a scan-out
        // reply behind slow work.
        let (pooled, stale, removed, misses) = {
            let mut pool = self.pool.lock(Role::Commit);
            let removed = pool.retire(live_inos, keep);
            // Resolve the buffer this commit publishes. Both lookups copy out
            // the payload, so neither borrow outlives its statement and the
            // fallback path can consult the cache again.
            let pooled = pool.resolve(ino);
            let stale = if pooled.is_none() {
                self.last_publish
                    .and_then(|lp| pool.get(lp.ino).map(|buf| (buf, lp.ino, true)))
            } else {
                None
            };
            (pooled, stale, removed, pool.pool_misses())
        };

        // Retain the retired buffers whose flip event has been reported for
        // reuse, and release the rest: a gralloc call with no latency bound,
        // explicitly outside the pool lock.
        let released = retire_to_release(&removed);
        release_retired(&released);
        self.retire_foreign(live_inos, keep);

        // A buffer the pool does not own is copied into one the presenter
        // does, rather than dropped. Only a pool miss reaches this, so the
        // zero-copy path is unchanged.
        let mut copy_foreign = false;
        let resolved = match pooled {
            Some(buf) => Some((buf, ino, false)),
            None => match self.foreign_target(ino, fb, src_fd) {
                Some(buf) => {
                    copy_foreign = true;
                    Some((buf, ino, false))
                }
                None => stale.or_else(|| {
                    self.last_publish.and_then(|lp| {
                        self.foreign
                            .get(&lp.ino)
                            .and_then(ForeignSlot::buf)
                            .map(|buf| (buf, lp.ino, true))
                    })
                }),
            },
        };

        match resolved {
            None => {
                // No buffer for this inode. Either the shim passed the
                // allocation through to the kernel, or the table was rebuilt
                // under a still-running client. There is no last buffer to fall
                // back to, so the frame is dropped and the fence released.
                // UNGATED: the dropped frame is the other half of the stale
                // path — a commit arrived that the presenter could not put
                // anything on the panel for. Once per inode.
                if !self.logged_unregistered.contains(&ino) {
                    self.logged_unregistered.push(ino);
                    asc_log!(
                        "{}",
                        format_args!("gndrm-daemon: asc DROPPED frame: no AHardwareBuffer for dma-buf ino {ino} (misses={misses})")
                    );
                    asc_log!(
                        "{}",
                        format_args!("gndrm-daemon: asc dropped framebuffer: {}x{} fmt={:#x} pitch={} (ino {ino})",fb.width,
                        fb.height,
                        fb.pixel_format,
                        fb.pitches[0])
                    );
                }
                close_fence(fence);
            }
            Some((buf, published_ino, stale)) => {
                let PoolBuf {
                    ahb, ctx, stride, ..
                } = buf;
                // The frame this registration is about to carry: its present id
                // was marked against the buffer before the commit that named it.
                // SAFETY: `ctx` is the leaked mailbox of this registered buffer.
                let (present_id, ticket) = take_present_mark(published_ino);
                unsafe {
                    (*ctx).present_id.store(present_id, Ordering::Release);
                    (*ctx).ticket.store(ticket, Ordering::Release);
                }
                // Attach a looper to *this* thread the first time we publish.
                // `ALooper_pollOnce` must run on the thread the looper was
                // prepared on: polling a looper prepared elsewhere fails with
                // "ALooper_pollOnce: No looper for this thread!" (the on-device
                // bug — `new()` prepared it on the serve thread while `publish`
                // runs on the daemon's commit worker). `publish` is always
                // called on that one worker, so the looper is prepared here,
                // lazily.
                //
                // The SurfaceFlinger completion callback does *not* depend on
                // this looper: libgui delivers it on its own binder thread
                // (`TransactionCompletedListener::onTransactionCompleted`) and
                // it stores the release fence across threads. The looper only
                // drains anything SurfaceFlinger queued to this thread.
                if self.looper.is_none() {
                    if let Some(looper) = self.api.and_then(|api| api.looper) {
                        // SAFETY: attaches (or returns the existing) looper for
                        // the calling thread; called with no special options.
                        if !unsafe { (looper.prepare)(0) }.is_null() {
                            self.looper = Some(looper);
                        }
                    }
                }
                // Service the looper, so a completion that has already happened
                // is reflected in the mailbox before it is read. A zero-timeout
                // poll dispatches only queued work. The mailbox is atomic, so
                // this needs no pool lock.
                if let Some(looper) = self.looper {
                    // SAFETY: the looper was prepared on this thread (above); no
                    // fd/event out-parameters are asked for.
                    unsafe {
                        (looper.poll_once)(0, ptr::null_mut(), ptr::null_mut(), ptr::null_mut())
                    };
                }
                // SAFETY: `ctx` points at the leaked mailbox of this slot.
                let pending = unsafe { (*ctx).release_fd.swap(-1, Ordering::AcqRel) };
                if pending >= 0 {
                    // This buffer's own release fence, filed by the transaction
                    // that replaced it: nothing may reuse the buffer until it
                    // signals. The wait is a guard, not a measurement — the hold
                    // is sampled by the watcher, where the fence actually ends
                    // (see `report_release`).
                    let timed_out = wait_fence(pending);
                    if timed_out && !self.warned_fence_timeout {
                        self.warned_fence_timeout = true;
                        log_warn!("{}", "asc: previous release fence timed out; reusing buffer anyway (logged once)");
                    }
                    // SAFETY: `pending` is a valid descriptor owned by this call.
                    unsafe { libc::close(pending) };
                }
                let mut fence = fence;
                let copy_failed =
                    copy_foreign && drawable && !self.copy_foreign(ino, fb, &mut fence);
                if copy_failed {
                    asc_log!(
                        "{}",
                        format_args!("gndrm-daemon: asc DROPPED frame: foreign dma-buf ino {ino} could not be copied (misses={misses})")
                    );
                    close_fence(fence);
                } else if drawable {
                    // No buffer for this inode: re-publish the last buffer rather
                    // than leave the layer empty (SurfaceFlinger compositing
                    // nothing shows the app's background through as a blank
                    // panel). UNGATED, because this is the "the old frame is
                    // still there" path: a committed framebuffer that could not
                    // be resolved re-publishes the previous one, so what the
                    // panel shows is a frame the game already replaced. Once per
                    // inode (the same latch the gated line uses), so a burst
                    // prints one line rather than one per frame.
                    if stale && !self.logged_stale.contains(&ino) {
                        self.logged_stale.push(ino);
                        asc_log!(
                            "{}",
                            format_args!("gndrm-daemon: asc STALE re-publish: no AHardwareBuffer for dma-buf ino {ino}; re-publishing ino {published_ino} (misses={misses})")
                        );
                        asc_log!(
                            "{}",
                            format_args!("gndrm-daemon: asc stale framebuffer: {}x{} fmt={:#x} pitch={} (ino {ino})",fb.width,
                            fb.height,
                            fb.pixel_format,
                            fb.pitches[0])
                        );
                    }
                    // Format and content are two questions; the published
                    // dataspace answers the first under the existing rule.
                    //
                    // Format — is the buffer on the layer 10-bit? Read from the
                    // committed framebuffer's own fourcc: the pool serves the layout
                    // the client asked for, so a 10-bit scan-out is a 10-bit buffer
                    // by construction. This is a fact about the pixels' layout, not
                    // their transfer function, and it is what the dataspace is
                    // keyed on — together with the content encoding below.
                    //
                    // Content — are those pixels PQ-encoded in fact? The committed
                    // fourcc cannot answer that: on the forced-composition path
                    // (FSR/SGSR) the committed framebuffer is the composite output
                    // image (`XBGR2101010`, `g_nDRMFormat`) whatever the frame's
                    // content. So on that path the two questions can disagree, and
                    // the register that would tell them apart is the connector's
                    // HDR-output state that the daemon already serves.
                    //
                    // AWAITS DEVICE CONFIRMATION: whether the connector state
                    // discriminates the FSR/SGSR flat-field failure is read, not
                    // measured. The deciding observation is this line's own
                    // `conn_hdr=` in an HDR-on FSR/SGSR arm beside a working one —
                    // until then the dataspace keeps its existing rule and only the
                    // log is added, so a working HDR case cannot be altered.
                    let hdr_buffer = ffi::is_ten_bit_drm_format(fb.pixel_format)
                        || served_ahb_format(session_format())
                            == ffi::AHARDWAREBUFFER_FORMAT_R10G10B10A2_UNORM;
                    let hdr_content = gndrm_daemon::hdr_output_active();
                    // The dataspace describes the encoding the pixels are in,
                    // which is the *output* encoding gamescope chose — not the
                    // buffer's bit depth. A 10-bit buffer carrying a frame
                    // gamescope tone-mapped to Gamma 22 is SDR data, and tagging
                    // it PQ makes the compositor model-map it a second time.
                    // `hdr_content` is gamescope's own connector state (the
                    // register its HDR commit sets), so the tag follows it.
                    let dataspace = if hdr_buffer && hdr_content {
                        ffi::ADATASPACE_BT2020_PQ
                    } else {
                        ffi::ADATASPACE_UNKNOWN
                    };
                    // Opt-in (`GNDRM_LOG_DAEMON`, set by the launcher in this
                    // process): the per-frame HDR decision and both inputs to it,
                    // printed on a change of any of the three and every
                    // [`PUBLISH_LOG_EVERY`]-th frame while it is on. `n` and `ino`
                    // pair with the commit probe's `commit #N fb=`: publishes per
                    // second against commits per second, and whether the buffer
                    // actually published is still moving. A publish index that
                    // advances while `ino` repeats is the retained-buffer
                    // re-publish (the composite output stopped arriving).
                    let hdr_key = (fb.pixel_format, hdr_buffer, hdr_content);
                    let publish_n = self.frames + 1;
                    if gndrm_daemon::verbose_logging()
                        && (self.last_hdr_log != Some(hdr_key)
                            || publish_n.is_multiple_of(PUBLISH_LOG_EVERY))
                    {
                        self.last_hdr_log = Some(hdr_key);
                        // The alpha field and two reference words, sampled only
                        // where this line prints. A publish index and an inode
                        // sequence advance whether or not the pixels differ, and
                        // whether a compositor can show the frame at all is a
                        // separate question the alpha field answers.
                        let sample = match Self::content_sample(ahb, stride, fb.height) {
                            Some((w0, w1)) => {
                                format!("a={} c={w0:#010x} p={w1:#010x}", w0 >> 30)
                            }
                            None => "a=none".to_string(),
                        };
                        asc_log!(
                            "{}",
                            format_args!("gndrm-daemon: asc hdr decision n={} fb={} ino={} {} fmt={:#x} buffer10={} conn_hdr={} dataspace={:#x}",publish_n,
                            fb.id,
                            published_ino,
                            sample,
                            fb.pixel_format,
                            hdr_buffer,
                            hdr_content,
                            dataspace)
                        );
                    }
                    let rect = ShowRect {
                        src,
                        dst,
                        dataspace,
                    };
                    // UNGATED, change-latched: the layer has come back from a
                    // hide. A hidden layer composites nothing (the app's window
                    // shows through), so this is the moment the panel stops
                    // being the app's own background and starts being the
                    // session's frame again — or, at session end, the other way
                    // round.
                    if self.hidden {
                        self.hidden = false;
                        asc_log!(
                            "{}",
                            format_args!("gndrm-daemon: asc layer shown again fb={} dst=({},{})-({},{}) mode={}x{} window={}x{}",fb.id,
                            dst.left,
                            dst.top,
                            dst.right,
                            dst.bottom,
                            self.mode_w,
                            self.mode_h,
                            self.win_w,
                            self.win_h)
                        );
                    }
                    // What this show replaces on the layer: the completion of
                    // this transaction reports the *replaced* buffer's release
                    // fence, so the mailbox that owns it has to be named before
                    // the callback can run.
                    // SAFETY: `ctx` is the leaked mailbox of this slot.
                    unsafe {
                        let prev = self.last_shown_ctx;
                        (*ctx).replaced_ctx.store(prev, Ordering::Relaxed);
                        // The frame the fence will describe, stamped while this
                        // buffer is still on the layer and therefore still the one
                        // `ticket` holds.
                        if !prev.is_null() {
                            let t = (*prev).ticket.load(Ordering::Acquire);
                            (*prev).release_ticket.store(t, Ordering::Release);
                        }
                    }
                    match build_show(&api, self.sc, ahb, ctx, rect, fence) {
                        Some(tx) => {
                            apply_transaction(&api, tx);
                            self.last_shown_ctx = ctx;
                            // SAFETY: `ctx` is the leaked mailbox of this slot.
                            unsafe {
                                (*ctx)
                                    .applied_ns
                                    .store(crate::vsync::now_ns(), Ordering::Relaxed);
                            }
                            // What is on the layer is what a later commit falls
                            // back to.
                            self.last_publish = Some(LastPublish {
                                ino: published_ino,
                                src,
                                dst,
                                dataspace,
                            });
                            // The frame is on the layer; dump what we
                            // published (rate-limited, off the reply path).
                            self.dump_published(ahb, fb.width, fb.height, stride);
                        }
                        // Transaction creation failed: nothing went out, so the
                        // acquire fence was not consumed and must be closed here.
                        None => {
                            close_fence(fence);
                        }
                    }
                } else {
                    // Plane has nothing to show: hide the layer so the panel
                    // goes dark the way a disabled KMS plane does. The last
                    // published buffer is retained: the plane can come back on
                    // the next commit, and hiding is a deliberate state rather
                    // than a lost buffer.
                    //
                    // UNGATED, change-latched: this is the one path where the
                    // daemon itself blanks the panel, so whether the session
                    // ever went dark (and when, and for how long) is visible in
                    // logcat. A steady state prints once at the transition.
                    if !self.hidden {
                        self.hidden = true;
                        asc_log!(
                            "{}",
                            format_args!("gndrm-daemon: asc layer HIDDEN (plane not drawable) fb={} src=({},{})-({},{}) dst=({},{})-({},{}) mode={}x{} window={}x{}",fb.id,
                            src.left,
                            src.top,
                            src.right,
                            src.bottom,
                            dst.left,
                            dst.top,
                            dst.right,
                            dst.bottom,
                            self.mode_w,
                            self.mode_h,
                            self.win_w,
                            self.win_h)
                        );
                    }
                    if let Some(tx) = build_hide(&api, self.sc) {
                        apply_transaction(&api, tx);
                    }
                    close_fence(fence);
                }
            }
        }

        self.frames += 1;
        if fb.id != 0 {
            crate::clear_booting_splash_on_first_frame();
            // The frame ring counts what the session produced, from the
            // producer reports (see the `FrameAcquired` branch in
            // `serve_scanout`): keeping both on one counter is what made a cap
            // above the panel read as fps the panel never showed. A session
            // without a producing shim (Xwayland GL, GDI, software rendering)
            // sends no reports at all, and for it a publish is the only frame
            // count there is.
            if !producer_reports_live() {
                crate::notify_frame_published();
            }
        }
    }

    /// The copy target for `ino`, a dma-buf the pool does not own, set up on
    /// first sight (map the source, allocate the target) and reused for every
    /// later frame of the same layout. `None` when there is no source to read
    /// or the copy is not possible (logged once per inode).
    fn foreign_target(
        &mut self,
        ino: u64,
        fb: &Framebuffer,
        src_fd: Option<RawFd>,
    ) -> Option<PoolBuf> {
        let layout = Layout::of(fb);
        if let Some(slot) = self.foreign.get(&ino) {
            if slot.layout == layout {
                return slot.buf();
            }
        }
        let src_fd = src_fd?;
        let api = self.api?;
        self.foreign.remove(&ino);
        let slot = match ForeignCopy::new(fb, src_fd) {
            Ok(copy) => {
                self.foreign_buffers += 1;
                // A copy target is the pool's own buffer, not one the shim
                // allocated: no producer holds it by a scan-out inode, so it is
                // registered unnamed and no release note is filed for it.
                let (ctx, _latched) = new_release_ctx(&api, 0);
                ForeignSlot {
                    layout,
                    copy: Some(copy),
                    ctx,
                }
            }
            Err(reason) => {
                self.note_foreign_failure(ino, layout, &reason);
                ForeignSlot {
                    layout,
                    copy: None,
                    ctx: ptr::null_mut(),
                }
            }
        };
        let buf = slot.buf();
        self.foreign.insert(ino, slot);
        buf
    }

    /// Copy the current frame of foreign `ino` into its target. Waits for the
    /// frame's acquire fence first (consuming it): the CPU reads the pixels,
    /// so the composer must not be handed the fence afterwards. `false` when
    /// the copy failed; the target is then dropped and the failure logged.
    fn copy_foreign(&mut self, ino: u64, fb: &Framebuffer, fence: &mut Option<RawFd>) -> bool {
        if let Some(fd) = fence.take() {
            wait_fence_for(fd, ACQUIRE_FENCE_TIMEOUT_MS);
            close_fence(Some(fd));
        }
        let Some(copy) = self.foreign.get_mut(&ino).and_then(|s| s.copy.as_mut()) else {
            return false;
        };
        match copy.copy() {
            Ok(()) => {
                let first = copy.copies == 1;
                self.foreign_frames += 1;
                if first && self.foreign_logged < FOREIGN_LOG_BUDGET {
                    self.foreign_logged += 1;
                    log_always!(
                        "{}",
                        format_args!("gndrm-daemon: foreign dma-buf ino={ino} {}x{} fourcc={} pitch={} -> copied into pool (reason: not pool-backed)",
                            fb.width,
                            fb.height,
                            gndrm_daemon::foreign::fourcc_name(fb.pixel_format),
                            fb.pitches[0])
                    );
                }
                true
            }
            Err(reason) => {
                if let Some(slot) = self.foreign.get_mut(&ino) {
                    slot.copy = None;
                }
                self.note_foreign_failure(ino, Layout::of(fb), &reason);
                false
            }
        }
    }

    /// The always-on failure line for a foreign buffer, within the budget.
    fn note_foreign_failure(&mut self, ino: u64, layout: Layout, reason: &str) {
        self.foreign_failures += 1;
        if self.foreign_logged < FOREIGN_LOG_BUDGET {
            self.foreign_logged += 1;
            log_always!(
                "{}",
                format_args!("gndrm-daemon: foreign dma-buf ino={ino} {layout} copy FAILED ({reason}); frames dropped")
            );
        }
    }

    /// Drop the copy targets of inodes the client no longer imports, keeping
    /// the one the layer may still fall back to.
    fn retire_foreign(&mut self, live_inos: &HashSet<u64>, keep: Option<u64>) {
        if self.foreign.is_empty() {
            return;
        }
        self.foreign
            .retain(|ino, _| live_inos.contains(ino) || Some(*ino) == keep);
    }

    /// Two reference words from a published buffer — the centre pixel and one at
    /// (400,300) — read under the same lock pattern as
    /// [`AscState::dump_published`]. The word's alpha field plus the word itself
    /// answer the two questions a counter cannot: does the buffer carry content,
    /// and can a compositor show it at all. That pair is what identified the
    /// scaler composite's alpha 0.
    ///
    /// `None` when the buffer cannot be locked, reported as `a=none`.
    fn content_sample(
        ahb: *mut ffi::AHardwareBuffer,
        stride: u32,
        height: u32,
    ) -> Option<(u32, u32)> {
        let mut base: *mut c_void = ptr::null_mut();
        // SAFETY: `ahb` is a live buffer the presenter owns; the lock maps its
        // pixels read-only at `base` until the matching unlock below.
        let rc = unsafe {
            ffi::AHardwareBuffer_lock(
                ahb,
                ffi::AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN,
                -1,
                ptr::null(),
                &mut base,
            )
        };
        if rc != 0 || base.is_null() {
            return None;
        }
        let stride_words = stride as usize / core::mem::size_of::<u32>();
        let pixels = base.cast::<u32>();
        let centre = (height as usize / 2) * stride_words + (stride_words / 2);
        // SAFETY: both offsets are inside the `stride * height` bytes the lock
        // maps, and nothing reads `base` after the unlock.
        let w0 = unsafe { pixels.add(centre).read_unaligned() };
        let w1 = unsafe { pixels.add(300 * stride_words + 400).read_unaligned() };
        // SAFETY: `ahb` is locked by this call and nothing reads `base` after.
        unsafe { ffi::AHardwareBuffer_unlock(ahb, ptr::null_mut()) };
        Some((w0, w1))
    }

    /// Keep a rolling PPM of the frame the presenter just published as
    /// `$GNDRM_DUMP_DIR/gn-fb-latest.ppm`, so what gamescope rendered here can
    /// be compared with the client's own submitted buffer (the ICD shim's
    /// `GAMENATIVE_DUMP_DIR` dump). The pool buffer is locked for CPU read and
    /// its rows converted to a P6 PPM in the session layout's channel order
    /// (see [`crate::dump::ppm`]).
    ///
    /// Runs after the transaction is applied, on the commit worker, so it holds
    /// no pool lock and never touches the serve loop. Rate-limited to the first
    /// frames, every [`DUMP_EVERY`]-th afterwards, and every geometry change.
    /// A no-op (a single cached env lookup) when the dump directory is unset.
    fn dump_published(
        &mut self,
        ahb: *mut ffi::AHardwareBuffer,
        width: u32,
        height: u32,
        stride: u32,
    ) {
        let Some(dir) = dump_dir() else {
            return;
        };
        let changed = self.dump_geom != Some((width, height));
        self.dump_geom = Some((width, height));
        let n = self.dump_n;
        self.dump_n += 1;
        if !changed && n >= DUMP_FIRST && !n.is_multiple_of(DUMP_EVERY) {
            return;
        }
        let mut base: *mut c_void = ptr::null_mut();
        // SAFETY: `ahb` is a live buffer the presenter owns; the lock maps its
        // pixels read-only at `base` until the matching unlock.
        let rc = unsafe {
            ffi::AHardwareBuffer_lock(
                ahb,
                ffi::AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN,
                -1,
                ptr::null(),
                &mut base,
            )
        };
        if rc != 0 || base.is_null() {
            crate::log::daemon_info(&format!(
                "{}",
                format_args!("dump {n}: AHardwareBuffer_lock failed (rc={rc})")
            ));
            return;
        }
        let len = stride as usize * height as usize;
        // SAFETY: the lock above maps `stride * height` readable bytes at
        // `base`; the slice is not used after the unlock below.
        let src = unsafe { core::slice::from_raw_parts(base.cast::<u8>(), len) };
        let ppm = crate::dump::ppm(
            src,
            width as usize,
            height as usize,
            stride as usize,
            gndrm_daemon::scanout_layout(),
        );
        // SAFETY: `ahb` is locked by this call and nothing reads `base` after.
        unsafe { ffi::AHardwareBuffer_unlock(ahb, ptr::null_mut()) };
        let path = std::path::Path::new(dir).join("gn-fb-latest.ppm");
        match std::fs::write(&path, &ppm) {
            Ok(()) => crate::log::daemon_info(&format!(
                "{}",
                format_args!("dump {n}: {width}x{height} stride={stride} wrote {}",
                    path.display())
            )),
            Err(e) => crate::log::daemon_info(&format!(
                "{}",
                format_args!("dump {n}: write failed: {e}")
            )),
        }
    }

    /// Put the last published buffer back on the layer.
    ///
    /// A `SurfaceControl` layer with no buffer set composites nothing, so any
    /// path that (re-)creates the layer over a table that already holds a
    /// published buffer must restore it rather than wait for the next commit.
    /// No-op when nothing has been published: there is no buffer to restore,
    /// and inventing one would put uninitialised memory on the panel.
    pub fn republish_last(&mut self) {
        let (Some(lp), Some(api)) = (self.last_publish, self.api) else {
            return;
        };
        if self.sc.is_null() {
            return;
        }
        let handles = self
            .pool
            .lock(Role::Serve)
            .get(lp.ino)
            .or_else(|| self.foreign.get(&lp.ino).and_then(ForeignSlot::buf));
        let Some(PoolBuf { ahb, ctx, .. }) = handles else {
            return;
        };
        let rect = ShowRect {
            src: lp.src,
            dst: lp.dst,
            dataspace: lp.dataspace,
        };
        if let Some(tx) = build_show(&api, self.sc, ahb, ctx, rect, None) {
            apply_transaction(&api, tx);
            self.last_shown_ctx = ctx;
            // SAFETY: `ctx` is the leaked mailbox of this slot.
            unsafe {
                (*ctx)
                    .applied_ns
                    .store(crate::vsync::now_ns(), Ordering::Relaxed);
            }
        }
        // UNGATED: a rebuilt layer has just been given the retained frame back.
        // This runs once per layer rebuild (session bring-up, a display power
        // cycle), never per frame.
        asc_log!(
            "{}",
            format_args!("gndrm-daemon: asc re-published retained ino {} onto a mode {}x{} in a {}x{} window (frames={})",
                lp.ino,
                self.mode_w,
                self.mode_h,
                self.win_w,
                self.win_h,
                self.frames)
        );
    }

    /// Adopt a new display window without ending the session. The listen
    /// sockets, the object stores and every gamescope connection stay exactly
    /// as they are; only the publish layer is rebuilt from `window` and the
    /// retained last frame put back on it, so a display power cycle never
    /// leaves the panel with a bufferless layer.
    ///
    /// The session's mode does not move: it is what gamescope is compositing at
    /// and what the connector advertises, and neither can be re-moded from here.
    /// A new window of another size is therefore reached by the same fit the
    /// first window used.
    ///
    /// Returns `false` when publishing is disabled or the new layer cannot be
    /// created, in which case the caller keeps the current window and layer.
    pub fn retarget(&mut self, window: *mut ffi::ANativeWindow, win_w: i32, win_h: i32) -> bool {
        let Some(api) = self.api else {
            return false;
        };
        if window.is_null() {
            return false;
        }
        // SAFETY: `window` is a live ANativeWindow the caller owns for the rest
        // of the session (it stores it before releasing the old one).
        let new_sc = unsafe { (api.sc_create_from_window)(window, SC_DEBUG_NAME.as_ptr()) };
        if new_sc.is_null() {
            log_warn!(
                "asc: retarget: ASurfaceControl_createFromWindow failed; keeping the current layer"
            );
            return false;
        }
        // The retarget replaces the layer the presenter publishes through;
        // every later transaction names the new one, and no mailbox holds a
        // layer handle, so no completion callback can reach the old layer this
        // call is about to drop.
        let old_sc = core::mem::replace(&mut self.sc, new_sc);
        self.win_w = win_w;
        self.win_h = win_h;
        // SAFETY: the old layer was created by this API, is no longer
        // referenced by the presenter or any mailbox, and is released exactly
        // once here. The window it was created from is released by the caller
        // afterwards.
        if !old_sc.is_null() {
            unsafe { (api.sc_release)(old_sc) };
        }
        // The cursor layer follows the window too; its current image goes
        // back on below.
        // SAFETY: as for the scan-out layer above.
        let new_cursor_sc =
            unsafe { (api.sc_create_from_window)(window, SC_CURSOR_DEBUG_NAME.as_ptr()) };
        let old_cursor_sc = core::mem::replace(&mut self.cursor.sc, new_cursor_sc);
        if !old_cursor_sc.is_null() {
            // SAFETY: created by this API, no longer referenced, released once.
            unsafe { (api.sc_release)(old_cursor_sc) };
        }
        log_info!(
            "{}",
            format_args!("asc: retargeted to the new window ({win_w}x{win_h})")
        );
        // A layer with no buffer set composites nothing (the app's background
        // shows through as a blank panel), so the retained frame goes straight
        // back on before anything else can run.
        self.republish_last();
        if let Some(last) = self.cursor.last.clone() {
            self.cursor.visible = false;
            self.show_cursor(&last, true);
        }
        true
    }

    /// Put a cursor-plane change on the cursor layer: show the cursor image at
    /// the plane's rect, move it, or hide it. A new image is copied once into a
    /// daemon-owned buffer in the session's layout; a motion is a geometry-only
    /// transaction. The scan-out layer is never touched here, so a cursor-only
    /// commit costs SurfaceFlinger one small layer update and no frame.
    /// `src_fd` is the cursor buffer's dma-buf, borrowed.
    pub fn publish_cursor(&mut self, update: &CursorUpdate, src_fd: Option<RawFd>) {
        if self.api.is_none() || self.cursor.sc.is_null() {
            return;
        }
        if update.fb.is_none() || update.ino.is_none() {
            self.cursor.last = None;
            self.hide_cursor();
            return;
        }
        // A new image (or a new framebuffer on the same buffer) is copied
        // before it goes on the layer.
        let mut new_buffer = false;
        if let (Some(fb), Some(ino)) = (update.fb.as_ref(), update.ino) {
            let layout = Layout::of(fb);
            let same_target = self
                .cursor
                .slot
                .as_ref()
                .is_some_and(|s| s.ino == ino && s.layout == layout);
            if !same_target {
                let Some(fd) = src_fd else {
                    self.note_cursor_failure(ino, layout, "no dma-buf");
                    return;
                };
                match ForeignCopy::new(fb, fd) {
                    Ok(copy) => {
                        self.cursor.slot = Some(CursorSlot {
                            ino,
                            layout,
                            fb_id: 0,
                            copy,
                        });
                    }
                    Err(reason) => {
                        self.note_cursor_failure(ino, layout, &reason);
                        return;
                    }
                }
            }
            let Some(slot) = self.cursor.slot.as_mut() else {
                return;
            };
            if slot.fb_id != fb.id {
                if let Err(reason) = slot.copy.copy() {
                    self.cursor.slot = None;
                    self.note_cursor_failure(ino, layout, &reason);
                    return;
                }
                slot.fb_id = fb.id;
                new_buffer = true;
                self.cursor.images += 1;
                if self.cursor.logged < CURSOR_LOG_BUDGET {
                    self.cursor.logged += 1;
                    log_always!(
                        "{}",
                        format_args!("gndrm-daemon: asc cursor image fb={} ino={ino} {} -> cursor layer (images={})",
                            fb.id,
                            layout,
                            self.cursor.images)
                    );
                }
            }
        }
        self.cursor.last = Some(update.clone());
        self.show_cursor(update, new_buffer);
    }

    /// Show (or move) the cursor layer at `update`'s rect; `set_buffer` puts
    /// the slot's buffer on it as well.
    fn show_cursor(&mut self, update: &CursorUpdate, set_buffer: bool) {
        let Some(api) = self.api else {
            return;
        };
        let Some(ahb) = self.cursor.slot.as_ref().map(|s| s.copy.ahb) else {
            return;
        };
        let p = &update.plane;
        let Some((src, dst)) = gndrm_daemon::scanout::cursor_rects(
            (p.crtc_x, p.crtc_y, p.crtc_w, p.crtc_h),
            (p.src_x >> 16, p.src_y >> 16, p.src_w >> 16, p.src_h >> 16),
            (self.mode_w as u32, self.mode_h as u32),
            (self.win_w as u32, self.win_h as u32),
        ) else {
            // Entirely off the mode: nothing of it is on the panel.
            self.hide_cursor();
            return;
        };
        let src = ffi::ARect {
            left: src.0,
            top: src.1,
            right: src.2,
            bottom: src.3,
        };
        let dst = ffi::ARect {
            left: dst.0,
            top: dst.1,
            right: dst.2,
            bottom: dst.3,
        };
        let plane = (
            src.right - src.left,
            src.bottom - src.top,
            dst.right - dst.left,
            dst.bottom - dst.top,
        );
        let (staged, rot_override) = cursor_overrides();
        let derived = gndrm_daemon::cursor::derive_rotation(
            (self.win_w as u32, self.win_h as u32),
            natural_landscape(),
        );
        let rot = rot_override.unwrap_or(derived);
        let mut transform = 0;
        let mut ahb = ahb;
        let (mut src, mut dst) = (src, dst);
        let mut restaged = false;
        // Staged: the cursor's opaque extent, pre-scaled and pre-rotated, so
        // the composer sees an unrotated, unscaled layer. A source that cannot
        // be read back (10-bit) or staged keeps the padded buffer.
        let mut shown_as = None;
        if let Some((plan, new_stage)) =
            staged.then(|| self.stage_cursor(&src, &dst, rot)).flatten()
        {
            if let Some((_, st)) = self.cursor.stage.as_ref() {
                ahb = st.ahb;
                restaged = new_stage;
                src = ffi::ARect {
                    left: 0,
                    top: 0,
                    right: st.width as c_int,
                    bottom: st.height as c_int,
                };
                dst = ffi::ARect {
                    left: plan.dst.0,
                    top: plan.dst.1,
                    right: plan.dst.2,
                    bottom: plan.dst.3,
                };
                transform = plan.transform;
                shown_as = Some(plan.rot);
            }
        }
        let presentation_changed = self.cursor.shown_as != Some(shown_as);
        self.cursor.shown_as = Some(shown_as);
        let set_buffer = set_buffer || presentation_changed || restaged;
        let sc = self.cursor.sc;
        let becomes_visible = !self.cursor.visible;
        if (becomes_visible || presentation_changed || restaged)
            && self.cursor.stage_logged < CURSOR_STAGE_LOG_BUDGET
        {
            self.cursor.stage_logged += 1;
            log_always!(
                "{}",
                format_args!("gndrm-daemon: asc cursor {} rot={rot} ({}) transform={transform} src=({},{})-({},{}) dst=({},{})-({},{}) plane_src={}x{} plane_dst={}x{} window={}x{} natural={}",
                    if shown_as.is_some() { "staged" } else { "padded" },
                    if rot_override.is_some() { "override" } else { "derived" },
                    src.left, src.top, src.right, src.bottom,
                    dst.left, dst.top, dst.right, dst.bottom,
                    plane.0, plane.1, plane.2, plane.3,
                    self.win_w, self.win_h,
                    if natural_landscape() { "landscape" } else { "portrait" })
            );
        }
        // SAFETY: one transaction, applied and deleted below; `sc` and `ahb`
        // are live (the slot or stage owns the buffer reference;
        // SurfaceFlinger takes its own on setBuffer).
        unsafe {
            let tx = (api.st_create)();
            if tx.is_null() {
                return;
            }
            (api.st_set_zorder)(tx, sc, Z_CURSOR);
            if becomes_visible || set_buffer {
                (api.st_set_visibility)(tx, sc, 1);
                (api.st_set_buffer_transparency)(
                    tx,
                    sc,
                    ffi::ASURFACE_TRANSACTION_TRANSPARENCY_TRANSLUCENT,
                );
            }
            if set_buffer || becomes_visible {
                (api.st_set_buffer)(tx, sc, ahb, -1);
                (api.st_set_buffer_dataspace)(tx, sc, ffi::ADATASPACE_UNKNOWN);
            }
            (api.st_set_geometry)(tx, sc, &src, &dst, transform);
            apply_transaction(&api, tx);
        }
        if becomes_visible {
            self.cursor.visible = true;
            if self.cursor.logged < CURSOR_LOG_BUDGET {
                self.cursor.logged += 1;
                log_always!(
                    "{}",
                    format_args!("gndrm-daemon: asc cursor layer shown dst=({},{})-({},{}) mode={}x{} window={}x{}",
                        dst.left,
                        dst.top,
                        dst.right,
                        dst.bottom,
                        self.mode_w,
                        self.mode_h,
                        self.win_w,
                        self.win_h)
                );
            }
        } else {
            self.cursor.moves += 1;
        }
    }

    /// Stage the current cursor image for the plane's visible `src` (buffer
    /// pixels) and `dst` (window pixels), pre-rotated by `rot`: the plan, and
    /// whether a new staged buffer was made. `None` when the image cannot be
    /// read back (10-bit) or staged; the caller then shows the padded buffer.
    fn stage_cursor(
        &mut self,
        src: &ffi::ARect,
        dst: &ffi::ARect,
        rot: u32,
    ) -> Option<(gndrm_daemon::cursor::StagePlan, bool)> {
        use gndrm_daemon::cursor::{opaque_extent, stage_plan, transform_cursor};
        let images = self.cursor.images;
        if self.cursor.pixels.as_ref().is_none_or(|p| p.0 != images) {
            self.cursor.pixels = self.cursor.slot.as_ref().and_then(|s| {
                s.copy
                    .stage_rows()
                    .map(|(px, w, _)| (images, px, w, s.copy.format))
            });
        }
        let (_, pixels, bw, format) = self.cursor.pixels.as_ref()?;
        let (bw, format) = (*bw, *format);
        let src_e = (src.left, src.top, src.right, src.bottom);
        let extent = match self.cursor.extent {
            Some((k, e)) if k == (images, src_e) => e,
            _ => {
                let e = opaque_extent(pixels, bw, src_e);
                self.cursor.extent = Some(((images, src_e), e));
                e
            }
        };
        let plan = stage_plan(
            src_e,
            (dst.left, dst.top, dst.right, dst.bottom),
            extent,
            rot,
        )?;
        // The crop must lie inside the read-back image.
        let rows = pixels.len() / (bw.max(1) as usize * 4);
        if plan.crop.0 + plan.crop.2 > bw || (plan.crop.1 + plan.crop.3) as usize > rows {
            return None;
        }
        let key = StageKey {
            images,
            rot: plan.rot,
            crop: plan.crop,
            out: plan.out,
        };
        if self.cursor.stage.as_ref().is_some_and(|(k, _)| *k == key) {
            return Some((plan, false));
        }
        let (img, w, h) = transform_cursor(pixels, bw, plan.crop, plan.out, plan.rot);
        match CursorStage::new(&img, w, h, format) {
            Ok(st) => {
                self.cursor.stage = Some((key, st));
                Some((plan, true))
            }
            Err(reason) => {
                self.cursor.stage = None;
                if self.cursor.stage_logged < CURSOR_STAGE_LOG_BUDGET {
                    self.cursor.stage_logged += 1;
                    log_always!(
                        "{}",
                        format_args!("gndrm-daemon: asc cursor stage FAILED ({reason}); showing the padded buffer")
                    );
                }
                None
            }
        }
    }

    /// Hide the cursor layer (cursor plane off). The image is kept: the
    /// cursor usually comes back with the same one.
    fn hide_cursor(&mut self) {
        if !self.cursor.visible {
            return;
        }
        let Some(api) = self.api else {
            return;
        };
        if let Some(tx) = build_hide(&api, self.cursor.sc) {
            // SAFETY: `tx` is live; the z-order build_hide sets is the
            // scan-out layer's, so restore the cursor's before applying.
            unsafe { (api.st_set_zorder)(tx, self.cursor.sc, Z_CURSOR) };
            apply_transaction(&api, tx);
        }
        self.cursor.visible = false;
        self.cursor.hides += 1;
    }

    /// The always-on failure line for a cursor image, within the budget.
    fn note_cursor_failure(&mut self, ino: u64, layout: Layout, reason: &str) {
        self.cursor.failures += 1;
        self.hide_cursor();
        if self.cursor.logged < CURSOR_LOG_BUDGET {
            self.cursor.logged += 1;
            log_always!(
                "{}",
                format_args!("gndrm-daemon: asc cursor image ino={ino} {layout} copy FAILED ({reason}); cursor hidden")
            );
        }
    }

    /// Adopt a window geometry reported by the same window without rebuilding the
    /// layer (a resize on the same window: the geometry only bounds the published
    /// rect).
    pub fn set_window(&mut self, win_w: i32, win_h: i32) {
        self.win_w = win_w;
        self.win_h = win_h;
    }
}

impl Drop for AscState {
    fn drop(&mut self) {
        // UNGATED session-end summary: what the layer was left holding and how
        // much of the session the presenter actually published. A session that
        // ends with the daemon having published nothing, or with the layer
        // hidden, is a different failure from one that published to the end —
        // and this is the last line the presenter can print.
        asc_log!(
            "{}",
            format_args!("gndrm-daemon: asc teardown frames_published={} hidden={} last_publish_ino={} last_dst=({},{})-({},{}) mode={}x{} window={}x{}",self.frames,
            self.hidden,
            self.last_publish.map_or(0, |lp| lp.ino),
            self.last_publish.map_or(0, |lp| lp.dst.left),
            self.last_publish.map_or(0, |lp| lp.dst.top),
            self.last_publish.map_or(0, |lp| lp.dst.right),
            self.last_publish.map_or(0, |lp| lp.dst.bottom),
            self.mode_w,
            self.mode_h,
            self.win_w,
            self.win_h)
        );
        // Return every buffer before the layer goes: the presenter owns the
        // daemon's reference on each, and SurfaceFlinger may still be reading
        // one. The map work runs under the lock; the gralloc releases run
        // outside it.
        let retired = self.pool.lock(Role::Serve).take_all();
        release_retired(&retired);
        if self.foreign_buffers > 0 || self.foreign_failures > 0 {
            log_always!(
                "{}",
                format_args!("gndrm-daemon: foreign dma-buf summary: copied_frames={} buffers={} failures={}",
                    self.foreign_frames,
                    self.foreign_buffers,
                    self.foreign_failures)
            );
        }
        if self.cursor.images > 0 || self.cursor.failures > 0 {
            log_always!(
                "{}",
                format_args!("gndrm-daemon: asc cursor summary images={} moves={} hides={} failures={}",
                    self.cursor.images,
                    self.cursor.moves,
                    self.cursor.hides,
                    self.cursor.failures)
            );
        }
        self.cursor.slot = None;
        if !self.cursor.sc.is_null() {
            if let Some(api) = self.api {
                // SAFETY: created by this API, released once, here.
                unsafe { (api.sc_release)(self.cursor.sc) };
            }
            self.cursor.sc = ptr::null_mut();
        }
        self.foreign.clear();
        if !self.sc.is_null() {
            if let Some(api) = self.api {
                // SAFETY: the layer was created by this API and is released
                // once, here; nothing publishes through it any more.
                unsafe { (api.sc_release)(self.sc) };
            }
            self.sc = ptr::null_mut();
        }
    }
}

/// The presenter the daemon drives: a handle onto the shared [`AscState`].
pub struct AscPresenter {
    state: Arc<Mutex<AscState>>,
}

impl AscPresenter {
    #[must_use]
    pub fn new(state: Arc<Mutex<AscState>>) -> Self {
        Self { state }
    }

    /// The lock cannot be left poisoned: the presenter runs under
    /// `panic = "abort"`-style bring-up paths where a panic is already fatal,
    /// and a poisoned table must still not wedge the frame loop.
    fn lock(&self) -> MutexGuard<'_, AscState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Presenter for AscPresenter {
    fn publish(
        &mut self,
        fb: &Framebuffer,
        plane: &PlaneState,
        ino: Option<u64>,
        live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    ) {
        self.lock().publish(fb, plane, ino, None, live_inos, fence);
    }

    fn publish_with_source(
        &mut self,
        fb: &Framebuffer,
        plane: &PlaneState,
        ino: Option<u64>,
        src_fd: Option<RawFd>,
        live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    ) {
        self.lock()
            .publish(fb, plane, ino, src_fd, live_inos, fence);
    }

    fn publish_cursor(&mut self, cursor: &CursorUpdate, src_fd: Option<RawFd>) {
        self.lock().publish_cursor(cursor, src_fd);
    }
}

/// Why [`serve_scanout`] handed the connection back to the acceptor.
///
/// The acceptor owns the fd's lifetime; this says what it should do next.
pub enum ScanoutChannel {
    /// The peer closed, the channel failed, or a request could not be answered.
    /// The acceptor closes this fd and accepts the next connection.
    Closed,
    /// A newer connection is already queued on the listener, so this fd is no
    /// longer the current one. The acceptor closes it and adopts the newest
    /// connection; requests sent on the newer one must never wait behind a
    /// reader parked on this one.
    Superseded,
    /// Teardown was signalled. The acceptor closes this fd and returns.
    Stop,
}

/// UNGATED one-shot: the first frame on the scan-out channel that is **not**
/// an allocator request. Reports the magic it carried, whether it decoded as a
/// game-window note, and the session latch the note drives — so the note's
/// arrival and its effect are visible in logcat with no env gate (the daemon
/// runs in the app process, which the container's env vars do not reach, so a
/// gated line here is invisible on device). One-shot: a static latch, flipped
/// on the first call, returns on every later one, so nothing formats again and
/// the allocator path never calls this at all.
fn log_first_non_request_frame(frame: &[u8]) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    let magic = frame
        .get(0..4)
        .map_or(0, |m| u32::from_le_bytes([m[0], m[1], m[2], m[3]]));
    asc_log!(
        "{}",
        format_args!("gndrm-daemon: scan-out channel non-request frame magic={magic:#010x}")
    );
}

/// Serve the scan-out allocator channel until the peer goes away.
///
/// One request per `SOCK_SEQPACKET` message, one response back with the
/// dma-buf attached via `SCM_RIGHTS` — the same out-of-band fd mechanism
/// `PRIME_HANDLE_TO_FD` already uses on the RPC channel. A short frame, a
/// non-request frame, or a peer that closes ends the call; the caller then
/// accepts the next connection, which is what lets a restarted shim (or a
/// restarted daemon, on the shim side) come back without a relaunch.
///
/// The loop waits on the request fd *together with* `listener` and `stop`, so a
/// connection that arrives while this one is idle supersedes it instead of
/// queueing behind it: one reader, and it is always on the current fd. Without
/// this, a shim that reconnects while the daemon is still parked here would have
/// its requests written to the new connection while the daemon read the old one
/// — the lost-request fault this fixes.
///
/// The reply is assembled from map work only. A matching spare is handed over
/// with no gralloc call at all; otherwise the allocation, the native-handle
/// lookup and every descriptor duplicate run with the pool lock dropped. A
/// refusal is logged with its precise cause and the time the request spent
/// waiting for the lock, so a shim-side `EAGAIN` can be told apart from pool
/// exhaustion, an allocator failure and lock contention.
pub fn serve_scanout(
    fd: RawFd,
    listener: RawFd,
    stop: RawFd,
    cap_fd: RawFd,
    pool: &Arc<TrackedMutex<ScanoutPool>>,
    doorbell: &Arc<WarmDoorbell>,
) -> ScanoutChannel {
    let d = diag::diag();
    // This connection is the allocator until it ends; a failed push to it is
    // left for this loop to close (see `gndrm_daemon::pushes`), and it stops
    // being served when the loop returns however that happens.
    gndrm_daemon::pushes::set_served(fd);
    let _served = ServedGuard(fd);
    // Every inbound type, not just the alloc request: a short buffer on a
    // SEQPACKET connection truncates the frame instead of failing it.
    let mut buf = [0u8; gndrm_uapi::wire::INBOUND_MAX];
    // Whether a connection opted in is `gndrm_daemon::pushes`' business: a
    // subscription outlives this loop, so the notes cannot be gated on it here.
    // Whether this connection reads the panel's latch grid, pushed whenever it
    // moves. Only a shim that declared so is sent one: an older shim's reply
    // reader takes a frame it does not know for a dead channel. Cleared when
    // this connection is done, whichever way it returns.

    let vsync_fd = crate::vsync::changed_fd();
    // Wake this loop when a completion lands: the callback runs on
    // SurfaceFlinger's thread and may not write the channel itself.
    let present_fd = eventfd_new();
    PRESENT_FD.store(present_fd, Ordering::Relaxed);
    PRESENT_AWARE.store(false, Ordering::Relaxed);
    let _present_channel = PresentChannel(present_fd);
    // Reused across turns: `poll` is rebuilt every turn (the descriptor set
    // moves), but the two vectors holding it are allocated once per channel.
    let mut pfds: Vec<libc::pollfd> = Vec::with_capacity(8);
    let mut retired: Vec<RawFd> = Vec::with_capacity(4);
    loop {
        // The channel is idle until a request arrives. Its state is what tells
        // a shim-side timeout apart from a request the daemon never read: if
        // the shim timed out while this row says "wait-request" and the request
        // counters did not move, the request never reached this thread.
        d.set_role(Role::Scanout, State::ScanoutWaitingRequest);
        // Wait for a request on the current fd, a newer connection on the
        // listener, a changed frame-cap target, or teardown. Watching the
        // listener here (rather than only after this loop returns) is what
        // makes the current fd the newest one; watching `cap_fd` is what lets a
        // target the user changed mid-session reach a shim that is not handing
        // a buffer over at that instant.
        // Retired subscribers keep reporting (see `pushes`), and this loop is
        // where the daemon spends its time: their descriptors are polled beside
        // the served one, and their frames read in the drain below.
        gndrm_daemon::pushes::fds_except_into(&mut retired, fd);
        // Slots 0..6 keep their meaning whatever the descriptors are: a `-1`
        // eventfd stays in its row (poll ignores it) rather than shifting the
        // indices the branches below read.
        pfds.clear();
        for fd in [fd, listener, stop, cap_fd, vsync_fd, present_fd] {
            pfds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for fd in &retired {
            pfds.push(libc::pollfd {
                fd: *fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // SAFETY: `pfds` holds live descriptors (`len` of them); a negative one
        // (no eventfd yet) is ignored by `poll`.
        let count = u32::try_from(pfds.len()).unwrap_or(u32::MAX);
        let ready = unsafe { libc::poll(pfds.as_mut_ptr(), count, -1) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            log_info!("{}", format_args!("asc: scan-out channel poll: {e}"));
            return ScanoutChannel::Closed;
        }
        if pfds[2].revents != 0 {
            return ScanoutChannel::Stop;
        }
        // Retired connections: read what they report, and forget one that has
        // gone away. Their allocator requests are not answered — the pool
        // belongs to this connection.
        for (i, fd) in retired.iter().enumerate() {
            if pfds[6 + i].revents == 0 {
                continue;
            }
            if !drain_retired(*fd) {
                let _ = gndrm_daemon::pushes::forget(*fd);
                // SAFETY: the peer closed; this registry held the only claim.
                unsafe { libc::close(*fd) };
            }
        }
        if pfds[3].revents != 0 {
            // The target changed. Drain the eventfd first, so a count left
            // behind cannot wake this loop again. The note is pushed even when
            // a request is pending: nothing else carries the target, and a
            // skipped push would be lost — the shim reads frames by their magic
            // whatever order they arrive in, so a note ahead of the reply it
            // waits for is in step.
            flush_cap(cap_fd);
        }
        if pfds[4].revents != 0 {
            // The panel's latch grid moved: its phase, or the lead the shim
            // should commit by. Drained first, like the cap's.
            flush_vsync(vsync_fd);
        }
        if pfds[5].revents != 0 {
            // A publish completed. Its id and latch time were queued by the
            // callback; written here, on the thread that owns the channel.
            drain_eventfd(present_fd);
            if PRESENT_AWARE.load(Ordering::Relaxed) {
                let done: Vec<PresentDone> = {
                    let mut q = PRESENT_DONE.lock().unwrap_or_else(|e| e.into_inner());
                    core::mem::take(&mut *q)
                };
                for note in done {
                    if send_resp(fd, &note.encode(), None).is_err() {
                        return ScanoutChannel::Closed;
                    }
                }
            }
        }
        if pfds[1].revents != 0 {
            // A newer connection is already queued: this fd has been superseded
            // and must be closed before the acceptor serves the newest one.
            log_info!(
                "{}",
                format_args!("asc: scan-out channel superseded (fd={fd}); adopting the newest connection")
            );
            return ScanoutChannel::Superseded;
        }
        if pfds[0].revents == 0 {
            continue;
        }
        let read_started = Instant::now();
        let (n, fd_in) = match recv_msg(fd, &mut buf) {
            Ok(v) => v,
            Err(e) => {
                log_info!(
                    "{}",
                    format_args!("asc: scan-out channel recv: {e} (requests={} replies={})",
                        d.requests_seen(),
                        d.replies_sent())
                );
                return ScanoutChannel::Closed;
            }
        };
        // Requests carry no descriptor; a stray one would leak in this process.
        if let Some(stray) = fd_in.filter(|f| *f >= 0) {
            // SAFETY: the descriptor was received into this process.
            unsafe { libc::close(stray) };
        }
        if n == 0 {
            log_info!(
                "{}",
                format_args!("asc: scan-out channel closed by peer (requests={} replies={})",
                    d.requests_seen(),
                    d.replies_sent())
            );
            return ScanoutChannel::Closed;
        }
        // The frame-cap opt-in shares this channel and is recognised the same
        // way, by its own magic before the allocator decode. A shim sends it
        // once per connection to declare it honours frame-cap notes; the daemon
        // latches that for this connection and answers with the current target
        // at once, so a shim that connects mid-session starts in step. A
        // connection that never subscribes is sent no note at all.
        if let Some(req) = LsfgConfig::decode(&buf[..n]) {
            // The shim asking for the app's frame-generation settings. An
            // unconfigured session answers with the untouched request shape, which
            // the shim reads as "use your defaults" rather than as "off".
            debug_assert!(req.is_request());
            let reply = crate::lsfg_settings().unwrap_or_else(LsfgConfig::request);
            if send_resp(fd, &reply.encode(), None).is_err() {
                return ScanoutChannel::Closed;
            }
            continue;
        }
        if let Some(sub) = FrameCapSubscribe::decode(&buf[..n]) {
            // Registered before the replies: from here the pushes follow this
            // connection for the rest of the session, whether or not it keeps
            // the allocator role. The replies below are its starting state.
            gndrm_daemon::pushes::add(fd, sub.caps);
            sync_vsync_subscription();
            if sub.caps & CAP_FRAME_CAP != 0 {
                let note = cap_now().note();
                if send_resp(fd, &note.encode(), None).is_err() {
                    return ScanoutChannel::Closed;
                }
                // The frame-generation settings ride with the subscription: a
                // presenter that connects after the app configured itself (the
                // usual case — the app pushes at launch, the shim connects when
                // the guest first allocates) would otherwise wait for the next
                // change the user makes.
                if let Some(settings) = crate::lsfg_settings() {
                    if send_resp(fd, &settings.encode(), None).is_err() {
                        return ScanoutChannel::Closed;
                    }
                }
            }
            if sub.caps & CAP_PRESENT_ID != 0 {
                // A new mark sender, whose tickets may start a new sequence.
                reset_present_marks();
                PRESENT_AWARE.store(true, Ordering::Relaxed);
                PRESENT_FD.store(present_fd, Ordering::Relaxed);
            }
            if sub.caps & CAP_VSYNC_NOTE != 0 {
                // The panel's latch grid, when it has been measured: a presenter
                // that connects mid-session starts on it.
                if let Some(note) = crate::vsync::note() {
                    if send_resp(fd, &note.encode(), None).is_err() {
                        return ScanoutChannel::Closed;
                    }
                }
            }
            log_info!(
                "{}",
                format_args!("asc: scan-out channel frame-cap subscribe (caps={:#x})",
                    sub.caps)
            );
            continue;
        }
        if let Some(acq) = FrameAcquired::decode(&buf[..n]) {
            note_produced(acq);
            continue;
        }
        if let Some(mark) = PresentMark::decode(&buf[..n]) {
            // The frame the shim is about to commit carries this present id.
            note_present_mark(mark);
            continue;
        }
        let Some(raw_req) = ScanoutAllocReq::decode(&buf[..n]) else {
            // A frame that is not an allocator request is logged and skipped,
            // not fatal.
            // The shim rides in the `.tzst` payload and this daemon in
            // `jniLibs`, so the two can be updated apart: a shim sending a
            // message this daemon does not know must cost at most that message,
            // never the scan-out allocator, which would strand the session's
            // buffers. A genuinely malformed allocator request is a bug the log
            // names either way.
            log_first_non_request_frame(&buf[..n]);
            log_warn!(
                "{}",
                format_args!("asc: unrecognised scan-out channel frame ({n} bytes); ignored")
            );
            continue;
        };
        if raw_req.is_lookup() {
            if !serve_lookup(fd, raw_req, pool) {
                return ScanoutChannel::Closed;
            }
            continue;
        }
        // A client that imports through `VK_ANDROID_external_memory_android_hardware_buffer`
        // flags the request; everything else sees the plain format.
        let want_ahb = raw_req.wants_ahb_handle();
        let req = raw_req.without_flags();
        let mut handed_ahb: *mut ffi::AHardwareBuffer = ptr::null_mut();
        // Open the request epoch. From here the watchdog samples this request,
        // and the reply has to be on the wire before the shim's budget expires.
        d.begin_request(req.width, req.height, req.format);
        // A receipt for every request the shim sent (rate-limited): a shim
        // timeout with no receipt here means the request never reached this
        // thread at all — a channel/accept fault, not a slow reply path.
        if d.receipt_allowed() {
            log_info!(
                "{}",
                format_args!("asc: scan-out request {}x{} len={} fmt={:#x} received (seq={})",
                    req.width,
                    req.height,
                    req.len,
                    req.format,
                    d.requests_seen())
            );
        }
        d.note_stage(Stage::Read, read_started.elapsed().as_millis() as u64);
        d.step(State::WaitPoolLock);

        // The pool lock is taken once for validation, the capacity reservation
        // and taking a ready spare — map work only. It is the only thing a
        // request can be made to wait on, so the time to acquire it is what a
        // contention-induced refusal is measured against. The guard reports how
        // long the acquisition took (the tracked lock publishes the holder and
        // the waiters into the diagnostic registry independent of this).
        //
        // A request the shim could not size from a connector mode arrives
        // length-only; it is resolved here to a geometry the daemon knows, so a
        // window-sized scan-out buffer is pool-owned exactly like a mode-sized
        // one. `width`/`height` below are the geometry actually served, which is
        // what the commit, the allocation and the spare must use.
        let (began, lock_waited_ms, width, height) = {
            let mut pool_guard = pool.lock(Role::Scanout);
            let waited = pool_guard.waited_ms();
            let (width, height, began) = if req.is_length_only() {
                match pool_guard.begin_alloc_len(req.len, req.format) {
                    Ok((w, h, reservation)) => (w, h, Ok(reservation)),
                    Err(e) => (0, 0, Err(e)),
                }
            } else {
                (
                    req.width,
                    req.height,
                    pool_guard.begin_alloc(req.width, req.height, req.format),
                )
            };
            (began, waited, width, height)
        };
        d.note_stage(Stage::Lock, lock_waited_ms);

        // A length-only request must be answered with a buffer whose real
        // allocation length is at least the length it asked for. The length
        // index resolves a length to a geometry, but a geometry's actual
        // allocation can be shorter than the length that resolved to it: the
        // same geometry is registered under several lengths, and a client
        // framebuffer can register a geometry under a length longer than its
        // own allocation. A buffer shorter than the request describes a dma-buf
        // larger than it is, so it is not handed out — the allocation falls
        // through to the kernel instead.
        let mut next = began;
        let (resp, fd_out, handed_out, refill_spare) = loop {
            let began =
                core::mem::replace(&mut next, Err(io::Error::from_raw_os_error(libc::EINVAL)));
            match began {
                // A retired buffer of this geometry: no allocation, just a fresh
                // client descriptor duplicated from the buffer the pool already
                // holds. This is the steady state.
                Ok(Reservation::Reuse {
                    ino: cached_ino,
                    payload,
                }) => {
                    d.step(State::ScanoutLocked);
                    let api = pool.lock(Role::Scanout).api();
                    let dup_started = Instant::now();
                    let resolved = api.and_then(|api| dup_scanout_fd(&api, payload.ahb));
                    d.note_stage(Stage::Alloc, dup_started.elapsed().as_millis() as u64);
                    match resolved {
                        Some((fd, ino, size)) => {
                            // A length-only request needs a buffer at least as long
                            // as it asked for; this one is shorter, so it cannot
                            // satisfy the request. Hand it back for a request it
                            // does fit and allocate instead.
                            if req.is_length_only() && !serves_len(size, req.len) {
                                // SAFETY: `fd` is a fresh descriptor this call owns.
                                unsafe { libc::close(fd) };
                                let geom = Geometry {
                                    width,
                                    height,
                                    format: req.format,
                                };
                                pool.lock(Role::Scanout).recycle(geom, cached_ino, payload);
                                next = pool.lock(Role::Scanout).begin_cold();
                                continue;
                            }
                            // The cached buffer was registered under `cached_ino`,
                            // and a dup of its dma-buf keeps the inode — so the
                            // fresh descriptor should carry the same identity.
                            // Verify rather than trust the assumption: if it moved,
                            // the registration key the client will import has
                            // changed, and the commit below re-registers under the
                            // identity actually handed out so the next commit for
                            // this buffer still resolves.
                            if ino != cached_ino {
                                log_warn!("{}", format_args!("asc: scan-out reuse ino moved ({cached_ino} -> {ino}); the free-list identity is stale, re-registering"));
                            }
                            let buffer = ScanoutBuffer {
                                fd,
                                ino,
                                stride: payload.stride,
                                size,
                            };
                            let replaced = pool.lock(Role::Scanout).commit(
                                width,
                                height,
                                &buffer,
                                payload.ahb,
                                false,
                                &"reused",
                            );
                            if let Some(old) = replaced {
                                release_retired(core::slice::from_ref(&retire_buf(old)));
                            }
                            handed_ahb = payload.ahb;
                            break (
                                ScanoutAllocResp {
                                    ret: 0,
                                    ino: buffer.ino,
                                    stride: buffer.stride,
                                },
                                Some(buffer.fd),
                                true,
                                false,
                            );
                        }
                        None => {
                            // The cached buffer could not be turned into a client
                            // descriptor: put it back so the pool does not lose it.
                            let geom = Geometry {
                                width,
                                height,
                                format: req.format,
                            };
                            pool.lock(Role::Scanout).recycle(geom, cached_ino, payload);
                            let refused = refuse_scanout(req, libc::EIO, lock_waited_ms, pool, d);
                            break (refused.0, refused.1, false, false);
                        }
                    }
                }
                // A pre-allocated spare matches: hand it over with no gralloc
                // allocation, and rebuild the spare afterwards.
                Ok(Reservation::Warm(warm)) => {
                    // The spare must satisfy a length-only request just like a
                    // reused buffer; put it back and allocate when it does not.
                    if req.is_length_only() && !serves_len(warm.buffer.size, req.len) {
                        pool.lock(Role::Scanout).put_warm(warm);
                        next = pool.lock(Role::Scanout).begin_cold();
                        continue;
                    }
                    d.step(State::ScanoutLocked);
                    let replaced = pool.lock(Role::Scanout).commit(
                        width,
                        height,
                        &warm.buffer,
                        warm.ahb,
                        false,
                        &"from spare",
                    );
                    if let Some(old) = replaced {
                        release_retired(core::slice::from_ref(&retire_buf(old)));
                    }
                    handed_ahb = warm.ahb;
                    break (
                        ScanoutAllocResp {
                            ret: 0,
                            ino: warm.buffer.ino,
                            stride: warm.buffer.stride,
                        },
                        Some(warm.buffer.fd),
                        true,
                        true,
                    );
                }
                // Nothing matched: allocate and register with the lock dropped
                // across the gralloc work.
                Ok(Reservation::Cold { evicted }) => {
                    // The cache dropped the longest-free buffer to stay under the
                    // cap; release it before allocating its replacement.
                    if let Some(payload) = evicted {
                        release_retired(core::slice::from_ref(&retire_buf(payload)));
                    }
                    let api = pool.lock(Role::Scanout).api();
                    d.step(State::ScanoutAllocating);
                    d.set_role(Role::Scanout, State::ScanoutAllocating);
                    let alloc_started = Instant::now();
                    let allocated = api.map_or(Err(libc::ENOSYS), |api| {
                        allocate_buffer(api, width, height, req.format)
                    });
                    d.note_stage(Stage::Alloc, alloc_started.elapsed().as_millis() as u64);
                    match allocated {
                        Ok((buffer, ahb)) => {
                            // A length-only request the resolved geometry cannot
                            // satisfy: the buffer is shorter than the request. Give
                            // the capacity and the buffer back so the allocation
                            // falls through to the kernel rather than describing a
                            // dma-buf larger than it is.
                            if req.is_length_only() && !serves_len(buffer.size, req.len) {
                                pool.lock(Role::Scanout).abort_alloc();
                                // SAFETY: `buffer.fd` is a fresh descriptor this
                                // call owns, and `ahb` holds the one reference the
                                // allocation returned.
                                unsafe {
                                    libc::close(buffer.fd);
                                    ffi::AHardwareBuffer_release(ahb);
                                }
                                let refused =
                                    refuse_scanout(req, libc::EINVAL, lock_waited_ms, pool, d);
                                break (refused.0, refused.1, false, false);
                            }
                            let replaced = pool.lock(Role::Scanout).commit(
                                width,
                                height,
                                &buffer,
                                ahb,
                                true,
                                &"allocated",
                            );
                            if let Some(old) = replaced {
                                release_retired(core::slice::from_ref(&retire_buf(old)));
                            }
                            handed_ahb = ahb;
                            break (
                                ScanoutAllocResp {
                                    ret: 0,
                                    ino: buffer.ino,
                                    stride: buffer.stride,
                                },
                                Some(buffer.fd),
                                true,
                                true,
                            );
                        }
                        Err(errno) => {
                            pool.lock(Role::Scanout).abort_alloc();
                            let refused = refuse_scanout(req, errno, lock_waited_ms, pool, d);
                            break (refused.0, refused.1, false, false);
                        }
                    }
                }
                Err(e) => {
                    let errno = e.raw_os_error().unwrap_or(libc::EIO).abs();
                    let refused = refuse_scanout(req, errno, lock_waited_ms, pool, d);
                    break (refused.0, refused.1, false, false);
                }
            }
        };

        d.step(State::ScanoutSending);
        d.set_role(Role::Scanout, State::ScanoutSending);
        let send_started = Instant::now();
        let sent = send_resp(fd, &resp.encode(), fd_out);
        d.note_stage(Stage::Reply, send_started.elapsed().as_millis() as u64);
        // Build the diagnostic line while the epoch is still open, so the step
        // it names is the step this request actually reached and `elapsed` is
        // live; then close the epoch with the reply outcome. A failure to write
        // the reply is always reported; a merely slow one is rate-limited (a
        // flood of them is itself the fault, and the counters survive in every
        // snapshot).
        let elapsed_ms = d.request_elapsed_ms();
        let send_failed = sent.is_err();
        let diag_line = if send_failed {
            Some(d.snapshot(&"reply-write-failed"))
        } else if elapsed_ms >= diag::STALL_WARN_MS && d.slow_report_allowed() {
            Some(d.snapshot(&"slow-reply"))
        } else {
            None
        };
        let total_ms = d.end_request(if send_failed {
            Reply::Failed
        } else {
            Reply::Written
        });
        if let Some(line) = diag_line {
            log_warn!(
                "{}",
                format_args!("asc: scan-out request {total_ms} ms: {line}")
            );
        }
        // The daemon's copy of the descriptor is closed on both paths.
        // `sendmsg` with `SCM_RIGHTS` installs a *duplicate* in the peer, so a
        // successful send still leaves this one open here, and a failed send
        // transfers nothing at all. Leaving it open pins the buffer's dma-buf
        // after the AHardwareBuffer is released, so the allocation never
        // returns to the gralloc pool and the allocator runs dry.
        if let Some(fd_out) = fd_out {
            // SAFETY: `fd_out` was created for this reply and is not used again.
            unsafe { libc::close(fd_out) };
        }
        if let Err(e) = sent {
            // The client abandoned the request (it timed out and reconnected)
            // or died. The buffer registered for this reply is taken straight
            // back out of the pool, so a refusal costs nothing: without this
            // the slot would stay registered for the rest of the session and
            // every collision would leak one of the pool's slots. The buffer
            // was never published, so it is kept for reuse.
            if handed_out && resp.ino != 0 {
                if let Some(removed) = pool.lock(Role::Scanout).take(resp.ino) {
                    discard_returned(removed);
                }
            }
            log_warn!("{}", format_args!("asc: scan-out channel send: {e}"));
            return ScanoutChannel::Closed;
        }
        // The handle follows its reply directly: this thread is the channel's
        // only writer, so nothing can land between the two frames.
        if want_ahb && resp.ret == 0 && !handed_ahb.is_null() {
            // SAFETY: the pool holds a reference to `handed_ahb` for as long as
            // the buffer is handed out, and `fd` is the live channel.
            let r = unsafe { ffi::AHardwareBuffer_sendHandleToUnixSocket(handed_ahb, fd) };
            if r != 0 {
                log_warn!(
                    "{}",
                    format_args!("asc: scan-out AHardwareBuffer handle send failed ({r})")
                );
                return ScanoutChannel::Closed;
            }
        }
        // The spare is one short only when the request consumed it (or a cold
        // allocation was made in its place); a request answered from the reuse
        // cache leaves the spare as it is. Raising the request is not on the
        // reply path.
        if refill_spare {
            doorbell.request((width, height, req.format));
        }
    }
}

/// Answer a [`gndrm_uapi::wire::SCANOUT_REQ_LOOKUP_INO`] request: a fresh descriptor of the
/// handed-out buffer whose dma-buf has inode `req.len`, then its
/// `AHardwareBuffer` when asked. gamescope's ICD uses it to import a guest's
/// pool buffer as an `AHardwareBuffer` on a driver that refuses the dma-buf.
/// Nothing in the pool changes. `false` when the channel broke.
fn serve_lookup(fd: RawFd, req: ScanoutAllocReq, pool: &Arc<TrackedMutex<ScanoutPool>>) -> bool {
    let ino = req.len;
    let (found, api) = {
        let guard = pool.lock(Role::Scanout);
        let found = guard.get(ino);
        if let Some(b) = found {
            // SAFETY: the pool holds a reference while the buffer is
            // registered; this one keeps it alive past the lock.
            unsafe { ffi::AHardwareBuffer_acquire(b.ahb) };
        }
        (found, guard.api())
    };
    let dup = found.and_then(|b| api.and_then(|api| dup_scanout_fd(&api, b.ahb)));
    let (resp, fd_out) = match (found, dup) {
        (Some(b), Some((dfd, dino, _))) => (
            ScanoutAllocResp {
                ret: 0,
                ino: dino,
                stride: b.stride,
            },
            Some(dfd),
        ),
        _ => (
            ScanoutAllocResp {
                ret: -libc::ENOENT,
                ino,
                stride: 0,
            },
            None,
        ),
    };
    log_info!(
        "{}",
        format_args!("asc: scan-out lookup ino {ino}: {}",
            if resp.ret == 0 {
                "found"
            } else {
                "not handed out"
            })
    );
    let mut ok = send_resp(fd, &resp.encode(), fd_out).is_ok();
    if let Some(dfd) = fd_out {
        // SAFETY: the duplicate was made for this reply and is not used again.
        unsafe { libc::close(dfd) };
    }
    if ok && resp.ret == 0 && req.wants_ahb_handle() {
        if let Some(b) = found {
            // SAFETY: `b.ahb` holds the reference acquired above; `fd` is the
            // live channel.
            let r = unsafe { ffi::AHardwareBuffer_sendHandleToUnixSocket(b.ahb, fd) };
            if r != 0 {
                log_warn!(
                    "{}",
                    format_args!("asc: scan-out lookup handle send failed ({r})")
                );
                ok = false;
            }
        }
    }
    if let Some(b) = found {
        // SAFETY: drops the reference acquired above.
        unsafe { ffi::AHardwareBuffer_release(b.ahb) };
    }
    ok
}

/// Release a payload the pool handed back, and close the release-fence
/// descriptor its mailbox still holds.
fn retire_buf(payload: PoolBuf) -> Retired {
    Retired {
        ahb: payload.ahb,
        fd: ctx_release_fd(&payload),
    }
}

/// Split the removals a retire produced into buffers to release: a buffer kept
/// for reuse only has the release fence of its finished registration closed,
/// while one that could not be reused needs its gralloc reference dropped.
/// Logs the decision for each buffer, so the steady state reads as "retained
/// for reuse" rather than a release.
fn retire_to_release(removed: &[Removed<PoolBuf>]) -> Vec<Retired> {
    let mut out = Vec::new();
    for r in removed {
        match r {
            Removed::Kept { ino, geom, payload } => {
                log_info!(
                    "{}",
                    format_args!("asc: dma-buf ino {ino} retired; retained for reuse ({}x{})",
                        geom.width,
                        geom.height)
                );
                let fd = ctx_release_fd(payload);
                if fd >= 0 {
                    // SAFETY: `fd` is the stale release fence this registration
                    // owned; it is not used again.
                    unsafe { libc::close(fd) };
                }
            }
            Removed::Released { ino, payload } => {
                log_info!(
                    "{}",
                    format_args!("asc: dma-buf ino {ino} retired; releasing its AHardwareBuffer")
                );
                out.push(retire_buf(*payload));
            }
        }
    }
    out
}

/// Handle a buffer the pool took back from an undelivered reply: a
/// never-published buffer goes to the reuse cache, anything else is released.
fn discard_returned(removed: Removed<PoolBuf>) {
    match removed {
        Removed::Kept { ino, payload, .. } => {
            log_info!(
                "{}",
                format_args!("asc: dma-buf ino {ino} returned; retained for reuse")
            );
            let fd = ctx_release_fd(&payload);
            if fd >= 0 {
                // SAFETY: a stale release fence this registration owned.
                unsafe { libc::close(fd) };
            }
        }
        Removed::Released { ino, payload } => {
            log_info!(
                "{}",
                format_args!("asc: dma-buf ino {ino} returned; releasing its AHardwareBuffer")
            );
            release_retired(core::slice::from_ref(&retire_buf(payload)));
        }
    }
}

/// Log a refused request with its precise cause and return the error reply.
///
/// A full pool, an allocator failure, a bad geometry, a disabled presenter and
/// lock contention are different faults that otherwise reach the shim as the
/// same bare "daemon allocated no buffer". The snapshot appended here names, at
/// the moment of the refusal, which thread held the pool lock and what every
/// other thread on the path was doing.
fn refuse_scanout(
    req: ScanoutAllocReq,
    errno: i32,
    waited_ms: u64,
    pool: &Arc<TrackedMutex<ScanoutPool>>,
    d: &'static Diag,
) -> (ScanoutAllocResp, Option<RawFd>) {
    let (owned, live, free) = {
        let pool = pool.lock(Role::Scanout);
        (pool.pool_owned(), pool.pool_live(), pool.pool_free())
    };
    // A length-only request has no geometry to name (it failed to resolve, or
    // resolved and then failed); report the length it carried instead.
    let what = if req.is_length_only() {
        format!("{}", format_args!("len={} (unresolved)", req.len))
    } else {
        format!("{}", format_args!("{}x{}", req.width, req.height))
    };
    log_warn!("{}", format_args!("asc: scan-out alloc {what} refused: {} (errno {errno}, waited {waited_ms} ms for the pool lock, pool={owned}/{MAX_SCANOUT_POOL} live={live} free={free}) {}",
        refusal_reason(errno),
        d.snapshot("refused")));
    (
        ScanoutAllocResp {
            ret: -errno,
            ..Default::default()
        },
        None,
    )
}

/// Ask the spare worker for a buffer of `(width, height, format)`.
///
/// Coalesced: several requests before the worker wakes leave one spare to
/// build. The worker waits here, never on the pool lock, so waking it can never
/// delay a reply.
#[derive(Default)]
pub struct WarmDoorbell {
    want: Mutex<WarmWant>,
    cv: Condvar,
}

/// Whether a spare is wanted and whether the worker should stop.
#[derive(Default)]
struct WarmWant {
    geom: Option<(u32, u32, u32)>,
    stop: bool,
}

impl WarmDoorbell {
    /// A doorbell with nothing wanted yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for a spare of `(width, height, format)`.
    fn request(&self, geom: (u32, u32, u32)) {
        let mut want = self.want.lock().unwrap_or_else(PoisonError::into_inner);
        want.geom = Some(geom);
        self.cv.notify_one();
    }

    /// Ask the worker to stop and wake it.
    pub fn stop(&self) {
        let mut want = self.want.lock().unwrap_or_else(PoisonError::into_inner);
        want.stop = true;
        self.cv.notify_all();
    }

    /// Block until a spare is wanted; `None` when the worker must exit.
    fn wait(&self) -> Option<(u32, u32, u32)> {
        let mut want = self.want.lock().unwrap_or_else(PoisonError::into_inner);
        while want.geom.is_none() && !want.stop {
            want = self.cv.wait(want).unwrap_or_else(PoisonError::into_inner);
        }
        if want.stop {
            None
        } else {
            want.geom.take()
        }
    }
}

/// Keep the pool's ready spare filled.
///
/// The spare takes `AHardwareBuffer_allocate` off the reply path entirely: a
/// request that finds one matching its geometry is answered from memory, and
/// this worker rebuilds it afterwards. It waits on its own doorbell, never on
/// the pool lock, so nothing it does can delay a reply.
pub fn warm_worker(pool: Arc<TrackedMutex<ScanoutPool>>, doorbell: Arc<WarmDoorbell>) {
    let d = diag::diag();
    loop {
        // Blocked on its own doorbell, never on the pool lock: this state is
        // what rules the spare worker in or out as a scan-out blocker.
        d.set_role(Role::Warm, State::WarmWaiting);
        let Some((width, height, format)) = doorbell.wait() else {
            break;
        };
        let api = pool.lock(Role::Warm).api();
        let Some(api) = api else {
            // Publishing is disabled; nothing can be handed out.
            continue;
        };
        d.set_role(Role::Warm, State::WarmAllocating);
        match allocate_buffer(api, width, height, format) {
            Ok((buffer, ahb)) => {
                let replaced = pool.lock(Role::Warm).install_warm(Warm {
                    width,
                    height,
                    format,
                    buffer,
                    ahb,
                });
                if let Some(old) = replaced {
                    release_warm(old);
                }
            }
            Err(_) => {
                // A transient gralloc failure must not leave the session
                // without a spare: ask again after a pause.
                thread::sleep(Duration::from_millis(WARM_RETRY_MS));
                doorbell.request((width, height, format));
            }
        }
    }
}

/// The wire and HAL format of the session's scan-out layout.
fn session_format() -> u32 {
    scanout_ahb_format(gndrm_daemon::scanout_layout())
}

/// Mismatched request formats reported so far (see [`check_request_format`]).
static FORMAT_MISMATCH_LOGGED: AtomicU32 = AtomicU32::new(0);
const FORMAT_MISMATCH_LOG_BUDGET: u32 = 4;

/// `EINVAL` unless `format` is the session layout's: the pool layout is a
/// contract with gamescope's rendering format, and another format would publish
/// channel-swapped pixels.
fn check_request_format(format: u32) -> io::Result<()> {
    let want = session_format();
    if format == want {
        return Ok(());
    }
    if FORMAT_MISMATCH_LOGGED.fetch_add(1, Ordering::Relaxed) < FORMAT_MISMATCH_LOG_BUDGET {
        log_always!(
            "{}",
            format_args!("gndrm-daemon: scan-out request format {format} refused, session layout is {want} ({})",
                gndrm_daemon::scanout_layout().as_env_value())
        );
    }
    Err(io::Error::from_raw_os_error(libc::EINVAL))
}

/// The `AHardwareBuffer` format the pool allocates for an incoming wire format.
///
/// The shim requests the session layout's format (`BGRA_8888` or `RGBA_8888`,
/// the 8-bit `XR24`/`XB24` layout gamescope renders in the SDR path). When HDR
/// output is on, gamescope scans out in a 2:10:10:10 format instead, so that
/// request is served as `R10G10B10A2_UNORM`: the pixels gamescope writes are then described
/// to SurfaceFlinger by the format they actually are. Both are 32 bpp, so the
/// stride and the pool's length/geometry keys are unchanged.
fn served_ahb_format(wire_format: u32) -> u32 {
    if wire_format == session_format() && gndrm_daemon::hdr_output_enabled() {
        ffi::AHARDWAREBUFFER_FORMAT_R10G10B10A2_UNORM
    } else {
        wire_format
    }
}

/// Allocate one scan-out `AHardwareBuffer` and resolve its client descriptor,
/// with no pool lock held.
///
/// The daemon owns the allocation because it owns the publish call (see the
/// module docs). The returned descriptor is the buffer's mappable dma-buf,
/// duplicated for the client; the inode is the registration key. `Err` is a
/// positive errno.
fn allocate_buffer(
    api: ffi::ScApi,
    width: u32,
    height: u32,
    format: u32,
) -> Result<(ScanoutBuffer, *mut ffi::AHardwareBuffer), i32> {
    let desc = ffi::AHardwareBufferDesc {
        width,
        height,
        layers: 1,
        format: served_ahb_format(format),
        usage: ffi::SCANOUT_POOL_USAGE,
        ..Default::default()
    };
    let mut ahb: *mut ffi::AHardwareBuffer = ptr::null_mut();
    // SAFETY: `desc` is a live stack value and `ahb` is an out-param this call
    // owns; on success it holds one reference.
    let rc = unsafe { ffi::AHardwareBuffer_allocate(&desc, &mut ahb) };
    if rc != 0 || ahb.is_null() {
        // Distinct from a full pool: the pool had room, the allocator itself
        // refused. Without this line both failures read the same from the
        // shim's side ("daemon allocated no buffer").
        log_warn!(
            "{}",
            format_args!("asc: AHardwareBuffer_allocate {width}x{height} failed (rc={rc})")
        );
        return Err(if rc != 0 { -rc } else { libc::ENOMEM });
    }
    let Some((fd, ino, size)) = dup_scanout_fd(&api, ahb) else {
        // SAFETY: the reference belongs to this call.
        unsafe { ffi::AHardwareBuffer_release(ahb) };
        return Err(libc::EIO);
    };
    let mut real = ffi::AHardwareBufferDesc::default();
    // SAFETY: `ahb` is a live buffer owned by this call.
    unsafe { ffi::AHardwareBuffer_describe(ahb, &mut real) };
    let stride = real.stride.saturating_mul(4);
    Ok((
        ScanoutBuffer {
            fd,
            ino,
            stride,
            size,
        },
        ahb,
    ))
}

/// Drop retired buffers' gralloc references, and close any descriptor that came
/// with them.
///
/// Runs outside the pool lock: this is the call whose latency must never sit
/// between the allocator and its reply.
fn release_retired(retired: &[Retired]) {
    for r in retired {
        if r.fd >= 0 {
            // SAFETY: `r.fd` is a descriptor owned by this call.
            unsafe { libc::close(r.fd) };
        }
        // SAFETY: the pool owned one reference on `r.ahb`.
        unsafe { ffi::AHardwareBuffer_release(r.ahb) };
    }
}

/// Release a spare that is being discarded (replaced, or dropped at teardown).
fn release_warm(warm: Warm) {
    if warm.buffer.fd >= 0 {
        // SAFETY: the spare owned this descriptor.
        unsafe { libc::close(warm.buffer.fd) };
    }
    // SAFETY: the spare owned one reference on the buffer.
    unsafe { ffi::AHardwareBuffer_release(warm.ahb) };
}

/// Human label for a refusal, so the shim's bare "daemon allocated no buffer"
/// can be traced back to the actual fault in the daemon log. Owned, because it
/// is composed into the caller's line (cold path: only on a refusal).
fn refusal_reason(errno: i32) -> String {
    match errno {
        libc::ENOSPC => "pool exhausted".to_string(),
        libc::ENOSYS => "presenter disabled (no publish API)".to_string(),
        libc::EINVAL => "unsupported geometry or format".to_string(),
        _ => "AHardwareBuffer allocation failed".to_string(),
    }
}

/// One show transaction's layer description: the FB source and CRTC
/// destination rects, plus the dataspace the published buffer carries (BT.2020
/// /ST.2084 for a 10-bit HDR scan-out buffer, unspecified for 8-bit).
#[derive(Clone, Copy)]
struct ShowRect {
    src: ffi::ARect,
    dst: ffi::ARect,
    dataspace: c_int,
}

/// Create a transaction that puts `ahb` on `sc` with the given geometry, but do
/// not apply it: the caller drops the pool lock before applying, because
/// `ASurfaceTransaction_apply` is the blocking SurfaceFlinger round-trip.
///
/// `fence` ownership moves into the transaction (`setBuffer` consumes it); the
/// caller must close it when this returns `None`.
fn build_show(
    api: &ffi::ScApi,
    sc: *mut ffi::ASurfaceControl,
    ahb: *mut ffi::AHardwareBuffer,
    ctx: *mut ReleaseCtx,
    rect: ShowRect,
    fence: Option<RawFd>,
) -> Option<*mut ffi::ASurfaceTransaction> {
    let ShowRect {
        src,
        dst,
        dataspace,
    } = rect;
    // SAFETY: one transaction per commit, deleted by the caller's
    // `apply_transaction`; all handles come from the API table and are non-null
    // by construction.
    unsafe {
        let tx = (api.st_create)();
        if tx.is_null() {
            return None;
        }
        (api.st_set_zorder)(tx, sc, 0);
        (api.st_set_visibility)(tx, sc, 1);
        (api.st_set_color)(tx, sc, 0.0, 0.0, 0.0, 1.0, ffi::ADATASPACE_UNKNOWN);
        // The scan-out buffer's alpha is not meaningful: the layer is the whole
        // frame, composited opaque, and gamescope's composite shaders are
        // written for a DRM plane whose X-prefixed fourcc ignores alpha (a pass
        // may store alpha 0 — cs_composite_rcas.comp does — without blanking
        // the frame). The pool's AHardwareBuffer is BGRA_8888 or RGBA_8888,
        // whose alpha SF would otherwise honour, so assert opacity here. See
        // ffi.rs.
        (api.st_set_buffer_transparency)(tx, sc, ffi::ASURFACE_TRANSACTION_TRANSPARENCY_OPAQUE);
        (api.st_set_geometry)(tx, sc, &src, &dst, 0);
        // setBuffer consumes the acquire fence: SurfaceFlinger closes it,
        // so it must not be closed here as well.
        (api.st_set_buffer)(tx, sc, ahb, fence.unwrap_or(-1));
        // The dataspace describes the buffer just set: BT.2020/ST.2084 (PQ) for
        // the 10-bit HDR scan-out buffer, ADATASPACE_UNKNOWN (SF's sRGB default)
        // for the 8-bit one. setColor's dataspace above only applies to the
        // background colour layer, not the buffer.
        (api.st_set_buffer_dataspace)(tx, sc, dataspace);
        // PQ on the layer is not enough for the display to enter its HDR mode:
        // SurfaceFlinger reports no HDR layer present and converts the frame to
        // BT709/sRGB for an SDR output, which clips it. The HDR static metadata
        // is what marks the layer HDR to the hardware composer, so it goes with
        // the buffer, in the same transaction.
        // PQ on the layer is not enough for the display to enter its HDR mode:
        // SurfaceFlinger reports no HDR layer present and converts the frame to
        // BT709/sRGB for an SDR output, which clips it. The HDR static metadata
        // is what marks the layer HDR to the hardware composer, so it goes with
        // the buffer, in the same transaction.
        //
        // It describes the panel, not the frame, so it is set when the layer
        // enters PQ and left alone until it leaves. Re-issuing it per commit
        // cost two binder transactions per frame for values the composer
        // already had; leaving PQ re-arms it, so an HDR toggle still lands.
        if dataspace == ffi::ADATASPACE_BT2020_PQ {
            if !PQ_METADATA_SET.load(core::sync::atomic::Ordering::Relaxed) {
                // UNGATED one-shot: whether the HDR metadata actually went out is
                // the difference between "the composer ignored it" and "we never
                // called", and no other probe can see that.
                static ONCE: core::sync::atomic::AtomicBool =
                    core::sync::atomic::AtomicBool::new(false);
                if !ONCE.swap(true, core::sync::atomic::Ordering::Relaxed) {
                    asc_log!(
                        "{}",
                        format_args!("gndrm-daemon: asc hdr metadata cta861_3={} smpte2086={} cap={}",
                            api.st_set_hdr_metadata_cta861_3.is_some(),
                            api.st_set_hdr_metadata_smpte2086.is_some(),
                            gndrm_daemon::hdr_output_capability().is_some())
                    );
                }
                if let Some(cap) = gndrm_daemon::hdr_output_capability() {
                    if let Some(set_cta) = api.st_set_hdr_metadata_cta861_3 {
                        let md = ffi::AHdrMetadataCta8613 {
                            max_content_light_level: cap.max_nits,
                            max_frame_average_light_level: cap.fall_nits,
                        };
                        (set_cta)(tx, sc, &md);
                    }
                    if let Some(set_smpte) = api.st_set_hdr_metadata_smpte2086 {
                        // BT.2020 primaries and D65: the colour volume the PQ
                        // buffer is encoded against, with the panel's own
                        // luminances.
                        let md = ffi::AHdrMetadataSmpte2086 {
                            display_primary_red: ffi::AColorXy { x: 0.708, y: 0.292 },
                            display_primary_green: ffi::AColorXy { x: 0.170, y: 0.797 },
                            display_primary_blue: ffi::AColorXy { x: 0.131, y: 0.046 },
                            white_point: ffi::AColorXy {
                                x: 0.3127,
                                y: 0.3290,
                            },
                            max_luminance: cap.max_nits,
                            min_luminance: cap.min_nits,
                        };
                        (set_smpte)(tx, sc, &md);
                    }
                }
                PQ_METADATA_SET.store(true, core::sync::atomic::Ordering::Relaxed);
            }
        } else {
            PQ_METADATA_SET.store(false, core::sync::atomic::Ordering::Relaxed);
        }
        // The completion reports the previous release fence, which is what
        // gates re-publishing this buffer.
        (api.st_set_on_complete)(tx, ctx.cast(), on_complete);
        Some(tx)
    }
}

/// Create a transaction that hides `sc` (plane disabled), unapplied. `fence`
/// ownership stays with the caller.
fn build_hide(
    api: &ffi::ScApi,
    sc: *mut ffi::ASurfaceControl,
) -> Option<*mut ffi::ASurfaceTransaction> {
    // SAFETY: as in `build_show`.
    unsafe {
        let tx = (api.st_create)();
        if tx.is_null() {
            return None;
        }
        (api.st_set_zorder)(tx, sc, 0);
        (api.st_set_visibility)(tx, sc, 0);
        Some(tx)
    }
}

/// Marks `fd` as the served connection for as long as this loop runs, so a push
/// that fails against it is left for the loop to close rather than closed twice.
struct ServedGuard(RawFd);

impl Drop for ServedGuard {
    fn drop(&mut self) {
        gndrm_daemon::pushes::clear_served(self.0);
    }
}

/// Apply and release a transaction from [`build_show`]/[`build_hide`]. Kept
/// separate from the build so the caller can drop the pool lock first: this is
/// the SurfaceFlinger round-trip that must not serialise the allocator.
fn apply_transaction(api: &ffi::ScApi, tx: *mut ffi::ASurfaceTransaction) {
    // SAFETY: `tx` is a live transaction owned by this call.
    unsafe {
        (api.st_apply)(tx);
        (api.st_delete)(tx);
    }
}

/// Duplicate the mappable dma-buf of `ahb` and read its identity.
///
/// `fd[0]` of the native handle is the buffer's dma-buf (measured on device,
/// doc `android/06`); its `stat.st_ino` is the same identity the GEM store
/// dedups `PRIME_FD_TO_HANDLE` imports by, which is how a committed
/// framebuffer later resolves back to this buffer.
fn dup_scanout_fd(api: &ffi::ScApi, ahb: *mut ffi::AHardwareBuffer) -> Option<(RawFd, u64, u64)> {
    // SAFETY: `ahb` is a live buffer owned by this call; the native handle it
    // returns is borrowed and stays valid while `ahb` does.
    let nh = unsafe { (api.ahb_get_native_handle)(ahb) };
    if nh.is_null() {
        return None;
    }
    // SAFETY: `nh` is a live native handle for the whole scan.
    let fd0 = unsafe { scanout_dmabuf_fd(nh) }?;
    let (ino, size) = fstat_ino_size(fd0)?;
    // The client owns the descriptor it is given, so it must be a fresh one.
    // SAFETY: dup of a descriptor this call can read; the result is owned here.
    let fd = unsafe { libc::fcntl(fd0, libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return None;
    }
    Some((fd, ino, size))
}

/// `DMA_BUF_MAGIC` (`<linux/magic.h>`), the `f_type` of every dma-buf.
const DMA_BUF_MAGIC: u64 = 0x444d_4142;

/// Whether `fd` is a dma-buf: its filesystem magic, or, where `fstatfs` is
/// refused (the app's SELinux domain on some vendor builds), the name the
/// kernel gives a dma-buf file (`/dmabuf:…`, `anon_inode:dmabuf` before 5.x).
fn is_dmabuf(fd: c_int) -> bool {
    let mut st: libc::statfs = unsafe { core::mem::zeroed() };
    // SAFETY: fstatfs on a descriptor the caller can read; `st` is written on success.
    if unsafe { libc::fstatfs(fd, &mut st) } == 0 && st.f_type as u64 == DMA_BUF_MAGIC {
        return true;
    }
    fd_link(fd).contains("dmabuf")
}

/// The `/proc/self/fd` link of `fd`, empty when it cannot be read.
fn fd_link(fd: c_int) -> String {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The descriptor of `nh` that is the buffer's dma-buf. Usually the first one,
/// but not on every gralloc: MediaTek's handles lead with a descriptor shared
/// by every buffer, which is not a dma-buf at all — its inode is the same for
/// every buffer, it has no length, and polling it for the compositor's
/// implicit-sync wait never reports the frame ready. When the first descriptor
/// is not a dma-buf, the largest dma-buf in the handle is taken (a handle can
/// also carry a small metadata dma-buf), else the descriptor with the largest
/// length, else the first.
///
/// # Safety
/// `nh` must be a live native handle.
unsafe fn scanout_dmabuf_fd(nh: *const ffi::NativeHandle) -> Option<c_int> {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    // SAFETY: the caller guarantees `nh` is live; `fd` bounds the index by
    // `num_fds`.
    let fds: Vec<c_int> = (0..8)
        .map_while(|i| unsafe { ffi::NativeHandle::fd(nh, i) })
        .collect();
    let fd0 = *fds.first()?;
    let largest = |want_dmabuf: bool| {
        fds.iter()
            .enumerate()
            .filter(|&(_, &fd)| !want_dmabuf || is_dmabuf(fd))
            .map(|(i, &fd)| (i, fd, dmabuf_len(fd)))
            .filter(|&(_, _, len)| want_dmabuf || len > 0)
            .fold(None, |best: Option<(usize, c_int, u64)>, cur| match best {
                Some(b) if b.2 >= cur.2 => Some(b),
                _ => Some(cur),
            })
    };
    let pick = if is_dmabuf(fd0) {
        (0, fd0)
    } else {
        largest(true)
            .or_else(|| largest(false))
            .map_or((0, fd0), |(i, fd, _)| (i, fd))
    };
    if !LOGGED.swap(true, Ordering::Relaxed) {
        let table = fds
            .iter()
            .enumerate()
            .map(|(i, &fd)| {
                let (ino, size) = fstat_ino_size(fd).unwrap_or((0, 0));
                format!(
                    "{i}:ino={ino},size={size},len={},dmabuf={},link={}",
                    dmabuf_len(fd),
                    is_dmabuf(fd),
                    fd_link(fd)
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        log_always!(
            "{}",
            format_args!("asc: scan-out handle descriptors [{table}]; handing out fd {}",
                pick.0)
        );
    }
    Some(pick.1)
}

/// Length of `fd`: `st_size`, or for a dma-buf the end offset where a kernel
/// leaves the inode size at zero. 0 when neither is known.
fn dmabuf_len(fd: c_int) -> u64 {
    let size = fstat_ino_size(fd).map_or(0, |(_, s)| s);
    if size != 0 || !is_dmabuf(fd) {
        return size;
    }
    // SAFETY: lseek on a descriptor the caller can read; a dma-buf only moves
    // its own offset, which no reader of this descriptor depends on.
    let end = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    if end > 0 {
        // SAFETY: as above.
        unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
        end as u64
    } else {
        0
    }
}

/// `(st_ino, st_size)` of `fd`, which for a dma-buf is the kernel's per-buffer
/// identity and the real allocation length.
fn fstat_ino_size(fd: c_int) -> Option<(u64, u64)> {
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: fstat on a descriptor we own; `st` is fully written on success.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return None;
    }
    Some((st.st_ino as u64, st.st_size as u64))
}

/// Receive one `SOCK_SEQPACKET` message into `buf`: its length and any single
/// descriptor it carried. A zero length means the peer closed.
fn recv_msg(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, Option<RawFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cbuf = [0u8; 64];
    // SAFETY: `fd` is a live socket, `iov` points at `buf`, and `cbuf` is a
    // zeroed control buffer sized for one SCM_RIGHTS descriptor.
    let (n, msg) = unsafe {
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = cbuf.len();
        (libc::recvmsg(fd, &mut msg, 0), msg)
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Ok((0, None));
    }
    // SAFETY: `msg` was filled by the successful recvmsg above.
    let mut out_fd: Option<RawFd> = None;
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg.is_null() {
        // SAFETY: `cmsg` comes from the kernel-parsed control buffer.
        unsafe {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let mut fd_in: RawFd = -1;
                core::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(cmsg).cast::<RawFd>(),
                    &mut fd_in,
                    1,
                );
                if let Some(previous) = out_fd.replace(fd_in) {
                    // More than one descriptor: keep the first, do not leak the
                    // rest.
                    libc::close(previous);
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((n as usize, out_fd))
}

/// `sendmsg(2)` one message, optionally attaching one descriptor via
/// `SCM_RIGHTS` (ownership of `fd_out` transfers to the peer on success).
///
/// `MSG_NOSIGNAL`: the peer may already have timed out and reconnected (or
/// died) — the send then fails with `EPIPE`, which must surface as a return
/// value the caller can recover from, not a `SIGPIPE` that takes the daemon
/// down with it.
fn send_resp(fd: RawFd, bytes: &[u8], fd_out: Option<RawFd>) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut cbuf = [0u8; 64];
    // SAFETY: `fd` is a live socket; the control buffer is only touched when a
    // descriptor is attached and is `CMSG_SPACE`-sized for exactly one.
    let n = unsafe {
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if let Some(f) = fd_out {
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(4) as usize;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(4) as usize;
            core::ptr::write(libc::CMSG_DATA(cmsg).cast::<RawFd>(), f);
        }
        libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL)
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Close a fence the presenter was given but did not pass on to a consumer.
/// Negative values are "no fence".
fn close_fence(fence: Option<RawFd>) {
    if let Some(fd) = fence.filter(|fd| *fd >= 0) {
        // SAFETY: ownership of the fence moved to the presenter.
        unsafe { libc::close(fd) };
    }
}

/// A snapshot of the session's frame cap — the target the scan-out channel
/// hands and pushes to the shim.
fn cap_now() -> FrameCap {
    *crate::FRAME_CAP
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Feed the app's frame ring from one producer report.
///
/// The count is what the session *produced* — frames the guest acquired plus the
/// ones frame generation inserted — which is the number the display cannot always
/// take. The publish path counts what the display actually latched, and that can
/// never exceed its rate.
pub(crate) fn note_produced(acq: FrameAcquired) {
    let previous = ACQUIRED_SEEN.swap(acq.count, Ordering::Relaxed);
    let generated_seen = GENERATED_SEEN.swap(acq.generated, Ordering::Relaxed);
    LAST_PRODUCER_REPORT_MS.store(monotonic_ms(), Ordering::Relaxed);
    if previous == 0 && acq.count != 0 {
        log_info!(
            "{}",
            format_args!("asc: acquire counter connected (count={})", acq.count)
        );
        // The frames behind this first total were already counted by the
        // publish path while no reports flowed; counting them again here would
        // put a one-off jump in the ring.
        return;
    }
    // A total that went backwards is a new producer on this channel, so
    // `saturating_sub` reports all of it as new. Bounded: one message is sent per
    // acquired frame, so the plausible delta is one frame plus the multiplier,
    // and a corrupt value must not spin here.
    let produced = acq
        .count
        .saturating_sub(previous)
        .saturating_add(acq.generated.saturating_sub(generated_seen));
    let ones = produced.min(MAX_PRODUCED_PER_MESSAGE);
    for _ in 0..ones {
        crate::notify_frame_published();
    }
    if produced > ones {
        log_warn!(
            "{}",
            format_args!("asc: produced delta {produced} clamped to {ones} for the frame ring")
        );
    }
}

/// Read what a connection that lost the allocator role still sends, and `false`
/// when it has gone away.
///
/// A subscription outlives that role, so the pushes reach a retired connection —
/// but its reports have to be read by someone too, and the loop that used to read
/// them has returned. Two kinds matter: the producer's running totals, which feed
/// the app's frame ring, and a present mark, which answers a guest's present wait.
/// Allocation is not served here: the pool belongs to whichever connection holds
/// it, so an alloc request on a retired connection is left to the shim's own
/// timeout, exactly as it was before any of this.
pub(crate) fn drain_retired(fd: RawFd) -> bool {
    // Every inbound type, not just the alloc request: a short buffer on a
    // SEQPACKET connection truncates the frame instead of failing it.
    let mut buf = [0u8; gndrm_uapi::wire::INBOUND_MAX];
    // SAFETY: `fd` is a connection this module owns until it is forgotten, and
    // `buf` is one full frame; DONTWAIT leaves an unarrived frame for later.
    let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
    if n == 0 {
        return false;
    }
    if n < 0 {
        let e = io::Error::last_os_error();
        // EAGAIN and EWOULDBLOCK are the same value on Linux.
        return e.raw_os_error() == Some(libc::EAGAIN);
    }
    let n = n as usize;
    if let Some(acq) = FrameAcquired::decode(&buf[..n]) {
        note_produced(acq);
    } else if let Some(mark) = PresentMark::decode(&buf[..n]) {
        note_present_mark(mark);
    } else if ScanoutAllocReq::decode(&buf[..n]).is_some() {
        // An allocation on a retired connection. The pool belongs to the
        // connection in service, so this one cannot be answered here — and
        // leaving it unanswered is what hangs the guest: it blocks until its
        // exchange times out, retries on the same dead channel, and a title
        // recreating its swapchain (a vsync toggle, a resolution change) never
        // finishes. Closing the connection instead makes the shim see a dead
        // channel, reconnect, and take the allocator back — and its connect
        // re-subscribes, so the pushes resume on the new descriptor.
        return false;
    }
    true
}

/// Push the current frame cap, and the frame-generation settings that ride with
/// it, to every subscriber. Both go out because one wake cannot say which of
/// them moved.
///
/// Every subscriber, not only the connection in service: the guest that gates on
/// the target may have handed the allocator role on long ago (`pushes`).
pub(crate) fn flush_cap(cap_fd: RawFd) {
    drain_eventfd(cap_fd);
    gndrm_daemon::pushes::push(CAP_FRAME_CAP, &cap_now().note().encode());
    if let Some(settings) = crate::lsfg_settings() {
        gndrm_daemon::pushes::push(CAP_FRAME_CAP, &settings.encode());
    }
    sync_vsync_subscription();
}

/// Push the panel's latch grid to every subscriber that asked for it.
pub(crate) fn flush_vsync(vsync_fd: RawFd) {
    drain_eventfd(vsync_fd);
    if let Some(note) = crate::vsync::note() {
        gndrm_daemon::pushes::push(CAP_VSYNC_NOTE, &note.encode());
    }
    sync_vsync_subscription();
}

/// Tell the latch-grid tracker whether anyone still reads its note. Called from
/// every place the subscriber set can change — a subscribe, a push that reaped a
/// peer that had gone, a retirement, a teardown — because `pushes` is the only
/// place that knows, and `vsync` is deliberately crate-root-free.
pub(crate) fn sync_vsync_subscription() {
    crate::vsync::set_subscribed(gndrm_daemon::pushes::wants(CAP_VSYNC_NOTE));
}

/// Drain the cap-change eventfd, so a count left behind cannot wake the scan-out
/// channel again on the next poll.
/// An eventfd for a wake the callback thread can raise: non-blocking, so a
/// burst of completions cannot park the SurfaceFlinger thread in a write.
fn eventfd_new() -> RawFd {
    // SAFETY: a fresh eventfd with no shared state.
    unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) }
}

/// Owns the present eventfd for one served connection: closing it also clears
/// the connection-scoped state, so a later connection starts unaware.
struct PresentChannel(RawFd);

impl Drop for PresentChannel {
    fn drop(&mut self) {
        PRESENT_FD.store(-1, Ordering::Relaxed);
        PRESENT_AWARE.store(false, Ordering::Relaxed);
        // SAFETY: the descriptor is this struct's and is closed once.
        unsafe { libc::close(self.0) };
    }
}

fn drain_eventfd(fd: RawFd) {
    let mut buf = [0u8; 8];
    // SAFETY: `fd` is the cap-change eventfd, created non-blocking; `buf` is an
    // 8-byte counter buffer. A negative `fd` (no eventfd) is a no-op.
    unsafe {
        libc::read(fd, buf.as_mut_ptr().cast(), buf.len());
    }
}

/// Poll `fd` until it signals; `true` when the frame budget expired first.
fn wait_fence(fd: RawFd) -> bool {
    wait_fence_for(fd, RELEASE_FENCE_TIMEOUT_MS)
}

/// Poll `fd` for up to `timeout_ms`; `true` when the budget expired first.
fn wait_fence_for(fd: RawFd, timeout_ms: c_int) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd for a duration.
    let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    n == 0
}
