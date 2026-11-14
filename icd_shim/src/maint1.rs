//! `VK_KHR_swapchain_maintenance1` (and `VK_EXT_swapchain_maintenance1`, the
//! extension it was promoted from) served by the shim.
//!
//! The guest's swapchain is the shim's own, so nothing the guest chains onto a
//! present, a swapchain creation or a surface query ever reaches the driver.
//! Before this module existed the EXT name was masked and the KHR name leaked
//! through from the driver's list: a guest that found the KHR name (vkd3d-proton
//! 3.0.1 and later, DXVK after 2.7.1) chained a present fence the shim never
//! signalled, then waited on it forever when its fence ring wrapped. DX12 titles
//! stopped after their first frames under gamescope while X11, whose Mesa WSI
//! owns the swapchain and signals the fence itself, played on.
//!
//! What is served, and how:
//! - **Present fences** (`VkSwapchainPresentFenceInfo`): the fence must signal
//!   once the present's wait semaphores may be reused. The shim already consumes
//!   those semaphores in a submit of its own per presented slot; the guest's
//!   fence is signalled by a second submit chained behind it (`proxy.rs`,
//!   `gn_vkQueuePresentKHR`).
//! - **Surface capabilities**: the scaling capabilities say no scaling (the
//!   swapchain is presented at its own extent and gamescope scales the layer),
//!   and the compatibility list names only the mode the query asked about.
//! - **Creation**: the present-modes list and the scaling struct are accepted;
//!   the deferred-allocation flag is ignored, which the extension allows.
//! - **`vkReleaseSwapchainImagesKHR`**: acquired, unpresented slots go back to
//!   the free list.
//!
//! TODO(maintenance1 follow-up): per-present mode switching. A guest may chain
//! `VkSwapchainPresentModeInfo` to switch between the modes it listed at
//! creation; today the compatibility answer lists one mode, so a conforming
//! guest never asks, and a request for another mode is logged once and ignored.
//! Serving it means feeding the requested mode into `guest_present_mode` /
//! `live_present_mode` and the cap gate's FIFO-versus-mailbox rule per present
//! (docs/15), which is the pacing-sensitive part and gets its own change. The
//! same follow-up may take `VK_KHR_present_id2` / `VK_KHR_present_wait2`
//! (today masked: both fall back to the v1 wait the shim serves) and the
//! `VkSurfacePresentScalingCapabilities` stretch modes.

use crate::vk::{VkExtent2D, VkFence, VkPresentModeKHR, VkSwapchainKHR, VK_PRESENT_MODE_FIFO_KHR};
use core::ffi::c_void;

/// The device extension names the guest sees; served here, not by the driver.
pub const EXT_SWAPCHAIN_MAINTENANCE1_KHR: &core::ffi::CStr = c"VK_KHR_swapchain_maintenance1";
pub const EXT_SWAPCHAIN_MAINTENANCE1_EXT: &core::ffi::CStr = c"VK_EXT_swapchain_maintenance1";
/// The instance extensions the device ones depend on: the surface-capability
/// structs below. Served by the shim's own surface queries.
pub const EXT_SURFACE_MAINTENANCE1_KHR: &core::ffi::CStr = c"VK_KHR_surface_maintenance1";
pub const EXT_SURFACE_MAINTENANCE1_EXT: &core::ffi::CStr = c"VK_EXT_surface_maintenance1";

// `VK_EXT_surface_maintenance1` is extension 275; the KHR aliases share values.
/// `VkSurfacePresentModeKHR`: chained into `VkPhysicalDeviceSurfaceInfo2KHR`.
pub const STYPE_SURFACE_PRESENT_MODE: i32 = 1_000_274_000;
/// `VkSurfacePresentScalingCapabilitiesKHR`: chained into `VkSurfaceCapabilities2KHR`.
pub const STYPE_SURFACE_PRESENT_SCALING_CAPABILITIES: i32 = 1_000_274_001;
/// `VkSurfacePresentModeCompatibilityKHR`: chained into `VkSurfaceCapabilities2KHR`.
pub const STYPE_SURFACE_PRESENT_MODE_COMPATIBILITY: i32 = 1_000_274_002;

// `VK_EXT_swapchain_maintenance1` is extension 276; the KHR aliases share values.
/// `VkPhysicalDeviceSwapchainMaintenance1FeaturesKHR`.
pub const STYPE_FEATURES: i32 = 1_000_275_000;
/// `VkSwapchainPresentFenceInfoKHR`: chained into `VkPresentInfoKHR`.
pub const STYPE_PRESENT_FENCE_INFO: i32 = 1_000_275_001;
/// `VkSwapchainPresentModesCreateInfoKHR`: chained into `VkSwapchainCreateInfoKHR`.
pub const STYPE_PRESENT_MODES_CREATE_INFO: i32 = 1_000_275_002;
/// `VkSwapchainPresentModeInfoKHR`: chained into `VkPresentInfoKHR`.
pub const STYPE_PRESENT_MODE_INFO: i32 = 1_000_275_003;
/// `VkSwapchainPresentScalingCreateInfoKHR`: chained into `VkSwapchainCreateInfoKHR`.
pub const STYPE_PRESENT_SCALING_CREATE_INFO: i32 = 1_000_275_004;
/// `VkReleaseSwapchainImagesInfoKHR`.
pub const STYPE_RELEASE_IMAGES_INFO: i32 = 1_000_275_005;
/// `VK_SWAPCHAIN_CREATE_DEFERRED_MEMORY_ALLOCATION_BIT_KHR`: a hint the
/// implementation may ignore. The shim allocates every slot at creation.
pub const CREATE_DEFERRED_MEMORY_ALLOCATION_BIT: u32 = 0x8;

/// `VkPhysicalDevicePresentId2FeaturesKHR` (`VK_KHR_present_id2`, extension 480, offset 2).
pub const STYPE_PRESENT_ID2_FEATURES: i32 = 1_000_479_002;
/// `VkPhysicalDevicePresentWait2FeaturesKHR` (`VK_KHR_present_wait2`, extension 481, offset 1).
pub const STYPE_PRESENT_WAIT2_FEATURES: i32 = 1_000_480_001;

/// The two leading members every Vulkan structure shares.
#[repr(C)]
struct Head {
    s_type: i32,
    p_next: *mut c_void,
}

/// `VkSwapchainPresentFenceInfoKHR` / `VkSwapchainPresentModeInfoKHR` /
/// `VkSwapchainPresentModesCreateInfoKHR` / `VkSurfacePresentModeCompatibilityKHR`:
/// a count and a pointer, the element type differing.
#[repr(C)]
struct CountAndArray {
    s_type: i32,
    p_next: *mut c_void,
    count: u32,
    array: *mut c_void,
}

/// `VkSurfacePresentModeKHR`.
#[repr(C)]
struct SurfacePresentMode {
    s_type: i32,
    p_next: *mut c_void,
    present_mode: VkPresentModeKHR,
}

/// `VkSurfacePresentScalingCapabilitiesKHR`.
#[repr(C)]
struct ScalingCapabilities {
    s_type: i32,
    p_next: *mut c_void,
    supported_present_scaling: u32,
    supported_present_gravity_x: u32,
    supported_present_gravity_y: u32,
    min_scaled_image_extent: VkExtent2D,
    max_scaled_image_extent: VkExtent2D,
}

/// `VkSwapchainPresentScalingCreateInfoKHR`.
#[repr(C)]
struct ScalingCreateInfo {
    s_type: i32,
    p_next: *mut c_void,
    scaling_behavior: u32,
    present_gravity_x: u32,
    present_gravity_y: u32,
}

/// `VkReleaseSwapchainImagesInfoKHR`.
#[repr(C)]
struct ReleaseImagesInfo {
    s_type: i32,
    p_next: *mut c_void,
    swapchain: VkSwapchainKHR,
    image_index_count: u32,
    image_indices: *const u32,
}

/// A feature struct of one `VkBool32`.
#[repr(C)]
struct OneFeature {
    s_type: i32,
    p_next: *mut c_void,
    feature: u32,
}

/// The first structure of type `s_type` in a chain, or null. Bounded: a
/// malformed chain must not walk forever.
///
/// # Safety
/// `p_next` must be null or the `pNext` of a live Vulkan structure chain.
unsafe fn find(p_next: *const c_void, s_type: i32) -> *mut Head {
    let mut head = p_next.cast_mut().cast::<Head>();
    for _ in 0..64 {
        if head.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: `head` is a structure in the chain; only its head is read
        // before its type is known.
        let node = unsafe { &*head };
        if node.s_type == s_type {
            return head;
        }
        head = node.p_next.cast::<Head>();
    }
    core::ptr::null_mut()
}

/// Element `index` of a count-and-array structure of type `s_type`, or `None`
/// when the chain has none or the index is past its count.
///
/// # Safety
/// `p_next` must be null or a live chain, and the structure of that type must
/// have the count-and-array layout with `T` elements.
unsafe fn array_entry<T: Copy>(p_next: *const c_void, s_type: i32, index: usize) -> Option<T> {
    let node = unsafe { find(p_next, s_type) };
    if node.is_null() {
        return None;
    }
    // SAFETY: the structure declares this type, so it has this layout.
    let s = unsafe { &*node.cast::<CountAndArray>() };
    if index as u32 >= s.count || s.array.is_null() {
        return None;
    }
    // SAFETY: the count bounds the array the guest supplied.
    Some(unsafe { *s.array.cast::<T>().add(index) })
}

/// The fence the guest chained for swapchain `index` of a present
/// (`VkSwapchainPresentFenceInfoKHR`), or 0 when none. A null handle in the
/// array means "no fence for this swapchain" and reads as 0 too.
///
/// # Safety
/// `p_next` must be the `pNext` of a live `VkPresentInfoKHR`.
pub(crate) unsafe fn present_fence(p_next: *const c_void, index: usize) -> VkFence {
    unsafe { array_entry::<VkFence>(p_next, STYPE_PRESENT_FENCE_INFO, index) }.unwrap_or(0)
}

/// The present mode the guest asked for swapchain `index` of a present
/// (`VkSwapchainPresentModeInfoKHR`), or `None` when it chained none.
///
/// # Safety
/// `p_next` must be the `pNext` of a live `VkPresentInfoKHR`.
pub(crate) unsafe fn present_mode_request(
    p_next: *const c_void,
    index: usize,
) -> Option<VkPresentModeKHR> {
    unsafe { array_entry::<VkPresentModeKHR>(p_next, STYPE_PRESENT_MODE_INFO, index) }
}

/// The present modes a swapchain was created to switch between
/// (`VkSwapchainPresentModesCreateInfoKHR`), or `None` when none were chained.
///
/// # Safety
/// `p_next` must be the `pNext` of a live `VkSwapchainCreateInfoKHR`.
pub(crate) unsafe fn create_modes(p_next: *const c_void) -> Option<Vec<VkPresentModeKHR>> {
    let node = unsafe { find(p_next, STYPE_PRESENT_MODES_CREATE_INFO) };
    if node.is_null() {
        return None;
    }
    // SAFETY: the structure declares this type, so it has this layout.
    let s = unsafe { &*node.cast::<CountAndArray>() };
    if s.array.is_null() {
        return Some(Vec::new());
    }
    let n = (s.count as usize).min(16);
    // SAFETY: the count bounds the array the guest supplied.
    Some(unsafe { core::slice::from_raw_parts(s.array.cast::<VkPresentModeKHR>(), n) }.to_vec())
}

/// The scaling behaviour a swapchain was created with
/// (`VkSwapchainPresentScalingCreateInfoKHR`) as `(scaling, gravity_x,
/// gravity_y)`, or `None` when none was chained. Zero means "the
/// implementation's default", which is the only behaviour served.
///
/// # Safety
/// `p_next` must be the `pNext` of a live `VkSwapchainCreateInfoKHR`.
pub(crate) unsafe fn create_scaling(p_next: *const c_void) -> Option<(u32, u32, u32)> {
    let node = unsafe { find(p_next, STYPE_PRESENT_SCALING_CREATE_INFO) };
    if node.is_null() {
        return None;
    }
    // SAFETY: the structure declares this type, so it has this layout.
    let s = unsafe { &*node.cast::<ScalingCreateInfo>() };
    Some((s.scaling_behavior, s.present_gravity_x, s.present_gravity_y))
}

/// `VkReleaseSwapchainImagesInfoKHR`: the swapchain and the image indices.
///
/// # Safety
/// `info` must point at a live `VkReleaseSwapchainImagesInfoKHR`.
pub(crate) unsafe fn release_info(info: *const c_void) -> (VkSwapchainKHR, Vec<u32>) {
    if info.is_null() {
        return (0, Vec::new());
    }
    // SAFETY: the caller passes the structure the entry point received.
    let s = unsafe { &*info.cast::<ReleaseImagesInfo>() };
    if s.s_type != STYPE_RELEASE_IMAGES_INFO || s.image_indices.is_null() {
        return (s.swapchain, Vec::new());
    }
    let n = (s.image_index_count as usize).min(64);
    // SAFETY: the count bounds the array the guest supplied.
    (
        s.swapchain,
        unsafe { core::slice::from_raw_parts(s.image_indices, n) }.to_vec(),
    )
}

/// Patch the feature structs a guest chains into `vkGetPhysicalDeviceFeatures2`
/// for the extensions this module decides: swapchain maintenance is served
/// (whatever the driver said about a swapchain it does not own), present id 2
/// and present wait 2 are not (the driver's yes would send the guest to entry
/// points the shim refuses).
///
/// # Safety
/// `p_next` must be the chain head of a `VkPhysicalDeviceFeatures2` the shim is
/// about to return to the guest.
pub(crate) unsafe fn features2(p_next: *mut c_void) {
    let mut head = p_next.cast::<Head>();
    for _ in 0..64 {
        if head.is_null() {
            return;
        }
        // SAFETY: `head` is a structure in the guest's chain.
        let node = unsafe { &*head };
        match node.s_type {
            // SAFETY: the structure declares this type, so it has this layout.
            STYPE_FEATURES => unsafe { (*head.cast::<OneFeature>()).feature = 1 },
            STYPE_PRESENT_ID2_FEATURES | STYPE_PRESENT_WAIT2_FEATURES => unsafe {
                (*head.cast::<OneFeature>()).feature = 0
            },
            _ => {}
        }
        head = node.p_next.cast::<Head>();
    }
}

/// Fill the surface-maintenance structs a guest chains into
/// `vkGetPhysicalDeviceSurfaceCapabilities2KHR`.
///
/// Scaling: none. The guest's swapchain is presented at its own extent and
/// gamescope scales the layer to the output, so the swapchain never has to be
/// scaled by the presentation engine; the scaled extents repeat the surface's
/// image extents, as the spec asks when no scaling is supported.
///
/// Compatibility: the mode the query names (`VkSurfacePresentModeKHR` in the
/// surface info), alone. Every mode the shim offers is a mode it can create a
/// swapchain in, but a mode switch after creation is not served yet (see the
/// module's TODO), and a guest that is told only one mode is compatible never
/// asks for one. Without the input struct, FIFO.
///
/// # Safety
/// `info_p_next` must be null or the `pNext` of a live
/// `VkPhysicalDeviceSurfaceInfo2KHR`; `caps_p_next` null or the `pNext` of the
/// `VkSurfaceCapabilities2KHR` being returned.
pub(crate) unsafe fn surface_caps2(
    info_p_next: *const c_void,
    caps_p_next: *mut c_void,
    min_image_extent: VkExtent2D,
    max_image_extent: VkExtent2D,
) {
    let asked = unsafe { find(info_p_next, STYPE_SURFACE_PRESENT_MODE) };
    let mode = if asked.is_null() {
        VK_PRESENT_MODE_FIFO_KHR
    } else {
        // SAFETY: the structure declares this type, so it has this layout.
        unsafe { (*asked.cast::<SurfacePresentMode>()).present_mode }
    };
    let scaling = unsafe { find(caps_p_next, STYPE_SURFACE_PRESENT_SCALING_CAPABILITIES) };
    if !scaling.is_null() {
        // SAFETY: the structure declares this type, so it has this layout.
        let s = unsafe { &mut *scaling.cast::<ScalingCapabilities>() };
        s.supported_present_scaling = 0;
        s.supported_present_gravity_x = 0;
        s.supported_present_gravity_y = 0;
        s.min_scaled_image_extent = min_image_extent;
        s.max_scaled_image_extent = max_image_extent;
    }
    let compat = unsafe { find(caps_p_next, STYPE_SURFACE_PRESENT_MODE_COMPATIBILITY) };
    if !compat.is_null() {
        // SAFETY: the structure declares this type, so it has this layout.
        let s = unsafe { &mut *compat.cast::<CountAndArray>() };
        if s.array.is_null() {
            s.count = 1;
        } else if s.count >= 1 {
            // SAFETY: the guest sized the array to `count`, which is at least 1.
            unsafe { *s.array.cast::<VkPresentModeKHR>() = mode };
            s.count = 1;
        }
        // A zero-sized array with a non-null pointer keeps its count of 0: the
        // guest asked for nothing and gets nothing (`VK_INCOMPLETE` semantics
        // do not apply to a chained struct, so no result changes).
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    fn ext(w: u32, h: u32) -> VkExtent2D {
        VkExtent2D {
            width: w,
            height: h,
        }
    }

    #[test]
    fn a_chain_without_a_fence_struct_reads_zero() {
        assert_eq!(unsafe { present_fence(ptr::null(), 0) }, 0);
        let other = Head {
            s_type: 7,
            p_next: ptr::null_mut(),
        };
        assert_eq!(unsafe { present_fence((&raw const other).cast(), 0) }, 0);
    }

    #[test]
    fn fences_are_read_per_swapchain_and_bounded_by_the_count() {
        let fences: [VkFence; 2] = [0x1111, 0x2222];
        let mut tail = Head {
            s_type: 99,
            p_next: ptr::null_mut(),
        };
        let fi = CountAndArray {
            s_type: STYPE_PRESENT_FENCE_INFO,
            p_next: (&raw mut tail).cast(),
            count: 2,
            array: fences.as_ptr().cast_mut().cast(),
        };
        let lead = Head {
            s_type: 5,
            p_next: (&raw const fi).cast_mut().cast(),
        };
        let p = (&raw const lead).cast::<c_void>();
        assert_eq!(unsafe { present_fence(p, 0) }, 0x1111);
        assert_eq!(unsafe { present_fence(p, 1) }, 0x2222);
        assert_eq!(unsafe { present_fence(p, 2) }, 0);
    }

    #[test]
    fn a_present_mode_request_is_read_per_swapchain() {
        let modes: [VkPresentModeKHR; 1] = [1];
        let mi = CountAndArray {
            s_type: STYPE_PRESENT_MODE_INFO,
            p_next: ptr::null_mut(),
            count: 1,
            array: modes.as_ptr().cast_mut().cast(),
        };
        let p = (&raw const mi).cast::<c_void>();
        assert_eq!(unsafe { present_mode_request(p, 0) }, Some(1));
        assert_eq!(unsafe { present_mode_request(p, 1) }, None);
        assert_eq!(unsafe { present_mode_request(ptr::null(), 0) }, None);
    }

    #[test]
    fn creation_structs_are_read_and_absent_ones_are_none() {
        assert_eq!(unsafe { create_modes(ptr::null()) }, None);
        assert_eq!(unsafe { create_scaling(ptr::null()) }, None);
        let modes: [VkPresentModeKHR; 2] = [2, 1];
        let sci = ScalingCreateInfo {
            s_type: STYPE_PRESENT_SCALING_CREATE_INFO,
            p_next: ptr::null_mut(),
            scaling_behavior: 1,
            present_gravity_x: 2,
            present_gravity_y: 4,
        };
        let mci = CountAndArray {
            s_type: STYPE_PRESENT_MODES_CREATE_INFO,
            p_next: (&raw const sci).cast_mut().cast(),
            count: 2,
            array: modes.as_ptr().cast_mut().cast(),
        };
        let p = (&raw const mci).cast::<c_void>();
        assert_eq!(unsafe { create_modes(p) }, Some(vec![2, 1]));
        assert_eq!(unsafe { create_scaling(p) }, Some((1, 2, 4)));
    }

    #[test]
    fn release_info_names_the_swapchain_and_its_indices() {
        let idx: [u32; 3] = [0, 2, 1];
        let ri = ReleaseImagesInfo {
            s_type: STYPE_RELEASE_IMAGES_INFO,
            p_next: ptr::null_mut(),
            swapchain: 0xabc,
            image_index_count: 3,
            image_indices: idx.as_ptr(),
        };
        assert_eq!(
            unsafe { release_info((&raw const ri).cast()) },
            (0xabc, vec![0, 2, 1])
        );
        assert_eq!(unsafe { release_info(ptr::null()) }, (0, vec![]));
    }

    #[test]
    fn features_are_served_for_maintenance_and_refused_for_the_v2_waits() {
        let mut wait2 = OneFeature {
            s_type: STYPE_PRESENT_WAIT2_FEATURES,
            p_next: ptr::null_mut(),
            feature: 1,
        };
        let mut id2 = OneFeature {
            s_type: STYPE_PRESENT_ID2_FEATURES,
            p_next: (&raw mut wait2).cast(),
            feature: 1,
        };
        let mut other = OneFeature {
            s_type: 4242,
            p_next: (&raw mut id2).cast(),
            feature: 1,
        };
        let mut m1 = OneFeature {
            s_type: STYPE_FEATURES,
            p_next: (&raw mut other).cast(),
            feature: 0,
        };
        unsafe { features2((&raw mut m1).cast()) };
        assert_eq!(m1.feature, 1);
        assert_eq!(other.feature, 1);
        assert_eq!(id2.feature, 0);
        assert_eq!(wait2.feature, 0);
        // A null chain is a no-op.
        unsafe { features2(ptr::null_mut()) };
    }

    #[test]
    fn surface_caps_say_no_scaling_and_one_compatible_mode() {
        let asked = SurfacePresentMode {
            s_type: STYPE_SURFACE_PRESENT_MODE,
            p_next: ptr::null_mut(),
            present_mode: 1,
        };
        let mut modes: [VkPresentModeKHR; 4] = [9, 9, 9, 9];
        let mut compat = CountAndArray {
            s_type: STYPE_SURFACE_PRESENT_MODE_COMPATIBILITY,
            p_next: ptr::null_mut(),
            count: 4,
            array: modes.as_mut_ptr().cast(),
        };
        let mut scaling = ScalingCapabilities {
            s_type: STYPE_SURFACE_PRESENT_SCALING_CAPABILITIES,
            p_next: (&raw mut compat).cast(),
            supported_present_scaling: 0xff,
            supported_present_gravity_x: 0xff,
            supported_present_gravity_y: 0xff,
            min_scaled_image_extent: ext(0, 0),
            max_scaled_image_extent: ext(0, 0),
        };
        unsafe {
            surface_caps2(
                (&raw const asked).cast(),
                (&raw mut scaling).cast(),
                ext(1, 1),
                ext(8192, 8192),
            )
        };
        assert_eq!(scaling.supported_present_scaling, 0);
        assert_eq!(scaling.supported_present_gravity_x, 0);
        assert_eq!(scaling.supported_present_gravity_y, 0);
        assert_eq!(scaling.min_scaled_image_extent.width, 1);
        assert_eq!(scaling.max_scaled_image_extent.height, 8192);
        assert_eq!(compat.count, 1);
        assert_eq!(modes[0], 1);
        assert_eq!(modes[1], 9);
    }

    #[test]
    fn a_compatibility_count_query_answers_one_and_defaults_to_fifo() {
        let mut compat = CountAndArray {
            s_type: STYPE_SURFACE_PRESENT_MODE_COMPATIBILITY,
            p_next: ptr::null_mut(),
            count: 0,
            array: ptr::null_mut(),
        };
        unsafe { surface_caps2(ptr::null(), (&raw mut compat).cast(), ext(1, 1), ext(2, 2)) };
        assert_eq!(compat.count, 1);
        let mut one: [VkPresentModeKHR; 1] = [9];
        compat.array = one.as_mut_ptr().cast();
        unsafe { surface_caps2(ptr::null(), (&raw mut compat).cast(), ext(1, 1), ext(2, 2)) };
        assert_eq!(one[0], VK_PRESENT_MODE_FIFO_KHR);
        // Nothing chained: nothing written, nothing read.
        unsafe { surface_caps2(ptr::null(), ptr::null_mut(), ext(1, 1), ext(2, 2)) };
    }
}
