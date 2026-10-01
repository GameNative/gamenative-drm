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
use std::collections::HashSet;
use std::io;
use std::os::fd::RawFd;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use gndrm_daemon::diag::{self, Diag, Reply, Role, Stage, State, TrackedMutex, LOCK_SCANOUT_POOL};
use gndrm_daemon::scanout::{Geometry, Removed, Reservation as CacheReservation, ReusePool};
use gndrm_daemon::{FlipHandle, FlipLatch, Framebuffer, PlaneState, Presenter};
use gndrm_uapi::wire::{ScanoutAllocReq, ScanoutAllocResp, SCANOUT_FORMAT_BGRA_8888};

use crate::ffi;
use crate::{log_info, log_warn};

/// Budget for the previous release fence (a 30 Hz frame). A fence that never
/// signals must not stall the flip loop, so the frame is published anyway and
/// the timeout is logged once.
const RELEASE_FENCE_TIMEOUT_MS: c_int = 33;

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

/// Debug name of the layer; shows up in SurfaceFlinger dumps.
const SC_DEBUG_NAME: &core::ffi::CStr = c"gndrm-scanout";

/// One scan-out buffer the daemon allocated, on its way to the client.
pub struct ScanoutBuffer {
    /// Fresh descriptor the caller owns and the shim hands to gamescope.
    pub fd: RawFd,
    /// `stat.st_ino` of `fd`: the identity a committed framebuffer resolves to.
    pub ino: u64,
    /// Row stride in bytes (`AHardwareBuffer_Desc::stride` counts pixels for a
    /// 32-bit format, and DRM pitches are bytes).
    pub stride: u32,
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
    /// Layer the fence is asked about; fixed for the presenter's lifetime.
    sc: *mut ffi::ASurfaceControl,
    previous_release: ffi::FnStStatsGetPreviousReleaseFenceFd,
    /// fd of the previous release fence, `-1` when none is waiting.
    release_fd: AtomicI32,
    /// The latch for the publish currently in flight through this slot. The
    /// completion callback fires it, which is the signal the worker waits on
    /// before emitting the flip event.
    latch: Mutex<Option<FlipLatch>>,
    /// Set by the completion callback: SurfaceFlinger reported the flip event
    /// for this registration, so the buffer is safe to hand back out (the
    /// reuse cache keys on it). Never cleared: the flag belongs to this
    /// registration, and a registration is not reused.
    latched: Arc<AtomicBool>,
}

// SAFETY: after construction the only mutation is the atomic and the latch
// slot (a `Mutex`), and the other fields are immutable — so the mailbox is
// safe to share with the callback thread.
unsafe impl Send for ReleaseCtx {}
unsafe impl Sync for ReleaseCtx {}

/// `ASurfaceTransaction_setOnComplete`: capture the fence that says
/// SurfaceFlinger has finished with the buffer published in the previous
/// transaction. First writer wins; a second fence arriving before the present
/// path consumed the first describes the same slot and is closed.
unsafe extern "C" fn on_complete(context: *mut c_void, stats: *mut ffi::ASurfaceTransactionStats) {
    if context.is_null() {
        return;
    }
    // Publish the callback's activity: the completion is what fires the latch
    // the commit worker waits on, so a snapshot must be able to say whether a
    // callback was running when a request stalled.
    diag::diag().set_role(Role::PresenterCallback, State::PresenterCallback);
    // SAFETY: `context` is the leaked ReleaseCtx this presenter handed to
    // setOnComplete; it outlives every callback by construction.
    let ctx = unsafe { &*context.cast::<ReleaseCtx>() };
    // SAFETY: `stats` belongs to this completion, `ctx.sc` is the layer that
    // was published in the transaction it reports.
    let fd = unsafe { (ctx.previous_release)(stats, ctx.sc) };
    if fd >= 0
        && ctx
            .release_fd
            .compare_exchange(-1, fd, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        // SAFETY: `fd` is a valid descriptor owned by this call.
        unsafe { libc::close(fd) };
    }
    // The transaction completed: SurfaceFlinger has reported this publish's
    // completion. Publish that as the reuse gate before firing the latch — a
    // retirement that observes the flag may then cache the buffer.
    ctx.latched.store(true, Ordering::Release);
    // Fire the latch the worker waits on for this publish. The worker is
    // serial, so only one latch is ever in flight; a stray callback finds an
    // empty slot and does nothing.
    if let Some(latch) = ctx
        .latch
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    {
        latch.fire();
    }
    diag::diag().set_role(Role::PresenterCallback, State::Idle);
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
    /// Child layer of the app's window; null while publishing is disabled.
    sc: *mut ffi::ASurfaceControl,
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
    fn new(api: Option<ffi::ScApi>, sc: *mut ffi::ASurfaceControl) -> Self {
        Self {
            api,
            sc,
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
        if format != SCANOUT_FORMAT_BGRA_8888 {
            // The pool layout is a contract with gamescope's rendering format
            // (B,G,R,X); another format would publish channel-swapped pixels.
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
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

    /// Resolve a length-only request to a geometry the pool knows, then reserve
    /// its answer. `EINVAL` when the length resolves to no known geometry (not
    /// a scan-out allocation: the shim leaves it to the kernel), and whatever
    /// [`Self::begin_alloc`] returns otherwise.
    ///
    /// # Errors
    /// `EINVAL` for an unknown length or a format the pool does not serve,
    /// `ENOSYS`/`ENOSPC` as [`Self::begin_alloc`].
    fn begin_alloc_len(&mut self, len: u64, format: u32) -> io::Result<(u32, u32, Reservation)> {
        if format != SCANOUT_FORMAT_BGRA_8888 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let Some(geom) = self.cache.resolve_len(len, format) else {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        };
        let reservation = self.begin_alloc(geom.width, geom.height, format)?;
        Ok((geom.width, geom.height, reservation))
    }

    /// Record a framebuffer's geometry, so a length-only request for the same
    /// window size resolves to it. Fed from two places: `publish` (every
    /// committed framebuffer the presenter sees) and the daemon's `ADDFB2`
    /// path, which reaches this sooner and for framebuffers that are never
    /// published as the primary plane.
    pub(crate) fn note_framebuffer(&mut self, width: u32, height: u32) {
        self.cache.note_geometry(Geometry {
            width,
            height,
            format: SCANOUT_FORMAT_BGRA_8888,
        });
    }

    /// Put a reused buffer back after its client descriptor could not be
    /// duplicated, so the pool does not lose the buffer. `ino` is the identity
    /// the free-list entry was registered under.
    fn recycle(&mut self, geom: Geometry, ino: u64, payload: PoolBuf) {
        self.cache.recycle(geom, ino, payload);
    }

    /// A fresh completion mailbox for one registration, with the reuse gate
    /// the completion callback publishes into.
    fn new_ctx(&self) -> (*mut ReleaseCtx, Arc<AtomicBool>) {
        let api = self
            .api
            .expect("a slot is only committed when the presenter is enabled");
        let latched = Arc::new(AtomicBool::new(false));
        let ctx = Box::into_raw(Box::new(ReleaseCtx {
            sc: self.sc,
            previous_release: api.st_stats_previous_release_fence_fd,
            release_fd: AtomicI32::new(-1),
            latch: Mutex::new(None),
            latched: Arc::clone(&latched),
        }));
        (ctx, latched)
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
            format: SCANOUT_FORMAT_BGRA_8888,
        };
        let (ctx, latched) = self.new_ctx();
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
        log_info!(
            "asc: scan-out {width}x{height} {origin} (ino={}, stride={}, pool={}/{MAX_SCANOUT_POOL})",
            buffer.ino,
            buffer.stride,
            self.owned()
        );
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
    /// (released outside the lock).
    fn install_warm(&mut self, warm: Warm) -> Option<Warm> {
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

/// The last buffer actually put on the layer, with the geometry it went out
/// with. A commit for an inode that is not registered re-publishes this rather
/// than leaving the layer empty: SurfaceFlinger compositing nothing drops the
/// app's own background through, which reads as a blank panel.
#[derive(Clone, Copy)]
struct LastPublish {
    ino: u64,
    src: ffi::ARect,
    dst: ffi::ARect,
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
    panel_w: c_int,
    panel_h: c_int,
    /// The scan-out buffer pool, on its own lock (see [`ScanoutPool`]). The
    /// lock is tracked (see `gndrm_daemon::diag`): a request that stalls on it
    /// must be able to name the thread holding it.
    pool: Arc<TrackedMutex<ScanoutPool>>,
    last_publish: Option<LastPublish>,
    frames: u64,
    warned_fence_timeout: bool,
    /// Inodes already reported as having no registered buffer (log once each).
    logged_unregistered: Vec<u64>,
    /// Inodes already reported as re-publishing a stale buffer (log once each).
    logged_stale: Vec<u64>,
}

// SAFETY: the state is only ever reached through the `Arc<Mutex<_>>` shared by
// the present path and the scan-out channel thread. The raw pointers it holds
// are SurfaceFlinger objects (thread-safe by contract) and the native window is
// not kept — it is used once, on the thread that owns it, to create the child
// layer.
unsafe impl Send for AscState {}

impl AscState {
    /// Create the child layer under `window`. Never fails: a missing publish
    /// surface or a null layer leaves the presenter disabled, which drops
    /// frames instead of failing the session.
    #[must_use]
    pub fn new(window: *mut ffi::ANativeWindow, panel_w: i32, panel_h: i32) -> Self {
        let loaded = ffi::ScApi::load();
        // SAFETY: `window` is a live ANativeWindow owned by the caller for the
        // whole session; createFromWindow only reads it.
        let sc = loaded.map_or(ptr::null_mut(), |api| unsafe {
            (api.sc_create_from_window)(window, SC_DEBUG_NAME.as_ptr())
        });
        let api = if sc.is_null() { None } else { loaded };
        if loaded.is_none() {
            log_warn!("asc: SurfaceControl API unavailable (pre-API-29?); publishing disabled");
        } else if sc.is_null() {
            log_warn!("asc: ASurfaceControl_createFromWindow failed; publishing disabled");
        } else {
            log_info!("asc: SurfaceControl layer created ({panel_w}x{panel_h} panel)");
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
            panel_w,
            panel_h,
            pool: Arc::new(TrackedMutex::new(
                diag::diag(),
                LOCK_SCANOUT_POOL,
                ScanoutPool::new(api, sc),
            )),
            last_publish: None,
            frames: 0,
            warned_fence_timeout: false,
            logged_unregistered: Vec::new(),
            logged_stale: Vec::new(),
        };
        // A layer is created empty, and an empty layer makes SurfaceFlinger
        // composite nothing. Everything this session knows lives in the
        // allocator pool, which is empty at this point, so there is normally
        // nothing to put up yet — re-publishing is a no-op here and only does
        // work on a path that re-creates the layer over an existing table.
        // The panel mode is a geometry the daemon can serve from a bare length,
        // so a mode-sized scan-out allocation resolves by length even before a
        // framebuffer of that size is committed.
        if panel_w > 0 && panel_h > 0 {
            state
                .pool
                .lock(Role::Commit)
                .note_framebuffer(panel_w as u32, panel_h as u32);
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

    /// Publish the committed primary-plane framebuffer, returning the handle
    /// the worker waits on before emitting the flip event.
    ///
    /// `ino` is the buffer identity the device resolved from the object store;
    /// `live_inos` is the set of still-imported identities the pool retire keys
    /// on.
    pub fn publish(
        &mut self,
        fb: &Framebuffer,
        plane: &PlaneState,
        ino: Option<u64>,
        live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    ) -> FlipHandle {
        let Some(api) = self.api else {
            close_fence(fence);
            return FlipHandle::latched_now();
        };
        if self.sc.is_null() {
            close_fence(fence);
            return FlipHandle::latched_now();
        }
        let Some(ino) = ino else {
            close_fence(fence);
            return FlipHandle::latched_now();
        };

        // KMS geometry: the FB's SRC rect (16.16 fixed point) is scaled into
        // the CRTC rect, and the output is black outside it. The layer is
        // sized to the CRTC rect only, so the app's window shows through
        // around it, and the layer's background covers transparent regions
        // inside it (gamescope letterboxes with transparent bars).
        let src = ffi::ARect {
            left: (plane.src_x >> 16) as c_int,
            top: (plane.src_y >> 16) as c_int,
            right: ((plane.src_x + plane.src_w) >> 16) as c_int,
            bottom: ((plane.src_y + plane.src_h) >> 16) as c_int,
        };
        // KMS clips the plane to the mode: the destination rect is the CRTC
        // rect bounded by the panel.
        let dst = ffi::ARect {
            left: plane.crtc_x.max(0),
            top: plane.crtc_y.max(0),
            right: (plane.crtc_x + plane.crtc_w).min(self.panel_w),
            bottom: (plane.crtc_y + plane.crtc_h).min(self.panel_h),
        };
        let drawable = src.right > src.left
            && src.bottom > src.top
            && dst.right > dst.left
            && dst.bottom > dst.top;
        let keep = self.last_publish.map(|lp| lp.ino);

        // The pool lock is held only for map work: retiring the buffers the
        // client stopped importing (into the reuse cache) and resolving the
        // payload this commit publishes. Every gralloc call, the looper/fence
        // service and the transaction build/apply run after it is dropped, so a
        // commit — including a retirement burst — can never queue a scan-out
        // reply behind slow work.
        let (resolved, removed, misses) = {
            let mut pool = self.pool.lock(Role::Commit);
            // Teach the pool this framebuffer's geometry so a later
            // length-only scan-out request for the same window size resolves to
            // it — the window geometries the shim cannot name arrive this way.
            pool.note_framebuffer(fb.width, fb.height);
            let removed = pool.retire(live_inos, keep);
            // Resolve the buffer this commit publishes. Both lookups copy out
            // the payload, so neither borrow outlives its statement and the
            // fallback path can consult the cache again.
            let resolved = pool.resolve(ino).map(|buf| (buf, ino, false));
            let resolved = resolved.or_else(|| {
                self.last_publish
                    .and_then(|lp| pool.get(lp.ino).map(|buf| (buf, lp.ino, true)))
            });
            (resolved, removed, pool.pool_misses())
        };

        // Retain the retired buffers whose flip event has been reported for
        // reuse, and release the rest: a gralloc call with no latency bound,
        // explicitly outside the pool lock.
        let released = retire_to_release(&removed);
        release_retired(&released);

        let handle = match resolved {
            None => {
                // No buffer for this inode. Either the shim passed the
                // allocation through to the kernel, or the table was rebuilt
                // under a still-running client. There is no last buffer to fall
                // back to, so the frame is dropped and the fence released.
                if !self.logged_unregistered.contains(&ino) {
                    self.logged_unregistered.push(ino);
                    log_warn!(
                        "asc: no AHardwareBuffer for dma-buf ino {ino}; frame dropped (misses={misses})"
                    );
                    log_warn!(
                        "asc: unregistered framebuffer: {}x{} fmt={:#x} pitch={} (ino {ino})",
                        fb.width,
                        fb.height,
                        fb.pixel_format,
                        fb.pitches[0]
                    );
                }
                close_fence(fence);
                FlipHandle::latched_now()
            }
            Some((buf, published_ino, stale)) => {
                let PoolBuf { ahb, ctx, .. } = buf;
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
                // it fires the latch across threads, which is why the commit
                // worker can wait for it on another thread. The looper only
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
                    if wait_fence(pending) && !self.warned_fence_timeout {
                        self.warned_fence_timeout = true;
                        log_warn!(
                            "asc: previous release fence timed out; reusing buffer anyway (logged once)"
                        );
                    }
                    // SAFETY: `pending` is a valid descriptor owned by this call.
                    unsafe { libc::close(pending) };
                }
                if drawable {
                    // No buffer for this inode: re-publish the last buffer rather
                    // than leave the layer empty (SurfaceFlinger compositing
                    // nothing shows the app's background through as a blank
                    // panel).
                    if stale && !self.logged_stale.contains(&ino) {
                        self.logged_stale.push(ino);
                        log_warn!(
                            "asc: no AHardwareBuffer for dma-buf ino {ino}; re-publishing ino {published_ino} (misses={misses})"
                        );
                        log_warn!(
                            "asc: unregistered framebuffer: {}x{} fmt={:#x} pitch={} (ino {ino})",
                            fb.width,
                            fb.height,
                            fb.pixel_format,
                            fb.pitches[0]
                        );
                    }
                    match build_show(&api, self.sc, ahb, ctx, src, dst, fence) {
                        Some(tx) => {
                            // Arm the latch before applying: the completion
                            // callback fires it, and that is what the worker
                            // waits on before emitting the flip event.
                            let (handle, latch) = FlipHandle::new();
                            // SAFETY: `ctx` is the leaked mailbox of this slot.
                            unsafe {
                                (*ctx)
                                    .latch
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .replace(latch);
                            }
                            apply_transaction(&api, tx);
                            // What is on the layer is what a later commit falls
                            // back to.
                            self.last_publish = Some(LastPublish {
                                ino: published_ino,
                                src,
                                dst,
                            });
                            handle
                        }
                        // Transaction creation failed: nothing went out, so the
                        // acquire fence was not consumed and must be closed here.
                        None => {
                            close_fence(fence);
                            FlipHandle::latched_now()
                        }
                    }
                } else {
                    // Plane has nothing to show: hide the layer so the panel
                    // goes dark the way a disabled KMS plane does. The last
                    // published buffer is retained: the plane can come back on
                    // the next commit, and hiding is a deliberate state rather
                    // than a lost buffer.
                    if let Some(tx) = build_hide(&api, self.sc) {
                        apply_transaction(&api, tx);
                    }
                    close_fence(fence);
                    FlipHandle::latched_now()
                }
            }
        };

        self.frames += 1;
        if fb.id != 0 {
            crate::PRESENTED_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
        handle
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
        let handles = self.pool.lock(Role::Serve).get(lp.ino);
        let Some(PoolBuf { ahb, ctx, .. }) = handles else {
            return;
        };
        if let Some(tx) = build_show(&api, self.sc, ahb, ctx, lp.src, lp.dst, None) {
            apply_transaction(&api, tx);
        }
        log_info!("asc: re-published ino {} onto the layer", lp.ino);
    }
}

impl Drop for AscState {
    fn drop(&mut self) {
        // Return every buffer before the layer goes: the presenter owns the
        // daemon's reference on each, and SurfaceFlinger may still be reading
        // one. The map work runs under the lock; the gralloc releases run
        // outside it.
        let retired = self.pool.lock(Role::Serve).take_all();
        release_retired(&retired);
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
    ) -> FlipHandle {
        self.lock().publish(fb, plane, ino, live_inos, fence)
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
    pool: &Arc<TrackedMutex<ScanoutPool>>,
    doorbell: &Arc<WarmDoorbell>,
) -> ScanoutChannel {
    let d = diag::diag();
    let mut buf = [0u8; ScanoutAllocReq::LEN];
    loop {
        // The channel is idle until a request arrives. Its state is what tells
        // a shim-side timeout apart from a request the daemon never read: if
        // the shim timed out while this row says "wait-request" and the request
        // counters did not move, the request never reached this thread.
        d.set_role(Role::Scanout, State::ScanoutWaitingRequest);
        // Wait for a request on the current fd, a newer connection on the
        // listener, or teardown. Watching the listener here (rather than only
        // after this loop returns) is what makes the current fd the newest one.
        let mut pfds = [
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: listener,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: three valid pollfds waited on indefinitely.
        let ready = unsafe { libc::poll(pfds.as_mut_ptr(), 3, -1) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            log_info!("asc: scan-out channel poll: {e}");
            return ScanoutChannel::Closed;
        }
        if pfds[2].revents != 0 {
            return ScanoutChannel::Stop;
        }
        if pfds[1].revents != 0 {
            // A newer connection is already queued: this fd has been superseded
            // and must be closed before the acceptor serves the newest one.
            log_info!("asc: scan-out channel superseded (fd={fd}); adopting the newest connection");
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
                    "asc: scan-out channel recv: {e} (requests={} replies={})",
                    d.requests_seen(),
                    d.replies_sent()
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
                "asc: scan-out channel closed by peer (requests={} replies={})",
                d.requests_seen(),
                d.replies_sent()
            );
            return ScanoutChannel::Closed;
        }
        let Some(req) = ScanoutAllocReq::decode(&buf[..n]) else {
            log_warn!("asc: malformed scan-out request ({n} bytes); channel dropped");
            return ScanoutChannel::Closed;
        };
        // Open the request epoch. From here the watchdog samples this request,
        // and the reply has to be on the wire before the shim's budget expires.
        d.begin_request(req.width, req.height, req.format);
        // A receipt for every request the shim sent (rate-limited): a shim
        // timeout with no receipt here means the request never reached this
        // thread at all — a channel/accept fault, not a slow reply path.
        if d.receipt_allowed() {
            log_info!(
                "asc: scan-out request {}x{} len={} fmt={:#x} received (seq={})",
                req.width,
                req.height,
                req.len,
                req.format,
                d.requests_seen()
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

        let (resp, fd_out, handed_out, refill_spare) = match began {
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
                    Some((fd, ino)) => {
                        // The cached buffer was registered under `cached_ino`,
                        // and a dup of its dma-buf keeps the inode — so the
                        // fresh descriptor should carry the same identity.
                        // Verify rather than trust the assumption: if it moved,
                        // the registration key the client will import has
                        // changed, and the commit below re-registers under the
                        // identity actually handed out so the next commit for
                        // this buffer still resolves.
                        if ino != cached_ino {
                            log_warn!(
                                "asc: scan-out reuse ino moved ({cached_ino} -> {ino}); the free-list identity is stale, re-registering"
                            );
                        }
                        let buffer = ScanoutBuffer {
                            fd,
                            ino,
                            stride: payload.stride,
                        };
                        let replaced = pool.lock(Role::Scanout).commit(
                            width,
                            height,
                            &buffer,
                            payload.ahb,
                            false,
                            "reused",
                        );
                        if let Some(old) = replaced {
                            release_retired(core::slice::from_ref(&retire_buf(old)));
                        }
                        (
                            ScanoutAllocResp {
                                ret: 0,
                                ino: buffer.ino,
                                stride: buffer.stride,
                            },
                            Some(buffer.fd),
                            true,
                            false,
                        )
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
                        (refused.0, refused.1, false, false)
                    }
                }
            }
            // A pre-allocated spare matches: hand it over with no gralloc
            // allocation, and rebuild the spare afterwards.
            Ok(Reservation::Warm(warm)) => {
                d.step(State::ScanoutLocked);
                let replaced = pool.lock(Role::Scanout).commit(
                    width,
                    height,
                    &warm.buffer,
                    warm.ahb,
                    false,
                    "from spare",
                );
                if let Some(old) = replaced {
                    release_retired(core::slice::from_ref(&retire_buf(old)));
                }
                (
                    ScanoutAllocResp {
                        ret: 0,
                        ino: warm.buffer.ino,
                        stride: warm.buffer.stride,
                    },
                    Some(warm.buffer.fd),
                    true,
                    true,
                )
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
                        let replaced = pool.lock(Role::Scanout).commit(
                            width,
                            height,
                            &buffer,
                            ahb,
                            true,
                            "allocated",
                        );
                        if let Some(old) = replaced {
                            release_retired(core::slice::from_ref(&retire_buf(old)));
                        }
                        (
                            ScanoutAllocResp {
                                ret: 0,
                                ino: buffer.ino,
                                stride: buffer.stride,
                            },
                            Some(buffer.fd),
                            true,
                            true,
                        )
                    }
                    Err(errno) => {
                        pool.lock(Role::Scanout).abort_alloc();
                        let refused = refuse_scanout(req, errno, lock_waited_ms, pool, d);
                        (refused.0, refused.1, false, false)
                    }
                }
            }
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(libc::EIO).abs();
                let refused = refuse_scanout(req, errno, lock_waited_ms, pool, d);
                (refused.0, refused.1, false, false)
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
            Some(d.snapshot("reply-write-failed"))
        } else if elapsed_ms >= diag::STALL_WARN_MS && d.slow_report_allowed() {
            Some(d.snapshot("slow-reply"))
        } else {
            None
        };
        let total_ms = d.end_request(if send_failed {
            Reply::Failed
        } else {
            Reply::Written
        });
        if let Some(line) = diag_line {
            log_warn!("asc: scan-out request {total_ms} ms: {line}");
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
            log_warn!("asc: scan-out channel send: {e}");
            return ScanoutChannel::Closed;
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
                    "asc: dma-buf ino {ino} retired; retained for reuse ({}x{})",
                    geom.width,
                    geom.height
                );
                let fd = ctx_release_fd(payload);
                if fd >= 0 {
                    // SAFETY: `fd` is the stale release fence this registration
                    // owned; it is not used again.
                    unsafe { libc::close(fd) };
                }
            }
            Removed::Released { ino, payload } => {
                log_info!("asc: dma-buf ino {ino} retired; releasing its AHardwareBuffer");
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
            log_info!("asc: dma-buf ino {ino} returned; retained for reuse");
            let fd = ctx_release_fd(&payload);
            if fd >= 0 {
                // SAFETY: a stale release fence this registration owned.
                unsafe { libc::close(fd) };
            }
        }
        Removed::Released { ino, payload } => {
            log_info!("asc: dma-buf ino {ino} returned; releasing its AHardwareBuffer");
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
        format!("len={} (unresolved)", req.len)
    } else {
        format!("{}x{}", req.width, req.height)
    };
    log_warn!(
        "asc: scan-out alloc {what} refused: {} (errno {errno}, waited {waited_ms} ms for the pool lock, pool={owned}/{MAX_SCANOUT_POOL} live={live} free={free}) {}",
        refusal_reason(errno),
        d.snapshot("refused")
    );
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
        format,
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
        log_warn!("asc: AHardwareBuffer_allocate {width}x{height} failed (rc={rc})");
        return Err(if rc != 0 { -rc } else { libc::ENOMEM });
    }
    let Some((fd, ino)) = dup_scanout_fd(&api, ahb) else {
        // SAFETY: the reference belongs to this call.
        unsafe { ffi::AHardwareBuffer_release(ahb) };
        return Err(libc::EIO);
    };
    let mut real = ffi::AHardwareBufferDesc::default();
    // SAFETY: `ahb` is a live buffer owned by this call.
    unsafe { ffi::AHardwareBuffer_describe(ahb, &mut real) };
    let stride = real.stride.saturating_mul(4);
    Ok((ScanoutBuffer { fd, ino, stride }, ahb))
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
/// can be traced back to the actual fault in the daemon log.
fn refusal_reason(errno: i32) -> &'static str {
    match errno {
        libc::ENOSPC => "pool exhausted",
        libc::ENOSYS => "presenter disabled (no publish API)",
        libc::EINVAL => "unsupported geometry or format",
        _ => "AHardwareBuffer allocation failed",
    }
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
    src: ffi::ARect,
    dst: ffi::ARect,
    fence: Option<RawFd>,
) -> Option<*mut ffi::ASurfaceTransaction> {
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
        (api.st_set_geometry)(tx, sc, &src, &dst, 0);
        // setBuffer consumes the acquire fence: SurfaceFlinger closes it,
        // so it must not be closed here as well.
        (api.st_set_buffer)(tx, sc, ahb, fence.unwrap_or(-1));
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
fn dup_scanout_fd(api: &ffi::ScApi, ahb: *mut ffi::AHardwareBuffer) -> Option<(RawFd, u64)> {
    // SAFETY: `ahb` is a live buffer owned by this call; the native handle it
    // returns is borrowed and stays valid while `ahb` does.
    let nh = unsafe { (api.ahb_get_native_handle)(ahb) };
    if nh.is_null() {
        return None;
    }
    // SAFETY: `nh` is a live native handle; fd 0 is the mappable dma-buf.
    let fd0 = unsafe { ffi::NativeHandle::fd(nh, 0) }?;
    let ino = fstat_ino(fd0)?;
    // The client owns the descriptor it is given, so it must be a fresh one.
    // SAFETY: dup of a descriptor this call can read; the result is owned here.
    let fd = unsafe { libc::fcntl(fd0, libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return None;
    }
    Some((fd, ino))
}

/// `st_ino` of `fd`, which for a dma-buf is the kernel's per-buffer identity.
fn fstat_ino(fd: c_int) -> Option<u64> {
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: fstat on a descriptor we own; `st` is fully written on success.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return None;
    }
    Some(st.st_ino as u64)
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

/// Poll `fd` until it signals; `true` when the frame budget expired first.
fn wait_fence(fd: RawFd) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd for a duration.
    let n = unsafe { libc::poll(&mut pfd, 1, RELEASE_FENCE_TIMEOUT_MS) };
    n == 0
}
