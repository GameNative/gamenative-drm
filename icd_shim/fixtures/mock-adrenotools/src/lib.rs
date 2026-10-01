//! mock libadrenotools.so — host-test fixture for the icd_shim integration
//! tests. Exports `adrenotools_open_libvulkan` with the real signature; the
//! "patching" is skipped (nothing to patch in a mock) and the driver library
//! is dlopened directly from `<driver_path>/<driver_name>` (or driver_path
//! alone when it already names a file).

use std::ffi::{c_char, c_void, CStr, CString};

#[allow(non_snake_case, clippy::too_many_arguments)]
#[no_mangle]
pub unsafe extern "C" fn adrenotools_open_libvulkan(
    dlopen_flags: i32,
    _feature_flags: i32,
    _tmp_lib_dir: *const c_char,
    _hooks_dir: *const c_char,
    driver_path: *const c_char,
    driver_name: *const c_char,
    _file_redirect_dir: *const c_char,
    _reserved: *mut c_void,
) -> *mut c_void {
    if driver_path.is_null() {
        return std::ptr::null_mut();
    }
    let path = unsafe { CStr::from_ptr(driver_path) }.to_string_lossy();
    let full = if driver_name.is_null() {
        path.into_owned()
    } else {
        let name = unsafe { CStr::from_ptr(driver_name) }.to_string_lossy();
        format!("{}/{name}", path.trim_end_matches('/'))
    };
    let c = CString::new(full.as_str()).unwrap();
    let h = unsafe { libc::dlopen(c.as_ptr(), dlopen_flags) };
    if h.is_null() {
        let err = unsafe { libc::dlerror() };
        if !err.is_null() {
            eprintln!(
                "mock-adrenotools: dlopen({full}) failed: {}",
                unsafe { CStr::from_ptr(err) }.to_string_lossy()
            );
        }
    }
    h
}
