//! The panel's latch grid as the flip-event timeline sees it.
//!
//! A card client that sets [`GNDRM_CLIENT_CAP_LATCH_GRID`] (gamescope) gets its
//! page-flip events stamped on `SurfaceFlinger`'s latch grid instead of the
//! session's 1200 Hz timeline: each stamp is `anchor + k*period - lead`, the
//! first such point at or after the publish returned and never earlier than the
//! previous stamp plus half a period. gamescope re-anchors its vblank timer on
//! every flip stamp (`page_flip_handler` -> `MarkVBlank`), so its paint tick
//! then lands `lead` + its own draw offset ahead of the latch the next frame
//! targets, every period, instead of at a phase the first flip happened to set.
//!
//! The grid itself is measured by the presentation layer (`gndrm-android`'s
//! `vsync` tracker, from every publish's completion latch time) and handed here
//! through [`set`]; this module only holds it, plus the lead, and owns the
//! claim arithmetic so it is host-testable.
//!
//! [`GNDRM_CLIENT_CAP_LATCH_GRID`]: gndrm_uapi::drm::GNDRM_CLIENT_CAP_LATCH_GRID

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;

/// The lead a stamp is placed ahead of its latch point when the environment
/// does not set one: the publish costs ~1.0 ms median and ~1.4 ms p90
/// (`docs/12-present-latency-budget.md`), plus the acquire fence margin.
pub const DEFAULT_LEAD_NS: u64 = 2_000_000;

/// Environment override for the lead, in microseconds.
pub const LEAD_ENV: &str = "GNDRM_GS_LATCH_LEAD_US";

/// The latch point the grid is anchored on (`CLOCK_MONOTONIC` ns), 0 before
/// any latch was seen.
static ANCHOR_NS: AtomicU64 = AtomicU64::new(0);
/// The panel period in ns, 0 while unknown.
static PERIOD_NS: AtomicU64 = AtomicU64::new(0);
/// The lead, resolved from [`LEAD_ENV`] once.
static LEAD_NS: OnceLock<u64> = OnceLock::new();
/// Card connections that set the cap and are still open.
static CLIENTS: AtomicU32 = AtomicU32::new(0);
/// Where a claimed latch point is reported (the presentation layer's
/// latch-miss counter). Set once by the presentation layer.
static CLAIM_SINK: OnceLock<fn(u64)> = OnceLock::new();

/// Record the grid: the latch point it is anchored on and its period.
pub fn set(anchor_ns: u64, period_ns: u64) {
    if anchor_ns == 0 || period_ns == 0 {
        return;
    }
    // Period first: a reader that sees the new anchor also sees its period.
    PERIOD_NS.store(period_ns, Ordering::Relaxed);
    ANCHOR_NS.store(anchor_ns, Ordering::Release);
}

/// Record the panel period alone (the app re-reported the rate); the anchor is
/// kept and re-measured on the next completion.
pub fn set_period(period_ns: u64) {
    if period_ns != 0 {
        PERIOD_NS.store(period_ns, Ordering::Relaxed);
    }
}

/// The panel period in ns, 0 while unknown.
#[must_use]
pub fn period_ns() -> u64 {
    PERIOD_NS.load(Ordering::Relaxed)
}

/// The grid as `(anchor, period, lead)`, `None` until a latch was seen.
#[must_use]
pub fn get() -> Option<(u64, u64, u64)> {
    let anchor = ANCHOR_NS.load(Ordering::Acquire);
    let period = PERIOD_NS.load(Ordering::Relaxed);
    (anchor != 0 && period != 0).then(|| (anchor, period, lead_ns()))
}

/// The lead a stamp sits ahead of its latch point: [`LEAD_ENV`] when it holds a
/// whole number of microseconds in `0..=50_000`, else [`DEFAULT_LEAD_NS`].
#[must_use]
pub fn lead_ns() -> u64 {
    *LEAD_NS.get_or_init(|| parse_lead_us(std::env::var(LEAD_ENV).ok().as_deref()))
}

/// [`LEAD_ENV`]'s value as a lead in ns (see [`lead_ns`]).
#[must_use]
pub fn parse_lead_us(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|us| *us <= 50_000)
        .map_or(DEFAULT_LEAD_NS, |us| us * 1_000)
}

/// A card connection set (`true`) or dropped (`false`) the cap.
pub fn set_client(on: bool) {
    if on {
        CLIENTS.fetch_add(1, Ordering::Relaxed);
    } else {
        // Balanced by construction: a connection clears only a cap it set.
        CLIENTS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Whether a connected client is placing its frames on the grid.
#[must_use]
pub fn client_connected() -> bool {
    CLIENTS.load(Ordering::Relaxed) != 0
}

/// The lead a frame-generation presenter should commit by while a grid client
/// is connected: half a period past that client's own wake, `P/2 + lead + 3 ms`,
/// but never beyond one period past it, which is the next wake.
#[must_use]
pub fn presenter_lead_ns(period_ns: u64, lead_ns: u64) -> u64 {
    (period_ns / 2 + lead_ns + 3_000_000).min(period_ns + lead_ns)
}

/// Register where claimed latch points are reported. First caller wins.
pub fn set_claim_sink(sink: fn(u64)) {
    let _ = CLAIM_SINK.set(sink);
}

/// Report the latch point a just-published frame was stamped for.
pub(crate) fn report_claim(latch_ns: u64) {
    if let Some(sink) = CLAIM_SINK.get() {
        sink(latch_ns);
    }
}

/// How a frame stamped for a latch point actually latched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatchOutcome {
    /// Within half a period of the claimed latch point.
    OnTime,
    /// More than half a period after it: the lead was too short (the publish,
    /// or the acquire fence, missed `SurfaceFlinger`'s deadline).
    Late,
    /// More than half a period before it: the publish made an earlier latch
    /// than the stamp gave it, so the lead is longer than it needs to be.
    Early,
}

/// Classify one completion: `latch_ns` the latch time `SurfaceFlinger`
/// reported, `claimed_ns` the latch point the frame's flip was stamped for.
#[must_use]
pub fn classify(latch_ns: u64, claimed_ns: u64, period_ns: u64) -> LatchOutcome {
    let half = period_ns / 2;
    if latch_ns > claimed_ns.saturating_add(half) {
        LatchOutcome::Late
    } else if latch_ns.saturating_add(half) < claimed_ns {
        LatchOutcome::Early
    } else {
        LatchOutcome::OnTime
    }
}

/// The first grid point `anchor + k*period - lead` at or after `floor_ns`.
#[must_use]
pub fn next_grid_point(floor_ns: u64, anchor_ns: u64, period_ns: u64, lead_ns: u64) -> u64 {
    let period = i128::from(period_ns.max(1));
    let base = i128::from(anchor_ns) - i128::from(lead_ns);
    let floor = i128::from(floor_ns);
    let k = (floor - base).div_euclid(period);
    let mut t = base + k * period;
    if t < floor {
        t += period;
    }
    u64::try_from(t).unwrap_or(floor_ns)
}

/// The vblank timeline a page-flip event claims.
///
/// A real KMS page-flip event is delivered at the vblank the flip takes effect
/// on, and gamescope re-anchors its entire vblank grid off that timestamp
/// (`Backends/DRMBackend.cpp` `page_flip_handler` → `vblankmanager` `MarkVBlank`).
/// As the display, gndrm owns that schedule.
///
/// **Legacy timeline** ([`Self::claim`]), every client without the latch-grid
/// cap: events are quantized to the session rate
/// (`SESSION_REFRESH_MHZ`, `objects.rs`) — one interval apart while frames keep up,
/// stepping whole intervals when one is late. The first claim is the instant
/// itself, so the phase is whatever the first flip happened to be. Stamping the
/// *emit* instant instead re-anchors that grid slightly early on every frame.
/// The grid is armed early by design (its aim-early offset, so a real kernel can
/// hold the flip to the panel cycle), so the error integrates into the grid's
/// rate: every divisor-based fps cap then runs fast by roughly that offset over
/// one interval — measured as +7 % on this port across four divisors (targets
/// 20/24/30/40 → 21/26/32/43).
///
/// **Latch grid** ([`Self::claim_on_grid`]), a client with the cap: the stamp
/// is the first `anchor + k*P - lead` at or after the publish and at or after
/// the previous stamp plus `P/2`, i.e. the latch the published frame targets,
/// minus the lead. Before the grid is known it walks like the legacy timeline
/// at the panel period.
#[derive(Debug)]
pub struct VblankTimeline {
    interval_ns: u64,
    claimed_ns: u64,
    started: bool,
}

impl VblankTimeline {
    /// A timeline whose legacy walk steps `interval_ns`.
    #[must_use]
    pub fn new(interval_ns: u64) -> Self {
        Self {
            interval_ns: interval_ns.max(1),
            claimed_ns: 0,
            started: false,
        }
    }

    /// Legacy: the next boundary of the session rate at or after `now_ns`.
    /// Never moves backwards, so a late frame skips whole vblanks instead of
    /// claiming a time in the past.
    pub fn claim(&mut self, now_ns: u64) -> u64 {
        self.walk(now_ns, self.interval_ns)
    }

    /// Latch grid: see the type doc. `grid` is [`get`]'s answer and
    /// `fallback_period_ns` the panel period to walk at while it is `None` (0
    /// for unknown, which walks at the legacy interval). Returns the stamp and,
    /// when it was placed on the grid, the latch point it stands for.
    pub fn claim_on_grid(
        &mut self,
        now_ns: u64,
        grid: Option<(u64, u64, u64)>,
        fallback_period_ns: u64,
    ) -> (u64, Option<u64>) {
        let Some((anchor, period, lead)) = grid else {
            let interval = if fallback_period_ns == 0 {
                self.interval_ns
            } else {
                fallback_period_ns
            };
            return (self.walk(now_ns, interval), None);
        };
        let floor = if self.started {
            now_ns.max(self.claimed_ns.saturating_add(period / 2))
        } else {
            now_ns
        };
        let t = next_grid_point(floor, anchor, period, lead);
        self.started = true;
        self.claimed_ns = t;
        (t, Some(t + lead))
    }

    fn walk(&mut self, now_ns: u64, interval_ns: u64) -> u64 {
        if !self.started {
            self.started = true;
            self.claimed_ns = now_ns;
            return now_ns;
        }
        let interval = interval_ns.max(1);
        let mut t = self.claimed_ns + interval;
        while t < now_ns {
            t += interval;
        }
        self.claimed_ns = t;
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 120 Hz.
    const P: u64 = 8_333_333;
    const LEAD: u64 = 2_000_000;
    const ANCHOR: u64 = 1_000_000_000;

    fn grid() -> Option<(u64, u64, u64)> {
        Some((ANCHOR, P, LEAD))
    }

    #[test]
    fn grid_point_is_the_first_at_or_after_the_floor() {
        // Exactly on a point.
        assert_eq!(
            next_grid_point(ANCHOR - LEAD, ANCHOR, P, LEAD),
            ANCHOR - LEAD
        );
        // Just past one: the next.
        assert_eq!(
            next_grid_point(ANCHOR - LEAD + 1, ANCHOR, P, LEAD),
            ANCHOR + P - LEAD
        );
        // Far after the anchor.
        let floor = ANCHOR + 100 * P + 123;
        assert_eq!(
            next_grid_point(floor, ANCHOR, P, LEAD),
            ANCHOR + 101 * P - LEAD
        );
        // Before the anchor: the grid extends backwards.
        assert_eq!(
            next_grid_point(ANCHOR - 3 * P, ANCHOR, P, LEAD),
            ANCHOR - 2 * P - LEAD
        );
    }

    #[test]
    fn the_first_grid_claim_is_the_next_latch_after_the_publish_minus_the_lead() {
        let mut v = VblankTimeline::new(833_333);
        // The publish returned 1 ms after a latch point: that latch is gone, the
        // frame targets the next one, and the stamp is that one minus the lead.
        let now = ANCHOR + 10 * P + 1_000_000;
        let (stamp, latch) = v.claim_on_grid(now, grid(), P);
        assert_eq!(stamp, ANCHOR + 11 * P - LEAD);
        assert_eq!(latch, Some(ANCHOR + 11 * P));
        // Inside the lead before a latch point: that point is already too
        // close, so the next one.
        let mut v = VblankTimeline::new(833_333);
        let now = ANCHOR + 20 * P - LEAD / 2;
        assert_eq!(v.claim_on_grid(now, grid(), P).0, ANCHOR + 21 * P - LEAD);
        // Ahead of the lead: this period's point.
        let mut v = VblankTimeline::new(833_333);
        let now = ANCHOR + 30 * P - LEAD - 500_000;
        assert_eq!(v.claim_on_grid(now, grid(), P).0, ANCHOR + 30 * P - LEAD);
    }

    #[test]
    fn a_second_flip_in_the_same_period_takes_the_next_point() {
        let mut v = VblankTimeline::new(833_333);
        let now = ANCHOR + 5 * P + 1_000_000;
        let first = v.claim_on_grid(now, grid(), P).0;
        // A cursor flip 200 us later: never the same point, never less than
        // half a period after the previous stamp.
        let second = v.claim_on_grid(now + 200_000, grid(), P).0;
        assert_eq!(second, first + P);
    }

    #[test]
    fn the_half_period_floor_holds_across_a_grid_shift() {
        let mut v = VblankTimeline::new(833_333);
        let now = ANCHOR + 5 * P + 1_000_000;
        let first = v.claim_on_grid(now, grid(), P).0;
        // The tracker moved the anchor 3 ms earlier: the shifted grid's point
        // after the previous claim is 3 ms before that claim + P, still more
        // than P/2 after it, so it is taken.
        let shifted = Some((ANCHOR - 3_000_000, P, LEAD));
        let second = v.claim_on_grid(now + 100_000, shifted, P).0;
        assert_eq!(second, first + P - 3_000_000);
        assert!(second >= first + P / 2);
        // Moved 6 ms earlier from there: first + P - 6 ms is under P/2 after
        // `second`, so the claim steps one more period.
        let mut v = VblankTimeline::new(833_333);
        let first = v.claim_on_grid(now, grid(), P).0;
        let shifted = Some((ANCHOR + P - 6_000_000, P, LEAD));
        let second = v.claim_on_grid(now, shifted, P).0;
        assert!(second >= first + P / 2, "{second} vs {first}");
        assert_eq!(second, first + 2 * P - 6_000_000);
    }

    #[test]
    fn a_late_publish_skips_whole_periods() {
        let mut v = VblankTimeline::new(833_333);
        let first = v.claim_on_grid(ANCHOR + 1_000_000, grid(), P).0;
        assert_eq!(first, ANCHOR + P - LEAD);
        // Three periods later.
        let now = ANCHOR + 4 * P;
        assert_eq!(v.claim_on_grid(now, grid(), P).0, ANCHOR + 5 * P - LEAD);
    }

    #[test]
    fn without_a_grid_the_claim_walks_at_the_panel_period() {
        let mut v = VblankTimeline::new(833_333);
        assert_eq!(v.claim_on_grid(5_000, None, P), (5_000, None));
        assert_eq!(v.claim_on_grid(5_000, None, P), (5_000 + P, None));
        // Unknown period: the legacy interval.
        assert_eq!(v.claim_on_grid(5_000, None, 0).0, 5_000 + P + 833_333);
        // The grid arrives: the next stamp is on it and past the walk's last.
        let (stamp, latch) = v.claim_on_grid(10_000, Some((ANCHOR, P, LEAD)), P);
        assert!(stamp >= 5_000 + P + 833_333 + P / 2);
        assert_eq!((stamp + LEAD - ANCHOR % P) % P, 0);
        assert_eq!(latch, Some(stamp + LEAD));
    }

    #[test]
    fn the_legacy_claim_is_unchanged() {
        // First claim is the instant; then one interval apart; a late frame
        // steps whole intervals.
        let mut v = VblankTimeline::new(833_333);
        assert_eq!(v.claim(1_000_000_500_000), 1_000_000_500_000);
        assert_eq!(v.claim(1_000_000_500_000), 1_000_001_333_333);
        assert_eq!(v.claim(1_000_003_000_000), 1_000_003_833_332);
    }

    #[test]
    fn the_presenter_lead_is_mid_way_between_two_wakes() {
        // 120 Hz: P/2 + 2 + 3 ms = 9.17 ms, inside one period past the wake.
        assert_eq!(presenter_lead_ns(P, LEAD), P / 2 + LEAD + 3_000_000);
        // 60 Hz: 8.33 + 5 = 13.33 ms.
        assert_eq!(presenter_lead_ns(16_666_666, LEAD), 8_333_333 + 5_000_000);
        // A short period caps at one period past the wake.
        assert_eq!(presenter_lead_ns(4_000_000, LEAD), 4_000_000 + LEAD);
    }

    #[test]
    fn lead_env_parses_microseconds_within_bounds() {
        assert_eq!(parse_lead_us(None), DEFAULT_LEAD_NS);
        assert_eq!(parse_lead_us(Some("1500")), 1_500_000);
        assert_eq!(parse_lead_us(Some(" 0 ")), 0);
        assert_eq!(parse_lead_us(Some("50001")), DEFAULT_LEAD_NS);
        assert_eq!(parse_lead_us(Some("-1")), DEFAULT_LEAD_NS);
        assert_eq!(parse_lead_us(Some("2ms")), DEFAULT_LEAD_NS);
    }

    #[test]
    fn classify_scores_half_a_period_either_side() {
        let c = ANCHOR;
        assert_eq!(classify(c, c, P), LatchOutcome::OnTime);
        assert_eq!(classify(c + P / 2, c, P), LatchOutcome::OnTime);
        assert_eq!(classify(c + P, c, P), LatchOutcome::Late);
        assert_eq!(classify(c - P, c, P), LatchOutcome::Early);
    }
}
