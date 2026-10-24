//! Scan-out export emulation for gamescope on a system driver.
//!
//! gamescope's flippable images are LINEAR, DRM-modifier tiled and exported as
//! dma-buf. On Adreno the driver allocates exportable memory from the system
//! dma-heap, which libgndrm_shim.so redirects to the daemon's AHardwareBuffer
//! pool. A system driver such as Mali neither reports that combination as
//! exportable nor allocates from the dma-heap, so the image is backed by a pool
//! buffer here instead: the image is created against the pool stride, its
//! memory imports the pool fd, and the export queries answer with that fd. A
//! driver that refuses the fd import gets the same pool buffer as an
//! `AHardwareBuffer`.

#![allow(non_snake_case)]

use crate::proxy::{
    find_device, find_instance_by_pd, gn_real_device_proc, gn_real_inst_proc, STYPE_IMPORT_AHB,
};
use crate::vk::*;
use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

const STYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES: VkStructureType = 1000071001;
const STYPE_IMAGE_FORMAT_LIST_CREATE_INFO: VkStructureType = 1000147000;
const ASPECT_MEMORY_PLANE_0: VkImageAspectFlags = 0x80;
const EXT_MEM_FEATURE_DEDICATED_ONLY: u32 = 0x1;
const EXT_MEM_FEATURE_EXPORTABLE: u32 = 0x2;
const EXT_MEM_FEATURE_IMPORTABLE: u32 = 0x4;
const MOD_LINEAR: u64 = 0;
const STYPE_MEMORY_FD_PROPERTIES: VkStructureType = 1000074001;

const DMABUF_UNKNOWN: u8 = 0;
const DMABUF_WORKS: u8 = 1;
const DMABUF_REFUSED: u8 = 2;

/// Whether the driver has imported a pool fd as a dma-buf yet, or refused to.
static DMABUF_IMPORT: AtomicU8 = AtomicU8::new(DMABUF_UNKNOWN);

static ENABLED: AtomicBool = AtomicBool::new(false);
static LOGGED_QUERY: AtomicBool = AtomicBool::new(false);
/// The driver refused a LINEAR DRM-modifier image in the format query. Such a
/// driver (PowerVR) still accepts the explicit-layout create and then keeps
/// the image in a layout of its own, so the pool buffer scans out sheared:
/// the images take the AHardwareBuffer route from the first one.
static LINEAR_MODIFIER_REFUSED: AtomicBool = AtomicBool::new(false);

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
    /// The imported memory until `vkAllocateMemory` hands it out, else 0.
    mem: VkDeviceMemory,
}

static IMAGES: Mutex<Vec<EmuImage>> = Mutex::new(Vec::new());
static MEMS: Mutex<Vec<(VkDeviceMemory, VkImage)>> = Mutex::new(Vec::new());

/// gamescope's own imports of a client dma-buf (a guest frame): the image and
/// whether it was created for the `AHardwareBuffer` route.
static IMPORTS: Mutex<Vec<(VkImage, bool)>> = Mutex::new(Vec::new());

/// Import images logged so far; gamescope imports each client buffer once, so
/// this only caps a client that churns its buffers.
static IMPORT_LOGS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const IMPORT_LOG_FIRST_N: u32 = 24;

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
struct MemoryFdProperties {
    sType: VkStructureType,
    pNext: *mut c_void,
    memoryTypeBits: u32,
}

type PfnGetMemoryFdProperties =
    unsafe extern "C" fn(VkDevice, u32, i32, *mut MemoryFdProperties) -> VkResult;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::Mutex;

    static SERIAL: Mutex<()> = Mutex::new(());

    #[repr(C)]
    struct Link {
        s_type: VkStructureType,
        p_next: *const c_void,
        tag: u32,
    }

    fn link(s_type: VkStructureType, tag: u32, next: *const c_void) -> Link {
        Link {
            s_type,
            p_next: next,
            tag,
        }
    }

    #[test]
    fn four_bpp_accepts_the_8_bit_formats_and_refuses_wider_ones() {
        // The formats the scan-out pool can serve: 32-bit-per-pixel colour.
        for f in [37, 43, 44, 50, 58, 64] {
            assert!(
                four_bpp(f as VkFormat),
                "format {f} should be four bytes per pixel"
            );
        }
        // 16-bit-per-channel, single-channel and the undefined format are not.
        for f in [0, 9, 97, 100, 109] {
            assert!(
                !four_bpp(f as VkFormat),
                "format {f} should not be four bytes per pixel"
            );
        }
    }

    #[test]
    fn find_in_walks_the_chain_and_returns_null_when_absent() {
        let tail = link(STYPE_IMAGE_FORMAT_LIST_CREATE_INFO, 2, ptr::null());
        let mid = link(1000147001, 1, (&raw const tail).cast());
        let head = link(
            STYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES,
            0,
            (&raw const mid).cast(),
        );

        let found = unsafe { find_in((&raw const head).cast(), 1000147001) };
        assert_eq!(found, (&raw const mid).cast::<c_void>());
        let found = unsafe {
            find_in(
                (&raw const head).cast(),
                STYPE_IMAGE_FORMAT_LIST_CREATE_INFO,
            )
        };
        assert_eq!(found, (&raw const tail).cast::<c_void>());
        let none = unsafe { find_in((&raw const head).cast(), 12345) };
        assert!(none.is_null());
    }

    #[test]
    fn find_in_accepts_an_empty_chain() {
        assert!(unsafe { find_in(ptr::null(), STYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES) }.is_null());
    }

    #[test]
    fn the_emulation_is_off_until_the_system_driver_path_enables_it() {
        let _g = SERIAL.lock().unwrap();
        // Exercised in both directions so the test does not depend on order.
        set_enabled(false);
        assert!(!enabled());
        set_enabled(true);
        assert!(enabled());
        set_enabled(false);
        assert!(!enabled());
    }

    fn entry(image: VkImage, height: u32) -> EmuImage {
        EmuImage {
            image,
            fd: -1,
            stride: 64,
            height,
            linear_tiling: false,
            mem: 0,
        }
    }

    #[test]
    fn a_reused_image_handle_resolves_to_the_newest_entry() {
        let _g = SERIAL.lock().unwrap();
        let h: VkImage = 0x5eed_0001;
        register_image(entry(h, 16));
        MEMS.lock().unwrap().push((0x5eed_1001, h));
        let (dropped, mems) = forget_image(h).unwrap();
        assert_eq!(dropped.height, 16);
        assert_eq!(mems, 1);
        assert!(emu_image(h).is_none());
        register_image(entry(h, 1080));
        assert_eq!(emu_image(h).unwrap().height, 1080);
        assert!(forget_image(h).is_some());
        assert!(forget_image(h).is_none());
    }

    #[test]
    fn registering_over_a_live_handle_replaces_the_old_entry() {
        let _g = SERIAL.lock().unwrap();
        let h: VkImage = 0x5eed_0002;
        register_image(entry(h, 16));
        MEMS.lock().unwrap().push((0x5eed_1002, h));
        register_image(entry(h, 720));
        assert_eq!(emu_image(h).unwrap().height, 720);
        assert_eq!(
            IMAGES
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.image == h)
                .count(),
            1
        );
        assert!(!MEMS.lock().unwrap().iter().any(|(_, i)| *i == h));
        forget_image(h);
    }

    #[test]
    fn freeing_memory_drops_its_pair() {
        let _g = SERIAL.lock().unwrap();
        MEMS.lock().unwrap().push((0x5eed_1003, 0x5eed_0003));
        forget_memory(0x5eed_1003);
        assert!(!MEMS.lock().unwrap().iter().any(|(m, _)| *m == 0x5eed_1003));
    }

    #[test]
    fn the_memory_type_pick_prefers_device_local_within_the_allowed_bits() {
        let t = |propertyFlags| VkMemoryType {
            propertyFlags,
            heapIndex: 0,
        };
        // The Mali-G720's types: 0x7, 0xb, 0x11 on heap 0, 0x21 on heap 1.
        let types = [t(0x7), t(0xb), t(0x11), t(0x21)];
        assert_eq!(pick_memory_type(&types, 0xf), Some(0));
        assert_eq!(pick_memory_type(&types, 0x2), Some(1));
        assert_eq!(pick_memory_type(&[t(0x6), t(0x7)], 0x3), Some(1));
        assert_eq!(pick_memory_type(&[t(0x6), t(0x6)], 0x2), Some(1));
        assert_eq!(pick_memory_type(&types, 0x10), None);
        assert_eq!(pick_memory_type(&types, 0), None);
    }

    #[test]
    fn a_pre_imported_memory_is_handed_out_once() {
        let _g = SERIAL.lock().unwrap();
        let h: VkImage = 0x5eed_0004;
        register_image(EmuImage {
            mem: 0x5eed_1004,
            ..entry(h, 16)
        });
        assert_eq!(take_memory(h), Some(0x5eed_1004));
        assert_eq!(take_memory(h), None);
        assert_eq!(forget_image(h).unwrap().0.mem, 0);
    }

    #[test]
    fn an_import_entry_follows_its_image_handle() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        note_import(0x5150, false);
        assert_eq!(import_route(0x5150), Some(false));
        note_import(0x5150, true);
        assert_eq!(import_route(0x5150), Some(true));
        forget_import(0x5150);
        assert_eq!(import_route(0x5150), None);
        assert_eq!(import_route(0), None);
    }

    #[test]
    fn the_linear_modifier_is_the_one_gamescope_flips() {
        // DRM_FORMAT_MOD_LINEAR; the export query answers with this modifier.
        assert_eq!(MOD_LINEAR, 0);
    }
}

fn emu_image(image: VkImage) -> Option<EmuImage> {
    IMAGES
        .lock()
        .ok()?
        .iter()
        .rfind(|e| e.image == image)
        .copied()
}

fn close_fd(fd: i32) {
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

fn register_image(e: EmuImage) {
    let Ok(mut v) = IMAGES.lock() else {
        return;
    };
    v.retain(|old| {
        if old.image != e.image {
            return true;
        }
        crate::logf!(gndrm_obf::obf!(
            "export-emu: image {:#x} reused; dropping stale fd={}",
            old.image,
            old.fd
        ));
        close_fd(old.fd);
        false
    });
    v.push(e);
    drop(v);
    if let Ok(mut m) = MEMS.lock() {
        m.retain(|(_, i)| *i != e.image);
    }
}

fn forget_image(image: VkImage) -> Option<(EmuImage, usize)> {
    let e = {
        let mut v = IMAGES.lock().ok()?;
        let mut found = None;
        v.retain(|old| {
            if old.image != image {
                return true;
            }
            if let Some(prev) = found.replace(*old) {
                close_fd(prev.fd);
            }
            false
        });
        found?
    };
    close_fd(e.fd);
    let mems = MEMS.lock().map_or(0, |mut m| {
        let before = m.len();
        m.retain(|(_, i)| *i != image);
        before - m.len()
    });
    Some((e, mems))
}

pub fn forget_memory(mem: VkDeviceMemory) {
    if let Ok(mut m) = MEMS.lock() {
        m.retain(|(mm, _)| *mm != mem);
    }
}

fn note_import(image: VkImage, ahb: bool) {
    if let Ok(mut v) = IMPORTS.lock() {
        v.retain(|(i, _)| *i != image);
        v.push((image, ahb));
    }
}

fn import_route(image: VkImage) -> Option<bool> {
    if image == 0 {
        return None;
    }
    IMPORTS
        .lock()
        .ok()?
        .iter()
        .find(|(i, _)| *i == image)
        .map(|(_, ahb)| *ahb)
}

fn forget_import(image: VkImage) {
    if let Ok(mut v) = IMPORTS.lock() {
        v.retain(|(i, _)| *i != image);
    }
}

fn fd_ino(fd: i32) -> u64 {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat into a zeroed struct; the call validates `fd`.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return 0;
    }
    st.st_ino as u64
}

unsafe fn real_dev<T>(device: VkDevice, name: &CStr) -> Option<(VkDevice, T)> {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return None;
    }
    let f = unsafe { gn_real_device_proc(gd, name) }?;
    Some((unsafe { (*gd).real }, unsafe {
        std::mem::transmute_copy(&f)
    }))
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
        gn_real_inst_proc((*gi).real, c"vkGetPhysicalDeviceImageFormatProperties2").or_else(|| {
            gn_real_inst_proc((*gi).real, c"vkGetPhysicalDeviceImageFormatProperties2KHR")
        })
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
        find_in(
            (*info).pNext,
            VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
        )
    } as *const VkPhysicalDeviceImageDrmFormatModifierInfoEXT;
    let extinfo = unsafe {
        find_in(
            (*info).pNext,
            VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
        )
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
        find_in(
            (*props).pNext as *const c_void,
            STYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES,
        )
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
    let features = if extprops.is_null() {
        0
    } else {
        unsafe { (*extprops).externalMemoryProperties.externalMemoryFeatures }
    };
    let lres = unsafe { real(pd, &lin, &mut lp) };
    if res != VK_SUCCESS {
        LINEAR_MODIFIER_REFUSED.store(true, Ordering::Relaxed);
    }
    if !LOGGED_QUERY.swap(true, Ordering::Relaxed) {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: modifier query fmt={} usage={:#x} flags={:#x} driver={res} features={features:#x} linear={lres}",
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
///
/// The memory is imported here, not at `vkAllocateMemory`: whether the driver
/// takes the pool buffer as a dma-buf is only known once it tries, and an image
/// for the `AHardwareBuffer` route has to be created differently. The dma-buf
/// route is tried first; once it is refused, later images take the other.
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
    let list = unsafe {
        find_in(
            ci.pNext,
            VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
        )
    } as *const VkImageDrmFormatModifierListCreateInfoEXT;
    let ext = unsafe {
        find_in(
            ci.pNext,
            VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        )
    } as *const VkExternalMemoryImageCreateInfo;
    if list.is_null() || ext.is_null() {
        return None;
    }
    let mods = unsafe {
        std::slice::from_raw_parts(
            (*list).pDrmFormatModifiers,
            (*list).drmFormatModifierCount as usize,
        )
    };
    if !mods.contains(&MOD_LINEAR) {
        return None;
    }
    // GNDRM_EXPORT_AHB=1 skips the dma-buf route for the whole process: the
    // A/B for a driver whose dma-buf import succeeds but scans out wrong.
    if DMABUF_IMPORT.load(Ordering::Relaxed) == DMABUF_UNKNOWN
        && std::env::var_os("GNDRM_EXPORT_AHB").is_some_and(|v| v == "1")
    {
        DMABUF_IMPORT.store(DMABUF_REFUSED, Ordering::Relaxed);
        crate::logf!(gndrm_obf::obf!(
            "export-emu: GNDRM_EXPORT_AHB=1, importing pool buffers as AHardwareBuffers from the start"
        ));
    }
    if DMABUF_IMPORT.load(Ordering::Relaxed) == DMABUF_UNKNOWN
        && LINEAR_MODIFIER_REFUSED.load(Ordering::Relaxed)
        && std::env::var_os("GNDRM_EXPORT_AHB").is_none_or(|v| v != "0")
    {
        DMABUF_IMPORT.store(DMABUF_REFUSED, Ordering::Relaxed);
        crate::logf!(gndrm_obf::obf!(
            "export-emu: the driver refuses LINEAR DRM-modifier images; importing pool buffers as AHardwareBuffers from the start (GNDRM_EXPORT_AHB=0 forces the dma-buf route)"
        ));
    }
    let route = DMABUF_IMPORT.load(Ordering::Relaxed);
    let (w, h) = (ci.extent.width, ci.extent.height);
    let buf = if route == DMABUF_WORKS {
        None
    } else {
        crate::scanout::allocate_ahb(w, h)
    }
    .or_else(|| crate::scanout::allocate(w, h));
    let Some(buf) = buf else {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: pool allocation {w}x{h} refused; driver create"
        ));
        return None;
    };
    let fl = unsafe { find_in(ci.pNext, STYPE_IMAGE_FORMAT_LIST_CREATE_INFO) }
        as *const ImageFormatListCreateInfo;
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
    let mut refused = None;
    if route != DMABUF_REFUSED {
        match unsafe { create_dmabuf_backed(gd, ci, &buf, fl_ptr, alloc) } {
            Ok((image, mem, linear_tiling)) => {
                DMABUF_IMPORT.store(DMABUF_WORKS, Ordering::Relaxed);
                unsafe { crate::scanout::ahb_release(buf.ahb) };
                unsafe { *out = image };
                log_image(
                    ci,
                    &buf,
                    image,
                    if linear_tiling {
                        "linear-tiling"
                    } else {
                        "explicit-modifier"
                    },
                );
                register_image(EmuImage {
                    image,
                    fd: buf.fd,
                    stride: buf.stride,
                    height: h,
                    linear_tiling,
                    mem,
                });
                return Some(VK_SUCCESS);
            }
            Err(r) => refused = Some(r),
        }
    }
    if buf.ahb.is_null() {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: no AHardwareBuffer from the daemon for {w}x{h}; nothing to fall back to"
        ));
        close_fd(buf.fd);
        return Some(refused.unwrap_or(VK_ERROR_INITIALIZATION_FAILED));
    }
    if let Some(r) = refused {
        DMABUF_IMPORT.store(DMABUF_REFUSED, Ordering::Relaxed);
        crate::logf!(gndrm_obf::obf!(
            "export-emu: dma-buf import failed ({r}), importing pool buffer as AHardwareBuffer"
        ));
    }
    let res = unsafe { create_ahb_backed(gd, ci, &buf, fl_ptr, alloc) };
    unsafe { crate::scanout::ahb_release(buf.ahb) };
    match res {
        Ok((image, mem)) => {
            unsafe { *out = image };
            log_image(ci, &buf, image, "ahb");
            register_image(EmuImage {
                image,
                fd: buf.fd,
                stride: buf.stride,
                height: h,
                linear_tiling: false,
                mem,
            });
            Some(VK_SUCCESS)
        }
        Err(r) => {
            close_fd(buf.fd);
            Some(r)
        }
    }
}

/// Image create for gamescope's import of a client dma-buf (a guest frame
/// arriving over linux-dmabuf): external DMA_BUF memory without a modifier
/// list, which is the export shape [`try_create_image`] handles. `None` when
/// the request is not one, so the caller creates normally.
///
/// While the driver takes pool buffers as dma-bufs the image is created as
/// asked and only remembered, so a refusal at `vkAllocateMemory` can switch the
/// process to the other route. Once the driver has refused one, the image is
/// created with OPTIMAL tiling for an `AHardwareBuffer` import instead: its
/// memory is the same pool buffer, resolved through the daemon by the inode of
/// the fd gamescope imports.
pub unsafe fn try_create_import_image(
    gd: *mut crate::proxy::GnDevice,
    ci: *const VkImageCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkImage,
) -> Option<VkResult> {
    if !enabled() {
        return None;
    }
    let ci = unsafe { &*ci };
    if !four_bpp(ci.format) {
        return None;
    }
    let ext = unsafe {
        find_in(
            ci.pNext,
            VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        )
    } as *const VkExternalMemoryImageCreateInfo;
    if ext.is_null()
        || unsafe { (*ext).handleTypes } & VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT == 0
    {
        return None;
    }
    let list = unsafe {
        find_in(
            ci.pNext,
            VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
        )
    };
    if !list.is_null() {
        return None;
    }
    let explicit = unsafe {
        find_in(
            ci.pNext,
            VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
        )
    } as *const VkImageDrmFormatModifierExplicitCreateInfoEXT;
    if !explicit.is_null()
        && (unsafe { (*explicit).drmFormatModifier } != MOD_LINEAR
            || unsafe { (*explicit).drmFormatModifierPlaneCount } != 1)
    {
        return None;
    }
    let create = unsafe { (*gd).f.create_image }?;
    let dev = unsafe { (*gd).real };
    if DMABUF_IMPORT.load(Ordering::Relaxed) != DMABUF_REFUSED {
        let res = unsafe { create(dev, ci, alloc, out) };
        if res == VK_SUCCESS {
            note_import(unsafe { *out }, false);
        }
        return Some(res);
    }
    use crate::proxy::VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID;
    let fl = unsafe { find_in(ci.pNext, STYPE_IMAGE_FORMAT_LIST_CREATE_INFO) }
        as *const ImageFormatListCreateInfo;
    let fl_copy = (!fl.is_null()).then(|| ImageFormatListCreateInfo {
        sType: STYPE_IMAGE_FORMAT_LIST_CREATE_INFO,
        pNext: ptr::null(),
        viewFormatCount: unsafe { (*fl).viewFormatCount },
        pViewFormats: unsafe { (*fl).pViewFormats },
    });
    let ext_img = VkExternalMemoryImageCreateInfo {
        sType: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        pNext: fl_copy
            .as_ref()
            .map_or(ptr::null(), |f| f as *const _ as *const c_void),
        handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID,
    };
    let ci2 = VkImageCreateInfo {
        pNext: &ext_img as *const _ as *const c_void,
        tiling: VK_IMAGE_TILING_OPTIMAL,
        ..*ci
    };
    let res = unsafe { create(dev, &ci2, alloc, out) };
    if res == VK_SUCCESS {
        note_import(unsafe { *out }, true);
    }
    if IMPORT_LOGS.fetch_add(1, Ordering::Relaxed) < IMPORT_LOG_FIRST_N {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: import image {:#x} {}x{} fmt={} usage={:#x} created for the AHardwareBuffer route: {res}",
            if res == VK_SUCCESS { unsafe { *out } } else { 0 },
            ci.extent.width,
            ci.extent.height,
            ci.format,
            ci.usage
        ));
    }
    Some(res)
}

/// `vkAllocateMemory` for an image [`try_create_import_image`] remembered.
/// `None` when the allocation is not a dma-buf import into one.
unsafe fn try_allocate_import(
    gd: *mut crate::proxy::GnDevice,
    real: PFN_vkAllocateMemory,
    ai: *const VkMemoryAllocateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkDeviceMemory,
) -> Option<VkResult> {
    let imp = unsafe { find_in((*ai).pNext, VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR) }
        as *const VkImportMemoryFdInfoKHR;
    if imp.is_null()
        || unsafe { (*imp).handleType } != VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT
    {
        return None;
    }
    let ded = unsafe {
        find_in(
            (*ai).pNext,
            VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        )
    } as *const VkMemoryDedicatedAllocateInfo;
    if ded.is_null() {
        return None;
    }
    let image = unsafe { (*ded).image };
    let ahb_route = import_route(image)?;
    let dev = unsafe { (*gd).real };
    let fd = unsafe { (*imp).fd };
    if !ahb_route {
        let res = unsafe { real(dev, ai, alloc, out) };
        if res == VK_ERROR_OUT_OF_DEVICE_MEMORY || res == VK_ERROR_INVALID_EXTERNAL_HANDLE {
            let ino = fd_ino(fd);
            if let Some(buf) = crate::scanout::lookup_ahb(ino) {
                close_fd(buf.fd);
                unsafe { crate::scanout::ahb_release(buf.ahb) };
                DMABUF_IMPORT.store(DMABUF_REFUSED, Ordering::Relaxed);
                crate::logf!(gndrm_obf::obf!(
                    "export-emu: dma-buf import of pool buffer ino={ino} for image {:#x} failed ({res}); later imports take the AHardwareBuffer route",
                    image
                ));
            }
        }
        return Some(res);
    }
    let ino = fd_ino(fd);
    let Some(buf) = crate::scanout::lookup_ahb(ino) else {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: import ino={ino} for image {:#x}: no pool AHardwareBuffer from the daemon",
            image
        ));
        return Some(VK_ERROR_INVALID_EXTERNAL_HANDLE);
    };
    close_fd(buf.fd);
    let res = unsafe { import_ahb_dedicated(gd, image, buf.ahb, fd, ino, out) };
    unsafe { crate::scanout::ahb_release(buf.ahb) };
    if res == VK_SUCCESS {
        // A successful import owns the descriptor it was given; the
        // `AHardwareBuffer` import does not take it, so it is closed here.
        close_fd(fd);
    }
    Some(res)
}

/// Import `ahb` as the dedicated memory of `image`, created with the
/// `AHardwareBuffer` handle type.
unsafe fn import_ahb_dedicated(
    gd: *mut crate::proxy::GnDevice,
    image: VkImage,
    ahb: *mut c_void,
    fd: i32,
    ino: u64,
    out: *mut VkDeviceMemory,
) -> VkResult {
    use crate::proxy::{
        AhbFormatProperties, AhbProperties, ImportAhb, STYPE_AHB_FORMAT_PROPERTIES,
        STYPE_AHB_PROPERTIES,
    };
    type PropsPfn = unsafe extern "C" fn(VkDevice, *const c_void, *mut AhbProperties) -> VkResult;
    let Some(alloc_mem) = (unsafe { (*gd).f.alloc_mem }) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let dev = unsafe { (*gd).real };
    let get_props: Option<PropsPfn> = unsafe {
        std::mem::transmute(gn_real_device_proc(
            gd,
            c"vkGetAndroidHardwareBufferPropertiesANDROID",
        ))
    };
    let Some(get_props) = get_props else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let mut fmt = AhbFormatProperties {
        s_type: STYPE_AHB_FORMAT_PROPERTIES,
        next: ptr::null_mut(),
        format: 0,
        external_format: 0,
        format_features: 0,
        components: [0; 4],
        ycbcr: [0; 4],
    };
    let mut props = AhbProperties {
        s_type: STYPE_AHB_PROPERTIES,
        next: (&raw mut fmt).cast(),
        allocation_size: 0,
        memory_type_bits: 0,
    };
    let res = unsafe { get_props(dev, ahb, &mut props) };
    if res != VK_SUCCESS || props.memory_type_bits == 0 {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: import ino={ino} AHB properties {res} memTypeBits={:#x}",
            props.memory_type_bits
        ));
        return if res != VK_SUCCESS {
            res
        } else {
            VK_ERROR_INVALID_EXTERNAL_HANDLE
        };
    }
    let img_bits = unsafe { image_requirements(gd, image) }.map_or(u32::MAX, |r| r.memoryTypeBits);
    let types = unsafe { memory_types(gd) };
    let mem_type = pick_memory_type(types, props.memory_type_bits & img_bits)
        .or_else(|| pick_memory_type(types, props.memory_type_bits))
        .unwrap_or(0);
    let ded = VkMemoryDedicatedAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        pNext: ptr::null(),
        image,
        buffer: 0,
    };
    let imp = ImportAhb {
        s_type: STYPE_IMPORT_AHB,
        next: &ded as *const _ as *const c_void,
        buffer: ahb,
    };
    let ai = VkMemoryAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        pNext: &imp as *const _ as *const c_void,
        allocationSize: props.allocation_size,
        memoryTypeIndex: mem_type,
    };
    let res = unsafe { alloc_mem(dev, &ai, ptr::null(), out) };
    if IMPORT_LOGS.fetch_add(1, Ordering::Relaxed) < IMPORT_LOG_FIRST_N || res != VK_SUCCESS {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: import ino={ino} as AHardwareBuffer for image {:#x} size={} type={} format={} fd link={} -> {res}",
            image,
            props.allocation_size,
            mem_type,
            fmt.format,
            crate::scanout::fd_link(fd)
        ));
    }
    res
}

fn log_image(ci: &VkImageCreateInfo, buf: &crate::scanout::ScanoutBuf, image: VkImage, how: &str) {
    crate::logf!(gndrm_obf::obf!(
        "export-emu: image {:#x} {}x{} fmt={} usage={:#x} pool fd={} ino={} stride={} {how}",
        image,
        ci.extent.width,
        ci.extent.height,
        ci.format,
        ci.usage,
        buf.fd,
        buf.ino,
        buf.stride
    ));
}

/// The first memory type in `bits`, preferring a device-local one.
fn pick_memory_type(types: &[VkMemoryType], bits: u32) -> Option<u32> {
    let allowed = |i: usize| i < 32 && bits & (1 << i) != 0;
    types
        .iter()
        .enumerate()
        .find(|(i, t)| allowed(*i) && t.propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT != 0)
        .or_else(|| types.iter().enumerate().find(|(i, _)| allowed(*i)))
        .map(|(i, _)| i as u32)
}

unsafe fn memory_types<'a>(gd: *mut crate::proxy::GnDevice) -> &'a [VkMemoryType] {
    let mp = unsafe { &(*gd).mem_props };
    &mp.memoryTypes[..(mp.memoryTypeCount as usize).min(mp.memoryTypes.len())]
}

unsafe fn image_requirements(
    gd: *mut crate::proxy::GnDevice,
    image: VkImage,
) -> Option<VkMemoryRequirements> {
    let get = unsafe { (*gd).f.get_mem_reqs }?;
    let mut reqs = VkMemoryRequirements {
        size: 0,
        alignment: 0,
        memoryTypeBits: 0,
    };
    unsafe { get((*gd).real, image, &mut reqs) };
    Some(reqs)
}

unsafe fn destroy_real_image(
    gd: *mut crate::proxy::GnDevice,
    image: VkImage,
    alloc: *const VkAllocationCallbacks,
) {
    if let Some(destroy) = unsafe { (*gd).f.destroy_image } {
        unsafe { destroy((*gd).real, image, alloc) };
    }
}

/// The image against the pool stride with an explicit LINEAR modifier (or
/// LINEAR tiling when the driver refuses that), its memory the pool fd
/// imported as a dma-buf.
unsafe fn create_dmabuf_backed(
    gd: *mut crate::proxy::GnDevice,
    ci: &VkImageCreateInfo,
    buf: &crate::scanout::ScanoutBuf,
    fl_ptr: *const c_void,
    alloc: *const VkAllocationCallbacks,
) -> Result<(VkImage, VkDeviceMemory, bool), VkResult> {
    let create = unsafe { (*gd).f.create_image }.ok_or(VK_ERROR_INITIALIZATION_FAILED)?;
    let alloc_mem = unsafe { (*gd).f.alloc_mem }.ok_or(VK_ERROR_INITIALIZATION_FAILED)?;
    let dev = unsafe { (*gd).real };
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
    let mut image: VkImage = 0;
    let mut linear_tiling = false;
    let mut res = unsafe { create(dev, &ci2, alloc, &mut image) };
    if res != VK_SUCCESS {
        let r1 = res;
        ci2.pNext = &ext_img as *const _ as *const c_void;
        ci2.tiling = VK_IMAGE_TILING_LINEAR;
        res = unsafe { create(dev, &ci2, alloc, &mut image) };
        linear_tiling = true;
        crate::logf!(gndrm_obf::obf!(
            "export-emu: explicit LINEAR create {r1}; LINEAR tiling create {res}"
        ));
    }
    if res != VK_SUCCESS {
        return Err(res);
    }
    let fail = |r: VkResult| {
        unsafe { destroy_real_image(gd, image, alloc) };
        Err(r)
    };
    // The driver's own row pitch for the image. The explicit-modifier create
    // asks for the pool stride, but a driver may answer with a pitch of its own
    // (PowerVR packs the rows at the image width): the daemon scans the pool
    // buffer out at its gralloc stride, so a different pitch shears every row
    // and the dma-buf route is refused for the AHardwareBuffer one.
    if let Some(gsl) = unsafe { gn_real_device_proc(gd, c"vkGetImageSubresourceLayout") } {
        let gsl: PFN_vkGetImageSubresourceLayout = unsafe { std::mem::transmute(gsl) };
        // DRM-modifier images are queried by memory plane; a driver that only
        // answers the colour aspect reports a zero pitch for the plane aspect.
        let mut pitch = 0u64;
        let mut offset = 0u64;
        for aspect in if linear_tiling {
            [VK_IMAGE_ASPECT_COLOR_BIT, 0]
        } else {
            [VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT, VK_IMAGE_ASPECT_COLOR_BIT]
        } {
            if aspect == 0 {
                break;
            }
            let sub = VkImageSubresource {
                aspectMask: aspect,
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
            if l.rowPitch != 0 {
                pitch = l.rowPitch;
                offset = l.offset;
                break;
            }
        }
        crate::logf!(gndrm_obf::obf!(
            "export-emu: {} image {:#x} layout pitch={pitch} offset={offset} (pool stride {})",
            if linear_tiling { "LINEAR-tiling" } else { "explicit-LINEAR" },
            image,
            buf.stride
        ));
        if pitch != 0 && (pitch != u64::from(buf.stride) || offset != 0) {
            crate::logf!(gndrm_obf::obf!(
                "export-emu: driver pitch {pitch} != pool stride {}; the dma-buf route cannot scan out, trying AHardwareBuffer",
                buf.stride
            ));
            return fail(VK_ERROR_FORMAT_NOT_SUPPORTED);
        }
    }
    let Some(reqs) = (unsafe { image_requirements(gd, image) }) else {
        return fail(VK_ERROR_INITIALIZATION_FAILED);
    };
    let pool_size = unsafe { libc::lseek(buf.fd, 0, libc::SEEK_END) };
    if pool_size >= 0 {
        unsafe { libc::lseek(buf.fd, 0, libc::SEEK_SET) };
    }
    if pool_size >= 0 && (pool_size as u64) < reqs.size {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: pool fd for image {:#x} is {} bytes < size={}",
            image,
            pool_size,
            reqs.size
        ));
        return fail(VK_ERROR_INVALID_EXTERNAL_HANDLE);
    }
    let mut fd_bits = u32::MAX;
    if let Some(gfp) = unsafe { gn_real_device_proc(gd, c"vkGetMemoryFdPropertiesKHR") } {
        let gfp: PfnGetMemoryFdProperties = unsafe { std::mem::transmute(gfp) };
        let mut p = MemoryFdProperties {
            sType: STYPE_MEMORY_FD_PROPERTIES,
            pNext: ptr::null_mut(),
            memoryTypeBits: 0,
        };
        let r = unsafe {
            gfp(
                dev,
                VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                buf.fd,
                &mut p,
            )
        };
        if r == VK_SUCCESS && p.memoryTypeBits & reqs.memoryTypeBits != 0 {
            fd_bits = p.memoryTypeBits;
        } else {
            crate::logf!(gndrm_obf::obf!(
                "export-emu: dma-buf fd properties {r} memTypeBits {:#x} (image {:#x})",
                p.memoryTypeBits,
                reqs.memoryTypeBits
            ));
        }
    }
    let types = unsafe { memory_types(gd) };
    let Some(mem_type) = pick_memory_type(types, reqs.memoryTypeBits & fd_bits) else {
        return fail(VK_ERROR_OUT_OF_DEVICE_MEMORY);
    };
    let fd = unsafe { libc::fcntl(buf.fd, libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return fail(VK_ERROR_OUT_OF_HOST_MEMORY);
    }
    let ded = VkMemoryDedicatedAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        pNext: ptr::null(),
        image,
        buffer: 0,
    };
    let imp = VkImportMemoryFdInfoKHR {
        sType: VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
        pNext: &ded as *const _ as *const c_void,
        handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        fd,
    };
    let ai = VkMemoryAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        pNext: &imp as *const _ as *const c_void,
        allocationSize: reqs.size,
        memoryTypeIndex: mem_type,
    };
    let mut mem: VkDeviceMemory = 0;
    let res = unsafe { alloc_mem(dev, &ai, ptr::null(), &mut mem) };
    crate::logf!(gndrm_obf::obf!(
        "export-emu: import pool fd for image {:#x} size={} pool={} type={} bits={:#x}/{:#x} -> {res}",
        image,
        reqs.size,
        pool_size,
        mem_type,
        reqs.memoryTypeBits,
        fd_bits
    ));
    if res != VK_SUCCESS {
        unsafe { libc::close(fd) };
        return fail(res);
    }
    Ok((image, mem, linear_tiling))
}

/// The image with OPTIMAL tiling, its memory the pool buffer's
/// `AHardwareBuffer` imported through
/// `VK_ANDROID_external_memory_android_hardware_buffer`: the layout is the one
/// gralloc chose for the pool's CPU-readable usage, i.e. linear at the pool
/// stride, which is what the export answers describe.
unsafe fn create_ahb_backed(
    gd: *mut crate::proxy::GnDevice,
    ci: &VkImageCreateInfo,
    buf: &crate::scanout::ScanoutBuf,
    fl_ptr: *const c_void,
    alloc: *const VkAllocationCallbacks,
) -> Result<(VkImage, VkDeviceMemory), VkResult> {
    use crate::proxy::{
        AhbFormatProperties, AhbProperties, ImportAhb, STYPE_AHB_FORMAT_PROPERTIES,
        STYPE_AHB_PROPERTIES, VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID,
    };
    type PropsPfn = unsafe extern "C" fn(VkDevice, *const c_void, *mut AhbProperties) -> VkResult;
    let create = unsafe { (*gd).f.create_image }.ok_or(VK_ERROR_INITIALIZATION_FAILED)?;
    let alloc_mem = unsafe { (*gd).f.alloc_mem }.ok_or(VK_ERROR_INITIALIZATION_FAILED)?;
    let dev = unsafe { (*gd).real };
    let get_props: Option<PropsPfn> = unsafe {
        std::mem::transmute(gn_real_device_proc(
            gd,
            c"vkGetAndroidHardwareBufferPropertiesANDROID",
        ))
    };
    let Some(get_props) = get_props else {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: driver resolves no vkGetAndroidHardwareBufferPropertiesANDROID"
        ));
        return Err(VK_ERROR_INITIALIZATION_FAILED);
    };
    let mut fmt = AhbFormatProperties {
        s_type: STYPE_AHB_FORMAT_PROPERTIES,
        next: ptr::null_mut(),
        format: 0,
        external_format: 0,
        format_features: 0,
        components: [0; 4],
        ycbcr: [0; 4],
    };
    let mut props = AhbProperties {
        s_type: STYPE_AHB_PROPERTIES,
        next: (&raw mut fmt).cast(),
        allocation_size: 0,
        memory_type_bits: 0,
    };
    let res = unsafe { get_props(dev, buf.ahb, &mut props) };
    crate::logf!(gndrm_obf::obf!(
        "export-emu: AHB ino={} size={} memTypeBits={:#x} format={} external={:#x} (image fmt {}): {res}",
        buf.ino,
        props.allocation_size,
        props.memory_type_bits,
        fmt.format,
        fmt.external_format,
        ci.format
    ));
    if res != VK_SUCCESS {
        return Err(res);
    }
    if props.memory_type_bits == 0 {
        return Err(VK_ERROR_INVALID_EXTERNAL_HANDLE);
    }
    let ext_img = VkExternalMemoryImageCreateInfo {
        sType: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        pNext: fl_ptr,
        handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID,
    };
    let ci2 = VkImageCreateInfo {
        pNext: &ext_img as *const _ as *const c_void,
        tiling: VK_IMAGE_TILING_OPTIMAL,
        ..*ci
    };
    let mut image: VkImage = 0;
    let res = unsafe { create(dev, &ci2, alloc, &mut image) };
    if res != VK_SUCCESS {
        crate::logf!(gndrm_obf::obf!(
            "export-emu: AHB image create failed: {res}"
        ));
        return Err(res);
    }
    let img_bits = unsafe { image_requirements(gd, image) }.map_or(u32::MAX, |r| r.memoryTypeBits);
    let types = unsafe { memory_types(gd) };
    let mem_type = pick_memory_type(types, props.memory_type_bits & img_bits)
        .or_else(|| pick_memory_type(types, props.memory_type_bits))
        .unwrap_or(0);
    let ded = VkMemoryDedicatedAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        pNext: ptr::null(),
        image,
        buffer: 0,
    };
    let imp = ImportAhb {
        s_type: STYPE_IMPORT_AHB,
        next: &ded as *const _ as *const c_void,
        buffer: buf.ahb,
    };
    let ai = VkMemoryAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        pNext: &imp as *const _ as *const c_void,
        allocationSize: props.allocation_size,
        memoryTypeIndex: mem_type,
    };
    let mut mem: VkDeviceMemory = 0;
    let res = unsafe { alloc_mem(dev, &ai, ptr::null(), &mut mem) };
    crate::logf!(gndrm_obf::obf!(
        "export-emu: AHB import for image {:#x} size={} type={} -> {res}",
        image,
        props.allocation_size,
        mem_type
    ));
    if res != VK_SUCCESS {
        unsafe { destroy_real_image(gd, image, alloc) };
        return Err(res);
    }
    Ok((image, mem))
}

fn take_memory(image: VkImage) -> Option<VkDeviceMemory> {
    let mut v = IMAGES.lock().ok()?;
    let e = v.iter_mut().rfind(|e| e.image == image)?;
    let mem = std::mem::replace(&mut e.mem, 0);
    (mem != 0).then_some(mem)
}

pub unsafe extern "C" fn gn_vkAllocateMemory(
    device: VkDevice,
    ai: *const VkMemoryAllocateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkDeviceMemory,
) -> VkResult {
    if let Some(r) = unsafe { crate::placed::try_allocate(device, ai, alloc, out) } {
        return r;
    }
    let Some((dev, real)) =
        (unsafe { real_dev::<PFN_vkAllocateMemory>(device, c"vkAllocateMemory") })
    else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    if !enabled() {
        return unsafe { real(dev, ai, alloc, out) };
    }
    if let Some(r) = unsafe { try_allocate_import(find_device(device), real, ai, alloc, out) } {
        return r;
    }
    if unsafe { !find_in((*ai).pNext, VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR).is_null() }
        || unsafe { !find_in((*ai).pNext, STYPE_IMPORT_AHB).is_null() }
    {
        return unsafe { real(dev, ai, alloc, out) };
    }
    let ded = unsafe {
        find_in(
            (*ai).pNext,
            VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        )
    } as *const VkMemoryDedicatedAllocateInfo;
    let image = if ded.is_null() {
        0
    } else {
        unsafe { (*ded).image }
    };
    let Some(mem) = (image != 0).then(|| take_memory(image)).flatten() else {
        return unsafe { real(dev, ai, alloc, out) };
    };
    unsafe { *out = mem };
    if let Ok(mut m) = MEMS.lock() {
        m.push((mem, image));
    }
    VK_SUCCESS
}

pub unsafe extern "C" fn gn_vkDestroyImage(
    device: VkDevice,
    image: VkImage,
    alloc: *const VkAllocationCallbacks,
) {
    let Some((dev, real)) = (unsafe { real_dev::<PFN_vkDestroyImage>(device, c"vkDestroyImage") })
    else {
        return;
    };
    let mut unclaimed = 0;
    if enabled() && image != 0 {
        forget_import(image);
        if let Some((e, mems)) = forget_image(image) {
            crate::logf!(gndrm_obf::obf!(
                "export-emu: drop image {:#x} pool fd={} mems={mems}",
                e.image,
                e.fd
            ));
            unclaimed = e.mem;
        }
    }
    unsafe { real(dev, image, alloc) };
    if unclaimed != 0 {
        if let Some((dev, free)) = unsafe { real_dev::<PFN_vkFreeMemory>(device, c"vkFreeMemory") }
        {
            unsafe { free(dev, unclaimed, ptr::null()) };
        }
    }
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
        unsafe {
            (*(props as *mut ImageDrmFormatModifierProperties)).drmFormatModifier = MOD_LINEAR
        };
        return VK_SUCCESS;
    }
    let Some((dev, real)) =
        (unsafe { real_dev::<Pfn>(device, c"vkGetImageDrmFormatModifierPropertiesEXT") })
    else {
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
    let Some((dev, real)) = (unsafe {
        real_dev::<PFN_vkGetImageSubresourceLayout>(device, c"vkGetImageSubresourceLayout")
    }) else {
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
    let ws = unsafe {
        std::slice::from_raw_parts(writes as *const WriteDescriptorSet, write_count as usize)
    };
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
    unsafe {
        real(
            dev,
            out.len() as u32,
            out.as_ptr() as *const c_void,
            copy_count,
            copies,
        )
    }
}
