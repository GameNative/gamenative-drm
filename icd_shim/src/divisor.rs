//! `VK_EXT_vertex_attribute_divisor` on a system driver that only lists the
//! promoted `VK_KHR_vertex_attribute_divisor` (Mali-G710). vkd3d-proton checks
//! the EXT name only. The features and pipeline structs share their sTypes with
//! the KHR ones; the EXT properties struct is answered from the KHR one, and the
//! device is created with the KHR name.

use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};

pub const EXT_NAME: &CStr = c"VK_EXT_vertex_attribute_divisor";
pub const KHR_NAME: &CStr = c"VK_KHR_vertex_attribute_divisor";
const STYPE_EXT_PROPERTIES: i32 = 1000190000;
const STYPE_KHR_PROPERTIES: i32 = 1000525000;

static FAKED: AtomicBool = AtomicBool::new(false);

pub fn note_faked() {
    if !FAKED.swap(true, Ordering::Relaxed) {
        crate::logf!(gndrm_obf::obf!(
            "proxy: advertised VK_EXT_vertex_attribute_divisor over the KHR extension"
        ));
    }
}

pub fn faked() -> bool {
    FAKED.load(Ordering::Relaxed)
}

#[repr(C)]
struct BaseOut {
    s_type: i32,
    p_next: *mut c_void,
}

#[repr(C)]
struct ExtProperties {
    s_type: i32,
    p_next: *mut c_void,
    max_vertex_attrib_divisor: u32,
}

#[repr(C)]
struct KhrProperties {
    s_type: i32,
    p_next: *mut c_void,
    max_vertex_attrib_divisor: u32,
    supports_non_zero_first_instance: u32,
}

/// `vkGetPhysicalDeviceProperties2` with the EXT properties struct swapped for
/// the KHR one around the driver call.
pub unsafe fn properties2(p: *mut c_void, call: &dyn Fn(*mut c_void)) {
    if !faked() || p.is_null() {
        call(p);
        return;
    }
    let mut prev = p as *mut BaseOut;
    let mut cur = unsafe { (*prev).p_next } as *mut BaseOut;
    while !cur.is_null() && unsafe { (*cur).s_type } != STYPE_EXT_PROPERTIES {
        prev = cur;
        cur = unsafe { (*cur).p_next } as *mut BaseOut;
    }
    if cur.is_null() {
        call(p);
        return;
    }
    let ext = cur as *mut ExtProperties;
    let mut khr = KhrProperties {
        s_type: STYPE_KHR_PROPERTIES,
        p_next: unsafe { (*cur).p_next },
        max_vertex_attrib_divisor: 0,
        supports_non_zero_first_instance: 0,
    };
    unsafe { (*prev).p_next = (&raw mut khr).cast() };
    call(p);
    unsafe {
        (*prev).p_next = cur.cast();
        (*ext).p_next = khr.p_next;
        (*ext).max_vertex_attrib_divisor = khr.max_vertex_attrib_divisor;
    }
}

const STYPE_FEATURES: i32 = 1000190002;

/// Features answered as the wrapper ICD does: both divisor bits on.
pub unsafe fn features2(f: *mut c_void) {
    if !faked() || f.is_null() {
        return;
    }
    let mut cur = unsafe { (*(f as *mut BaseOut)).p_next } as *mut BaseOut;
    while !cur.is_null() {
        if unsafe { (*cur).s_type } == STYPE_FEATURES {
            let v = unsafe { (cur as *mut u8).add(16).cast::<[u32; 2]>() };
            unsafe { *v = [1, 1] };
        }
        cur = unsafe { (*cur).p_next } as *mut BaseOut;
    }
}
