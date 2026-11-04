//! The resolve probe: the guest never sees a device extension, or a core
//! version, whose commands the shim cannot resolve on that physical device.
//!
//! The shim used to forward the driver's device extension list and apiVersion.
//! When the guest later asked `vkGetDeviceProcAddr` for a command of an
//! extension it had enabled and the shim's resolver answered NULL, the guest
//! had no way back: Mesa zink installs an abort stub for every NULL entry point
//! of an enabled extension and dies on the first call ("ZINK: a Vulkan
//! function was called without being loaded"). On a Honor GT (SD 8 Gen 3,
//! MagicOS Android 16) every Vulkan 1.4 command and every promoted alias of one
//! resolves to NULL through the shim on Xwayland's device while Turnip lists
//! 1.4 and the extensions those commands came from.
//!
//! So, once per physical device, the shim creates a device of its own on the
//! guest's real instance with every driver-listed device extension enabled,
//! resolves every command of every listed extension and of every core version
//! through the same gate the guest's lookups use
//! (`proxy::device_gate`, the real instance's `vkGetDeviceProcAddr` on the real
//! device), and destroys the device. An extension with a command that does not
//! resolve is hidden: dropped from the enumerated list, masked at device
//! creation, its feature structs reported false. The reported apiVersion is
//! clamped to the highest core version whose commands all resolve.
//!
//! When everything resolves nothing changes: the list and the apiVersion are
//! the driver's. `GN_VKSHIM_NO_RESOLVE_PROBE=1` skips the probe. See
//! `docs/22-unresolvable-extensions.md`.

use crate::vk_dispatch_gen::{
    DeviceExtCommands, VK_ALIASES, VK_CORE_DEVICE_COMMANDS, VK_DEVICE_EXTENSION_COMMANDS,
};
use crate::proxy;
use crate::vk::*;
use std::ffi::{c_char, CStr, CString};
use std::ptr;
use std::sync::{Arc, Mutex};

/// The escape hatch: set to `1` to skip the probe (the driver's list and
/// apiVersion pass through as before).
pub const NO_PROBE_ENV: &str = "GN_VKSHIM_NO_RESOLVE_PROBE";

const MINOR_MASK: u32 = 0x3FF << 12;

fn minor(v: u32) -> u32 {
    (v >> 12) & 0x3FF
}

/// The registry row of a device extension.
pub fn device_ext(name: &str) -> Option<&'static DeviceExtCommands> {
    VK_DEVICE_EXTENSION_COMMANDS
        .binary_search_by(|e| e.0.cmp(name))
        .ok()
        .map(|i| &VK_DEVICE_EXTENSION_COMMANDS[i])
}

/// The other registry spellings of `name`: the alias the shim has always
/// tried first (`vk_dispatch_gen::alias_of`), then the rest of its alias group
/// (the core name and every extension spelling of it). The registry has no
/// alias chains, so one level is the whole group.
pub fn other_spellings(name: &str) -> impl Iterator<Item = &'static str> {
    let target = VK_ALIASES
        .binary_search_by(|(n, _)| n.cmp(&name))
        .ok()
        .map(|i| VK_ALIASES[i].1);
    let root = target.unwrap_or(name);
    let mut group: Vec<&'static str> = target.into_iter().collect();
    for (n, t) in VK_ALIASES {
        if *t == root && *n != name {
            group.push(n);
        }
    }
    // `alias_of` first, so the guest path tries what it has always tried.
    if let Some(f) = crate::vk_dispatch_gen::alias_of(name) {
        if let Some(i) = group.iter().position(|g| *g == f) {
            group.remove(i);
            group.insert(0, f);
        }
    }
    group.into_iter()
}

/// Whether `name` resolves, through itself or any other registry spelling.
pub fn resolves(name: &str, raw: &dyn Fn(&str) -> bool) -> bool {
    raw(name) || other_spellings(name).any(raw)
}

/// What the probe decided for one physical device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// The listed extensions with at least one command that does not resolve,
    /// in the order the driver listed them.
    pub hidden: Vec<&'static str>,
    /// The highest core version whose device commands all resolve, as
    /// `VK_API_VERSION_1_x` (1.0 when even 1.1's do not), never above the
    /// driver's.
    pub core: u32,
}

/// Which of `listed` (the driver's device extensions) to hide and which core
/// version to report, given `raw`: does the guest's lookup find this exact
/// name. WSI-scoped extensions are left to `crate::wsi_policy`, and an
/// extension with no device-level command is never hidden.
pub fn evaluate(listed: &[&str], driver_api: u32, raw: &dyn Fn(&str) -> bool) -> Verdict {
    let mut hidden = Vec::new();
    for n in listed {
        let Some(e) = device_ext(n) else {
            continue;
        };
        if is_wsi(e.0) || hidden.contains(&e.0) {
            continue;
        }
        if e.1.iter().any(|c| !resolves(c, raw)) {
            hidden.push(e.0);
        }
    }
    let base = driver_api & !MINOR_MASK & !0xFFF;
    let mut core = driver_api & !0xFFF;
    for (v, cmds, _) in VK_CORE_DEVICE_COMMANDS {
        if minor(*v) > minor(driver_api) {
            break;
        }
        if !cmds.iter().all(|c| resolves(c, raw)) {
            core = base | ((minor(*v) - 1) << 12);
            break;
        }
    }
    Verdict { hidden, core }
}

fn is_wsi(name: &str) -> bool {
    crate::vk_dispatch_gen::VK_WSI_EXTENSIONS
        .binary_search_by(|e| e.0.cmp(name))
        .is_ok()
}

/// The apiVersion to report: the driver's, its minor lowered to `core`'s when
/// the probe stopped below it. Variant, major and patch are the driver's.
pub fn clamp_api(driver_api: u32, core: u32) -> u32 {
    if minor(core) >= minor(driver_api) {
        driver_api
    } else {
        (driver_api & !MINOR_MASK) | (minor(core) << 12)
    }
}

/// The feature structs `(sType, VkBool32 count)` to report false: those of the
/// hidden extensions and of the core versions above `core`, unless an
/// extension still listed or a core version still reported names the same
/// struct (a promoted extension's struct is its core version's too).
pub fn zeroed_features(listed: &[&str], v: &Verdict, driver_api: u32) -> Vec<(i32, u32)> {
    let mut cand: Vec<(i32, u32)> = Vec::new();
    let mut keep: Vec<i32> = Vec::new();
    for n in listed {
        if let Some(e) = device_ext(n) {
            if v.hidden.contains(&e.0) {
                cand.extend_from_slice(e.2);
            } else {
                keep.extend(e.2.iter().map(|f| f.0));
            }
        }
    }
    for (ver, _, feats) in VK_CORE_DEVICE_COMMANDS {
        if minor(*ver) <= minor(v.core) {
            keep.extend(feats.iter().map(|f| f.0));
        } else if minor(*ver) <= minor(driver_api) {
            cand.extend_from_slice(feats);
        }
    }
    let mut out: Vec<(i32, u32)> = Vec::new();
    for f in cand {
        if !keep.contains(&f.0) && !out.iter().any(|o| o.0 == f.0) {
            out.push(f);
        }
    }
    out
}

/// One physical device's probe result, as the hooks use it.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Extensions hidden from the guest.
    pub hidden: Vec<&'static str>,
    /// The apiVersion to report, when it is lower than the driver's.
    pub api: Option<u32>,
    /// Feature structs reported false.
    pub zero: Vec<(i32, u32)>,
}

impl Outcome {
    pub fn from_verdict(listed: &[&str], v: &Verdict, driver_api: u32) -> Self {
        let api = clamp_api(driver_api, v.core);
        Outcome {
            hidden: v.hidden.clone(),
            api: (api != driver_api).then_some(api),
            zero: zeroed_features(listed, v, driver_api),
        }
    }

    pub fn hides(&self, name: &CStr) -> bool {
        name.to_str().is_ok_and(|n| self.hidden.contains(&n))
    }

    /// Lower `api` (a reported apiVersion) to the probed one.
    pub fn clamp(&self, api: &mut u32) {
        if let Some(a) = self.api {
            if *api > a {
                *api = a;
            }
        }
    }

    /// Report the hidden features false in a `VkPhysicalDeviceFeatures2`
    /// chain.
    ///
    /// # Safety
    /// `p_next` must be the chain head of a `VkPhysicalDeviceFeatures2` the
    /// shim is about to return to the guest.
    pub unsafe fn zero_features(&self, p_next: *mut core::ffi::c_void) {
        if !self.zero.is_empty() {
            let bools = |s: i32| self.zero.iter().find(|f| f.0 == s).map(|f| f.1);
            unsafe { crate::wsi_policy::zero_features_in(p_next, &bools) };
        }
    }

    /// The probe line's text after `proxy pd: `.
    pub fn describe(&self, probed: usize, driver_api: u32) -> String {
        let ver = |v: u32| format!("{}.{}", (v >> 22) & 0x7F, minor(v));
        if self.hidden.is_empty() && self.api.is_none() {
            return format!(
                "all {probed} listed device extensions and core version {} resolve",
                ver(driver_api)
            );
        }
        format!(
            "hiding {} extensions whose commands do not resolve: [{}]; core version {} (driver {})",
            self.hidden.len(),
            self.hidden.join(", "),
            ver(self.api.unwrap_or(driver_api)),
            ver(driver_api)
        )
    }
}

// ------------------------------------------------------------- the probe

/// What a probe result is cached under: the driver's identity for the device
/// and the apiVersion the guest's instance asked for (the system loader's
/// device-level answers can depend on it). A process creates several
/// instances, each with its own physical-device handles, and every one of
/// them would otherwise probe the same GPU again.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    vendor: u32,
    device: u32,
    driver: u32,
    uuid: [u8; 16],
    instance_api: u32,
}

static CACHE: Mutex<Vec<(Key, Arc<Outcome>)>> = Mutex::new(Vec::new());

/// The probe result for `pd`, probing on the first call for its GPU. `None`
/// when the probe is switched off (`GN_VKSHIM_NO_RESOLVE_PROBE=1`) or `pd`
/// belongs to no instance the shim created; a probe that could not run caches
/// an empty outcome (nothing hidden).
///
/// # Safety
/// `pd` must be a physical device of an instance the proxy created.
pub unsafe fn for_pd(pd: VkPhysicalDevice) -> Option<Arc<Outcome>> {
    if std::env::var_os(NO_PROBE_ENV).is_some_and(|v| v == "1") {
        return None;
    }
    let gi = unsafe { proxy::find_instance_by_pd(pd) };
    if gi.is_null() {
        return None;
    }
    let get_props: Option<PFN_vkGetPhysicalDeviceProperties> = unsafe {
        std::mem::transmute(proxy::gn_real_inst_proc((*gi).real, c"vkGetPhysicalDeviceProperties"))
    };
    let get_props = get_props?;
    let mut props: VkPhysicalDeviceProperties = unsafe { std::mem::zeroed() };
    unsafe { get_props(pd, &mut props) };
    let key = Key {
        vendor: props.vendorID,
        device: props.deviceID,
        driver: props.driverVersion,
        uuid: props.pipelineCacheUUID,
        instance_api: unsafe { (*gi).api_version },
    };
    // Held across the probe: a second thread asking for the same GPU waits for
    // the first answer instead of creating a second probe device. The probe
    // calls only the driver, never back into the shim's hooks.
    let mut cache = CACHE.lock().ok()?;
    if let Some((_, o)) = cache.iter().find(|(k, _)| *k == key) {
        return Some(o.clone());
    }
    let listed = unsafe { proxy::driver_device_exts(gi, pd) };
    let names: Vec<&str> = listed.iter().filter_map(|n| n.to_str().ok()).collect();
    let outcome = match unsafe { probe(gi, pd, &listed, props.apiVersion) } {
        Ok(v) => {
            let o = Outcome::from_verdict(&names, &v, props.apiVersion);
            crate::logi!(gndrm_obf::obf!(
                "proxy pd: {}",
                o.describe(probe_count(&names), props.apiVersion)
            ));
            o
        }
        Err(why) => {
            crate::logi!(gndrm_obf::obf!(
                "proxy pd: resolve probe did not run ({why}); every listed extension is kept"
            ));
            Outcome::default()
        }
    };
    let o = Arc::new(outcome);
    if cache.len() < 16 {
        cache.push((key, o.clone()));
    }
    Some(o)
}

/// The listed extensions the probe judges (registry device extensions outside
/// the WSI scope).
fn probe_count(listed: &[&str]) -> usize {
    listed
        .iter()
        .filter(|n| device_ext(n).is_some() && !is_wsi(n))
        .count()
}

/// Create the probe device on the guest's instance, resolve every command of
/// every listed extension and of every core version through the guest's gate,
/// destroy it.
unsafe fn probe(
    gi: *mut proxy::GnInstance,
    pd: VkPhysicalDevice,
    listed: &[CString],
    driver_api: u32,
) -> Result<Verdict, String> {
    let (Some(create), Some(gdp)) = (unsafe { (*gi).f.create_device }, unsafe { (*gi).f.gdp })
    else {
        return Err("no vkCreateDevice / vkGetDeviceProcAddr on the instance".into());
    };
    // Every driver-listed extension the guest could enable. The WSI-scoped
    // ones are the WSI policy's (`crate::wsi_policy`), served by the shim or
    // masked, and the system loader intercepts the swapchain itself.
    let enable: Vec<*const c_char> = listed
        .iter()
        .filter(|n| n.to_str().map_or(true, |s| !is_wsi(s)))
        .map(|n| n.as_ptr())
        .collect();
    let prio = 1.0f32;
    let qci = VkDeviceQueueCreateInfo {
        sType: VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        pNext: ptr::null(),
        flags: 0,
        queueFamilyIndex: 0,
        queueCount: 1,
        pQueuePriorities: &prio,
    };
    // The same chain link the guest's creates carry (`gn_vkCreateDevice`), so
    // the hooks build this device's dispatch the way they build the guest's.
    let mut link = VkLayerDeviceCreateInfo {
        sType: VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO,
        pNext: ptr::null_mut(),
        function: VK_LAYER_LINK_INFO,
        u: VkLayerFunction_u {
            pLayerDeviceInfo: proxy::chain_dev_link(),
        },
    };
    let ci = VkDeviceCreateInfo {
        sType: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        pNext: (&raw mut link).cast(),
        flags: 0,
        queueCreateInfoCount: 1,
        pQueueCreateInfos: &qci,
        enabledLayerCount: 0,
        ppEnabledLayerNames: ptr::null(),
        enabledExtensionCount: enable.len() as u32,
        ppEnabledExtensionNames: enable.as_ptr(),
        pEnabledFeatures: ptr::null(),
    };
    let mut dev: VkDevice = ptr::null_mut();
    let res = unsafe { create(pd, &ci, ptr::null(), &mut dev) };
    if res != VK_SUCCESS || dev.is_null() {
        return Err(format!(
            "probe vkCreateDevice with {} extensions returned {res}",
            enable.len()
        ));
    }
    let raw = |n: &str| -> bool {
        let Ok(c) = CString::new(n) else {
            return false;
        };
        // The shim's own handlers answer whatever the driver says.
        proxy::device_func(&c).is_some() || unsafe { proxy::device_gate(gdp, dev, &c) }.is_some()
    };
    let names: Vec<&str> = listed.iter().filter_map(|n| n.to_str().ok()).collect();
    let v = evaluate(&names, driver_api, &raw);
    let destroy: Option<PFN_vkDestroyDevice> =
        unsafe { std::mem::transmute(proxy::device_gate(gdp, dev, c"vkDestroyDevice")) };
    if let Some(destroy) = destroy {
        unsafe { destroy(dev, ptr::null()) };
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_3: u32 = (1 << 22) | (3 << 12);
    const V1_4: u32 = (1 << 22) | (4 << 12);

    /// The Honor GT's Xwayland device: every 1.4 core command and every
    /// promoted alias of one is NULL.
    fn honor_raw(n: &str) -> bool {
        let v14: &[&str] = VK_CORE_DEVICE_COMMANDS
            .iter()
            .find(|c| c.0 == V1_4)
            .unwrap()
            .1;
        !(v14.contains(&n) || crate::vk_dispatch_gen::alias_of(n).is_some_and(|a| v14.contains(&a)))
    }

    const HONOR_LISTED: &[&str] = &[
        "VK_KHR_swapchain",
        "VK_KHR_maintenance4",
        "VK_KHR_maintenance5",
        "VK_KHR_maintenance6",
        "VK_KHR_push_descriptor",
        "VK_KHR_dynamic_rendering_local_read",
        "VK_EXT_host_image_copy",
        "VK_KHR_line_rasterization",
        "VK_EXT_line_rasterization",
        "VK_KHR_map_memory2",
        "VK_KHR_dynamic_rendering",
        "VK_EXT_robustness2",
        "VK_KHR_present_wait",
    ];

    #[test]
    fn the_honor_case_hides_the_1_4_extensions_and_clamps_to_1_3() {
        let v = evaluate(HONOR_LISTED, V1_4 | 359, &honor_raw);
        assert_eq!(
            v.hidden,
            vec![
                "VK_KHR_maintenance5",
                "VK_KHR_maintenance6",
                "VK_KHR_push_descriptor",
                "VK_KHR_dynamic_rendering_local_read",
                "VK_EXT_host_image_copy",
                "VK_KHR_line_rasterization",
                "VK_EXT_line_rasterization",
                "VK_KHR_map_memory2",
            ]
        );
        assert_eq!(v.core, V1_3);
        assert_eq!(clamp_api(V1_4 | 359, v.core), V1_3 | 359);
        let o = Outcome::from_verdict(HONOR_LISTED, &v, V1_4 | 359);
        assert_eq!(o.api, Some(V1_3 | 359));
        assert!(o.hides(c"VK_KHR_maintenance5"));
        assert!(!o.hides(c"VK_KHR_maintenance4"));
        let mut api = V1_4 | 359;
        o.clamp(&mut api);
        assert_eq!(api, V1_3 | 359);
        assert!(o.describe(13, V1_4 | 359).starts_with(
            "hiding 8 extensions whose commands do not resolve: [VK_KHR_maintenance5, "
        ));
        assert!(o.describe(13, V1_4 | 359).ends_with("; core version 1.3 (driver 1.4)"));
    }

    /// The rule's invariant: a device where everything resolves sees exactly
    /// the driver's list, apiVersion and features.
    #[test]
    fn nothing_changes_when_everything_resolves() {
        for api in [V1_3 | 280, V1_4 | 359, (1 << 22) | (1 << 12), (1 << 22) | (9 << 12) | 3] {
            let v = evaluate(HONOR_LISTED, api, &|_| true);
            assert!(v.hidden.is_empty());
            assert_eq!(clamp_api(api, v.core), api);
            let o = Outcome::from_verdict(HONOR_LISTED, &v, api);
            assert_eq!(o.api, None);
            assert!(o.zero.is_empty());
            let mut a = api;
            o.clamp(&mut a);
            assert_eq!(a, api);
            let kept: Vec<&str> = HONOR_LISTED
                .iter()
                .copied()
                .filter(|n| !o.hides(&std::ffi::CString::new(*n).unwrap()))
                .collect();
            assert_eq!(kept, HONOR_LISTED);
            assert!(o.describe(13, api).starts_with("all 13 listed"));
        }
    }

    #[test]
    fn a_failing_core_version_clamps_to_the_one_below() {
        // 1.2's vkWaitSemaphores and its KHR spelling both missing: 1.1.
        let raw = |n: &str| n != "vkWaitSemaphores" && n != "vkWaitSemaphoresKHR";
        assert_eq!(evaluate(&[], V1_4, &raw).core, (1 << 22) | (1 << 12));
        // A 1.1 command missing: 1.0.
        let raw = |n: &str| !n.starts_with("vkGetDeviceQueue2");
        assert_eq!(evaluate(&[], V1_3, &raw).core, 1 << 22);
        // A 1.4 command missing on a 1.3 driver: never probed, no clamp.
        let raw = |n: &str| n != "vkCmdBindIndexBuffer2" && n != "vkCmdBindIndexBuffer2KHR";
        assert_eq!(evaluate(&[], V1_3 | 7, &raw).core, V1_3);
    }

    #[test]
    fn a_core_command_resolves_through_any_spelling() {
        // Only the EXT spelling of vkCmdSetLineStipple exists: 1.4 holds, and
        // both line-rasterization extensions resolve.
        let raw = |n: &str| !n.starts_with("vkCmdSetLineStipple") || n == "vkCmdSetLineStippleEXT";
        let listed = ["VK_KHR_line_rasterization", "VK_EXT_line_rasterization"];
        let v = evaluate(&listed, V1_4, &raw);
        assert!(v.hidden.is_empty());
        assert_eq!(v.core, V1_4);
        let mut s: Vec<&str> = other_spellings("vkCmdSetLineStipple").collect();
        s.sort_unstable();
        assert_eq!(s, ["vkCmdSetLineStippleEXT", "vkCmdSetLineStippleKHR"]);
        let mut s: Vec<&str> = other_spellings("vkCmdSetLineStippleKHR").collect();
        assert_eq!(s.remove(0), "vkCmdSetLineStipple");
        assert_eq!(s, ["vkCmdSetLineStippleEXT"]);
        assert_eq!(other_spellings("vkCreateDevice").count(), 0);
    }

    #[test]
    fn wsi_and_command_less_extensions_are_never_hidden() {
        let v = evaluate(
            &["VK_KHR_swapchain", "VK_KHR_present_wait", "VK_EXT_robustness2", "VK_GOOGLE_unknown"],
            V1_3,
            &|_| false,
        );
        assert!(v.hidden.is_empty());
        assert_eq!(v.core, 1 << 22);
    }

    #[test]
    fn hidden_features_are_zeroed_unless_a_visible_owner_names_them() {
        let v = evaluate(HONOR_LISTED, V1_4, &honor_raw);
        let z = zeroed_features(HONOR_LISTED, &v, V1_4);
        let st = |n: &str| device_ext(n).unwrap().2[0].0;
        // maintenance5 and host image copy: hidden and 1.4's, gone.
        assert!(z.iter().any(|f| f.0 == st("VK_KHR_maintenance5")));
        assert!(z.iter().any(|f| f.0 == st("VK_EXT_host_image_copy")));
        // The Vulkan14Features struct (sType 55) goes with the version.
        assert!(z.iter().any(|f| f.0 == 55));
        // 1.3 structs stay: maintenance4 is listed and 1.3 is still reported.
        assert!(!z.iter().any(|f| f.0 == st("VK_KHR_maintenance4")));
        assert!(!z.iter().any(|f| f.0 == 53));
        // A struct shared by a hidden and a visible extension stays.
        let only_khr_hidden = Verdict { hidden: vec!["VK_KHR_line_rasterization"], core: V1_4 };
        let z = zeroed_features(
            &["VK_KHR_line_rasterization", "VK_EXT_line_rasterization"],
            &only_khr_hidden,
            V1_4,
        );
        assert!(z.is_empty());
    }
}
