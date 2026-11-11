//! The X11 wrapper's feature lift, served by the shim.
//!
//! Under X11 a container's ICD is `libvulkan_wrapper.so`: it reports a fixed
//! set of `VkPhysicalDeviceFeatures` whatever the driver answered, then at
//! `vkCreateDevice` ANDs each of those back down to the driver's own answer
//! (`wrapper_physical_device.c`: `supported_features->dualSrcBlend = true;
//! // Missing on G715 r38p1`; `wrapper_device.c`: `DISABLE_FEATURE`). Both
//! halves are the mechanism: a driver that gates a feature it implements
//! refuses a create that requests the bit (`VK_ERROR_FEATURE_NOT_PRESENT`), so
//! advertising alone turns a higher D3D feature level into a failed device.
//!
//! Under gamescope this shim is the ICD. A Turnip build that misdetects the
//! chip reports the gated bits as zero, DXVK answers `D3D_FEATURE_LEVEL_0`, and
//! the game exits with "Minimum required feature level D3D_FEATURE_LEVEL_10_0
//! not supported". Lifting here instead of driving the wrapper keeps one
//! emulation layer: the same bits, in the layer that already sits between the
//! guest and the driver.
//!
//! The list is the wrapper's minus `textureCompressionBC`. The wrapper backs
//! that bit with real BCN decoding (`bcdec.c`, the bc6/bc7 compute shaders) and
//! this shim has no such path, so advertising it would trade a clean
//! unsupported-format for corrupt textures. `logicOp` and
//! `variableMultisampleRate` are commented out in the wrapper as well, and stay
//! the driver's answer here.

#![allow(non_snake_case)]

use crate::proxy::{gn_real_inst_proc, GnInstance};
use crate::vk::*;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

static LOGGED: AtomicBool = AtomicBool::new(false);
static MASKED: AtomicBool = AtomicBool::new(false);
static MISMATCH: AtomicBool = AtomicBool::new(false);

/// `VkPhysicalDeviceFeatures` in spec order: 55 VkBool32s the driver and the
/// guest agree on, addressed by name so the overlay and the mask cannot drift.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CoreFeatures {
    pub robustBufferAccess: u32,
    pub fullDrawIndexUint32: u32,
    pub imageCubeArray: u32,
    pub independentBlend: u32,
    pub geometryShader: u32,
    pub tessellationShader: u32,
    pub sampleRateShading: u32,
    pub dualSrcBlend: u32,
    pub logicOp: u32,
    pub multiDrawIndirect: u32,
    pub drawIndirectFirstInstance: u32,
    pub depthClamp: u32,
    pub depthBiasClamp: u32,
    pub fillModeNonSolid: u32,
    pub depthBounds: u32,
    pub wideLines: u32,
    pub largePoints: u32,
    pub alphaToOne: u32,
    pub multiViewport: u32,
    pub samplerAnisotropy: u32,
    pub textureCompressionETC2: u32,
    pub textureCompressionASTC_LDR: u32,
    pub textureCompressionBC: u32,
    pub occlusionQueryPrecise: u32,
    pub pipelineStatisticsQuery: u32,
    pub vertexPipelineStoresAndAtomics: u32,
    pub fragmentStoresAndAtomics: u32,
    pub shaderTessellationAndGeometryPointSize: u32,
    pub shaderImageGatherExtended: u32,
    pub shaderStorageImageExtendedFormats: u32,
    pub shaderStorageImageMultisample: u32,
    pub shaderStorageImageReadWithoutFormat: u32,
    pub shaderStorageImageWriteWithoutFormat: u32,
    pub shaderUniformBufferArrayDynamicIndexing: u32,
    pub shaderSampledImageArrayDynamicIndexing: u32,
    pub shaderStorageBufferArrayDynamicIndexing: u32,
    pub shaderStorageImageArrayDynamicIndexing: u32,
    pub shaderClipDistance: u32,
    pub shaderCullDistance: u32,
    pub shaderFloat64: u32,
    pub shaderInt64: u32,
    pub shaderInt16: u32,
    pub shaderResourceResidency: u32,
    pub shaderResourceMinLod: u32,
    pub sparseBinding: u32,
    pub sparseResidencyBuffer: u32,
    pub sparseResidencyImage2D: u32,
    pub sparseResidencyImage3D: u32,
    pub sparseResidency2Samples: u32,
    pub sparseResidency4Samples: u32,
    pub sparseResidency8Samples: u32,
    pub sparseResidency16Samples: u32,
    pub sparseResidencyAliased: u32,
    pub variableMultisampleRate: u32,
    pub inheritedQueries: u32,
}

/// The struct a `pNext` chain entry points at, and the shape the driver fills.
#[repr(C)]
struct Features2Out {
    s_type: VkStructureType,
    p_next: *mut c_void,
    features: CoreFeatures,
}

/// The core features reported as supported whatever the driver answered. Each
/// one is masked back to the driver's answer at create time, so the pair is
/// what lifts DXVK's feature level without asking the driver for a capability
/// it gated.
fn lift_core(c: &mut CoreFeatures) -> Vec<&'static str> {
    let mut raised = Vec::new();
    macro_rules! set {
        ($($f:ident),* $(,)?) => {
            $(
                if c.$f == 0 {
                    c.$f = 1;
                    raised.push(stringify!($f));
                }
            )*
        };
    }
    set!(
        geometryShader,
        multiViewport,
        depthClamp,
        depthBiasClamp,
        fillModeNonSolid,
        shaderClipDistance,
        shaderCullDistance,
        dualSrcBlend,
        multiDrawIndirect,
        vertexPipelineStoresAndAtomics,
    );
    raised
}

/// Drop every lifted bit the driver did not answer for itself. Only the lifted
/// bits: the rest of the create is the guest's own request and the driver's
/// answer was never about it.
pub fn mask(c: &mut CoreFeatures, base: &CoreFeatures) -> Vec<&'static str> {
    let mut cleared = Vec::new();
    macro_rules! and {
        ($($f:ident),* $(,)?) => {
            $(
                if c.$f != 0 && base.$f == 0 {
                    cleared.push(stringify!($f));
                }
                c.$f &= base.$f;
            )*
        };
    }
    and!(
        geometryShader,
        multiViewport,
        depthClamp,
        depthBiasClamp,
        fillModeNonSolid,
        shaderClipDistance,
        shaderCullDistance,
        dualSrcBlend,
        multiDrawIndirect,
        vertexPipelineStoresAndAtomics,
    );
    // The bits the driver will not actually have enabled. The wrapper logs the
    // same at its create ("Faking feature support for ..."); here it is the
    // calibration signal when a game misbehaves under a lift the driver gated.
    if !cleared.is_empty() && !MASKED.swap(true, Ordering::Relaxed) {
        crate::logf!("feature lift masked back to the driver's answer: {}",
            cleared.join(" "));
    }
    cleared
}

/// The bits the driver answers zero, in spec order. DXVK computes its D3D
/// feature level from exactly this set, so the line is the whole diagnosis of a
/// session that reports `D3D_FEATURE_LEVEL_0` — including the bits the lift
/// does not touch (`tessellationShader`, `logicOp`, `drawIndirectFirstInstance`).
fn zeros(c: &CoreFeatures) -> Vec<&'static str> {
    let mut z = Vec::new();
    macro_rules! scan {
        ($($f:ident),* $(,)?) => { $( if c.$f == 0 { z.push(stringify!($f)); } )* };
    }
    scan!(
        robustBufferAccess, fullDrawIndexUint32, imageCubeArray, independentBlend,
        geometryShader, tessellationShader, sampleRateShading, dualSrcBlend,
        logicOp, multiDrawIndirect, drawIndirectFirstInstance, depthClamp,
        depthBiasClamp, fillModeNonSolid, depthBounds, wideLines, largePoints,
        alphaToOne, multiViewport, samplerAnisotropy, textureCompressionETC2,
        textureCompressionASTC_LDR, textureCompressionBC, occlusionQueryPrecise,
        pipelineStatisticsQuery, vertexPipelineStoresAndAtomics,
        fragmentStoresAndAtomics, shaderTessellationAndGeometryPointSize,
        shaderImageGatherExtended, shaderStorageImageExtendedFormats,
        shaderStorageImageMultisample, shaderStorageImageReadWithoutFormat,
        shaderStorageImageWriteWithoutFormat,
        shaderUniformBufferArrayDynamicIndexing, shaderSampledImageArrayDynamicIndexing,
        shaderStorageBufferArrayDynamicIndexing, shaderStorageImageArrayDynamicIndexing,
        shaderClipDistance, shaderCullDistance, shaderFloat64, shaderInt64, shaderInt16,
        shaderResourceResidency, shaderResourceMinLod, sparseBinding,
        sparseResidencyBuffer, sparseResidencyImage2D, sparseResidencyImage3D,
        sparseResidency2Samples, sparseResidency4Samples, sparseResidency8Samples,
        sparseResidency16Samples, sparseResidencyAliased, variableMultisampleRate,
        inheritedQueries,
    );
    z
}

/// Overlay the lift on a `VkPhysicalDeviceFeatures2` the driver has answered.
/// Logged once per process: first the driver's zeros — the answer the guest
/// would get without the lift, and the one DXVK's feature level comes from —
/// then the bits this lift raised.
pub unsafe fn features(f: *mut c_void) {
    if f.is_null() {
        return;
    }
    let c = unsafe { &mut *((f as *mut u8).add(16).cast::<CoreFeatures>()) };
    let zero = zeros(c);
    let raised = lift_core(c);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        crate::logf!("driver core features zero: {}",
            zero.join(" "));
        crate::logf!("wrapper-style feature lift over the driver's zeros: {}",
            raised.join(" "));
    }
}

/// The bits the guest asks for that the driver answers zero: the create-time
/// reason the driver refuses with `VK_ERROR_FEATURE_NOT_PRESENT`, which names no
/// bit itself. Named once, before the mask, so the offender is read rather than
/// bisected.
pub fn request_mismatch(c: &CoreFeatures, base: &CoreFeatures) -> Vec<&'static str> {
    let mut bad = Vec::new();
    macro_rules! diff {
        ($($f:ident),* $(,)?) => {
            $( if c.$f != 0 && base.$f == 0 { bad.push(stringify!($f)); } )*
        };
    }
    diff!(
        robustBufferAccess, fullDrawIndexUint32, imageCubeArray, independentBlend,
        geometryShader, tessellationShader, sampleRateShading, dualSrcBlend,
        logicOp, multiDrawIndirect, drawIndirectFirstInstance, depthClamp,
        depthBiasClamp, fillModeNonSolid, depthBounds, wideLines, largePoints,
        alphaToOne, multiViewport, samplerAnisotropy, textureCompressionETC2,
        textureCompressionASTC_LDR, textureCompressionBC, occlusionQueryPrecise,
        pipelineStatisticsQuery, vertexPipelineStoresAndAtomics,
        fragmentStoresAndAtomics, shaderTessellationAndGeometryPointSize,
        shaderImageGatherExtended, shaderStorageImageExtendedFormats,
        shaderStorageImageMultisample, shaderStorageImageReadWithoutFormat,
        shaderStorageImageWriteWithoutFormat,
        shaderUniformBufferArrayDynamicIndexing, shaderSampledImageArrayDynamicIndexing,
        shaderStorageBufferArrayDynamicIndexing, shaderStorageImageArrayDynamicIndexing,
        shaderClipDistance, shaderCullDistance, shaderFloat64, shaderInt64, shaderInt16,
        shaderResourceResidency, shaderResourceMinLod, sparseBinding,
        sparseResidencyBuffer, sparseResidencyImage2D, sparseResidencyImage3D,
        sparseResidency2Samples, sparseResidency4Samples, sparseResidency8Samples,
        sparseResidency16Samples, sparseResidencyAliased, variableMultisampleRate,
        inheritedQueries,
    );
    if !bad.is_empty() && !MISMATCH.swap(true, Ordering::Relaxed) {
        crate::logf!("guest requests features the driver answers zero: {}",
            bad.join(" "));
    }
    bad
}

/// The driver's own answer for this physical device, through the real instance.
/// This is the base the create-time mask ANDs against; `None` when the driver
/// answers neither query. The distinction matters: an unreadable base is not a
/// driver that supports nothing, and masking against zeros would strip bits the
/// driver does have from a guest that asked for them.
pub unsafe fn driver_core(gi: *mut GnInstance, pd: VkPhysicalDevice) -> Option<CoreFeatures> {
    let mut base: CoreFeatures = unsafe { std::mem::zeroed() };
    if gi.is_null() {
        return None;
    }
    type Pfn2 = unsafe extern "C" fn(VkPhysicalDevice, *mut c_void);
    let inst = unsafe { (*gi).real };
    if let Some(raw) = unsafe { gn_real_inst_proc(inst, c"vkGetPhysicalDeviceFeatures2") } {
        let f: Pfn2 = unsafe { std::mem::transmute(raw) };
        let mut q = Features2Out {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
            p_next: ptr::null_mut(),
            features: unsafe { std::mem::zeroed() },
        };
        unsafe { f(pd, &mut q as *mut Features2Out as *mut c_void) };
        return Some(q.features);
    }
    if let Some(raw) = unsafe { gn_real_inst_proc(inst, c"vkGetPhysicalDeviceFeatures") } {
        let f: unsafe extern "C" fn(VkPhysicalDevice, *mut CoreFeatures) =
            unsafe { std::mem::transmute(raw) };
        unsafe { f(pd, &mut base) };
        return Some(base);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_the_vulkan_struct() {
        assert_eq!(std::mem::size_of::<CoreFeatures>(), 55 * 4);
        let c: CoreFeatures = unsafe { std::mem::zeroed() };
        let off = |p: *const u32| p as usize - std::ptr::addr_of!(c) as usize;
        assert_eq!(off(std::ptr::addr_of!(c.geometryShader)), 4 * 4);
        assert_eq!(off(std::ptr::addr_of!(c.dualSrcBlend)), 7 * 4);
        assert_eq!(
            off(std::ptr::addr_of!(c.vertexPipelineStoresAndAtomics)),
            25 * 4
        );
        assert_eq!(off(std::ptr::addr_of!(c.inheritedQueries)), 54 * 4);
    }

    #[test]
    fn lift_raises_the_list_and_nothing_else() {
        let mut c: CoreFeatures = unsafe { std::mem::zeroed() };
        let raised = lift_core(&mut c);
        assert_eq!(raised.len(), 10);
        assert!(raised.contains(&"dualSrcBlend"));
        assert!(raised.contains(&"multiDrawIndirect"));
        // Left to the driver, as in the wrapper.
        assert_eq!(c.logicOp, 0);
        assert_eq!(c.variableMultisampleRate, 0);
        // Backed by real BCN decoding in the wrapper, absent here.
        assert_eq!(c.textureCompressionBC, 0);
        assert_eq!(c.tessellationShader, 0);
    }

    #[test]
    fn lift_keeps_what_the_driver_already_answered() {
        let mut c: CoreFeatures = unsafe { std::mem::zeroed() };
        c.geometryShader = 1;
        let raised = lift_core(&mut c);
        assert_eq!(raised.len(), 9);
        assert!(!raised.contains(&"geometryShader"));
        assert_eq!(c.geometryShader, 1);
    }

    #[test]
    fn mask_clears_only_the_lifted_bits() {
        let mut base: CoreFeatures = unsafe { std::mem::zeroed() };
        base.dualSrcBlend = 1;
        let mut c: CoreFeatures = unsafe { std::mem::zeroed() };
        lift_core(&mut c);
        c.logicOp = 1;
        let cleared = mask(&mut c, &base);
        assert_eq!(c.dualSrcBlend, 1, "kept: the driver has it");
        assert_eq!(c.geometryShader, 0, "dropped: the driver gated it");
        assert_eq!(c.logicOp, 1, "not lifted, so never masked");
        assert_eq!(cleared.len(), 9);
        assert!(!cleared.contains(&"dualSrcBlend"));
    }
}
