//! Passthrough Vulkan ICD for GameNative's GameScope mode.
//!
//! The Khronos loader in the imagefs loads exactly one ICD for the whole
//! session (VK_ICD_FILENAMES points at this library). Neither on-device guest
//! driver can be listed there directly: adrenotools driver packages only work
//! after libadrenotools' hook patching (raw load fails instance creation), and
//! the payload's termux freedreno build — while direct-loadable with the
//! wayland platform compiled in — is a fixed build the user cannot pick per
//! game. This shim sits between the loader and the real driver:
//!
//!   - gamescope's own process (comm contains "gamescope") gets the payload's
//!     libvulkan_freedreno.so, the build its renderer is verified against.
//!   - every other process (wine/DXVK) gets the user-selected adrenotools
//!     driver, opened through libadrenotools with the same call flow
//!     libvulkan_wrapper.so uses (env-driven driver/hooks paths, temp/ dir for
//!     the patched copies). A driver advertising VK_KHR_wayland_surface is
//!     passed through directly — winewayland maps that host extension to the
//!     client VK_KHR_win32_surface DXVK requires. One without it (the whole
//!     adrenotools turnip family: no wayland platform compiled in) is driven
//!     through the WSI proxy (proxy.rs), which implements wayland
//!     presentation on top of the driver's VK_EXT_headless_surface plus a
//!     shim-owned dma-buf swapchain. If both the open and the proxy's
//!     prerequisites fail, the shim falls back to the payload freedreno
//!     build so the guest never ends up driverless.
//!   - with no /dev/kgsl-3d0 (a non-Adreno GPU) or GN_VKSHIM_SYSTEM_DRIVER=1,
//!     every process (gamescope included) gets the Android system loader
//!     (/system/lib64/libvulkan.so) driven through the WSI proxy. The proxy is
//!     required there even without a surface: the outer loader stamps the
//!     first word of every dispatchable handle, and the system loader keeps
//!     its own per-handle data in that same word.
//!
//! GN_VKSHIM_NO_ADRENOTOOLS=1 skips the adrenotools path entirely;
//! GN_VKSHIM_NO_PROXY=1 keeps the passthrough-or-fallback behavior without
//! engaging the proxy (debug escape hatches). Decisions are logged to
//! stderr, which surfaces as System.out in logcat.
//!
//! The loader-ICD interface surface is the minimal negotiated-v5 set:
//! vk_icdNegotiateLoaderICDInterfaceVersion, vk_icdGetInstanceProcAddr,
//! vk_icdGetPhysicalDeviceProcAddr. Everything else resolves through GIPA.

#![allow(clippy::missing_safety_doc)]
// A Vulkan ICD's whole job is punning resolved symbols into typed proc
// pointers (and back for dispatch); per-site transmute annotations would be
// pure noise. Sizes are always fn-pointer-sized.
#![allow(clippy::missing_transmute_annotations)]

/// Emit a `gn-vkshim:` line to stderr, unless `GNDRM_LOG_VKSHIM` is off
/// (unset, `""` or `"0"` — the default). `$e` is a ready-to-format value: a
/// call site passes `gndrm_obf::obf!(...)` so the message text is decoded at
/// runtime and never lands in `.rodata`. The prefix is decoded likewise.
#[macro_export]
macro_rules! logf {
    ($e:expr) => {
        if $crate::vkshim_enabled() {
            eprintln!("{} {}", ::gndrm_obf::obf!("gn-vkshim:"), $e);
        }
    };
}

/// `GNDRM_LOG_VKSHIM` gate, resolved once and cached.
pub fn vkshim_enabled() -> bool {
    // 0 = unresolved, 1 = off, 2 = on.
    static GATE: AtomicU8 = AtomicU8::new(0);
    match GATE.load(Ordering::Relaxed) {
        2 => true,
        1 => false,
        _ => {
            let on = std::env::var_os("GNDRM_LOG_VKSHIM")
                .is_some_and(|v| !v.is_empty() && v != OsStr::new("0"));
            GATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

pub mod divisor;
pub mod export;
pub mod placed;
pub mod proxy;
pub mod scanout;
pub mod segv;
pub mod thunk;
pub mod vk;
pub mod vk_dispatch_gen;
pub mod wlproto;
pub mod x11;

use std::ffi::{c_char, c_void, CStr, CString, OsStr};
use std::ptr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Once;
use vk::{GipaFn, PFN_vkVoidFunction, VkExtensionProperties, VkInstance, VkResult, VK_SUCCESS};

type IcdGipaFn = Option<unsafe extern "C" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction>;

/// Stamped into the startup line so a stale payload is never mistaken for the
/// current build (that cost a whole debugging pass). The value is emitted by
/// `build.rs` at compile time (`GN_SHIM_BUILD`); it used to be a hardcoded
/// constant that never advanced, so every rebuild reported the same date.
pub const SHIM_BUILD: &str = env!("GN_SHIM_BUILD");

/// Signature recovered from libvulkan_wrapper.so's call site: dlopen flags,
/// feature bitmask (bit0 always set, bit1 = file redirect dir present), temp
/// dir for patched copies, hooks dir, driver path, driver name, redirect dir,
/// reserved NULL. Returns a dlopen-compatible handle to the patched driver.
type AdrenotoolsOpenLibvulkanFn = unsafe extern "C" fn(
    dlopen_flags: i32,
    feature_flags: i32,
    tmp_lib_dir: *const c_char,
    hooks_dir: *const c_char,
    driver_path: *const c_char,
    driver_name: *const c_char,
    file_redirect_dir: *const c_char,
    reserved: *mut c_void,
) -> *mut c_void;

struct DriverState {
    handle: *mut c_void,
    gipa: IcdGipaFn,
    gpdpa: IcdGipaFn,
    proxy_active: bool,
}

unsafe impl Send for DriverState {}

static mut DRIVER: DriverState = DriverState {
    handle: ptr::null_mut(),
    gipa: None,
    gpdpa: None,
    proxy_active: false,
};
static INIT: Once = Once::new();

fn is_gamescope_process() -> bool {
    let comm = std::fs::read_to_string("/proc/self/comm").unwrap_or_default();
    comm.contains("gamescope")
}

unsafe fn resolve_gipa(handle: *mut c_void) -> IcdGipaFn {
    let mut gipa = unsafe { libc::dlsym(handle, c"vk_icdGetInstanceProcAddr".as_ptr()) };
    if gipa.is_null() {
        gipa = unsafe { libc::dlsym(handle, c"vkGetInstanceProcAddr".as_ptr()) };
    }
    if gipa.is_null() {
        return None;
    }
    Some(unsafe { std::mem::transmute(gipa) })
}

/// Fetches the driver's instance extension list.
fn driver_instance_exts(handle: *mut c_void) -> Vec<VkExtensionProperties> {
    let gipa = unsafe { resolve_gipa(handle) };
    let mut enum_ext: Option<vk::PFN_vkEnumerateInstanceExtensionProperties> =
        gipa.and_then(|g| unsafe {
            std::mem::transmute(g(
                ptr::null_mut(),
                c"vkEnumerateInstanceExtensionProperties".as_ptr(),
            ))
        });
    if enum_ext.is_none() {
        enum_ext = unsafe {
            let sym = libc::dlsym(handle, c"vkEnumerateInstanceExtensionProperties".as_ptr());
            if sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(sym))
            }
        };
    }
    let Some(enum_ext) = enum_ext else {
        return Vec::new();
    };
    let mut count: u32 = 0;
    if unsafe { enum_ext(ptr::null(), &mut count, ptr::null_mut()) } != VK_SUCCESS || count == 0 {
        return Vec::new();
    }
    let mut props = vec![
        VkExtensionProperties {
            extensionName: [0; vk::VK_MAX_EXTENSION_NAME_SIZE],
            specVersion: 0,
        };
        count as usize
    ];
    if unsafe { enum_ext(ptr::null(), &mut count, props.as_mut_ptr()) } != VK_SUCCESS {
        return Vec::new();
    }
    props.truncate(count as usize);
    props
}

fn ext_list_has(props: &[VkExtensionProperties], name: &CStr) -> bool {
    props.iter().any(|p| {
        let n = unsafe { CStr::from_ptr(p.extensionName.as_ptr()) };
        n == name
    })
}

const SYSTEM_LOADER: &CStr = c"/system/lib64/libvulkan.so";

fn kgsl_present() -> bool {
    // `GN_VKSHIM_ADRENO=1` pins the Adreno path where the device node cannot be
    // the answer — a host build running the probe tests, or a system driver that
    // must still be exercised behind the payload driver. Without it the node is
    // the only evidence.
    if std::env::var_os("GN_VKSHIM_ADRENO").is_some_and(|v| v == OsStr::new("1")) {
        return true;
    }
    match std::fs::metadata("/dev/kgsl-3d0") {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

fn system_driver_forced() -> bool {
    std::env::var_os("GN_VKSHIM_SYSTEM_DRIVER").is_some_and(|v| v == OsStr::new("1"))
}

/// gamescope runs as `linker64 .../bin/gamescope`, so comm alone can miss it.
fn is_gamescope_binary() -> bool {
    if is_gamescope_process() {
        return true;
    }
    let cmdline = std::fs::read("/proc/self/cmdline").unwrap_or_default();
    cmdline
        .split(|&b| b == 0)
        .take(2)
        .any(|a| a.rsplit(|&b| b == b'/').next() == Some(b"gamescope".as_slice()))
}

/// Guest-only: an ICD (the container's wrapper) to drive instead of the system loader.
fn wrapper_icd() -> Option<std::ffi::CString> {
    if is_gamescope_binary() {
        return None;
    }
    let p = std::env::var_os("GN_VKSHIM_WRAPPER_ICD")?;
    if p.is_empty() || std::fs::metadata(&p).is_err() {
        return None;
    }
    std::ffi::CString::new(p.into_encoded_bytes()).ok()
}

fn negotiate_icd(h: *mut c_void) {
    let sym = unsafe { libc::dlsym(h, c"vk_icdNegotiateLoaderICDInterfaceVersion".as_ptr()) };
    if sym.is_null() {
        return;
    }
    let f: unsafe extern "C" fn(*mut u32) -> VkResult = unsafe { std::mem::transmute(sym) };
    let mut v: u32 = 5;
    let r = unsafe { f(&mut v) };
    logf!(gndrm_obf::obf!(
        "wrapper icd: negotiated interface {v} ({r})"
    ));
}

fn try_system_driver() {
    let wrapper = wrapper_icd();
    let path = wrapper.as_deref().unwrap_or(SYSTEM_LOADER);
    let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() && wrapper.is_some() {
        logf!(gndrm_obf::obf!(
            "wrapper icd: dlopen failed: {}",
            dlerror_str()
        ));
        return;
    }
    if wrapper.is_some() {
        logf!(gndrm_obf::obf!(
            "wrapper icd: driving {}",
            path.to_string_lossy()
        ));
        negotiate_icd(h);
    }
    if h.is_null() {
        logf!(gndrm_obf::obf!(
            "dlopen system vulkan loader failed: {}",
            dlerror_str()
        ));
        return;
    }
    let Some(gipa) = (unsafe { resolve_gipa(h) }) else {
        logf!(gndrm_obf::obf!(
            "system vulkan loader exports no vkGetInstanceProcAddr"
        ));
        return;
    };
    if !proxy::device_prereqs(gipa) {
        logf!(gndrm_obf::obf!("system vulkan driver fails the proxy's prerequisite probe (see 'proxy prereq' above) — not usable in GameScope mode"));
        return;
    }
    let gamescope = is_gamescope_binary();
    if gamescope {
        proxy::set_fake_robustness2(true);
        export::set_enabled(true);
    }
    // The wrapper ICD emulates VK_EXT_map_memory_placed itself.
    if wrapper.is_none() {
        placed::set_enabled(true);
    }
    proxy::set_real_gipa(gipa);
    proxy::set_real_handle(h);
    unsafe {
        DRIVER.proxy_active = true;
        DRIVER.handle = h;
    }
    if gamescope {
        logf!(gndrm_obf::obf!(
            "gamescope process: using system vulkan driver via WSI proxy [shim {}]",
            SHIM_BUILD
        ));
    } else {
        logf!(gndrm_obf::obf!(
            "guest process: using system vulkan driver via WSI proxy [shim {}]",
            SHIM_BUILD
        ));
    }
}

fn try_adrenotools() {
    let path = std::env::var("ADRENOTOOLS_DRIVER_PATH").ok();
    let redirect = std::env::var("ADRENOTOOLS_REDIRECT_DIR").ok();
    let name = std::env::var("ADRENOTOOLS_DRIVER_NAME").ok();
    let hooks = std::env::var("ADRENOTOOLS_HOOKS_PATH").ok().or_else(|| {
        std::env::var("PREFIX")
            .ok()
            .map(|p| format!("{}", gndrm_obf::obf!("{p}/lib")))
    });
    let Some(hooks) = hooks else {
        logf!(gndrm_obf::obf!(
            "ADRENOTOOLS_HOOKS_PATH unset and no PREFIX — skipping adrenotools"
        ));
        return;
    };
    let Some(path) = path else {
        logf!(gndrm_obf::obf!(
            "ADRENOTOOLS_DRIVER_PATH unset — skipping adrenotools"
        ));
        return;
    };
    if !std::path::Path::new(&path).exists() {
        logf!(gndrm_obf::obf!(
            "ADRENOTOOLS_DRIVER_PATH {path} not accessible — skipping adrenotools"
        ));
        return;
    }
    let at = unsafe {
        libc::dlopen(
            c"libadrenotools.so".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        )
    };
    if at.is_null() {
        logf!(gndrm_obf::obf!(
            "dlopen libadrenotools.so failed: {}",
            dlerror_str()
        ));
        return;
    }
    let open_vk: Option<AdrenotoolsOpenLibvulkanFn> = unsafe {
        let sym = libc::dlsym(at, c"adrenotools_open_libvulkan".as_ptr());
        if sym.is_null() {
            None
        } else {
            Some(std::mem::transmute(sym))
        }
    };
    let Some(open_vk) = open_vk else {
        logf!(gndrm_obf::obf!(
            "adrenotools_open_libvulkan not exported by libadrenotools.so"
        ));
        return;
    };
    // Patched copies live in a temp/ dir beside the driver (same layout the
    // wrapper creates); libadrenotools (re)generates them as needed.
    let tmp = format!(
        "{}",
        gndrm_obf::obf!("{path}{}temp", if path.ends_with('/') { "" } else { "/" })
    );
    let _ = std::fs::create_dir_all(&tmp);
    let c_tmp = CString::new(tmp).unwrap();
    let c_hooks = CString::new(hooks).unwrap();
    let c_path = CString::new(path.as_str()).unwrap();
    let c_name = name.as_ref().map(|n| CString::new(n.as_str()).unwrap());
    let c_redirect = redirect.as_ref().map(|r| CString::new(r.as_str()).unwrap());
    let h = unsafe {
        open_vk(
            libc::RTLD_NOW,
            if redirect.is_some() { 3 } else { 1 },
            c_tmp.as_ptr(),
            c_hooks.as_ptr(),
            c_path.as_ptr(),
            c_name.as_ref().map_or(ptr::null(), |n| n.as_ptr()),
            c_redirect.as_ref().map_or(ptr::null(), |r| r.as_ptr()),
            ptr::null_mut(),
        )
    };
    if h.is_null() {
        logf!(gndrm_obf::obf!(
            "adrenotools_open_libvulkan failed for {}{}",
            path,
            name.as_deref().unwrap_or("")
        ));
        return;
    }
    let exts = driver_instance_exts(h);
    let has_wayland = ext_list_has(&exts, c"VK_KHR_wayland_surface");
    let has_headless = ext_list_has(&exts, c"VK_EXT_headless_surface");
    let label = name.as_deref().unwrap_or(path.as_str()).to_string();
    if has_wayland {
        logf!(gndrm_obf::obf!(
            "using adrenotools driver {label} (VK_KHR_wayland_surface present)"
        ));
        unsafe { DRIVER.handle = h };
        return;
    }
    // No wayland WSI (the whole adrenotools turnip family): present through
    // the WSI proxy (proxy.rs) on top of VK_EXT_headless_surface instead.
    if !has_headless || std::env::var_os("GN_VKSHIM_NO_PROXY").is_some() {
        logf!(gndrm_obf::obf!(
            "adrenotools driver {label} lacks VK_KHR_wayland_surface{} — not usable in GameScope mode",
            if has_headless {
                " (GN_VKSHIM_NO_PROXY set)"
            } else {
                " and VK_EXT_headless_surface"
            }
        ));
        return;
    }
    let Some(gipa) = (unsafe { resolve_gipa(h) }) else {
        logf!(gndrm_obf::obf!("adrenotools driver {label} fails the proxy's prerequisite probe (see 'proxy prereq' above) — not usable in GameScope mode"));
        return;
    };
    let gipa_fn: GipaFn = gipa;
    if !proxy::device_prereqs(gipa_fn) {
        logf!(gndrm_obf::obf!("adrenotools driver {label} fails the proxy's prerequisite probe (see 'proxy prereq' above) — not usable in GameScope mode"));
        return;
    }
    proxy::set_real_gipa(gipa_fn);
    proxy::set_real_handle(h);
    unsafe {
        DRIVER.proxy_active = true;
        DRIVER.handle = h;
    }
    logf!(gndrm_obf::obf!(
        "using adrenotools driver {label} via WSI proxy (headless surface + dma-buf export) [shim {}]",
        SHIM_BUILD
    ));
}

/// The real driver .so handle the shim opened (adrenotools- or freedreno-
/// backed). The proxy uses it to resolve an entry point it intercepts but must
/// forward unchanged in a process where the proxy is not active.
pub fn driver_handle() -> *mut c_void {
    unsafe { DRIVER.handle }
}

fn dlerror_str() -> String {
    let err = unsafe { libc::dlerror() };
    if err.is_null() {
        "<unknown>".into()
    } else {
        unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned()
    }
}

fn init_once() {
    // Round-v diagnostic: install the chaining SIGSEGV/SIGBUS register dump
    // before any driver code runs, so it sits in front of wine's SEH handler.
    #[cfg(feature = "segv_dump")]
    crate::segv::install();
    let kgsl = kgsl_present();
    if !kgsl || system_driver_forced() {
        if kgsl {
            logf!(gndrm_obf::obf!(
                "GN_VKSHIM_SYSTEM_DRIVER=1: using the system vulkan loader"
            ));
        } else {
            logf!(gndrm_obf::obf!(
                "no /dev/kgsl-3d0: using the system vulkan loader"
            ));
        }
        try_system_driver();
    }
    if kgsl && unsafe { DRIVER.handle }.is_null() {
        if is_gamescope_process() {
            let h = unsafe {
                libc::dlopen(
                    c"libvulkan_freedreno.so".as_ptr(),
                    libc::RTLD_NOW | libc::RTLD_LOCAL,
                )
            };
            logf!(gndrm_obf::obf!(
                "gamescope process: payload freedreno {}",
                if h.is_null() {
                    dlerror_str()
                } else {
                    "loaded".into()
                }
            ));
            unsafe { DRIVER.handle = h };
        } else {
            if std::env::var_os("GN_VKSHIM_NO_ADRENOTOOLS").is_none() {
                try_adrenotools();
            }
            if unsafe { DRIVER.handle }.is_null() {
                let h = unsafe {
                    libc::dlopen(
                        c"libvulkan_freedreno.so".as_ptr(),
                        libc::RTLD_NOW | libc::RTLD_LOCAL,
                    )
                };
                logf!(gndrm_obf::obf!(
                    "guest process: falling back to payload freedreno {}",
                    if h.is_null() {
                        dlerror_str()
                    } else {
                        "loaded".into()
                    }
                ));
                unsafe { DRIVER.handle = h };
            }
        }
    }
    let handle = unsafe { DRIVER.handle };
    if handle.is_null() {
        return;
    }
    unsafe {
        let drv = &raw mut DRIVER;
        (*drv).gipa = resolve_gipa(handle);
        (*drv).gpdpa = {
            let sym = libc::dlsym(handle, c"vk_icdGetPhysicalDeviceProcAddr".as_ptr());
            if sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(sym))
            }
        };
        if (*drv).gipa.is_none() {
            logf!(gndrm_obf::obf!(
                "driver exports no instance proc addr — ICD unusable"
            ));
        }
    }
}

fn ensure_init() {
    INIT.call_once(init_once);
}

/// Whether the shim is presenting through the WSI proxy, i.e. whether it hands
/// out wrapper handles at all. The dispatch policy swaps a wrapper for the real
/// handle only when wrappers exist; without the proxy every handle is the
/// driver's own and is passed straight through.
pub fn proxy_active() -> bool {
    unsafe { DRIVER.proxy_active }
}

/// # Safety: loader-ICD v5 interface (called by the Khronos loader).
#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "C" fn vk_icdNegotiateLoaderICDInterfaceVersion(
    pSupportedVersion: *mut u32,
) -> VkResult {
    ensure_init();
    unsafe {
        if *pSupportedVersion > 5 {
            *pSupportedVersion = 5;
        }
    }
    VK_SUCCESS
}

/// # Safety: loader-ICD v5 interface (called by the Khronos loader).
#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "C" fn vk_icdGetInstanceProcAddr(
    instance: VkInstance,
    pName: *const c_char,
) -> PFN_vkVoidFunction {
    ensure_init();
    let name = unsafe { CStr::from_ptr(pName) };
    // The loader asks this entry point for instance- and physical-device-level
    // names, but the ICD contract is that it resolves *all* names, so device
    // names are answered here too. `entrypoint_handler` serves a name the shim
    // implements, but only while the WSI proxy is active (that is the only time
    // the shim owns wrapper handles); every other name — and every name at all
    // when the proxy is inactive — is served through the one unlisted-name
    // policy (`proxy::serve_unlisted`), which returns the driver's own entry
    // point so a fallback session uses the driver's own WSI rather than the
    // shim's, whose surfaces do not exist without the proxy.
    let fnptr = proxy::entrypoint_handler(name, proxy_active());
    if fnptr.is_some() {
        return fnptr;
    }
    let raw = unsafe { DRIVER.gipa?(instance, pName) };
    proxy::note_instance_miss(name, raw.is_none());
    proxy::serve_unlisted(name, raw, proxy::DispatchCtx::Instance)
}

/// # Safety: loader-ICD v5 interface (called by the Khronos loader).
#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "C" fn vk_icdGetPhysicalDeviceProcAddr(
    instance: VkInstance,
    pName: *const c_char,
) -> PFN_vkVoidFunction {
    ensure_init();
    // Physical-device-level functions (surface queries) resolve through GPDPA
    // as well as GIPA — intercept on both paths, device functions too (see
    // vk_icdGetInstanceProcAddr), and serve the rest through the same
    // unlisted-name policy. Like GIPA, the handler lookup is gated on the proxy
    // being active, so a fallback session gets the driver's own surface path
    // rather than the shim's, whose surfaces only the proxy creates. GPDPA never
    // resolves device-level names, so the policy's swap branch cannot fire here.
    let name = unsafe { CStr::from_ptr(pName) };
    let fnptr = proxy::entrypoint_handler(name, proxy_active());
    if fnptr.is_some() {
        return fnptr;
    }
    let raw = unsafe {
        match DRIVER.gpdpa {
            Some(gpdpa) => gpdpa(instance, pName),
            None => DRIVER.gipa?(instance, pName),
        }
    };
    proxy::note_instance_miss(name, raw.is_none());
    proxy::serve_unlisted(name, raw, proxy::DispatchCtx::PhysicalDevice)
}
