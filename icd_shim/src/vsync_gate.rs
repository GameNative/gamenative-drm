//! The cap's acquire gate when gamescope's limiter does not hold the frames.
//!
//! gamescope's limiter holds only commits declared FIFO
//! (`wlserver_surface_is_fifo`): a mailbox, immediate or FIFO-relaxed commit is
//! taken at the next composite whatever the limiter says. When the container
//! asks for one of those modes and a cap is set, the shim's acquire gate is
//! the cap. A gate on a clock of its own (one release every `1e9 / cap` ns)
//! beats against the display: a 60 cap on a 59.97 Hz panel put two releases in
//! one vblank every half minute or so and ran 57-59 fps. This gate releases
//! frames on the display's vblank grid instead, the grid the daemon stamps its
//! flip events on and gamescope composites from.
//!
//! The grid is `anchor - stamp_lead + k * period` from the daemon's
//! [`VsyncNote`]: `SurfaceFlinger`'s latch points, measured, moved earlier by
//! the lead the daemon stamps flip events with. gamescope wakes to composite
//! just before each of those instants (wakeup = stamp + interval - draw time -
//! red zone), so a release on one starts the guest's frame right after
//! gamescope took the previous frame and gives it the whole period, less
//! gamescope's own margin, to reach the next composite.
//!
//! Which grid points release a frame:
//! - a cap at the display's rate (within [`SNAP_TOLERANCE`]): every vblank;
//! - a cap that divides the display's rate: every `refresh / cap`-th vblank;
//! - any other cap below the rate: the vblank nearest each instant of an ideal
//!   `1e9 / cap` timeline, which averages the cap on vblank boundaries (45 on
//!   60 Hz releases 3 of every 4 vblanks);
//! - a cap above the rate: every vblank already gets a frame, so the gate runs
//!   on the cap's own clock ([`crate::scanout::next_acquire_after`]).
//!
//! A frame that arrives after its release point is not held: it starts at once,
//! and the release after it is the next point of the schedule, so a guest
//! slower than the cap runs at its own rate rather than at a divisor of the
//! display's.
//!
//! The grid exists only once the daemon has sent a note with a stamp lead
//! ([`current_grid`]). Released on the latch points themselves (a daemon that
//! predates the field sends 0), a frame starts ~2 ms after gamescope's wakeup
//! and straddles the next composite: measured on a Retroid Pocket 6, 58.3 fps
//! and 102 frames replaced in gamescope's mailbox per minute at a 60 cap on a
//! 59.975 Hz panel, and 13 % of the daemon's publishes late at 120 Hz. Until
//! the gate runs on its own clock at the cap period. gamescope's limiter is not
//! a substitute: it withholds a frame callback this shim never waits on, and its
//! divisor only acts when the cap is below the refresh and divides it.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use gndrm_uapi::wire::{VsyncNote, FPS_UNCAPPED};

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// How close `refresh / cap` has to be to a whole number for the cap to be
/// taken as that divisor of the display's rate, as a fraction of it. A "60 Hz"
/// panel measures 59.94-59.99 Hz, 0.02-0.1 % off a 60 cap; 0.5 % keeps a 59
/// cap (1.7 % off) on the averaging timeline, where it gets 59.
const SNAP_TOLERANCE: f64 = 0.005;

/// The display's vblank grid as the daemon last reported it; a period of 0
/// means no report yet. Atomics: written by whichever thread reads the scan-out
/// channel, read by every acquire.
static ANCHOR_NS: AtomicU64 = AtomicU64::new(0);
static PERIOD_NS: AtomicU32 = AtomicU32::new(0);
static STAMP_LEAD_NS: AtomicU32 = AtomicU32::new(0);

/// Adopt the daemon's latch grid (see [`VsyncNote`]).
pub(crate) fn apply_note(note: VsyncNote) {
    ANCHOR_NS.store(note.anchor_ns, Ordering::Relaxed);
    STAMP_LEAD_NS.store(note.stamp_lead_ns, Ordering::Relaxed);
    PERIOD_NS.store(note.period_ns, Ordering::Relaxed);
}

/// The vblank grid releases sit on: the daemon's flip-stamp grid, `None`
/// until a note carrying a stamp lead has arrived (0 means the daemon did not
/// say; one that measures a lead always sends at least 1 ns).
pub(crate) fn current_grid() -> Option<Grid> {
    let period = u64::from(PERIOD_NS.load(Ordering::Relaxed));
    let lead = u64::from(STAMP_LEAD_NS.load(Ordering::Relaxed));
    if period == 0 || lead == 0 {
        return None;
    }
    let anchor = ANCHOR_NS.load(Ordering::Relaxed);
    Some(Grid {
        anchor_ns: anchor.saturating_sub(lead),
        period_ns: period,
        clock: GateClock::Vsync,
    })
}

/// What the acquire gate releases on, as the census names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateClock {
    /// No gate: nothing to cap, or gamescope's limiter holds the frames.
    Off,
    /// The display's vblank grid from the daemon's note.
    Vsync,
    /// The cap's own `1e9 / cap` clock: a cap above the display's rate, a
    /// gate that is not this module's (frame generation's source rate, the X11
    /// Present route), or no grid known.
    Clock,
}

impl GateClock {
    pub(crate) fn name(self) -> &'static str {
        match self {
            GateClock::Off => "off",
            GateClock::Vsync => "vsync",
            GateClock::Clock => "clock",
        }
    }
}

/// Release instants `anchor_ns + k * period_ns`, `CLOCK_MONOTONIC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Grid {
    pub anchor_ns: u64,
    pub period_ns: u64,
    pub clock: GateClock,
}

impl Grid {
    /// The last grid instant at or before `t_ns`.
    fn at_or_before(&self, t_ns: u64) -> u64 {
        let p = i128::from(self.period_ns.max(1));
        let off = i128::from(t_ns) - i128::from(self.anchor_ns);
        (i128::from(self.anchor_ns) + off.div_euclid(p) * p) as u64
    }

    /// The grid instant nearest `t_ns` (the later one on a tie).
    fn nearest(&self, t_ns: u64) -> u64 {
        self.at_or_before(t_ns.saturating_add(self.period_ns / 2))
    }
}

/// How an acquire is paced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pacing {
    Off,
    /// One release per `period_ns` on the shim's clock.
    Clock {
        period_ns: u64,
    },
    /// Releases on `grid`, spaced by `interval_ns` of the ideal timeline.
    Grid {
        grid: Grid,
        interval_ns: u64,
    },
}

impl Pacing {
    /// A cap of `fps` on the shim's own clock, off when uncapped.
    #[must_use]
    pub(crate) fn on_clock(fps: u32) -> Self {
        match crate::scanout::period_ns(fps) {
            0 => Pacing::Off,
            period_ns => Pacing::Clock { period_ns },
        }
    }

    pub(crate) fn clock(self) -> GateClock {
        match self {
            Pacing::Off => GateClock::Off,
            Pacing::Clock { .. } => GateClock::Clock,
            Pacing::Grid { grid, .. } => grid.clock,
        }
    }
}

/// How a cap of `cap_fps` is released on `grid` (see the module notes).
#[must_use]
pub(crate) fn pacing(cap_fps: u32, grid: Option<Grid>) -> Pacing {
    if cap_fps == FPS_UNCAPPED {
        return Pacing::Off;
    }
    let cap_period = NANOS_PER_SECOND / u64::from(cap_fps);
    let Some(grid) = grid.filter(|g| g.period_ns != 0) else {
        return Pacing::Clock {
            period_ns: cap_period,
        };
    };
    let ratio = cap_period as f64 / grid.period_ns as f64;
    let whole = ratio.round();
    if whole >= 1.0 && (ratio - whole).abs() <= whole * SNAP_TOLERANCE {
        Pacing::Grid {
            grid,
            interval_ns: whole as u64 * grid.period_ns,
        }
    } else if ratio < 1.0 {
        Pacing::Clock {
            period_ns: cap_period,
        }
    } else {
        Pacing::Grid {
            grid,
            interval_ns: cap_period,
        }
    }
}

/// How late after its release point an acquire can start and still count as
/// on time: the wake-up of the gate's own wait, with room for a loaded CPU.
const ON_TIME_NS: u64 = 500_000;

/// Late starts booked by [`Schedule::book`], and the release points they moved
/// past, for the census.
static LATE_N: AtomicU64 = AtomicU64::new(0);
static SKIPPED_N: AtomicU64 = AtomicU64::new(0);

/// Which arm of [`Schedule::book`] booked the release: on time, late by less
/// than a slot, late by a period or more. Census only — the three fire in
/// different steady states, and which one fires is what decides whether the cap
/// can hold at all.
static ARM_ON_N: AtomicU64 = AtomicU64::new(0);
static ARM_UNDER_N: AtomicU64 = AtomicU64::new(0);
static ARM_LATE_N: AtomicU64 = AtomicU64::new(0);
/// Bookings that ran on the shim's own clock because the grid had no period.
static BOOK_CLOCK_N: AtomicU64 = AtomicU64::new(0);
/// The furthest a booking ever put the next release *behind* now, ns; 0 when
/// every booking put it ahead. A release point that lands behind now holds
/// nothing, which is exactly the shape a cap that reads high has.
static BOOK_BEHIND_MAX_NS: AtomicU64 = AtomicU64::new(0);

/// `(on time, under a slot, a period or more late, gridless, furthest behind)`:
/// [`Schedule::book`]'s four outcomes and the worst one, for the census.
pub(crate) fn book_counts() -> (u64, u64, u64, u64, u64) {
    (
        ARM_ON_N.load(Ordering::Relaxed),
        ARM_UNDER_N.load(Ordering::Relaxed),
        ARM_LATE_N.load(Ordering::Relaxed),
        BOOK_CLOCK_N.load(Ordering::Relaxed),
        BOOK_BEHIND_MAX_NS.load(Ordering::Relaxed),
    )
}

/// `(late, skipped)`: acquires that started after their release point, and the
/// release points the schedule moved past for them.
pub(crate) fn late_counts() -> (u64, u64) {
    (
        LATE_N.load(Ordering::Relaxed),
        SKIPPED_N.load(Ordering::Relaxed),
    )
}

/// A swapchain's release schedule on the grid.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// The grid instant the next acquire may start at; 0 before the first.
    pub next_ns: u64,
    /// The ideal-timeline instant `next_ns` was rounded from.
    ideal_ns: u64,
}

impl Schedule {
    /// Book an acquire that started at `now_ns` (at or after `next_ns`) and
    /// return the instant the one after it may start.
    ///
    /// An acquire that started before the grid instant after its release point
    /// (on time, or late by less than a period) is booked to that release
    /// point, and the schedule goes on from there: the next release is
    /// `interval` further along the ideal timeline, on the grid instant
    /// nearest it. A start later than that but still inside its slot (before
    /// `interval` has passed on the ideal timeline) is booked to the grid
    /// instant it is in and keeps the slot's timeline, so the release after it
    /// is at most the rest of the slot away and the cap holds on average. A
    /// start past the whole slot, or the first, starts the timeline again from
    /// the grid instant at or before it, so a stall carries no debt. The
    /// release point is read on the grid as it is now: the daemon re-pushes
    /// the phase as it moves, and a point booked on the old phase must not let
    /// two releases into one period of the new one. Starts later than
    /// [`ON_TIME_NS`] after their point are counted (`gate_late`), with the
    /// points passed (`skipped`).
    ///
    /// The release after a late start is never shortened to the next grid
    /// instant: that rule (the first shim of payload 20261103.4) let a guest
    /// whose frame outlasts one grid period plus [`ON_TIME_NS`] arrive late for
    /// every release, so a cap below the display's rate stopped holding at all
    /// (cap 20 on a 120 Hz panel ran at the guest's own 40-55 fps; at 60 Hz
    /// only a guest faster than ~58 fps recovered).
    pub(crate) fn book(&mut self, now_ns: u64, grid: Grid, interval_ns: u64) -> u64 {
        if grid.period_ns == 0 {
            return self.book_clock(now_ns, interval_ns);
        }
        let at = grid.at_or_before(now_ns);
        let due = (self.next_ns != 0).then(|| grid.nearest(self.next_ns));
        // Late against the instant the acquire was released for, which after a
        // clock booking is off the grid.
        let late = due.filter(|_| now_ns >= self.next_ns.saturating_add(ON_TIME_NS));
        if let Some(due) = late {
            LATE_N.fetch_add(1, Ordering::Relaxed);
            SKIPPED_N.fetch_add(at.saturating_sub(due) / grid.period_ns, Ordering::Relaxed);
        }
        let (slot, ideal) = match due {
            Some(due) if at <= due => {
                ARM_ON_N.fetch_add(1, Ordering::Relaxed);
                (due, self.ideal_ns)
            }
            // Late by a period or more but still inside its slot: the slot's
            // timeline goes on, so the frame after this one is due where the
            // cap says, not a whole interval after a late start.
            Some(_) if now_ns < self.ideal_ns.saturating_add(interval_ns) => {
                ARM_UNDER_N.fetch_add(1, Ordering::Relaxed);
                (at, self.ideal_ns)
            }
            _ => {
                ARM_LATE_N.fetch_add(1, Ordering::Relaxed);
                (at, at)
            }
        };
        let ideal_next = ideal.saturating_add(interval_ns);
        // Never two releases on one grid instant.
        let next = grid
            .nearest(ideal_next)
            .max(slot.saturating_add(grid.period_ns));
        BOOK_BEHIND_MAX_NS.fetch_max(now_ns.saturating_sub(next), Ordering::Relaxed);
        self.ideal_ns = ideal_next;
        self.next_ns = next;
        next
    }

    /// Book an acquire that started at `now_ns` on the shim's own clock
    /// ([`crate::scanout::next_acquire_after`]).
    pub(crate) fn book_clock(&mut self, now_ns: u64, period_ns: u64) -> u64 {
        BOOK_CLOCK_N.fetch_add(1, Ordering::Relaxed);
        self.next_ns = crate::scanout::next_acquire_after(self.next_ns, now_ns, period_ns);
        BOOK_BEHIND_MAX_NS.fetch_max(now_ns.saturating_sub(self.next_ns), Ordering::Relaxed);
        // `book` reads `ideal_ns` as an absolute instant: leaving it at 0
        // resolves `nearest(ideal_ns + interval)` near CLOCK_MONOTONIC zero and
        // the schedule collapses to one release per vblank.
        self.ideal_ns = self.next_ns;
        self.next_ns
    }

    /// Book an acquire that started at `now_ns` under `pacing`; `next_ns` is
    /// left alone when there is no gate.
    pub(crate) fn book_paced(&mut self, now_ns: u64, pacing: Pacing) {
        match pacing {
            Pacing::Off => {}
            Pacing::Clock { period_ns } => {
                self.book_clock(now_ns, period_ns);
            }
            Pacing::Grid { grid, interval_ns } => {
                self.book(now_ns, grid, interval_ns);
            }
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

    /// A 59.97 Hz panel, as the Retroid Pocket 6 measures.
    const P60: u64 = 16_675_004;
    /// A 120 Hz panel.
    const P120: u64 = 8_333_333;

    fn grid(period: u64) -> Grid {
        Grid {
            anchor_ns: 1_000_000_000,
            period_ns: period,
            clock: GateClock::Vsync,
        }
    }

    /// Releases of a guest that is always ready: it asks again the instant it
    /// is let go (plus a wake-up jitter), for `n` frames.
    fn run_ready(g: Grid, interval: u64, n: usize) -> Vec<u64> {
        let mut s = Schedule::default();
        let mut now = g.anchor_ns;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            if s.next_ns != 0 && now < s.next_ns {
                now = s.next_ns;
            }
            let start = now + 50_000; // the wait returns a hair late
            out.push(start);
            s.book(start, g, interval);
            now = start + 1_000_000; // a frame's CPU work before the next acquire
        }
        out
    }

    fn interval(p: Pacing) -> u64 {
        match p {
            Pacing::Grid { interval_ns, .. } => interval_ns,
            other => panic!("expected a grid gate, got {other:?}"),
        }
    }

    /// The vblank index each release fell in.
    fn vblanks(g: Grid, starts: &[u64]) -> Vec<u64> {
        starts
            .iter()
            .map(|&t| (g.at_or_before(t) - g.anchor_ns) / g.period_ns)
            .collect()
    }

    #[test]
    fn a_cap_at_the_display_rate_releases_once_per_vblank() {
        let g = grid(P60);
        // 60 on a 59.97 Hz panel: the display's rate, not 60.000.
        let p = pacing(60, Some(g));
        assert_eq!(interval(p), P60);
        let starts = run_ready(g, interval(p), 600);
        let v = vblanks(g, &starts);
        assert!(v.windows(2).all(|w| w[1] == w[0] + 1), "one per vblank");
        // 600 releases over 600 vblanks: the panel's rate exactly.
        assert_eq!(v[599] - v[0], 599);
        // Phase: each release is on its grid instant, not wherever a free clock
        // drifted to.
        for &t in &starts {
            assert_eq!(t - g.at_or_before(t), 50_000);
        }
    }

    #[test]
    fn thirty_on_sixty_releases_every_second_vblank() {
        let g = grid(P60);
        let p = pacing(30, Some(g));
        assert_eq!(interval(p), 2 * P60);
        let v = vblanks(g, &run_ready(g, interval(p), 300));
        assert!(v.windows(2).all(|w| w[1] == w[0] + 2));
    }

    #[test]
    fn sixty_on_a_hundred_and_twenty_releases_every_second_vblank() {
        let g = grid(P120);
        let p = pacing(60, Some(g));
        assert_eq!(interval(p), 2 * P120);
        let v = vblanks(g, &run_ready(g, interval(p), 300));
        assert!(v.windows(2).all(|w| w[1] == w[0] + 2));
    }

    #[test]
    fn forty_five_on_sixty_averages_the_cap_on_vblanks() {
        let g = grid(16_666_667);
        let p = pacing(45, Some(g));
        assert_eq!(interval(p), NANOS_PER_SECOND / 45);
        let v = vblanks(g, &run_ready(g, interval(p), 451));
        // Every gap is one or two vblanks, never zero.
        assert!(v.windows(2).all(|w| (1..=2).contains(&(w[1] - w[0]))));
        // 450 intervals of 1/45 s span 600 vblanks of a 60 Hz panel.
        let span = v[450] - v[0];
        assert!((599..=601).contains(&span), "span {span}");
    }

    #[test]
    fn a_late_frame_starts_at_once_and_the_schedule_goes_on() {
        let g = grid(P60);
        let iv = interval(pacing(60, Some(g)));
        let mut s = Schedule::default();
        let first = g.anchor_ns + 10_000;
        let due = s.book(first, g, iv);
        assert_eq!(due, g.anchor_ns + P60);
        // The guest comes back 6 ms after its release point. The gate does not
        // hold it (it is past `next_ns`), and the release after it is the next
        // vblank, not a whole period from now.
        let late = due + 6_000_000;
        assert!(late >= s.next_ns);
        assert_eq!(s.book(late, g, iv), due + P60);
        // Late by more than a period: assigned to the vblank it is in, and the
        // next release is the one after.
        let very_late = due + P60 + 3 * P60 + 2_000_000;
        assert_eq!(s.book(very_late, g, iv), due + 5 * P60);
    }

    /// A guest that is always ready, booked under `first` for `n_first`
    /// frames, then under a 30 cap on a 60 Hz grid: the gaps, in vblanks,
    /// between the grid releases.
    fn gaps_after(first: Pacing, n_first: usize) -> Vec<u64> {
        let g = grid(16_666_667);
        let then = pacing(30, Some(g));
        let mut s = Schedule::default();
        let mut now = g.anchor_ns + 1_000_000;
        for _ in 0..n_first {
            now = now.max(s.next_ns);
            s.book_paced(now, first);
            now += 1_000_000;
        }
        let mut starts = Vec::new();
        for _ in 0..7 {
            now = now.max(s.next_ns) + 50_000;
            starts.push(now);
            s.book_paced(now, then);
            now += 1_000_000;
        }
        vblanks(g, &starts)
            .windows(2)
            .map(|w| w[1] - w[0])
            .collect()
    }

    #[test]
    fn a_grid_cap_binds_after_frames_booked_on_the_clock() {
        // The first grid release follows the clock's last due point, which is
        // off the grid, so it may land a vblank later; every one after it is
        // two vblanks apart. Before the fix the gaps were [0, 1, 1, 1, 1, 1].
        let check = |gaps: Vec<u64>| {
            assert!((2..=3).contains(&gaps[0]), "{gaps:?}");
            assert_eq!(gaps[1..], [2, 2, 2, 2, 2], "{gaps:?}");
        };
        // Frame generation turned off at a cap below the display's rate: the
        // source-rate gate's clock (40 fps) first, then 30 on 60.
        check(gaps_after(Pacing::on_clock(40), 10));
        // A cap moved from above the panel's rate (90 on 60, the cap's own
        // clock) to below it.
        check(gaps_after(Pacing::on_clock(90), 10));
        // And with no clock booking at all.
        assert_eq!(gaps_after(Pacing::Off, 0), [2, 2, 2, 2, 2, 2]);
    }

    #[test]
    fn a_phase_push_does_not_let_two_releases_into_one_vblank() {
        let g = grid(P60);
        let iv = interval(pacing(60, Some(g)));
        let mut s = Schedule::default();
        let due = s.book(g.anchor_ns + 10_000, g, iv);
        // The daemon re-anchors the grid 400 us later; the guest is released
        // on the old point, which is now just before the new one.
        let moved = Grid {
            anchor_ns: g.anchor_ns + 400_000,
            ..g
        };
        let next = s.book(due + 20_000, moved, iv);
        assert_eq!(next, due + 400_000 + P60);
        // And 400 us earlier: the old point is just after the new one.
        let earlier = Grid {
            anchor_ns: g.anchor_ns - 400_000,
            ..g
        };
        let after = s.book(next + 20_000, earlier, iv);
        assert_eq!(after, next - 800_000 + P60);
    }

    #[test]
    fn a_guest_below_the_cap_is_not_quantized_to_a_divisor() {
        // 50 fps on a 60 Hz cap: every frame is late for its release point, so
        // the gate never holds it, and it keeps its own 20 ms cadence.
        let g = grid(16_666_667);
        let iv = interval(pacing(60, Some(g)));
        let mut s = Schedule::default();
        let mut t = g.anchor_ns;
        for _ in 0..100 {
            assert!(s.next_ns == 0 || t >= s.next_ns, "held at {t}");
            s.book(t, g, iv);
            t += 20_000_000;
        }
    }

    #[test]
    fn a_late_frame_on_a_divisor_cap_keeps_its_slots_timeline() {
        // 30 on 60, the guest late by half a vblank: it starts at once, and the
        // release after it is where the cap puts it, two vblanks after the
        // point this one was released for.
        let g = grid(P60);
        let iv = interval(pacing(30, Some(g)));
        let mut s = Schedule::default();
        let due = s.book(g.anchor_ns, g, iv);
        assert_eq!(due, g.anchor_ns + 2 * P60);
        assert_eq!(s.book(due + P60 / 2, g, iv), due + 2 * P60);
        // Late by a whole vblank, still inside the slot: the release after it
        // is still the slot's, so the hold is the rest of the slot, not a
        // whole slot from the late start.
        let next = s.next_ns;
        assert_eq!(s.book(next + P60 + 100_000, g, iv), next + 2 * P60);
        // Past the whole slot: the timeline restarts from the start, no debt.
        let next = s.next_ns;
        assert_eq!(s.book(next + 2 * P60 + 100_000, g, iv), next + 4 * P60);
        // An on-time guest is still held to every second vblank.
        let next = s.next_ns;
        assert_eq!(s.book(next + 50_000, g, iv), next + 2 * P60);
    }

    /// Releases of a guest whose acquire-to-acquire time is `frame_ns`, with
    /// its wait returning `wake_ns` late, under `cap` on `g`, for `n` frames
    /// with one `hitch_ns` stall at frame 100: the gaps, in vblanks, between
    /// consecutive releases.
    fn gaps_of_a_slow_guest(g: Grid, cap: u32, frame_ns: u64, wake_ns: u64, hitch_ns: u64, n: usize) -> Vec<u64> {
        let iv = interval(pacing(cap, Some(g)));
        let mut s = Schedule::default();
        let mut t = g.anchor_ns;
        let mut starts = Vec::new();
        for i in 0..n {
            if s.next_ns != 0 && t < s.next_ns {
                t = s.next_ns + wake_ns;
            }
            starts.push(t);
            s.book(t, g, iv);
            t += frame_ns + if i == 100 { hitch_ns } else { 0 };
        }
        vblanks(g, &starts).windows(2).map(|w| w[1] - w[0]).collect()
    }

    /// `gaps` holds the cap's cadence `every` vblanks, except the release after
    /// the hitch at index 100 and the two after it, which catch the slot's
    /// timeline up (a start inside its slot keeps the timeline); over the whole
    /// run there are never more releases than the cap allows.
    fn assert_cadence(gaps: &[u64], every: u64) {
        let off: Vec<usize> = gaps
            .iter()
            .enumerate()
            .filter(|&(i, &gap)| gap != every && !(100..103).contains(&i))
            .map(|(i, _)| i)
            .collect();
        assert!(off.is_empty(), "releases off the {every}-vblank cadence at {off:?}: {gaps:?}");
        assert!(gaps[100] > every, "the hitch frame was not late: {:?}", &gaps[98..104]);
        let total: u64 = gaps.iter().sum();
        assert!(total >= every * gaps.len() as u64, "more releases than the cap: {:?}", &gaps[98..104]);
    }

    #[test]
    fn a_guest_slower_than_a_vblank_is_held_to_a_cap_below_the_rate() {
        // Cap 20 on a 120 Hz grid, 18 ms frames (the guest alone would run at
        // 55 fps), every wake-up 1 ms late (late for its point) and one 80 ms
        // hitch: the releases stay on every sixth vblank, and the cadence is
        // back within three releases of the hitch. On the first shim of
        // payload 20261103.4 the first late start was followed at the next
        // vblank, the 18 ms frame was late for that too, and the cap never
        // held again (the guest ran at its own rate).
        let gaps = gaps_of_a_slow_guest(grid(P120), 20, 18_000_000, 1_000_000, 80_000_000, 600);
        assert_cadence(&gaps, 6);
        // 22 ms frames at 60 Hz under cap 20: every third vblank.
        let gaps = gaps_of_a_slow_guest(grid(P60), 20, 22_000_000, 1_000_000, 80_000_000, 600);
        assert_cadence(&gaps, 3);
    }

    #[test]
    fn a_guest_with_16_ms_of_work_is_never_skipped() {
        // 16 ms from acquire to the next acquire on a 59.97 Hz grid, and every
        // seventh frame 1.5 ms slower (late for its point): the late frames
        // start at once, the release after each is the next grid instant, and
        // every vblank gets a start.
        let g = grid(P60);
        let iv = interval(pacing(60, Some(g)));
        let mut s = Schedule::default();
        let mut t = g.anchor_ns;
        let mut starts = Vec::new();
        for n in 0..600 {
            if s.next_ns != 0 && t < s.next_ns {
                t = s.next_ns + 50_000;
            }
            starts.push(t);
            s.book(t, g, iv);
            t += if n % 7 == 6 { 17_500_000 } else { 16_000_000 };
        }
        let v = vblanks(g, &starts);
        assert!(
            v.windows(2).all(|w| w[1] == w[0] + 1),
            "a vblank was skipped"
        );
    }

    /// One test for the process-global grid, so no other test races it.
    #[test]
    fn the_grid_is_the_daemons_flip_stamp_grid_and_needs_its_lead() {
        // No note yet: no grid.
        assert_eq!(current_grid(), None);
        let note = VsyncNote {
            anchor_ns: 5_000_000_000,
            period_ns: P60 as u32,
            lead_ns: 9_000_000,
            stamp_lead_ns: 0,
        };
        // A note without a stamp lead (a daemon that predates it): no grid.
        apply_note(note);
        assert_eq!(current_grid(), None);
        // With one: the latch grid moved earlier by the stamp lead, the
        // instants gamescope's vblanks are stamped at.
        apply_note(VsyncNote {
            stamp_lead_ns: 2_500_000,
            ..note
        });
        let g = current_grid().expect("the daemon's grid");
        assert_eq!(g.clock, GateClock::Vsync);
        assert_eq!(g.period_ns, P60);
        assert_eq!(g.anchor_ns, 5_000_000_000 - 2_500_000);
        apply_note(VsyncNote {
            anchor_ns: 0,
            period_ns: 0,
            lead_ns: 0,
            stamp_lead_ns: 0,
        });
        // No grid at all: the cap's own clock.
        assert_eq!(
            pacing(30, None),
            Pacing::Clock {
                period_ns: NANOS_PER_SECOND / 30
            }
        );
    }

    #[test]
    fn caps_above_the_rate_and_none_are_not_on_the_grid() {
        let g = grid(P120);
        assert_eq!(pacing(FPS_UNCAPPED, Some(g)), Pacing::Off);
        assert_eq!(
            pacing(190, Some(g)),
            Pacing::Clock {
                period_ns: NANOS_PER_SECOND / 190
            }
        );
        // 59 on 60 Hz is not the display's rate: 59, on vblanks.
        let g60 = grid(16_666_667);
        assert_eq!(interval(pacing(59, Some(g60))), NANOS_PER_SECOND / 59);
    }
}
