//! `VK_EXT_map_memory_placed` emulation for a system driver that lacks it.
//!
//! wow64 winevulkan needs host mappings below 4 GiB and asks for them through
//! placed maps. The driver's own mappings cannot be moved (kbase refuses fixed
//! addresses), so, as the X11 wrapper ICD does, host-visible memory of a
//! device that enabled placed maps is backed by an AHardwareBuffer BLOB
//! imported into the driver, and a placed map is an `mmap` of that buffer's
//! fd at the placed address.

#![allow(non_snake_case)]

use crate::proxy::{find_device, find_instance_by_pd, gn_real_device_proc, gn_real_inst_proc};
use crate::vk::*;
use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

pub const EXT_NAME: &CStr = c"VK_EXT_map_memory_placed";
pub const STYPE_FEATURES: VkStructureType = 1000272000;
const STYPE_PROPERTIES: VkStructureType = 1000272001;
const STYPE_MAP_PLACED_INFO: VkStructureType = 1000272002;
const MAP_PLACED_BIT: u32 = 0x1;
const UNMAP_RESERVE_BIT: u32 = 0x1;
const VK_ERROR_MEMORY_MAP_FAILED: VkResult = -5;

static ENABLED: AtomicBool = AtomicBool::new(false);
static FAKED: AtomicBool = AtomicBool::new(false);
static REQUESTED: AtomicBool = AtomicBool::new(false);
static LOG_N: AtomicUsize = AtomicUsize::new(0);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Set once the extension has been advertised in place of the driver.
pub fn note_faked() {
    FAKED.store(true, Ordering::Relaxed);
}

pub fn faked() -> bool {
    FAKED.load(Ordering::Relaxed)
}

/// Set when a device is created with the (emulated) extension enabled.
pub fn note_requested() {
    REQUESTED.store(true, Ordering::Relaxed);
}

#[repr(C)]
struct BaseOut {
    sType: VkStructureType,
    pNext: *mut c_void,
}

#[repr(C)]
struct PlacedFeatures {
    sType: VkStructureType,
    pNext: *mut c_void,
    memoryMapPlaced: u32,
    memoryMapRangePlaced: u32,
    memoryUnmapReserve: u32,
}

#[repr(C)]
struct PlacedProperties {
    sType: VkStructureType,
    pNext: *mut c_void,
    minPlacedMemoryMapAlignment: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MemoryMapInfo {
    sType: VkStructureType,
    pNext: *const c_void,
    flags: u32,
    memory: VkDeviceMemory,
    offset: u64,
    size: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MemoryUnmapInfo {
    sType: VkStructureType,
    pNext: *const c_void,
    flags: u32,
    memory: VkDeviceMemory,
}

#[repr(C)]
struct MemoryMapPlacedInfo {
    sType: VkStructureType,
    pNext: *const c_void,
    pPlacedAddress: *mut c_void,
}

unsafe fn find_out(mut p: *mut c_void, stype: VkStructureType) -> *mut c_void {
    while !p.is_null() {
        let b = p as *mut BaseOut;
        if unsafe { (*b).sType } == stype {
            return p;
        }
        p = unsafe { (*b).pNext };
    }
    ptr::null_mut()
}

fn page_size() -> u64 {
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p > 0 {
        p as u64
    } else {
        4096
    }
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

unsafe fn real_pd<T>(pd: VkPhysicalDevice, names: &[&CStr]) -> Option<T> {
    let gi = unsafe { find_instance_by_pd(pd) };
    if gi.is_null() {
        return None;
    }
    for n in names {
        if let Some(f) = unsafe { gn_real_inst_proc((*gi).real, n) } {
            return Some(unsafe { std::mem::transmute_copy(&f) });
        }
    }
    None
}

pub unsafe extern "C" fn gn_vkGetPhysicalDeviceFeatures2(pd: VkPhysicalDevice, f: *mut c_void) {
    type Pfn = unsafe extern "C" fn(VkPhysicalDevice, *mut c_void);
    let Some(real) = (unsafe {
        real_pd::<Pfn>(
            pd,
            &[
                c"vkGetPhysicalDeviceFeatures2",
                c"vkGetPhysicalDeviceFeatures2KHR",
            ],
        )
    }) else {
        return;
    };
    unsafe { real(pd, f) };
    unsafe { crate::divisor::features2(f) };
    if !enabled() || f.is_null() {
        return;
    }
    let pf =
        unsafe { find_out((*(f as *mut BaseOut)).pNext, STYPE_FEATURES) } as *mut PlacedFeatures;
    if !pf.is_null() {
        unsafe {
            (*pf).memoryMapPlaced = 1;
            (*pf).memoryMapRangePlaced = 0;
            (*pf).memoryUnmapReserve = 1;
        }
    }
}

pub unsafe extern "C" fn gn_vkGetPhysicalDeviceProperties2(pd: VkPhysicalDevice, p: *mut c_void) {
    type Pfn = unsafe extern "C" fn(VkPhysicalDevice, *mut c_void);
    let Some(real) = (unsafe {
        real_pd::<Pfn>(
            pd,
            &[
                c"vkGetPhysicalDeviceProperties2",
                c"vkGetPhysicalDeviceProperties2KHR",
            ],
        )
    }) else {
        return;
    };
    unsafe { crate::divisor::properties2(p, &|q| real(pd, q)) };
    if !enabled() || p.is_null() {
        return;
    }
    let pp = unsafe { find_out((*(p as *mut BaseOut)).pNext, STYPE_PROPERTIES) }
        as *mut PlacedProperties;
    if !pp.is_null() {
        unsafe { (*pp).minPlacedMemoryMapAlignment = page_size() };
    }
}

const STYPE_AHB_PROPERTIES: VkStructureType = 1000129000;
const STYPE_IMPORT_AHB: VkStructureType = 1000129003;
const STYPE_IMPORT_HOST_POINTER: VkStructureType = 1000178000;
const AHB_FORMAT_BLOB: u32 = 0x21;
const AHB_USAGE: u64 = 0x3 | 0x30 | (1 << 24);
const MEMORY_PROPERTY_HOST_VISIBLE: u32 = 0x2;
const WHOLE_SIZE: u64 = u64::MAX;

#[repr(C)]
#[derive(Default)]
struct AhbDesc {
    width: u32,
    height: u32,
    layers: u32,
    format: u32,
    usage: u64,
    stride: u32,
    rfu0: u32,
    rfu1: u64,
}

#[repr(C)]
struct NativeHandle {
    version: i32,
    num_fds: i32,
    num_ints: i32,
    data: [i32; 0],
}

#[repr(C)]
struct AhbProperties {
    sType: VkStructureType,
    pNext: *mut c_void,
    allocationSize: u64,
    memoryTypeBits: u32,
}

#[repr(C)]
struct ImportAhb {
    sType: VkStructureType,
    pNext: *const c_void,
    buffer: *mut c_void,
}

struct AhbApi {
    allocate: unsafe extern "C" fn(*const AhbDesc, *mut *mut c_void) -> i32,
    release: unsafe extern "C" fn(*mut c_void),
    native_handle: unsafe extern "C" fn(*const c_void) -> *const NativeHandle,
}

fn ahb_api() -> Option<&'static AhbApi> {
    static API: std::sync::OnceLock<Option<AhbApi>> = std::sync::OnceLock::new();
    API.get_or_init(|| unsafe {
        let h = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if h.is_null() {
            return None;
        }
        let a = libc::dlsym(h, c"AHardwareBuffer_allocate".as_ptr());
        let r = libc::dlsym(h, c"AHardwareBuffer_release".as_ptr());
        let n = libc::dlsym(h, c"AHardwareBuffer_getNativeHandle".as_ptr());
        if a.is_null() || r.is_null() || n.is_null() {
            return None;
        }
        Some(AhbApi {
            allocate: std::mem::transmute(a),
            release: std::mem::transmute(r),
            native_handle: std::mem::transmute(n),
        })
    })
    .as_ref()
}

struct Backed {
    mem: VkDeviceMemory,
    ahb: *mut c_void,
    fd: i32,
    size: u64,
    mapped: usize,
    mapped_len: usize,
}

unsafe impl Send for Backed {}

static BACKED: Mutex<Vec<Backed>> = Mutex::new(Vec::new());

unsafe fn find_in(mut p: *const c_void, stype: VkStructureType) -> *const c_void {
    while !p.is_null() {
        let b = p as *const BaseOut;
        if unsafe { (*b).sType } == stype {
            return p;
        }
        p = unsafe { (*b).pNext } as *const c_void;
    }
    ptr::null()
}

fn round_page(v: u64) -> u64 {
    let p = page_size();
    v.div_ceil(p) * p
}

/// Host-visible allocation on a placed-map device, backed by an AHB. `None`
/// when the request is not one this backs, so the caller allocates normally.
pub unsafe fn try_allocate(
    device: VkDevice,
    ai: *const VkMemoryAllocateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkDeviceMemory,
) -> Option<VkResult> {
    if !enabled() || !REQUESTED.load(Ordering::Relaxed) {
        return None;
    }
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return None;
    }
    let ai_ref = unsafe { &*ai };
    let mp = unsafe { &(*gd).mem_props };
    let ty = ai_ref.memoryTypeIndex as usize;
    if ty >= mp.memoryTypeCount as usize
        || mp.memoryTypes[ty].propertyFlags & MEMORY_PROPERTY_HOST_VISIBLE == 0
    {
        return None;
    }
    unsafe {
        let ded = find_in(
            ai_ref.pNext,
            VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        ) as *const VkMemoryDedicatedAllocateInfo;
        if (!ded.is_null() && (*ded).image != 0)
            || !find_in(ai_ref.pNext, VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO).is_null()
            || !find_in(ai_ref.pNext, VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR).is_null()
            || !find_in(ai_ref.pNext, STYPE_IMPORT_AHB).is_null()
            || !find_in(ai_ref.pNext, STYPE_IMPORT_HOST_POINTER).is_null()
        {
            return None;
        }
    }
    let api = ahb_api()?;
    type PropsPfn = unsafe extern "C" fn(VkDevice, *const c_void, *mut AhbProperties) -> VkResult;
    let get_props: PropsPfn = unsafe {
        std::mem::transmute(gn_real_device_proc(
            gd,
            c"vkGetAndroidHardwareBufferPropertiesANDROID",
        )?)
    };
    let alloc_mem = unsafe { (*gd).f.alloc_mem }?;
    let dev = unsafe { (*gd).real };
    let size = round_page(ai_ref.allocationSize.max(1));
    let desc = AhbDesc {
        width: size as u32,
        height: 1,
        layers: 1,
        format: AHB_FORMAT_BLOB,
        usage: AHB_USAGE,
        ..Default::default()
    };
    let mut ahb: *mut c_void = ptr::null_mut();
    if size > u64::from(u32::MAX)
        || unsafe { (api.allocate)(&desc, &mut ahb) } != 0
        || ahb.is_null()
    {
        crate::logf!(gndrm_obf::obf!(
            "placed: AHB allocation of {size} bytes failed"
        ));
        return None;
    }
    let mut props = AhbProperties {
        sType: STYPE_AHB_PROPERTIES,
        pNext: ptr::null_mut(),
        allocationSize: 0,
        memoryTypeBits: 0,
    };
    let pres = unsafe { get_props(dev, ahb, &mut props) };
    let chosen = if props.memoryTypeBits & (1 << ty) != 0 {
        Some(ty as u32)
    } else {
        (0..mp.memoryTypeCount).find(|&i| {
            props.memoryTypeBits & (1 << i) != 0
                && mp.memoryTypes[i as usize].propertyFlags & MEMORY_PROPERTY_HOST_VISIBLE != 0
        })
    };
    let nh = unsafe { (api.native_handle)(ahb) };
    let fd = if nh.is_null() || unsafe { (*nh).num_fds } < 1 {
        -1
    } else {
        unsafe { *(*nh).data.as_ptr() }
    };
    let (Some(chosen), VK_SUCCESS, true) = (chosen, pres, fd >= 0) else {
        crate::logf!(gndrm_obf::obf!(
            "placed: AHB unusable (props {pres}, types {:#x}, fd {fd}) for type {ty}",
            props.memoryTypeBits
        ));
        unsafe { (api.release)(ahb) };
        return None;
    };
    let imp = ImportAhb {
        sType: STYPE_IMPORT_AHB,
        pNext: ai_ref.pNext,
        buffer: ahb,
    };
    let ai2 = VkMemoryAllocateInfo {
        sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        pNext: &imp as *const _ as *const c_void,
        allocationSize: props.allocationSize,
        memoryTypeIndex: chosen,
    };
    let res = unsafe { alloc_mem(dev, &ai2, alloc, out) };
    if LOG_N.fetch_add(1, Ordering::Relaxed) < 4 {
        crate::logf!(gndrm_obf::obf!(
            "placed: AHB-backed alloc {} -> {} bytes type {ty}->{chosen} fd {fd}: {res}",
            ai_ref.allocationSize,
            props.allocationSize
        ));
    }
    if res != VK_SUCCESS {
        unsafe { (api.release)(ahb) };
        return None;
    }
    if let Ok(mut v) = BACKED.lock() {
        v.push(Backed {
            mem: unsafe { *out },
            ahb,
            fd,
            size: props.allocationSize,
            mapped: 0,
            mapped_len: 0,
        });
    }
    Some(VK_SUCCESS)
}

/// Drop a backed allocation's CPU mapping. `true` when `mem` is backed.
unsafe fn unmap_backed(mem: VkDeviceMemory, keep_reserved: bool) -> bool {
    let Ok(mut v) = BACKED.lock() else {
        return false;
    };
    let Some(b) = v.iter_mut().find(|b| b.mem == mem) else {
        return false;
    };
    if b.mapped != 0 {
        unsafe {
            if keep_reserved {
                libc::mmap(
                    b.mapped as *mut c_void,
                    b.mapped_len,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_FIXED,
                    -1,
                    0,
                );
            } else {
                libc::munmap(b.mapped as *mut c_void, b.mapped_len);
            }
        }
        b.mapped = 0;
        b.mapped_len = 0;
    }
    true
}

pub unsafe extern "C" fn gn_vkMapMemory2KHR(
    device: VkDevice,
    info: *const c_void,
    pp: *mut *mut c_void,
) -> VkResult {
    type Pfn = unsafe extern "C" fn(VkDevice, *const c_void, *mut *mut c_void) -> VkResult;
    let Some((dev, real)) = (unsafe { real_dev::<Pfn>(device, c"vkMapMemory2KHR") }) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let mi = unsafe { *(info as *const MemoryMapInfo) };
    let placed = unsafe { find_in(mi.pNext, STYPE_MAP_PLACED_INFO) } as *const MemoryMapPlacedInfo;
    if !enabled() || mi.flags & MAP_PLACED_BIT == 0 || placed.is_null() {
        return unsafe { real(dev, info, pp) };
    }
    let want = unsafe { (*placed).pPlacedAddress };
    let Ok(mut v) = BACKED.lock() else {
        return VK_ERROR_MEMORY_MAP_FAILED;
    };
    let Some(b) = v.iter_mut().find(|b| b.mem == mi.memory) else {
        drop(v);
        crate::logf!(gndrm_obf::obf!(
            "placed: mem {:#x} is not AHB-backed; placed map refused",
            mi.memory
        ));
        return VK_ERROR_MEMORY_MAP_FAILED;
    };
    let size = if mi.size == WHOLE_SIZE {
        b.size - mi.offset
    } else {
        mi.size
    };
    let len = round_page(size) as usize;
    let r = unsafe {
        libc::mmap(
            want,
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_FIXED,
            b.fd,
            mi.offset as libc::off_t,
        )
    };
    if r != want {
        crate::logf!(gndrm_obf::obf!(
            "placed: mmap of AHB fd {} at {want:p} failed: {}",
            b.fd,
            std::io::Error::last_os_error()
        ));
        return VK_ERROR_MEMORY_MAP_FAILED;
    }
    b.mapped = want as usize;
    b.mapped_len = len;
    unsafe { *pp = want };
    VK_SUCCESS
}

pub unsafe extern "C" fn gn_vkUnmapMemory2KHR(device: VkDevice, info: *const c_void) -> VkResult {
    type Pfn = unsafe extern "C" fn(VkDevice, *const c_void) -> VkResult;
    let Some((dev, real)) = (unsafe { real_dev::<Pfn>(device, c"vkUnmapMemory2KHR") }) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let mut ui = unsafe { *(info as *const MemoryUnmapInfo) };
    if enabled() && unsafe { unmap_backed(ui.memory, ui.flags & UNMAP_RESERVE_BIT != 0) } {
        return VK_SUCCESS;
    }
    ui.flags &= !UNMAP_RESERVE_BIT;
    unsafe { real(dev, &ui as *const _ as *const c_void) }
}

pub unsafe extern "C" fn gn_vkUnmapMemory(device: VkDevice, mem: VkDeviceMemory) {
    let Some((dev, real)) = (unsafe { real_dev::<PFN_vkUnmapMemory>(device, c"vkUnmapMemory") })
    else {
        return;
    };
    if enabled() && unsafe { unmap_backed(mem, false) } {
        return;
    }
    unsafe { real(dev, mem) }
}

pub unsafe extern "C" fn gn_vkFreeMemory(
    device: VkDevice,
    mem: VkDeviceMemory,
    alloc: *const VkAllocationCallbacks,
) {
    let Some((dev, real)) = (unsafe { real_dev::<PFN_vkFreeMemory>(device, c"vkFreeMemory") })
    else {
        return;
    };
    let backed = if enabled() && unsafe { unmap_backed(mem, false) } {
        BACKED.lock().ok().and_then(|mut v| {
            let i = v.iter().position(|b| b.mem == mem)?;
            Some(v.swap_remove(i))
        })
    } else {
        None
    };
    unsafe { real(dev, mem, alloc) };
    if let (Some(b), Some(api)) = (backed, ahb_api()) {
        unsafe { (api.release)(b.ahb) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static SERIAL: Mutex<()> = Mutex::new(());

    #[repr(C)]
    struct Link {
        s_type: VkStructureType,
        p_next: *mut c_void,
        tag: u32,
    }

    fn link(s_type: VkStructureType, tag: u32, next: *mut c_void) -> Link {
        Link {
            s_type,
            p_next: next,
            tag,
        }
    }

    #[test]
    fn find_out_walks_the_chain() {
        let mut tail = link(STYPE_PROPERTIES, 2, ptr::null_mut());
        let mut mid = link(1000272003, 1, (&raw mut tail).cast());
        let mut head = link(STYPE_FEATURES, 0, (&raw mut mid).cast());

        assert_eq!(
            unsafe { find_out((&raw mut head).cast(), 1000272003) },
            (&raw mut mid).cast::<c_void>()
        );
        assert_eq!(
            unsafe { find_out((&raw mut head).cast(), STYPE_PROPERTIES) },
            (&raw mut tail).cast::<c_void>()
        );
        assert!(unsafe { find_out((&raw mut head).cast(), 999) }.is_null());
    }

    #[test]
    fn find_in_walks_the_chain() {
        let tail = link(STYPE_MAP_PLACED_INFO, 2, ptr::null_mut());
        let head = link(
            STYPE_FEATURES,
            0,
            (&raw const tail).cast::<c_void>() as *mut c_void,
        );
        assert_eq!(
            unsafe { find_in((&raw const head).cast(), STYPE_MAP_PLACED_INFO) },
            (&raw const tail).cast::<c_void>()
        );
        assert!(unsafe { find_in((&raw const head).cast(), 999) }.is_null());
        assert!(unsafe { find_in(ptr::null(), STYPE_FEATURES) }.is_null());
    }

    #[test]
    fn a_page_is_a_page() {
        let p = page_size();
        assert!(p >= 4096, "page size {p} is implausible");
        assert_eq!(round_page(0), 0);
        assert_eq!(round_page(1), p);
        assert_eq!(round_page(p), p);
        assert_eq!(round_page(p + 1), 2 * p);
    }

    #[test]
    fn the_emulation_flags_start_off_and_follow_their_setters() {
        let _g = SERIAL.lock().unwrap();
        set_enabled(false);
        assert!(!enabled());
        set_enabled(true);
        assert!(enabled());
        set_enabled(false);
        assert!(!enabled());
    }

    #[test]
    fn the_stypes_are_the_extension_s_own() {
        assert_eq!(STYPE_FEATURES, 1000272000);
        // The map-placed bit is what a caller asks for; the extension name is
        // the one advertised in place of the driver's.
        assert_eq!(EXT_NAME.to_str().unwrap(), "VK_EXT_map_memory_placed");
    }
}
