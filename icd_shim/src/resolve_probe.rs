//! The resolve probe: the guest never sees a core version whose commands the
//! shim cannot resolve on that physical device.
//!
//! The shim used to forward the driver's apiVersion. On a Honor GT (SD 8 Gen 3,
//! MagicOS Android 16) Xwayland asks for instance 1.4.315 and Turnip reports
//! 1.4.359, yet every Vulkan 1.4 core device command resolves NULL: the OEM
//! loader hands Turnip an instance below 1.4, and Mesa gates a core device
//! command on the instance's `app_info.api_version`. Mesa zink trusts the
//! reported 1.4, installs an abort stub for every NULL core entry point and
//! dies on the first call ("ZINK: a Vulkan function was called without being
//! loaded").
//!
//! So, once per GPU and process, the shim creates a device of its own on the
//! guest's real instance, resolves the device commands of each core version
//! by exact name through the same gate the guest's lookups use
//! (`proxy::device_gate`, the real instance's `vkGetDeviceProcAddr` on the
//! real device), and destroys the device. The first version with a command
//! that does not resolve clamps the reported apiVersion to the one below it
//! and its `VkPhysicalDeviceVulkan1xFeatures` struct (and every higher one)
//! reads false. Only versions up to the apiVersion the guest's instance
//! forwarded are judged: Mesa answers NULL for a core name above it on every
//! device by design, and the guest asked for no more.
//!
//! The exact core name only: a promoted alias (`...KHR`/`...EXT`) resolves
//! only for an enabled extension, and a guest that trusts the core version
//! enables none of them.
//!
//! When everything resolves nothing changes. The probe is skipped (no device)
//! when it cannot clamp anything (instance or driver 1.0) and with
//! `GN_VKSHIM_NO_RESOLVE_PROBE=1`. See `docs/22-unresolvable-extensions.md`.

use crate::proxy;
use crate::vk::*;
use crate::vk_dispatch_gen::VK_CORE_DEVICE_COMMANDS;
use std::ffi::CString;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// The escape hatch: set to `1` to skip the probe (the driver's apiVersion
/// passes through as before).
pub const NO_PROBE_ENV: &str = "GN_VKSHIM_NO_RESOLVE_PROBE";

const MINOR_MASK: u32 = 0x3FF << 12;

fn minor(v: u32) -> u32 {
    (v >> 12) & 0x3FF
}

fn ver(v: u32) -> String {
    format!("{}.{}", (v >> 22) & 0x7F, minor(v))
}

/// What the probe decided for one physical device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// The highest core version whose device commands all resolve, as
    /// `VK_API_VERSION_1_x` (1.0 when even 1.1's do not), never above the
    /// driver's.
    pub core: u32,
    /// The first core command that did not resolve, when `core` is below the
    /// driver's version.
    pub first_missing: Option<&'static str>,
}

/// Which core version to report, given `raw`: does the guest's lookup find
/// this exact name, and `instance_api`: the apiVersion the guest's instance
/// handed the system loader (0 = 1.0). Versions above `instance_api` or the
/// driver's are not judged and never clamp.
pub fn evaluate(driver_api: u32, instance_api: u32, raw: &dyn Fn(&str) -> bool) -> Verdict {
    let base = driver_api & !MINOR_MASK & !0xFFF;
    for (v, cmds, _) in VK_CORE_DEVICE_COMMANDS {
        if minor(*v) > minor(driver_api) || minor(*v) > minor(instance_api) {
            break;
        }
        if let Some(c) = cmds.iter().find(|c| !raw(c)) {
            return Verdict {
                core: base | ((minor(*v) - 1) << 12),
                first_missing: Some(*c),
            };
        }
    }
    Verdict {
        core: driver_api & !0xFFF,
        first_missing: None,
    }
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

/// The feature structs `(sType, VkBool32 count)` to report false: the
/// `VkPhysicalDeviceVulkan1xFeatures` of every version above `core` up to the
/// driver's. Promoted extensions' structs stay: their extensions stay listed.
pub fn zeroed_features(core: u32, driver_api: u32) -> Vec<(i32, u32)> {
    VK_CORE_DEVICE_COMMANDS
        .iter()
        .filter(|(v, _, _)| minor(*v) > minor(core) && minor(*v) <= minor(driver_api))
        .flat_map(|(_, _, f)| f.iter().copied())
        .collect()
}

/// One physical device's probe result, as the hooks use it.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The apiVersion to report, when it is lower than the driver's.
    pub api: Option<u32>,
    /// Feature structs reported false.
    pub zero: Vec<(i32, u32)>,
    /// The first core command that did not resolve, when `api` is set.
    pub first_missing: Option<&'static str>,
}

impl Outcome {
    pub fn from_verdict(v: &Verdict, driver_api: u32) -> Self {
        let api = clamp_api(driver_api, v.core);
        let clamped = api != driver_api;
        Outcome {
            api: clamped.then_some(api),
            zero: if clamped {
                zeroed_features(v.core, driver_api)
            } else {
                Vec::new()
            },
            first_missing: clamped.then_some(v.first_missing).flatten(),
        }
    }

    /// Lower `api` (a reported apiVersion) to the probed one.
    pub fn clamp(&self, api: &mut u32) {
        if let Some(a) = self.api {
            if *api > a {
                *api = a;
            }
        }
    }

    /// Report the clamped versions' features false in a
    /// `VkPhysicalDeviceFeatures2` chain.
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

    /// The probe line's text after `resolve probe: `, without the time.
    pub fn describe(&self, driver_api: u32) -> String {
        match (self.api, self.first_missing) {
            (Some(a), Some(c)) => {
                format!(
                    "core version {} (driver {}; {c} unresolved)",
                    ver(a),
                    ver(driver_api)
                )
            }
            (Some(a), None) => format!("core version {} (driver {})", ver(a), ver(driver_api)),
            (None, _) => format!("core version {}", ver(driver_api)),
        }
    }
}

/// Why the probe cannot clamp anything, so no device is created: a 1.0
/// instance (Wine helpers) judges no version, a 1.0 driver has none to judge.
pub fn skip_reason(driver_api: u32, instance_api: u32) -> Option<&'static str> {
    if minor(instance_api) == 0 {
        Some("instance 1.0")
    } else if minor(driver_api) == 0 {
        Some("driver 1.0")
    } else {
        None
    }
}

// ------------------------------------------------------------- the probe

/// What a probe result is cached under: the driver's identity for the device
/// and the apiVersion the guest's instance asked for. A process creates
/// several instances, each with its own physical-device handles, and every
/// one of them would otherwise probe the same GPU again.
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
/// belongs to no instance the shim created; a skipped probe, or one that
/// could not run, caches an empty outcome (the driver's apiVersion).
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
        std::mem::transmute(proxy::gn_real_inst_proc(
            (*gi).real,
            c"vkGetPhysicalDeviceProperties",
        ))
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
    let driver_api = props.apiVersion;
    let instance_api = unsafe { (*gi).forwarded_api };
    let outcome = if let Some(why) = skip_reason(driver_api, instance_api) {
        crate::logi!("proxy pd: resolve probe: skipped ({why})");
        Outcome::default()
    } else {
        let t = Instant::now();
        let r = unsafe { probe(gi, pd, driver_api, instance_api) };
        let ms = t.elapsed().as_secs_f64() * 1e3;
        match r {
            Ok(v) => {
                let o = Outcome::from_verdict(&v, driver_api);
                crate::logi!("proxy pd: resolve probe: {} ({ms:.1} ms)",
                    o.describe(driver_api));
                o
            }
            Err(why) => {
                crate::logi!("proxy pd: resolve probe: did not run ({why}); core version {} kept ({ms:.1} ms)",
                    ver(driver_api));
                Outcome::default()
            }
        }
    };
    let o = Arc::new(outcome);
    if cache.len() < 16 {
        cache.push((key, o.clone()));
    }
    Some(o)
}

/// Create the probe device on the guest's instance, resolve every core
/// version's device commands through the guest's gate, destroy it.
unsafe fn probe(
    gi: *mut proxy::GnInstance,
    pd: VkPhysicalDevice,
    driver_api: u32,
    instance_api: u32,
) -> Result<Verdict, String> {
    let (Some(create), Some(gdp)) = (unsafe { (*gi).f.create_device }, unsafe { (*gi).f.gdp })
    else {
        return Err("no vkCreateDevice / vkGetDeviceProcAddr on the instance".into());
    };
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
    // No extensions: Mesa gates a core device command on the instance's
    // apiVersion alone, never on an enabled extension, so the exact core names
    // resolve (or not) the same on a bare device. A bare device is also the
    // cheapest to create and leaves the WSI-scoped names to `wsi_policy`.
    let ci = VkDeviceCreateInfo {
        sType: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        pNext: (&raw mut link).cast(),
        flags: 0,
        queueCreateInfoCount: 1,
        pQueueCreateInfos: &qci,
        enabledLayerCount: 0,
        ppEnabledLayerNames: ptr::null(),
        enabledExtensionCount: 0,
        ppEnabledExtensionNames: ptr::null(),
        pEnabledFeatures: ptr::null(),
    };
    let mut dev: VkDevice = ptr::null_mut();
    let res = unsafe { create(pd, &ci, ptr::null(), &mut dev) };
    if res != VK_SUCCESS || dev.is_null() {
        return Err(format!("probe vkCreateDevice returned {res}"));
    }
    let raw = |n: &str| -> bool {
        let Ok(c) = CString::new(n) else {
            return false;
        };
        // The shim's own handlers answer whatever the driver says.
        proxy::device_func(&c).is_some() || unsafe { proxy::device_gate(gdp, dev, &c) }.is_some()
    };
    let v = evaluate(driver_api, instance_api, &raw);
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

    const V1_0: u32 = 1 << 22;
    const V1_1: u32 = (1 << 22) | (1 << 12);
    const V1_3: u32 = (1 << 22) | (3 << 12);
    const V1_4: u32 = (1 << 22) | (4 << 12);
    /// sType of VkPhysicalDeviceVulkan13Features / Vulkan14Features.
    const VULKAN_1_3_FEATURES: i32 = 53;
    const VULKAN_1_4_FEATURES: i32 = 55;

    fn v14_cmds() -> &'static [&'static str] {
        VK_CORE_DEVICE_COMMANDS
            .iter()
            .find(|c| c.0 == V1_4)
            .unwrap()
            .1
    }

    /// Mesa on any device: a core name resolves iff the instance it was
    /// created on reached its version.
    fn mesa_at(instance_api: u32, n: &str) -> bool {
        match VK_CORE_DEVICE_COMMANDS.iter().find(|c| c.1.contains(&n)) {
            Some(c) => minor(c.0) <= minor(instance_api),
            None => true,
        }
    }

    /// The Honor GT: Xwayland forwards instance 1.4, the OEM loader hands
    /// Turnip less, every 1.4 core name is NULL.
    #[test]
    fn the_honor_case_clamps_to_1_3_and_names_the_command() {
        let v = evaluate(V1_4 | 359, V1_4 | 315, &|n: &str| mesa_at(V1_3, n));
        assert_eq!(v.core, V1_3);
        assert_eq!(v.first_missing, Some("vkCmdBindDescriptorSets2"));
        assert_eq!(v14_cmds()[0], "vkCmdBindDescriptorSets2");
        assert_eq!(skip_reason(V1_4 | 359, V1_4 | 315), None);
        let o = Outcome::from_verdict(&v, V1_4 | 359);
        assert_eq!(o.api, Some(V1_3 | 359));
        let mut api = V1_4 | 359;
        o.clamp(&mut api);
        assert_eq!(api, V1_3 | 359);
        assert_eq!(o.zero, vec![(VULKAN_1_4_FEATURES, 21)]);
        assert_eq!(
            o.describe(V1_4 | 359),
            "core version 1.3 (driver 1.4; vkCmdBindDescriptorSets2 unresolved)"
        );
    }

    /// The RP6 (Turnip 1.4): every process keeps the driver's 1.4, whatever
    /// its instance asked for.
    #[test]
    fn the_rp6_keeps_1_4() {
        // gamescope: instance 1.3, the 1.4 names NULL; 1.4 is not judged.
        let v = evaluate(V1_4 | 359, V1_3, &|n: &str| mesa_at(V1_3, n));
        assert_eq!((v.core, v.first_missing), (V1_4, None));
        // Xwayland: instance 1.4, everything resolves.
        let v = evaluate(V1_4 | 359, V1_4 | 315, &|n: &str| mesa_at(V1_4, n));
        assert_eq!((v.core, v.first_missing), (V1_4, None));
        for v in [
            evaluate(V1_4 | 359, V1_3, &|n: &str| mesa_at(V1_3, n)),
            evaluate(V1_4 | 359, V1_4, &|_| true),
        ] {
            let o = Outcome::from_verdict(&v, V1_4 | 359);
            assert_eq!(o.api, None);
            assert!(o.zero.is_empty());
            assert_eq!(o.describe(V1_4 | 359), "core version 1.4");
        }
    }

    /// Wine helpers: instance 1.0 (or no app info) creates no probe device.
    #[test]
    fn a_1_0_instance_or_driver_skips_the_probe() {
        assert_eq!(skip_reason(V1_4 | 359, 0), Some("instance 1.0"));
        assert_eq!(skip_reason(V1_4 | 359, V1_0), Some("instance 1.0"));
        assert_eq!(skip_reason(V1_0 | 7, V1_3), Some("driver 1.0"));
        assert_eq!(skip_reason(V1_4 | 359, V1_1), None);
        // Were it to run, nothing would be judged.
        let v = evaluate(V1_4 | 359, 0, &|_| false);
        assert_eq!((v.core, v.first_missing), (V1_4, None));
        let o = Outcome::default();
        let mut api = V1_4 | 359;
        o.clamp(&mut api);
        assert_eq!(api, V1_4 | 359);
    }

    #[test]
    fn nothing_changes_when_everything_resolves() {
        for api in [V1_3 | 280, V1_4 | 359, V1_1, (1 << 22) | (9 << 12) | 3] {
            let v = evaluate(api, api, &|_| true);
            assert_eq!(v.first_missing, None);
            assert_eq!(clamp_api(api, v.core), api);
            let o = Outcome::from_verdict(&v, api);
            assert_eq!(o.api, None);
            assert!(o.zero.is_empty());
        }
    }

    #[test]
    fn a_failing_core_version_clamps_to_the_one_below() {
        // 1.2's vkWaitSemaphores missing: 1.1, even with its KHR spelling.
        let raw = |n: &str| n != "vkWaitSemaphores";
        let v = evaluate(V1_4, V1_4, &raw);
        assert_eq!((v.core, v.first_missing), (V1_1, Some("vkWaitSemaphores")));
        // A 1.1 command missing: 1.0.
        let raw = |n: &str| !n.starts_with("vkGetDeviceQueue2");
        assert_eq!(evaluate(V1_3, V1_3, &raw).core, V1_0);
        // A 1.4 command missing on a 1.3 driver: never probed, no clamp.
        let raw = |n: &str| n != "vkCmdBindIndexBuffer2";
        assert_eq!(evaluate(V1_3 | 7, V1_4, &raw).core, V1_3);
    }

    #[test]
    fn clamp_api_keeps_variant_major_and_patch() {
        assert_eq!(clamp_api(V1_4 | 359, V1_3), V1_3 | 359);
        assert_eq!(clamp_api(V1_4 | 359, V1_0), V1_0 | 359);
        assert_eq!(clamp_api(V1_4 | 359, V1_4), V1_4 | 359);
        assert_eq!(clamp_api(V1_3 | 7, V1_4), V1_3 | 7);
        let variant = 1 << 29;
        assert_eq!(clamp_api(variant | V1_4 | 2, V1_3), variant | V1_3 | 2);
    }

    #[test]
    fn only_the_vulkan_1x_feature_structs_above_the_clamp_go() {
        assert_eq!(zeroed_features(V1_3, V1_4), vec![(VULKAN_1_4_FEATURES, 21)]);
        assert!(!zeroed_features(V1_3, V1_4)
            .iter()
            .any(|f| f.0 == VULKAN_1_3_FEATURES));
        assert!(zeroed_features(V1_4, V1_4).is_empty());
        let z: Vec<i32> = zeroed_features(V1_1, V1_4).iter().map(|f| f.0).collect();
        assert_eq!(z, [51, VULKAN_1_3_FEATURES, VULKAN_1_4_FEATURES]);
        let z: Vec<i32> = zeroed_features(V1_0, V1_3).iter().map(|f| f.0).collect();
        assert_eq!(z, [49, 51, VULKAN_1_3_FEATURES]);
    }
}
