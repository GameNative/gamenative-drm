//! GEM-style reuse cache for presenter-owned scan-out buffers.
//!
//! The Android presenter (`gndrm-android`'s `asc.rs`) owns the scan-out
//! `AHardwareBuffer`s and the gralloc calls around them, but the *policy* of
//! which buffer a request may be answered from lives here so it is
//! host-testable. A real kernel DRM driver recycles GEM buffers instead of
//! freeing and re-allocating one per frame; this is that cache:
//!
//! - a buffer retired from service (its client stopped importing it) is kept
//!   in a free list keyed by geometry, not `AHardwareBuffer_release`d;
//! - a later request of the same geometry is answered from the free list with
//!   no gralloc allocation at all;
//! - a buffer is only reusable once the presenter has reported its flip event
//!   (`latched`), the same rule that governs re-publishing — a buffer
//!   SurfaceFlinger may still be compositing is never handed back out;
//! - the cap bounds `live + free + reserved`, so the pool settles at the
//!   session's working set and then allocates nothing more;
//! - a request may arrive as a bare allocation length (the shim matched no
//!   connector mode), which is resolved to a geometry the daemon knows — a
//!   connector mode, a committed framebuffer size, or a geometry already in the
//!   pool — so a window-sized scan-out buffer is pool-owned just like a
//!   mode-sized one, and a length no known geometry produces stays kernel-owned.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Bytes per pixel of the pool's single scan-out format. [`Geometry::alloc_len`]
/// is the *tight* byte size `width * height * 4`; a real scan-out allocation is
/// row-padded and longer, so the length index is authoritative only when keyed
/// on the allocation's own length (the GEM `st_size`, supplied by the `ADDFB2`
/// feed). This tight size is the fallback for a geometry whose tight size is
/// already page-aligned (every connector mode).
const SCANOUT_BPP: u64 = 4;

/// Smallest and largest row alignment a linear scan-out allocation is padded
/// to, and the page size its length is rounded up to.
const ROW_ALIGN_MIN: u64 = 64;
const ROW_ALIGN_MAX: u64 = 4096;
const PAGE_LEN: u64 = 4096;

/// Whether a buffer whose real allocation length (`st_size`) is `buffer_len`
/// can answer a length-only request for `requested_len` bytes.
#[must_use]
pub fn serves_len(buffer_len: u64, requested_len: u64) -> bool {
    buffer_len >= requested_len
}

/// Where a scan-out buffer lands on the session's window: a KMS plane rect in
/// *mode* pixels, mapped into the app's window (the display's own pixels).
///
/// The session's mode is its scan-out size, which the app may ask for below the
/// window's (the Upscaling Resolution), so the two are not the same box. The
/// layer is composited in window pixels: a smaller mode is scaled up uniformly
/// and centred — a mode of another aspect is letterboxed, never stretched — and
/// that upscale belongs to the display compositor rather than to gamescope,
/// whose composite stops at the mode. Mode equal to window is the identity, so
/// a 1:1 present is unchanged.
///
/// `crtc` is `(x, y, width, height)` in mode pixels, as the plane committed it;
/// the result is `(left, top, right, bottom)` in window pixels, clipped to the
/// mode (a plane rect can exceed it) and therefore inside the window.
#[must_use]
pub fn layer_rect(
    crtc: (i32, i32, i32, i32),
    mode: (u32, u32),
    window: (u32, u32),
) -> (i32, i32, i32, i32) {
    let mw = i64::from(mode.0.max(1));
    let mh = i64::from(mode.1.max(1));
    let ww = i64::from(window.0.max(1));
    let wh = i64::from(window.1.max(1));
    // The mode's box fitted into the window: width-limited or height-limited.
    let (fit_w, fit_h) = if ww * mh <= wh * mw {
        (ww, mh * ww / mw)
    } else {
        (mw * wh / mh, wh)
    };
    let off_x = (ww - fit_w) / 2;
    let off_y = (wh - fit_h) / 2;
    // One axis: clamp to the mode's span, scale, offset, round to nearest.
    let place =
        |v: i64, span: i64, fit: i64, off: i64| off + (v.clamp(0, span) * fit + span / 2) / span;
    let (x, y, w, h) = (
        i64::from(crtc.0),
        i64::from(crtc.1),
        i64::from(crtc.2),
        i64::from(crtc.3),
    );
    (
        place(x, mw, fit_w, off_x) as i32,
        place(y, mh, fit_h, off_y) as i32,
        place(x + w, mw, fit_w, off_x) as i32,
        place(y + h, mh, fit_h, off_y) as i32,
    )
}

/// A rect as `(left, top, right, bottom)`.
pub type Edges = (i32, i32, i32, i32);

/// Where the cursor plane lands on the session's window: the source rect to
/// sample (buffer pixels) and the destination (window pixels), as
/// `(left, top, right, bottom)` each.
///
/// A KMS cursor may hang off the mode's edges (negative `CRTC_X`/`CRTC_Y`, or
/// past the right/bottom). The part outside the mode is clipped and the source
/// is cropped by the same fraction, so the cursor is cut at the edge rather
/// than squashed into what is left, which [`layer_rect`] alone would do. The
/// visible part is then fitted into the window exactly like the scan-out layer.
///
/// `crtc` is `(x, y, width, height)` in mode pixels; `src` is
/// `(x, y, width, height)` in buffer pixels. `None` when nothing of the cursor
/// is on the mode (or either rect is empty): the layer is hidden.
#[must_use]
pub fn cursor_rects(
    crtc: (i32, i32, i32, i32),
    src: (u32, u32, u32, u32),
    mode: (u32, u32),
    window: (u32, u32),
) -> Option<(Edges, Edges)> {
    let (x, y, w, h) = (
        i64::from(crtc.0),
        i64::from(crtc.1),
        i64::from(crtc.2),
        i64::from(crtc.3),
    );
    let (sx, sy, sw, sh) = (
        i64::from(src.0),
        i64::from(src.1),
        i64::from(src.2),
        i64::from(src.3),
    );
    if w <= 0 || h <= 0 || sw <= 0 || sh <= 0 {
        return None;
    }
    let (mw, mh) = (i64::from(mode.0), i64::from(mode.1));
    let (x0, x1) = (x.max(0), (x + w).min(mw));
    let (y0, y1) = (y.max(0), (y + h).min(mh));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    // The visible slice of the destination, mapped back into the source.
    let crop = |lo: i64, hi: i64, origin: i64, span: i64, s0: i64, slen: i64| {
        (
            s0 + (lo - origin) * slen / span,
            s0 + ((hi - origin) * slen + span - 1) / span,
        )
    };
    let (sl, sr) = crop(x0, x1, x, w, sx, sw);
    let (st, sb) = crop(y0, y1, y, h, sy, sh);
    if sr <= sl || sb <= st {
        return None;
    }
    let dst = layer_rect(
        (x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32),
        mode,
        window,
    );
    if dst.2 <= dst.0 || dst.3 <= dst.1 {
        return None;
    }
    Some(((sl as i32, st as i32, sr as i32, sb as i32), dst))
}

/// Geometry a pooled buffer can serve. A free buffer only matches a request of
/// the exact same width, height and format.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Geometry {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Gralloc format (the presenter's single RGBA layout).
    pub format: u32,
}

impl Geometry {
    /// The tight byte size of this geometry (`width * height * 4`), used as the
    /// length key when the pool commits a geometry it owns. A real scan-out
    /// allocation is row-padded and longer than this; the exact length is
    /// supplied by the `ADDFB2` feed, which reads it from the GEM's `st_size`.
    #[must_use]
    pub fn alloc_len(self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * SCANOUT_BPP
    }

    /// Whether `len` is this geometry's length with each row padded to a
    /// power-of-two alignment and the whole rounded to a page, and if so how
    /// many bytes of padding a row carries. That is the shape of a linear
    /// allocation whose real length the pool has not been told yet.
    fn row_padding_for(self, len: u64) -> Option<u64> {
        let row = u64::from(self.width) * SCANOUT_BPP;
        let rows = u64::from(self.height);
        (ROW_ALIGN_MIN.trailing_zeros()..=ROW_ALIGN_MAX.trailing_zeros())
            .map(|shift| row.next_multiple_of(1 << shift))
            .find(|pitch| (pitch * rows).next_multiple_of(PAGE_LEN) == len)
            .map(|pitch| pitch - row)
    }
}

/// A buffer the client holds (handed out but not yet retired).
struct Live<T> {
    geom: Geometry,
    payload: T,
    /// Set once a commit published this buffer, so a buffer that was never
    /// imported is not treated as retired (and a never-published buffer is
    /// always safe to reuse).
    seen_live: bool,
    /// Fired by the presenter's completion callback when SurfaceFlinger
    /// reports the flip event. Reuse requires it.
    latched: Arc<AtomicBool>,
}

/// A retired buffer held for reuse.
///
/// `ino` is the identity the buffer was registered under when it was last
/// handed out. The reuse cache keeps it so a handout can *verify* the
/// `st_ino`-stability assumption (a duplicate of the cached dma-buf must keep
/// the same inode) instead of trusting it, and re-register under whatever
/// identity the fresh descriptor actually carries.
struct Free<T> {
    geom: Geometry,
    ino: u64,
    payload: T,
}

/// What [`ReusePool::begin`] reserved for one request.
pub enum Reservation<T> {
    /// A retired buffer of the requested geometry: hand it straight out, no
    /// allocation. This is the steady state. `ino` is the identity it was
    /// registered under when last retired, for the caller to re-register
    /// against (and to check against the fresh descriptor's inode).
    Reuse { ino: u64, payload: T },
    /// Nothing reusable matched; the caller allocates outside the lock.
    /// `evicted` is a free buffer the pool dropped to stay under the cap and
    /// the caller must release.
    Cold { evicted: Option<T> },
    /// The cap is reached with nothing that may be evicted (nothing free).
    Full,
}

/// A buffer taken out of the live set. `Kept` buffers are already back in the
/// free list; `Released` ones could not be reused and must be released.
pub enum Removed<T> {
    /// Presented and its flip event reported: held in the free list for reuse.
    Kept {
        /// The retired buffer's identity.
        ino: u64,
        /// The geometry it can be handed back out for.
        geom: Geometry,
        /// A copy of the cached payload, so the caller can close whatever the
        /// registration still held.
        payload: T,
    },
    /// Not reusable — the flip event was not reported — so the caller releases
    /// the buffer.
    Released {
        /// The retired buffer's identity.
        ino: u64,
        /// The buffer to release.
        payload: T,
    },
}

/// A reuse cache over an opaque buffer payload `T`.
///
/// `T` is whatever the caller needs to hand a buffer out and release it; the
/// cache only stores and counts it. It must be `Copy` because a `Kept` buffer
/// is both held in the free list and handed back to the caller for bookkeeping.
pub struct ReusePool<T> {
    cap: usize,
    live: HashMap<u64, Live<T>>,
    /// Retired buffers, oldest first: the front is the one that has been free
    /// the longest and is therefore the first to be released when the cap is
    /// needed for a different geometry.
    free: Vec<Free<T>>,
    /// Cold allocations in flight outside the lock, counted against the cap so
    /// concurrent reservations cannot exceed it.
    reserved: usize,
    /// Commits that resolved to no registration at all: the presenter was
    /// asked to publish a buffer it never handed out. Counted (not merely
    /// logged) so a session can be checked for the fault by number.
    misses: u64,
    /// Geometries a length-only request may resolve to, keyed by allocation
    /// length. `DMA_HEAP_IOCTL_ALLOC` carries only a length, so a scan-out
    /// allocation that matches no connector mode arrives as a length and is
    /// resolved here — from the connector modes the daemon advertises, from
    /// every framebuffer the client registers, and from the geometries already
    /// in the pool (folded in on commit). Keyed by allocation length *and*
    /// format: two formats at the same resolution share a byte length, so a
    /// length-only key would hand a request the wrong layout. See
    /// [`Self::resolve_len`].
    by_len: HashMap<(u64, u32), Geometry>,
}

impl<T: Copy> ReusePool<T> {
    /// A pool owning at most `cap` buffers.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            live: HashMap::new(),
            free: Vec::new(),
            reserved: 0,
            misses: 0,
            by_len: HashMap::new(),
        }
    }

    /// Buffers the pool owns: handed out, held for reuse, and cold allocations
    /// in flight. This is the number the cap bounds.
    #[must_use]
    pub fn owned(&self) -> usize {
        self.live.len() + self.free.len() + self.reserved
    }

    /// Buffers handed out and not yet retired.
    #[must_use]
    pub fn live_len(&self) -> usize {
        self.live.len()
    }

    /// Retired buffers held for reuse.
    #[must_use]
    pub fn free_len(&self) -> usize {
        self.free.len()
    }

    /// Commits that resolved to no registration (see [`Self::resolve`]).
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Record a geometry a length-only request may be resolved against: a
    /// connector mode the daemon advertises, or a framebuffer geometry the
    /// client registered. `alloc_len` is the **real** allocation length of a
    /// buffer of this geometry — the row-padded, page-rounded size the client
    /// allocated with, which is what a length-only request carries and what the
    /// `ADDFB2` feed reads from the GEM's `st_size`. Keyed by length *and*
    /// format, so a request is never answered with a geometry of a different
    /// layout. Where two geometries share both, the first note wins.
    pub fn note_geometry(&mut self, geom: Geometry, alloc_len: u64) {
        if geom.width == 0 || geom.height == 0 {
            return;
        }
        self.by_len.entry((alloc_len, geom.format)).or_insert(geom);
    }

    /// Resolve an allocation length and format to a geometry the daemon can
    /// serve, or `None` when no known geometry produces that pairing — i.e. the
    /// buffer is not a scan-out allocation and stays kernel-owned. A length
    /// registered for a geometry wins; otherwise a known geometry whose
    /// row-padded linear length is exactly `len` serves it, the least padded
    /// first, so a mode-sized buffer resolves before its real length has been
    /// registered by a framebuffer or the spare.
    ///
    /// The resolved geometry is a guess about the caller's layout, not a
    /// promise about the buffer's size: two geometries can share a length
    /// (tight 1280x960 and 1280x800 with 6144 B rows are both 4915200 B), and
    /// the buffer allocated for the one picked may be shorter than `len`. The
    /// caller must check the handed-out buffer's real length with
    /// [`serves_len`] and refuse the request when it does not hold.
    #[must_use]
    pub fn resolve_len(&self, len: u64, format: u32) -> Option<Geometry> {
        if let Some(geom) = self.by_len.get(&(len, format)) {
            return Some(*geom);
        }
        self.by_len
            .values()
            .filter(|geom| geom.format == format)
            .filter_map(|geom| geom.row_padding_for(len).map(|pad| (pad, *geom)))
            .min_by_key(|(pad, geom)| (*pad, geom.width, geom.height))
            .map(|(_, geom)| geom)
    }

    /// Reserve an answer to a length-only request: resolve `len` to a geometry,
    /// then [`Self::begin`]. `None` when the length is not a scan-out
    /// allocation.
    pub fn begin_len(&mut self, len: u64, format: u32) -> Option<Reservation<T>> {
        let geom = self.resolve_len(len, format)?;
        Some(self.begin(geom))
    }

    /// Take a retired buffer of exactly `geom`, if one is held, returned with
    /// the identity it was last registered under. This is the allocation-free
    /// answer and is preferred over every other. `remove` (not `swap_remove`)
    /// keeps the free list ordered oldest-first, which the over-cap eviction
    /// relies on.
    pub fn take_reusable(&mut self, geom: Geometry) -> Option<(u64, T)> {
        let i = self.free.iter().position(|f| f.geom == geom)?;
        let f = self.free.remove(i);
        Some((f.ino, f.payload))
    }

    /// Reserve capacity for one cold allocation. The caller has already taken
    /// any matching free buffer (see [`Self::take_reusable`]); this only
    /// bounds the pool and evicts the longest-free buffer when the cap is
    /// reached, so a geometry change cannot wedge it.
    ///
    /// # Panics
    /// Never in practice: `remove(0)` is guarded by the `is_empty` check.
    pub fn reserve(&mut self) -> Reservation<T> {
        if self.owned() < self.cap {
            self.reserved += 1;
            return Reservation::Cold { evicted: None };
        }
        // Over the cap with nothing matching: drop the buffer that has been
        // free the longest. A pool full of live buffers is a genuine ENOSPC.
        if self.free.is_empty() {
            return Reservation::Full;
        }
        let evicted = self.free.remove(0).payload;
        self.reserved += 1;
        Reservation::Cold {
            evicted: Some(evicted),
        }
    }

    /// Reserve an answer to a request of `geom`: a matching retired buffer when
    /// one is held, otherwise capacity for a cold allocation. A free buffer is
    /// preferred over an allocation, which is what stops the per-frame churn.
    pub fn begin(&mut self, geom: Geometry) -> Reservation<T> {
        if let Some((ino, payload)) = self.take_reusable(geom) {
            return Reservation::Reuse { ino, payload };
        }
        self.reserve()
    }

    /// Give back the capacity [`Self::begin`] reserved when the allocation
    /// failed (or a reused payload could not be handed out).
    pub fn abort_cold(&mut self) {
        self.reserved = self.reserved.saturating_sub(1);
    }

    /// Put a reused payload back after it could not be handed out (its client
    /// descriptor could not be duplicated), so the pool does not pay an
    /// allocation for a buffer it already holds.
    pub fn recycle(&mut self, geom: Geometry, ino: u64, payload: T) {
        self.free.push(Free { geom, ino, payload });
    }

    /// Register a handed-out buffer under `ino`. `cold` releases the capacity
    /// [`Self::begin`] reserved; `latched` is the flag the presenter's
    /// completion callback fires. Returns any payload replaced by an `ino`
    /// collision (the caller releases it).
    pub fn commit(
        &mut self,
        ino: u64,
        geom: Geometry,
        payload: T,
        latched: Arc<AtomicBool>,
        cold: bool,
    ) -> Option<T> {
        if cold {
            self.reserved = self.reserved.saturating_sub(1);
        }
        // A committed geometry is one the pool can serve again, so a later
        // length-only request for the same window size resolves to it. The pool
        // knows only the tight size here; a row-padded allocation's real length
        // is supplied by the `ADDFB2` feed, which sees the GEM's `st_size`.
        self.note_geometry(geom, geom.alloc_len());
        self.live
            .insert(
                ino,
                Live {
                    geom,
                    payload,
                    seen_live: false,
                    latched,
                },
            )
            .map(|old| old.payload)
    }

    /// Mark `ino` as published (its client imported it at least once).
    pub fn mark_live(&mut self, ino: u64) {
        if let Some(entry) = self.live.get_mut(&ino) {
            entry.seen_live = true;
        }
    }

    /// Resolve a commit's identity to its registration and mark it published
    /// (`seen_live`), returning the payload. `None` when the inode was never
    /// registered: the presenter was asked to publish a buffer it does not own
    /// (a client dma-buf gamescope scanned out directly, or a table rebuilt
    /// under a running client). Such a commit is counted in [`Self::misses`]
    /// so it is a number in the diagnostics rather than only a log line — the
    /// caller must fall back, and a nonzero total is the fault.
    pub fn resolve(&mut self, ino: u64) -> Option<T> {
        match self.live.get_mut(&ino) {
            Some(entry) => {
                entry.seen_live = true;
                Some(entry.payload)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// The payload handed out under `ino`, copied out.
    #[must_use]
    pub fn get(&self, ino: u64) -> Option<T> {
        self.live.get(&ino).map(|entry| entry.payload)
    }

    /// Take `ino` out of the live set, applying the reuse rule (used when a
    /// reply could not be delivered, so the buffer never reached the client).
    pub fn take(&mut self, ino: u64) -> Option<Removed<T>> {
        let entry = self.live.remove(&ino)?;
        Some(self.remove_one(ino, entry))
    }

    /// Retire the buffers whose client stopped importing them and that are not
    /// the fallback the layer still points at.
    ///
    /// `live_inos` is the set of identities still backed by a GEM object at
    /// commit time. The device refuses `RMFB` of an in-flight framebuffer, so
    /// an inode leaves `live_inos` only after its flip event has fired — but
    /// the per-buffer `latched` flag is still consulted (and required) here, so
    /// the reuse rule does not depend on that invariant holding.
    pub fn retire(&mut self, live_inos: &HashSet<u64>, keep: Option<u64>) -> Vec<Removed<T>> {
        let victims: Vec<u64> = self
            .live
            .iter()
            .filter(|(ino, entry)| {
                entry.seen_live && !live_inos.contains(*ino) && Some(**ino) != keep
            })
            .map(|(ino, _)| *ino)
            .collect();
        victims
            .into_iter()
            .filter_map(|ino| {
                self.live
                    .remove(&ino)
                    .map(|entry| self.remove_one(ino, entry))
            })
            .collect()
    }

    /// Apply the reuse rule to one removed entry: a never-published or
    /// flip-reported buffer is kept for reuse, anything else must be released.
    fn remove_one(&mut self, ino: u64, entry: Live<T>) -> Removed<T> {
        if !entry.seen_live || entry.latched.load(Ordering::Acquire) {
            self.free.push(Free {
                geom: entry.geom,
                ino,
                payload: entry.payload,
            });
            Removed::Kept {
                ino,
                geom: entry.geom,
                payload: entry.payload,
            }
        } else {
            Removed::Released {
                ino,
                payload: entry.payload,
            }
        }
    }

    /// Take every buffer the pool owns, for teardown. The caller releases them.
    pub fn drain(&mut self) -> Vec<T> {
        let live = self.live.drain().map(|(_, entry)| entry.payload);
        let free = self.free.drain(..).map(|f| f.payload);
        self.reserved = 0;
        live.chain(free).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gndrm_uapi::wire::SCANOUT_FORMAT_RGBA_8888;

    fn geom(width: u32, height: u32) -> Geometry {
        Geometry {
            width,
            height,
            format: SCANOUT_FORMAT_RGBA_8888,
        }
    }

    /// A payload that is just an id, standing in for a buffer handle.
    type Id = u64;

    /// Hand one buffer out of a freshly-begun request: take whatever the cache
    /// offers (a reused payload, or the caller's freshly allocated `id`) and
    /// register it as published. Returns the busiest reuse question — whether
    /// the request allocated.
    fn hand_out(pool: &mut ReusePool<Id>, g: Geometry, id: Id, cold_id: Id) -> Arc<AtomicBool> {
        let latch = Arc::new(AtomicBool::new(false));
        let (payload, cold) = match pool.begin(g) {
            Reservation::Reuse { payload, .. } => (payload, false),
            Reservation::Cold { .. } => (cold_id, true),
            Reservation::Full => panic!("{}", format_args!("pool reported full at {g:?}")),
        };
        pool.commit(id, g, payload, Arc::clone(&latch), cold);
        latch
    }

    #[test]
    fn same_length_different_format_resolves_separately() {
        let mut pool: ReusePool<()> = ReusePool::new(4);
        // Two layouts of one resolution share a byte length, so a length-only
        // key would answer both requests with whichever was noted first.
        let rgba = Geometry {
            width: 1280,
            height: 960,
            format: SCANOUT_FORMAT_RGBA_8888,
        };
        let xbgr = Geometry {
            width: 1280,
            height: 960,
            format: 0x3432_4258,
        };
        assert_eq!(rgba.alloc_len(), xbgr.alloc_len());
        pool.note_geometry(rgba, rgba.alloc_len());
        pool.note_geometry(xbgr, xbgr.alloc_len());
        assert_eq!(pool.resolve_len(rgba.alloc_len(), rgba.format), Some(rgba));
        assert_eq!(pool.resolve_len(xbgr.alloc_len(), xbgr.format), Some(xbgr));
        // An unrelated format at the same length resolves to nothing.
        assert_eq!(pool.resolve_len(rgba.alloc_len(), 0xdead_beef), None);
    }

    #[test]
    fn retired_buffer_is_reused_after_its_flip_event() {
        let g = geom(1280, 960);
        let mut pool = ReusePool::<Id>::new(4);

        // First request allocates cold and publishes.
        let latch = hand_out(&mut pool, g, 100, 100);
        pool.mark_live(100);

        // The client stops importing it and the presenter reports the flip.
        latch.store(true, Ordering::Release);
        let removed = pool.retire(&HashSet::new(), None);
        assert_eq!(removed.len(), 1);
        assert!(
            matches!(removed[0], Removed::Kept { ino: 100, .. }),
            "a latched buffer must be kept for reuse, not released"
        );
        assert_eq!(pool.free_len(), 1, "the retired buffer is held for reuse");

        // The next request of the same geometry is answered from the cache:
        // no allocation.
        match pool.begin(g) {
            Reservation::Reuse {
                ino: 100,
                payload: 100,
            } => {}
            other => panic!(
                "{}",
                format_args!("expected the retired buffer to be handed back out, got {}",
                    describe(&other))
            ),
        }
        assert_eq!(pool.free_len(), 0, "the reused buffer left the free list");
    }

    /// The reuse cache remembers the identity a retired buffer was registered
    /// under, so a free-list handout can (a) check the fresh descriptor's
    /// inode against it and (b) re-register under the identity actually
    /// carried. Without the stored `ino` the handout is a blind re-register.
    #[test]
    fn reused_buffer_resolves_on_the_next_commit() {
        let g = geom(1280, 960);
        let mut pool = ReusePool::<Id>::new(4);

        // A published, latched buffer retires into the free list with its ino.
        let latch = hand_out(&mut pool, g, 42, 42);
        pool.mark_live(42);
        latch.store(true, Ordering::Release);
        pool.retire(&HashSet::new(), None);

        // The reuse handout carries that identity back out.
        let reserved = match pool.begin(g) {
            Reservation::Reuse { ino, payload } => (ino, payload),
            other => panic!(
                "{}",
                format_args!("expected reuse, got {}", describe(&other))
            ),
        };
        assert_eq!(reserved, (42, 42), "the free-list entry kept its ino");

        // Re-registering the handed-out buffer (same ino: a dup keeps the
        // inode) makes the next commit resolve rather than miss.
        let latch = Arc::new(AtomicBool::new(false));
        pool.commit(42, g, reserved.1, latch, false);
        assert_eq!(pool.resolve(42), Some(42));
        assert_eq!(pool.misses(), 0, "a reused buffer must not miss");
    }

    /// A retain/reuse cycle must never drop the registration of a buffer that
    /// is still live in the pool: after `Kept` the entry is back in the free
    /// list with its identity, and re-handing it out resolves again.
    #[test]
    fn registration_survives_a_retain_reuse_cycle() {
        let g = geom(640, 480);
        let mut pool = ReusePool::<Id>::new(4);
        let latch = hand_out(&mut pool, g, 7, 7);
        pool.mark_live(7);
        latch.store(true, Ordering::Release);

        let removed = pool.retire(&HashSet::new(), None);
        assert!(matches!(removed[0], Removed::Kept { ino: 7, .. }));
        assert_eq!(pool.misses(), 0, "a retire is not a miss");

        // Two full cycles: each handout re-registers and each commit resolves.
        for _ in 0..2 {
            let (ino, payload) = match pool.begin(g) {
                Reservation::Reuse { ino, payload } => (ino, payload),
                other => panic!(
                    "{}",
                    format_args!("expected reuse, got {}", describe(&other))
                ),
            };
            assert_eq!(ino, 7, "the retained identity is stable across cycles");
            let latch = Arc::new(AtomicBool::new(false));
            pool.commit(7, g, payload, Arc::clone(&latch), false);
            assert_eq!(pool.resolve(7), Some(7));
            pool.mark_live(7);
            latch.store(true, Ordering::Release);
            pool.retire(&HashSet::new(), None);
        }
        assert_eq!(pool.misses(), 0);
    }

    /// A commit for an identity the pool never handed out (a client dma-buf
    /// gamescope scanned out directly) must be *counted* as a miss, not
    /// silently ignored — the presenter falls back to its last frame, and the
    /// number is what says the fallback happened.
    #[test]
    fn unregistered_commit_is_reported_not_silent() {
        let mut pool = ReusePool::<Id>::new(4);
        assert_eq!(pool.misses(), 0);
        assert_eq!(
            pool.resolve(0xDEAD),
            None,
            "an unknown ino resolves to none"
        );
        assert_eq!(pool.misses(), 1, "the miss is counted");
        // Resolving a registered ino still works and is not a miss.
        let g = geom(320, 200);
        let latch = hand_out(&mut pool, g, 1, 1);
        pool.mark_live(1);
        assert_eq!(pool.resolve(1), Some(1));
        assert_eq!(pool.misses(), 1);
        drop(latch);
    }

    #[test]
    fn in_flight_buffer_is_not_reused_before_its_flip_event() {
        let g = geom(1280, 960);
        let mut pool = ReusePool::<Id>::new(4);

        // Allocated and published, but the presenter has *not* reported the
        // flip event: SurfaceFlinger may still be compositing it.
        let latch = hand_out(&mut pool, g, 100, 100);
        pool.mark_live(100);
        assert!(!latch.load(Ordering::Acquire));

        let removed = pool.retire(&HashSet::new(), None);
        assert_eq!(removed.len(), 1);
        assert!(
            matches!(removed[0], Removed::Released { ino: 100, .. }),
            "an unlatched buffer must not be kept for reuse"
        );
        assert_eq!(pool.free_len(), 0, "nothing was cached");

        // The next request cannot be answered from the cache.
        match pool.begin(g) {
            Reservation::Cold { evicted: None } => {}
            other => panic!(
                "{}",
                format_args!("expected a cold allocation while the buffer is in flight, got {}",
                    describe(&other))
            ),
        }
    }

    #[test]
    fn geometry_change_evicts_the_longest_free_buffer_instead_of_failing() {
        let mut pool = ReusePool::<Id>::new(2);
        let a = geom(1280, 960);
        let b = geom(640, 480);

        // Fill the cap with two retired buffers of geometry `a`.
        let la0 = hand_out(&mut pool, a, 1, 1);
        pool.mark_live(1);
        let la1 = hand_out(&mut pool, a, 2, 2);
        pool.mark_live(2);
        la0.store(true, Ordering::Release);
        la1.store(true, Ordering::Release);
        assert_eq!(pool.retire(&HashSet::new(), None).len(), 2);
        assert_eq!(pool.free_len(), 2);

        // A request of a different geometry at the cap must evict a free
        // buffer rather than report the pool full. Which of the two is freed
        // first inside one retire is unspecified (they were freed in the same
        // step), so either id is a valid victim.
        match pool.begin(b) {
            Reservation::Cold { evicted: Some(id) } => {
                assert!(id == 1 || id == 2, "evicted an unexpected buffer {id}")
            }
            other => panic!(
                "{}",
                format_args!("expected an eviction, got {}", describe(&other))
            ),
        }
        assert_eq!(pool.free_len(), 1, "exactly one buffer was evicted");
    }

    #[test]
    fn steady_state_stops_allocating() {
        let g = geom(1280, 960);
        let mut pool = ReusePool::<Id>::new(8);
        let mut next_id: Id = 1;
        let mut allocations = 0u32;

        // Emulate the per-frame cycle: a request, then the previous frame's
        // buffer retiring after its flip event. The working set is 3.
        let mut frames: Vec<(Id, Arc<AtomicBool>)> = Vec::new();
        for frame in 0..40u32 {
            let reservation = pool.begin(g);
            let (id, cold) = match reservation {
                Reservation::Reuse { payload, .. } => (payload, false),
                Reservation::Cold { .. } => {
                    allocations += 1;
                    next_id += 1;
                    (next_id, true)
                }
                Reservation::Full => panic!(
                    "{}",
                    format_args!("pool unexpectedly full at frame {frame}")
                ),
            };
            let latch = Arc::new(AtomicBool::new(false));
            pool.commit(id, g, id, latch.clone(), cold);
            pool.mark_live(id);
            frames.push((id, latch));

            // Each published buffer flips before the next publish.
            for (_, l) in &frames {
                l.store(true, Ordering::Release);
            }
            // Retire everything but the three most recent (the working set).
            if frames.len() > 3 {
                let (old_id, _) = frames.remove(0);
                // The remaining frames are still imported by the client, so
                // only `old_id` is retired.
                let live: HashSet<u64> = frames.iter().map(|(id, _)| *id).collect();
                let removed = pool.retire(&live, None);
                assert!(removed
                    .iter()
                    .any(|r| matches!(r, Removed::Kept { ino, .. } if *ino == old_id)));
            }
        }

        // Warm-up allocates up to the working set; past that the cache answers
        // every request and no further allocation happens.
        assert!(
            allocations <= 4,
            "the cache must settle at the working set, allocated {allocations} times"
        );
        assert!(
            pool.owned() <= 4,
            "the pool must settle at the working set, owns {}",
            pool.owned()
        );
    }

    #[test]
    fn drain_returns_every_held_buffer() {
        let g = geom(16, 16);
        let mut pool = ReusePool::<Id>::new(4);
        let latch = hand_out(&mut pool, g, 7, 7);
        pool.mark_live(7);
        latch.store(true, Ordering::Release);
        pool.retire(&HashSet::new(), None);
        // A different geometry, so this one is a genuine second buffer rather
        // than the one just retired.
        let latch2 = hand_out(&mut pool, geom(32, 32), 8, 8);
        pool.mark_live(8);
        latch2.store(true, Ordering::Release);
        let mut drained = pool.drain();
        drained.sort_unstable();
        assert_eq!(drained, vec![7, 8]);
        assert_eq!(pool.owned(), 0);
    }

    /// A length-only request — what the shim sends for a window-sized scan-out
    /// allocation it cannot size from a connector mode — resolves to a geometry
    /// the daemon knows, and a length no known geometry produces stays
    /// kernel-owned (the 4096-byte heap allocations).
    #[test]
    fn length_only_request_resolves_to_a_known_geometry() {
        let mut pool = ReusePool::<Id>::new(8);
        let mode = geom(1280, 960);
        let window = geom(800, 600);

        // Unknown until the daemon learns it: from a connector mode or from a
        // framebuffer the client commits.
        assert_eq!(pool.resolve_len(window.alloc_len(), window.format), None);
        pool.note_geometry(mode, mode.alloc_len());
        pool.note_geometry(window, window.alloc_len());

        // A bare length now resolves to the exact geometry, and a length-only
        // request behaves exactly like a geometry one.
        assert_eq!(pool.resolve_len(mode.alloc_len(), mode.format), Some(mode));
        assert_eq!(
            pool.resolve_len(window.alloc_len(), window.format),
            Some(window)
        );
        match pool.begin_len(window.alloc_len(), window.format) {
            Some(Reservation::Cold { evicted: None }) => {}
            Some(other) => panic!(
                "{}",
                format_args!("expected a cold reservation, got {}", describe(&other))
            ),
            None => panic!("{}", "a known geometry must resolve"),
        }
        assert_eq!(
            pool.resolve_len(4096, 5),
            None,
            "4096 B is not a frame length"
        );
        assert!(pool.begin_len(4096, 5).is_none());
    }

    /// A real scan-out allocation is row-padded, so the length a length-only
    /// request carries is the padded, page-rounded size — never the tight
    /// `width * height * 4`. The index must be keyed on that real length (what
    /// the `ADDFB2` feed reads from the GEM's `st_size`), or the layer is
    /// refused and re-publishes a stale buffer.
    #[test]
    fn padded_allocation_length_resolves_to_its_geometry() {
        let mut pool = ReusePool::<Id>::new(4);
        // 251x570 with pitch 1024: 1024*570 rounds up to 585728 at a 4096-byte
        // page, which is neither 251*570*4 (572280) nor page-aligned tight.
        let g = geom(251, 570);
        let padded = 585_728u64;
        assert_ne!(padded, g.alloc_len());
        assert_eq!(pool.resolve_len(padded, g.format), None);
        pool.note_geometry(g, padded);
        assert_eq!(pool.resolve_len(padded, g.format), Some(g));
        assert_eq!(
            pool.resolve_len(g.alloc_len(), g.format),
            None,
            "the tight length is not what the allocation reported"
        );
    }

    /// A compressed (UBWC) scan-out allocation is longer than both the tight
    /// and the row-padded linear size: the Adreno UBWC surface for 1280x960 is
    /// 4947968 B, not the 4915200 B a LINEAR image needs. The `ADDFB2` feed
    /// registers a client framebuffer with that real length, so a length-only
    /// scan-out request for it must resolve once the geometry is noted — the
    /// round-U refusal was this exact lookup returning `None`. Both classes
    /// resolve; neither shadows the other.
    #[test]
    fn compressed_allocation_length_resolves_to_its_geometry() {
        let mut pool = ReusePool::<Id>::new(4);
        let g = geom(1280, 960);
        let linear = g.alloc_len();
        let ubwc = 4_947_968u64;
        assert_eq!(linear, 4_915_200);
        assert_ne!(linear, ubwc);
        assert_eq!(
            pool.resolve_len(ubwc, g.format),
            None,
            "an unregistered compressed length must not resolve"
        );
        pool.note_geometry(g, linear);
        pool.note_geometry(g, ubwc);
        assert_eq!(pool.resolve_len(linear, g.format), Some(g));
        assert_eq!(
            pool.resolve_len(ubwc, g.format),
            Some(g),
            "the compressed allocation length resolves to its geometry"
        );
    }

    #[test]
    fn row_padded_mode_length_resolves_before_its_real_length_is_registered() {
        let mut pool = ReusePool::<Id>::new(4);
        let panel = geom(2780, 1264);
        pool.note_geometry(panel, panel.alloc_len());
        assert_eq!(pool.resolve_len(14_237_696, panel.format), Some(panel));
        assert_eq!(pool.resolve_len(106_496, panel.format), None);
        assert_eq!(pool.resolve_len(14_237_696, panel.format + 1), None);
    }

    #[test]
    fn padded_match_shorter_than_the_request_is_refused() {
        let mut pool = ReusePool::<Id>::new(4);
        let window = geom(1280, 800);
        let tall = geom(1280, 960);
        pool.note_geometry(window, window.alloc_len());
        let len = tall.alloc_len();
        assert_eq!(len, 4_915_200);
        assert_eq!(pool.resolve_len(len, window.format), Some(window));
        assert!(!serves_len(window.alloc_len(), len));
        assert!(serves_len(len, len));
        pool.note_geometry(tall, len);
        assert_eq!(pool.resolve_len(len, tall.format), Some(tall));
    }

    #[test]
    fn row_padding_never_stands_in_for_a_different_size() {
        let mut pool = ReusePool::<Id>::new(4);
        let panel = geom(2560, 1600);
        pool.note_geometry(panel, panel.alloc_len());
        assert_eq!(
            pool.resolve_len(geom(1280, 800).alloc_len(), panel.format),
            None
        );
        assert_eq!(
            pool.resolve_len(panel.alloc_len() + 4096, panel.format),
            None
        );
    }

    #[test]
    fn registered_length_wins_over_row_padding() {
        let mut pool = ReusePool::<Id>::new(4);
        let wide = geom(1280, 960);
        let narrow = geom(1250, 960);
        pool.note_geometry(narrow, narrow.alloc_len());
        assert_eq!(
            pool.resolve_len(wide.alloc_len(), wide.format),
            Some(narrow)
        );
        pool.note_geometry(wide, wide.alloc_len());
        assert_eq!(pool.resolve_len(wide.alloc_len(), wide.format), Some(wide));
    }

    #[test]
    fn least_padded_geometry_serves_an_ambiguous_length() {
        let mut pool = ReusePool::<Id>::new(4);
        let exact = geom(1280, 960);
        let padded = geom(1250, 960);
        pool.note_geometry(padded, padded.alloc_len());
        pool.note_geometry(exact, 4_947_968);
        assert_eq!(
            pool.resolve_len(exact.alloc_len(), exact.format),
            Some(exact)
        );
    }

    /// A commit teaches the pool its geometry, so a window size first seen as a
    /// framebuffer becomes resolvable by length afterwards.
    #[test]
    fn committed_geometry_becomes_resolvable_by_length() {
        let mut pool = ReusePool::<Id>::new(4);
        let g = geom(960, 600);
        assert_eq!(pool.resolve_len(g.alloc_len(), g.format), None);

        let latch = hand_out(&mut pool, g, 5, 5);
        assert_eq!(
            pool.resolve_len(g.alloc_len(), g.format),
            Some(g),
            "the committed geometry is now a pool key"
        );
        drop(latch);
    }

    /// One pool serves several geometries at once — the mode plus the window
    /// sizes games read at — with each answered from its own working set and no
    /// eviction (thrash) between them.
    #[test]
    fn multiple_geometries_share_one_pool_without_thrashing() {
        const CAP: usize = 12;
        let mut pool = ReusePool::<Id>::new(CAP);
        let geoms = [geom(1280, 960), geom(960, 600), geom(800, 600)];
        for g in geoms {
            pool.note_geometry(g, g.alloc_len());
        }

        let mut next_id: Id = 100;
        let mut allocations = 0u32;
        let mut settled_at: Option<u32> = None;
        let mut in_flight: Vec<(Id, Arc<AtomicBool>)> = Vec::new();

        for round in 0..60u32 {
            // One frame of each window gamescope is compositing, each requested
            // by length as the shim would send it.
            for g in geoms {
                let reservation = pool
                    .begin_len(g.alloc_len(), g.format)
                    .expect("known geometry");
                let (id, cold) = match reservation {
                    Reservation::Reuse { payload, .. } => (payload, false),
                    Reservation::Cold { evicted: None } => {
                        allocations += 1;
                        next_id += 1;
                        (next_id, true)
                    }
                    Reservation::Cold { evicted: Some(_) } => {
                        panic!(
                            "{}",
                            format_args!("pool thrashed: evicted a free buffer at cap {CAP}")
                        )
                    }
                    Reservation::Full => panic!("{}", format_args!("pool wedged at cap {CAP}")),
                };
                let latch = Arc::new(AtomicBool::new(false));
                pool.commit(id, g, id, Arc::clone(&latch), cold);
                pool.mark_live(id);
                in_flight.push((id, latch));
            }
            // Everything published so far flips before the retire below.
            for (_, l) in &in_flight {
                l.store(true, Ordering::Release);
            }
            // Retire everything but the three most recent (one per geometry).
            while in_flight.len() > geoms.len() {
                let (old_id, _) = in_flight.remove(0);
                let live: HashSet<u64> = in_flight.iter().map(|(id, _)| *id).collect();
                let removed = pool.retire(&live, None);
                assert!(removed
                    .iter()
                    .any(|r| matches!(r, Removed::Kept { ino, .. } if *ino == old_id)));
            }
            if round == 5 {
                settled_at = Some(allocations);
            }
        }

        let settled = settled_at.expect("round 5 ran");
        assert_eq!(
            allocations, settled,
            "allocations must stop once every geometry's working set is warm"
        );
        assert!(
            pool.owned() <= CAP,
            "the pool must stay under its cap, owns {}",
            pool.owned()
        );
        assert_eq!(pool.misses(), 0, "every length-only request resolved");
    }

    fn describe<T>(r: &Reservation<T>) -> &'static str {
        match r {
            Reservation::Reuse { .. } => "reuse",
            Reservation::Cold { .. } => "cold",
            Reservation::Full => "full",
        }
    }

    /// The mode filling the window is the identity, so a session whose scan-out size
    /// is the display's presents exactly as before this mapping existed — including a
    /// plane rect that is letterboxed inside the mode.
    #[test]
    fn layer_rect_is_the_identity_for_a_mode_that_fills_the_window() {
        assert_eq!(
            layer_rect((0, 0, 1280, 720), (1280, 720), (1280, 720)),
            (0, 0, 1280, 720)
        );
        assert_eq!(
            layer_rect((0, 27, 1280, 904), (1280, 960), (1280, 960)),
            (0, 27, 1280, 931),
            "a window letterboxed inside the mode keeps its rect"
        );
    }

    /// A mode below the window is upscaled by the display compositor: one uniform
    /// 1.5x here, covering the window.
    #[test]
    fn layer_rect_upscales_a_mode_below_the_window() {
        assert_eq!(
            layer_rect((0, 0, 1280, 720), (1280, 720), (1920, 1080)),
            (0, 0, 1920, 1080)
        );
        assert_eq!(
            layer_rect((320, 180, 640, 360), (1280, 720), (1920, 1080)),
            (480, 270, 1440, 810),
            "a window inside the mode is scaled with it"
        );
    }

    /// A mode of another aspect is letterboxed, never stretched.
    #[test]
    fn layer_rect_letterboxes_a_mode_of_another_aspect() {
        assert_eq!(
            layer_rect((0, 0, 1024, 768), (1024, 768), (1920, 1080)),
            (240, 0, 1680, 1080),
            "4:3 in a 16:9 window: pillarboxed"
        );
    }

    /// A plane rect that runs past the mode is clipped to it, so the layer never
    /// leaves the window.
    #[test]
    fn layer_rect_clips_to_the_mode() {
        assert_eq!(
            layer_rect((-40, -40, 1280 + 80, 720 + 80), (1280, 720), (1920, 1080)),
            (0, 0, 1920, 1080)
        );
    }

    #[test]
    fn cursor_rects_place_an_unclipped_cursor_one_to_one() {
        let r = cursor_rects(
            (100, 200, 64, 64),
            (0, 0, 64, 64),
            (1920, 1080),
            (1920, 1080),
        );
        assert_eq!(r, Some(((0, 0, 64, 64), (100, 200, 164, 264))));
    }

    #[test]
    fn cursor_rects_crop_the_source_at_the_mode_edge() {
        // Hanging 20 px off the left and 30 px off the bottom.
        let r = cursor_rects(
            (-20, 1050, 64, 64),
            (0, 0, 64, 64),
            (1920, 1080),
            (1920, 1080),
        );
        assert_eq!(r, Some(((20, 0, 64, 30), (0, 1050, 44, 1080))));
        // Past the right edge.
        let r = cursor_rects(
            (1900, 0, 64, 64),
            (0, 0, 64, 64),
            (1920, 1080),
            (1920, 1080),
        );
        assert_eq!(r, Some(((0, 0, 20, 64), (1900, 0, 1920, 64))));
    }

    #[test]
    fn cursor_rects_hide_a_cursor_entirely_off_the_mode() {
        assert_eq!(
            cursor_rects(
                (-64, 10, 64, 64),
                (0, 0, 64, 64),
                (1920, 1080),
                (1920, 1080)
            ),
            None
        );
        assert_eq!(
            cursor_rects(
                (1920, 10, 64, 64),
                (0, 0, 64, 64),
                (1920, 1080),
                (1920, 1080)
            ),
            None
        );
        assert_eq!(
            cursor_rects((10, 10, 0, 64), (0, 0, 64, 64), (1920, 1080), (1920, 1080)),
            None
        );
    }

    #[test]
    fn cursor_rects_follow_the_scan_out_fit_into_a_larger_window() {
        // 1280x720 mode upscaled into a 1920x1080 window: x1.5 both axes.
        let r = cursor_rects(
            (640, 360, 32, 32),
            (0, 0, 32, 32),
            (1280, 720),
            (1920, 1080),
        );
        assert_eq!(r, Some(((0, 0, 32, 32), (960, 540, 1008, 588))));
        // A scaled cursor (64 px buffer drawn at 32 px) keeps its whole source.
        let r = cursor_rects((0, 0, 32, 32), (0, 0, 64, 64), (1920, 1080), (1920, 1080));
        assert_eq!(r, Some(((0, 0, 64, 64), (0, 0, 32, 32))));
    }
}
