//! Which WSI-scoped extensions the guest sees: one policy table over the
//! registry's WSI scope (`vk_dispatch_gen::VK_WSI_EXTENSIONS`).
//!
//! Under the WSI proxy the guest's surfaces and swapchains are the shim's own:
//! the driver never sees one. Every extension whose subject is a surface, a
//! swapchain or a present is therefore a question about the shim, and the
//! driver's answer to it says nothing. The driver's list used to pass through
//! with a hand-written list of exceptions, so every WSI extension a new driver
//! listed and the shim did not implement leaked to the guest (present timing,
//! FIFO latest ready, mutable-format swapchains, ...), each one a feature the
//! guest could build on and the shim would silently drop.
//!
//! The policy, per WSI-scoped extension:
//! - [`Served::Driver`]: the driver's copy is listed and enabled; the shim
//!   intercepts the commands that take its surfaces and swapchains.
//! - [`Served::Shim`]: the driver's copy is hidden and masked at creation, the
//!   shim advertises its own (`spec` version) and answers the extension itself.
//! - hidden ([`HIDDEN`], with a reason): removed from the list the guest sees,
//!   masked at instance and device creation, its feature structs reported false
//!   and stripped from the driver's create chain, its commands refused where the
//!   shim has no handler of its own.
//!
//! The completeness test fails when the registry gains a WSI-scoped extension
//! this table does not classify; see `docs/21-wsi-extension-policy.md`.

use crate::vk_dispatch_gen::{ExtKind, WsiExt, VK_WSI_EXTENSIONS};
use core::ffi::{c_void, CStr};

/// How the shim serves a WSI-scoped extension.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Served {
    /// The driver's copy passes through; the shim intercepts its commands.
    Driver,
    /// The driver's copy is hidden; the shim advertises this spec version.
    Shim { spec: u32 },
}

/// The WSI-scoped extensions the shim serves. The `Shim` entries are appended
/// to the guest's lists in this order (instance, then device).
// One row per name, kept single-line so the table stays grep-able.
#[rustfmt::skip]
pub static SERVED: &[(&CStr, Served)] = &[
    // Instance. The surface, its capabilities-2 queries and the colour spaces
    // are the driver's names: the shim answers each query for its own surfaces
    // (`gn_vkGetPhysicalDeviceSurface*`). `VK_EXT_swapchain_colorspace` is the
    // driver's copy only: the shim does not add it, because advertising it made
    // vkd3d-proton titles present once per swapchain and then stop.
    (c"VK_KHR_surface", Served::Driver),
    (c"VK_KHR_get_surface_capabilities2", Served::Driver),
    (c"VK_EXT_swapchain_colorspace", Served::Driver),
    // The three surface platforms the proxy creates surfaces for: winewayland,
    // winex11 (the gamescope path) and Mesa's zink WSI.
    (c"VK_KHR_wayland_surface", Served::Shim { spec: 6 }),
    (c"VK_KHR_xlib_surface", Served::Shim { spec: 6 }),
    (c"VK_KHR_xcb_surface", Served::Shim { spec: 6 }),
    // What `VK_KHR_swapchain_maintenance1` depends on (`crate::maint1`).
    (c"VK_KHR_surface_maintenance1", Served::Shim { spec: 6 }),
    (c"VK_EXT_surface_maintenance1", Served::Shim { spec: 6 }),
    // Device. The swapchain and HDR metadata keep the driver's names (the proxy
    // adds `VK_EXT_hdr_metadata` itself when the driver lacks it).
    (c"VK_KHR_swapchain", Served::Driver),
    (c"VK_EXT_hdr_metadata", Served::Driver),
    // Present ids and waits (`crate::present`) and swapchain maintenance
    // (`crate::maint1`), served for the shim's own swapchain.
    (crate::present::EXT_PRESENT_ID, Served::Shim { spec: 1 }),
    (crate::present::EXT_PRESENT_WAIT, Served::Shim { spec: 1 }),
    (crate::maint1::EXT_SWAPCHAIN_MAINTENANCE1_KHR, Served::Shim { spec: 1 }),
    (crate::maint1::EXT_SWAPCHAIN_MAINTENANCE1_EXT, Served::Shim { spec: 1 }),
];

/// The WSI-scoped extensions the shim hides, each with why.
#[rustfmt::skip]
pub static HIDDEN: &[(&str, &str)] = &[
    ("VK_AMD_display_native_hdr", "no native-HDR display mode on the shim's surfaces"),
    ("VK_EXT_acquire_drm_display", "no displays: gamescope owns the output"),
    ("VK_EXT_direct_mode_display", "no displays: gamescope owns the output"),
    ("VK_EXT_display_control", "no display power or vblank events on the shim's swapchain"),
    ("VK_EXT_display_surface_counter", "no surface counters on the shim's surfaces"),
    ("VK_EXT_headless_surface", "the proxy serves only the wayland/xlib/xcb surfaces"),
    ("VK_EXT_image_compression_control_swapchain", "the shim allocates its own swapchain slots"),
    ("VK_EXT_multisampled_render_to_swapchain", "the shim's swapchain ignores the creation flag"),
    ("VK_EXT_present_mode_fifo_latest_ready", "the shim's swapchain offers no such present mode"),
    ("VK_EXT_present_timing", "no present timing from the shim's swapchain"),
    ("VK_GOOGLE_display_timing", "no present timing from the shim's swapchain"),
    ("VK_GOOGLE_surfaceless_query", "the proxy answers surface queries only for its surfaces"),
    ("VK_HUAWEI_hdr_vivid", "the shim's swapchain ignores HDR Vivid metadata"),
    ("VK_KHR_android_surface", "the guest presents through the proxy, never an ANativeWindow"),
    ("VK_KHR_display", "no displays: gamescope owns the output"),
    ("VK_KHR_display_swapchain", "no displays: gamescope owns the output"),
    ("VK_KHR_get_display_properties2", "no displays: gamescope owns the output"),
    ("VK_KHR_incremental_present", "the shim's present ignores present regions"),
    ("VK_KHR_present_id2", "not served; guests fall back to the v1 present id"),
    ("VK_KHR_present_mode_fifo_latest_ready", "the shim's swapchain offers no such present mode"),
    ("VK_KHR_present_wait2", "not served; guests fall back to the v1 present wait"),
    ("VK_KHR_shared_presentable_image", "the shim's swapchain has no shared present modes"),
    ("VK_KHR_surface_protected_capabilities", "the shim's surface queries do not answer it"),
    ("VK_KHR_swapchain_mutable_format", "the shim's swapchain ignores the mutable-format flag"),
    ("VK_NV_display_stereo", "no displays: gamescope owns the output"),
    ("VK_NV_low_latency2", "its markers and sleeps pace the driver's swapchain, not the shim's"),
    ("VK_NV_present_barrier", "no present barrier on the shim's swapchain"),
    ("VK_NV_present_metering", "the shim's present ignores metering"),
];

/// The registry row for `name`, when it is WSI-scoped.
pub fn wsi_ext(name: &CStr) -> Option<&'static WsiExt> {
    let n = name.to_str().ok()?;
    VK_WSI_EXTENSIONS
        .binary_search_by(|e| e.0.cmp(n))
        .ok()
        .map(|i| &VK_WSI_EXTENSIONS[i])
}

/// How `name` is served, `None` for a name the shim does not serve.
pub fn served(name: &CStr) -> Option<Served> {
    SERVED.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

/// A WSI-scoped extension the shim does not serve. A WSI-scoped name missing
/// from both tables counts as hidden too (the completeness test names it).
pub fn hidden(name: &CStr) -> bool {
    wsi_ext(name).is_some() && served(name).is_none()
}

/// Whether the driver's copy of `name` is kept from the guest: removed from the
/// enumerated list and masked at creation. True for every hidden name and for
/// the names the shim advertises itself.
pub fn driver_copy_hidden(name: &CStr) -> bool {
    wsi_ext(name).is_some() && served(name) != Some(Served::Driver)
}

/// The names the shim advertises itself for `kind`, with their spec versions,
/// in [`SERVED`] order.
pub fn shim_advertised(kind: ExtKind) -> impl Iterator<Item = (&'static CStr, u32)> {
    SERVED.iter().filter_map(move |(n, s)| match s {
        Served::Shim { spec } if wsi_ext(n).is_some_and(|e| e.1 == kind) => Some((*n, *spec)),
        _ => None,
    })
}

/// A command of a hidden extension: never handed out from the driver. Only
/// commands no core version and no non-WSI extension requires are listed per
/// extension, and a command a served extension also requires stays available.
///
/// Hiding the name is not enough: the guest got `vkWaitForPresentKHR` from the
/// driver together with a shim swapchain handle the driver had never seen, and
/// vkd3d-proton's present task stalled on it. The reverse is wrong too: refusing
/// a command while advertising its extension handed DXVK's frame thread a NULL
/// entry point (it spun in exception dispatch), which is why a command is only
/// refused when every extension that names it is hidden.
pub fn refused_command(name: &CStr) -> bool {
    let Ok(n) = name.to_str() else {
        return false;
    };
    let mut owners = VK_WSI_EXTENSIONS
        .iter()
        .filter(|e| e.3.contains(&n))
        .peekable();
    owners.peek().is_some() && owners.all(|e| served_str(e.0).is_none())
}

/// [`served`] for a generated (NUL-less) name.
fn served_str(name: &str) -> Option<Served> {
    SERVED
        .iter()
        .find(|(n, _)| n.to_bytes() == name.as_bytes())
        .map(|(_, s)| *s)
}

/// The feature-struct sTypes stripped from the driver's create chain: the
/// structs of every WSI-scoped extension whose driver copy is kept from the
/// guest. The shim's own features are its answer, not the driver's, and a
/// hidden extension's struct names a feature the driver must not be asked for.
pub fn stripped_feature_stypes() -> impl Iterator<Item = i32> {
    VK_WSI_EXTENSIONS
        .iter()
        .filter(|e| served_str(e.0) != Some(Served::Driver))
        .flat_map(|e| e.2.iter().map(|f| f.0))
}

/// The feature structs of the hidden extensions, `(sType, VkBool32 count)`.
fn hidden_features() -> impl Iterator<Item = (i32, u32)> {
    VK_WSI_EXTENSIONS
        .iter()
        .filter(|e| served_str(e.0).is_none())
        .flat_map(|e| e.2.iter().copied())
}

/// Report every hidden extension's features false in a guest's
/// `vkGetPhysicalDeviceFeatures2` chain: the driver's yes would send the guest
/// to a feature it cannot enable.
///
/// # Safety
/// `p_next` must be the chain head of a `VkPhysicalDeviceFeatures2` the shim is
/// about to return to the guest.
pub unsafe fn zero_hidden_features(p_next: *mut c_void) {
    let bools = |s: i32| hidden_features().find(|f| f.0 == s).map(|f| f.1);
    unsafe { zero_features_in(p_next, &bools) }
}

/// Zero the leading `VkBool32` members of every struct in a features chain
/// for which `bools` names a count.
///
/// # Safety
/// `p_next` must be a valid Vulkan structure chain the shim may write to, and
/// `bools` must never name more members than the struct's leading VkBool32s.
pub unsafe fn zero_features_in(p_next: *mut c_void, bools: &dyn Fn(i32) -> Option<u32>) {
    /// The two leading members every Vulkan structure shares.
    #[repr(C)]
    struct Head {
        s_type: i32,
        p_next: *mut c_void,
    }
    let mut head = p_next.cast::<Head>();
    for _ in 0..64 {
        if head.is_null() {
            return;
        }
        // SAFETY: `head` is a structure in the guest's chain.
        let node = unsafe { &*head };
        if let Some(bools) = bools(node.s_type) {
            // SAFETY: the structure declares this type, so its first `bools`
            // members after the header are VkBool32s.
            let first = unsafe { head.add(1).cast::<u32>() };
            for i in 0..bools as usize {
                unsafe { *first.add(i) = 0 };
            }
        }
        head = node.p_next.cast::<Head>();
    }
}

/// `names` the policy hides, joined for a log line.
pub fn hidden_names<'a>(names: impl IntoIterator<Item = &'a CStr>) -> String {
    let mut out: Vec<&str> = Vec::new();
    for n in names {
        if let Ok(s) = n.to_str() {
            if hidden(n) && !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_served(name: &str) -> bool {
        served_str(name).is_some()
    }

    /// Every WSI-scoped extension in the registry is classified: served, or
    /// hidden with a reason. A registry update that adds one fails here until it
    /// is placed in `SERVED` or `HIDDEN`.
    #[test]
    fn every_wsi_extension_is_classified() {
        let unclassified: Vec<&str> = VK_WSI_EXTENSIONS
            .iter()
            .map(|e| e.0)
            .filter(|n| !is_served(n) && !HIDDEN.iter().any(|(h, _)| h == n))
            .collect();
        assert!(
            unclassified.is_empty(),
            "WSI-scoped extensions with no policy (add each to SERVED or HIDDEN): {unclassified:?}"
        );
        for (n, reason) in HIDDEN {
            assert!(!reason.is_empty(), "{n} is hidden without a reason");
            assert!(
                VK_WSI_EXTENSIONS.iter().any(|e| e.0 == *n),
                "{n} is in HIDDEN but not WSI-scoped in the registry"
            );
        }
        for (n, _) in SERVED {
            assert!(
                wsi_ext(n).is_some(),
                "{n:?} is in SERVED but not WSI-scoped in the registry"
            );
        }
    }

    #[test]
    fn no_name_is_both_served_and_hidden() {
        for (n, _) in HIDDEN {
            assert!(!is_served(n), "{n} is both served and hidden");
        }
        let mut names: Vec<&str> = HIDDEN.iter().map(|h| h.0).collect();
        names.extend(SERVED.iter().map(|s| s.0.to_str().unwrap()));
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "a name is listed twice");
    }

    /// The instance names the enumeration appends are exactly the shim-served
    /// instance extensions, in the order the proxy has always listed them, and
    /// the device ones exactly the present and maintenance names.
    #[test]
    fn shim_advertised_names_match_the_enumeration() {
        let inst: Vec<(&CStr, u32)> = shim_advertised(ExtKind::Instance).collect();
        assert_eq!(
            inst,
            vec![
                (c"VK_KHR_wayland_surface", 6),
                (c"VK_KHR_xlib_surface", 6),
                (c"VK_KHR_xcb_surface", 6),
                (c"VK_KHR_surface_maintenance1", 6),
                (c"VK_EXT_surface_maintenance1", 6),
            ]
        );
        let dev: Vec<(&CStr, u32)> = shim_advertised(ExtKind::Device).collect();
        assert_eq!(
            dev,
            vec![
                (c"VK_KHR_present_id", 1),
                (c"VK_KHR_present_wait", 1),
                (c"VK_KHR_swapchain_maintenance1", 1),
                (c"VK_EXT_swapchain_maintenance1", 1),
            ]
        );
        // The colour-space name is the driver's: never added by the shim.
        assert_eq!(served(c"VK_EXT_swapchain_colorspace"), Some(Served::Driver));
        assert!(!driver_copy_hidden(c"VK_EXT_swapchain_colorspace"));
    }

    #[test]
    fn served_and_hidden_names_are_masked_as_before() {
        for n in [
            c"VK_KHR_present_id",
            c"VK_KHR_present_wait",
            c"VK_KHR_swapchain_maintenance1",
            c"VK_EXT_swapchain_maintenance1",
            c"VK_KHR_present_id2",
            c"VK_KHR_present_wait2",
            c"VK_EXT_present_timing",
            c"VK_KHR_swapchain_mutable_format",
        ] {
            assert!(driver_copy_hidden(n), "{n:?}");
        }
        for n in [
            c"VK_KHR_swapchain",
            c"VK_KHR_surface",
            c"VK_EXT_hdr_metadata",
            c"VK_KHR_maintenance5",
        ] {
            assert!(!driver_copy_hidden(n), "{n:?}");
        }
        assert!(hidden(c"VK_KHR_present_wait2"));
        assert!(!hidden(c"VK_KHR_present_wait"));
        // Not WSI-scoped: frame boundaries and image compression touch
        // presents and swapchains only on the side.
        assert!(wsi_ext(c"VK_EXT_frame_boundary").is_none());
        assert!(wsi_ext(c"VK_EXT_image_compression_control").is_none());
        assert!(wsi_ext(c"VK_NV_representative_fragment_test").is_none());
    }

    #[test]
    fn hidden_commands_are_refused_and_served_ones_are_not() {
        assert!(refused_command(c"vkWaitForPresent2KHR"));
        assert!(refused_command(c"vkGetPastPresentationTimingEXT"));
        assert!(refused_command(c"vkSetLatencySleepModeNV"));
        for n in [
            c"vkWaitForPresentKHR",
            c"vkReleaseSwapchainImagesKHR",
            c"vkReleaseSwapchainImagesEXT",
            c"vkQueuePresentKHR",
            c"vkSetHdrMetadataEXT",
            c"vkGetPhysicalDeviceSurfaceCapabilities2KHR",
            // Also VK_KHR_device_group's: not the swapchain's to refuse.
            c"vkAcquireNextImage2KHR",
            c"vkGetDeviceGroupSurfacePresentModesKHR",
            c"vkCreateDevice",
        ] {
            assert!(!refused_command(n), "{n:?}");
        }
    }

    /// The strip list keeps the hand-written entries it replaced (present id,
    /// present wait, swapchain maintenance) and adds the hidden extensions'.
    #[test]
    fn strip_list_covers_the_served_and_hidden_features() {
        let strip: Vec<i32> = stripped_feature_stypes().collect();
        for s in [
            crate::present::STYPE_PRESENT_ID_FEATURES,
            crate::present::STYPE_PRESENT_WAIT_FEATURES,
            crate::maint1::STYPE_FEATURES,
            crate::maint1::STYPE_PRESENT_ID2_FEATURES,
            crate::maint1::STYPE_PRESENT_WAIT2_FEATURES,
        ] {
            assert!(strip.contains(&s), "{s}");
        }
        // A hidden feature struct never shares an sType with a served one: the
        // zeroing would clobber the shim's yes.
        let served: Vec<i32> = VK_WSI_EXTENSIONS
            .iter()
            .filter(|e| is_served(e.0))
            .flat_map(|e| e.2.iter().map(|f| f.0))
            .collect();
        for (s, _) in hidden_features() {
            assert!(
                !served.contains(&s),
                "{s} is both a served and a hidden feature"
            );
        }
    }

    #[test]
    fn hidden_features_are_zeroed_and_others_kept() {
        #[repr(C)]
        struct Three {
            s_type: i32,
            p_next: *mut c_void,
            b: [u32; 3],
            tail: u32,
        }
        #[repr(C)]
        struct One {
            s_type: i32,
            p_next: *mut c_void,
            b: u32,
        }
        let mut id = One {
            s_type: crate::present::STYPE_PRESENT_ID_FEATURES,
            p_next: core::ptr::null_mut(),
            b: 1,
        };
        let mut timing = Three {
            s_type: 1_000_208_000,
            p_next: (&raw mut id).cast(),
            b: [1, 1, 1],
            tail: 7,
        };
        let mut wait2 = One {
            s_type: crate::maint1::STYPE_PRESENT_WAIT2_FEATURES,
            p_next: (&raw mut timing).cast(),
            b: 1,
        };
        unsafe { zero_hidden_features((&raw mut wait2).cast()) };
        assert_eq!(wait2.b, 0);
        assert_eq!(timing.b, [0, 0, 0]);
        assert_eq!(timing.tail, 7);
        assert_eq!(id.b, 1);
        unsafe { zero_hidden_features(core::ptr::null_mut()) };
    }
}
