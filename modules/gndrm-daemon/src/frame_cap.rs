//! The daemon's half of the fps cap: the target, and the note that carries it
//! to the shim.
//!
//! The cap is enforced in the ICU shim, which is the side that sees a slot's
//! release and holds the only gate that can block the guest (the guest blocks
//! in `vkAcquireNextImageKHR` until a slot frees). The shim derives the release
//! floor — one period (`1 / target`) since its own previous release — from the
//! target, so the only thing that has to cross the wire is the target itself,
//! and the only thing that has to be decided here is its value.
//!
//! The value arrives through `nativeSetFpsLimit` (the app's limiter) and is
//! held here so the scan-out channel can answer a subscribe and push a change
//! from one place.

use gndrm_uapi::wire::{FrameCapNote, FPS_UNCAPPED};

/// The fps cap's target.
#[derive(Debug, Clone, Copy)]
pub struct FrameCap {
    target_fps: u32,
}

impl FrameCap {
    /// A cap with no target: everything uncapped.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            target_fps: FPS_UNCAPPED,
        }
    }

    /// The current target, [`FPS_UNCAPPED`] when the limiter is off.
    #[must_use]
    pub const fn target_fps(&self) -> u32 {
        self.target_fps
    }

    /// Whether a cap is in force.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.target_fps != FPS_UNCAPPED
    }

    /// Set the target, reporting whether it changed so a caller pushes the new
    /// value to the shim only when it did.
    pub fn set_target_fps(&mut self, fps: u32) -> bool {
        if fps == self.target_fps {
            return false;
        }
        self.target_fps = fps;
        true
    }

    /// The note to hand a subscribed shim: the current target.
    #[must_use]
    pub const fn note(&self) -> FrameCapNote {
        FrameCapNote {
            fps: self.target_fps,
        }
    }
}

impl Default for FrameCap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_uncapped() {
        let cap = FrameCap::new();
        assert_eq!(cap.target_fps(), FPS_UNCAPPED);
        assert!(!cap.is_active());
        assert_eq!(cap.note().fps, FPS_UNCAPPED);
    }

    #[test]
    fn target_change_is_reported_once() {
        let mut cap = FrameCap::new();
        assert!(cap.set_target_fps(40));
        assert!(cap.is_active());
        assert_eq!(cap.target_fps(), 40);
        assert_eq!(cap.note().fps, 40);
        assert!(!cap.set_target_fps(40));
        // Turning the cap off is itself a change the shim must be told.
        assert!(cap.set_target_fps(FPS_UNCAPPED));
        assert!(!cap.is_active());
        assert_eq!(cap.note().fps, FPS_UNCAPPED);
    }
}
