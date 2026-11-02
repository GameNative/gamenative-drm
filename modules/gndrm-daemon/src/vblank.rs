//! The display's vblank grid, as the page-flip events report it.
//!
//! A KMS driver stamps a page-flip event with the hardware vblank the flip took
//! effect on, and gamescope schedules its next composite from that stamp
//! (`DRMBackend.cpp` `page_flip_handler` → `MarkVBlank`, re-armed on every
//! event: wakeup = stamp + interval − draw time − red zone). The stamps have to
//! be the display's own timeline, at the display's own rate, or gamescope's
//! composite phase walks across the display's latch point.
//!
//! On Android the display's timeline reaches the app process through
//! `AChoreographer` (`gndrm-android`'s `choreographer.rs` feeds
//! [`note_choreographer_vsync`]); the latch times `SurfaceFlinger` reports for
//! the daemon's own publishes are the fallback ([`note_latch_grid`]). Neither
//! source is a KMS vblank, so this module decides which instant stands in for
//! one: the latch deadline, the point a publish must reach `SurfaceFlinger` by
//! to be the frame its next latch takes. That is what a KMS vblank is to the
//! kernel's commit path — the point a flip must be queued by to be scanned out
//! on the next cycle — and gamescope's own margin (its red zone) then lands
//! every commit ahead of the latch rather than ahead of a present time whose
//! phase against the latch is arbitrary.
//!
//! The stamp sits a measured lead ahead of that latch ([`LeadLoop`]): gamescope's
//! red zone was sized for a kernel commit, and the daemon's apply path needs more.
//!
//! The period comes from measured vsync spacing ([`VsyncRing`]), never from the
//! integer rate the mode advertises: a panel sold as 60 Hz latches at 59.97 or
//! 59.99, and a timeline walking 16 666 666 ns slides across the latch point
//! once every half a minute or so.

use std::sync::{Mutex, PoisonError};

/// Vsyncs the period is fitted over.
const RING: usize = 32;
/// Vsyncs the ring needs before its period is trusted.
const MIN_LOCK: usize = 8;
/// A source that has said nothing for this long is not followed any more.
const STALE_NS: u64 = 1_000_000_000;
/// The slowest display period taken as a period rather than a gap (10 Hz).
const MAX_PERIOD_NS: u64 = 100_000_000;
/// The fastest (1 kHz): anything closer together is a duplicate callback.
const MIN_PERIOD_NS: u64 = 1_000_000;
/// How far a fitted period may sit from the mode's advertised one (2 %), in
/// per mille: a "60 Hz" panel latches at 59.97, never at 30 or 120.
const MODE_TOLERANCE_PER_MILLE: u64 = 20;

/// Recent display vsyncs and the period fitted through them.
///
/// Each sample is placed on a vsync count from the ring's first one: the step to
/// the previous sample divided by the period, rounded, so a callback that came a
/// vsync late counts as two steps rather than one long one. The period is the
/// least-squares slope of time against count over the ring, which a single
/// late timestamp barely moves.
#[derive(Debug, Default)]
pub struct VsyncRing {
    /// `(timestamp_ns, vsync count from the first sample)`, oldest first.
    samples: std::collections::VecDeque<(u64, u64)>,
    /// The fitted period; before the first fit, the seed the steps are counted
    /// with. 0 before any step was seen.
    period_ns: u64,
    /// Whether `period_ns` is a fit over [`MIN_LOCK`] vsyncs or more of the
    /// current rate. Survives a gap in the callbacks, not a rate change.
    locked: bool,
    /// The mode's advertised period, 0 when unknown. When known, the steps are
    /// counted against it and a fit outside [`MODE_TOLERANCE_PER_MILLE`] of it
    /// is rejected: a callback thread starved for a stretch delivers every
    /// second vsync, which counts as two steps of the mode's period instead of
    /// one step of twice it.
    mode_ns: u64,
    /// A fit is being rejected; set on the first rejection of a run, so it is
    /// reported once.
    rejecting: bool,
}

/// What one [`VsyncRing::push`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingEvent {
    /// Nothing worth reporting.
    None,
    /// The ring locked onto a period it was not locked onto before: the first
    /// lock, or the first after a rate change.
    Locked(u64),
    /// A fit this far from the mode's period was rejected; the period stays.
    /// Reported on the first rejection of a run.
    Rejected(u64),
}

impl VsyncRing {
    /// The fitted period, `None` until the ring is locked.
    #[must_use]
    pub fn period_ns(&self) -> Option<u64> {
        self.locked.then_some(self.period_ns)
    }

    /// The mode's advertised period (0 when unknown). A change starts the fit
    /// again from it: the app reports the new mode when the display changes.
    pub fn set_mode(&mut self, mode_ns: u64) {
        if mode_ns == self.mode_ns {
            return;
        }
        self.mode_ns = mode_ns;
        self.samples.clear();
        self.period_ns = mode_ns;
        self.locked = false;
        self.rejecting = false;
    }

    fn within_mode(&self, period: u64) -> bool {
        self.mode_ns == 0
            || period.abs_diff(self.mode_ns) * 1000 <= self.mode_ns * MODE_TOLERANCE_PER_MILLE
    }

    /// Fold in one vsync timestamp (`CLOCK_MONOTONIC` ns).
    pub fn push(&mut self, t_ns: u64) -> RingEvent {
        let Some(&(last_t, last_n)) = self.samples.back() else {
            self.samples.push_back((t_ns, 0));
            return RingEvent::None;
        };
        if t_ns <= last_t || t_ns - last_t < MIN_PERIOD_NS {
            return RingEvent::None;
        }
        let step = t_ns - last_t;
        if self.period_ns == 0 {
            if step > MAX_PERIOD_NS {
                // Not a period: the callbacks paused. Count from here.
                self.restart(t_ns);
                return RingEvent::None;
            }
            self.period_ns = step;
        }
        let period = self.period_ns;
        if step > 8 * period {
            // A gap (the callbacks paused, or the thread was starved): counting
            // across it would ask a period for more precision than it has. The
            // fitted period and the lock stay; the count starts again.
            self.restart(t_ns);
            return RingEvent::None;
        }
        let k = (step + period / 2) / period;
        let residual = step.abs_diff(k * period);
        if k == 0 || residual > period / 4 {
            if self.mode_ns != 0 {
                // With the mode known, a rate change arrives as a new mode
                // (`set_mode`); this is a stray callback. Count from here.
                self.restart(t_ns);
                return RingEvent::None;
            }
            // The step is not a whole number of periods: the display changed
            // rate. Seed from this step and lock again.
            self.reseed(t_ns, step);
            return RingEvent::None;
        }
        self.samples.push_back((t_ns, last_n + k));
        if self.samples.len() > RING {
            self.samples.pop_front();
        }
        // A full ring without one single-vsync step is counting a rate at a
        // multiple of the period it holds (the display halved its rate): take
        // the shortest step instead.
        if self.mode_ns == 0
            && self.samples.len() == RING
            && self
                .samples
                .iter()
                .zip(self.samples.iter().skip(1))
                .all(|(a, b)| b.1 - a.1 >= 2)
        {
            let shortest = self
                .samples
                .iter()
                .zip(self.samples.iter().skip(1))
                .map(|(a, b)| b.0 - a.0)
                .min()
                .unwrap_or(step);
            self.reseed(t_ns, shortest);
            return RingEvent::None;
        }
        if self.samples.len() < MIN_LOCK {
            return RingEvent::None;
        }
        let fitted = self.fit();
        if !self.within_mode(fitted) {
            let first = !self.rejecting;
            self.rejecting = true;
            return if first {
                RingEvent::Rejected(fitted)
            } else {
                RingEvent::None
            };
        }
        self.rejecting = false;
        let was = self.locked.then_some(self.period_ns);
        self.period_ns = fitted;
        self.locked = true;
        match was {
            None => RingEvent::Locked(fitted),
            Some(_) => RingEvent::None,
        }
    }

    /// Restart the count at `t_ns`, keeping the period and the lock.
    fn restart(&mut self, t_ns: u64) {
        self.samples.clear();
        self.samples.push_back((t_ns, 0));
    }

    /// Restart the count at `t_ns` on a new seed period, unlocked.
    fn reseed(&mut self, t_ns: u64, seed_ns: u64) {
        self.restart(t_ns);
        self.period_ns = if (MIN_PERIOD_NS..=MAX_PERIOD_NS).contains(&seed_ns) {
            seed_ns
        } else {
            0
        };
        self.locked = false;
    }

    /// Least-squares slope of timestamp against vsync count over the ring.
    fn fit(&self) -> u64 {
        let (t0, n0) = self.samples[0];
        let len = self.samples.len() as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for &(t, n) in &self.samples {
            let x = (n - n0) as f64;
            let y = (t - t0) as f64;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let den = len * sxx - sx * sx;
        if den <= 0.0 {
            return self.period_ns;
        }
        let slope = (len * sxy - sx * sy) / den;
        if slope.is_finite() && slope >= MIN_PERIOD_NS as f64 && slope <= MAX_PERIOD_NS as f64 {
            slope.round() as u64
        } else {
            self.period_ns
        }
    }
}

/// Where a [`Grid`]'s phase and period came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridSource {
    /// Choreographer's frame deadline (API 33+), measured period.
    ChoreographerDeadline,
    /// The latch grid's phase with Choreographer's measured period (API 29-32,
    /// where Choreographer reports the vsync time only).
    ChoreographerLatch,
    /// Choreographer's vsync time and measured period, with no latch seen yet.
    ChoreographerVsync,
    /// The latch grid alone: its phase, and the mode's advertised period.
    Latch,
}

impl GridSource {
    /// A short name for the log.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            GridSource::ChoreographerDeadline => "choreographer-deadline",
            GridSource::ChoreographerLatch => "choreographer+latch",
            GridSource::ChoreographerVsync => "choreographer-vsync",
            GridSource::Latch => "latch",
        }
    }
}

/// A display timeline: the latch instants `anchor_ns + k * period_ns`, and the
/// lead a flip event is stamped ahead of the latch it lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub anchor_ns: u64,
    pub period_ns: u64,
    pub source: GridSource,
    /// How far ahead of its latch a flip event is stamped (see [`LeadLoop`]).
    pub lead_ns: u64,
}

impl Grid {
    /// The first grid instant at or after `t_ns`.
    #[must_use]
    pub fn at_or_after(&self, t_ns: u64) -> u64 {
        let p = i128::from(self.period_ns.max(1));
        let off = i128::from(t_ns) - i128::from(self.anchor_ns);
        // ceil(off / p) for either sign.
        let k = off.div_euclid(p) + i128::from(off.rem_euclid(p) != 0);
        (i128::from(self.anchor_ns) + k * p) as u64
    }
}

/// The signed distance from `x` to the nearest instant of a `period` grid
/// through 0, in `[-period/2, period/2)`.
#[must_use]
pub fn wrap(x: i64, period: i64) -> i64 {
    let half = period / 2;
    (x + half).rem_euclid(period) - half
}

/// A signed margin folded into `[-period/4, 3*period/4)`: a publish that
/// landed more than three quarters of a period ahead of a latch is read as late
/// for the latch before, and one late by more than a quarter as very early.
/// The lead keeps margins near a few milliseconds, so the first never happens
/// while it holds; the second (a stall) only nudges the lead down one step.
#[must_use]
pub fn fold_margin(x: i64, period: i64) -> i64 {
    let q = period / 4;
    (x + q).rem_euclid(period) - q
}

/// The default lead before the loop has measured anything. On the Retroid
/// Pocket 6 (2026-11-02) a stamp on the deadline itself left the publish
/// 1.0 / 2.1 / 4.5 ms (p10 / p50 / p90) ahead of the latch.
const START_LEAD_NS: u64 = 2_000_000;
/// The margin the loop holds the 10th percentile at: three times the ~1 ms
/// spread of the apply path, capped at 30 % of a period for fast panels.
const TARGET_P10_NS: u64 = 3_000_000;
/// One step of the quantile loop.
const LEAD_STEP_NS: u64 = 20_000;
/// The override, in microseconds: a fixed lead, no loop.
pub const LEAD_ENV: &str = "GNDRM_GS_LATCH_LEAD_US";

/// How far ahead of `SurfaceFlinger`'s latch a flip event is stamped.
///
/// gamescope commits about its red zone (1.65 ms) ahead of the stamp it was
/// handed one period earlier. That was sized for a kernel commit; here the
/// commit still crosses the daemon's commit worker and a binder apply before it
/// reaches `SurfaceFlinger`, with ~1 ms of jitter, so the tail of the applies
/// crossed the latch. The lead moves every stamp, and with it gamescope's
/// whole schedule, earlier by the same amount.
///
/// The loop measures each publish's margin (latch time minus apply time) and
/// moves the lead so that one publish in ten lands less than
/// [`TARGET_P10_NS`] ahead of its latch: a stochastic quantile tracker (+9
/// steps on a margin below target, −1 step above). The price is latency: a
/// frame waits the lead longer between gamescope's commit and the latch.
#[derive(Debug, Clone, Copy)]
pub struct LeadLoop {
    lead_ns: u64,
    fixed: bool,
}

impl Default for LeadLoop {
    fn default() -> Self {
        Self {
            lead_ns: START_LEAD_NS,
            fixed: false,
        }
    }
}

impl LeadLoop {
    /// A loop held at `lead_ns` (the [`LEAD_ENV`] override).
    #[must_use]
    pub fn fixed(lead_ns: u64) -> Self {
        Self {
            lead_ns,
            fixed: true,
        }
    }

    /// The lead in force, clamped to a third of `period` (half of it for the
    /// fixed override: a lead of a period or more would stamp every event
    /// before the previous latch).
    #[must_use]
    pub fn lead_ns(&self, period: u64) -> u64 {
        if self.fixed {
            self.lead_ns.min(period / 2)
        } else {
            self.lead_ns.min(period / 3)
        }
    }

    /// The 10th-percentile margin the loop aims for at `period`.
    #[must_use]
    pub fn target_ns(period: u64) -> u64 {
        TARGET_P10_NS.min(period * 3 / 10)
    }

    /// Fold in one publish's margin (folded, see [`fold_margin`]).
    pub fn update(&mut self, margin_ns: i64, period: u64) {
        if self.fixed || period == 0 {
            return;
        }
        if margin_ns < Self::target_ns(period) as i64 {
            self.lead_ns += 9 * LEAD_STEP_NS;
        } else {
            self.lead_ns = self.lead_ns.saturating_sub(LEAD_STEP_NS);
        }
        self.lead_ns = self.lead_ns.min(period / 3);
    }
}

/// How often the cross-check is reported.
const REPORT_NS: u64 = 10_000_000_000;

/// One window's cross-check (see [`DisplayVsync::note_publish`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VsyncReport {
    pub source: Option<GridSource>,
    pub period_ns: u64,
    /// Latch minus apply, folded: p10, p50, p90.
    pub margin_ns: [i64; 3],
    /// Publishes that missed the latch they were aimed at (negative margin).
    pub late: u32,
    pub publishes: u32,
    pub vsyncs: u32,
    /// Flip events pushed past the first latch after their publish by the
    /// one-claim-per-instant rule (a second publish inside one period).
    pub pushed: u32,
    pub lead_ns: u64,
    /// `SurfaceFlinger`'s latch against Choreographer's deadline, when both are
    /// known.
    pub latch_after_deadline_ns: Option<i64>,
}

#[derive(Debug, Default)]
struct Window {
    start_ns: u64,
    margins: Vec<i64>,
    late: u32,
    vsyncs: u32,
}

/// Claims pushed by the ordering rule, counted by the commit worker.
static PUSHED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Everything the display has told the daemon about its timeline.
#[derive(Debug, Default)]
pub struct DisplayVsync {
    ring: VsyncRing,
    /// The latest Choreographer vsync and, when it reported one, the preferred
    /// frame timeline's deadline.
    chor: Option<(u64, Option<u64>)>,
    /// The latch grid: anchor and the period it was tracked at.
    latch: Option<(u64, u64)>,
    /// `SurfaceFlinger`'s latch against the deadline grid, smoothed over the
    /// publishes' latch times: the deadline is the latch Choreographer
    /// predicts, this is where it is taken.
    latch_after_deadline: Option<i64>,
    lead: LeadLoop,
    window: Window,
    /// A fit the ring just rejected, for the log (taken once).
    rejected_fit: Option<u64>,
}

/// What [`DisplayVsync::note_vsync`] reports back for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsyncLock {
    /// The grid the flip events follow from now on.
    pub grid: Grid,
    /// The latch grid's phase against it, when a latch has been seen: how far
    /// after the stamped instant `SurfaceFlinger` latched.
    pub latch_offset_ns: Option<i64>,
}

impl DisplayVsync {
    /// One Choreographer vsync: its timestamp and, on API 33+, the preferred
    /// frame timeline's deadline. Returns the lock to report when the ring
    /// locked onto a period.
    pub fn note_vsync(
        &mut self,
        vsync_ns: u64,
        deadline_ns: Option<u64>,
        mode_ns: u64,
    ) -> Option<VsyncLock> {
        self.ring.set_mode(mode_ns);
        let ev = self.ring.push(vsync_ns);
        if let RingEvent::Rejected(p) = ev {
            self.rejected_fit = Some(p);
        }
        self.chor = Some((vsync_ns, deadline_ns));
        self.window.vsyncs += 1;
        match ev {
            RingEvent::Locked(_) => {
                let grid = self.grid(vsync_ns)?;
                let latch_offset_ns = self.latch.map(|(anchor, _)| {
                    wrap(anchor as i64 - grid.anchor_ns as i64, grid.period_ns as i64)
                });
                Some(VsyncLock {
                    grid,
                    latch_offset_ns,
                })
            }
            RingEvent::None | RingEvent::Rejected(_) => None,
        }
    }

    /// The latch grid's current anchor and the period it is tracked at.
    pub fn note_latch(&mut self, anchor_ns: u64, period_ns: u64) {
        if anchor_ns != 0 && period_ns != 0 {
            self.latch = Some((anchor_ns, period_ns));
        }
    }

    /// The measured period while Choreographer is locked and fresh at `now_ns`.
    #[must_use]
    pub fn measured_period_ns(&self, now_ns: u64) -> Option<u64> {
        let (vsync, _) = self.chor?;
        (now_ns.saturating_sub(vsync) < STALE_NS)
            .then(|| self.ring.period_ns())
            .flatten()
    }

    /// Hold the lead at `lead_ns` instead of measuring it.
    pub fn fix_lead(&mut self, lead_ns: u64) {
        self.lead = LeadLoop::fixed(lead_ns);
    }

    /// One completed publish: the instant its apply returned and the latch
    /// `SurfaceFlinger` took it at. Moves the lead, tracks the latch against
    /// the deadline, and every [`REPORT_NS`] returns the window's cross-check.
    pub fn note_publish(&mut self, applied_ns: u64, latch_ns: u64) -> Option<VsyncReport> {
        let grid = self.grid(latch_ns);
        if let Some(g) = grid {
            let period = g.period_ns as i64;
            if let (Some((_, Some(deadline))), Some(_)) = (self.chor, self.ring.period_ns()) {
                let e = wrap(latch_ns as i64 - deadline as i64, period);
                self.latch_after_deadline = Some(match self.latch_after_deadline {
                    None => e,
                    // Folded around the current estimate, so a phase near half
                    // a period does not average two edges into the middle.
                    Some(d) => d + wrap(e - d, period) / 8,
                });
            }
            if applied_ns != 0 {
                let margin = fold_margin(latch_ns as i64 - applied_ns as i64, period);
                self.lead.update(margin, g.period_ns);
                if margin < 0 {
                    self.window.late += 1;
                }
                if self.window.margins.len() < 4096 {
                    self.window.margins.push(margin);
                }
            }
        }
        if self.window.start_ns == 0 || latch_ns < self.window.start_ns {
            self.window = Window {
                start_ns: latch_ns,
                ..Window::default()
            };
            return None;
        }
        let span = latch_ns - self.window.start_ns;
        if span < REPORT_NS {
            return None;
        }
        let w = std::mem::replace(
            &mut self.window,
            Window {
                start_ns: latch_ns,
                ..Window::default()
            },
        );
        let pushed = PUSHED.swap(0, std::sync::atomic::Ordering::Relaxed);
        if span > 2 * REPORT_NS {
            // The window spans a gap between sessions: nothing to report.
            return None;
        }
        let mut m = w.margins;
        m.sort_unstable();
        let q = |f: usize| m.get(m.len() * f / 10).copied().unwrap_or(0);
        let period_ns = grid.map_or(0, |g| g.period_ns);
        Some(VsyncReport {
            source: grid.map(|g| g.source),
            period_ns,
            margin_ns: [q(1), q(5), q(9)],
            late: w.late,
            publishes: m.len() as u32,
            vsyncs: w.vsyncs,
            pushed,
            lead_ns: self.lead.lead_ns(period_ns),
            latch_after_deadline_ns: self.latch_after_deadline,
        })
    }

    /// The grid flip events follow at `now_ns`, `None` when no source is fresh.
    ///
    /// Its instants are `SurfaceFlinger`'s latch points. Choreographer's
    /// measured period wins whenever it is locked and fresh. Its phase is the
    /// frame deadline when it reports one, moved by where the publishes were
    /// actually latched against it; otherwise the latch grid's, and the vsync
    /// time only until a latch has been seen. With Choreographer silent, the
    /// latch grid alone is followed.
    #[must_use]
    pub fn grid(&self, now_ns: u64) -> Option<Grid> {
        let fresh = |t: u64| now_ns.saturating_sub(t) < STALE_NS;
        if let (Some(period), Some((vsync, deadline))) = (self.ring.period_ns(), self.chor) {
            if fresh(vsync) {
                let latch = self.latch.filter(|&(a, _)| fresh(a));
                let (anchor_ns, source) = match (deadline, latch) {
                    (Some(d), _) => (
                        d.saturating_add_signed(self.latch_after_deadline.unwrap_or(0)),
                        GridSource::ChoreographerDeadline,
                    ),
                    (None, Some((a, _))) => (a, GridSource::ChoreographerLatch),
                    (None, None) => (vsync, GridSource::ChoreographerVsync),
                };
                return Some(Grid {
                    anchor_ns,
                    period_ns: period,
                    source,
                    lead_ns: self.lead.lead_ns(period),
                });
            }
        }
        let (anchor_ns, period_ns) = self.latch.filter(|&(a, _)| fresh(a))?;
        Some(Grid {
            anchor_ns,
            period_ns,
            source: GridSource::Latch,
            lead_ns: self.lead.lead_ns(period_ns),
        })
    }
}

static DISPLAY: Mutex<Option<DisplayVsync>> = Mutex::new(None);

fn with_display<R>(f: impl FnOnce(&mut DisplayVsync) -> R) -> R {
    let mut g = DISPLAY.lock().unwrap_or_else(PoisonError::into_inner);
    f(g.get_or_insert_with(|| {
        let mut d = DisplayVsync::default();
        // Read once per process: the daemon lives in the app process, so the
        // variable has to be in the app's environment, not the guest's.
        if let Some(us) = std::env::var(LEAD_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            d.fix_lead(us * 1000);
        }
        d
    }))
}

/// The display's measured period while Choreographer is locked and fresh at
/// `now_ns`, `None` otherwise.
#[must_use]
pub fn measured_period_ns(now_ns: u64) -> Option<u64> {
    with_display(|d| d.measured_period_ns(now_ns))
}

/// Feed one completed publish (see [`DisplayVsync::note_publish`]).
pub fn note_publish_latch(applied_ns: u64, latch_ns: u64) -> Option<VsyncReport> {
    with_display(|d| d.note_publish(applied_ns, latch_ns))
}

/// Feed one display vsync from the platform's vsync source (see
/// [`DisplayVsync::note_vsync`]).
///
/// The mode's advertised period bounds the fit (see [`VsyncRing::set_mode`]).
pub fn note_choreographer_vsync(vsync_ns: u64, deadline_ns: Option<u64>) -> Option<VsyncLock> {
    let mhz = u64::from(crate::advertised_refresh_mhz());
    let mode_ns = 1_000_000_000_000u64.checked_div(mhz).unwrap_or(0);
    with_display(|d| d.note_vsync(vsync_ns, deadline_ns, mode_ns))
}

/// A fit the ring rejected as too far from the mode's period since the last
/// call, with the mode's period: `(fit, mode)`. Reported once per run of
/// rejections.
pub fn take_rejected_fit() -> Option<(u64, u64)> {
    with_display(|d| d.rejected_fit.take().map(|p| (p, d.ring.mode_ns)))
}

/// Feed the latch grid's current anchor (see [`DisplayVsync::note_latch`]).
pub fn note_latch_grid(anchor_ns: u64, period_ns: u64) {
    with_display(|d| d.note_latch(anchor_ns, period_ns));
}

/// The grid the flip events follow now, `None` when no source is fresh.
#[must_use]
pub fn display_grid(now_ns: u64) -> Option<Grid> {
    with_display(|d| d.grid(now_ns))
}

/// The vblank timeline a page-flip event claims.
///
/// gamescope re-anchors its whole vblank schedule on each event's timestamp, so
/// the timestamp is the latch the frame lands on — the first latch instant of
/// the display's [`Grid`] at or after the publish returned — moved earlier by
/// the grid's lead (see [`LeadLoop`]). A frame published between the deadline
/// Choreographer predicted and the latch `SurfaceFlinger` actually took is
/// claimed for that latch, not the next: claiming the next would push
/// gamescope's next wakeup a whole period out and leave one latch with no new
/// frame. While no grid is known the timeline walks the mode's advertised
/// interval from the clock, as before any source reported.
///
/// Stamping the emit instant instead would re-anchor gamescope's grid early on
/// every frame (it arms early by design), and every divisor cap then ran fast
/// by that offset: +7 % measured across targets 20/24/30/40.
///
/// Either way a claim never moves backwards and never repeats: a second frame
/// inside one period claims the next instant, and a late frame skips whole
/// periods instead of claiming a time in the past.
#[derive(Debug, Default)]
pub struct VblankTimeline {
    claimed_ns: u64,
    started: bool,
}

impl VblankTimeline {
    /// The vblank a frame published at `now_ns` lands on. `grid` is the
    /// display's timeline when one is known, `interval_ns` the mode's advertised
    /// period, walked when it is not.
    pub fn claim(&mut self, now_ns: u64, grid: Option<Grid>, interval_ns: u64) -> u64 {
        let interval_ns = interval_ns.max(1);
        let t = match grid {
            Some(g) => {
                // At least half a period after the last claim: one claim per
                // grid instant, whatever grid (or walk) the last one was on.
                let floor = if self.started {
                    self.claimed_ns + g.lead_ns + g.period_ns / 2 + 1
                } else {
                    0
                };
                let landed = g.at_or_after(now_ns);
                let latch = g.at_or_after(now_ns.max(floor));
                if latch != landed {
                    PUSHED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                latch.saturating_sub(g.lead_ns)
            }
            None if !self.started => now_ns,
            None => {
                let mut t = self.claimed_ns + interval_ns;
                if t < now_ns {
                    t += (now_ns - t).div_ceil(interval_ns) * interval_ns;
                }
                t
            }
        };
        let t = if self.started {
            t.max(self.claimed_ns + 1)
        } else {
            t
        };
        self.started = true;
        self.claimed_ns = t;
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Retroid Pocket 6's "60 Hz" panel, as measured.
    const P: u64 = 16_670_000;
    const T0: u64 = 1_000_000_000_000;

    fn locked_ring(period: u64) -> (VsyncRing, u64) {
        let mut r = VsyncRing::default();
        let mut t = T0;
        let mut locked = None;
        for _ in 0..RING {
            if let RingEvent::Locked(p) = r.push(t) {
                locked = Some(p);
            }
            t += period;
        }
        assert_eq!(locked, Some(period));
        (r, t)
    }

    #[test]
    fn the_period_is_the_measured_spacing_not_the_mode() {
        let (r, _) = locked_ring(P);
        assert_eq!(r.period_ns(), Some(P));
    }

    #[test]
    fn no_period_before_enough_vsyncs() {
        let mut r = VsyncRing::default();
        for i in 0..MIN_LOCK as u64 - 1 {
            assert_eq!(r.push(T0 + i * P), RingEvent::None);
        }
        assert_eq!(r.period_ns(), None);
        assert_eq!(r.push(T0 + (MIN_LOCK as u64 - 1) * P), RingEvent::Locked(P));
    }

    #[test]
    fn skipped_and_jittered_callbacks_still_fit_the_period() {
        let mut r = VsyncRing::default();
        let mut n = 0u64;
        for i in 0..200u64 {
            // Every fifth callback misses a vsync; timestamps jitter by 50 us.
            n += if i % 5 == 4 { 2 } else { 1 };
            let jitter = if i % 2 == 0 { 50_000 } else { 0 };
            r.push(T0 + n * P + jitter);
        }
        let p = r.period_ns().expect("locked");
        assert!(p.abs_diff(P) < 2_000, "fitted {p}");
    }

    #[test]
    fn a_rate_change_relocks_on_the_new_period() {
        let (mut r, mut t) = locked_ring(P);
        let fast = 8_324_000;
        let mut relocked = None;
        for _ in 0..RING {
            if let RingEvent::Locked(p) = r.push(t) {
                relocked = Some(p);
            }
            t += fast;
        }
        assert_eq!(relocked, Some(fast));
        assert_eq!(r.period_ns(), Some(fast));
    }

    #[test]
    fn a_halved_rate_relocks_too() {
        // 120 Hz to 60: every step is two of the old periods, which would count
        // as a skipped vsync forever if the ring did not notice.
        let fast = 8_324_000;
        let (mut r, mut t) = locked_ring(fast);
        let mut relocked = None;
        for _ in 0..3 * RING {
            if let RingEvent::Locked(p) = r.push(t) {
                relocked = Some(p);
            }
            t += 2 * fast + 6_000;
        }
        assert_eq!(relocked, Some(2 * fast + 6_000));
    }

    #[test]
    fn a_pause_keeps_the_lock() {
        let (mut r, t) = locked_ring(P);
        assert_eq!(r.push(t + 2_000_000_000), RingEvent::None);
        assert_eq!(r.period_ns(), Some(P));
    }

    #[test]
    fn grid_instants_round_up_on_either_side_of_the_anchor() {
        let g = Grid {
            anchor_ns: 1_000,
            period_ns: 100,
            source: GridSource::Latch,
            lead_ns: 0,
        };
        assert_eq!(g.at_or_after(1_000), 1_000);
        assert_eq!(g.at_or_after(1_001), 1_100);
        assert_eq!(g.at_or_after(950), 1_000);
        assert_eq!(g.at_or_after(899), 900);
        assert_eq!(g.at_or_after(100), 100);
    }

    fn display_locked(deadline_lead: Option<u64>) -> (DisplayVsync, u64) {
        let mut d = DisplayVsync::default();
        let mut t = T0;
        for _ in 0..MIN_LOCK {
            d.note_vsync(t, deadline_lead.map(|l| t + l), 0);
            t += P;
        }
        (d, t - P)
    }

    #[test]
    fn the_deadline_is_the_stamped_instant_when_reported() {
        let (d, last) = display_locked(Some(P + 3_000_000));
        let g = d.grid(last + 1_000).expect("grid");
        assert_eq!(g.source, GridSource::ChoreographerDeadline);
        assert_eq!(g.period_ns, P);
        assert_eq!(g.anchor_ns, last + P + 3_000_000);
        // Instants fall on the deadline's phase.
        assert_eq!(g.at_or_after(last + 1_000), last + 3_000_000);
    }

    #[test]
    fn without_a_deadline_the_latch_phase_is_used_with_the_measured_period() {
        let (mut d, last) = display_locked(None);
        assert_eq!(
            d.grid(last).map(|g| g.source),
            Some(GridSource::ChoreographerVsync)
        );
        d.note_latch(last + 4_000_000, 16_666_666);
        let g = d.grid(last + 5_000_000).expect("grid");
        assert_eq!(g.source, GridSource::ChoreographerLatch);
        assert_eq!(g.anchor_ns, last + 4_000_000);
        assert_eq!(g.period_ns, P);
    }

    #[test]
    fn the_latch_grid_alone_is_the_fallback_and_stale_sources_drop_out() {
        let mut d = DisplayVsync::default();
        assert_eq!(d.grid(T0), None);
        d.note_latch(T0, 16_666_666);
        let g = d.grid(T0 + 1_000).expect("grid");
        assert_eq!(g.source, GridSource::Latch);
        assert_eq!(g.period_ns, 16_666_666);
        assert_eq!(d.grid(T0 + STALE_NS + 1), None);

        let (d, last) = display_locked(Some(P));
        assert!(d.grid(last + STALE_NS - 1).is_some());
        assert_eq!(d.grid(last + STALE_NS + 1), None);
    }

    #[test]
    fn the_lock_reports_the_latch_offset() {
        let mut d = DisplayVsync::default();
        let mut t = T0;
        d.note_latch(T0 + 500_000, 16_666_666);
        let mut lock = None;
        for _ in 0..MIN_LOCK {
            if let Some(l) = d.note_vsync(t, Some(t + 2 * P), 0) {
                lock = Some(l);
            }
            t += P;
        }
        let lock = lock.expect("locked");
        assert_eq!(lock.grid.source, GridSource::ChoreographerDeadline);
        assert_eq!(lock.latch_offset_ns, Some(500_000));
    }

    #[test]
    fn claims_follow_the_display_grid_not_the_mode() {
        let grid = Grid {
            anchor_ns: T0,
            period_ns: P,
            source: GridSource::ChoreographerDeadline,
            lead_ns: 0,
        };
        let mut tl = VblankTimeline::default();
        // gamescope publishes ~3 ms before each deadline, every frame, for
        // two minutes: every claim is the deadline it made, so the stamps walk
        // the display's 16.670 ms and not the mode's 16.667 ms.
        let mut prev = 0;
        for k in 1..7_200u64 {
            let publish = T0 + k * P - 3_000_000;
            let c = tl.claim(publish, Some(grid), 16_666_666);
            assert_eq!(c, T0 + k * P, "frame {k}");
            if prev != 0 {
                assert_eq!(c - prev, P);
            }
            prev = c;
        }
    }

    #[test]
    fn a_late_frame_skips_whole_periods_and_never_moves_back() {
        let grid = Grid {
            anchor_ns: T0,
            period_ns: P,
            source: GridSource::ChoreographerDeadline,
            lead_ns: 0,
        };
        let mut tl = VblankTimeline::default();
        assert_eq!(tl.claim(T0 + P - 1_000, Some(grid), 16_666_666), T0 + P);
        // Published 1.5 periods late: lands two periods on.
        assert_eq!(
            tl.claim(T0 + 2 * P + P / 2, Some(grid), 16_666_666),
            T0 + 3 * P
        );
        // Two publishes inside one period: the second takes the next instant.
        assert_eq!(
            tl.claim(T0 + 3 * P + 10, Some(grid), 16_666_666),
            T0 + 4 * P
        );
        assert_eq!(
            tl.claim(T0 + 3 * P + 20, Some(grid), 16_666_666),
            T0 + 5 * P
        );
    }

    #[test]
    fn switching_grids_never_moves_back() {
        let mut tl = VblankTimeline::default();
        // The walk claims first (no source yet), then a grid whose instants sit
        // a third of a period before the walk's.
        let first = tl.claim(T0, None, 16_666_666);
        assert_eq!(first, T0);
        let grid = Grid {
            anchor_ns: T0 - P / 3,
            period_ns: P,
            source: GridSource::Latch,
            lead_ns: 0,
        };
        let c = tl.claim(T0 + 1_000, Some(grid), 16_666_666);
        assert!(c > first + P / 2, "{c}");
        assert_eq!(c, T0 - P / 3 + P);
        // And back to the walk: one interval after the last claim.
        assert_eq!(tl.claim(c, None, 16_666_666), c + 16_666_666);
    }

    #[test]
    fn the_walk_is_unchanged_without_a_grid() {
        let mut tl = VblankTimeline::default();
        let i = 16_666_666;
        assert_eq!(tl.claim(T0, None, i), T0);
        assert_eq!(tl.claim(T0, None, i), T0 + i);
        assert_eq!(tl.claim(T0 + 3 * i + 5, None, i), T0 + 4 * i);
    }

    #[test]
    fn fold_reads_a_quarter_period_late_as_late() {
        let p = 100;
        assert_eq!(fold_margin(10, p), 10);
        assert_eq!(fold_margin(70, p), 70);
        assert_eq!(fold_margin(80, p), -20);
        assert_eq!(fold_margin(99, p), -1);
        assert_eq!(fold_margin(-30, p), 70);
    }

    #[test]
    fn a_lead_stamps_ahead_of_the_latch_and_keeps_the_rules() {
        let grid = Grid {
            anchor_ns: T0,
            period_ns: P,
            source: GridSource::ChoreographerDeadline,
            lead_ns: 3_000_000,
        };
        let mut tl = VblankTimeline::default();
        // Published 1 ms before the latch: stamped 3 ms before it, i.e. in the
        // past, as a KMS event's vblank is by the time it is read.
        assert_eq!(
            tl.claim(T0 + P - 1_000_000, Some(grid), 16_666_666),
            T0 + P - 3_000_000
        );
        // The next frame, on time.
        assert_eq!(
            tl.claim(T0 + 2 * P - 2_000_000, Some(grid), 16_666_666),
            T0 + 2 * P - 3_000_000
        );
        // Late (just after the latch): the latch after.
        assert_eq!(
            tl.claim(T0 + 3 * P + 100, Some(grid), 16_666_666),
            T0 + 4 * P - 3_000_000
        );
    }

    /// Model the session: gamescope commits `red_zone` ahead of the stamp it
    /// was handed one period later, the apply lands `pipeline` after that with
    /// a deterministic spread, `SurfaceFlinger` latches on the grid. Returns
    /// the steady lead and the share of publishes that missed their latch.
    fn run_session(pipeline: i64, spread: i64, frames: u64) -> (u64, f64, i64) {
        let mut d = DisplayVsync::default();
        let mut tl = VblankTimeline::default();
        let mut t = T0;
        for _ in 0..MIN_LOCK {
            d.note_vsync(t, Some(t + P), 0);
            t += P;
        }
        let mut stamp = tl.claim(t, d.grid(t), 16_666_666);
        let (mut late, mut counted) = (0u64, 0u64);
        let mut margins = Vec::new();
        for k in 0..frames {
            d.note_vsync(t, Some(t + P), 0);
            t += P;
            // A spread of -spread..+spread, in a fixed pseudo-random order.
            let jitter = ((k * 7919) % 101) as i64 * 2 * spread / 100 - spread;
            let applied = (stamp + P - 1_650_000) as i64 + pipeline + jitter;
            let applied = applied as u64;
            let g = d.grid(applied).expect("grid");
            let latch = Grid { lead_ns: 0, ..g }.at_or_after(applied);
            stamp = tl.claim(applied, Some(g), 16_666_666);
            d.note_publish(applied, latch);
            if k > frames / 2 {
                counted += 1;
                let aimed = stamp + g.lead_ns;
                if latch != aimed {
                    late += 1;
                }
                margins.push(latch as i64 - applied as i64);
            }
        }
        margins.sort_unstable();
        (
            d.lead.lead_ns(P),
            late as f64 / counted as f64,
            margins[margins.len() / 10],
        )
    }

    #[test]
    fn the_lead_holds_the_tenth_percentile_at_its_target() {
        for pipeline in [0i64, 1_000_000, 2_500_000] {
            let (lead, late, p10) = run_session(pipeline, 2_000_000, 6_000);
            assert!(late < 0.001, "pipeline {pipeline}: late {late} lead {lead}");
            assert!(
                (p10 - TARGET_P10_NS as i64).abs() < 400_000,
                "pipeline {pipeline}: p10 {p10} lead {lead}"
            );
        }
    }

    #[test]
    fn a_fixed_lead_does_not_move() {
        let mut l = LeadLoop::fixed(4_000_000);
        for _ in 0..100 {
            l.update(-1, P);
        }
        assert_eq!(l.lead_ns(P), 4_000_000);
    }

    #[test]
    fn a_publish_after_the_deadline_but_before_the_latch_claims_that_latch() {
        let mut d = DisplayVsync::default();
        d.fix_lead(0);
        let mut t = T0;
        for _ in 0..MIN_LOCK {
            d.note_vsync(t, Some(t + P), 0);
            t += P;
        }
        // `SurfaceFlinger` takes every publish 1.5 ms after the deadline.
        for k in 0..64u64 {
            let latch = T0 + (MIN_LOCK as u64 + 1 + k) * P + 1_500_000;
            d.note_publish(latch - 4_000_000, latch);
        }
        let g = d.grid(t).expect("grid");
        let mut tl = VblankTimeline::default();
        let deadline = t + 3 * P;
        // Published 1 ms after the deadline: still the latch 0.5 ms later.
        let c = tl.claim(deadline + 1_000_000, Some(g), 16_666_666);
        assert!(c.abs_diff(deadline + 1_500_000) < 50_000, "{c}");
    }

    #[test]
    fn the_cross_check_reports_every_ten_seconds() {
        let mut d = DisplayVsync::default();
        let mut t = T0;
        let mut reports = Vec::new();
        for k in 0..1_300u64 {
            d.note_vsync(t, Some(t + P), 0);
            // Every 60th vsync carries no publish.
            if k > MIN_LOCK as u64 && k % 60 != 0 {
                let latch = t + P;
                if let Some(r) = d.note_publish(latch - 3_000_000, latch) {
                    reports.push(r);
                }
            }
            t += P;
        }
        assert_eq!(reports.len(), 2);
        let r = &reports[0];
        assert_eq!(r.source, Some(GridSource::ChoreographerDeadline));
        assert_eq!(r.period_ns, P);
        assert_eq!(r.margin_ns, [3_000_000; 3]);
        assert_eq!(r.late, 0);
        assert!(r.vsyncs > r.publishes, "{r:?}");
        assert!((590..=600).contains(&r.vsyncs), "{r:?}");
    }

    #[test]
    fn a_starved_thread_cannot_halve_the_rate_the_mode_names() {
        // 120 Hz mode; the callback thread delivers only every second vsync
        // for 64 straight vsyncs. Each step counts two of the mode's periods,
        // and the period stays 8.33 ms.
        let p120 = 8_333_333u64;
        let mut r = VsyncRing::default();
        r.set_mode(p120);
        let mut t = T0;
        for _ in 0..RING {
            r.push(t);
            t += p120;
        }
        assert_eq!(r.period_ns(), Some(p120));
        for _ in 0..2 * RING {
            r.push(t);
            t += 2 * p120;
        }
        assert_eq!(r.period_ns(), Some(p120));
    }

    #[test]
    fn a_fit_outside_the_mode_band_is_rejected_once() {
        // The mode says 60 Hz; the callbacks come at 16.0 ms (4 % off).
        let mut r = VsyncRing::default();
        r.set_mode(16_666_667);
        let mut events = Vec::new();
        for i in 0..64u64 {
            let e = r.push(T0 + i * 16_000_000);
            if e != RingEvent::None {
                events.push(e);
            }
        }
        assert_eq!(events, vec![RingEvent::Rejected(16_000_000)]);
        assert_eq!(r.period_ns(), None);
    }

    #[test]
    fn a_new_mode_starts_the_fit_again() {
        let mut r = VsyncRing::default();
        r.set_mode(16_666_667);
        let mut t = T0;
        for _ in 0..RING {
            r.push(t);
            t += P;
        }
        assert_eq!(r.period_ns(), Some(P));
        r.set_mode(8_333_333);
        assert_eq!(r.period_ns(), None);
        let mut locked = None;
        for _ in 0..RING {
            if let RingEvent::Locked(p) = r.push(t) {
                locked = Some(p);
            }
            t += 8_337_836;
        }
        assert_eq!(locked, Some(8_337_836));
    }

    #[test]
    fn the_fixed_lead_is_clamped_to_half_a_period() {
        assert_eq!(LeadLoop::fixed(50_000_000).lead_ns(P), P / 2);
    }
}
