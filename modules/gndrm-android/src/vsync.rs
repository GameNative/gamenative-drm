//! The panel's latch grid, measured from the daemon's own publishes and pushed
//! to the presenter shim as a [`VsyncNote`].
//!
//! `SurfaceFlinger` takes the newest buffer a layer holds at each of its latch
//! points, one panel period apart, and shows it at the vsync after. The shim
//! commits generated frames on a timer of its own; unless that timer is phased
//! to the latch points its frames land unevenly, because the beat between the
//! two grids parks two commits inside one period every so often and only the
//! second is ever shown. The completion stats of every publish carry the latch
//! time `SurfaceFlinger` used, which is a sample of the grid. This module tracks
//! the grid's phase from those samples, measures how early the daemon's own
//! apply lands ahead of the latch it makes, and from that derives the lead the
//! shim should commit by so its frames arrive in the middle of a period, not on
//! an edge.

//!
//! The tracker itself is Android-free and has its unit tests run on the host
//! by `tests/vsync.rs`; only the glue below it (the eventfd, the clock, the
//! sampling entry points) is Android-only.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use gndrm_uapi::wire::VsyncNote;

/// Whether any client reads the note. The lead is measured only then: a shim
/// that commits on its own clock gives margins that do not follow the lead, and
/// the loop would walk it to a clamp.
///
/// Kept as a local flag rather than read from the subscription registry, because
/// this file is compiled on its own by the host harness
/// (`tests/vsync.rs`) and must reach nothing in the crate root. The scan-out
/// side refreshes it whenever the set of subscribers changes
/// (`asc::sync_vsync_subscription`): a subscriber keeps its registration after
/// it hands the allocator role on, and it is still placing frames by this note.
static SUBSCRIBED: AtomicBool = AtomicBool::new(false);

/// Record whether anyone reads the note (see [`SUBSCRIBED`]).
pub(crate) fn set_subscribed(on: bool) {
    SUBSCRIBED.store(on, Ordering::Relaxed);
}

/// eventfd the scan-out serve loop polls; written when the note to push
/// changed. Created lazily, once per process, and never closed: the serve loop
/// outlives nothing that could reclaim it.
#[cfg(target_os = "android")]
static CHANGED_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// The eventfd a changed note rings, or -1 when one could not be made.
#[cfg(target_os = "android")]
pub(crate) fn changed_fd() -> std::os::fd::RawFd {
    let fd = CHANGED_FD.load(Ordering::Acquire);
    if fd != -1 {
        return fd;
    }
    // SAFETY: eventfd with valid flags; the descriptor is owned by this static.
    let made = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if made < 0 {
        CHANGED_FD.store(-2, Ordering::Release);
        return -1;
    }
    match CHANGED_FD.compare_exchange(-1, made, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => made,
        Err(other) => {
            // SAFETY: `made` is a descriptor this call owns and lost the race.
            unsafe { libc::close(made) };
            if other >= 0 {
                other
            } else {
                -1
            }
        }
    }
}

/// `CLOCK_MONOTONIC` in nanoseconds, the clock `SurfaceFlinger` stamps with.
#[cfg(target_os = "android")]
pub(crate) fn now_ns() -> u64 {
    // SAFETY: clock_gettime with a valid clock id and a valid out-pointer.
    let ts = unsafe {
        let mut ts: libc::timespec = core::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        ts
    };
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}

/// A phase or lead move below this is not worth a push: the shim extrapolates
/// the grid by its period, and the margin the lead aims for is a third of one.
const PUSH_STEP_NS: u64 = 300_000;
/// A period move below this is not worth a push: the measured period wanders by
/// a few microseconds between fits, and the phase push above re-anchors the
/// shim's extrapolation long before such an error adds up.
const PERIOD_STEP_NS: u64 = 1_000;
/// The shortest lead worth asking for.
const MIN_LEAD_NS: u64 = 500_000;
/// Samples off the grid before the grid is taken to have moved.
const REPHASE_AFTER: u32 = 4;

/// The grid's state: the latch point it is anchored on, the lead the shim is
/// told, and what was last pushed.
struct Tracker {
    anchor_ns: u64,
    lead_ns: u64,
    outliers: u32,
    pushed: Option<VsyncNote>,
}

static TRACKER: Mutex<Tracker> = Mutex::new(Tracker {
    anchor_ns: 0,
    lead_ns: 0,
    outliers: 0,
    pushed: None,
});

/// The signed distance of `x` from the nearest multiple of `period`, in
/// `[-period/2, period/2)`.
fn wrap(x: i64, period: i64) -> i64 {
    let half = period / 2;
    (x + half).rem_euclid(period) - half
}

impl Tracker {
    /// Fold in one completion: the latch point `SurfaceFlinger` took the frame
    /// at and the instant the daemon's apply of it returned (0 when unknown).
    /// `pacing` says whether a shim is placing frames by the note, which is
    /// when the lead can be measured. `stamp_lead` is how far ahead of a latch
    /// the flip events are stamped now (0 when they follow no grid); it rides
    /// in the note so the shim can find gamescope's vblank grid. Returns the
    /// note to push when it moved.
    fn track(
        &mut self,
        latch_ns: u64,
        applied_ns: u64,
        period: u64,
        pacing: bool,
        stamp_lead: u64,
    ) -> Option<VsyncNote> {
        if self.anchor_ns == 0 {
            self.anchor_ns = latch_ns;
            self.lead_ns = (period / 2 + 1_000_000).min(period * 3 / 4);
        } else {
            // The grid point this sample should sit on, and how far off it is.
            let offset = latch_ns as i64 - self.anchor_ns as i64;
            let period_i = period as i64;
            let err = wrap(offset, period_i);
            let predicted = latch_ns as i64 - err;
            if err.abs() > period_i / 4 {
                // Off the grid: `SurfaceFlinger` moved its latch phase, or this was
                // a frame it took late. A run of them is the former.
                self.outliers += 1;
                if self.outliers >= REPHASE_AFTER {
                    self.anchor_ns = latch_ns;
                    self.outliers = 0;
                }
            } else {
                self.outliers = 0;
                self.anchor_ns = (predicted + err / 8) as u64;
            }
        }
        // How early the frame was: the apply returned `margin` before the latch
        // took it. Aim for half a period, the middle of the window between two
        // latch points, so the pipeline's jitter either way still makes the
        // same point (measured on an Odin 3 at 120 Hz: a third of a period left
        // the late tail on the edge, and 8 % of the frames a period late). The
        // error is taken modulo the period: a frame that missed its point by a
        // hair shows as almost a whole period early, and the wrap reads it as
        // late, which is what it was.
        if pacing && applied_ns != 0 && latch_ns > applied_ns {
            let margin = latch_ns - applied_ns;
            if margin < 4 * period {
                let target = period / 2;
                let d = wrap(margin as i64 - target as i64, period as i64);
                let lead = self.lead_ns as i64 - d / 8;
                self.lead_ns = lead.clamp(MIN_LEAD_NS as i64, 2 * period as i64) as u64;
            }
        }
        let note = VsyncNote {
            anchor_ns: self.anchor_ns,
            period_ns: u32::try_from(period).unwrap_or(u32::MAX),
            lead_ns: u32::try_from(self.lead_ns).unwrap_or(u32::MAX),
            stamp_lead_ns: u32::try_from(stamp_lead).unwrap_or(u32::MAX),
        };
        let moved = match self.pushed {
            None => true,
            Some(p) => {
                let phase =
                    wrap(note.anchor_ns as i64 - p.anchor_ns as i64, period as i64).unsigned_abs();
                u64::from(p.period_ns.abs_diff(note.period_ns)) > PERIOD_STEP_NS
                    || phase > PUSH_STEP_NS
                    || u64::from(p.lead_ns.abs_diff(note.lead_ns)) > PUSH_STEP_NS
                    || u64::from(p.stamp_lead_ns.abs_diff(note.stamp_lead_ns)) > PUSH_STEP_NS
            }
        };
        if moved {
            self.pushed = Some(note);
            Some(note)
        } else {
            None
        }
    }

    /// The current note, recorded as pushed; `None` until a latch was seen.
    fn note(&mut self, period: u64, stamp_lead: u64) -> Option<VsyncNote> {
        if self.anchor_ns == 0 || period == 0 {
            return None;
        }
        let note = VsyncNote {
            anchor_ns: self.anchor_ns,
            period_ns: u32::try_from(period).unwrap_or(u32::MAX),
            lead_ns: u32::try_from(self.lead_ns).unwrap_or(u32::MAX),
            stamp_lead_ns: u32::try_from(stamp_lead).unwrap_or(u32::MAX),
        };
        self.pushed = Some(note);
        Some(note)
    }
}

/// The panel period in nanoseconds: the display's measured vsync spacing while
/// Choreographer reports it (`gndrm_daemon::vblank`), else the rate the app
/// reported, 0 when neither. The samples cannot give the period on their own:
/// while the daemon is the only thing publishing, their spacing is the daemon's
/// own rate.
#[cfg(target_os = "android")]
fn period_ns(now_ns: u64) -> u64 {
    if let Some(p) = gndrm_daemon::vblank::measured_period_ns(now_ns) {
        return p;
    }
    let mhz = crate::panel_refresh_mhz();
    if mhz == 0 {
        0
    } else {
        1_000_000_000_000 / u64::from(mhz)
    }
}

/// How far ahead of its latch the flip events are stamped at `now_ns`, 0 while
/// they follow no display grid (`gndrm_daemon::vblank::display_grid`).
#[cfg(target_os = "android")]
fn stamp_lead_ns(now_ns: u64) -> u64 {
    gndrm_daemon::vblank::display_grid(now_ns).map_or(0, |g| g.lead_ns)
}

/// One publish's completion: the latch point `SurfaceFlinger` took it at and
/// the instant the daemon's apply of it returned. Called from the completion
/// callback, on `SurfaceFlinger`'s thread.
#[cfg(target_os = "android")]
pub(crate) fn sample(latch_ns: u64, applied_ns: u64) {
    let period = period_ns(latch_ns);
    if period == 0 || latch_ns == 0 {
        return;
    }
    let pacing = crate::lsfg_pacing() && SUBSCRIBED.load(Ordering::Relaxed);
    // Read before the tracker's lock: the display's state has a lock of its own.
    let stamp_lead = stamp_lead_ns(latch_ns);
    let note = {
        let mut t = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
        let first = t.pushed.is_none();
        let note = t.track(latch_ns, applied_ns, period, pacing, stamp_lead);
        // The flip events' fallback timeline, and Choreographer's cross-check.
        gndrm_daemon::vblank::note_latch_grid(t.anchor_ns, period);
        if first && note.is_some() {
            log_info!(
                "{}",
                gndrm_obf::obf!(
                    "vsync: panel latch grid period={}us; pushing it to the presenter",
                    period / 1000
                )
            );
        }
        note
    };
    // The flip events' lead and the periodic cross-check (one line per 10 s).
    if let Some(r) = gndrm_daemon::vblank::note_publish_latch(applied_ns, latch_ns) {
        crate::choreographer::log_report(&r);
    }
    if note.is_some() {
        let fd = changed_fd();
        if fd >= 0 {
            let one: u64 = 1;
            // SAFETY: the eventfd this module owns, non-blocking; one increment
            // cannot fill the counter.
            unsafe { libc::write(fd, (&raw const one).cast(), 8) };
        }
    }
}

/// The note to hand a shim now, `None` before any latch was seen.
#[cfg(target_os = "android")]
pub(crate) fn note() -> Option<VsyncNote> {
    let now = now_ns();
    let period = period_ns(now);
    let stamp_lead = stamp_lead_ns(now);
    TRACKER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .note(period, stamp_lead)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: u64 = 8_333_333;

    fn fresh() -> Tracker {
        Tracker {
            anchor_ns: 0,
            lead_ns: 0,
            outliers: 0,
            pushed: None,
        }
    }

    #[test]
    fn wrap_is_the_signed_distance_to_the_nearest_grid_point() {
        let p = 100;
        assert_eq!(wrap(0, p), 0);
        assert_eq!(wrap(10, p), 10);
        assert_eq!(wrap(90, p), -10);
        assert_eq!(wrap(-10, p), -10);
        assert_eq!(wrap(250, p), -50);
        assert_eq!(wrap(149, p), 49);
    }

    #[test]
    fn the_first_sample_anchors_the_grid_and_is_pushed() {
        let mut t = fresh();
        let note = t
            .track(1_000_000_000, 0, P, false, 0)
            .expect("first sample pushes");
        assert_eq!(note.anchor_ns, 1_000_000_000);
        assert_eq!(u64::from(note.period_ns), P);
        assert_eq!(u64::from(note.lead_ns), P / 2 + 1_000_000);
    }

    #[test]
    fn samples_on_the_grid_do_not_push_again() {
        let mut t = fresh();
        t.track(1_000_000_000, 0, P, false, 0);
        for k in 1..200 {
            // Jitter of +-100 us, as `SurfaceFlinger`'s latch wakeups have.
            let jitter = if k % 2 == 0 { 100_000 } else { -100_000 };
            let latch = (1_000_000_000 + k * P) as i64 + jitter;
            assert_eq!(t.track(latch as u64, 0, P, false, 0), None, "sample {k}");
        }
        // The anchor stayed within the jitter of the grid.
        assert!(wrap(t.anchor_ns as i64 - 1_000_000_000, P as i64).unsigned_abs() < 150_000);
    }

    #[test]
    fn a_moved_latch_phase_is_adopted_after_a_run_of_samples() {
        let mut t = fresh();
        t.track(1_000_000_000, 0, P, false, 0);
        // `SurfaceFlinger` moves its latch 3 ms later from now on.
        let shift = 3_000_000;
        let mut pushed = None;
        for k in 1..=REPHASE_AFTER as u64 + 1 {
            if let Some(n) = t.track(1_000_000_000 + k * P + shift, 0, P, false, 0) {
                pushed = Some(n);
            }
        }
        let note = pushed.expect("the new phase is pushed");
        assert_eq!(
            wrap(
                note.anchor_ns as i64 - 1_000_000_000 - shift as i64,
                P as i64
            ),
            0
        );
    }

    #[test]
    fn a_single_late_sample_is_ignored() {
        let mut t = fresh();
        t.track(1_000_000_000, 0, P, false, 0);
        assert_eq!(t.track(1_000_000_000 + P + 3_000_000, 0, P, false, 0), None);
        assert_eq!(t.track(1_000_000_000 + 2 * P, 0, P, false, 0), None);
        assert_eq!(wrap(t.anchor_ns as i64 - 1_000_000_000, P as i64), 0);
    }

    /// Run the lead loop against a modelled pipeline: the shim commits
    /// `lead` before a latch point, the frame reaches `SurfaceFlinger`
    /// `pipeline` later, and is taken at the first latch point after that.
    fn settle(pipeline: u64) -> u64 {
        let mut t = fresh();
        t.track(1_000_000_000, 0, P, true, 0);
        for k in 1..400u64 {
            let target = 1_000_000_000 + k * P;
            let applied = target - t.lead_ns + pipeline;
            // The latch that actually takes it.
            let latch = if applied < target { target } else { target + P };
            t.track(latch, applied, P, true, 0);
        }
        t.lead_ns
    }

    #[test]
    fn the_lead_settles_half_a_period_ahead_of_a_latch() {
        // The margin is half a period modulo the period: a pipeline longer
        // than a period settles one latch point later, which is still one
        // frame per period, evenly.
        for pipeline in [500_000u64, 2_000_000, 4_000_000, 6_000_000, 9_000_000] {
            let lead = settle(pipeline);
            let margin = wrap(lead as i64 - pipeline as i64 - (P / 2) as i64, P as i64);
            assert!(
                margin.abs() < 400_000,
                "pipeline {pipeline}: lead {lead} off by {margin}"
            );
        }
    }

    #[test]
    fn the_lead_is_not_touched_while_nothing_paces_on_it() {
        let mut t = fresh();
        t.track(1_000_000_000, 0, P, false, 0);
        let lead = t.lead_ns;
        for k in 1..50u64 {
            t.track(
                1_000_000_000 + k * P,
                1_000_000_000 + k * P - 100_000,
                P,
                false,
                0,
            );
        }
        assert_eq!(t.lead_ns, lead);
    }

    #[test]
    fn the_stamp_lead_rides_in_the_note_and_a_move_is_pushed() {
        let mut t = fresh();
        let first = t
            .track(1_000_000_000, 0, P, false, 2_000_000)
            .expect("first sample pushes");
        assert_eq!(first.stamp_lead_ns, 2_000_000);
        // A wander inside the push step is not worth a push.
        assert_eq!(t.track(1_000_000_000 + P, 0, P, false, 2_200_000), None);
        // A move past it is.
        let moved = t
            .track(1_000_000_000 + 2 * P, 0, P, false, 2_400_000)
            .expect("a moved stamp lead is pushed");
        assert_eq!(moved.stamp_lead_ns, 2_400_000);
        assert_eq!(
            t.note(P, 2_400_000).map(|n| n.stamp_lead_ns),
            Some(2_400_000)
        );
    }

    #[test]
    fn note_is_none_until_a_latch_was_seen() {
        let mut t = fresh();
        assert_eq!(t.note(P, 0), None);
        t.track(1_000_000_000, 0, P, false, 0);
        assert!(t.note(P, 0).is_some());
        assert_eq!(t.note(0, 0), None);
    }
}
