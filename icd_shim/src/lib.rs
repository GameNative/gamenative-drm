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
//!
//! GN_VKSHIM_NO_ADRENOTOOLS=1 skips the adrenotools path entirely;
//! GN_VKSHIM_NO_PROXY=1 keeps the passthrough-or-fallback behavior without
//! engaging the proxy (debug escape hatches). Decisions are logged to
//! stderr, which surfaces as System.out in logcat.
//!
//! The loader-ICD interface surface is the minimal negotiated-v5 set:
//! vk_icdNegotiateLoaderICDInterfaceVersion, vk_icdGetInstanceProcAddr,
//! vk_icdGetPhysicalDeviceProcAddr. Everything else resolves through GIPA.
//!
//! Rust port of gamescope/android/icd-shim/adrenotools_icd.c.

#![allow(clippy::missing_safety_doc)]
// A Vulkan ICD's whole job is punning resolved symbols into typed proc
// pointers (and back for dispatch); per-site transmute annotations would be
// pure noise. Sizes are always fn-pointer-sized.
#![allow(clippy::missing_transmute_annotations)]

/// Emit a `gn-vkshim:` line to stderr, unless `GNDRM_LOG_VKSHIM` is off
/// (unset, `""` or `"0"` — the default). The format arguments are only
/// evaluated when the gate is on.
#[macro_export]
macro_rules! logf {
    ($fmt:literal $(, $($arg:tt)*)?) => {
        if $crate::vkshim_enabled() {
            eprintln!("gn-vkshim: {}", format_args!($fmt $(, $($arg)*)?));
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

pub mod proxy;
pub mod scanout;
pub mod thunk;
pub mod vk;
pub mod wlproto;

use std::ffi::{c_char, c_int, c_void, CStr, CString, OsStr};
use std::ptr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Once;
use vk::{GipaFn, PFN_vkVoidFunction, VkExtensionProperties, VkInstance, VkResult, VK_SUCCESS};

type IcdGipaFn = Option<unsafe extern "C" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction>;

/// Stamped into the startup line so a stale payload is never mistaken for the
/// current build (that cost a whole debugging pass).
pub const SHIM_BUILD: &str = "20260930z5";

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

fn try_adrenotools() {
    let path = std::env::var("ADRENOTOOLS_DRIVER_PATH").ok();
    let redirect = std::env::var("ADRENOTOOLS_REDIRECT_DIR").ok();
    let name = std::env::var("ADRENOTOOLS_DRIVER_NAME").ok();
    let hooks = std::env::var("ADRENOTOOLS_HOOKS_PATH")
        .ok()
        .or_else(|| std::env::var("PREFIX").ok().map(|p| format!("{p}/lib")));
    let Some(hooks) = hooks else {
        logf!("ADRENOTOOLS_HOOKS_PATH unset and no PREFIX — skipping adrenotools");
        return;
    };
    let Some(path) = path else {
        logf!("ADRENOTOOLS_DRIVER_PATH unset — skipping adrenotools");
        return;
    };
    if !std::path::Path::new(&path).exists() {
        logf!("ADRENOTOOLS_DRIVER_PATH {path} not accessible — skipping adrenotools");
        return;
    }
    let at = unsafe {
        libc::dlopen(
            c"libadrenotools.so".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        )
    };
    if at.is_null() {
        logf!("dlopen libadrenotools.so failed: {}", dlerror_str());
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
        logf!("adrenotools_open_libvulkan not exported by libadrenotools.so");
        return;
    };
    // Patched copies live in a temp/ dir beside the driver (same layout the
    // wrapper creates); libadrenotools (re)generates them as needed.
    let tmp = format!("{path}{}temp", if path.ends_with('/') { "" } else { "/" });
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
        logf!(
            "adrenotools_open_libvulkan failed for {}{}",
            path,
            name.as_deref().unwrap_or("")
        );
        return;
    }
    let exts = driver_instance_exts(h);
    let has_wayland = ext_list_has(&exts, c"VK_KHR_wayland_surface");
    let has_headless = ext_list_has(&exts, c"VK_EXT_headless_surface");
    let label = name.as_deref().unwrap_or(path.as_str()).to_string();
    if has_wayland {
        logf!("using adrenotools driver {label} (VK_KHR_wayland_surface present)");
        unsafe { DRIVER.handle = h };
        return;
    }
    // No wayland WSI (the whole adrenotools turnip family): present through
    // the WSI proxy (proxy.rs) on top of VK_EXT_headless_surface instead.
    if !has_headless || std::env::var_os("GN_VKSHIM_NO_PROXY").is_some() {
        logf!(
            "adrenotools driver {label} lacks VK_KHR_wayland_surface{} — not usable in GameScope mode",
            if has_headless {
                " (GN_VKSHIM_NO_PROXY set)"
            } else {
                " and VK_EXT_headless_surface"
            }
        );
        return;
    }
    let Some(gipa) = (unsafe { resolve_gipa(h) }) else {
        logf!("adrenotools driver {label} fails the proxy's prerequisite probe (see 'proxy prereq' above) — not usable in GameScope mode");
        return;
    };
    let gipa_fn: GipaFn = gipa;
    if !proxy::device_prereqs(gipa_fn) {
        logf!("adrenotools driver {label} fails the proxy's prerequisite probe (see 'proxy prereq' above) — not usable in GameScope mode");
        return;
    }
    proxy::set_real_gipa(gipa_fn);
    proxy::set_real_handle(h);
    unsafe {
        DRIVER.proxy_active = true;
        DRIVER.handle = h;
    }
    logf!(
        "using adrenotools driver {label} via WSI proxy (headless surface + dma-buf export) [shim {}]",
        SHIM_BUILD
    );
    // X11-mode diagnostic: winex11's XOpenDisplay fails silently, so log
    // exactly what this process sees and probe the same libxcb dial path.
    {
        let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "(unset)".into());
        let dpy = std::env::var("DISPLAY").unwrap_or_else(|_| "(unset)".into());
        logf!("x11 probe: TMPDIR={tmp} DISPLAY={dpy}");
        logf!(
            "x11 probe: XAUTHORITY={} HOME={}",
            std::env::var("XAUTHORITY").unwrap_or_else(|_| "(unset)".into()),
            std::env::var("HOME").unwrap_or_else(|_| "(unset)".into())
        );
        // Lineage check: a gamescope-spawned wine process inherits these from
        // gamescope's env; a process spawned by a stale (X11-mode) wineserver
        // lacks them.
        logf!(
            "x11 probe: GAMESCOPE_X11_DISPLAY={} GAMENATIVE_DRM_SOCKET={} XDG_RUNTIME_DIR={}",
            std::env::var("GAMESCOPE_X11_DISPLAY").unwrap_or_else(|_| "(unset)".into()),
            std::env::var("GAMENATIVE_DRM_SOCKET").unwrap_or_else(|_| "(unset)".into()),
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "(unset)".into())
        );
        probe_xcb_connect();
        probe_raw_dial(&tmp);
        probe_xcb_replica(&tmp);
        probe_xcb_via_fd(&tmp);
    }
}

// Decisive dial-vs-setup split: do our own raw fs connect (proven to work),
// then hand the fd to libxcb's xcb_connect_to_fd with NULL auth — exactly
// what xcb_connect does after a successful dial with no .Xauthority found.
// If this succeeds, the failure lives in the dial; if it fails, the failure
// lives in the setup exchange inside libxcb.
fn probe_xcb_via_fd(tmp: &str) {
    type XcbConnectToFd = unsafe extern "C" fn(c_int, *const c_void) -> *mut c_void;
    type XcbHasError = unsafe extern "C" fn(*mut c_void) -> c_int;
    type XcbDisconnect = unsafe extern "C" fn(*mut c_void);
    let path = format!("{tmp}/.X11-unix/X0");
    let fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        ptr::copy_nonoverlapping(path.as_ptr(), addr.sun_path.as_mut_ptr().cast(), path.len());
        let len = (path.len() + 1) as libc::socklen_t
            + std::mem::size_of::<libc::sa_family_t>() as libc::socklen_t;
        if libc::connect(fd, (&raw const addr).cast::<libc::sockaddr>(), len) != 0 {
            logf!(
                "x11 probe via-fd: raw connect failed: {}",
                std::io::Error::last_os_error()
            );
            libc::close(fd);
            return;
        }
        fd
    };
    let h = unsafe { libc::dlopen(c"libxcb.so.1".as_ptr(), libc::RTLD_NOW) };
    if h.is_null() {
        logf!("x11 probe via-fd: dlopen failed");
        unsafe { libc::close(fd) };
        return;
    }
    unsafe {
        let to_fd: Option<XcbConnectToFd> =
            std::mem::transmute(libc::dlsym(h, c"xcb_connect_to_fd".as_ptr()));
        let has_error: Option<XcbHasError> =
            std::mem::transmute(libc::dlsym(h, c"xcb_connection_has_error".as_ptr()));
        let disconnect: Option<XcbDisconnect> =
            std::mem::transmute(libc::dlsym(h, c"xcb_disconnect".as_ptr()));
        if let (Some(to_fd), Some(has_error)) = (to_fd, has_error) {
            let conn = to_fd(fd, ptr::null());
            let err = has_error(conn);
            logf!("x11 probe via-fd: xcb_connect_to_fd(raw fd, NULL auth) -> error={err} (0=ok)");
            if err == 0 {
                if let Some(disconnect) = disconnect {
                    disconnect(conn);
                }
            }
        } else {
            logf!("x11 probe via-fd: symbols missing");
            libc::close(fd);
        }
        libc::dlclose(h);
    }
}

// Raw connect(2) attempts with errno, to see exactly which dial target the
// session refuses and why (ENOENT = wrong path, EACCES = SELinux,
// ECONNREFUSED = nothing listening there).
fn probe_raw_dial(tmp: &str) {
    let fs_path = format!("{tmp}/.X11-unix/X0");
    let candidates: [(String, bool); 3] = [
        (fs_path, false),
        (format!("{tmp}/.X11-unix/X0"), true),
        ("/tmp/.X11-unix/X0".to_string(), true),
    ];
    for (path, is_abstract) in &candidates {
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            let bytes = path.as_bytes();
            let off = usize::from(*is_abstract);
            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                addr.sun_path.as_mut_ptr().add(off).cast(),
                bytes.len(),
            );
            let len = (off + bytes.len() + 1) as libc::socklen_t
                + std::mem::size_of::<libc::sa_family_t>() as libc::socklen_t;
            let rc = libc::connect(fd, (&raw const addr).cast::<libc::sockaddr>(), len);
            let e = std::io::Error::last_os_error();
            logf!(
                "x11 probe raw: {}{} -> {}",
                if *is_abstract { "abstract " } else { "fs " },
                path,
                if rc == 0 {
                    "CONNECTED".to_string()
                } else {
                    format!("{e} ({})", e.raw_os_error().unwrap_or(-1))
                }
            );
            // On a live connection, complete the X11 setup exchange to learn
            // whether the server would accept this client (status: 1=ok,
            // 0=failed, 2=authenticate).
            if rc == 0 {
                let setup: [u8; 12] = [b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                let wr = libc::write(fd, setup.as_ptr().cast(), setup.len());
                let mut reply = [0u8; 8];
                let rd = libc::read(fd, reply.as_mut_ptr().cast(), reply.len());
                logf!(
                    "x11 probe raw: handshake -> write={wr} read={rd} status={} (1=success,0=fail,2=auth)",
                    if rd > 0 { reply[0] } else { b'?' }
                );
            }
            libc::close(fd);
        }
    }
}

fn probe_xcb_connect() {
    type XcbConnect = unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut c_void;
    type XcbHasError = unsafe extern "C" fn(*mut c_void) -> c_int;
    type XcbDisconnect = unsafe extern "C" fn(*mut c_void);
    let h = unsafe { libc::dlopen(c"libxcb.so.1".as_ptr(), libc::RTLD_NOW) };
    if h.is_null() {
        logf!("x11 probe: dlopen(libxcb.so.1) failed: {}", dlerror_str());
        return;
    }
    unsafe {
        let connect: Option<XcbConnect> =
            std::mem::transmute(libc::dlsym(h, c"xcb_connect".as_ptr()));
        // Identify which libxcb file actually backs this handle.
        if let Some(connect) = connect {
            let mut info: libc::Dl_info = std::mem::zeroed();
            if libc::dladdr(connect as *const c_void, &mut info) != 0 && !info.dli_fname.is_null() {
                logf!(
                    "x11 probe: libxcb resolved from {}",
                    CStr::from_ptr(info.dli_fname).to_string_lossy()
                );
            }
        }
        let has_error: Option<XcbHasError> =
            std::mem::transmute(libc::dlsym(h, c"xcb_connection_has_error".as_ptr()));
        let disconnect: Option<XcbDisconnect> =
            std::mem::transmute(libc::dlsym(h, c"xcb_disconnect".as_ptr()));
        if let (Some(connect), Some(has_error)) = (connect, has_error) {
            let mut screen: c_int = -1;
            let conn = connect(ptr::null(), &mut screen);
            let err = has_error(conn);
            logf!("x11 probe: xcb_connect(DISPLAY) -> error={err} (0=ok,1=io/refused,2=unsupported,3=mem,4=ext,5=closed)");
            if err == 0 {
                if let Some(disconnect) = disconnect {
                    disconnect(conn);
                }
            }
        } else {
            logf!("x11 probe: libxcb symbols missing");
        }
        libc::dlclose(h);
    }
    // The layer above it: winex11 actually calls libX11's XOpenDisplay.
    let hx = unsafe { libc::dlopen(c"libX11.so.6".as_ptr(), libc::RTLD_NOW) };
    if hx.is_null() {
        logf!("x11 probe: dlopen(libX11.so.6) failed: {}", dlerror_str());
        return;
    }
    unsafe {
        type XOpenDisplay = unsafe extern "C" fn(*const c_char) -> *mut c_void;
        type XCloseDisplay = unsafe extern "C" fn(*mut c_void) -> c_int;
        let open: Option<XOpenDisplay> =
            std::mem::transmute(libc::dlsym(hx, c"XOpenDisplay".as_ptr()));
        let close: Option<XCloseDisplay> =
            std::mem::transmute(libc::dlsym(hx, c"XCloseDisplay".as_ptr()));
        if let Some(open) = open {
            let dpy = open(ptr::null());
            logf!(
                "x11 probe: XOpenDisplay(DISPLAY) -> {}",
                if dpy.is_null() {
                    "NULL (winex11's failure reproduced)"
                } else {
                    "OK"
                }
            );
            if !dpy.is_null() {
                if let Some(close) = close {
                    close(dpy);
                }
            }
        } else {
            logf!("x11 probe: XOpenDisplay symbol missing");
        }
        libc::dlclose(hx);
    }
}

// Faithful replication of libxcb's _xcb_open sequence for ":0":
// abstract "\0$TMPDIR/.X11-unix/X0" first, then fs socket with
// SOCK_CLOEXEC, SO_SNDBUF bump, namelen=110 and NO memset (matching
// _xcb_open_unix). Prints errno at each failing step.
fn probe_xcb_replica(tmp: &str) {
    let path = format!("{tmp}/.X11-unix/X0");
    unsafe {
        // 1) abstract attempt (expect ECONNREFUSED -> fallback)
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        ptr::copy_nonoverlapping(
            path.as_ptr(),
            addr.sun_path.as_mut_ptr().add(1).cast(),
            path.len(),
        );
        let len = (1 + path.len() + 1 + 2) as libc::socklen_t;
        let rc = libc::connect(fd, (&raw const addr).cast::<libc::sockaddr>(), len);
        let e = std::io::Error::last_os_error();
        logf!(
            "x11 probe replica: abstract -> {}",
            if rc == 0 {
                "CONNECTED".to_string()
            } else {
                format!("{e} ({})", e.raw_os_error().unwrap_or(-1))
            }
        );
        libc::close(fd);

        // 2) fs attempt, libxcb's exact shape: no memset, namelen=110
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            logf!(
                "x11 probe replica: socket() failed: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        let mut buf = [0u8; 110];
        // deliberately NOT zeroing beyond what we write, like libxcb's
        // uninitialized stack; but for deterministic logging we zero first.
        buf[0] = 1; // sa_family (lo byte, little endian)
        ptr::copy_nonoverlapping(path.as_ptr(), buf.as_mut_ptr().add(2), path.len());
        let rc = libc::connect(fd, buf.as_ptr().cast::<libc::sockaddr>(), 110);
        let e = std::io::Error::last_os_error();
        logf!(
            "x11 probe replica: fs(namelen=110) -> {}",
            if rc == 0 {
                "CONNECTED".to_string()
            } else {
                format!("{e} ({})", e.raw_os_error().unwrap_or(-1))
            }
        );
        libc::close(fd);
    }
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
    if is_gamescope_process() {
        let h = unsafe {
            libc::dlopen(
                c"libvulkan_freedreno.so".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            )
        };
        logf!(
            "gamescope process: payload freedreno {}",
            if h.is_null() {
                dlerror_str()
            } else {
                "loaded".into()
            }
        );
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
            logf!(
                "guest process: falling back to payload freedreno {}",
                if h.is_null() {
                    dlerror_str()
                } else {
                    "loaded".into()
                }
            );
            unsafe { DRIVER.handle = h };
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
            logf!("driver exports no instance proc addr — ICD unusable");
        }
    }
}

fn ensure_init() {
    INIT.call_once(init_once);
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
    // names (device-level ones come through vkGetDeviceProcAddr), but the ICD
    // contract is that it resolves *all* names, so device_func() applies in
    // both modes: a guest that resolved e.g. vkQueueSubmit2 from the driver
    // would hand it wrapper command buffers and fault. Everything else is
    // delegated, which is correct because a thunk only translates handles the
    // shim wraps, and the loader calls these with physical-device handles,
    // terminator instances or no handle at all.
    let fnptr = if unsafe { DRIVER.proxy_active } {
        proxy::instance_func(name)
    } else {
        None
    }
    .or_else(|| proxy::device_func(name));
    if fnptr.is_some() {
        return fnptr;
    }
    unsafe { DRIVER.gipa?(instance, pName) }
}

/// # Safety: loader-ICD v5 interface (called by the Khronos loader).
#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "C" fn vk_icdGetPhysicalDeviceProcAddr(
    instance: VkInstance,
    pName: *const c_char,
) -> PFN_vkVoidFunction {
    ensure_init();
    // Physical-device-level functions (surface queries) resolve through
    // GPDPA as well as GIPA — intercept on both paths, device functions too
    // (see vk_icdGetInstanceProcAddr).
    let name = unsafe { CStr::from_ptr(pName) };
    let fnptr = if unsafe { DRIVER.proxy_active } {
        proxy::instance_func(name)
    } else {
        None
    }
    .or_else(|| proxy::device_func(name));
    if fnptr.is_some() {
        return fnptr;
    }
    unsafe {
        if let Some(gpdpa) = DRIVER.gpdpa {
            return gpdpa(instance, pName);
        }
        DRIVER.gipa?(instance, pName)
    }
}
