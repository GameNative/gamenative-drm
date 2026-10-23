//! Placing frames on the panel.
//!
//! The presenter commits generated frames on a timer. The display takes the
//! newest buffer it holds at each of its latch points, one panel period apart,
//! so a timer that is not phased to those points beats against them: every so
//! often two commits fall inside one period and the second replaces the first
//! before it is shown. This module turns an instant a frame is wanted at into
//! the latch point it lands on and the commit instant that reaches it, and
//! bounds how many frames a source period can carry at all.

/// The panel's latch grid, as the daemon reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    /// One latch point, `CLOCK_MONOTONIC` ns.
    pub anchor_ns: u64,
    /// The spacing of latch points, ns.
    pub period_ns: u64,
    /// How far ahead of a latch point a commit has to be made to be the buffer
    /// that point takes, ns.
    pub lead_ns: u64,
}

impl Grid {
    /// The first latch point at or after `t`.
    #[must_use]
    pub fn latch_at_or_after(&self, t: u64) -> u64 {
        if self.period_ns == 0 {
            return t;
        }
        if t <= self.anchor_ns {
            let back = (self.anchor_ns - t) / self.period_ns;
            return self.anchor_ns - back * self.period_ns;
        }
        self.anchor_ns + (t - self.anchor_ns).div_ceil(self.period_ns) * self.period_ns
    }

    /// The latch point the next queued frame takes: the one after
    /// `prev_latch_ns` (0 when nothing is queued), or the first one a commit
    /// made now can still reach when that has passed. Frames queued one after
    /// another take consecutive points, one per period, which is what the panel
    /// shows, in order; a run of `n` frames is this point and the `n - 1` after
    /// it.
    #[must_use]
    pub fn first_latch(&self, now_ns: u64, prev_latch_ns: u64) -> u64 {
        let reachable = self.latch_at_or_after(now_ns.saturating_add(self.lead_ns));
        if prev_latch_ns == 0 {
            reachable
        } else {
            reachable.max(prev_latch_ns.saturating_add(self.period_ns))
        }
    }

    /// The instant a frame has to be committed by to be taken at `latch_ns`.
    #[must_use]
    pub fn commit_for(&self, latch_ns: u64) -> u64 {
        latch_ns.saturating_sub(self.lead_ns)
    }
}

/// The rate the source is paced to while generating `multiplier` frames per
/// source frame for a `panel_hz` panel: the user's `cap`, unless the panel
/// cannot show two frames per source frame at it, in which case half the
/// panel's rate, so the panel is filled evenly (a 60 cap on a 90 Hz panel runs
/// the game at 45 and shows 90; on a 120 Hz panel it stays 60 and shows 120).
/// A cap of 0 (none) or an unknown panel is returned as is.
#[must_use]
pub fn source_pace(cap: u32, panel_hz: u32, multiplier: u32) -> u32 {
    if cap == 0 || panel_hz == 0 || multiplier < 2 {
        return cap;
    }
    // The tolerance `panel_budget` applies: a 120 Hz panel reported as 119 still
    // holds a 60 cap twice.
    if u64::from(panel_hz) * 17 / 16 >= 2 * u64::from(cap) {
        cap
    } else {
        panel_hz / 2
    }
}

/// How many generated frames a panel can show between two source frames, at
/// most `capacity`.
///
/// A panel shows one frame per period; a source period at `source_hz` holds
/// `panel_hz / source_hz` of them, one of which is the source frame itself.
/// Anything above that is committed and never shown, and which frames are
/// dropped drifts, so the output looks uneven. A rate a hair under an exact
/// multiple (a 120 Hz panel reporting 119.88, a guest measured at 61 against a
/// 60 cap) still gets the multiple: the presenter's queue bound absorbs the
/// difference as an occasional skipped batch. 0 for either rate means it is
/// unknown, which leaves the capacity alone.
#[must_use]
pub fn panel_budget(panel_hz: u32, source_hz: u32, capacity: u32) -> u32 {
    if panel_hz == 0 || source_hz == 0 {
        return capacity;
    }
    let slots = u64::from(panel_hz) * 17 / 16 / u64::from(source_hz);
    u32::try_from(slots)
        .unwrap_or(u32::MAX)
        .saturating_sub(1)
        .min(capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: u64 = 8_333_333;

    fn grid(anchor: u64, lead: u64) -> Grid {
        Grid {
            anchor_ns: anchor,
            period_ns: P,
            lead_ns: lead,
        }
    }

    #[test]
    fn latch_points_lie_on_the_grid_either_side_of_the_anchor() {
        let g = grid(1_000_000_000, 0);
        assert_eq!(g.latch_at_or_after(1_000_000_000), 1_000_000_000);
        assert_eq!(g.latch_at_or_after(1_000_000_001), 1_000_000_000 + P);
        assert_eq!(g.latch_at_or_after(1_000_000_000 + P), 1_000_000_000 + P);
        assert_eq!(g.latch_at_or_after(1_000_000_000 - 1), 1_000_000_000);
        assert_eq!(g.latch_at_or_after(1_000_000_000 - P), 1_000_000_000 - P);
        assert_eq!(
            g.latch_at_or_after(1_000_000_000 - P - 1),
            1_000_000_000 - P
        );
        assert_eq!(g.latch_at_or_after(0), 1_000_000_000 % P);
    }

    #[test]
    fn the_first_frame_takes_the_first_point_its_commit_can_reach() {
        let g = grid(1_000_000_000, 4_000_000);
        // 1 ms after a point: the next is 7.3 ms away, within the lead's reach.
        let latch = g.first_latch(1_001_000_000, 0);
        assert_eq!(latch, 1_000_000_000 + P);
        assert_eq!(g.commit_for(latch), latch - 4_000_000);
        // Inside the lead of a point: that point cannot be reached any more.
        assert_eq!(
            g.first_latch(1_000_000_000 + P - 3_000_000, 0),
            1_000_000_000 + 2 * P
        );
    }

    #[test]
    fn a_queued_frame_takes_the_point_after_the_previous_one() {
        let g = grid(1_000_000_000, 4_000_000);
        let prev = 1_000_000_000 + 5 * P;
        // Asked well ahead of time: the point after the previous frame's.
        assert_eq!(g.first_latch(1_000_000_000, prev), prev + P);
        // A run of three from there is three consecutive points.
        let first = g.first_latch(1_000_000_000, prev);
        assert_eq!(
            [first, first + P, first + 2 * P],
            [prev + P, prev + 2 * P, prev + 3 * P]
        );
    }

    #[test]
    fn a_frame_asked_for_late_skips_to_the_first_reachable_point() {
        let g = grid(1_000_000_000, 4_000_000);
        let prev = 1_000_000_000 + P;
        let now = 1_000_000_000 + 10 * P + 1;
        assert_eq!(g.first_latch(now, prev), 1_000_000_000 + 11 * P);
    }

    #[test]
    fn no_period_means_no_grid() {
        let g = Grid {
            anchor_ns: 5,
            period_ns: 0,
            lead_ns: 7,
        };
        assert_eq!(g.first_latch(100, 0), 107);
        assert_eq!(g.commit_for(107), 100);
    }

    #[test]
    fn source_pace_halves_the_panel_when_the_cap_does_not_fit_twice() {
        assert_eq!(source_pace(60, 120, 2), 60);
        assert_eq!(source_pace(60, 119, 3), 60);
        assert_eq!(source_pace(60, 90, 2), 45);
        assert_eq!(source_pace(90, 144, 2), 72);
        assert_eq!(source_pace(60, 60, 2), 30);
        assert_eq!(source_pace(30, 60, 2), 30);
        assert_eq!(source_pace(60, 90, 1), 60);
        assert_eq!(source_pace(0, 90, 2), 0);
        assert_eq!(source_pace(60, 0, 2), 60);
    }

    #[test]
    fn panel_budget_counts_the_slots_a_source_period_leaves() {
        assert_eq!(panel_budget(120, 60, 3), 1);
        assert_eq!(panel_budget(120, 40, 3), 2);
        assert_eq!(panel_budget(120, 30, 3), 3);
        assert_eq!(panel_budget(120, 30, 2), 2);
        assert_eq!(panel_budget(60, 60, 3), 0);
        assert_eq!(panel_budget(60, 30, 3), 1);
        assert_eq!(panel_budget(90, 45, 3), 1);
        assert_eq!(panel_budget(90, 60, 3), 0);
        assert_eq!(panel_budget(144, 72, 3), 1);
        assert_eq!(panel_budget(144, 48, 3), 2);
    }

    #[test]
    fn panel_budget_tolerates_a_rate_just_over_the_multiple() {
        assert_eq!(panel_budget(120, 61, 3), 1);
        assert_eq!(panel_budget(120, 62, 3), 1);
        assert_eq!(panel_budget(119, 60, 3), 1);
        assert_eq!(panel_budget(120, 41, 3), 2);
    }

    #[test]
    fn panel_budget_leaves_an_unknown_rate_alone() {
        assert_eq!(panel_budget(0, 60, 3), 3);
        assert_eq!(panel_budget(120, 0, 3), 3);
    }
}
