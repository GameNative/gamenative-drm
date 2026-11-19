//! The acquire and generation limits, in frames per second.
//!
//! One rule, four inputs: whether the guest asked for a synced present mode,
//! its cap, frame generation's multiplier, and the panel's refresh. gamescope
//! is told the guest's own mode and nothing else — the cap and the multiplier
//! never rewrite it — so what a cap or a multiplier changes is *how often the
//! guest may acquire* and *how many frames the generator may add*.
//!
//! Synced (FIFO / FIFO-relaxed), the panel is the ceiling in both directions: a
//! cap below it is the cap, no cap is the refresh, and generation may fill up
//! to the refresh but not past it. Unsynced (immediate / mailbox) the guest is
//! free to run past the panel when it asks to — the compositor drops what it
//! cannot show — so only a cap or generation bounds it.
//!
//! | present | cap | LSFG | acquire | generated |
//! |---|---|---|---|---|
//! | synced | on | off | cap | — |
//! | synced | off | off | refresh | — |
//! | synced | on | on | cap | `cap × (m−1)`, at most `refresh − cap` |
//! | synced | off | on | `refresh / m` | `acquire × (m−1)` |
//! | unsynced | on | off | cap | — |
//! | unsynced | off | off | none | — |
//! | unsynced | on | on | cap | `cap × (m−1)` |
//! | unsynced | off | on | `refresh / m` | `acquire × (m−1)` |

/// What one session allows, in frames per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// Frames/s the guest may acquire. `None` = no limit.
    pub acquire_per_s: Option<u32>,
    /// Frames/s the generator may insert. `None` = generation off.
    pub generate_per_s: Option<u32>,
}

impl Limits {
    /// How many frames the generator may add after this source frame, `None`
    /// when it may not add any.
    ///
    /// The ceiling is a rate, not a ratio, so the batch is not required to be a
    /// whole number of frames per source frame: a cap 70 session on a 120 Hz
    /// panel leaves 50 generated against 70 source frames -- five frames every
    /// seven. `credit` carries the remainder between calls, which is what makes
    /// the generator's output land on the rate in the table instead of on the
    /// floor of it. Flooring the ratio instead switched generation off entirely
    /// at any cap above half the refresh, where the table still promises the
    /// panel's leftover.
    ///
    /// The return value never exceeds `multiplier - 1`: asking for more than the
    /// configured multiplier is not what a rate bound means.
    pub(crate) fn generate_batch(&self, multiplier: u32, credit: &mut u64) -> Option<u32> {
        let generate = self.generate_per_s? as u64;
        let acquire = self.acquire_per_s? as u64;
        if acquire == 0 {
            return Some(0);
        }
        *credit += generate;
        let mut batch = *credit / acquire;
        *credit %= acquire;
        let asked = u64::from(multiplier.saturating_sub(1));
        if batch > asked {
            batch = asked;
        }
        Some(batch as u32)
    }
}

/// The limits for one session. `refresh_hz` is the panel's, and 0 when the
/// daemon has not reported it yet: rules that need it then impose no limit
/// rather than a wrong one.
pub(crate) fn limits(
    vsync: bool,
    cap_per_s: Option<u32>,
    multiplier: Option<u32>,
    refresh_hz: u32,
) -> Limits {
    let cap = cap_per_s.filter(|c| *c > 0);
    let mult = multiplier.filter(|m| *m >= 2);

    let acquire_per_s = match (cap, mult) {
        // A cap the user asked for is the cap, whatever else is on.
        (Some(c), _) => Some(c),
        // Uncounted, generation derives the source rate from the panel so its
        // output lands on the refresh exactly.
        (None, Some(m)) => (refresh_hz > 0).then(|| (refresh_hz / m).max(1)),
        // No cap: a synced present mode is already held by the panel.
        (None, None) if vsync => (refresh_hz > 0).then_some(refresh_hz),
        (None, None) => None,
    };

    let generate_per_s = match (mult, acquire_per_s) {
        (Some(m), Some(a)) => {
            let raw = a.saturating_mul(m - 1);
            // Synced, the panel bounds what it can show *and the source frames
            // are part of that budget*: acquire + generated <= refresh, so what
            // is left for the generator is `refresh - acquire`. Clamping at
            // `refresh` alone let a cap 40 ×2 session on a 60 Hz panel produce
            // 80 commits a second, of which the compositor dropped 20 — the
            // budget is the panel's, so it belongs in the same subtraction.
            //
            // That leftover is a rate, not a ratio of whole frames: at 60 Hz,
            // cap 40, ×2 it is 20 generated against 40 source frames, one every
            // other source frame. `generate_batch` carries the remainder so the
            // average lands on it; flooring the ratio instead zeroed generation
            // at any cap above half the refresh, where the panel still had room.
            //
            // Unsynced, only the cap does, and the compositor drops the
            // remainder by design.
            Some(if vsync && refresh_hz > 0 {
                raw.min(refresh_hz.saturating_sub(a))
            } else {
                raw
            })
        }
        _ => None,
    };

    Limits {
        acquire_per_s,
        generate_per_s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table in the module notes, row by row.
    #[test]
    fn the_table_holds() {
        // synced, cap, no generation
        assert_eq!(
            limits(true, Some(40), None, 60),
            Limits {
                acquire_per_s: Some(40),
                generate_per_s: None
            }
        );
        // synced, no cap, no generation: the panel is the limit
        assert_eq!(
            limits(true, None, None, 60),
            Limits {
                acquire_per_s: Some(60),
                generate_per_s: None
            }
        );
        // synced, cap, generation: generation fills what the panel has left
        // after the source frames
        assert_eq!(
            limits(true, Some(40), Some(2), 60),
            Limits {
                acquire_per_s: Some(40),
                generate_per_s: Some(20)
            }
        );
        // synced, cap, generation, panel too slow for both: the panel still
        // leaves its 20 after the source frames whatever the multiplier asks
        assert_eq!(
            limits(true, Some(40), Some(4), 60),
            Limits {
                acquire_per_s: Some(40),
                generate_per_s: Some(20)
            }
        );
        // synced, no cap, generation: source derives from the panel
        assert_eq!(
            limits(true, None, Some(2), 60),
            Limits {
                acquire_per_s: Some(30),
                generate_per_s: Some(30)
            }
        );
        // unsynced, cap, no generation
        assert_eq!(
            limits(false, Some(40), None, 60),
            Limits {
                acquire_per_s: Some(40),
                generate_per_s: None
            }
        );
        // unsynced, no cap: nothing bounds it
        assert_eq!(
            limits(false, None, None, 60),
            Limits {
                acquire_per_s: None,
                generate_per_s: None
            }
        );
        // unsynced, cap, generation: the cap alone bounds it
        assert_eq!(
            limits(false, Some(40), Some(2), 60),
            Limits {
                acquire_per_s: Some(40),
                generate_per_s: Some(40)
            }
        );
        // unsynced, no cap, generation: source still derives from the panel
        assert_eq!(
            limits(false, None, Some(2), 60),
            Limits {
                acquire_per_s: Some(30),
                generate_per_s: Some(30)
            }
        );
    }

    #[test]
    fn an_unreported_panel_imposes_no_limit() {
        assert_eq!(
            limits(true, None, None, 0),
            Limits {
                acquire_per_s: None,
                generate_per_s: None
            }
        );
        // A cap still stands: it does not need the panel.
        assert_eq!(
            limits(true, Some(40), None, 0),
            Limits {
                acquire_per_s: Some(40),
                generate_per_s: None
            }
        );
    }

    #[test]
    fn a_multiplier_of_one_is_not_generation() {
        assert_eq!(
            limits(true, Some(40), Some(1), 60),
            Limits {
                acquire_per_s: Some(40),
                generate_per_s: None
            }
        );
    }

    #[test]
    fn the_generator_spends_a_rate_that_is_not_a_whole_frame_per_source() {
        // 40 acquires, 60 panel, ×4 asked: the panel leaves 20 generated in
        // total -- one every other source frame, not none.
        let l = limits(true, Some(40), Some(4), 60);
        assert_eq!(l.generate_per_s, Some(20));
        let mut credit = 0u64;
        let batches: Vec<u32> = (0..8).map(|_| l.generate_batch(4, &mut credit).unwrap()).collect();
        assert_eq!(batches, vec![0, 1, 0, 1, 0, 1, 0, 1]);
        // 20 generated over 40 source frames is exactly the panel's leftover.
        assert_eq!(batches.iter().sum::<u32>(), 4);

        // and a rate that is more than one per source keeps the multiplier's cap
        let l = limits(true, Some(30), Some(2), 60);
        let mut credit = 0u64;
        assert!((0..20).all(|_| l.generate_batch(2, &mut credit).unwrap() <= 1));
    }

    #[test]
    fn a_cap_above_half_the_panel_still_gets_its_leftover() {
        // 70 on a 120 Hz panel: the table's total is 70 + 50, not 70 + 0.
        let l = limits(true, Some(70), Some(2), 120);
        assert_eq!(l.acquire_per_s, Some(70));
        assert_eq!(l.generate_per_s, Some(50));
        let mut credit = 0u64;
        let batches: Vec<u32> = (0..70).map(|_| l.generate_batch(2, &mut credit).unwrap()).collect();
        // 50 frames over 70 source frames, ~5 every 7
        assert_eq!(batches.iter().sum::<u32>(), 50);
        assert!(batches.iter().all(|&b| b <= 1));
    }

    /// `refresh - acquire` is spent, not rounded away: at 60 Hz, cap 40, ×2 the
    /// panel leaves 20 generated against 40 source frames -- one every other
    /// source frame.
    #[test]
    fn a_cap_above_half_the_panel_spends_its_leftover() {
        let l = limits(true, Some(40), Some(2), 60);
        assert_eq!(l.generate_per_s, Some(20));
        let mut credit = 0u64;
        let batches: Vec<u32> = (0..40).map(|_| l.generate_batch(2, &mut credit).unwrap()).collect();
        assert_eq!(batches.iter().sum::<u32>(), 20);
        // A cap at or below half the panel is one per source.
        let l = limits(true, Some(30), Some(2), 60);
        assert_eq!(l.generate_per_s, Some(30));
        let mut credit = 0u64;
        assert_eq!(l.generate_batch(2, &mut credit), Some(1));
        // A cap above the panel cannot underflow: nothing is left to generate.
        let l = limits(true, Some(120), Some(2), 60);
        assert_eq!(l.generate_per_s, Some(0));
        let mut credit = 0u64;
        assert_eq!(l.generate_batch(2, &mut credit), Some(0));
    }
}
