//! Games that fail unless the swapchain has exactly the image count they asked
//! for.
//!
//! The Vulkan spec lets a swapchain have more images than `minImageCount`, and
//! the shim normally adds spares for pacing. These games treat that as fatal or
//! misbehave. Mesa's X11 WSI handles them with `vk_x11_strict_image_count`; the
//! guest's swapchain is the shim's own under the WSI proxy, so Mesa never sees
//! it and the shim carries the same list. See `docs/23-strict-image-count.md`.

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

#[derive(Debug, PartialEq)]
struct StrictGame {
    game: Match,
    /// Mesa's `vk_x11_override_min_image_count`, where it differs from the
    /// minimum the shim already reports.
    min_image_count: Option<u32>,
}

const fn listed(game: Match) -> StrictGame {
    StrictGame {
        game,
        min_image_count: None,
    }
}

/// Every `vk_x11_strict_image_count` entry of Mesa's `00-mesa-defaults.conf`.
static STRICT_GAMES: &[StrictGame] = &[
    listed(Match::Exe("RainbowSix_Vulkan.exe")),
    listed(Match::Exe("R6-Extraction.exe")),
    StrictGame {
        game: Match::Exe("Hades.exe"),
        min_image_count: Some(3),
    },
    listed(Match::Engine("UnrealEngine4", 23)),
    listed(Match::Exe("DOOMx64vk.exe")),
    listed(Match::Exe("Youngblood_x64vk.exe")),
    listed(Match::Exe("NewColossus_x64vk.exe")),
    listed(Match::AppName("metroexodus")),
    listed(Match::Exe("AtlasFallen (VK).exe")),
    listed(Match::AppName("DetroitBecomeHuman")),
];

/// The kernel keeps 15 bytes of a process name.
const COMM_LEN: usize = 15;

fn find_strict_game(
    comm: &str,
    app_name: Option<&str>,
    engine_name: Option<&str>,
    engine_version: u32,
) -> Option<&'static StrictGame> {
    STRICT_GAMES.iter().find(|g| match g.game {
        Match::Exe(exe) => comm == &exe[..exe.len().min(COMM_LEN)],
        Match::AppName(name) => app_name.is_some_and(|a| a.contains(name)),
        Match::Engine(name, max_version) => {
            engine_name.is_some_and(|e| e.contains(name)) && engine_version <= max_version
        }
    })
}

static MATCHED: OnceLock<&'static StrictGame> = OnceLock::new();

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
            crate::logi!(gndrm_obf::obf!("strict image count on: {:?}", game.game));
        }
    }
}

pub fn strict() -> bool {
    MATCHED.get().is_some()
}

pub fn min_image_count() -> Option<u32> {
    MATCHED.get().and_then(|g| g.min_image_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(comm: &str) -> Option<&'static StrictGame> {
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
    fn only_hades_changes_the_reported_minimum() {
        let raised: Vec<_> = STRICT_GAMES
            .iter()
            .filter(|g| g.min_image_count.is_some())
            .map(|g| g.game)
            .collect();
        assert_eq!(raised, [Match::Exe("Hades.exe")]);
    }
}
