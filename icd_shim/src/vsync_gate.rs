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
//! then the shim declares FIFO under a cap and gamescope's limiter holds it
//! (`proxy::choose_live_mode`).

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
    /// nearest it. A start later than that, or the first, starts the timeline
    /// again from the grid instant at or before it, so a stall carries no debt
    /// and a late frame is never held for a whole slot. The release point is
    /// read on the grid as it is now: the daemon re-pushes the phase as it
    /// moves, and a point booked on the old phase must not let two releases
    /// into one period of the new one. Starts later than [`ON_TIME_NS`] after
    /// their point are counted (`late`), with the points passed (`skipped`).
    pub(crate) fn book(&mut self, now_ns: u64, grid: Grid, interval_ns: u64) -> u64 {
        if grid.period_ns == 0 {
            return self.book_clock(now_ns, interval_ns);
        }
        let at = grid.at_or_before(now_ns);
        let due = (self.next_ns != 0).then(|| grid.nearest(self.next_ns));
        if let Some(due) = due.filter(|&d| now_ns >= d.saturating_add(ON_TIME_NS)) {
            LATE_N.fetch_add(1, Ordering::Relaxed);
            SKIPPED_N.fetch_add(at.saturating_sub(due) / grid.period_ns, Ordering::Relaxed);
        }
        let (slot, ideal) = match due {
            Some(due) if at <= due => (due, self.ideal_ns),
            _ => (at, at),
        };
        let ideal_next = ideal.saturating_add(interval_ns);
        // Never two releases on one grid instant.
        let next = grid
            .nearest(ideal_next)
            .max(slot.saturating_add(grid.period_ns));
        self.ideal_ns = ideal_next;
        self.next_ns = next;
        next
    }

    /// Book an acquire that started at `now_ns` on the shim's own clock
    /// ([`crate::scanout::next_acquire_after`]).
    pub(crate) fn book_clock(&mut self, now_ns: u64, period_ns: u64) -> u64 {
        self.next_ns = crate::scanout::next_acquire_after(self.next_ns, now_ns, period_ns);
        self.ideal_ns = 0;
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

/// How long before a stamp instant gamescope latches the commits it composites
/// for it: its red zone (1.65 ms) plus its composite time, which the shim does
/// not see; ~1 ms on the devices measured.
const COMPOSITE_MARGIN_NS: u64 = 2_500_000;

/// Frames the commit window held, and held frames a newer one replaced.
static WIN_HOLD_N: AtomicU64 = AtomicU64::new(0);
static WIN_REPLACED_N: AtomicU64 = AtomicU64::new(0);

/// `(holds, replaced)` of the commit window, for the census.
pub(crate) fn window_counts() -> (u64, u64) {
    (
        WIN_HOLD_N.load(Ordering::Relaxed),
        WIN_REPLACED_N.load(Ordering::Relaxed),
    )
}

/// Count one frame the commit window held.
pub(crate) fn note_window_hold() {
    WIN_HOLD_N.fetch_add(1, Ordering::Relaxed);
}

/// Count one held frame a newer one replaced before it was committed.
pub(crate) fn note_window_replaced() {
    WIN_REPLACED_N.fetch_add(1, Ordering::Relaxed);
}

/// One commit per gamescope composite, on the commit side.
///
/// Under mailbox gamescope composites the newest commit it holds at each of
/// its wakeups, `COMPOSITE_MARGIN_NS` ahead of each stamp instant. Two commits
/// between two wakeups and the older one is never shown: the panel repeats the
/// frame before it. The acquire gate limits how many frames start; it cannot
/// space when they finish. A guest that paces itself just below the display's
/// rate (Unity's frame-rate target with VSync off: ~59.3 fps on a 59.97 Hz
/// panel) is late for nearly every release point, so the gate never holds it,
/// and its frames reach the commit with its own jitter. FIFO absorbs that with
/// its queue; this is the one-deep equivalent for mailbox: a frame whose
/// composite window already has a commit is held to the next window and
/// committed at its stamp instant, which is after that window's composite, so
/// it is the frame the next one shows. A newer frame that is ready while one
/// is held replaces it (mailbox: newest wins). A frame is held at most one
/// period plus the margin.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CommitWindow {
    /// The start of the window of the last commit, `CLOCK_MONOTONIC` ns.
    last_start_ns: Option<u64>,
}

impl CommitWindow {
    /// The start of the composite window `t_ns` falls in: windows run from one
    /// wakeup (`COMPOSITE_MARGIN_NS` before a stamp instant) to the next.
    fn window_start(grid: Grid, t_ns: u64) -> u64 {
        Grid {
            anchor_ns: grid.anchor_ns.saturating_sub(COMPOSITE_MARGIN_NS),
            ..grid
        }
        .at_or_before(t_ns)
    }

    /// `None` when a frame ready at `now_ns` can be committed now; otherwise
    /// the instant to commit it at, the stamp instant that closes the window
    /// already used. Compared by time, not by index, so a re-anchored grid
    /// (the daemon's anchor moves by whole periods) reads the same windows.
    #[must_use]
    pub(crate) fn hold_until(&self, grid: Grid, now_ns: u64) -> Option<u64> {
        let last = self.last_start_ns?;
        let start = Self::window_start(grid, now_ns);
        (start <= last.saturating_add(grid.period_ns / 2))
            .then(|| last + grid.period_ns + COMPOSITE_MARGIN_NS)
    }

    /// Record a commit made at `now_ns`.
    pub(crate) fn committed(&mut self, grid: Grid, now_ns: u64) {
        self.last_start_ns = Some(Self::window_start(grid, now_ns));
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

    /// Deterministic jitter in `[-amp, amp]` ns.
    struct Jitter(u64);
    impl Jitter {
        fn next(&mut self, amp: i64) -> i64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) % (2 * amp as u64 + 1)) as i64 - amp
        }
    }

    /// What a guest's frames do between the gate and gamescope.
    struct Run {
        /// When each frame reached gamescope.
        commits: Vec<u64>,
        /// Frames the commit window held, and held frames a newer one replaced.
        holds: usize,
        replaced_in_shim: usize,
    }

    /// A guest that wants to start a frame every `own_interval` (+- `jitter`) on
    /// its own clock, gated at a 60 cap on `g`, each frame ready `latency`
    /// after its start, committed with or without the commit window.
    fn simulate(
        g: Grid,
        own_interval: u64,
        jitter: i64,
        latency: u64,
        frames: usize,
        window: bool,
    ) -> Run {
        let iv = interval(pacing(60, Some(g)));
        let mut sched = Schedule::default();
        let mut rng = Jitter(7);
        let mut want = g.anchor_ns + 3_000_000;
        let mut last_start = 0;
        let mut ready = Vec::with_capacity(frames);
        for _ in 0..frames {
            let mut start = want.max(last_start + 1);
            if sched.next_ns != 0 {
                start = start.max(sched.next_ns);
            }
            sched.book(start, g, iv);
            last_start = start;
            ready.push(start + latency);
            want = want.saturating_add_signed(own_interval as i64 + rng.next(jitter));
        }
        if !window {
            return Run {
                commits: ready,
                holds: 0,
                replaced_in_shim: 0,
            };
        }
        let mut w = CommitWindow::default();
        let mut run = Run {
            commits: Vec::new(),
            holds: 0,
            replaced_in_shim: 0,
        };
        let mut held: Option<u64> = None;
        for r in ready {
            if let Some(until) = held {
                if r < until {
                    // Ready while the older frame is held: it replaces it.
                    run.replaced_in_shim += 1;
                    continue;
                }
                run.commits.push(until);
                w.committed(g, until);
                held = None;
            }
            match w.hold_until(g, r) {
                None => {
                    run.commits.push(r);
                    w.committed(g, r);
                }
                Some(until) => {
                    run.holds += 1;
                    held = Some(until);
                }
            }
        }
        if let Some(until) = held {
            run.commits.push(until);
        }
        run
    }

    /// Commits gamescope replaces: two in one composite window.
    fn replaced_in_gamescope(g: Grid, commits: &[u64]) -> usize {
        commits
            .windows(2)
            .filter(|c| CommitWindow::window_start(g, c[0]) == CommitWindow::window_start(g, c[1]))
            .count()
    }

    #[test]
    fn a_self_paced_guest_below_the_rate_gets_one_commit_per_composite() {
        // Unity's frame-rate target with VSync off: 59.3 fps +- 1.5 ms on a
        // 59.97 Hz panel, frames ready about a period after they start.
        let g = grid(P60);
        let own = NANOS_PER_SECOND * 10 / 593;
        let bare = simulate(g, own, 1_500_000, 15_000_000, 1200, false);
        let queued = simulate(g, own, 1_500_000, 15_000_000, 1200, true);
        let lost_bare = replaced_in_gamescope(g, &bare.commits);
        assert!(
            lost_bare > 0,
            "the jitter puts two commits in one composite"
        );
        assert_eq!(replaced_in_gamescope(g, &queued.commits), 0);
        assert!(queued.holds > 0);
        // Holding a frame to the next window costs fewer frames than gamescope
        // replacing them did.
        assert!(
            queued.replaced_in_shim < lost_bare,
            "{} replaced in the shim, {lost_bare} in gamescope",
            queued.replaced_in_shim
        );
    }

    #[test]
    fn a_guest_well_below_the_rate_is_never_held() {
        // 32 fps, GPU-bound.
        let g = grid(P60);
        let run = simulate(g, 31_250_000, 1_000_000, 30_000_000, 300, true);
        assert_eq!(run.holds, 0);
        assert_eq!(run.replaced_in_shim, 0);
        assert_eq!(replaced_in_gamescope(g, &run.commits), 0);
    }

    #[test]
    fn a_fast_guest_is_limited_by_the_gate_and_the_window_replaces_nothing() {
        // 300 fps uncapped: the gate releases one frame per vblank, and those
        // reach gamescope one per composite with nothing for the window to do.
        let g = grid(P60);
        let run = simulate(g, 3_333_333, 300_000, 2_000_000, 600, true);
        assert_eq!(run.replaced_in_shim, 0);
        assert_eq!(replaced_in_gamescope(g, &run.commits), 0);
        let span = run.commits[599] - run.commits[0];
        assert_eq!((span + P60 / 2) / P60, 599, "one frame per vblank");
    }

    #[test]
    fn a_second_frame_in_a_window_is_held_to_the_next_stamp_instant() {
        let g = grid(P60);
        let mut w = CommitWindow::default();
        // Nothing committed yet: commit at once.
        assert_eq!(w.hold_until(g, g.anchor_ns + 5_000_000), None);
        w.committed(g, g.anchor_ns + 5_000_000);
        // Another frame before the next wakeup: held to the stamp instant that
        // closes this window (after gamescope's composite for it).
        let until = w.hold_until(g, g.anchor_ns + 12_000_000);
        assert_eq!(until, Some(g.anchor_ns + P60));
        // After the wakeup it is the next window's first commit.
        assert_eq!(w.hold_until(g, g.anchor_ns + P60), None);
        assert_eq!(
            w.hold_until(g, g.anchor_ns + P60 - COMPOSITE_MARGIN_NS),
            None
        );
        // A re-anchored grid (the daemon moves its anchor by whole periods)
        // reads the same windows.
        let moved = Grid {
            anchor_ns: g.anchor_ns + 50 * P60,
            ..g
        };
        assert_eq!(
            w.hold_until(moved, g.anchor_ns + 12_000_000),
            Some(g.anchor_ns + P60)
        );
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
    fn a_late_frame_on_a_divisor_cap_waits_for_no_more_than_the_schedule() {
        // 30 on 60, the guest late by most of a vblank: it starts at once, and
        // the one after it is due at the slot after the one it was given.
        let g = grid(P60);
        let iv = interval(pacing(30, Some(g)));
        let mut s = Schedule::default();
        let due = s.book(g.anchor_ns, g, iv);
        assert_eq!(due, g.anchor_ns + 2 * P60);
        assert_eq!(s.book(due + P60 / 2, g, iv), due + 2 * P60);
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
