//! icd_probe — loader-side harness for the adrenotools ICD shim.
//!
//! Dlopens a built `libadrenotools_icd.so` exactly like the Khronos loader
//! would (negotiated-v5 interface) and drives instance creation end to end.
//! Every stage prints a `PROBE`-prefixed line; the exit code identifies the
//! first failing stage so host tests and on-device runs can assert without
//! parsing free text:
//!
//!   0  full pass (instance created, physical devices enumerated, destroyed)
//!   2  dlopen of the ICD failed
//!   3  a negotiated-v5 export is missing
//!   4  vk_icdNegotiateLoaderICDInterfaceVersion failed / misbehaved
//!   5  GIPA did not resolve a core entry point
//!   6  vkEnumerateInstanceExtensionProperties failed
//!   7  vkCreateInstance failed
//!   8  vkEnumeratePhysicalDevices failed / zero devices
//!
//! Usage: icd_probe [--comm <name>] <path-to-libadrenotools_icd.so>
//!
//! `--comm` renames the calling thread via prctl(PR_SET_NAME) BEFORE the
//! dlopen, which is what the shim's /proc/self/comm gamescope detection
//! reads (15-byte kernel limit applies).

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;

use adrenotools_icd::vk;

type NegotiateFn = unsafe extern "C" fn(*mut u32) -> vk::VkResult;

/// NUL-terminated byte string → *const c_char (the lint gate's old parser
/// rejects c"" literals; byte strings are identical on the wire).
const fn cs(s: &'static [u8]) -> *const c_char {
    s.as_ptr() as *const c_char
}

fn fail(code: i32, msg: String) -> ! {
    println!("PROBE FAIL[{code}] {msg}");
    std::process::exit(code);
}

fn main() {
    let mut comm: Option<String> = None;
    let mut so_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--comm" {
            comm = Some(
                args.next()
                    .unwrap_or_else(|| fail(64, "--comm needs a value".into())),
            );
        } else if so_path.is_none() {
            so_path = Some(a);
        } else {
            fail(64, format!("unexpected argument {a}"));
        }
    }
    let Some(so_path) = so_path else {
        fail(64, "usage: icd_probe [--comm <name>] <icd.so>".into());
    };

    if let Some(name) = comm {
        let bytes = CString::new(name.as_str()).unwrap();
        // PR_SET_NAME truncates to 15 bytes + NUL; that is fine for
        // "gamescope" and any test double.
        let r = unsafe { libc::prctl(libc::PR_SET_NAME, bytes.as_ptr(), 0, 0, 0) };
        if r != 0 {
            fail(64, format!("prctl(PR_SET_NAME {name}) failed: {r}"));
        }
        println!("PROBE comm set to {name}");
    }

    let c_path = CString::new(so_path.as_str()).unwrap();
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        let err = unsafe { libc::dlerror() };
        let msg = if err.is_null() {
            "<no dlerror>".into()
        } else {
            unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned()
        };
        fail(2, format!("dlopen({so_path}): {msg}"));
    }
    println!("PROBE dlopen ok");

    let sym = |name: &'static [u8]| -> *mut c_void {
        let p = unsafe { libc::dlsym(handle, cs(name)) };
        if p.is_null() {
            fail(
                3,
                format!(
                    "missing export {}",
                    CStr::from_bytes_with_nul(name).unwrap().to_str().unwrap()
                ),
            );
        }
        p
    };
    let negotiate: NegotiateFn =
        unsafe { std::mem::transmute(sym(b"vk_icdNegotiateLoaderICDInterfaceVersion\0")) };
    let gipa: vk::GipaFn = unsafe { std::mem::transmute(sym(b"vk_icdGetInstanceProcAddr\0")) };
    let _gpdpa: vk::GipaFn =
        unsafe { std::mem::transmute(sym(b"vk_icdGetPhysicalDeviceProcAddr\0")) };
    println!("PROBE exports ok (negotiate/GIPA/GPDPA present)");

    // Loader offers 7; a v5 ICD must clamp to 5 and return VK_SUCCESS.
    let mut offered: u32 = 7;
    let r = unsafe { negotiate(&mut offered) };
    println!("PROBE negotiate result={r} clamped={offered}");
    if r != vk::VK_SUCCESS || offered != 5 {
        fail(
            4,
            format!("negotiate result={r} clamped={offered}, want 0/5"),
        );
    }

    let resolve =
        |inst: vk::VkInstance, name: &'static [u8], code: i32| -> vk::PFN_vkVoidFunction {
            let f = unsafe { gipa(inst, cs(name)) };
            if f.is_none() {
                fail(
                    code,
                    format!(
                        "GIPA({}) returned null",
                        CStr::from_bytes_with_nul(name).unwrap().to_str().unwrap()
                    ),
                );
            }
            f
        };

    let enum_exts: vk::PFN_vkEnumerateInstanceExtensionProperties = unsafe {
        std::mem::transmute(resolve(
            ptr::null_mut(),
            b"vkEnumerateInstanceExtensionProperties\0",
            5,
        ))
    };
    let create_instance: vk::PFN_vkCreateInstance =
        unsafe { std::mem::transmute(resolve(ptr::null_mut(), b"vkCreateInstance\0", 5)) };

    let mut n_exts: u32 = 0;
    let r = unsafe { enum_exts(ptr::null(), &mut n_exts, ptr::null_mut()) };
    if r != vk::VK_SUCCESS {
        fail(
            6,
            format!("vkEnumerateInstanceExtensionProperties(count) -> {r}"),
        );
    }
    let mut props = vec![
        vk::VkExtensionProperties {
            extensionName: [0; vk::VK_MAX_EXTENSION_NAME_SIZE],
            specVersion: 0,
        };
        n_exts as usize
    ];
    let r = unsafe { enum_exts(ptr::null(), &mut n_exts, props.as_mut_ptr()) };
    if r != vk::VK_SUCCESS {
        fail(
            6,
            format!("vkEnumerateInstanceExtensionProperties(list) -> {r}"),
        );
    }
    println!("PROBE instance_exts={n_exts}");
    for p in &props[..n_exts as usize] {
        let name = unsafe { CStr::from_ptr(p.extensionName.as_ptr()) };
        println!("PROBE ext {}", name.to_str().unwrap());
    }

    let app = vk::VkApplicationInfo {
        sType: vk::VK_STRUCTURE_TYPE_APPLICATION_INFO,
        pNext: ptr::null(),
        pApplicationName: cs(b"icd_probe\0"),
        applicationVersion: 0,
        pEngineName: ptr::null(),
        engineVersion: 0,
        apiVersion: vk::VK_API_VERSION_1_3,
    };
    let ci = vk::VkInstanceCreateInfo {
        sType: vk::VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        pNext: ptr::null(),
        flags: 0,
        pApplicationInfo: &app,
        enabledLayerCount: 0,
        ppEnabledLayerNames: ptr::null(),
        enabledExtensionCount: 0,
        ppEnabledExtensionNames: ptr::null(),
    };
    let mut inst: vk::VkInstance = ptr::null_mut();
    let r = unsafe { create_instance(&ci, ptr::null(), &mut inst) };
    if r != vk::VK_SUCCESS || inst.is_null() {
        fail(7, format!("vkCreateInstance -> {r} inst={inst:?}"));
    }
    println!("PROBE create_instance ok inst={inst:?}");

    // Instance-level procs must resolve against the live instance (mesa
    // resolves only global functions for a NULL instance).
    let enum_pds: vk::PFN_vkEnumeratePhysicalDevices =
        unsafe { std::mem::transmute(resolve(inst, b"vkEnumeratePhysicalDevices\0", 8)) };
    let mut n_pds: u32 = 0;
    let r = unsafe { enum_pds(inst, &mut n_pds, ptr::null_mut()) };
    if r != vk::VK_SUCCESS || n_pds == 0 {
        fail(
            8,
            format!("vkEnumeratePhysicalDevices -> {r} count={n_pds}"),
        );
    }
    println!("PROBE phys_devices={n_pds}");

    if let Some(destroy) = unsafe { gipa(inst, cs(b"vkDestroyInstance\0")) }.map(|f| unsafe {
        std::mem::transmute::<unsafe extern "C" fn(), vk::PFN_vkDestroyInstance>(f)
    }) {
        unsafe { destroy(inst, ptr::null()) };
    }
    println!("PROBE done");
    std::process::exit(0);
}
