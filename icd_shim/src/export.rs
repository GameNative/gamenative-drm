//! Scan-out export emulation for gamescope on a system driver.
//!
//! gamescope's flippable images are LINEAR, DRM-modifier tiled and exported as
//! dma-buf. On Adreno the driver allocates exportable memory from the system
//! dma-heap, which libgndrm_shim.so redirects to the daemon's AHardwareBuffer
//! pool. A system driver such as Mali neither reports that combination as
//! exportable nor allocates from the dma-heap, so the image is backed by a pool
//! buffer here instead: the image is created against the pool stride, its
//! memory imports the pool fd, and the export queries answer with that fd.

#![allow(non_snake_case)]

use crate::proxy::{find_device, find_instance_by_pd, gn_real_device_proc, gn_real_inst_proc};
use crate::vk::*;
use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

const STYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES: VkStructureType = 1000071001;
const STYPE_IMAGE_FORMAT_LIST_CREATE_INFO: VkStructureType = 1000147000;
const ASPECT_MEMORY_PLANE_0: VkImageAspectFlags = 0x80;
const EXT_MEM_FEATURE_DEDICATED_ONLY: u32 = 0x1;
const EXT_MEM_FEATURE_EXPORTABLE: u32 = 0x2;
const EXT_MEM_FEATURE_IMPORTABLE: u32 = 0x4;
const MOD_LINEAR: u64 = 0;

static ENABLED: AtomicBool = AtomicBool::new(false);
static LOGGED_QUERY: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[derive(Clone, Copy)]
struct EmuImage {
    image: VkImage,
    fd: i32,
    stride: u32,
    height: u32,
    linear_tiling: bool,
}

static IMAGES: Mutex<Vec<EmuImage>> = Mutex::new(Vec::new());
static MEMS: Mutex<Vec<(VkDeviceMemory, VkImage)>> = Mutex::new(Vec::new());

#[repr(C)]
struct BaseIn {
    sType: VkStructureType,
    pNext: *const c_void,
}

#[repr(C)]
struct ExternalMemoryProperties {
    externalMemoryFeatures: u32,
    exportFromImportedHandleTypes: u32,
    compatibleHandleTypes: u32,
}

#[repr(C)]
struct ExternalImageFormatProperties {
    sType: VkStructureType,
    pNext: *mut c_void,
    externalMemoryProperties: ExternalMemoryProperties,
}

#[repr(C)]
struct PhysicalDeviceExternalImageFormatInfo {
    sType: VkStructureType,
    pNext: *const c_void,
    handleType: u32,
}

#[repr(C)]
struct ImageFormatListCreateInfo {
    sType: VkStructureType,
    pNext: *const c_void,
    viewFormatCount: u32,
    pViewFormats: *const VkFormat,
}

#[repr(C)]
struct ImageDrmFormatModifierProperties {
    sType: VkStructureType,
    pNext: *mut c_void,
    drmFormatModifier: u64,
}

unsafe fn find_in(mut p: *const c_void, stype: VkStructureType) -> *const c_void {
    while !p.is_null() {
        let b = p as *const BaseIn;
        if unsafe { (*b).sType } == stype {
            return p;
        }
        p = unsafe { (*b).pNext };
    }
    ptr::null()
}

fn four_bpp(format: VkFormat) -> bool {
    matches!(format, 37 | 43 | 44 | 50 | 58 | 64)
}

fn emu_image(image: VkImage) -> Option<EmuImage> {
    IMAGES.lock().ok()?.iter().find(|e| e.image == image).copied()
}

unsafe fn real_dev<T>(device: VkDevice, name: &CStr) -> Option<(VkDevice, T)> {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return None;
    }
    let f = unsafe { gn_real_device_proc(gd, name) }?;
    Some((unsafe { (*gd).real }, unsafe { std::mem::transmute_copy(&f) }))
}

/// `vkGetPhysicalDeviceImageFormatProperties2`: a LINEAR DRM-modifier dma-buf
/// query the driver refuses (or answers as not exportable) is answered from
/// the plain LINEAR query instead, as exportable.
pub unsafe extern "C" fn gn_vkGetPhysicalDeviceImageFormatProperties2(
    pd: VkPhysicalDevice,
    info: *const VkPhysicalDeviceImageFormatInfo2,
    props: *mut VkImageFormatProperties2,
) -> VkResult {
    let gi = unsafe { find_instance_by_pd(pd) };
    if gi.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let raw = unsafe {
        gn_real_inst_proc((*gi).real, c"vkGetPhysicalDeviceImageFormatProperties2")
            .or_else(|| gn_real_inst_proc((*gi).real, c"vkGetPhysicalDeviceImageFormatProperties2KHR"))
    };
    let Some(raw) = raw else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let real: PFN_vkGetPhysicalDeviceImageFormatProperties2 = unsafe { std::mem::transmute(raw) };
    let res = unsafe { real(pd, info, props) };
    if !enabled() || unsafe { (*info).tiling } != VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT {
        return res;
    }
    let modinfo = unsafe {
        find_in((*info).pNext, VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT)
    } as *const VkPhysicalDeviceImageDrmFormatModifierInfoEXT;
    let extinfo = unsafe {
        find_in((*info).pNext, VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO)
    } as *const PhysicalDeviceExternalImageFormatInfo;
    if modinfo.is_null()
        || unsafe { (*modinfo).drmFormatModifier } != MOD_LINEAR
        || extinfo.is_null()
        || unsafe { (*extinfo).handleType } != VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT
        || !four_bpp(unsafe { (*info).format })
    {
        return res;
    }
    let extprops = unsafe {
        find_in((*props).pNext as *const c_void, STYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES)
    } as *mut ExternalImageFormatProperties;
    if res == VK_SUCCESS
        && (extprops.is_null()
            || unsafe { (*extprops).externalMemoryProperties.externalMemoryFeatures }
                & EXT_MEM_FEATURE_EXPORTABLE
                != 0)
    {
        return res;
    }
    let lin = VkPhysicalDeviceImageFormatInfo2 {
        sType: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
        pNext: ptr::null(),
        format: unsafe { (*info).format },
        type_: unsafe { (*info).type_ },
        tiling: VK_IMAGE_TILING_LINEAR,
        usage: unsafe { (*info).usage },
        flags: unsafe { (*info).flags },
    };
    let mut lp = VkImageFormatProperties2 {
        sType: VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2,
        pNext: ptr::null_mut(),
        imageFormatProperties: unsafe { (*props).imageFormatProperties },
    };
    let lres = unsafe { real(pd, &lin, &mut lp) };
    if !LOGGED_QUERY.swap(true, Ordering::Relaxed) {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: modifier query fmt={} usage={:#x} flags={:#x} driver={res} linear={lres}",
            lin.format,
            lin.usage,
            lin.flags
        ));
    }
    if lres != VK_SUCCESS {
        return res;
    }
    unsafe {
        (*props).imageFormatProperties = lp.imageFormatProperties;
        (*props).imageFormatProperties.maxMipLevels = 1;
        (*props).imageFormatProperties.maxArrayLayers = 1;
        if !extprops.is_null() {
            (*extprops).externalMemoryProperties = ExternalMemoryProperties {
                externalMemoryFeatures: EXT_MEM_FEATURE_DEDICATED_ONLY
                    | EXT_MEM_FEATURE_EXPORTABLE
                    | EXT_MEM_FEATURE_IMPORTABLE,
                exportFromImportedHandleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                compatibleHandleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
            };
        }
    }
    VK_SUCCESS
}

/// Image create for an exportable LINEAR DRM-modifier image. Returns `None`
/// when the request is not one this emulates, so the caller creates normally.
pub unsafe fn try_create_image(
    gd: *mut crate::proxy::GnDevice,
    ci: *const VkImageCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkImage,
) -> Option<VkResult> {
    if !enabled() {
        return None;
    }
    let ci = unsafe { &*ci };
    if ci.tiling != VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT || !four_bpp(ci.format) {
        return None;
    }
    let list = unsafe { find_in(ci.pNext, VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT) }
        as *const VkImageDrmFormatModifierListCreateInfoEXT;
    let ext = unsafe { find_in(ci.pNext, VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO) }
        as *const VkExternalMemoryImageCreateInfo;
    if list.is_null() || ext.is_null() {
        return None;
    }
    let mods = unsafe {
        std::slice::from_raw_parts((*list).pDrmFormatModifiers, (*list).drmFormatModifierCount as usize)
    };
    if !mods.contains(&MOD_LINEAR) {
        return None;
    }
    let Some(buf) = crate::scanout::allocate(ci.extent.width, ci.extent.height) else {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: pool allocation {}x{} refused; driver create",
            ci.extent.width,
            ci.extent.height
        ));
        return None;
    };
    let create = unsafe { (*gd).f.create_image.unwrap() };
    let dev = unsafe { (*gd).real };
    let fl = unsafe { find_in(ci.pNext, STYPE_IMAGE_FORMAT_LIST_CREATE_INFO) } as *const ImageFormatListCreateInfo;
    let fl_copy = if fl.is_null() {
        None
    } else {
        Some(ImageFormatListCreateInfo {
            sType: STYPE_IMAGE_FORMAT_LIST_CREATE_INFO,
            pNext: ptr::null(),
            viewFormatCount: unsafe { (*fl).viewFormatCount },
            pViewFormats: unsafe { (*fl).pViewFormats },
        })
    };
    let fl_ptr = fl_copy
        .as_ref()
        .map_or(ptr::null(), |f| f as *const _ as *const c_void);
    let ext_img = VkExternalMemoryImageCreateInfo {
        sType: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        pNext: fl_ptr,
        handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    let layout = VkSubresourceLayout {
        offset: 0,
        size: 0,
        rowPitch: u64::from(buf.stride),
        arrayPitch: 0,
        depthPitch: 0,
    };
    let explicit = VkImageDrmFormatModifierExplicitCreateInfoEXT {
        sType: VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
        pNext: &ext_img as *const _ as *const c_void,
        drmFormatModifier: MOD_LINEAR,
        drmFormatModifierPlaneCount: 1,
        pPlaneLayouts: &layout,
    };
    let mut ci2 = VkImageCreateInfo {
        pNext: &explicit as *const _ as *const c_void,
        ..*ci
    };
    let mut linear_tiling = false;
    let mut res = unsafe { create(dev, &ci2, alloc, out) };
    if res != VK_SUCCESS {
        let r1 = res;
        ci2.pNext = &ext_img as *const _ as *const c_void;
        ci2.tiling = VK_IMAGE_TILING_LINEAR;
        res = unsafe { create(dev, &ci2, alloc, out) };
        linear_tiling = true;
        crate::logf!(gndrm_obf::obf!(
            "export-emu: explicit LINEAR create {r1}; LINEAR tiling create {res}"
        ));
    }
    if res != VK_SUCCESS {
        unsafe { libc::close(buf.fd) };
        return Some(res);
    }
    let image = unsafe { *out };
    if linear_tiling {
        if let Some(gsl) = unsafe { gn_real_device_proc(gd, c"vkGetImageSubresourceLayout") } {
            let gsl: PFN_vkGetImageSubresourceLayout = unsafe { std::mem::transmute(gsl) };
            let sub = VkImageSubresource {
                aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                mipLevel: 0,
                arrayLayer: 0,
            };
            let mut l = VkSubresourceLayout {
                offset: 0,
                size: 0,
                rowPitch: 0,
                arrayPitch: 0,
                depthPitch: 0,
            };
            unsafe { gsl(dev, image, &sub, &mut l) };
            if l.rowPitch != u64::from(buf.stride) {
                crate::logf!(gndrm_obf::obf!(
                    "export-emu: LINEAR pitch {} != pool stride {}",
                    l.rowPitch,
                    buf.stride
                ));
            }
        }
    }
    crate::logf!(gndrm_obf::obf!(
        "export-emu: image {:#x} {}x{} fmt={} usage={:#x} pool fd={} ino={} stride={} {}",
        image,
        ci.extent.width,
        ci.extent.height,
        ci.format,
        ci.usage,
        buf.fd,
        buf.ino,
        buf.stride,
        if linear_tiling { "linear-tiling" } else { "explicit-modifier" }
    ));
    if let Ok(mut v) = IMAGES.lock() {
        v.push(EmuImage {
            image,
            fd: buf.fd,
            stride: buf.stride,
            height: ci.extent.height,
            linear_tiling,
        });
    }
    Some(VK_SUCCESS)
}

pub unsafe extern "C" fn gn_vkAllocateMemory(
    device: VkDevice,
    ai: *const VkMemoryAllocateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkDeviceMemory,
) -> VkResult {
    let Some((dev, real)) = (unsafe { real_dev::<PFN_vkAllocateMemory>(device, c"vkAllocateMemory") }) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    if !enabled() {
        return unsafe { real(dev, ai, alloc, out) };
    }
    let ded = unsafe { find_in((*ai).pNext, VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO) }
        as *const VkMemoryDedicatedAllocateInfo;
    let emu = if ded.is_null() { None } else { emu_image(unsafe { (*ded).image }) };
    let Some(emu) = emu else {
        return unsafe { real(dev, ai, alloc, out) };
    };
    let fd = unsafe { libc::fcntl(emu.fd, libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return VK_ERROR_OUT_OF_HOST_MEMORY;
    }
    let ded2 = VkMemoryDedicatedAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        pNext: ptr::null(),
        image: emu.image,
        buffer: 0,
    };
    let imp = VkImportMemoryFdInfoKHR {
        sType: VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
        pNext: &ded2 as *const _ as *const c_void,
        handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        fd,
    };
    let ai2 = VkMemoryAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        pNext: &imp as *const _ as *const c_void,
        allocationSize: unsafe { (*ai).allocationSize },
        memoryTypeIndex: unsafe { (*ai).memoryTypeIndex },
    };
    let res = unsafe { real(dev, &ai2, alloc, out) };
    crate::logf!(gndrm_obf::obf!(
        "export-emu: import pool fd for image {:#x} size={} type={} -> {res}",
        emu.image,
        ai2.allocationSize,
        ai2.memoryTypeIndex
    ));
    if res != VK_SUCCESS {
        unsafe { libc::close(fd) };
        return res;
    }
    if let Ok(mut m) = MEMS.lock() {
        m.push((unsafe { *out }, emu.image));
    }
    VK_SUCCESS
}

pub unsafe extern "C" fn gn_vkGetMemoryFdKHR(
    device: VkDevice,
    info: *const VkMemoryGetFdInfoKHR,
    fd: *mut i32,
) -> VkResult {
    type Pfn = unsafe extern "C" fn(VkDevice, *const VkMemoryGetFdInfoKHR, *mut i32) -> VkResult;
    let mem = unsafe { (*info).memory };
    let img = MEMS
        .lock()
        .ok()
        .and_then(|m| m.iter().find(|(mm, _)| *mm == mem).map(|(_, i)| *i));
    if let Some(e) = img.and_then(emu_image) {
        let d = unsafe { libc::fcntl(e.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if d < 0 {
            return VK_ERROR_OUT_OF_HOST_MEMORY;
        }
        unsafe { *fd = d };
        return VK_SUCCESS;
    }
    let Some((dev, real)) = (unsafe { real_dev::<Pfn>(device, c"vkGetMemoryFdKHR") }) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    unsafe { real(dev, info, fd) }
}

pub unsafe extern "C" fn gn_vkGetImageDrmFormatModifierPropertiesEXT(
    device: VkDevice,
    image: VkImage,
    props: *mut c_void,
) -> VkResult {
    type Pfn = unsafe extern "C" fn(VkDevice, VkImage, *mut c_void) -> VkResult;
    if emu_image(image).is_some() {
        unsafe { (*(props as *mut ImageDrmFormatModifierProperties)).drmFormatModifier = MOD_LINEAR };
        return VK_SUCCESS;
    }
    let Some((dev, real)) = (unsafe { real_dev::<Pfn>(device, c"vkGetImageDrmFormatModifierPropertiesEXT") }) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    unsafe { real(dev, image, props) }
}

pub unsafe extern "C" fn gn_vkGetImageSubresourceLayout(
    device: VkDevice,
    image: VkImage,
    sub: *const VkImageSubresource,
    out: *mut VkSubresourceLayout,
) {
    let Some((dev, real)) =
        (unsafe { real_dev::<PFN_vkGetImageSubresourceLayout>(device, c"vkGetImageSubresourceLayout") })
    else {
        return;
    };
    if let Some(e) = emu_image(image) {
        if unsafe { (*sub).aspectMask } == ASPECT_MEMORY_PLANE_0 {
            unsafe {
                *out = VkSubresourceLayout {
                    offset: 0,
                    size: u64::from(e.stride) * u64::from(e.height),
                    rowPitch: u64::from(e.stride),
                    arrayPitch: 0,
                    depthPitch: 0,
                };
            }
            if e.linear_tiling {
                let c = VkImageSubresource {
                    aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                    mipLevel: 0,
                    arrayLayer: 0,
                };
                unsafe { real(dev, image, &c, out) };
            }
            return;
        }
    }
    unsafe { real(dev, image, sub, out) }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct WriteDescriptorSet {
    sType: VkStructureType,
    pNext: *const c_void,
    dstSet: u64,
    dstBinding: u32,
    dstArrayElement: u32,
    descriptorCount: u32,
    descriptorType: i32,
    pImageInfo: *const DescriptorImageInfo,
    pBufferInfo: *const DescriptorBufferInfo,
    pTexelBufferView: *const u64,
}

#[repr(C)]
struct DescriptorImageInfo {
    sampler: u64,
    imageView: u64,
    imageLayout: i32,
}

#[repr(C)]
struct DescriptorBufferInfo {
    buffer: u64,
    offset: u64,
    range: u64,
}

static LOGGED_NULL_SKIP: AtomicBool = AtomicBool::new(false);

unsafe fn element_is_null(w: &WriteDescriptorSet, i: usize) -> bool {
    unsafe {
        match w.descriptorType {
            1..=3 | 10 if !w.pImageInfo.is_null() => (*w.pImageInfo.add(i)).imageView == 0,
            4 | 5 if !w.pTexelBufferView.is_null() => *w.pTexelBufferView.add(i) == 0,
            6..=9 if !w.pBufferInfo.is_null() => (*w.pBufferInfo.add(i)).buffer == 0,
            _ => false,
        }
    }
}

/// `vkUpdateDescriptorSets` without nullDescriptor: null elements (gamescope's
/// unused sampler slots and missing LUTs) are left unwritten instead.
pub unsafe extern "C" fn gn_vkUpdateDescriptorSets(
    device: VkDevice,
    write_count: u32,
    writes: *const c_void,
    copy_count: u32,
    copies: *const c_void,
) {
    type Pfn = unsafe extern "C" fn(VkDevice, u32, *const c_void, u32, *const c_void);
    let Some((dev, real)) = (unsafe { real_dev::<Pfn>(device, c"vkUpdateDescriptorSets") }) else {
        return;
    };
    if !enabled() || writes.is_null() || write_count == 0 {
        return unsafe { real(dev, write_count, writes, copy_count, copies) };
    }
    let ws = unsafe { std::slice::from_raw_parts(writes as *const WriteDescriptorSet, write_count as usize) };
    let mut out: Vec<WriteDescriptorSet> = Vec::with_capacity(ws.len() + 8);
    let mut skipped = 0usize;
    for w in ws {
        let n = w.descriptorCount as usize;
        let mut i = 0;
        while i < n {
            if unsafe { element_is_null(w, i) } {
                skipped += 1;
                i += 1;
                continue;
            }
            let start = i;
            while i < n && !unsafe { element_is_null(w, i) } {
                i += 1;
            }
            let mut run = *w;
            run.dstArrayElement = w.dstArrayElement + start as u32;
            run.descriptorCount = (i - start) as u32;
            unsafe {
                if !w.pImageInfo.is_null() {
                    run.pImageInfo = w.pImageInfo.add(start);
                }
                if !w.pBufferInfo.is_null() {
                    run.pBufferInfo = w.pBufferInfo.add(start);
                }
                if !w.pTexelBufferView.is_null() {
                    run.pTexelBufferView = w.pTexelBufferView.add(start);
                }
            }
            out.push(run);
        }
    }
    if skipped > 0 && !LOGGED_NULL_SKIP.swap(true, Ordering::Relaxed) {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: skipped {skipped} null descriptor element(s) in {write_count} write(s)"
        ));
    }
    unsafe { real(dev, out.len() as u32, out.as_ptr() as *const c_void, copy_count, copies) }
}
