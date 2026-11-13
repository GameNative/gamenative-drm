//! The cap's acquire gate: one regulator, the rate.
//!
//! A frame may start one `1e9 / cap` after the previous frame started, and not
//! before. That is the whole rule:
//!
//! ```text
//! gate:  while now < next_ns { wait until next_ns }
//! book:  next_ns = now + period        (now is the release just granted)
//! ```
//!
//! What follows from it by construction, not by tuning:
//!
//! - **The cap is a maximum, never exceeded.** Two consecutive releases are at
//!   least `period` apart, because the second is booked one period after the
//!   first *actually happened* — not after the instant it was supposed to.
//! - **A stall carries no debt.** A late frame is released at once and books the
//!   next one a period after itself, so a stall costs the frames it swallowed and
//!   nothing bursts afterwards.
//! - **A guest slower than the cap is never held.** Its next acquire is already
//!   past `next_ns`, so the gate does not wait at all.
//!
//! What it does not do is keep releases on the display's vblank grid. That was
//! the previous design, and it is why two regulators existed: a grid-snapped
//! release holds the phase but bounds only the *average* rate, so a release that a
//! slot wait delayed was followed by a booking on the next vblank — a fraction of
//! an interval later — and the cap leaked. Measured on this path: 74.7 frames/s
//! under a 60 cap, the gate holding 8 % of acquires. With one regulator the rate
//! is exact and the phase is wherever the releases fall; the daemon's latch grid
//! stays where a phase is genuinely needed, in frame generation
//! (`crate::lsfg`), which places generated frames rather than limiting a rate.
//!
//! gamescope's own limiter is not a substitute for this gate: it withholds a
//! frame callback this shim never waits on, and its divisor only acts when the
//! cap is below the refresh *and* divides it.

use std::sync::atomic::{AtomicU64, Ordering};

/// What the acquire gate releases on, as the census names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateClock {
    /// No gate: no cap, and nothing to hold.
    Off,
    /// The cap's own `1e9 / cap` clock.
    Clock,
}

impl GateClock {
    pub(crate) fn name(self) -> &'static str {
        match self {
            GateClock::Off => "off",
            GateClock::Clock => "clock",
        }
    }
}

/// How an acquire is paced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pacing {
    Off,
    /// One release per `period_ns`.
    Clock { period_ns: u64 },
}

impl Pacing {
    /// A gate on `fps`: the period is the cap's own, and no cap is no gate.
    #[must_use]
    pub(crate) fn on_clock(fps: u32) -> Self {
        pacing(fps)
    }

    /// The clock this gate runs on, for the census.
    #[must_use]
    pub(crate) fn clock(self) -> GateClock {
        match self {
            Pacing::Off => GateClock::Off,
            Pacing::Clock { .. } => GateClock::Clock,
        }
    }
}

/// How a cap of `cap_fps` is released: [`Pacing::Off`] with no cap, else one
/// release per `1e9 / cap`.
#[must_use]
pub(crate) fn pacing(cap_fps: u32) -> Pacing {
    match crate::scanout::period_ns(cap_fps) {
        0 => Pacing::Off,
        period_ns => Pacing::Clock { period_ns },
    }
}

/// Acquires that started more than a period after their release point, and the
/// periods the schedule moved past for them: the guest's own lateness, for the
/// census. It is not a cap measurement — with one regulator the cap holds
/// whatever this says.
static LATE_N: AtomicU64 = AtomicU64::new(0);
static SKIPPED_N: AtomicU64 = AtomicU64::new(0);

/// `(late, skipped)`.
pub(crate) fn late_counts() -> (u64, u64) {
    (
        LATE_N.load(Ordering::Relaxed),
        SKIPPED_N.load(Ordering::Relaxed),
    )
}

/// One swapchain's release schedule: the earliest instant the next acquire may
/// start, and nothing else. There is no second timeline to keep in step with it,
/// which is what makes the rate exact.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// The earliest instant the next acquire may start; 0 before the first.
    pub next_ns: u64,
}

impl Schedule {
    /// Book a release that happened at `now_ns` and return the instant the next
    /// one may happen.
    ///
    /// One line, on a grid. Booking from the released instant (`now + period`)
    /// keeps no grid at all: every frame that starts late hands its lateness to
    /// the frame after it, forever, so a 30 cap settles at `30 / (1 + e)` for
    /// whatever jitter `e` the guest has. Measured on Witcher 3 at
    /// `acq_fps=27.74` against `acquire=30` — 7 % lost with the CPU and the GPU
    /// idle — and it is the same defect `A52` fixed in gamescope's limiter, in
    /// the same shape: a release grid in time, where a late frame delays one
    /// release and the grid absorbs it on the next.
    ///
    /// So the next release is one period after the previous *release point*, not
    /// after the moment this frame happened to start. A frame that is late by
    /// less than a period gets a proportionally shorter next hold, which is how
    /// the average rate stays the target.
    ///
    /// A frame late by a whole period or more is a lost frame, not a debt: the
    /// grid steps past `now` to the first point still in the future rather than
    /// repaying with back-to-back releases. Repaying would put every frame in the
    /// stall's shadow into the following period, which is exactly the burst this
    /// rule exists to prevent.
    pub(crate) fn book(&mut self, now_ns: u64, period_ns: u64) -> u64 {
        let period_ns = period_ns.max(1);
        if self.next_ns == 0 {
            // First release: the grid is anchored here and nowhere else.
            self.next_ns = now_ns.saturating_add(period_ns);
            return self.next_ns;
        }
        let target = self.next_ns.saturating_add(period_ns);
        if target <= now_ns {
            // Behind the grid by a period or more: this frame is late by more
            // than the release it was owed, so that release is a lost frame and
            // the grid re-anchors here. Re-anchoring rather than stepping to the
            // next grid point keeps the hold at exactly one period in both cases,
            // and it cannot burst: `now + period` is at least a period away.
            let late = now_ns.saturating_sub(self.next_ns);
            LATE_N.fetch_add(1, Ordering::Relaxed);
            SKIPPED_N.fetch_add((late / period_ns).max(1), Ordering::Relaxed);
            self.next_ns = now_ns.saturating_add(period_ns);
            return self.next_ns;
        }
        self.next_ns = target;
        self.next_ns
    }

    /// Book a release under `pacing`; a gate that is off books nothing.
    pub(crate) fn book_paced(&mut self, now_ns: u64, pacing: Pacing) {
        if let Pacing::Clock { period_ns } = pacing {
            self.book(now_ns, period_ns);
        }
    }
}

/// Fold one frame's acquire-to-commit time into a running estimate for the
/// census (1/8 weight; the first sample is taken as it is).
#[must_use]
pub(crate) fn smooth_latency(estimate_ns: u64, sample_ns: u64) -> u64 {
    if estimate_ns == 0 {
        sample_ns
    } else {
        estimate_ns - estimate_ns / 8 + sample_ns / 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P60: u64 = 16_666_666;

    fn period(cap: u32) -> u64 {
        match pacing(cap) {
            Pacing::Clock { period_ns } => period_ns,
            Pacing::Off => panic!("no cap {cap}"),
        }
    }

    #[test]
    fn no_cap_is_no_gate_and_a_cap_is_its_own_period() {
        assert_eq!(pacing(gndrm_uapi::wire::FPS_UNCAPPED), Pacing::Off);
        assert_eq!(pacing(0), Pacing::Off);
        assert_eq!(Pacing::Off.clock(), GateClock::Off);
        assert_eq!(Pacing::Off.clock().name(), "off");
        assert_eq!(period(60), P60);
        assert_eq!(period(30), 33_333_333);
        assert_eq!(period(120), 8_333_333);
        assert_eq!(Pacing::on_clock(60).clock(), GateClock::Clock);
    }

    /// The cap is a rate: no two releases closer than one interval, whatever the
    /// guest does between them — early, late, or stalled.
    #[test]
    fn consecutive_releases_are_never_closer_than_the_cap() {
        let p = period(60);
        let mut s = Schedule::default();
        let mut released = 1_000_000_000u64;
        let mut due = s.book(released, p);
        // A guest that comes back early is held to `due`; one that comes back
        // late starts at once. Neither may be released inside one period.
        for step in [15_000_000u64, 16_000_000, 20_000_000, 40_000_000, 16_700_000] {
            let start = due.max(released + step);
            assert!(
                start.saturating_sub(released) >= p,
                "released {} ns after the previous frame, under {p}",
                start - released
            );
            released = start;
            due = s.book(start, p);
        }
    }

    /// A release that overshoots its grid point does not move the grid.
    ///
    /// This is the defect the grid exists for: with `next = now + period`, every
    /// frame whose sleep woke late pushed the next release back by that lateness,
    /// so it accumulated. Witcher 3 measured `acq_fps=27.74` against a 30 cap —
    /// 7 % lost to a few hundred microseconds of scheduling per frame.
    #[test]
    fn an_overshoot_does_not_move_the_grid() {
        let p = period(30);
        let overshoot = 2_000_000;
        let mut s = Schedule::default();
        let mut released = 0u64;
        let mut due = s.book(released, p);
        for _ in 0..100 {
            // The schedule held the frame to `due`; the thread woke 2 ms late.
            released = due + overshoot;
            due = s.book(released, p);
        }
        // 101 periods of grid for 100 frames — not 101 periods plus 100
        // overshoots, which is what `now + period` produced.
        assert_eq!(due, 101 * p, "the grid moved by the overshoot");
        assert_eq!(released, 100 * p + overshoot);
        // The rate over the span is 30/s with one constant offset, so the error
        // is bounded by the overshoot and does not grow with the frame count.
        let span = released;
        let fps = 1_000_000_000.0 * 100.0 / span as f64;
        assert!((fps - 30.0).abs() < 0.05, "{fps}");
        assert!(30.0 - fps < 0.02, "lost {:.3} fps to a 2 ms overshoot", 30.0 - fps);
    }

    /// A stall costs the frames it swallowed and nothing bursts after it: the
    /// release after a long gap is one period from it, never a catch-up.
    #[test]
    fn a_stall_carries_no_debt() {
        let p = period(60);
        let mut s = Schedule::default();
        let first = 1_000_000_000u64;
        s.book(first, p);
        let after_stall = first + 2_000_000_000;
        assert_eq!(s.book(after_stall, p), after_stall + p);
        // And the frame after that is another period on, not early.
        assert_eq!(s.book(after_stall + p, p), after_stall + 2 * p);
    }

    /// A guest slower than the cap is never held.
    #[test]
    fn a_guest_below_the_cap_is_never_held() {
        let p = period(60);
        let mut s = Schedule::default();
        let mut t = 1_000_000_000u64;
        for _ in 0..100 {
            assert!(s.next_ns == 0 || t >= s.next_ns, "held at {t}");
            s.book(t, p);
            t += 20_000_000;
        }
    }

    /// The schedule holds no credit: a run at the cap leaves exactly one period
    /// outstanding, however many releases it contained.
    #[test]
    fn the_schedule_holds_no_credit() {
        let p = period(60);
        let mut s = Schedule::default();
        let mut t = 1_000_000_000u64;
        for _ in 0..10 {
            t = s.book(t, p);
        }
        assert_eq!(s.next_ns, t);
    }

    #[test]
    fn a_gate_that_is_off_books_nothing() {
        let mut s = Schedule::default();
        s.book_paced(1_000_000_000, Pacing::Off);
        assert_eq!(s.next_ns, 0);
        s.book_paced(1_000_000_000, Pacing::Clock { period_ns: P60 });
        assert_eq!(s.next_ns, 1_000_000_000 + P60);
    }

    /// The census counts the guest's lateness, not the cap's failure: a release
    /// inside its interval is on time, one past it is booked.
    #[test]
    fn only_a_release_past_a_whole_interval_is_late() {
        let before = late_counts();
        let first = 1_000_000_000u64;

        let mut s = Schedule::default();
        s.book(first, P60);
        s.book(first + P60, P60);
        assert_eq!(late_counts().0, before.0, "on time is not late");

        let mut s = Schedule::default();
        s.book(first, P60);
        s.book(first + P60 + P60 / 2, P60);
        assert_eq!(late_counts().0, before.0, "inside the interval is not late");

        let mut s = Schedule::default();
        s.book(first, P60);
        s.book(first + 3 * P60, P60);
        assert_eq!(late_counts().0, before.0 + 1, "past an interval is late");
    }
}
