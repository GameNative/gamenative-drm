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
//! | synced | on | on | cap | `cap × (m−1)`, at most `refresh` |
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
    /// How many frames the generator may add per source frame, `None` when it
    /// may not add any.
    ///
    /// The ceiling is a rate, so a panel that cannot show `acquire × multiplier`
    /// frames leaves the generator fewer per source frame; it never leaves it
    /// more. `multiplier` is the configured one, and the return value never
    /// exceeds `multiplier - 1`.
    pub(crate) fn generate_per_source(&self, multiplier: u32) -> Option<u32> {
        let generate = self.generate_per_s?;
        let acquire = self.acquire_per_s?;
        let asked = multiplier.saturating_sub(1);
        Some(asked.min(generate / acquire.max(1)))
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
            // Synced, the panel bounds what it can show; unsynced, only the
            // cap does, and the compositor drops the remainder.
            Some(if vsync && refresh_hz > 0 {
                raw.min(refresh_hz)
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
        // synced, cap, generation: generation fills up to the panel
        assert_eq!(
            limits(true, Some(40), Some(2), 60),
            Limits {
                acquire_per_s: Some(40),
                generate_per_s: Some(40)
            }
        );
        // synced, cap, generation, panel too slow for neither
        assert_eq!(
            limits(true, Some(40), Some(4), 60),
            Limits {
                acquire_per_s: Some(40),
                generate_per_s: Some(60)
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
    fn generation_per_source_narrows_when_the_panel_is_the_ceiling() {
        // 40 acquires, 60 panel, ×4 asked: the panel leaves 20 generated in
        // total, i.e. none per source frame at this rate.
        let l = limits(true, Some(40), Some(4), 60);
        assert_eq!(l.generate_per_s, Some(60));
        assert_eq!(l.generate_per_source(4), Some(1));
        // Unsynced the same numbers leave the ask intact.
        let l = limits(false, Some(40), Some(4), 60);
        assert_eq!(l.generate_per_source(4), Some(3));
    }
}
