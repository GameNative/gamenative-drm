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
        crate::logf!("proxy: advertised VK_EXT_vertex_attribute_divisor over the KHR extension");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;
    use std::sync::Mutex;

    /// The flags are process-global, so tests that set them take turns and each
    /// starts from the same state.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// Every test starts from "not faked", whatever ran before it.
    fn reset() {
        FAKED.store(false, Ordering::Relaxed);
    }

    #[repr(C)]
    struct Head {
        s_type: i32,
        p_next: *mut c_void,
    }

    #[repr(C)]
    struct Link {
        s_type: i32,
        p_next: *mut c_void,
        value: u32,
    }

    fn head(next: *mut c_void) -> Head {
        Head {
            s_type: 1,
            p_next: next,
        }
    }

    fn link(s_type: i32, value: u32) -> Link {
        Link {
            s_type,
            p_next: ptr::null_mut(),
            value,
        }
    }

    #[test]
    fn not_faked_passes_the_chain_through_untouched() {
        let _g = SERIAL.lock().unwrap();
        reset();
        assert!(!faked());
        let mut ext = link(STYPE_EXT_PROPERTIES, 0);
        let mut h = head((&raw mut ext).cast());
        let before = h.p_next;
        let seen = std::cell::Cell::new(ptr::null_mut());
        unsafe {
            properties2((&raw mut h).cast(), &|p| {
                seen.set((*(p as *mut Head)).p_next);
            })
        };
        assert_eq!(seen.get(), before);
        assert_eq!(unsafe { (*(h.p_next as *mut Link)).value }, 0);
    }

    #[test]
    fn faked_answers_the_ext_struct_from_the_khr_one() {
        let _g = SERIAL.lock().unwrap();
        reset();
        note_faked();
        let mut ext = link(STYPE_EXT_PROPERTIES, 0);
        let mut h = head((&raw mut ext).cast());
        unsafe {
            properties2((&raw mut h).cast(), &|p| {
                // The driver must see the KHR sType in the same place in the chain.
                let khr = (*(p as *mut Head)).p_next as *mut Link;
                assert_eq!((*khr).s_type, STYPE_KHR_PROPERTIES);
                (*khr).value = 8;
            })
        };
        // ... and the caller's EXT struct must carry the answer afterwards.
        let ext_after = &ext;
        assert_eq!(ext_after.s_type, STYPE_EXT_PROPERTIES);
        assert_eq!(ext_after.value, 8);
        // The chain is restored.
        assert_eq!(h.p_next, (&raw mut ext) as *mut c_void);
    }

    #[test]
    fn faked_without_the_ext_struct_leaves_the_chain_alone() {
        let _g = SERIAL.lock().unwrap();
        reset();
        note_faked();
        let mut other = link(42, 7);
        let mut h = head((&raw mut other).cast());
        let seen = std::cell::Cell::new(0);
        unsafe {
            properties2((&raw mut h).cast(), &|p| {
                let next = (*(p as *mut Head)).p_next as *mut Link;
                seen.set((*next).s_type);
            })
        };
        assert_eq!(seen.get(), 42);
        assert_eq!(unsafe { (*(h.p_next as *mut Link)).value }, 7);
    }

    #[test]
    fn features_sets_both_divisor_bits() {
        let _g = SERIAL.lock().unwrap();
        reset();
        note_faked();
        let mut features = link(STYPE_FEATURES, 0);
        let mut h = head((&raw mut features).cast());
        unsafe { features2((&raw mut h).cast()) };
        let bits = unsafe { *((&raw mut features) as *mut u8).add(16).cast::<[u32; 2]>() };
        assert_eq!(bits, [1, 1]);
    }

    #[test]
    fn features_ignores_other_stypes() {
        let _g = SERIAL.lock().unwrap();
        reset();
        note_faked();
        let mut other = link(7, 0);
        let mut h = head((&raw mut other).cast());
        unsafe { features2((&raw mut h).cast()) };
        assert_eq!(other.value, 0);
    }

    #[test]
    fn names_are_the_extension_and_its_promoted_spelling() {
        assert_eq!(
            EXT_NAME.to_str().unwrap(),
            "VK_EXT_vertex_attribute_divisor"
        );
        assert_eq!(
            KHR_NAME.to_str().unwrap(),
            "VK_KHR_vertex_attribute_divisor"
        );
    }
}
