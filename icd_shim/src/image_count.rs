//! Games that fail unless the swapchain has exactly the image count they asked
//! for.
//!
//! The Vulkan spec lets a swapchain have more images than `minImageCount`, and
//! creation may raise the count for mailbox or for frame generation. These
//! games treat that as fatal or misbehave. Mesa's X11 WSI handles them with
//! `vk_x11_strict_image_count`; the guest's swapchain is the shim's own under
//! the WSI proxy, so Mesa never sees it and the shim carries the same list.
//!
//! The list is the only remaining exception: creation honours every other
//! guest's ask (Mesa's convention, `proxy::swapchain_slots`), so what a listed
//! game needs today is not a different minimum but the absence of the bumps —
//! and, behind a swapchain with no image to spare, hidden slots to present
//! from. See `docs/23-strict-image-count.md`.

use std::ffi::{c_char, CStr};
use std::sync::OnceLock;

use crate::vk::VkApplicationInfo;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Match {
    /// Mesa's `executable`, compared the way `/proc/self/comm` holds it.
    Exe(&'static str),
    /// Mesa's `application_name_match`.
    AppName(&'static str),
    /// Mesa's `engine_name_match` and an `engine_versions` range starting at 0.
    Engine(&'static str, u32),
}

/// Every `vk_x11_strict_image_count` entry of Mesa's `00-mesa-defaults.conf`.
///
/// Mesa's `vk_x11_override_min_image_count` has no place here: it exists to
/// raise a minimum that would otherwise be lower than what the game needs, and
/// the shim advertises `proxy::SC_DEFAULT_IMAGES` for every surface.
static STRICT_GAMES: &[Match] = &[
    Match::Exe("RainbowSix_Vulkan.exe"),
    Match::Exe("R6-Extraction.exe"),
    Match::Exe("Hades.exe"),
    Match::Engine("UnrealEngine4", 23),
    Match::Exe("DOOMx64vk.exe"),
    Match::Exe("Youngblood_x64vk.exe"),
    Match::Exe("NewColossus_x64vk.exe"),
    Match::AppName("metroexodus"),
    Match::Exe("AtlasFallen (VK).exe"),
    Match::AppName("DetroitBecomeHuman"),
];

/// The hidden slots behind a swapchain that asked for fewer than three images.
///
/// A swapchain of 3 or more already has an image to spare, so it gets none. Below
/// that it gets the frame on screen, the one committed behind it, and one for each
/// of the two frames the guest can have between its present and the commit.
///
/// The rule is the ask, not the game. It began as the strict list's exception, but
/// the need is a property of the count: a guest with two images cannot pipeline
/// past the hold the display imposes (`min(cap, depth x refresh / hold)`), and Doom
/// 2016 measured 24 fps against a cap of 60 with the GPU idle before it had any.
/// The caller must pass the guest's *requested* count, never the served one: a
/// mailbox floor can raise the served count past the ask, and a guest that asked
/// for two has no image to spare however many it was given.
pub fn hidden_slot_count(guest_slots: u32) -> u32 {
    if guest_slots < 3 {
        4
    } else {
        0
    }
}

/// The kernel keeps 15 bytes of a process name.
const COMM_LEN: usize = 15;

fn find_strict_game(
    comm: &str,
    app_name: Option<&str>,
    engine_name: Option<&str>,
    engine_version: u32,
) -> Option<&'static Match> {
    STRICT_GAMES.iter().find(|g| match **g {
        Match::Exe(exe) => comm == &exe[..exe.len().min(COMM_LEN)],
        Match::AppName(name) => app_name.is_some_and(|a| a.contains(name)),
        Match::Engine(name, max_version) => {
            engine_name.is_some_and(|e| e.contains(name)) && engine_version <= max_version
        }
    })
}

static MATCHED: OnceLock<&'static Match> = OnceLock::new();

unsafe fn c_str<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

/// Called for each instance the guest creates, before any of its swapchains.
///
/// # Safety
/// `app` is null or the guest's valid `VkApplicationInfo`.
pub unsafe fn note_instance(app: *const VkApplicationInfo) {
    let (app_name, engine_name, engine_version) = if app.is_null() {
        (None, None, 0)
    } else {
        let a = unsafe { &*app };
        unsafe {
            (
                c_str(a.pApplicationName),
                c_str(a.pEngineName),
                a.engineVersion,
            )
        }
    };
    let comm = std::fs::read_to_string("/proc/self/comm").unwrap_or_default();
    if let Some(game) = find_strict_game(comm.trim_end(), app_name, engine_name, engine_version) {
        if MATCHED.set(game).is_ok() {
            crate::logi!("strict image count on: {:?}", game);
        }
    }
}

pub fn strict() -> bool {
    MATCHED.get().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(comm: &str) -> Option<&'static Match> {
        find_strict_game(comm, None, None, 0)
    }

    #[test]
    fn exe_matches_the_name_the_kernel_keeps() {
        assert!(exe("DOOMx64vk.exe").is_some());
        assert!(exe("NewColossus_x64").is_some());
        assert!(exe("Youngblood_x64v").is_some());
        assert!(exe("AtlasFallen (VK").is_some());
    }

    #[test]
    fn other_processes_do_not_match() {
        for comm in [
            "",
            "DOOMx64.exe",
            "DOOMEternalx64v",
            "Hades2.exe",
            "gamescope",
            "Xwayland",
        ] {
            assert_eq!(exe(comm), None, "{comm}");
        }
    }

    #[test]
    fn application_name_matches_anywhere_in_the_name() {
        let found = find_strict_game("wine", Some("metroexodus_v2"), None, 0);
        assert!(found.is_some());
        assert_eq!(
            find_strict_game("wine", Some("MetroExodus.exe"), None, 0),
            None
        );
    }

    #[test]
    fn engine_matches_inside_its_version_range() {
        let ue = |version| find_strict_game("game", None, Some("UnrealEngine4.23"), version);
        assert!(ue(23).is_some());
        assert_eq!(ue(24), None);
        assert_eq!(find_strict_game("game", None, Some("DXVK"), 0), None);
    }

    #[test]
    fn only_a_swapchain_with_no_image_to_spare_gets_hidden_slots() {
        // Doom 2016 asks for two and is served mailbox, which floors the count at
        // four; the ring is sized from the ask, so this is the case that matters.
        assert_eq!(hidden_slot_count(2), 4);
        // Three already has one to spare, and a larger ask needs nothing.
        assert_eq!(hidden_slot_count(3), 0);
    }

    /// The list is the only exception left: no entry asks creation for
    /// anything a conforming guest would not itself have asked for.
    #[test]
    fn every_entry_is_a_name_and_nothing_else() {
        assert_eq!(STRICT_GAMES.len(), 10);
        assert!(STRICT_GAMES.contains(&Match::Exe("Hades.exe")));
        assert!(STRICT_GAMES.contains(&Match::Exe("DOOMx64vk.exe")));
    }
}
