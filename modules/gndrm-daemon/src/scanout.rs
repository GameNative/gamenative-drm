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
//!   session's working set and then allocates nothing more.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Geometry a pooled buffer can serve. A free buffer only matches a request of
/// the exact same width, height and format.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Geometry {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Gralloc format (the presenter's single BGRA layout).
    pub format: u32,
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
struct Free<T> {
    geom: Geometry,
    payload: T,
}

/// What [`ReusePool::begin`] reserved for one request.
pub enum Reservation<T> {
    /// A retired buffer of the requested geometry: hand it straight out, no
    /// allocation. This is the steady state.
    Reuse(T),
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

    /// Take a retired buffer of exactly `geom`, if one is held. This is the
    /// allocation-free answer and is preferred over every other. `remove`
    /// (not `swap_remove`) keeps the free list ordered oldest-first, which the
    /// over-cap eviction relies on.
    pub fn take_reusable(&mut self, geom: Geometry) -> Option<T> {
        let i = self.free.iter().position(|f| f.geom == geom)?;
        Some(self.free.remove(i).payload)
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
        if let Some(payload) = self.take_reusable(geom) {
            return Reservation::Reuse(payload);
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
    pub fn recycle(&mut self, geom: Geometry, payload: T) {
        self.free.push(Free { geom, payload });
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

    fn geom(width: u32, height: u32) -> Geometry {
        Geometry {
            width,
            height,
            format: 5,
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
            Reservation::Reuse(p) => (p, false),
            Reservation::Cold { .. } => (cold_id, true),
            Reservation::Full => panic!("pool reported full at {g:?}"),
        };
        pool.commit(id, g, payload, Arc::clone(&latch), cold);
        latch
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
            Reservation::Reuse(100) => {}
            other => panic!(
                "expected the retired buffer to be handed back out, got {}",
                describe(&other)
            ),
        }
        assert_eq!(pool.free_len(), 0, "the reused buffer left the free list");
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
                "expected a cold allocation while the buffer is in flight, got {}",
                describe(&other)
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
            other => panic!("expected an eviction, got {}", describe(&other)),
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
                Reservation::Reuse(id) => (id, false),
                Reservation::Cold { .. } => {
                    allocations += 1;
                    next_id += 1;
                    (next_id, true)
                }
                Reservation::Full => panic!("pool unexpectedly full at frame {frame}"),
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

    fn describe<T>(r: &Reservation<T>) -> &'static str {
        match r {
            Reservation::Reuse(_) => "reuse",
            Reservation::Cold { .. } => "cold",
            Reservation::Full => "full",
        }
    }
}
