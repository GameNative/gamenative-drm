// mock_vkdriver — minimal fake Vulkan ICD used by the icd_shim host
// integration tests. Two crates share this source (see
// fixtures/mock-freedreno) so the same code answers both
// `libmock_vkdriver.so` (adrenotools-side driver) and
// `libvulkan_freedreno.so` (payload fallback) lookups.
//
// Behavior knobs (env):
//   MOCK_INSTANCE_EXTS — comma list for vkEnumerateInstanceExtensionProperties
//                        (default "VK_EXT_headless_surface")
//   MOCK_DEVICE_EXTS   — comma list for vkEnumerateDeviceExtensionProperties
//                        (default: the proxy's four dma-buf prereqs)
//   MOCK_CREATE_RES    — VkResult vkCreateInstance returns (default 0);
//                        lets a test emulate a driver that refuses instances
//
// Every vkCreateInstance prints the enabled extension list to stderr, which
// the tests use to verify the proxy strips/injects exactly what the design
// says (wayland out, headless in).
//
// Mesa realism: a NULL instance resolves only global functions; instance
// functions require a live instance handle — this is what forces the shim's
// "fetch against a live instance" ordering to be correct.

use std::ffi::{c_char, CStr};
use std::ptr;

// Standalone vulkan FFI (a real driver only needs the headers; linking the
// shim's rlib would drag its vk_icd* exports into this cdylib and collide
// with the mock's own). Layout-compatible subset of icd_shim/src/vk.rs.
mod vk {
    #![allow(non_snake_case, non_camel_case_types, dead_code)]
    use std::ffi::{c_char, c_void};

    pub type VkResult = i32;
    pub const VK_SUCCESS: VkResult = 0;
    pub const VK_ERROR_INITIALIZATION_FAILED: VkResult = -3;
    pub type VkInstance = *mut c_void;
    pub type VkPhysicalDevice = *mut c_void;
    pub const VK_STRUCTURE_TYPE_APPLICATION_INFO: i32 = 0;
    pub const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: i32 = 1;
    pub const VK_API_VERSION_1_3: u32 = 1 << 22 | 3 << 12;
    pub const VK_MAX_EXTENSION_NAME_SIZE: usize = 256;

    pub enum VkAllocationCallbacks {}

    #[repr(C)]
    pub struct VkExtensionProperties {
        pub extensionName: [c_char; VK_MAX_EXTENSION_NAME_SIZE],
        pub specVersion: u32,
    }

    #[repr(C)]
    pub struct VkInstanceCreateInfo {
        pub sType: i32,
        pub pNext: *const c_void,
        pub flags: u32,
        pub pApplicationInfo: *const c_void,
        pub enabledLayerCount: u32,
        pub ppEnabledLayerNames: *const *const c_char,
        pub enabledExtensionCount: u32,
        pub ppEnabledExtensionNames: *const *const c_char,
    }

    pub type PFN_vkVoidFunction = Option<unsafe extern "C" fn()>;
    pub type PFN_vkCreateInstance = unsafe extern "C" fn(
        *const VkInstanceCreateInfo,
        *const VkAllocationCallbacks,
        *mut VkInstance,
    ) -> VkResult;
    pub type PFN_vkDestroyInstance = unsafe extern "C" fn(VkInstance, *const VkAllocationCallbacks);
    pub type PFN_vkEnumeratePhysicalDevices =
        unsafe extern "C" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult;
    pub type PFN_vkEnumerateInstanceExtensionProperties =
        unsafe extern "C" fn(*const c_char, *mut u32, *mut VkExtensionProperties) -> VkResult;
    pub type PFN_vkEnumerateInstanceVersion = unsafe extern "C" fn(*mut u32) -> VkResult;
    pub type PFN_vkEnumerateDeviceExtensionProperties = unsafe extern "C" fn(
        VkPhysicalDevice,
        *const c_char,
        *mut u32,
        *mut VkExtensionProperties,
    ) -> VkResult;
}

const FAKE_INSTANCE: vk::VkInstance = 0x6d0c6b15_74616e63usize as vk::VkInstance; // "mock-istanc"
const FAKE_PD: vk::VkPhysicalDevice = 0x6d0c6b70_64000000usize as vk::VkPhysicalDevice;

type Pfn = vk::PFN_vkVoidFunction;

/// Pun a typed vulkan entry point into the loader's void-function slot.
/// All vulkan PFNs are same-sized fn pointers.
fn pfn<T>(f: T) -> Pfn {
    assert!(std::mem::size_of::<T>() == std::mem::size_of::<Pfn>());
    unsafe { std::mem::transmute_copy::<T, Pfn>(&f) }
}

fn env_list(var: &str, default: &str) -> Vec<String> {
    std::env::var(var)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn fill_exts(
    names: &[String],
    count: *mut u32,
    props: *mut vk::VkExtensionProperties,
) -> vk::VkResult {
    if names.is_empty() {
        unsafe { *count = 0 };
        return vk::VK_SUCCESS;
    }
    unsafe {
        if props.is_null() {
            *count = names.len() as u32;
            return vk::VK_SUCCESS;
        }
        let n = (*count as usize).min(names.len());
        for (i, name) in names.iter().take(n).enumerate() {
            let p = props.add(i);
            ptr::write_bytes(
                (*p).extensionName.as_mut_ptr(),
                0,
                vk::VK_MAX_EXTENSION_NAME_SIZE,
            );
            let b = name.as_bytes();
            ptr::copy_nonoverlapping(b.as_ptr(), (*p).extensionName.as_mut_ptr().cast(), b.len());
            (*p).specVersion = 1;
        }
        *count = n as u32;
    }
    vk::VK_SUCCESS
}

unsafe extern "C" fn mock_enumerate_instance_extension_properties(
    _layer: *const c_char,
    count: *mut u32,
    props: *mut vk::VkExtensionProperties,
) -> vk::VkResult {
    fill_exts(
        &env_list("MOCK_INSTANCE_EXTS", "VK_EXT_headless_surface"),
        count,
        props,
    )
}

unsafe extern "C" fn mock_enumerate_device_extension_properties(
    _pd: vk::VkPhysicalDevice,
    _layer: *const c_char,
    count: *mut u32,
    props: *mut vk::VkExtensionProperties,
) -> vk::VkResult {
    fill_exts(
        &env_list(
            "MOCK_DEVICE_EXTS",
            "VK_KHR_external_memory,VK_KHR_external_memory_fd,VK_EXT_external_memory_dma_buf,VK_EXT_image_drm_format_modifier",
        ),
        count,
        props,
    )
}

unsafe extern "C" fn mock_create_instance(
    ci: *const vk::VkInstanceCreateInfo,
    _alloc: *const vk::VkAllocationCallbacks,
    out: *mut vk::VkInstance,
) -> vk::VkResult {
    if ci.is_null() || unsafe { (*ci).sType } != vk::VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO {
        eprintln!("mock-driver: vkCreateInstance BAD sType {:?}", unsafe {
            ci.as_ref().map(|c| c.sType)
        });
        return vk::VK_ERROR_INITIALIZATION_FAILED;
    }
    let mut exts = Vec::new();
    unsafe {
        for i in 0..(*ci).enabledExtensionCount as isize {
            let p = (*ci).ppEnabledExtensionNames.offset(i);
            if !p.is_null() && !(*p).is_null() {
                exts.push(CStr::from_ptr(*p).to_string_lossy().into_owned());
            }
        }
    }
    eprintln!("mock-driver: vkCreateInstance exts={exts:?}");
    let res = std::env::var("MOCK_CREATE_RES")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(vk::VK_SUCCESS);
    if res == vk::VK_SUCCESS {
        unsafe { *out = FAKE_INSTANCE };
    }
    res
}

unsafe extern "C" fn mock_destroy_instance(
    _inst: vk::VkInstance,
    _alloc: *const vk::VkAllocationCallbacks,
) {
}

unsafe extern "C" fn mock_enumerate_physical_devices(
    _inst: vk::VkInstance,
    count: *mut u32,
    pds: *mut vk::VkPhysicalDevice,
) -> vk::VkResult {
    unsafe {
        if pds.is_null() {
            *count = 1;
        } else if *count >= 1 {
            *pds = FAKE_PD;
            *count = 1;
        }
    }
    vk::VK_SUCCESS
}

unsafe extern "C" fn mock_enumerate_instance_version(version: *mut u32) -> vk::VkResult {
    unsafe { *version = vk::VK_API_VERSION_1_3 };
    vk::VK_SUCCESS
}

/// NULL instance → global functions only (mesa behavior); live instance →
/// the instance-level set as well.
unsafe extern "C" fn mock_gipa(inst: vk::VkInstance, name: *const c_char) -> Pfn {
    if name.is_null() {
        return None;
    }
    let n = unsafe { CStr::from_ptr(name) };
    let global: Pfn = match n.to_bytes() {
        b"vkCreateInstance" => pfn::<vk::PFN_vkCreateInstance>(mock_create_instance),
        b"vkEnumerateInstanceExtensionProperties" => {
            pfn::<vk::PFN_vkEnumerateInstanceExtensionProperties>(
                mock_enumerate_instance_extension_properties,
            )
        }
        b"vkEnumerateInstanceVersion" => {
            pfn::<vk::PFN_vkEnumerateInstanceVersion>(mock_enumerate_instance_version)
        }
        _ => None,
    };
    if global.is_some() || inst.is_null() {
        return global;
    }
    match n.to_bytes() {
        b"vkDestroyInstance" => pfn::<vk::PFN_vkDestroyInstance>(mock_destroy_instance),
        b"vkEnumeratePhysicalDevices" => {
            pfn::<vk::PFN_vkEnumeratePhysicalDevices>(mock_enumerate_physical_devices)
        }
        b"vkEnumerateDeviceExtensionProperties" => {
            pfn::<vk::PFN_vkEnumerateDeviceExtensionProperties>(
                mock_enumerate_device_extension_properties,
            )
        }
        _ => None,
    }
}

#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "C" fn vk_icdGetInstanceProcAddr(
    inst: vk::VkInstance,
    name: *const c_char,
) -> Pfn {
    unsafe { mock_gipa(inst, name) }
}

#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "C" fn vkGetInstanceProcAddr(inst: vk::VkInstance, name: *const c_char) -> Pfn {
    unsafe { mock_gipa(inst, name) }
}

#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "C" fn vk_icdGetPhysicalDeviceProcAddr(
    _inst: vk::VkInstance,
    _name: *const c_char,
) -> Pfn {
    None
}

#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "C" fn vk_icdNegotiateLoaderICDInterfaceVersion(v: *mut u32) -> vk::VkResult {
    unsafe {
        if *v > 5 {
            *v = 5;
        }
    }
    vk::VK_SUCCESS
}
