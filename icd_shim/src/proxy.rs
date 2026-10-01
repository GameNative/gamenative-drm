//! wayland WSI proxy for guest vulkan drivers that lack
//! VK_KHR_wayland_surface (the whole adrenotools turnip family: no wayland
//! platform compiled in, so no passthrough can ever present on GameScope's
//! wayland socket).
//!
//! The proxy advertises VK_KHR_wayland_surface itself and implements the
//! whole swapchain contract client-side. Nothing hooks-side may ever touch
//! a surface handle: the hooks' surface-query/swapchain trampolines assume
//! loader-wrapped surface objects their WSI never produces, so the
//! swapchain is a shim-owned ring of dma-buf images (system-heap fds
//! IMPORTED into vulkan) the app renders into directly.
//!
//! Backpressure: a slot is reusable only after the compositor's
//! wl_buffer.release, so presentation throttles to compositor rate.
//!
//! Handle ownership: the outer loader stamps its trampoline dispatch data
//! over the first words of the ICD's device handle after vkCreateDevice
//! returns (its terminator_CreateDevice stores into [handle]). The hooks'
//! exported device functions are `ldr x8,[x0]; br [x8,#slot]` stubs that
//! dispatch through that first word, so calling them on a stamped handle
//! reads the OUTER loader's table and lands back in this shim — mutual
//! recursion. The proxy therefore keeps every hooks handle private: the
//! loader only ever sees the shim's own wrapper objects (device, queue,
//! command buffer), each with a scratch prefix the loader may stamp; the
//! hooks' handles underneath stay pristine, so their stubs/trampolines
//! behave exactly as in a direct adrenotools load. Device functions the
//! proxy does not intercept are reached through per-name thunks that swap
//! the wrapper for the real handle and tail-branch to the hooks' export.

#![allow(non_snake_case)]

use crate::thunk;
use crate::vk::*;
use crate::wlproto::{self, wl_display, wl_proxy};
use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicUsize, Ordering};

// First-N diagnostics for the present pipeline handoffs (acquire ->
// present -> worker commit -> compositor release). Bounded counters, so
// steady-state frame pacing stays silent in logcat.
static LOG_ACQ: AtomicUsize = AtomicUsize::new(0);
static LOG_PRESENT: AtomicUsize = AtomicUsize::new(0);
static LOG_SUBMIT: AtomicUsize = AtomicUsize::new(0);

// ------------------------------------------------ bring-up diagnostics (strip)
// An entry point that never returns means the fault is inside that call: wine
// turns the access violation into an SEH exception, so the shim's frame is
// abandoned and nothing after the call runs. The next shim entry reports it
// with the arguments, which distinguishes "our wrapper faulted" from "the
// driver faulted on what we passed it". Entries are timestamped so a call that
// is merely still running on another thread is never mistaken for one that was
// abandoned. Also stamped into the frame dumps, because the startup line
// rotates out of logcat long before a crash.
pub const E_SUBMIT2: usize = 1;
pub const E_SUBMIT: usize = 2;
pub const E_PRESENT: usize = 3;
pub const E_ACQUIRE: usize = 4;
pub const E_PIPELINES: usize = 5;
pub const E_ALLOC_CMD: usize = 6;
pub const E_SCR_CREATE: usize = 7;
pub const E_SCR_DESTROY: usize = 8;

fn entry_name(id: usize) -> &'static str {
    match id {
        E_SUBMIT2 => "vkQueueSubmit2",
        E_SUBMIT => "vkQueueSubmit",
        E_PRESENT => "vkQueuePresentKHR",
        E_ACQUIRE => "vkAcquireNextImageKHR",
        E_PIPELINES => "vkCreateGraphicsPipelines",
        E_ALLOC_CMD => "vkAllocateCommandBuffers",
        E_SCR_CREATE => "vkCreateSwapchainKHR",
        E_SCR_DESTROY => "vkDestroySwapchainKHR",
        _ => "?",
    }
}

struct InFlight {
    id: usize,
    args: [usize; 5],
    at: std::time::Instant,
    token: usize,
}

static IN_FLIGHT: Mutex<Vec<InFlight>> = Mutex::new(Vec::new());
static ENTRY_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Two seconds is far longer than any shim call takes; past that the call was
/// abandoned rather than merely in progress.
const ABANDONED_MS: u128 = 2000;

fn report_abandoned(now_entry: Option<usize>) {
    let Ok(mut v) = IN_FLIGHT.lock() else {
        return;
    };
    let now = std::time::Instant::now();
    v.retain(|e| {
        if now.duration_since(e.at).as_millis() > ABANDONED_MS {
            crate::logf!(
                "proxy: ABANDONED {} args={:x} {:x} {:x} {:x} {:x} -> now in {}",
                entry_name(e.id),
                e.args[0],
                e.args[1],
                e.args[2],
                e.args[3],
                e.args[4],
                now_entry.map_or("(flush)".to_string(), |i| entry_name(i).to_string())
            );
            false
        } else {
            true
        }
    });
}

fn enter(id: usize, args: [usize; 5]) -> usize {
    report_abandoned(Some(id));
    let token = ENTRY_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    if let Ok(mut v) = IN_FLIGHT.lock() {
        v.push(InFlight {
            id,
            args,
            at: std::time::Instant::now(),
            token,
        });
    }
    token
}

fn leave(token: usize) {
    if let Ok(mut v) = IN_FLIGHT.lock() {
        v.retain(|e| e.token != token);
    }
}
static LOG_COMMIT: AtomicUsize = AtomicUsize::new(0);
static LOG_RELEASE: AtomicUsize = AtomicUsize::new(0);
const LOG_FIRST_N: usize = 8;

// Swapchain handshake liveness. The stall class that matters — the app stops
// presenting, or the compositor stops releasing — is invisible under first-N
// budgets, so these sites are RATE-LIMITED instead of budgeted, and the idle
// present worker prints the same line as a heartbeat. Every line carries the
// per-slot flags, so a frozen pipeline always leaves a trail.
static N_ACQ: AtomicUsize = AtomicUsize::new(0);
static N_PRES: AtomicUsize = AtomicUsize::new(0);
static N_REL: AtomicUsize = AtomicUsize::new(0);
static N_COMMIT_OK: AtomicUsize = AtomicUsize::new(0);
static N_STARVE: AtomicUsize = AtomicUsize::new(0);
static RATE_STALL: Mutex<Option<Instant>> = Mutex::new(None);
static RATE_WAIT: Mutex<Option<Instant>> = Mutex::new(None);
/// The last (semaphore, value) the guest asked us to *signal*. A GPU wait for a
/// value the driver never reaches shows up as `want` > `last_signalled`.
static LAST_SIG: Mutex<Option<(String, u64)>> = Mutex::new(None);
static RATE_REL: Mutex<Option<Instant>> = Mutex::new(None);
static RATE_IDLE: Mutex<Option<Instant>> = Mutex::new(None);
const RATE_EVERY: Duration = Duration::from_secs(1);
const IDLE_EVERY: Duration = Duration::from_secs(2);

fn rate_ok(slot: &'static Mutex<Option<Instant>>, every: Duration) -> bool {
    let Ok(mut g) = slot.lock() else {
        return false;
    };
    let now = Instant::now();
    if let Some(prev) = *g {
        if now.duration_since(prev) < every {
            return false;
        }
    }
    *g = Some(now);
    true
}

/// "0:-F 1:A- …": A = held by the app, F = committed and awaiting release.
fn slot_flags(sc: *mut GnSwapchain) -> String {
    let mut flags = String::new();
    let mut in_flight = 0usize;
    let Ok(g) = (unsafe { (*sc).shared.lock() }) else {
        return String::from("<locked>");
    };
    for (i, s) in unsafe { (*sc).slots.iter() }.enumerate() {
        if s.in_flight {
            in_flight += 1;
        }
        flags.push_str(&format!(
            "{i}:{}{} ",
            if s.acquired { "A" } else { "-" },
            if s.in_flight { "F" } else { "-" }
        ));
    }
    drop(g);
    format!(
        "acq={} pres={} rel={} commit={} starved={} in_flight={in_flight} slots=[{}]",
        N_ACQ.load(Ordering::Relaxed),
        N_PRES.load(Ordering::Relaxed),
        N_REL.load(Ordering::Relaxed),
        N_COMMIT_OK.load(Ordering::Relaxed),
        N_STARVE.load(Ordering::Relaxed),
        flags.trim_end()
    )
}

/// Counters only — safe to call while `shared` is held.
fn counters() -> String {
    format!(
        "acq={} pres={} rel={} commit={} starved={}",
        N_ACQ.load(Ordering::Relaxed),
        N_PRES.load(Ordering::Relaxed),
        N_REL.load(Ordering::Relaxed),
        N_COMMIT_OK.load(Ordering::Relaxed),
        N_STARVE.load(Ordering::Relaxed)
    )
}

/// Serializes every send-side wl_connection access (request marshaling +
/// wl_display_flush). libwayland does not internally lock the out-buffer;
/// concurrent send-side access from the present worker and the dispatch
/// thread's internal pre-poll flush corrupts the buffer state, which makes
/// a marshal's inline flush fail EAGAIN and wl_display_fatal() latch the
/// display error — after which wl_display_flush returns -1 forever without
/// sending anything (the "commits never reach the compositor" stall).
static SEND_LOCK: Mutex<()> = Mutex::new(());

/// Routes libwayland's own diagnostics (e.g. "Error marshalling request for
/// X.Y") to logcat; the default handler writes to stderr, which the wine
/// process does not surface.
extern "C" fn gn_wl_log(fmt: *const c_char, ap: *mut c_void) {
    let mut buf = [0 as c_char; 512];
    unsafe { wlproto::vsnprintf(buf.as_mut_ptr(), buf.len(), fmt, ap) };
    let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    crate::logf!("libwayland: {}", msg.trim_end());
}

/// Game FPS measured at the wayland commit point, published once per second
/// to $GAMENATIVE_FPS_FILE (a single float line) for the app's HUD — in
/// Gamescope mode the X11 renderer's frame counter never ticks. The reader
/// treats a stale file as 0, so nothing is written while no frames commit.
static FPS_FILE: OnceLock<Option<std::ffi::OsString>> = OnceLock::new();

fn fps_note_commit() {
    static WINDOW: OnceLock<Mutex<(Instant, u32)>> = OnceLock::new();
    let Some(path) = FPS_FILE
        .get_or_init(|| std::env::var_os("GAMENATIVE_FPS_FILE").filter(|v| !v.is_empty()))
        .as_ref()
    else {
        return;
    };
    let w = WINDOW.get_or_init(|| Mutex::new((Instant::now(), 0)));
    let mut g = w.lock().unwrap();
    g.1 += 1;
    let elapsed = g.0.elapsed();
    if elapsed >= Duration::from_secs(1) {
        let _ = std::fs::write(
            path,
            format!("{:.1}\n", f64::from(g.1) / elapsed.as_secs_f64()),
        );
        *g = (Instant::now(), 0);
    }
}

// -------------------------------- bring-up frame dumps
// dma-buf sync ioctl (same values as the daemon's EGL presenter).
const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_START: u64 = 0 << 2;
const DMA_BUF_SYNC_END: u64 = 1 << 2;
const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;

/// With GAMENATIVE_DUMP_DIR set, a rolling dump of the game's committed
/// buffers is kept as gn-buf-latest.ppm (every DUMP_EVERY-th commit, plus an
/// alpha census and probe pixels in logcat) — so what the game itself
/// rendered can be compared with the compositor's output (the daemon's
/// GNDRM_DUMP_DIR dumps). Slots are LINEAR exports, so a plain mmap reads
/// them correctly; the bytes are R,G,B,X for our XBGR8888 buffers.
static DUMP_DIR: OnceLock<Option<std::ffi::OsString>> = OnceLock::new();
static DUMP_N: AtomicUsize = AtomicUsize::new(0);
const DUMP_EVERY: usize = 30;

fn dump_slot(sc: *mut GnSwapchain, s: *mut GnSlot, idx: i32) {
    let Some(dir) = DUMP_DIR
        .get_or_init(|| std::env::var_os("GAMENATIVE_DUMP_DIR").filter(|v| !v.is_empty()))
        .as_ref()
    else {
        return;
    };
    if unsafe { (*s).modifier } != GN_DRM_MOD_LINEAR {
        static TILED: OnceLock<()> = OnceLock::new();
        TILED.get_or_init(|| crate::logf!("dump: tiled present image, CPU dump disabled"));
        return;
    }
    let n = DUMP_N.fetch_add(1, Ordering::Relaxed);
    if !n.is_multiple_of(DUMP_EVERY) {
        return;
    }
    unsafe {
        let w = (*sc).extent.width as usize;
        let h = (*sc).extent.height as usize;
        let stride = (*s).stride as usize;
        let fd = (*s).fd;
        let len = stride * h;
        let base = libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if base == libc::MAP_FAILED {
            crate::logf!("dump {n}: mmap failed: {}", std::io::Error::last_os_error());
            return;
        }
        let begin = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_START;
        libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &begin);
        let p = (base as *const u8).add((*s).offset as usize);
        let px = |x: usize, y: usize| {
            let q = p.add(y * stride + x * 4);
            (*q, *q.add(1), *q.add(2), *q.add(3))
        };
        let mut a0 = 0usize;
        for y in 0..h {
            for x in 0..w {
                if *p.add(y * stride + x * 4 + 3) == 0 {
                    a0 += 1;
                }
            }
        }
        crate::logf!(
            "dump {n} [shim {}]: slot={idx} {w}x{h} stride={stride} alpha0={a0}/{} px(w/16,h/8)={:?} px(centre)={:?} px(3w/4,3h/4)={:?}",
            crate::SHIM_BUILD,
            w * h,
            px(w / 16, h / 8),
            px(w / 2, h / 2),
            px(3 * w / 4, 3 * h / 4)
        );
        let mut out = Vec::with_capacity(w * h * 3 + 16);
        out.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
        for y in 0..h {
            for x in 0..w {
                // The slot is BGRA in memory (the pool's layout), so the red
                // channel is byte 2 — writing 0,1,2 as P6 "R,G,B" would swap
                // red and blue in the dump and every colour read from it.
                let q = p.add(y * stride + x * 4);
                out.extend_from_slice(&[*q.add(2), *q.add(1), *q]);
            }
        }
        let path = std::path::Path::new(dir).join("gn-buf-latest.ppm");
        match std::fs::write(&path, &out) {
            Ok(()) => crate::logf!("dump {n}: wrote {}", path.display()),
            Err(e) => crate::logf!("dump {n}: write failed: {e}"),
        }
        let end = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END;
        libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &end);
        libc::munmap(base, len);
    }
}
use std::ptr;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const GN_MAX_SC_IMAGES: usize = 8;
pub const GN_MAX_PRESENT: usize = 8; // swapchainCount handled per vkQueuePresentKHR
pub const GN_JOB_CAP: usize = 16;

pub const fn gn_fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}
pub const GN_DRM_FORMAT_XRGB8888: u32 = gn_fourcc(b'X', b'R', b'2', b'4');
pub const GN_DRM_FORMAT_XBGR8888: u32 = gn_fourcc(b'X', b'B', b'2', b'4');
pub const GN_DRM_MOD_LINEAR: u64 = 0;
// Qualcomm scanout layouts (DRM_FORMAT_MOD_QCOM_*): COMPRESSED is UBWC, TILED3
// the same macrotile layout without compression. Either names the layout of a
// present image outright, instead of the explicit-LINEAR export the shim
// otherwise has to force (a LINEAR export is the only layout it can map and
// read, and the only one the driver cannot silently replace with a compressed
// payload). GN_VKSHIM_SC_MOD picks the candidates, LINEAR always last.
pub const GN_DRM_MOD_QCOM_COMPRESSED: u64 = 0x0500_0000_0000_0001;
pub const GN_DRM_MOD_QCOM_TILED3: u64 = 0x0500_0000_0000_0003;

fn sc_modifiers() -> &'static [u64] {
    match std::env::var("GN_VKSHIM_SC_MOD").as_deref() {
        Ok("compressed") => &[GN_DRM_MOD_QCOM_COMPRESSED],
        Ok("tiled3") => &[GN_DRM_MOD_QCOM_TILED3],
        Ok("auto") => &[GN_DRM_MOD_QCOM_COMPRESSED, GN_DRM_MOD_QCOM_TILED3],
        _ => &[],
    }
}

/// Swapchain image ceiling from the container config (`WRAPPER_MAX_IMAGE_COUNT`,
/// 0 = the wrapper's default). The proxy caps both the `maxImageCount` it
/// advertises and its ring to the same value, so the guest's chosen image count
/// and the pool it is handed agree. A value below minImageCount (2) is not a
/// legal `maxImageCount` and is ignored.
fn max_swapchain_images() -> u32 {
    match std::env::var("WRAPPER_MAX_IMAGE_COUNT")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        Some(v) if v >= 2 => v.min(GN_MAX_SC_IMAGES as u32),
        _ => GN_MAX_SC_IMAGES as u32,
    }
}

/// Swapchain/WSI extensions the guest must not see. The proxy owns the
/// swapchain and implements none of their entry points, so a guest that
/// enabled one would call it through the wrapper-swapping thunk with a shim
/// swapchain handle the driver has never seen — and a title pacing against a
/// present_wait that never signals presents unfinished frames.
fn unimplemented_wsi_ext(name: &CStr) -> bool {
    name == c"VK_KHR_present_wait"
        || name == c"VK_KHR_present_id"
        || name == c"VK_EXT_swapchain_maintenance1"
}

// ---------------------------------------------------------------- objects

#[derive(Default, Clone, Copy)]
pub struct RealInstFns {
    pub gdp: Option<PFN_vkGetDeviceProcAddr>,
    pub enum_pds: Option<PFN_vkEnumeratePhysicalDevices>,
    pub create_device: Option<PFN_vkCreateDevice>,
    pub destroy_instance: Option<PFN_vkDestroyInstance>,
}

#[derive(Default, Clone, Copy)]
pub struct RealDevFns {
    pub gdp: Option<PFN_vkGetDeviceProcAddr>,
    pub get_queue: Option<PFN_vkGetDeviceQueue>,
    pub create_image: Option<PFN_vkCreateImage>,
    pub create_pipelines: Option<PFN_vkCreateGraphicsPipelines>,
    pub get_device_queue2: Option<PFN_vkGetDeviceQueue2>,
    pub submit2: Option<PFN_vkQueueSubmit2>,
    pub wait_semaphores:
        Option<unsafe extern "C" fn(VkDevice, *const VkSemaphoreWaitInfo, u64) -> VkResult>,
    pub destroy_image: Option<PFN_vkDestroyImage>,
    pub get_mem_reqs: Option<PFN_vkGetImageMemoryRequirements>,
    pub get_layout: Option<PFN_vkGetImageSubresourceLayout>,
    pub alloc_mem: Option<PFN_vkAllocateMemory>,
    pub free_mem: Option<PFN_vkFreeMemory>,
    pub bind_mem: Option<PFN_vkBindImageMemory>,
    pub create_fence: Option<PFN_vkCreateFence>,
    pub destroy_fence: Option<PFN_vkDestroyFence>,
    pub wait_fences: Option<PFN_vkWaitForFences>,
    pub reset_fences: Option<PFN_vkResetFences>,
    pub create_sem: Option<PFN_vkCreateSemaphore>,
    pub destroy_sem: Option<PFN_vkDestroySemaphore>,
    pub alloc_cmd: Option<PFN_vkAllocateCommandBuffers>,
    pub free_cmd: Option<PFN_vkFreeCommandBuffers>,
    pub submit: Option<PFN_vkQueueSubmit>,
    pub wait_idle: Option<PFN_vkDeviceWaitIdle>,
    pub destroy_device: Option<PFN_vkDestroyDevice>,
}

pub struct GnInstance {
    pub real: VkInstance,
    pub f: RealInstFns,
    pub pds: [VkPhysicalDevice; 8],
    pub n_pds: u32,
    pub next: *mut GnInstance,
}

/// Scratch words at the top of every shim-owned dispatchable handle: the
/// outer loader stamps its trampoline data over the handle's first words
/// after creation, so the fields it may clobber must not hold anything.
pub const GN_HANDLE_SCRATCH: usize = 6;

#[repr(C)]
pub struct GnDevice {
    pub scratch: [usize; GN_HANDLE_SCRATCH],
    pub real: VkDevice,
    pub inst: *mut GnInstance,
    pub f: RealDevFns,
    pub queues: *mut GnQueue, // wrapper list
    pub cmds: *mut GnCmd,     // wrapper list
    pub next: *mut GnDevice,
}

#[repr(C)]
pub struct GnQueue {
    pub scratch: [usize; GN_HANDLE_SCRATCH],
    pub real: VkQueue,
    pub gd: *mut GnDevice,
    pub next: *mut GnQueue,
}

#[repr(C)]
pub struct GnCmd {
    pub scratch: [usize; GN_HANDLE_SCRATCH],
    pub real: VkCommandBuffer,
    pub gd: *mut GnDevice,
    pub next: *mut GnCmd,
}

pub struct GnSurface {
    pub inst: *mut GnInstance,
    pub dpy: *mut wl_display,         // winewayland's, or our own (X11 mode)
    pub parent: *mut wl_proxy,        // commit target wl_surface
    pub own_surface: *mut wl_proxy,   // X11 mode: shim-created wl_surface
    pub swapchain_obj: *mut wl_proxy, // X11 mode: gamescope_swapchain
    pub next: *mut GnSurface,
}

pub struct GnSlot {
    pub owner: *mut GnSwapchain, // release listener -> slot free
    pub image: VkImage,
    pub mem: VkDeviceMemory,
    pub fd: i32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    pub buf: *mut wl_proxy, // created on first present (worker thread)
    pub fence: VkFence,
    pub sem: VkSemaphore, // chains multi-swapchain presents
    pub acquired: bool,   // owned by the app (acquire -> present)
    pub in_flight: bool,  // committed, awaiting wl_buffer.release
}

/// Swapchain state shared by the app threads (acquire/present), the
/// present worker and the wayland release listener — one mutex, two
/// condvars (put = job queued, free = slot released), like the C.
pub struct ScShared {
    pub jobs: VecDeque<i32>,
    pub stopping: bool,
}

pub struct GnSwapchain {
    pub surf: *mut GnSurface,
    pub dev: *mut GnDevice,
    pub format: VkFormat,
    pub extent: VkExtent2D,
    pub fourcc: u32,
    pub usage: VkImageUsageFlags,
    pub slots: Vec<GnSlot>,
    pub worker: Option<std::thread::JoinHandle<()>>,
    pub shared: Mutex<ScShared>,
    pub cv_put: Condvar,
    pub cv_free: Condvar,
    pub next: *mut GnSwapchain,
}

// ---------------------------------------------------------------- wayland

pub struct GnWayland {
    pub dpy: *mut wl_display, // winewayland's, or our own (X11 mode)
    pub comp: *mut wl_proxy,
    pub subcomp: *mut wl_proxy,
    pub dmabuf: *mut wl_proxy,
    pub swapchain_factory: *mut wl_proxy, // gamescope_swapchain_factory_v2
    pub queue: *mut wlproto::wl_event_queue,
    pub dispatch_started: bool,
    pub own_dpy: bool, // X11 mode: we connected (WAYLAND_DISPLAY env)
    pub ready: bool,
    pub failed: bool,
}

// Touched from the setup thread, the dispatch thread and present workers
// (raw wayland pointers — the C's locking model is kept verbatim).
unsafe impl Send for GnWayland {}

static G_WL: Mutex<GnWayland> = Mutex::new(GnWayland {
    dpy: ptr::null_mut(),
    comp: ptr::null_mut(),
    subcomp: ptr::null_mut(),
    dmabuf: ptr::null_mut(),
    swapchain_factory: ptr::null_mut(),
    queue: ptr::null_mut(),
    dispatch_started: false,
    own_dpy: false,
    ready: false,
    failed: false,
});

/// wl_registry.global handler (dispatch thread).
extern "C" fn registry_global(
    data: *mut c_void,
    reg: *mut wl_proxy,
    id: u32,
    iface: *const c_char,
    version: u32,
) {
    let w = data as *mut GnWayland;
    let iface = unsafe { CStr::from_ptr(iface) }.to_bytes();
    unsafe {
        if iface == b"wl_compositor" && version >= 4 {
            (*w).comp = wlproto::registry_bind(reg, id, &wlproto::wl_compositor_interface, 4);
        } else if iface == b"wl_subcompositor" {
            (*w).subcomp = wlproto::registry_bind(reg, id, &wlproto::wl_subcompositor_interface, 1);
        } else if iface == b"zwp_linux_dmabuf_v1" && version >= 3 {
            (*w).dmabuf =
                wlproto::registry_bind(reg, id, &wlproto::ZWP_LINUX_DMABUF_V1_INTERFACE, 3);
        } else if iface == b"gamescope_swapchain_factory_v2" {
            (*w).swapchain_factory = wlproto::registry_bind(
                reg,
                id,
                &wlproto::GAMESCOPE_SWAPCHAIN_FACTORY_V2_INTERFACE,
                1,
            );
        }
    }
}

extern "C" fn registry_remove(_data: *mut c_void, _reg: *mut wl_proxy, _id: u32) {}

// Listener vtables: libwayland indexes them by event opcode. Stored as raw
// fn pointers — wl_proxy_add_listener takes an untyped implementation array.
type RawListenerFn = extern "C" fn();
static REGISTRY_LISTENER: [RawListenerFn; 2] = [
    unsafe {
        std::mem::transmute::<
            extern "C" fn(*mut c_void, *mut wl_proxy, u32, *const c_char, u32),
            RawListenerFn,
        >(registry_global)
    },
    unsafe {
        std::mem::transmute::<extern "C" fn(*mut c_void, *mut wl_proxy, u32), RawListenerFn>(
            registry_remove,
        )
    },
];
static BUFFER_LISTENER: [RawListenerFn; 1] = [unsafe {
    std::mem::transmute::<extern "C" fn(*mut c_void, *mut wl_proxy), RawListenerFn>(
        on_buffer_release,
    )
}];

fn dispatch_main() {
    let (dpy, queue) = {
        let w = G_WL.lock().unwrap();
        (w.dpy, w.queue)
    };
    let fd = unsafe { wlproto::wl_display_get_fd(dpy) };
    let mut failed = false;
    // Canonical multi-threaded libwayland read loop: flushes go through
    // SEND_LOCK (never concurrent with another sender, see SEND_LOCK), the
    // blocking poll happens with no lock held, and event dispatch runs
    // lock-free on this thread. wl_display_dispatch_queue() cannot be used:
    // its internal pre-poll flush races the present worker's marshal+flush.
    'outer: loop {
        {
            let _sg = SEND_LOCK.lock().unwrap();
            if unsafe { wlproto::wl_display_flush(dpy) } < 0
                && std::io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN)
            {
                failed = true;
                break;
            }
        }
        loop {
            let n = unsafe { wlproto::wl_display_dispatch_queue_pending(dpy, queue) };
            if n <= 0 {
                if n < 0 {
                    failed = true;
                }
                break;
            }
        }
        if failed {
            break;
        }
        while unsafe { wlproto::wl_display_prepare_read_queue(dpy, queue) } != 0 {
            if unsafe { wlproto::wl_display_dispatch_queue_pending(dpy, queue) } < 0 {
                failed = true;
                break 'outer;
            }
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, -1) } < 0 {
            let e = std::io::Error::last_os_error().raw_os_error();
            unsafe { wlproto::wl_display_cancel_read(dpy) };
            if e == Some(libc::EINTR) {
                continue;
            }
            failed = true;
            break;
        }
        if unsafe { wlproto::wl_display_read_events(dpy) } < 0 {
            failed = true;
            break;
        }
    }
    let err = unsafe { wlproto::wl_display_get_error(dpy) };
    G_WL.lock().unwrap().failed = true;
    crate::logf!(
        "wayland dispatch thread exited: {}",
        if err != 0 {
            std::io::Error::from_raw_os_error(err).to_string()
        } else if failed {
            std::io::Error::last_os_error().to_string()
        } else {
            "display closed".to_string()
        }
    );
}

/// Binds the globals the proxy needs on the gamescope connection (once per
/// process) and starts the dispatch thread for our private event queue.
/// `dpy` null means X11 mode: wine has no wayland display, so the shim opens
/// its own connection (gamescope sets WAYLAND_DISPLAY for its children).
/// wl_compositor >= 4 is required for wl_surface_damage_buffer.
fn wl_setup(dpy: *mut wl_display) -> bool {
    let mut w = G_WL.lock().unwrap();
    if w.ready || w.failed {
        return !w.failed;
    }
    let wptr: *mut GnWayland = &mut *w;
    unsafe {
        let dpy = if dpy.is_null() {
            let c = wlproto::wl_display_connect(std::ptr::null());
            if c.is_null() {
                crate::logf!("proxy wayland connect failed (WAYLAND_DISPLAY unset?)");
                (*wptr).failed = true;
                return false;
            }
            (*wptr).own_dpy = true;
            c
        } else {
            dpy
        };
        (*wptr).dpy = dpy;
        wlproto::wl_log_set_handler_client(gn_wl_log);
        (*wptr).queue = wlproto::wl_display_create_queue(dpy);
        let reg = wlproto::display_get_registry(dpy);
        wlproto::wl_proxy_set_queue(reg, (*wptr).queue);
        wlproto::wl_proxy_add_listener(reg, REGISTRY_LISTENER.as_ptr() as *mut _, wptr.cast());
        if wlproto::wl_display_roundtrip_queue(dpy, (*wptr).queue) < 0 {
            (*wptr).failed = true;
        }
        wlproto::wl_proxy_destroy(reg);
        if !(*wptr).failed && (*wptr).dmabuf.is_null() {
            crate::logf!("compositor misses zwp_linux_dmabuf_v1 v3 — proxy cannot present");
            (*wptr).failed = true;
        }
        if !(*wptr).failed {
            let spawned = std::thread::Builder::new()
                .name("gn-wl-dispatch".into())
                .spawn(dispatch_main);
            if spawned.is_err() {
                crate::logf!("failed to start wayland dispatch thread");
                (*wptr).failed = true;
            }
        }
        (*wptr).ready = !(*wptr).failed;
        !(*wptr).failed
    }
}

/// wl_buffer.release handler (dispatch thread): the slot is reusable.
extern "C" fn on_buffer_release(data: *mut c_void, _buf: *mut wl_proxy) {
    let slot = data as *mut GnSlot;
    unsafe {
        let sc = (*slot).owner;
        if LOG_RELEASE.fetch_add(1, Ordering::Relaxed) < LOG_FIRST_N {
            crate::logf!("proxy release: slot={slot:?}");
        }
        let _g = (*sc).shared.lock().unwrap();
        (*slot).in_flight = false;
        (*sc).cv_free.notify_all();
        drop(_g);
        N_REL.fetch_add(1, Ordering::Relaxed);
        if rate_ok(&RATE_REL, RATE_EVERY) {
            crate::logf!("gn-vkshim release: slot={slot:?} {}", slot_flags(sc));
        }
    }
}

// ------------------------------------------------------------- bookkeeping

pub struct Globals {
    pub real_gipa: Option<GipaFn>,
    pub real_handle: *mut c_void,
    pub instances: *mut GnInstance,
    pub devices: *mut GnDevice,
    pub surfaces: *mut GnSurface,
    pub swapchains: *mut GnSwapchain,
}

// All list heads and the real-driver pointers sit behind one mutex (the
// C's g_mtx); raw list pointers are only walked under it.
unsafe impl Send for Globals {}

pub static G: Mutex<Globals> = Mutex::new(Globals {
    real_gipa: None,
    real_handle: ptr::null_mut(),
    instances: ptr::null_mut(),
    devices: ptr::null_mut(),
    surfaces: ptr::null_mut(),
    swapchains: ptr::null_mut(),
});

pub fn set_real_gipa(gipa: GipaFn) {
    G.lock().unwrap().real_gipa = Some(gipa);
}

pub fn set_real_handle(handle: *mut c_void) {
    G.lock().unwrap().real_handle = handle;
}

unsafe fn find_instance(inst: VkInstance) -> *mut GnInstance {
    let g = G.lock().unwrap();
    let mut p = g.instances;
    while !p.is_null() {
        if (*p).real == inst {
            return p;
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

unsafe fn find_instance_by_pd(pd: VkPhysicalDevice) -> *mut GnInstance {
    let g = G.lock().unwrap();
    let mut p = g.instances;
    while !p.is_null() {
        for i in 0..(*p).n_pds as usize {
            if (*p).pds[i] == pd {
                return p;
            }
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

unsafe fn find_device(dev: VkDevice) -> *mut GnDevice {
    let g = G.lock().unwrap();
    let mut p = g.devices;
    while !p.is_null() {
        if p.cast::<c_void>() == dev {
            return p;
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

unsafe fn find_queue(q: VkQueue) -> *mut GnQueue {
    let g = G.lock().unwrap();
    let mut p = g.devices;
    while !p.is_null() {
        let mut gq = (*p).queues;
        while !gq.is_null() {
            if gq.cast::<c_void>() == q {
                return gq;
            }
            gq = (*gq).next;
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

/// Command buffers are looked up across every device, not just the queue's:
/// a handle the shim never wrapped is passed through untouched, and a wrapper
/// that belongs to another device still has to be translated (a miss would
/// hand the driver a shim pointer).
// Every command-buffer wrapper the shim ever handed out, so a lookup miss can
// be classified: a handle that was a wrapper but is no longer in any list is a
// shim bug (the driver would receive a shim pointer), while an unknown handle
// is one the guest got from the driver itself and must be passed through.
static WRAPPER_HISTORY: Mutex<Vec<usize>> = Mutex::new(Vec::new());

fn noted_wrapper(p: usize) {
    if let Ok(mut v) = WRAPPER_HISTORY.lock() {
        if v.len() < 4096 {
            v.push(p);
        }
    }
}

fn was_wrapper(p: usize) -> bool {
    WRAPPER_HISTORY
        .lock()
        .map(|v| v.contains(&p))
        .unwrap_or(false)
}

unsafe fn find_cmd_any(cmd: VkCommandBuffer) -> *mut GnCmd {
    let g = G.lock().unwrap();
    let mut d = g.devices;
    while !d.is_null() {
        unsafe {
            let mut gc = (*d).cmds;
            while !gc.is_null() {
                if gc.cast::<c_void>() == cmd {
                    return gc;
                }
                gc = (*gc).next;
            }
            d = (*d).next;
        }
    }
    ptr::null_mut()
}

unsafe fn real_cmd(cmd: VkCommandBuffer) -> VkCommandBuffer {
    let gc = unsafe { find_cmd_any(cmd) };
    if !gc.is_null() {
        return unsafe { (*gc).real };
    }
    // Not in any device's list. Pass it through: a handle the shim never
    // wrapped is the driver's own. But say so when it *was* a wrapper —
    // that case hands the driver a shim pointer and faults inside it.
    if was_wrapper(cmd as usize) {
        crate::logf!("proxy: BUG submit carries an untracked wrapper CB {cmd:?}");
    }
    cmd
}

unsafe fn find_cmd(gd: *mut GnDevice, cmd: VkCommandBuffer) -> *mut GnCmd {
    let mut gc = (*gd).cmds;
    while !gc.is_null() {
        if gc.cast::<c_void>() == cmd {
            return gc;
        }
        gc = (*gc).next;
    }
    ptr::null_mut()
}

unsafe fn find_surface(surface: VkSurfaceKHR) -> *mut GnSurface {
    let g = G.lock().unwrap();
    let mut p = g.surfaces;
    while !p.is_null() {
        if p as usize as VkSurfaceKHR == surface {
            return p;
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

unsafe fn find_swapchain(sc: VkSwapchainKHR) -> *mut GnSwapchain {
    let g = G.lock().unwrap();
    let mut p = g.swapchains;
    while !p.is_null() {
        if p as usize as VkSwapchainKHR == sc {
            return p;
        }
        p = (*p).next;
    }
    ptr::null_mut()
}

/// Resolves a real device-level function by name without touching the
/// hooks' vkGetDeviceProcAddr (its unknown-name path tail-calls a poison
/// slot). Order: the hooks' exported vk* entries, then the instance GIPA
/// (falls through to the real driver's name tables — mesa resolves device
/// functions from a live instance), then None. Both are safe to call with
/// the hooks' handles because those handles are never handed to the outer
/// loader: their first word keeps pointing at the hooks' own dispatch
/// table (see "Handle ownership" above).
/// Vulkan 1.4/maintenance core names the driver only exports under the
/// extension spelling it implemented first.
fn alias_of(name: &CStr) -> Option<&'static str> {
    Some(match name.to_str().ok()? {
        "vkMapMemory2" => "vkMapMemory2KHR",
        "vkUnmapMemory2" => "vkUnmapMemory2KHR",
        "vkGetImageSubresourceLayout2" => "vkGetImageSubresourceLayout2KHR",
        "vkGetDeviceImageSubresourceLayout" => "vkGetDeviceImageSubresourceLayoutKHR",
        "vkCopyMemoryToImage" => "vkCopyMemoryToImageEXT",
        "vkCopyImageToMemory" => "vkCopyImageToMemoryEXT",
        "vkCopyImageToImage" => "vkCopyImageToImageEXT",
        "vkCmdBindIndexBuffer2" => "vkCmdBindIndexBuffer2KHR",
        "vkCmdBindDescriptorSets2" => "vkCmdBindDescriptorSets2KHR",
        "vkCmdPushConstants2" => "vkCmdPushConstants2KHR",
        "vkCmdPushDescriptorSet" => "vkCmdPushDescriptorSetKHR",
        "vkCmdPushDescriptorSet2" => "vkCmdPushDescriptorSet2KHR",
        "vkCmdPushDescriptorSetWithTemplate" => "vkCmdPushDescriptorSetWithTemplateKHR",
        "vkCmdPushDescriptorSetWithTemplate2" => "vkCmdPushDescriptorSetWithTemplate2KHR",
        "vkCmdSetRenderingAttachmentLocations" => "vkCmdSetRenderingAttachmentLocationsKHR",
        "vkCmdSetRenderingInputAttachmentIndices" => "vkCmdSetRenderingInputAttachmentIndicesKHR",
        "vkGetRenderingAreaGranularity" => "vkGetRenderingAreaGranularityKHR",
        "vkCmdSetLineStipple" => "vkCmdSetLineStippleEXT",
        _ => return None,
    })
}

unsafe fn gn_real_device_proc(gd: *mut GnDevice, name: &CStr) -> PFN_vkVoidFunction {
    let (handle, gipa, inst) = {
        let g = G.lock().unwrap();
        let inst = if !gd.is_null() {
            (*(*gd).inst).real
        } else if !g.instances.is_null() {
            (*g.instances).real
        } else {
            ptr::null_mut()
        };
        (g.real_handle, g.real_gipa, inst)
    };
    let mut fnptr: PFN_vkVoidFunction = None;
    let mut via = "miss";
    // Which handle a name resolves through decides whether the guest gets the
    // driver's own implementation or the hooks' replacement. The copy/memory
    // entry points are exactly where the guest's calls came back empty, so
    // allow forcing the driver's own (GN_VKSHIM_PREFER_DRIVER_PROC=1).
    let prefer_driver = std::env::var_os("GN_VKSHIM_PREFER_DRIVER_PROC").is_some();
    if !handle.is_null() && !prefer_driver {
        let sym = unsafe { libc::dlsym(handle, name.as_ptr()) };
        if !sym.is_null() {
            via = "dlsym";
            fnptr =
                Some(unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn()>(sym) });
        }
    }
    if fnptr.is_none() && !inst.is_null() {
        if let Some(gipa) = gipa {
            fnptr = unsafe { gipa(inst, name.as_ptr()) };
            if fnptr.is_some() {
                via = "gipa";
            }
        }
    }
    // Core names the driver only exports under an extension spelling.
    if fnptr.is_none() {
        if let Some(alias) = alias_of(name) {
            let c = std::ffi::CString::new(alias).unwrap();
            fnptr = unsafe { gn_real_device_proc(gd, &c) };
            if fnptr.is_some() {
                via = "alias";
            }
        }
    }
    // The image/memory entry points are where the guest's calls silently do
    // nothing, so record where each one came from.
    if let Ok(mut seen) = REAL_PROC_SEEN.lock() {
        let n = name.to_string_lossy().into_owned();
        if seen.len() < 128
            && !seen.iter().any(|s| s == &n)
            && (n.contains("Copy")
                || n.contains("Clear")
                || n.contains("Bind")
                || n.contains("Allocate")
                || n.contains("Map")
                || n.contains("CreateImage")
                || n.contains("SubresourceLayout"))
        {
            crate::logf!("proxy: real proc {n} via {via}");
            seen.push(n);
        }
    }
    fnptr
}

/// Instance-level resolution likewise prefers the hooks' exported vk*
/// entries: those are the same public entry points an app links against,
/// accepting the static loader's object model. The instance GIPA's answers
/// are internal trampolines that assume loader-wrapped surfaces (they read
/// [surface] as a dispatch table); only fall back to it for names the hooks
/// don't export (vkCreateHeadlessSurfaceEXT).
unsafe fn gn_real_inst_proc(inst: VkInstance, name: &CStr) -> PFN_vkVoidFunction {
    let (handle, gipa) = {
        let g = G.lock().unwrap();
        (g.real_handle, g.real_gipa)
    };
    if !handle.is_null() {
        let sym = unsafe { libc::dlsym(handle, name.as_ptr()) };
        if !sym.is_null() {
            return Some(unsafe {
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn()>(sym)
            });
        }
    }
    if let Some(gipa) = gipa {
        return unsafe { gipa(inst, name.as_ptr()) };
    }
    None
}

// Chain-link resolvers for the create-info we hand to the hooks. The outer
// loader appends a VkLayer{Instance,Device}CreateInfo link to the pNext
// chain; forwarded verbatim, the hooks initialize their dispatch tables
// from it — resolving through the outer loader into THIS shim's entry
// points, which lands our gn_* functions in their device/queue slots and
// recurses (our QueueSubmit → hooks forwarder → our GetDeviceQueue → …)
// until the stack overflows. Terminating the chain with resolvers confined
// to the hooks' own namespace keeps every slot they initialize consistent
// with their own CreateDevice.
unsafe extern "C" fn gn_chain_gipa(
    instance: VkInstance,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    let gipa = G.lock().unwrap().real_gipa;
    match gipa {
        Some(gipa) => unsafe { gipa(instance, name) },
        None => None,
    }
}

unsafe extern "C" fn gn_chain_gpdpa(
    instance: VkInstance,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    unsafe { gn_chain_gipa(instance, name) }
}

unsafe extern "C" fn gn_chain_gdpa(_device: VkDevice, name: *const c_char) -> PFN_vkVoidFunction {
    unsafe { gn_real_device_proc(ptr::null_mut(), CStr::from_ptr(name)) }
}

// Statics shared with the loader's chain links (contents never change).
struct SyncLink<T>(T);
unsafe impl<T> Sync for SyncLink<T> {}

static G_CHAIN_INST_LINK: SyncLink<VkLayerInstanceLink> = SyncLink(VkLayerInstanceLink {
    pNext: ptr::null_mut(),
    pfnNextGetInstanceProcAddr: Some(unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction,
            unsafe extern "C" fn(),
        >(gn_chain_gipa)
    }),
    pfnNextGetPhysicalDeviceProcAddr: Some(unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction,
            unsafe extern "C" fn(),
        >(gn_chain_gpdpa)
    }),
});

static G_CHAIN_DEV_LINK: SyncLink<VkLayerDeviceLink> = SyncLink(VkLayerDeviceLink {
    pNext: ptr::null_mut(),
    pfnNextGetInstanceProcAddr: Some(unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction,
            unsafe extern "C" fn(),
        >(gn_chain_gipa)
    }),
    pfnNextGetDeviceProcAddr: Some(unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(VkDevice, *const c_char) -> PFN_vkVoidFunction,
            unsafe extern "C" fn(),
        >(gn_chain_gdpa)
    }),
});

/// from_raw_parts that tolerates the vulkan "NULL pointer with zero count"
/// idiom (creating a slice from (NULL, 0) is UB in Rust even when never
/// read — and the UB-checking debug build aborts on it).
unsafe fn slice_or_empty<'a, T>(p: *const T, n: usize) -> &'a [T] {
    if p.is_null() || n == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(p, n) }
    }
}

// ------------------------------------------------------- prereq probe

fn dev_exts_satisfied(
    enum_de: PFN_vkEnumerateDeviceExtensionProperties,
    pd: VkPhysicalDevice,
) -> bool {
    const WANT: [&CStr; 4] = [
        c"VK_KHR_external_memory",
        c"VK_KHR_external_memory_fd",
        c"VK_EXT_external_memory_dma_buf",
        c"VK_EXT_image_drm_format_modifier",
    ];
    let mut n: u32 = 0;
    if unsafe { enum_de(pd, ptr::null(), &mut n, ptr::null_mut()) } != VK_SUCCESS {
        return false;
    }
    let mut props = vec![
        VkExtensionProperties {
            extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
            specVersion: 0,
        };
        n as usize
    ];
    if unsafe { enum_de(pd, ptr::null(), &mut n, props.as_mut_ptr()) } != VK_SUCCESS {
        return false;
    }
    for want in WANT {
        let found = props[..n as usize].iter().any(|p| {
            let name = unsafe { CStr::from_ptr(p.extensionName.as_ptr()) };
            name == want
        });
        if !found {
            crate::logf!("proxy prereq: device lacks {}", want.to_str().unwrap());
            return false;
        }
    }
    true
}

/// Probes the driver's first physical device (throwaway instance) for the
/// device extensions the proxy's dma-buf export path needs.
pub fn device_prereqs(gipa: GipaFn) -> bool {
    let create_inst: Option<PFN_vkCreateInstance> =
        unsafe { std::mem::transmute(gipa(ptr::null_mut(), c"vkCreateInstance".as_ptr())) };
    let Some(create_inst) = create_inst else {
        crate::logf!("proxy prereq: driver exposes no vkCreateInstance");
        return false;
    };
    let app = VkApplicationInfo {
        sType: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        pNext: ptr::null(),
        pApplicationName: c"gn-vkshim-probe".as_ptr(),
        applicationVersion: 0,
        pEngineName: ptr::null(),
        engineVersion: 0,
        apiVersion: VK_API_VERSION_1_3,
    };
    let ci = VkInstanceCreateInfo {
        sType: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        pNext: ptr::null(),
        flags: 0,
        pApplicationInfo: &app,
        enabledLayerCount: 0,
        ppEnabledLayerNames: ptr::null(),
        enabledExtensionCount: 0,
        ppEnabledExtensionNames: ptr::null(),
    };
    let mut inst: VkInstance = ptr::null_mut();
    let res = unsafe { create_inst(&ci, ptr::null(), &mut inst) };
    if res != VK_SUCCESS {
        crate::logf!("proxy prereq: probe vkCreateInstance failed ({res})");
        return false;
    }
    // Instance-level procs must be fetched against a live instance: mesa
    // resolves only global functions for a NULL instance.
    let enum_pds: Option<PFN_vkEnumeratePhysicalDevices> =
        unsafe { std::mem::transmute(gipa(inst, c"vkEnumeratePhysicalDevices".as_ptr())) };
    let enum_de: Option<PFN_vkEnumerateDeviceExtensionProperties> = unsafe {
        std::mem::transmute(gipa(inst, c"vkEnumerateDeviceExtensionProperties".as_ptr()))
    };
    let destroy_inst: Option<PFN_vkDestroyInstance> =
        unsafe { std::mem::transmute(gipa(inst, c"vkDestroyInstance".as_ptr())) };
    let mut ok = false;
    match (enum_pds, enum_de, destroy_inst) {
        (Some(enum_pds), Some(enum_de), _) => {
            let mut n: u32 = 0;
            if unsafe { enum_pds(inst, &mut n, ptr::null_mut()) } == VK_SUCCESS && n > 0 {
                let mut pd: VkPhysicalDevice = ptr::null_mut();
                let mut one: u32 = 1;
                if unsafe { enum_pds(inst, &mut one, &mut pd) } == VK_SUCCESS {
                    ok = dev_exts_satisfied(enum_de, pd);
                }
            } else {
                crate::logf!("proxy prereq: no physical devices");
            }
        }
        _ => crate::logf!("proxy prereq: instance-level procs unresolved"),
    }
    if let Some(destroy_inst) = destroy_inst {
        unsafe { destroy_inst(inst, ptr::null()) };
    }
    ok
}

// ------------------------------------------------ instance-level functions

unsafe extern "C" fn gn_vkCreateInstance(
    ci: *const VkInstanceCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkInstance,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gipa = G.lock().unwrap().real_gipa;
    let Some(gipa) = gipa else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let real_create: Option<PFN_vkCreateInstance> =
        unsafe { std::mem::transmute(gipa(ptr::null_mut(), c"vkCreateInstance".as_ptr())) };
    let Some(real_create) = real_create else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let mut exts: [*const c_char; 32] = [ptr::null(); 32];
    let mut n: usize = 0;
    let names = unsafe {
        slice_or_empty(
            ci.ppEnabledExtensionNames,
            ci.enabledExtensionCount as usize,
        )
    };
    for &name in names {
        let n_ = unsafe { CStr::from_ptr(name) };
        if n_ == c"VK_KHR_wayland_surface" || n_ == c"VK_KHR_xlib_surface" {
            continue; // proxied, not real
        }
        if n < exts.len() {
            exts[n] = name;
            n += 1;
        }
    }
    let mut ci2 = VkInstanceCreateInfo { ..*ci };
    ci2.enabledExtensionCount = n as u32;
    ci2.ppEnabledExtensionNames = exts.as_ptr();
    // Confine the loader's chain link to the hooks' namespace (see
    // G_CHAIN_INST_LINK).
    let mut p = ci2.pNext as *mut VkBaseOutStructure;
    while !p.is_null() {
        unsafe {
            if (*p).sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO {
                let lci = p.cast::<VkLayerInstanceCreateInfo>();
                if (*lci).function == VK_LAYER_LINK_INFO {
                    (*lci).u.pLayerInfo = &raw const G_CHAIN_INST_LINK.0 as *mut _;
                }
            }
            p = (*p).pNext;
        }
    }
    let res = unsafe { real_create(&ci2, alloc, out) };
    if res != VK_SUCCESS {
        return res;
    }
    let gi = Box::new(GnInstance {
        real: unsafe { *out },
        f: RealInstFns {
            gdp: unsafe { std::mem::transmute(gn_real_inst_proc(*out, c"vkGetDeviceProcAddr")) },
            enum_pds: unsafe {
                std::mem::transmute(gn_real_inst_proc(*out, c"vkEnumeratePhysicalDevices"))
            },
            create_device: unsafe {
                std::mem::transmute(gn_real_inst_proc(*out, c"vkCreateDevice"))
            },
            destroy_instance: unsafe {
                std::mem::transmute(gn_real_inst_proc(*out, c"vkDestroyInstance"))
            },
        },
        pds: [ptr::null_mut(); 8],
        n_pds: 0,
        next: ptr::null_mut(),
    });
    let gi = Box::into_raw(gi);
    unsafe {
        if let Some(enum_pds) = (*gi).f.enum_pds {
            let mut cnt: u32 = 0;
            if enum_pds(*out, &mut cnt, ptr::null_mut()) == VK_SUCCESS && cnt > 0 {
                if cnt > 8 {
                    cnt = 8;
                }
                if enum_pds(*out, &mut cnt, (*gi).pds.as_mut_ptr()) == VK_SUCCESS {
                    (*gi).n_pds = cnt;
                }
            }
        }
    }
    {
        let mut g = G.lock().unwrap();
        unsafe {
            (*gi).next = g.instances;
            g.instances = gi;
        }
    }
    crate::logf!("proxy instance {:?} (headless WSI backing)", unsafe {
        *out
    });
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkDestroyInstance(
    instance: VkInstance,
    alloc: *const VkAllocationCallbacks,
) {
    let gi = {
        let mut g = G.lock().unwrap();
        let mut pp: *mut *mut GnInstance = &raw mut g.instances;
        unsafe {
            while !(*pp).is_null() && (**pp).real != instance {
                pp = &raw mut (**pp).next;
            }
            let gi = *pp;
            if !gi.is_null() {
                *pp = (*gi).next;
            }
            gi
        }
    };
    if gi.is_null() {
        return;
    }
    unsafe {
        if let Some(destroy) = (*gi).f.destroy_instance {
            destroy(instance, alloc);
        }
        drop(Box::from_raw(gi));
    }
}

unsafe extern "C" fn gn_vkEnumerateInstanceExtensionProperties(
    layer: *const c_char,
    pCount: *mut u32,
    pProps: *mut VkExtensionProperties,
) -> VkResult {
    let gipa = G.lock().unwrap().real_gipa;
    let Some(gipa) = gipa else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let real_enum: Option<PFN_vkEnumerateInstanceExtensionProperties> = unsafe {
        std::mem::transmute(gipa(
            ptr::null_mut(),
            c"vkEnumerateInstanceExtensionProperties".as_ptr(),
        ))
    };
    let Some(real_enum) = real_enum else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    unsafe {
        if pProps.is_null() {
            let mut n: u32 = 0;
            let res = real_enum(layer, &mut n, ptr::null_mut());
            if res != VK_SUCCESS {
                return res;
            }
            *pCount = n + 2;
            return VK_SUCCESS;
        }
        let cap = *pCount;
        if cap == 0 {
            *pCount = 0;
            return VK_INCOMPLETE;
        }
        let mut n = cap.saturating_sub(2);
        let res = real_enum(layer, &mut n, pProps);
        if res != VK_SUCCESS && res != VK_INCOMPLETE {
            return res;
        }
        let total = n + 2;
        if cap >= total {
            // Both surface extensions are proxied: winewayland clients use
            // the wayland one, winex11 clients (the gamescope path) the xlib
            // one — the real driver never sees either.
            for (i, name) in [
                &b"VK_KHR_wayland_surface\0"[..],
                &b"VK_KHR_xlib_surface\0"[..],
            ]
            .iter()
            .enumerate()
            {
                let slot = &mut *pProps.add(n as usize + i);
                slot.specVersion = 6;
                slot.extensionName[..name.len()]
                    .copy_from_slice(std::mem::transmute::<&[u8], &[c_char]>(name));
            }
            *pCount = total;
            return VK_SUCCESS;
        }
        *pCount = total;
        VK_INCOMPLETE
    }
}

/// Forwards the driver's device-extension list minus the WSI extensions the
/// proxy's client-side swapchain cannot honour. Without this the list passes
/// through verbatim, and a guest that enabled e.g. VK_KHR_present_wait would
/// call it through the wrapper-swapping thunk with a shim swapchain handle the
/// driver has never seen.
unsafe extern "C" fn gn_vkEnumerateDeviceExtensionProperties(
    pd: VkPhysicalDevice,
    layer: *const c_char,
    count: *mut u32,
    props: *mut VkExtensionProperties,
) -> VkResult {
    let gipa = G.lock().unwrap().real_gipa;
    let Some(gipa) = gipa else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    // Instance-level function: mesa resolves it only against a live instance.
    let gi = unsafe { find_instance_by_pd(pd) };
    let inst = if gi.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*gi).real }
    };
    let real: Option<PFN_vkEnumerateDeviceExtensionProperties> = unsafe {
        std::mem::transmute(gipa(inst, c"vkEnumerateDeviceExtensionProperties".as_ptr()))
    };
    let Some(real) = real else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    unsafe {
        let mut n: u32 = 0;
        let res = real(pd, layer, &mut n, ptr::null_mut());
        if res != VK_SUCCESS {
            return res;
        }
        let mut buf = vec![
            VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: 0,
            };
            n as usize
        ];
        let mut got = n;
        let res = real(pd, layer, &mut got, buf.as_mut_ptr());
        if res != VK_SUCCESS && res != VK_INCOMPLETE {
            return res;
        }
        let mut kept = 0usize;
        for i in 0..got as usize {
            if !unimplemented_wsi_ext(CStr::from_ptr(buf[i].extensionName.as_ptr())) {
                buf[kept] = buf[i];
                kept += 1;
            }
        }
        if props.is_null() {
            *count = kept as u32;
            return VK_SUCCESS;
        }
        let n_copy = (*count as usize).min(kept);
        ptr::copy_nonoverlapping(buf.as_ptr(), props, n_copy);
        *count = n_copy as u32;
        if n_copy < kept {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        }
    }
}

// ------------------------------------------------- wayland surface (proxy)

unsafe extern "C" fn gn_vkCreateWaylandSurfaceKHR(
    instance: VkInstance,
    ci: *const VkWaylandSurfaceCreateInfoKHR,
    _alloc: *const VkAllocationCallbacks,
    out: *mut VkSurfaceKHR,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gi = unsafe { find_instance(instance) };
    if gi.is_null() {
        crate::logf!("proxy surface create FAILED: instance {instance:?} not one of ours");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    if !wl_setup(ci.display.cast()) {
        crate::logf!("proxy surface create FAILED: wayland setup failed");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let s = Box::new(GnSurface {
        inst: gi,
        dpy: ci.display.cast(),
        parent: ci.surface.cast(),
        own_surface: ptr::null_mut(),
        swapchain_obj: ptr::null_mut(),
        next: ptr::null_mut(),
    });
    let s = Box::into_raw(s);
    {
        let mut g = G.lock().unwrap();
        unsafe {
            (*s).next = g.surfaces;
            g.surfaces = s;
        }
    }
    unsafe {
        *out = s as usize as VkSurfaceKHR;
        crate::logf!(
            "proxy surface {:?} (direct attach on wl_surface {:?})",
            s,
            ci.surface
        );
    }
    VK_SUCCESS
}

// ------------------------------------------------- xlib surface (proxy, X11 mode)
//
// Under winex11.drv wine has no wl_surface for the window; the shim creates
// its OWN wl_surface on its own gamescope connection and binds it to the X
// window via the upstream gamescope_swapchain override mechanism: the
// compositor then routes this surface's commits as the X window's content
// (wlserver handle_override_window_content re-points the window's
// x11_surface_info at our surface and drains g_PendingCommits for it).
//
// This is the only WSI provider in the stack: the Vulkan driver (turnip)
// exports no xlib or wayland WSI entry points at all, so handing an X11
// surface to it cannot work (its ICD carries no vkCreateXlibSurfaceKHR, and
// surface setup then fails VK_ERROR_INITIALIZATION_FAILED). The shim must
// present every window itself, so there is no driver-WSI fallback route.

unsafe extern "C" fn gn_vkCreateXlibSurfaceKHR(
    instance: VkInstance,
    ci: *const VkXlibSurfaceCreateInfoKHR,
    _alloc: *const VkAllocationCallbacks,
    out: *mut VkSurfaceKHR,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gi = unsafe { find_instance(instance) };
    if gi.is_null() {
        crate::logf!("proxy xlib surface create FAILED: instance {instance:?} not one of ours");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    if !wl_setup(ptr::null_mut()) {
        crate::logf!("proxy xlib surface create FAILED: wayland setup failed");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let (comp, factory, dpy) = {
        let w = G_WL.lock().unwrap();
        (w.comp, w.swapchain_factory, w.dpy)
    };
    if factory.is_null() {
        crate::logf!("compositor misses gamescope_swapchain_factory_v2 — cannot bind XID");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe {
        let _sg = SEND_LOCK.lock().unwrap();
        let surf = wlproto::compositor_create_surface(comp);
        if surf.is_null() {
            crate::logf!("proxy xlib surface create FAILED: wl_surface create failed");
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        let scobj = wlproto::swapchain_factory_create_swapchain(factory, surf);
        // Single xwayland server (--xwayland-count 1) is always id 0.
        wlproto::swapchain_override_window_content(scobj, 0, ci.window as u32);
        wlproto::wl_display_flush(dpy);
        drop(_sg);
        let s = Box::new(GnSurface {
            inst: gi,
            dpy,
            parent: surf,
            own_surface: surf,
            swapchain_obj: scobj,
            next: ptr::null_mut(),
        });
        let s = Box::into_raw(s);
        {
            let mut g = G.lock().unwrap();
            (*s).next = g.surfaces;
            g.surfaces = s;
        }
        *out = s as usize as VkSurfaceKHR;
        crate::logf!(
            "proxy surface {:?} (own wl_surface {:?} bound to XID {:#x})",
            s,
            surf,
            ci.window
        );
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceXlibPresentationSupportKHR(
    _pd: VkPhysicalDevice,
    _qf: u32,
    _dpy: *mut c_void,
    _visual: usize,
) -> VkBool32 {
    VK_TRUE
}

unsafe extern "C" fn gn_vkDestroySurfaceKHR(
    _instance: VkInstance,
    surface: VkSurfaceKHR,
    _alloc: *const VkAllocationCallbacks,
) {
    let s = unsafe { find_surface(surface) };
    if s.is_null() {
        return;
    }
    {
        let mut g = G.lock().unwrap();
        let mut pp: *mut *mut GnSurface = &raw mut g.surfaces;
        unsafe {
            while !(*pp).is_null() && *pp != s {
                pp = &raw mut (**pp).next;
            }
            if !(*pp).is_null() {
                *pp = (*s).next;
            }
        }
    }
    unsafe {
        let _sg = SEND_LOCK.lock().unwrap();
        if !(*s).swapchain_obj.is_null() {
            wlproto::wl_proxy_destroy((*s).swapchain_obj);
        }
        if !(*s).own_surface.is_null() {
            wlproto::wl_proxy_destroy((*s).own_surface);
        }
        wlproto::wl_display_flush((*s).dpy);
        drop(_sg);
        drop(Box::from_raw(s));
    }
}

// Surface queries are answered locally: the proxy owns the wayland
// presentation contract, and the hooks' surface-query trampolines expect
// loader-wrapped surface objects their VK_EXT_headless_surface path never
// produces (the ICD-level mesa surface's first word is its object-type
// enum, not a dispatch table).
unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfaceCapabilitiesKHR(
    _pd: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    caps: *mut VkSurfaceCapabilitiesKHR,
) -> VkResult {
    if unsafe { find_surface(surface) }.is_null() {
        crate::logf!("proxy surface caps query: {surface:#x} not one of ours");
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    unsafe {
        *caps = VkSurfaceCapabilitiesKHR {
            minImageCount: 2,
            maxImageCount: max_swapchain_images(),
            currentExtent: VkExtent2D {
                width: u32::MAX,
                height: u32::MAX,
            }, // wayland: app picks
            minImageExtent: VkExtent2D {
                width: 1,
                height: 1,
            },
            maxImageExtent: VkExtent2D {
                width: 8192,
                height: 8192,
            },
            maxImageArrayLayers: 1,
            supportedTransforms: VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
            currentTransform: VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
            supportedCompositeAlpha: VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
            // Exactly what the pool's AHardwareBuffer backs, no more. The pool
            // allocates GPU_SAMPLED_IMAGE | GPU_COLOR_OUTPUT | COMPOSER_OVERLAY
            // | CPU_READ_OFTEN and the guest renders straight into it, so
            // SAMPLED_BIT comes with the first flag and COLOR_ATTACHMENT_BIT
            // with the second. The transfer bits are on every image the proxy
            // creates (`GnSwapchain::usage`) and the WSI smoke client actually
            // exercises them: its present readback copies a swapchain image to
            // host memory (TRANSFER_SRC) and its clear scene clears one in
            // TRANSFER_DST_OPTIMAL (TRANSFER_DST). COMPOSER_OVERLAY and
            // CPU_READ_OFTEN are SurfaceFlinger/host usages with no Vulkan
            // equivalent. No STORAGE_BIT: an AHardwareBuffer grants that only
            // through AHARDWAREBUFFER_USAGE_GPU_DATA_BUFFER, which this pool
            // does not set, so advertising it would promise a capability the
            // scan-out buffer cannot back.
            supportedUsageFlags: VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT
                | VK_IMAGE_USAGE_TRANSFER_SRC_BIT
                | VK_IMAGE_USAGE_TRANSFER_DST_BIT
                | VK_IMAGE_USAGE_SAMPLED_BIT,
        };
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfaceFormatsKHR(
    _pd: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    count: *mut u32,
    formats: *mut VkSurfaceFormatKHR,
) -> VkResult {
    if unsafe { find_surface(surface) }.is_null() {
        crate::logf!("proxy surface formats query: {surface:#x} not one of ours");
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    const K_FMTS: [VkSurfaceFormatKHR; 2] = [
        VkSurfaceFormatKHR {
            format: VK_FORMAT_B8G8R8A8_UNORM,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_R8G8B8A8_UNORM,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
    ];
    // BGRA_8888 only by default, so the guest renders in the scan-out pool's
    // own layout (see `advertises_rgba` — offering RGBA let Nidhogg render
    // R/B-exchanged into a daemon BGRA buffer, which reached the panel as
    // cyan). RGBA is offered only under `GN_VKSHIM_ALLOW_RGBA`.
    let n_formats = if advertises_rgba() { K_FMTS.len() } else { 1 };
    unsafe {
        if formats.is_null() {
            *count = n_formats as u32;
            return VK_SUCCESS;
        }
        let n = (*count).min(n_formats as u32) as usize;
        ptr::copy_nonoverlapping(K_FMTS.as_ptr(), formats, n);
        *count = n as u32;
        if n < n_formats {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        }
    }
}

/// The present modes the proxy's client-side swapchain serves. The proxy owns
/// the swapchain (a dma-buf ring published straight to the compositor) and
/// never presents through the driver's WSI, so there is no surface on which the
/// underlying driver's vkGetPhysicalDeviceSurfacePresentModesKHR could be
/// asked: the shim's VkSurfaceKHR is a private pointer the driver has never
/// seen, and this driver family carries no wayland WSI to build one with. Every
/// mode here is served by the same publish path — presents never block on the
/// compositor and a slot is reused on wl_buffer.release — so the guest may pick
/// any of them; FIFO is always offered.
const SERVED_PRESENT_MODES: [VkPresentModeKHR; 3] = [
    VK_PRESENT_MODE_IMMEDIATE_KHR,
    VK_PRESENT_MODE_MAILBOX_KHR,
    VK_PRESENT_MODE_FIFO_KHR,
];

/// Parses MESA_VK_WSI_PRESENT_MODE exactly as mesa's wsi_common.c does: the four
/// names are matched case-sensitively and anything else is reported and ignored.
fn parse_present_mode_override(val: Option<&str>) -> Option<VkPresentModeKHR> {
    let mode = match val? {
        "fifo" => VK_PRESENT_MODE_FIFO_KHR,
        "relaxed" => VK_PRESENT_MODE_FIFO_RELAXED_KHR,
        "mailbox" => VK_PRESENT_MODE_MAILBOX_KHR,
        "immediate" => VK_PRESENT_MODE_IMMEDIATE_KHR,
        _ => {
            crate::logf!("Invalid MESA_VK_WSI_PRESENT_MODE value!");
            return None;
        }
    };
    Some(mode)
}

/// The list offered to the guest: the override mode first when the ring can
/// serve it, then the served modes in their fixed order, FIFO always present.
/// An override the ring cannot serve is dropped instead of advertised, matching
/// mesa's "Unsupported …" behaviour of keeping the requested mode.
fn offered_present_modes(
    served: &[VkPresentModeKHR],
    override_mode: Option<VkPresentModeKHR>,
) -> Vec<VkPresentModeKHR> {
    let mut v = Vec::with_capacity(served.len() + 1);
    if let Some(m) = override_mode {
        if served.contains(&m) {
            v.push(m);
        } else {
            crate::logf!("Unsupported MESA_VK_WSI_PRESENT_MODE value!");
        }
    }
    for &m in served {
        if !v.contains(&m) {
            v.push(m);
        }
    }
    if !v.contains(&VK_PRESENT_MODE_FIFO_KHR) {
        v.push(VK_PRESENT_MODE_FIFO_KHR);
    }
    v
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfacePresentModesKHR(
    _pd: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    count: *mut u32,
    modes: *mut VkPresentModeKHR,
) -> VkResult {
    if unsafe { find_surface(surface) }.is_null() {
        crate::logf!("proxy surface present-modes query: {surface:#x} not one of ours");
        return VK_ERROR_SURFACE_LOST_KHR;
    }
    let modes_list = offered_present_modes(
        &SERVED_PRESENT_MODES,
        parse_present_mode_override(std::env::var("MESA_VK_WSI_PRESENT_MODE").ok().as_deref()),
    );
    unsafe {
        if modes.is_null() {
            *count = modes_list.len() as u32;
            return VK_SUCCESS;
        }
        let n = (*count).min(modes_list.len() as u32) as usize;
        ptr::copy_nonoverlapping(modes_list.as_ptr(), modes, n);
        *count = n as u32;
        if n < modes_list.len() {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        }
    }
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceSurfaceSupportKHR(
    _pd: VkPhysicalDevice,
    _qf: u32,
    _surface: VkSurfaceKHR,
    supported: *mut VkBool32,
) -> VkResult {
    unsafe { *supported = VK_TRUE };
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkGetPhysicalDeviceWaylandPresentationSupportKHR(
    _pd: VkPhysicalDevice,
    _qf: u32,
    _dpy: *mut wl_display,
) -> VkBool32 {
    VK_TRUE
}

// ------------------------------------------------------- device functions

unsafe extern "C" fn gn_vkCreateDevice(
    pd: VkPhysicalDevice,
    ci: *const VkDeviceCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkDevice,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gi = unsafe { find_instance_by_pd(pd) };
    if gi.is_null() || unsafe { (*gi).f.create_device.is_none() } {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    const ADD_EXTS: [&CStr; 4] = [
        c"VK_KHR_external_memory",
        c"VK_KHR_external_memory_fd",
        c"VK_EXT_external_memory_dma_buf",
        c"VK_EXT_image_drm_format_modifier",
    ];
    let mut exts: [*const c_char; 48] = [ptr::null(); 48];
    let mut n: usize = 0;
    let names = unsafe {
        slice_or_empty(
            ci.ppEnabledExtensionNames,
            ci.enabledExtensionCount as usize,
        )
    };
    for &name in names {
        // The guest must never enable a WSI extension the proxy cannot honour
        // (see `unimplemented_wsi_ext`); drop it before the driver sees it.
        let n_ = unsafe { CStr::from_ptr(name) };
        if unimplemented_wsi_ext(n_) {
            crate::logf!("proxy device: masking {}", n_.to_string_lossy());
            continue;
        }
        if n < exts.len() {
            exts[n] = name;
            n += 1;
        }
    }
    'add: for add in ADD_EXTS {
        for &e in &exts[..n] {
            if unsafe { CStr::from_ptr(e) } == add {
                continue 'add;
            }
        }
        if n < exts.len() {
            exts[n] = add.as_ptr();
            n += 1;
        }
    }
    // Optionally mask the pipeline-library extensions (GN_VKSHIM_NO_GPL=1):
    // DXVK then builds monolithic pipelines instead of partial ones it links,
    // which is the A/B for a suspected bad partial/link path in this driver
    // (the X11 vulkan wrapper fakes VK_KHR_pipeline_library, so X11 never uses
    // it). Off by default: without it DXVK keeps its pipeline-compilation
    // optimisation, and masking it means compiling every state monolithically,
    // which is slow enough on this driver to look like a hang.
    if std::env::var_os("GN_VKSHIM_NO_GPL").is_some() {
        for drop in [
            c"VK_EXT_graphics_pipeline_library",
            c"VK_KHR_pipeline_library",
        ] {
            let mut i = 0;
            while i < n {
                if unsafe { CStr::from_ptr(exts[i]) } == drop {
                    crate::logf!("proxy device: masking {}", drop.to_string_lossy());
                    for j in i..n - 1 {
                        exts[j] = exts[j + 1];
                    }
                    n -= 1;
                } else {
                    i += 1;
                }
            }
        }
    }
    let mut ci2 = VkDeviceCreateInfo { ..*ci };
    ci2.enabledExtensionCount = n as u32;
    ci2.ppEnabledExtensionNames = exts.as_ptr();
    // Confine the loader's chain link to the hooks' namespace (see
    // G_CHAIN_DEV_LINK).
    let mut p = ci2.pNext as *mut VkBaseOutStructure;
    while !p.is_null() {
        unsafe {
            if (*p).sType == VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO {
                let lci = p.cast::<VkLayerDeviceCreateInfo>();
                if (*lci).function == VK_LAYER_LINK_INFO {
                    (*lci).u.pLayerDeviceInfo = &raw const G_CHAIN_DEV_LINK.0 as *mut _;
                }
            }
            p = (*p).pNext;
        }
    }
    let res = unsafe { (*gi).f.create_device.unwrap()(pd, &ci2, alloc, out) };
    if res != VK_SUCCESS {
        return res;
    }
    let mut gd = Box::new(GnDevice {
        scratch: [0; GN_HANDLE_SCRATCH],
        real: unsafe { *out },
        inst: gi,
        f: RealDevFns::default(),
        queues: ptr::null_mut(),
        cmds: ptr::null_mut(),
        next: ptr::null_mut(),
    });
    // Real-device function resolution never uses the hooks'
    // vkGetDeviceProcAddr: its answers are vulkan::driver::* trampolines
    // that index a high dispatch region which isn't valid for this device
    // (GetDeviceQueue through it returned a NULL queue, crashing the
    // trampoline's own handle-magic check). The hooks' exported vk* entries
    // forward through the device's ordinary low dispatch slots — always
    // consistent with the CreateDevice that produced the handle; the
    // instance GIPA falls through to the real driver's name tables for the
    // extension names the hooks don't export.
    let gdp: *mut GnDevice = &mut *gd;
    macro_rules! resolve {
        ($field:ident, $sym:literal) => {
            gd.f.$field = unsafe {
                std::mem::transmute(gn_real_device_proc(
                    gdp,
                    CStr::from_bytes_with_nul_unchecked(concat!($sym, "\0").as_bytes()),
                ))
            };
        };
    }
    resolve!(get_queue, "vkGetDeviceQueue");
    resolve!(create_image, "vkCreateImage");
    resolve!(create_pipelines, "vkCreateGraphicsPipelines");
    resolve!(get_device_queue2, "vkGetDeviceQueue2");
    resolve!(submit2, "vkQueueSubmit2");
    resolve!(wait_semaphores, "vkWaitSemaphoresKHR");
    resolve!(destroy_image, "vkDestroyImage");
    resolve!(get_mem_reqs, "vkGetImageMemoryRequirements");
    resolve!(get_layout, "vkGetImageSubresourceLayout");
    resolve!(alloc_mem, "vkAllocateMemory");
    resolve!(free_mem, "vkFreeMemory");
    resolve!(bind_mem, "vkBindImageMemory");
    resolve!(create_fence, "vkCreateFence");
    resolve!(destroy_fence, "vkDestroyFence");
    resolve!(wait_fences, "vkWaitForFences");
    resolve!(reset_fences, "vkResetFences");
    resolve!(create_sem, "vkCreateSemaphore");
    resolve!(destroy_sem, "vkDestroySemaphore");
    resolve!(alloc_cmd, "vkAllocateCommandBuffers");
    resolve!(free_cmd, "vkFreeCommandBuffers");
    resolve!(submit, "vkQueueSubmit");
    resolve!(wait_idle, "vkDeviceWaitIdle");
    resolve!(destroy_device, "vkDestroyDevice");
    let qf = if ci.pQueueCreateInfos.is_null() {
        0
    } else {
        unsafe { (*ci.pQueueCreateInfos).queueFamilyIndex }
    };
    let gd = Box::into_raw(gd);
    {
        let mut g = G.lock().unwrap();
        unsafe {
            (*gd).next = g.devices;
            g.devices = gd;
        }
    }
    // Hand the loader our own wrapper; gd.real stays private (its first
    // word must keep pointing at the hooks' dispatch table).
    unsafe {
        *out = gd.cast::<c_void>();
        crate::logf!(
            "proxy device {:?} (real {:?}, queue family {qf})",
            gd,
            (*gd).real
        );
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkDestroyDevice(device: VkDevice, _alloc: *const VkAllocationCallbacks) {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return;
    }
    unsafe {
        if let Some(wait_idle) = (*gd).f.wait_idle {
            wait_idle((*gd).real);
        }
        if let Some(destroy_device) = (*gd).f.destroy_device {
            destroy_device((*gd).real, ptr::null());
        }
    }
    {
        let mut g = G.lock().unwrap();
        let mut pp: *mut *mut GnDevice = &raw mut g.devices;
        unsafe {
            while !(*pp).is_null() && *pp != gd {
                pp = &raw mut (**pp).next;
            }
            if !(*pp).is_null() {
                *pp = (*gd).next;
            }
            while !(*gd).queues.is_null() {
                let gq = (*gd).queues;
                (*gd).queues = (*gq).next;
                drop(Box::from_raw(gq));
            }
            while !(*gd).cmds.is_null() {
                let gc = (*gd).cmds;
                (*gd).cmds = (*gc).next;
                drop(Box::from_raw(gc));
            }
        }
    }
    unsafe {
        drop(Box::from_raw(gd));
    }
}

unsafe extern "C" fn gn_vkGetDeviceQueue(
    device: VkDevice,
    family: u32,
    index: u32,
    out: *mut VkQueue,
) {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return;
    }
    let mut gq = Box::new(GnQueue {
        scratch: [0; GN_HANDLE_SCRATCH],
        real: ptr::null_mut(),
        gd,
        next: ptr::null_mut(),
    });
    unsafe {
        if let Some(get_queue) = (*gd).f.get_queue {
            get_queue((*gd).real, family, index, &mut gq.real);
        }
    }
    let gq = Box::into_raw(gq);
    {
        let _g = G.lock().unwrap();
        unsafe {
            (*gq).next = (*gd).queues;
            (*gd).queues = gq;
        }
    }
    unsafe { *out = gq.cast::<c_void>() };
}

unsafe extern "C" fn gn_vkGetDeviceQueue2(
    device: VkDevice,
    info: *const VkDeviceQueueInfo2,
    out: *mut VkQueue,
) {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return;
    }
    // Per device: these resolved entry points are bound to the device that
    // produced the handle, so a process-global cache hands a later device a
    // function that fails its own handle check (a NULL queue or worse).
    let Some(real_fn) = (unsafe { (*gd).f.get_device_queue2 }) else {
        unsafe { gn_vkGetDeviceQueue(device, (*info).queueFamilyIndex, (*info).queueIndex, out) };
        return;
    };
    let mut gq = Box::new(GnQueue {
        scratch: [0; GN_HANDLE_SCRATCH],
        real: ptr::null_mut(),
        gd,
        next: ptr::null_mut(),
    });
    unsafe {
        real_fn((*gd).real, info, &mut gq.real);
    }
    let gq = Box::into_raw(gq);
    {
        let _g = G.lock().unwrap();
        unsafe {
            (*gq).next = (*gd).queues;
            (*gd).queues = gq;
        }
    }
    unsafe { *out = gq.cast::<c_void>() };
}

// ------------------------------------------------- swapchain + present

fn vk_format_to_fourcc(format: VkFormat) -> u32 {
    match format {
        // DRM fourccs name the LE 32-bit word: XRGB8888 = 0xXXRRGGBB, i.e.
        // memory bytes B,G,R,X — VK_FORMAT_B8G8R8A8_UNORM's layout.
        VK_FORMAT_B8G8R8A8_UNORM => GN_DRM_FORMAT_XRGB8888,
        VK_FORMAT_R8G8B8A8_UNORM => GN_DRM_FORMAT_XBGR8888,
        _ => 0,
    }
}

/// The scan-out pool's memory layout, which is what a guest swapchain buffer
/// actually is whenever the daemon's allocator catches it. Kept as the single
/// accepted/offered format (see [`advertises_rgba`]).
const SCANOUT_SWAPCHAIN_FORMAT: VkFormat = VK_FORMAT_B8G8R8A8_UNORM;

/// Whether the proxy offers (and accepts) `VK_FORMAT_R8G8B8A8_UNORM` in
/// addition to `B8G8R8A8_UNORM`. **Off by default.**
///
/// This is measured, not stylistic. The scan-out pool allocates
/// `AHARDWAREBUFFER_FORMAT_B8G8R8A8_UNORM`, so a guest surface the pool owns
/// must be that same layout. A guest that picks RGBA
/// (Nidhogg did: `proxy swapchain … 1280x960 fmt 37 -> fourcc 0x34324258`)
/// renders R,G,B,X into a buffer the daemon then publishes to SurfaceFlinger
/// *as* BGRA, exchanging red and blue: Nidhogg's yellow/red art reached the
/// panel as cyan, while a BGRA guest (Dishonored, `fmt 44`) was correct. The
/// memory is the pool's, so the format the guest renders in must be the
/// pool's. `GN_VKSHIM_ALLOW_RGBA=1` re-offers RGBA for bring-up A/B;
/// `GN_VKSHIM_FORCE_BGRA` forces this off regardless.
fn advertises_rgba() -> bool {
    std::env::var_os("GN_VKSHIM_FORCE_BGRA").is_none()
        && std::env::var_os("GN_VKSHIM_ALLOW_RGBA").is_some()
}

/// Whether a guest swapchain of `format` may be created, given whether RGBA is
/// offered (`allow_rgba` = [`advertises_rgba`]). Pure so the rule is
/// host-testable: exactly `SCANOUT_SWAPCHAIN_FORMAT`, plus RGBA only when
/// explicitly re-enabled.
fn swapchain_format_allowed(format: VkFormat, allow_rgba: bool) -> bool {
    format == SCANOUT_SWAPCHAIN_FORMAT || (allow_rgba && format == VK_FORMAT_R8G8B8A8_UNORM)
}

unsafe fn slot_release(sc: *mut GnSwapchain, s: *mut GnSlot) {
    let gd = (*sc).dev;
    unsafe {
        if !(*s).buf.is_null() {
            wlproto::wl_proxy_destroy((*s).buf);
            (*s).buf = ptr::null_mut();
        }
        if (*s).fence != 0 {
            if let Some(f) = (*gd).f.destroy_fence {
                f((*gd).real, (*s).fence, ptr::null());
            }
        }
        if (*s).sem != 0 {
            if let Some(f) = (*gd).f.destroy_sem {
                f((*gd).real, (*s).sem, ptr::null());
            }
        }
        if (*s).image != 0 {
            if let Some(f) = (*gd).f.destroy_image {
                f((*gd).real, (*s).image, ptr::null());
            }
        }
        if (*s).mem != 0 {
            if let Some(f) = (*gd).f.free_mem {
                f((*gd).real, (*s).mem, ptr::null());
            }
        }
        if (*s).fd >= 0 {
            libc::close((*s).fd);
        }
        (*s).fd = -1;
    }
}

// Present targets are dma-bufs the shim allocates from the system heap and
// IMPORTS into vulkan. The adrenotools driver family cannot export fds
// (vkGetMemoryFdKHR / vkGetImageDrmFormatModifierPropertiesEXT are not
// reachable through its hooks), so the ownership is inverted: the shim owns
// the fd, vulkan borrows it, and the same fd is committed to wayland.
fn dma_heap_open() -> i32 {
    static HEAP_FD: OnceLock<i32> = OnceLock::new();
    *HEAP_FD.get_or_init(|| {
        let path = CString::new("/dev/dma_heap/system").unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            crate::logf!(
                "proxy: /dev/dma_heap/system: {}",
                std::io::Error::last_os_error()
            );
        }
        fd
    })
}

fn dma_heap_alloc(len: usize) -> i32 {
    let heap = dma_heap_open();
    if heap < 0 {
        return -1;
    }
    // DMA_HEAP_IOCTL_ALLOC: _IOWR('H', 0x0, struct dma_heap_allocation_data)
    #[repr(C)]
    struct dma_heap_allocation_data {
        len: u64,
        fd: u32,
        fd_flags: u32,
        heap_flags: u64,
    }
    // _IOC(_IOC_READ|_IOC_WRITE=3 << 30, size << 16, magic 'H' << 8, nr 0)
    const DMA_HEAP_IOCTL_ALLOC: libc::c_ulong = (3u64 << 30)
        | ((std::mem::size_of::<dma_heap_allocation_data>() as u64) << 16)
        | ((b'H' as u64) << 8);
    let mut data = dma_heap_allocation_data {
        len: len as u64,
        fd: 0,
        // O_RDWR: the fd crosses to gndrm, whose CPU upload path mmaps it
        // PROT_READ — an accmode-less fd fails mmap with EACCES.
        fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
        heap_flags: 0,
    };
    if unsafe { libc::ioctl(heap, DMA_HEAP_IOCTL_ALLOC as _, &mut data) } != 0 {
        crate::logf!(
            "proxy: dma_heap alloc({len}): {}",
            std::io::Error::last_os_error()
        );
        return -1;
    }
    data.fd as i32
}

/// Fence/semaphore pair every present slot needs, regardless of layout.
unsafe fn slot_init_sync(sc: *mut GnSwapchain, s: *mut GnSlot) -> VkResult {
    let gd = (*sc).dev;
    let dev = unsafe { (*gd).real };
    let f = unsafe { (*gd).f };
    let fci = VkFenceCreateInfo {
        sType: VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
        pNext: ptr::null(),
        flags: 0,
    };
    let sci = VkSemaphoreCreateInfo {
        sType: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
        pNext: ptr::null(),
        flags: 0,
    };
    let mut res = unsafe { f.create_fence.unwrap()(dev, &fci, ptr::null(), &mut (*s).fence) };
    if res == VK_SUCCESS {
        res = unsafe { f.create_sem.unwrap()(dev, &sci, ptr::null(), &mut (*s).sem) };
    }
    res
}

/// GN_VKSHIM_SC_MOD present image: the driver picks its own scanout layout and
/// names it through the DRM modifier, so the guest renders into the same kind
/// of target a stock driver allocates. The driver cannot export fds, so the
/// shim still owns the dma-heap allocation: the modifier is what tells
/// gamescope how to read it. A tiled image has no CPU-readable layout, hence
/// no vkGetImageSubresourceLayout probe and no frame dump.
unsafe fn slot_init_tiled(sc: *mut GnSwapchain, s: *mut GnSlot, modifier: u64) -> VkResult {
    let gd = (*sc).dev;
    let dev = unsafe { (*gd).real };
    let f = unsafe { (*gd).f };
    let w = (*sc).extent.width;
    unsafe {
        if (*s).fd >= 0 {
            libc::close((*s).fd);
            (*s).fd = -1;
        }
    }
    (*s).stride = (w * 4 + 255) & !255;
    (*s).offset = 0;
    (*s).modifier = modifier;
    let modlist = VkImageDrmFormatModifierListCreateInfoEXT {
        sType: VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
        pNext: ptr::null(),
        drmFormatModifierCount: 1,
        pDrmFormatModifiers: &modifier,
    };
    let ext_img = VkExternalMemoryImageCreateInfo {
        sType: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        pNext: &modlist as *const _ as *const c_void,
        handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    let ici = VkImageCreateInfo {
        sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        pNext: &ext_img as *const _ as *const c_void,
        flags: 0,
        imageType: VK_IMAGE_TYPE_2D,
        format: (*sc).format,
        extent: VkExtent3D {
            width: w,
            height: (*sc).extent.height,
            depth: 1,
        },
        mipLevels: 1,
        arrayLayers: 1,
        samples: VK_SAMPLE_COUNT_1_BIT,
        tiling: VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
        usage: (*sc).usage,
        sharingMode: VK_SHARING_MODE_EXCLUSIVE,
        queueFamilyIndexCount: 0,
        pQueueFamilyIndices: ptr::null(),
        initialLayout: VK_IMAGE_LAYOUT_UNDEFINED,
    };
    let mut res = unsafe { f.create_image.unwrap()(dev, &ici, ptr::null(), &mut (*s).image) };
    if res != VK_SUCCESS {
        crate::logf!("proxy slot: modifier {modifier:#x} image create: {res}");
        (*s).image = 0;
        return res;
    }
    let mut reqs = VkMemoryRequirements {
        size: 0,
        alignment: 0,
        memoryTypeBits: 0,
    };
    unsafe { f.get_mem_reqs.unwrap()(dev, (*s).image, &mut reqs) };
    let mut mem_type = u32::MAX;
    for i in 0..32 {
        if reqs.memoryTypeBits & (1 << i) != 0 {
            mem_type = i;
            break;
        }
    }
    // The driver's own size: a compressed layout carries a metadata region
    // past the pixels, and it is only known once the image exists.
    (*s).fd = dma_heap_alloc(reqs.size as usize);
    crate::logf!(
        "proxy slot: modifier {modifier:#x} {}x{} reqs {} mem_type {mem_type} fd {} stride {}",
        w,
        (*sc).extent.height,
        reqs.size,
        (*s).fd,
        (*s).stride
    );
    if mem_type == u32::MAX || (*s).fd < 0 {
        res = VK_ERROR_INITIALIZATION_FAILED;
    } else {
        let ded = VkMemoryDedicatedAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
            pNext: ptr::null(),
            image: (*s).image,
            buffer: 0,
        };
        let imp = VkImportMemoryFdInfoKHR {
            sType: VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
            pNext: &ded as *const _ as *const c_void,
            handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
            fd: unsafe { libc::dup((*s).fd) },
        };
        let mai = VkMemoryAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            pNext: &imp as *const _ as *const c_void,
            allocationSize: reqs.size,
            memoryTypeIndex: mem_type,
        };
        res = unsafe { f.alloc_mem.unwrap()(dev, &mai, ptr::null(), &mut (*s).mem) };
        if res == VK_SUCCESS {
            res = unsafe { f.bind_mem.unwrap()(dev, (*s).image, (*s).mem, 0) };
        }
        if res != VK_SUCCESS {
            crate::logf!("proxy slot: modifier {modifier:#x} memory: {res}");
        }
    }
    if res != VK_SUCCESS {
        unsafe {
            if (*s).mem != 0 {
                f.free_mem.unwrap()(dev, (*s).mem, ptr::null());
            }
            if (*s).image != 0 {
                f.destroy_image.unwrap()(dev, (*s).image, ptr::null());
            }
            if (*s).fd >= 0 {
                libc::close((*s).fd);
            }
        }
        (*s).mem = 0;
        (*s).image = 0;
        (*s).fd = -1;
        return res;
    }
    slot_init_sync(sc, s)
}

unsafe fn slot_init(sc: *mut GnSwapchain, s: *mut GnSlot) -> VkResult {
    // GN_VKSHIM_SC_MOD=auto|compressed|tiled3. Driver-chosen scanout layouts
    // first; the explicit-LINEAR export below is always the fallback.
    for &modifier in sc_modifiers() {
        if unsafe { slot_init_tiled(sc, s, modifier) } == VK_SUCCESS {
            return VK_SUCCESS;
        }
    }
    let gd = (*sc).dev;
    let dev = unsafe { (*gd).real };
    let f = unsafe { (*gd).f };
    (*s).fd = -1;
    (*s).stride = ((*sc).extent.width * 4 + 255) & !255;
    (*s).offset = 0;
    (*s).modifier = GN_DRM_MOD_LINEAR;
    let mut plane = VkSubresourceLayout {
        offset: 0,
        size: 0,
        rowPitch: 0,
        arrayPitch: 0,
        depthPitch: 0,
    };
    for attempt in 0..2 {
        unsafe {
            if (*s).fd >= 0 {
                libc::close((*s).fd);
            }
        }
        // The swapchain image must be a buffer the daemon owns: the presenter
        // can only publish one of its own `AHardwareBuffer`s. Ask for it over
        // the scan-out channel explicitly rather than relying on the
        // LD_PRELOAD shim to intercept this process's heap allocation. The
        // returned stride is authoritative for the image this fd backs.
        match crate::scanout::allocate((*sc).extent.width, (*sc).extent.height) {
            Some(buf) => {
                (*s).fd = buf.fd;
                if buf.stride != 0 {
                    (*s).stride = buf.stride;
                }
                crate::logf!(
                    "proxy slot: pool buffer fd={} ino={} stride={}",
                    buf.fd,
                    buf.ino,
                    (*s).stride
                );
            }
            None => {
                // No daemon channel, or it refused: a local system-heap
                // allocation, the pre-existing behaviour, which the LD_PRELOAD
                // shim may still redirect.
                (*s).fd = dma_heap_alloc((*s).stride as usize * (*sc).extent.height as usize);
            }
        }
        if (*s).fd < 0 {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        plane.offset = 0;
        plane.rowPitch = (*s).stride as u64;
        let modexp = VkImageDrmFormatModifierExplicitCreateInfoEXT {
            sType: VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
            pNext: ptr::null(),
            drmFormatModifier: GN_DRM_MOD_LINEAR,
            drmFormatModifierPlaneCount: 1,
            pPlaneLayouts: &plane,
        };
        let ext_img = VkExternalMemoryImageCreateInfo {
            sType: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
            pNext: &modexp as *const _ as *const c_void,
            handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        };
        let ici = VkImageCreateInfo {
            sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            pNext: &ext_img as *const _ as *const c_void,
            flags: 0,
            imageType: VK_IMAGE_TYPE_2D,
            format: (*sc).format,
            extent: VkExtent3D {
                width: (*sc).extent.width,
                height: (*sc).extent.height,
                depth: 1,
            },
            mipLevels: 1,
            arrayLayers: 1,
            samples: VK_SAMPLE_COUNT_1_BIT,
            // TILING_LINEAR forbids UBWC outright; OPTIMAL+explicit LINEAR
            // modifier still lets turnip pick a compressed layout (the fd
            // then holds a UBWC payload, not scan-out pixels).
            tiling: VK_IMAGE_TILING_LINEAR,
            usage: (*sc).usage,
            sharingMode: VK_SHARING_MODE_EXCLUSIVE,
            queueFamilyIndexCount: 0,
            pQueueFamilyIndices: ptr::null(),
            initialLayout: VK_IMAGE_LAYOUT_UNDEFINED,
        };
        let res = unsafe { f.create_image.unwrap()(dev, &ici, ptr::null(), &mut (*s).image) };
        if res != VK_SUCCESS {
            crate::logf!("proxy import image create failed: {res}");
            return res;
        }
        let mut reqs = VkMemoryRequirements {
            size: 0,
            alignment: 0,
            memoryTypeBits: 0,
        };
        unsafe { f.get_mem_reqs.unwrap()(dev, (*s).image, &mut reqs) };
        let mut mem_type = u32::MAX;
        for i in 0..32 {
            if reqs.memoryTypeBits & (1 << i) != 0 {
                mem_type = i;
                break;
            }
        }
        if mem_type == u32::MAX {
            crate::logf!("proxy import image: no memory type");
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        let ded = VkMemoryDedicatedAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
            pNext: ptr::null(),
            image: (*s).image,
            buffer: 0,
        };
        let imp = VkImportMemoryFdInfoKHR {
            sType: VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
            pNext: &ded as *const _ as *const c_void,
            handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
            fd: unsafe { libc::dup((*s).fd) },
        };
        let mai = VkMemoryAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            pNext: &imp as *const _ as *const c_void,
            allocationSize: reqs.size,
            memoryTypeIndex: mem_type,
        };
        let mut res = unsafe { f.alloc_mem.unwrap()(dev, &mai, ptr::null(), &mut (*s).mem) };
        if res == VK_SUCCESS {
            res = unsafe { f.bind_mem.unwrap()(dev, (*s).image, (*s).mem, 0) };
        }
        if res != VK_SUCCESS {
            crate::logf!("proxy import memory failed: {res}");
            return res;
        }
        // Ground-truth the driver's plane layout: a driver honoring the
        // explicit LINEAR modifier reports exactly the pitch handed in;
        // anything else means the layout guess was wrong, so retry once
        // with the reported pitch.
        if let Some(get_layout) = f.get_layout {
            let sub = VkImageSubresource {
                aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                mipLevel: 0,
                arrayLayer: 0,
            };
            let mut got = VkSubresourceLayout {
                offset: 0,
                size: 0,
                rowPitch: 0,
                arrayPitch: 0,
                depthPitch: 0,
            };
            unsafe { get_layout(dev, (*s).image, &sub, &mut got) };
            crate::logf!(
                "proxy slot layout: pitch {} (asked {}), offset {}",
                got.rowPitch,
                (*s).stride,
                got.offset
            );
            if got.rowPitch != 0
                && got.rowPitch != (*s).stride as u64
                && got.rowPitch < 65536
                && attempt == 0
            {
                unsafe {
                    f.destroy_image.unwrap()(dev, (*s).image, ptr::null());
                    f.free_mem.unwrap()(dev, (*s).mem, ptr::null());
                }
                (*s).image = 0;
                (*s).mem = 0;
                (*s).stride = got.rowPitch as u32;
                continue;
            }
        }
        break;
    }
    slot_init_sync(sc, s)
}

unsafe fn slot_make_buffer(sc: *mut GnSwapchain, s: *mut GnSlot) -> *mut wl_proxy {
    let (dmabuf, queue) = {
        let w = G_WL.lock().unwrap();
        (w.dmabuf, w.queue)
    };
    let params = unsafe { wlproto::dmabuf_create_params(dmabuf) };
    if params.is_null() {
        return ptr::null_mut();
    }
    let _sg = SEND_LOCK.lock().unwrap();
    unsafe {
        wlproto::params_add(params, (*s).fd, 0, (*s).offset, (*s).stride, (*s).modifier);
        let buf = wlproto::params_create_immed(
            params,
            (*sc).extent.width as i32,
            (*sc).extent.height as i32,
            (*sc).fourcc,
            0,
        );
        wlproto::wl_proxy_destroy(params);
        if !buf.is_null() {
            wlproto::wl_proxy_add_listener(buf, BUFFER_LISTENER.as_ptr() as *mut _, s.cast());
            wlproto::wl_proxy_set_queue(buf, queue);
        }
        buf
    }
}

fn pop_job(sc: *mut GnSwapchain) -> i32 {
    let mut g = unsafe { (*sc).shared.lock().unwrap() };
    loop {
        if let Some(job) = g.jobs.pop_front() {
            return job;
        }
        if g.stopping {
            return -1;
        }
        let (guard, res) = unsafe { (*sc).cv_put.wait_timeout(g, IDLE_EVERY).unwrap() };
        g = guard;
        if res.timed_out() {
            // No present arrived: say so once every IDLE_EVERY with the slot
            // flags, so an app that stops presenting is visible instead of
            // silent.
            drop(g);
            if rate_ok(&RATE_IDLE, IDLE_EVERY) {
                crate::logf!(
                    "gn-vkshim idle: present worker has no work {}",
                    slot_flags(sc)
                );
            }
            g = unsafe { (*sc).shared.lock().unwrap() };
        }
    }
}

fn worker_main(sc: *mut GnSwapchain) {
    loop {
        let idx = pop_job(sc);
        if idx < 0 {
            break;
        }
        unsafe {
            let s = (*sc).slots.as_mut_ptr().add(idx as usize);
            let t0 = Instant::now();
            if let Some(wait_fences) = (*(*sc).dev).f.wait_fences {
                wait_fences((*(*sc).dev).real, 1, &(*s).fence, VK_TRUE, u64::MAX);
            }
            let fence_ms = t0.elapsed().as_millis();
            if (*s).buf.is_null() {
                (*s).buf = slot_make_buffer(sc, s);
            }
            if (*s).buf.is_null() {
                crate::logf!("proxy: wl_buffer creation failed — dropping frame");
                let _g = (*sc).shared.lock().unwrap();
                (*s).in_flight = false;
                (*sc).cv_free.notify_all();
                drop(_g);
                continue;
            }
            let parent = (*(*sc).surf).parent;
            let t1 = Instant::now();
            let flush_rc = {
                let _sg = SEND_LOCK.lock().unwrap();
                wlproto::surface_attach(parent, (*s).buf, 0, 0);
                wlproto::surface_damage_buffer(
                    parent,
                    0,
                    0,
                    (*sc).extent.width as i32,
                    (*sc).extent.height as i32,
                );
                wlproto::surface_commit(parent);
                wlproto::wl_display_flush((*(*sc).surf).dpy)
            };
            let commit_ms = t1.elapsed().as_millis();
            fps_note_commit();
            dump_slot(sc, s, idx);
            let n = LOG_COMMIT.fetch_add(1, Ordering::Relaxed);
            let done = N_COMMIT_OK.fetch_add(1, Ordering::Relaxed);
            if n < LOG_FIRST_N || n.is_multiple_of(120) || fence_ms >= 100 {
                crate::logf!(
                    "proxy commit: sc={sc:?} idx={idx} {}x{} flush_rc={flush_rc} dpy_err={} fence={fence_ms}ms commit={commit_ms}ms #{} {}",
                    (*sc).extent.width,
                    (*sc).extent.height,
                    wlproto::wl_display_get_error((*(*sc).surf).dpy),
                    done + 1,
                    counters(),
                );
            }
        }
    }
}

unsafe extern "C" fn gn_vkCreateSwapchainKHR(
    device: VkDevice,
    ci: *const VkSwapchainCreateInfoKHR,
    _alloc: *const VkAllocationCallbacks,
    out: *mut VkSwapchainKHR,
) -> VkResult {
    let ci = unsafe { &*ci };
    let gd = unsafe { find_device(device) };
    let surf = unsafe { find_surface(ci.surface) };
    crate::logf!(
        "proxy swapchain create: device={device:?} surface={:#x} {}x{} fmt={} -> gd={:?} surf={:?}",
        ci.surface,
        ci.imageExtent.width,
        ci.imageExtent.height,
        ci.imageFormat,
        gd,
        surf
    );
    if gd.is_null() || surf.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let fourcc = vk_format_to_fourcc(ci.imageFormat);
    if fourcc == 0 {
        crate::logf!("proxy: unsupported swapchain format {}", ci.imageFormat);
        return VK_ERROR_FORMAT_NOT_SUPPORTED;
    }
    // Only the scan-out pool's own layout can be served end to end (see
    // `advertises_rgba`): the guest often renders straight into a daemon-owned
    // BGRA buffer, so a swapchain in any other channel order would publish
    // R/B-exchanged pixels. Refuse it loudly rather than render wrong colours.
    if !swapchain_format_allowed(ci.imageFormat, advertises_rgba()) {
        crate::logf!(
            "proxy: refusing swapchain format {} (scan-out pool is B8G8R8A8; set GN_VKSHIM_ALLOW_RGBA to opt in)",
            ci.imageFormat
        );
        return VK_ERROR_FORMAT_NOT_SUPPORTED;
    }
    let mut n_slots = ci.minImageCount + 1;
    if n_slots < 2 {
        n_slots = 2;
    }
    if n_slots > max_swapchain_images() {
        n_slots = max_swapchain_images();
    }
    let mut sc = Box::new(GnSwapchain {
        surf,
        dev: gd,
        format: ci.imageFormat,
        extent: ci.imageExtent,
        fourcc,
        usage: ci.imageUsage | VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
        slots: Vec::new(),
        worker: None,
        shared: Mutex::new(ScShared {
            jobs: VecDeque::with_capacity(GN_JOB_CAP),
            stopping: false,
        }),
        cv_put: Condvar::new(),
        cv_free: Condvar::new(),
        next: ptr::null_mut(),
    });
    let scp: *mut GnSwapchain = &mut *sc;
    for _ in 0..n_slots {
        sc.slots.push(GnSlot {
            owner: scp,
            image: 0,
            mem: 0,
            fd: -1,
            stride: 0,
            offset: 0,
            modifier: 0,
            buf: ptr::null_mut(),
            fence: 0,
            sem: 0,
            acquired: false,
            in_flight: false,
        });
    }
    for inited in 0..n_slots as usize {
        let res = unsafe { slot_init(scp, sc.slots.as_mut_ptr().add(inited)) };
        if res != VK_SUCCESS {
            for j in (0..inited).rev() {
                unsafe { slot_release(scp, sc.slots.as_mut_ptr().add(j)) };
            }
            crate::logf!("proxy: slot init failed: {res}");
            return res;
        }
    }
    let sc_addr = scp as usize;
    let worker = std::thread::Builder::new()
        .name("gn-present".into())
        .spawn(move || worker_main(sc_addr as *mut GnSwapchain));
    let Ok(worker) = worker else {
        for j in 0..n_slots as usize {
            unsafe { slot_release(scp, sc.slots.as_mut_ptr().add(j)) };
        }
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    sc.worker = Some(worker);
    let sc = Box::into_raw(sc);
    {
        let mut g = G.lock().unwrap();
        unsafe {
            (*sc).next = g.swapchains;
            g.swapchains = sc;
        }
    }
    unsafe {
        *out = sc as usize as VkSwapchainKHR;
        crate::logf!(
            "proxy swapchain {:?}: {}x{} fmt {} -> fourcc {:#x}, {} images; requested usage={:#x} minImages={} presentMode={} compositeAlpha={:#x} colorspace={} clipped={} tiling=LINEAR",
            sc,
            (*sc).extent.width,
            (*sc).extent.height,
            (*sc).format,
            (*sc).fourcc,
            (*sc).slots.len(),
            ci.imageUsage,
            ci.minImageCount,
            ci.presentMode,
            ci.compositeAlpha,
            ci.imageColorSpace,
            ci.clipped
        );
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkGetSwapchainImagesKHR(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    count: *mut u32,
    images: *mut VkImage,
) -> VkResult {
    let sc = unsafe { find_swapchain(swapchain) };
    if sc.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe {
        let n_slots = (*sc).slots.len();
        if images.is_null() {
            *count = n_slots as u32;
            return VK_SUCCESS;
        }
        let n = (*count).min(n_slots as u32) as usize;
        for i in 0..n {
            *images.add(i) = (*(*sc).slots.as_ptr().add(i)).image;
        }
        *count = n as u32;
        if n < n_slots {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        }
    }
}

unsafe extern "C" fn gn_vkDestroySwapchainKHR(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    _alloc: *const VkAllocationCallbacks,
) {
    report_abandoned(None);
    let sc = unsafe { find_swapchain(swapchain) };
    if sc.is_null() {
        return;
    }
    {
        let mut g = unsafe { (*sc).shared.lock().unwrap() };
        g.stopping = true;
        unsafe {
            (*sc).cv_put.notify_all();
            (*sc).cv_free.notify_all();
        }
        drop(g);
    }
    if let Some(worker) = unsafe { (*sc).worker.take() } {
        let _ = worker.join();
    }
    for i in 0..unsafe { (*sc).slots.len() } {
        unsafe { slot_release(sc, (*sc).slots.as_mut_ptr().add(i)) };
    }
    {
        let _sg = SEND_LOCK.lock().unwrap();
        unsafe { wlproto::wl_display_flush((*(*sc).surf).dpy) };
    }
    {
        let mut g = G.lock().unwrap();
        let mut pp: *mut *mut GnSwapchain = &raw mut g.swapchains;
        unsafe {
            while !(*pp).is_null() && *pp != sc {
                pp = &raw mut (**pp).next;
            }
            if !(*pp).is_null() {
                *pp = (*sc).next;
            }
        }
    }
    unsafe {
        drop(Box::from_raw(sc));
    }
}

// Acquire picks a free slot (blocks up to timeout for the compositor's
// wl_buffer.release) and signals the app's semaphore/fence with an empty
// submit: the slot carries no GPU work of ours, the app renders into the
// imported dma-buf image directly.
unsafe fn gn_acquire_common(
    sc: *mut GnSwapchain,
    timeout: u64,
    sem: VkSemaphore,
    fence: VkFence,
    index: *mut u32,
) -> VkResult {
    let gd = (*sc).dev;
    let acq_t0 = Instant::now();
    N_ACQ.fetch_add(1, Ordering::Relaxed);
    let deadline = (timeout != u64::MAX).then(|| Instant::now() + Duration::from_nanos(timeout));
    let mut idx: i32 = -1;
    let mut timed_out = false;
    {
        let mut g = (*sc).shared.lock().unwrap();
        loop {
            for k in 0..(*sc).slots.len() {
                let s = (*sc).slots.as_mut_ptr().add(k);
                if !(*s).acquired && !(*s).in_flight {
                    idx = k as i32;
                    (*s).acquired = true;
                    break;
                }
            }
            if idx >= 0 || g.stopping {
                break;
            }
            if timeout == 0 {
                break;
            }
            match deadline {
                Some(dl) => {
                    let now = Instant::now();
                    if now >= dl {
                        timed_out = true;
                        break;
                    }
                    let (guard, res) = (*sc).cv_free.wait_timeout(g, dl - now).unwrap();
                    g = guard;
                    if res.timed_out() {
                        timed_out = true;
                        break;
                    }
                }
                None => {
                    g = (*sc).cv_free.wait(g).unwrap();
                }
            }
        }
        if g.stopping && idx < 0 {
            drop(g);
            return VK_ERROR_OUT_OF_DATE_KHR;
        }
        drop(g);
    }
    let _ = timed_out;
    if idx < 0 {
        // Every slot is held by the compositor. This is the signature to look
        // for when the game goes silent: an app that spins here is waiting for
        // a wl_buffer.release that never comes.
        N_STARVE.fetch_add(1, Ordering::Relaxed);
        if rate_ok(&RATE_STALL, RATE_EVERY) {
            crate::logf!(
                "gn-vkshim stall: acquire found no free slot (timeout={timeout}) {}",
                slot_flags(sc)
            );
        }
        return if timeout == 0 {
            VK_NOT_READY
        } else {
            VK_TIMEOUT
        };
    }
    if sem != 0 || fence != 0 {
        let gq = unsafe { (*gd).queues };
        if gq.is_null() {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        let si = VkSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            pNext: ptr::null(),
            waitSemaphoreCount: 0,
            pWaitSemaphores: ptr::null(),
            pWaitDstStageMask: ptr::null(),
            commandBufferCount: 0,
            pCommandBuffers: ptr::null(),
            signalSemaphoreCount: if sem != 0 { 1 } else { 0 },
            pSignalSemaphores: if sem != 0 { &sem } else { ptr::null() },
        };
        let res = unsafe { (*gd).f.submit.unwrap()((*gq).real, 1, &si, fence) };
        if res != VK_SUCCESS {
            let _g = (*sc).shared.lock().unwrap();
            (*(*sc).slots.as_mut_ptr().add(idx as usize)).acquired = false;
            (*sc).cv_free.notify_all();
            drop(_g);
            return res;
        }
    }
    *index = idx as u32;
    let acq_ms = acq_t0.elapsed().as_millis();
    let n = LOG_ACQ.fetch_add(1, Ordering::Relaxed);
    if n < LOG_FIRST_N || n.is_multiple_of(120) || acq_ms >= 100 {
        crate::logf!("proxy acquire: sc={sc:?} slot={idx} wait={acq_ms}ms");
    }
    VK_SUCCESS
}

unsafe extern "C" fn gn_vkAcquireNextImageKHR(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    timeout: u64,
    sem: VkSemaphore,
    fence: VkFence,
    index: *mut u32,
) -> VkResult {
    report_abandoned(None);
    let sc = unsafe { find_swapchain(swapchain) };
    if sc.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe { gn_acquire_common(sc, timeout, sem, fence, index) }
}

unsafe extern "C" fn gn_vkAcquireNextImage2KHR(
    _device: VkDevice,
    ai: *const VkAcquireNextImageInfoKHR,
    index: *mut u32,
) -> VkResult {
    let ai = unsafe { &*ai };
    let sc = unsafe { find_swapchain(ai.swapchain) };
    if sc.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    unsafe { gn_acquire_common(sc, ai.timeout, ai.semaphore, ai.fence, index) }
}

// Present: the app's render completion
unsafe extern "C" fn gn_vkQueuePresentKHR(queue: VkQueue, pi: *const VkPresentInfoKHR) -> VkResult {
    report_abandoned(None);
    N_PRES.fetch_add(1, Ordering::Relaxed);
    let pi = unsafe { &*pi };
    let gq = unsafe { find_queue(queue) };
    let mut gd = if gq.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*gq).gd }
    };
    if gd.is_null() {
        gd = G.lock().unwrap().devices;
    }
    if gd.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let real_q = if gq.is_null() {
        queue
    } else {
        unsafe { (*gq).real }
    };
    let nsc = (pi.swapchainCount as usize).min(GN_MAX_PRESENT);
    let mut chain_prev: VkSemaphore = 0;
    let swapchains = unsafe { slice_or_empty(pi.pSwapchains, nsc) };
    let indices = unsafe { slice_or_empty(pi.pImageIndices, nsc) };
    for i in 0..nsc {
        let sc = unsafe { find_swapchain(swapchains[i]) };
        if sc.is_null() || indices[i] as usize >= unsafe { (*sc).slots.len() } {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        let slot = unsafe { (*sc).slots.as_mut_ptr().add(indices[i] as usize) };
        let (waits, mut n_waits) = if chain_prev == 0 {
            (pi.pWaitSemaphores, pi.waitSemaphoreCount)
        } else {
            (&chain_prev as *const VkSemaphore, 1)
        };
        // Signal the chain semaphore only when a later present will wait
        // it; an unwaited binary semaphore could not be re-signaled.
        let signal_chain = i + 1 < nsc;
        unsafe {
            (*gd).f.reset_fences.unwrap()((*gd).real, 1, &(*slot).fence);
        }
        let mut wait_stages = [VK_PIPELINE_STAGE_ALL_COMMANDS_BIT; GN_MAX_PRESENT];
        if n_waits as usize > GN_MAX_PRESENT {
            n_waits = GN_MAX_PRESENT as u32;
        }
        let si = VkSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            pNext: ptr::null(),
            waitSemaphoreCount: n_waits,
            pWaitSemaphores: waits,
            pWaitDstStageMask: wait_stages.as_mut_ptr(),
            commandBufferCount: 0,
            pCommandBuffers: ptr::null(),
            signalSemaphoreCount: if signal_chain { 1 } else { 0 },
            pSignalSemaphores: if signal_chain {
                unsafe { &(*slot).sem }
            } else {
                ptr::null()
            },
        };
        let res = unsafe { (*gd).f.submit.unwrap()(real_q, 1, &si, (*slot).fence) };
        if res != VK_SUCCESS {
            crate::logf!("proxy: present wait submit failed: {res}");
            return res;
        }
        if signal_chain {
            chain_prev = unsafe { (*slot).sem };
        }
        let mut g = unsafe { (*sc).shared.lock().unwrap() };
        unsafe {
            (*slot).acquired = false;
            (*slot).in_flight = true;
            if g.jobs.len() < GN_JOB_CAP {
                g.jobs.push_back(indices[i] as i32);
                (*sc).cv_put.notify_one();
                drop(g);
            } else {
                // Job queue full: commit synchronously instead of dropping.
                (*slot).in_flight = false;
                drop(g);
                if let Some(wait_fences) = (*(*sc).dev).f.wait_fences {
                    wait_fences((*(*sc).dev).real, 1, &(*slot).fence, VK_TRUE, u64::MAX);
                }
                if (*slot).buf.is_null() {
                    (*slot).buf = slot_make_buffer(sc, slot);
                }
                if !(*slot).buf.is_null() {
                    let parent = (*(*sc).surf).parent;
                    let _sg = SEND_LOCK.lock().unwrap();
                    wlproto::surface_attach(parent, (*slot).buf, 0, 0);
                    wlproto::surface_damage_buffer(
                        parent,
                        0,
                        0,
                        (*sc).extent.width as i32,
                        (*sc).extent.height as i32,
                    );
                    wlproto::surface_commit(parent);
                    wlproto::wl_display_flush((*(*sc).surf).dpy);
                }
            }
        }
        if LOG_PRESENT.fetch_add(1, Ordering::Relaxed) < LOG_FIRST_N {
            crate::logf!("proxy present: sc={sc:?} idx={} {}", indices[i], counters());
        }
    }
    VK_SUCCESS
}

// The app's own submits carry shim wrapper command buffers; swap each for
// the real handle before handing the batch to the hooks.
unsafe extern "C" fn gn_vkQueueSubmit(
    queue: VkQueue,
    count: u32,
    submits: *const VkSubmitInfo,
    fence: VkFence,
) -> VkResult {
    let token = enter(
        E_SUBMIT,
        [
            queue as usize,
            count as usize,
            submits as usize,
            fence as usize,
            0,
        ],
    );
    let res = unsafe { submit_impl(queue, count, submits, fence) };
    leave(token);
    res
}

unsafe fn submit_impl(
    queue: VkQueue,
    count: u32,
    submits: *const VkSubmitInfo,
    fence: VkFence,
) -> VkResult {
    let gq = unsafe { find_queue(queue) };
    if gq.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let gd = unsafe { (*gq).gd };
    let mut copy = unsafe { slice_or_empty(submits, count as usize).to_vec() };
    // One slot per submit entry: an entry with no command buffers must not
    // shift the translated buffers of the entries after it.
    let mut swapped: Vec<Option<Vec<VkCommandBuffer>>> = Vec::with_capacity(copy.len());
    for si in copy.iter_mut() {
        let ncmd = si.commandBufferCount as usize;
        if ncmd == 0 {
            swapped.push(None);
            continue;
        }
        let pcs = unsafe { slice_or_empty(si.pCommandBuffers, ncmd) };
        let rc: Vec<VkCommandBuffer> = pcs.iter().map(|&c| unsafe { real_cmd(c) }).collect();
        swapped.push(Some(rc));
    }
    for (i, si) in copy.iter_mut().enumerate() {
        if let Some(rc) = &swapped[i] {
            si.pCommandBuffers = rc.as_ptr();
        }
    }
    {
        let seq = LOG_SUBMIT.fetch_add(1, Ordering::Relaxed);
        crate::logf!(
            "proxy submit #{seq}: n={count} q={queue:?} entries={:?} fence={fence:?}",
            copy.iter()
                .map(|s| s.commandBufferCount)
                .collect::<Vec<_>>()
        );
    }
    unsafe { (*gd).f.submit.unwrap()((*gq).real, count, copy.as_ptr(), fence) }
}

/// GPU waits. The guest blocks here when submitted work never completes, so log
/// what it wants against the last value it asked us to signal. The line is
/// printed BEFORE the wait, so if the wait never returns, the last line in the
/// log *is* the block; a returned wait longer than a second is logged too.
unsafe extern "C" fn gn_vkWaitSemaphores(
    device: VkDevice,
    wi: *const VkSemaphoreWaitInfo,
    timeout: u64,
) -> VkResult {
    report_abandoned(None);
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let Some(f) = (unsafe { (*gd).f.wait_semaphores }) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    if rate_ok(&RATE_WAIT, RATE_EVERY) {
        let info = unsafe { &*wi };
        let n = info.semaphoreCount as usize;
        let sems = unsafe { slice_or_empty(info.pSemaphores, n) };
        let vals = unsafe { slice_or_empty(info.pValues, n) };
        let wants = sems
            .iter()
            .zip(vals.iter())
            .take(4)
            .map(|(s, v)| format!("{s:?}@{v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let last = LAST_SIG
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|(s, v)| format!("{s}@{v}"))
            .unwrap_or_else(|| String::from("<none>"));
        crate::logf!("proxy wait: n={n} want=[{wants}] last_signalled={last} timeout={timeout}");
    }
    let t0 = Instant::now();
    let res = unsafe { f((*gd).real, wi, timeout) };
    let ms = t0.elapsed().as_millis();
    if ms >= 1000 {
        crate::logf!("proxy wait done: res={res} after={ms}ms");
    }
    res
}

unsafe extern "C" fn gn_vkQueueSubmit2(
    queue: VkQueue,
    count: u32,
    submits: *const VkSubmitInfo2,
    fence: VkFence,
) -> VkResult {
    let token = enter(
        E_SUBMIT2,
        [
            queue as usize,
            count as usize,
            submits as usize,
            fence as usize,
            0,
        ],
    );
    let res = unsafe { submit2_impl(queue, count, submits, fence) };
    leave(token);
    res
}

unsafe fn submit2_impl(
    queue: VkQueue,
    count: u32,
    submits: *const VkSubmitInfo2,
    fence: VkFence,
) -> VkResult {
    let gq = unsafe { find_queue(queue) };
    if gq.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let gd = unsafe { (*gq).gd };
    // Per device (see gn_vkGetDeviceQueue2): the hooks' resolved entry points
    // are tied to the device handle that resolved them.
    let Some(real_fn) = (unsafe { (*gd).f.submit2 }) else {
        return VK_ERROR_FEATURE_NOT_PRESENT;
    };
    let mut copy = unsafe { slice_or_empty(submits, count as usize).to_vec() };
    let mut swapped: Vec<Option<Vec<VkCommandBufferSubmitInfo>>> = Vec::with_capacity(copy.len());
    for si in copy.iter_mut() {
        let ncmd = si.commandBufferInfoCount as usize;
        if ncmd == 0 {
            swapped.push(None);
            continue;
        }
        let pcs = unsafe { slice_or_empty(si.pCommandBufferInfos, ncmd) };
        let mut rc: Vec<VkCommandBufferSubmitInfo> = pcs.to_vec();
        for c in rc.iter_mut() {
            c.commandBuffer = unsafe { real_cmd(c.commandBuffer) };
        }
        swapped.push(Some(rc));
    }
    for (i, si) in copy.iter_mut().enumerate() {
        if let Some(rc) = &swapped[i] {
            si.pCommandBufferInfos = rc.as_ptr();
        }
    }
    {
        // Every submit, terse: the faulting call is the last line before the
        // exception, and logcat still holds that tail. The abandonment tracker
        // reports whether it returned. The pairs are what we hand the driver —
        // a real handle on the left of no arrow means it was never translated.
        let seq = LOG_SUBMIT.fetch_add(1, Ordering::Relaxed);
        let mut cbs: Vec<String> = Vec::new();
        let mut sems: Vec<String> = Vec::new();
        {
            let originals = unsafe { slice_or_empty(submits, count as usize) };
            // Semaphores are the guest's cross-submit ordering: a wait that is
            // never signalled is what a spinning DXVK thread looks like from
            // here, so log the first few of each kind per entry.
            for si in originals.iter().take(2) {
                let nw = si.waitSemaphoreInfoCount as usize;
                let ws = unsafe {
                    slice_or_empty(si.pWaitSemaphoreInfos as *const VkSemaphoreSubmitInfo, nw)
                };
                for s in ws.iter().take(3) {
                    sems.push(format!("W{:?}@{}", s.semaphore, s.value));
                }
                let ns = si.signalSemaphoreInfoCount as usize;
                let ss = unsafe {
                    slice_or_empty(si.pSignalSemaphoreInfos as *const VkSemaphoreSubmitInfo, ns)
                };
                for s in ss.iter().take(3) {
                    sems.push(format!("S{:?}@{}", s.semaphore, s.value));
                    if let Ok(mut g) = LAST_SIG.lock() {
                        *g = Some((format!("{:?}", s.semaphore), s.value));
                    }
                }
            }
            for (i, si) in originals.iter().enumerate() {
                let n = si.commandBufferInfoCount as usize;
                let pcs = unsafe { slice_or_empty(si.pCommandBufferInfos, n) };
                for (j, c) in pcs.iter().enumerate().take(4) {
                    match swapped.get(i).and_then(|s| s.as_ref()) {
                        Some(rc) => {
                            cbs.push(format!("{:?}->{:?}", c.commandBuffer, rc[j].commandBuffer))
                        }
                        None => cbs.push(format!("{:?}->?", c.commandBuffer)),
                    }
                }
            }
        }
        crate::logf!(
            "proxy submit2 #{seq}: n={count} q={queue:?} real_q={:?} entries={:?} cbs={cbs:?} sems={sems:?} fence={fence:?}",
            unsafe { (*gq).real },
            copy.iter()
                .map(|s| s.commandBufferInfoCount)
                .collect::<Vec<_>>()
        );
    }
    unsafe { real_fn((*gq).real, count, copy.as_ptr(), fence) }
}

// Command buffers handed to the app are shim wrappers (same scratch-prefix
// layout as the device/queue handles), keeping the hooks' buffers private.
// This is load-bearing beyond privacy: every entry point the shim does not
// intercept is served by a generated thunk whose whole body is
// `ldr x0, [x0, #scratch*8]` — it reads the real handle out of the wrapper at
// a fixed offset and tail-branches to the hooks. Handing out a real buffer
// makes that read garbage and the driver faults on it (vkBeginCommandBuffer
// 0xc0000005).
unsafe extern "C" fn gn_vkAllocateCommandBuffers(
    device: VkDevice,
    ci: *const VkCommandBufferAllocateInfo,
    out: *mut VkCommandBuffer,
) -> VkResult {
    report_abandoned(None);
    let ci = unsafe { &*ci };
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let mut rc = vec![ptr::null_mut::<c_void>(); ci.commandBufferCount as usize];
    let res = unsafe { (*gd).f.alloc_cmd.unwrap()((*gd).real, ci, rc.as_mut_ptr()) };
    if res != VK_SUCCESS {
        return res;
    }
    {
        let _g = G.lock().unwrap();
        for (i, &real) in rc.iter().enumerate() {
            let gc = Box::new(GnCmd {
                scratch: [0; GN_HANDLE_SCRATCH],
                real,
                gd,
                next: ptr::null_mut(),
            });
            let gc = Box::into_raw(gc);
            unsafe {
                (*gc).next = (*gd).cmds;
                (*gd).cmds = gc;
                noted_wrapper(gc as usize);
                *out.add(i) = gc.cast::<c_void>();
                crate::logf!(
                    "proxy alloc cmd: wrapper={:?} real={real:?}",
                    gc.cast::<c_void>()
                );
            }
        }
    }
    VK_SUCCESS
}

// Bring-up: log the first occurrence of each image format the guest creates.
// Texture formats are the one Vulkan-level input we cannot see otherwise —
// DXVK logs a format only when its own table lacks it, so emulation gaps in
// the driver (e.g. BCn on Adreno) stay silent and show up as black elements.
static SEEN_IMAGE: Mutex<Vec<(VkFormat, VkImageUsageFlags, VkImageTiling)>> =
    Mutex::new(Vec::new());
const LOG_IMAGE_FIRST_N: usize = 48;

// ------------------------------ pipeline creation (winding compensation)
//
// This driver applies a negative-height (D3D-style) viewport to the image but
// does NOT invert the effective winding, so with cullMode enabled a client's
// front faces are culled as back faces. Measurements: a quad with
// frontFace=CW + cull BACK is correctly culled, and a flipped viewport does
// flip the image, yet a quad under a flipped viewport is not culled. DXVK
// renders D3D content with a flipped viewport, so its culling is inverted
// relative to what it asks for. Compensate by flipping frontFace on pipelines
// that cull. GN_VKSHIM_FLIP_FRONT_FACE=1 enables it.
unsafe extern "C" fn gn_vkCreateGraphicsPipelines(
    device: VkDevice,
    cache: VkPipelineCache,
    count: u32,
    cis: *const VkGraphicsPipelineCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkPipeline,
) -> VkResult {
    report_abandoned(None);
    let gd = unsafe { find_device(device) };
    if gd.is_null() || unsafe { (*gd).f.create_pipelines.is_none() } {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let real = unsafe { (*gd).f.create_pipelines.unwrap() };
    if std::env::var_os("GN_VKSHIM_FLIP_FRONT_FACE").is_none() || cis.is_null() {
        return unsafe { real((*gd).real, cache, count, cis, alloc, out) };
    }
    // Copy the create infos and the rasterization state we need to change:
    // the caller's structs are not ours to modify. Capacity is reserved up
    // front so the pointers into these vectors stay valid.
    let mut infos: Vec<VkGraphicsPipelineCreateInfo> = Vec::with_capacity(count as usize);
    let mut rasts: Vec<VkPipelineRasterizationStateCreateInfo> = Vec::with_capacity(count as usize);
    let mut flipped = 0u32;
    for i in 0..count as usize {
        let mut ci = unsafe { *cis.add(i) };
        if !ci.pRasterizationState.is_null() {
            let mut rs = unsafe { *ci.pRasterizationState };
            if rs.cullMode != VK_CULL_MODE_NONE {
                rs.frontFace = if rs.frontFace == VK_FRONT_FACE_COUNTER_CLOCKWISE {
                    VK_FRONT_FACE_CLOCKWISE
                } else {
                    VK_FRONT_FACE_COUNTER_CLOCKWISE
                };
                rasts.push(rs);
                ci.pRasterizationState = unsafe { rasts.as_ptr().add(rasts.len() - 1) };
                flipped += 1;
            }
        }
        infos.push(ci);
    }
    let res = unsafe { real((*gd).real, cache, count, infos.as_ptr(), alloc, out) };
    if flipped > 0 {
        crate::logf!("proxy: winding compensation applied to {flipped}/{count} pipeline(s)");
    }
    res
}

unsafe extern "C" fn gn_vkCreateImage(
    device: VkDevice,
    ci: *const VkImageCreateInfo,
    alloc: *const VkAllocationCallbacks,
    out: *mut VkImage,
) -> VkResult {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let ci_ref = unsafe { &*ci };
    let res = unsafe { (*gd).f.create_image.unwrap()((*gd).real, ci, alloc, out) };
    if res == VK_SUCCESS {
        let key = (ci_ref.format, ci_ref.usage, ci_ref.tiling);
        let mut seen = SEEN_IMAGE.lock().unwrap();
        if seen.len() < LOG_IMAGE_FIRST_N && !seen.contains(&key) {
            seen.push(key);
            crate::logf!(
                "guest image: fmt={} {}x{} mips={} layers={} samples={:#x} tiling={} usage={:#x}",
                ci_ref.format,
                ci_ref.extent.width,
                ci_ref.extent.height,
                ci_ref.mipLevels,
                ci_ref.arrayLayers,
                ci_ref.samples,
                ci_ref.tiling,
                ci_ref.usage
            );
        }
    }
    res
}

unsafe extern "C" fn gn_vkFreeCommandBuffers(
    device: VkDevice,
    pool: VkCommandPool,
    count: u32,
    cmds: *const VkCommandBuffer,
) {
    let gd = unsafe { find_device(device) };
    if gd.is_null() {
        return;
    }
    let mut rc = vec![ptr::null_mut::<c_void>(); count as usize];
    {
        let _g = G.lock().unwrap();
        for (i, slot) in rc.iter_mut().enumerate() {
            let cmd = unsafe { *cmds.add(i) };
            let gc = unsafe { find_cmd(gd, cmd) };
            if !gc.is_null() {
                *slot = unsafe { (*gc).real };
                unsafe {
                    let mut pp: *mut *mut GnCmd = &raw mut (*gd).cmds;
                    while !(*pp).is_null() && *pp != gc {
                        pp = &raw mut (**pp).next;
                    }
                    if !(*pp).is_null() {
                        *pp = (*gc).next;
                    }
                    drop(Box::from_raw(gc));
                }
            } else {
                *slot = cmd;
            }
        }
    }
    unsafe {
        if let Some(free_cmd) = (*gd).f.free_cmd {
            free_cmd((*gd).real, pool, count, rc.as_ptr());
        }
    }
}

// ------------------------------------------------------- dispatch tables

static G_INST_FUNCS: &[(&CStr, PFN_vkVoidFunction)] = &[
    (c"vkCreateInstance", unsafe {
        std::mem::transmute(
            gn_vkCreateInstance
                as unsafe extern "C" fn(
                    *const VkInstanceCreateInfo,
                    *const VkAllocationCallbacks,
                    *mut VkInstance,
                ) -> VkResult,
        )
    }),
    (c"vkDestroyInstance", unsafe {
        std::mem::transmute(
            gn_vkDestroyInstance as unsafe extern "C" fn(VkInstance, *const VkAllocationCallbacks),
        )
    }),
    (c"vkEnumerateInstanceExtensionProperties", unsafe {
        std::mem::transmute(
            gn_vkEnumerateInstanceExtensionProperties
                as unsafe extern "C" fn(
                    *const c_char,
                    *mut u32,
                    *mut VkExtensionProperties,
                ) -> VkResult,
        )
    }),
    (c"vkEnumerateDeviceExtensionProperties", unsafe {
        std::mem::transmute(
            gn_vkEnumerateDeviceExtensionProperties
                as unsafe extern "C" fn(
                    VkPhysicalDevice,
                    *const c_char,
                    *mut u32,
                    *mut VkExtensionProperties,
                ) -> VkResult,
        )
    }),
    (c"vkCreateDevice", unsafe {
        std::mem::transmute(
            gn_vkCreateDevice
                as unsafe extern "C" fn(
                    VkPhysicalDevice,
                    *const VkDeviceCreateInfo,
                    *const VkAllocationCallbacks,
                    *mut VkDevice,
                ) -> VkResult,
        )
    }),
    (c"vkGetDeviceProcAddr", unsafe {
        std::mem::transmute(
            gn_vkGetDeviceProcAddr
                as unsafe extern "C" fn(VkDevice, *const c_char) -> PFN_vkVoidFunction,
        )
    }),
    (c"vkCreateWaylandSurfaceKHR", unsafe {
        std::mem::transmute(
            gn_vkCreateWaylandSurfaceKHR
                as unsafe extern "C" fn(
                    VkInstance,
                    *const VkWaylandSurfaceCreateInfoKHR,
                    *const VkAllocationCallbacks,
                    *mut VkSurfaceKHR,
                ) -> VkResult,
        )
    }),
    (c"vkCreateXlibSurfaceKHR", unsafe {
        std::mem::transmute(
            gn_vkCreateXlibSurfaceKHR
                as unsafe extern "C" fn(
                    VkInstance,
                    *const VkXlibSurfaceCreateInfoKHR,
                    *const VkAllocationCallbacks,
                    *mut VkSurfaceKHR,
                ) -> VkResult,
        )
    }),
    (c"vkDestroySurfaceKHR", unsafe {
        std::mem::transmute(
            gn_vkDestroySurfaceKHR
                as unsafe extern "C" fn(VkInstance, VkSurfaceKHR, *const VkAllocationCallbacks),
        )
    }),
    (c"vkGetPhysicalDeviceSurfaceCapabilitiesKHR", unsafe {
        std::mem::transmute(
            gn_vkGetPhysicalDeviceSurfaceCapabilitiesKHR
                as unsafe extern "C" fn(
                    VkPhysicalDevice,
                    VkSurfaceKHR,
                    *mut VkSurfaceCapabilitiesKHR,
                ) -> VkResult,
        )
    }),
    (c"vkGetPhysicalDeviceSurfaceFormatsKHR", unsafe {
        std::mem::transmute(
            gn_vkGetPhysicalDeviceSurfaceFormatsKHR
                as unsafe extern "C" fn(
                    VkPhysicalDevice,
                    VkSurfaceKHR,
                    *mut u32,
                    *mut VkSurfaceFormatKHR,
                ) -> VkResult,
        )
    }),
    (c"vkGetPhysicalDeviceSurfacePresentModesKHR", unsafe {
        std::mem::transmute(
            gn_vkGetPhysicalDeviceSurfacePresentModesKHR
                as unsafe extern "C" fn(
                    VkPhysicalDevice,
                    VkSurfaceKHR,
                    *mut u32,
                    *mut VkPresentModeKHR,
                ) -> VkResult,
        )
    }),
    (c"vkGetPhysicalDeviceSurfaceSupportKHR", unsafe {
        std::mem::transmute(
            gn_vkGetPhysicalDeviceSurfaceSupportKHR
                as unsafe extern "C" fn(
                    VkPhysicalDevice,
                    u32,
                    VkSurfaceKHR,
                    *mut VkBool32,
                ) -> VkResult,
        )
    }),
    (
        c"vkGetPhysicalDeviceWaylandPresentationSupportKHR",
        unsafe {
            std::mem::transmute(
                gn_vkGetPhysicalDeviceWaylandPresentationSupportKHR
                    as unsafe extern "C" fn(VkPhysicalDevice, u32, *mut wl_display) -> VkBool32,
            )
        },
    ),
    (c"vkGetPhysicalDeviceXlibPresentationSupportKHR", unsafe {
        std::mem::transmute(
            gn_vkGetPhysicalDeviceXlibPresentationSupportKHR
                as unsafe extern "C" fn(VkPhysicalDevice, u32, *mut c_void, usize) -> VkBool32,
        )
    }),
];

static G_DEV_FUNCS: &[(&CStr, PFN_vkVoidFunction)] = &[
    (c"vkCreateSwapchainKHR", unsafe {
        std::mem::transmute(
            gn_vkCreateSwapchainKHR
                as unsafe extern "C" fn(
                    VkDevice,
                    *const VkSwapchainCreateInfoKHR,
                    *const VkAllocationCallbacks,
                    *mut VkSwapchainKHR,
                ) -> VkResult,
        )
    }),
    (c"vkDestroySwapchainKHR", unsafe {
        std::mem::transmute(
            gn_vkDestroySwapchainKHR
                as unsafe extern "C" fn(VkDevice, VkSwapchainKHR, *const VkAllocationCallbacks),
        )
    }),
    (c"vkGetSwapchainImagesKHR", unsafe {
        std::mem::transmute(
            gn_vkGetSwapchainImagesKHR
                as unsafe extern "C" fn(
                    VkDevice,
                    VkSwapchainKHR,
                    *mut u32,
                    *mut VkImage,
                ) -> VkResult,
        )
    }),
    (c"vkQueuePresentKHR", unsafe {
        std::mem::transmute(
            gn_vkQueuePresentKHR
                as unsafe extern "C" fn(VkQueue, *const VkPresentInfoKHR) -> VkResult,
        )
    }),
    (c"vkAcquireNextImageKHR", unsafe {
        std::mem::transmute(
            gn_vkAcquireNextImageKHR
                as unsafe extern "C" fn(
                    VkDevice,
                    VkSwapchainKHR,
                    u64,
                    VkSemaphore,
                    VkFence,
                    *mut u32,
                ) -> VkResult,
        )
    }),
    (c"vkAcquireNextImage2KHR", unsafe {
        std::mem::transmute(
            gn_vkAcquireNextImage2KHR
                as unsafe extern "C" fn(
                    VkDevice,
                    *const VkAcquireNextImageInfoKHR,
                    *mut u32,
                ) -> VkResult,
        )
    }),
    (c"vkQueueSubmit", unsafe {
        std::mem::transmute(
            gn_vkQueueSubmit
                as unsafe extern "C" fn(VkQueue, u32, *const VkSubmitInfo, VkFence) -> VkResult,
        )
    }),
    (c"vkQueueSubmit2", unsafe {
        std::mem::transmute(
            gn_vkQueueSubmit2
                as unsafe extern "C" fn(VkQueue, u32, *const VkSubmitInfo2, VkFence) -> VkResult,
        )
    }),
    (c"vkWaitSemaphoresKHR", unsafe {
        std::mem::transmute(
            gn_vkWaitSemaphores
                as unsafe extern "C" fn(VkDevice, *const VkSemaphoreWaitInfo, u64) -> VkResult,
        )
    }),
    (c"vkWaitSemaphores", unsafe {
        std::mem::transmute(
            gn_vkWaitSemaphores
                as unsafe extern "C" fn(VkDevice, *const VkSemaphoreWaitInfo, u64) -> VkResult,
        )
    }),
    (c"vkAllocateCommandBuffers", unsafe {
        std::mem::transmute(
            gn_vkAllocateCommandBuffers
                as unsafe extern "C" fn(
                    VkDevice,
                    *const VkCommandBufferAllocateInfo,
                    *mut VkCommandBuffer,
                ) -> VkResult,
        )
    }),
    (c"vkFreeCommandBuffers", unsafe {
        std::mem::transmute(
            gn_vkFreeCommandBuffers
                as unsafe extern "C" fn(VkDevice, VkCommandPool, u32, *const VkCommandBuffer),
        )
    }),
    (c"vkCreateGraphicsPipelines", unsafe {
        std::mem::transmute(
            gn_vkCreateGraphicsPipelines
                as unsafe extern "C" fn(
                    VkDevice,
                    VkPipelineCache,
                    u32,
                    *const VkGraphicsPipelineCreateInfo,
                    *const VkAllocationCallbacks,
                    *mut VkPipeline,
                ) -> VkResult,
        )
    }),
    (c"vkCreateImage", unsafe {
        std::mem::transmute(
            gn_vkCreateImage
                as unsafe extern "C" fn(
                    VkDevice,
                    *const VkImageCreateInfo,
                    *const VkAllocationCallbacks,
                    *mut VkImage,
                ) -> VkResult,
        )
    }),
    (c"vkGetDeviceQueue", unsafe {
        std::mem::transmute(
            gn_vkGetDeviceQueue as unsafe extern "C" fn(VkDevice, u32, u32, *mut VkQueue),
        )
    }),
    (c"vkGetDeviceQueue2", unsafe {
        std::mem::transmute(
            gn_vkGetDeviceQueue2
                as unsafe extern "C" fn(VkDevice, *const VkDeviceQueueInfo2, *mut VkQueue),
        )
    }),
    (c"vkDestroyDevice", unsafe {
        std::mem::transmute(
            gn_vkDestroyDevice as unsafe extern "C" fn(VkDevice, *const VkAllocationCallbacks),
        )
    }),
];

/// Interception table: the instance table is consulted for GIPA and GPDPA
/// lookups (surface functions resolve via both).
pub fn instance_func(name: &CStr) -> PFN_vkVoidFunction {
    G_INST_FUNCS
        .iter()
        .find(|(n, _)| *n == name)
        .and_then(|(_, f)| *f)
}

/// Interception table: the device table for vkGetDeviceProcAddr lookups.
/// For entry points the shim does not intercept: a thunk that swaps the
/// wrapper handle for the real one. Never the driver's own function — the
/// guest holds shim wrappers for devices, queues and command buffers, so a raw
/// entry point receives a wrapper and faults inside the driver. Reached from
/// the loader-ICD entry points, which answer instance- and device-level names
/// from one table; both must agree, or the guest ends up mixing wrapper and
/// raw handles across one command stream.
pub fn device_func(name: &CStr) -> PFN_vkVoidFunction {
    G_DEV_FUNCS
        .iter()
        .find(|(n, _)| *n == name)
        .and_then(|(_, f)| *f)
}

pub static GDPA_MISSED: Mutex<Vec<String>> = Mutex::new(Vec::new());
static REAL_PROC_SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" fn gn_vkGetDeviceProcAddr(
    device: VkDevice,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    let name = unsafe { CStr::from_ptr(name) };
    if unimplemented_wsi_ext(name) {
        // The proxy owns the swapchain: never hand the driver one of these with
        // a shim swapchain handle it has never seen.
        return None;
    }
    if let Some(f) = device_func(name) {
        return Some(f);
    }
    // Never the hooks' vkGetDeviceProcAddr (see gn_real_device_proc). The
    // loader will call whatever we return with the shim's wrapper handle,
    // so route the rest through a wrapper-swapping thunk.
    let target = unsafe { gn_real_device_proc(find_device(device), name) };
    if target.is_none() {
        // Returning NULL hands the guest the loader's no-op stub: the call
        // silently does nothing. Name every such entry point once.
        if let Ok(mut missed) = GDPA_MISSED.lock() {
            let n = name.to_string_lossy().into_owned();
            if missed.len() < 64 && !missed.contains(&n) {
                crate::logf!("proxy: vkGetDeviceProcAddr MISS (no-op stub): {n}");
                missed.push(n);
            }
        }
        return None;
    }
    thunk::thunk_for(name.to_str().unwrap(), target)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two layouts the proxy knows must map to the DRM fourcc of their
    /// memory order: BGRA memory is XRGB8888, RGBA memory is XBGR8888.
    #[test]
    fn format_to_fourcc_names_the_memory_order() {
        assert_eq!(
            vk_format_to_fourcc(VK_FORMAT_B8G8R8A8_UNORM),
            GN_DRM_FORMAT_XRGB8888
        );
        assert_eq!(
            vk_format_to_fourcc(VK_FORMAT_R8G8B8A8_UNORM),
            GN_DRM_FORMAT_XBGR8888
        );
        assert_eq!(
            vk_format_to_fourcc(37 + 1000),
            0,
            "unknown formats are refused"
        );
    }

    /// Regression for the Nidhogg B/R swap: only the scan-out pool's layout is
    /// accepted by default, so a guest cannot render RGBA into a buffer the
    /// daemon publishes as BGRA. RGBA is reachable only under the explicit A/B
    /// opt-in.
    #[test]
    fn only_the_scanout_layout_is_accepted_by_default() {
        assert!(swapchain_format_allowed(VK_FORMAT_B8G8R8A8_UNORM, false));
        assert!(swapchain_format_allowed(VK_FORMAT_B8G8R8A8_UNORM, true));
        assert!(!swapchain_format_allowed(VK_FORMAT_R8G8B8A8_UNORM, false));
        assert!(swapchain_format_allowed(VK_FORMAT_R8G8B8A8_UNORM, true));
        assert_eq!(SCANOUT_SWAPCHAIN_FORMAT, VK_FORMAT_B8G8R8A8_UNORM);
    }

    /// The override parser mirrors mesa's `MESA_VK_WSI_PRESENT_MODE`: exactly
    /// four names, matched case-sensitively; anything else is ignored.
    #[test]
    fn present_mode_override_matches_mesa() {
        assert_eq!(
            parse_present_mode_override(Some("fifo")),
            Some(VK_PRESENT_MODE_FIFO_KHR)
        );
        assert_eq!(
            parse_present_mode_override(Some("relaxed")),
            Some(VK_PRESENT_MODE_FIFO_RELAXED_KHR)
        );
        assert_eq!(
            parse_present_mode_override(Some("mailbox")),
            Some(VK_PRESENT_MODE_MAILBOX_KHR)
        );
        assert_eq!(
            parse_present_mode_override(Some("immediate")),
            Some(VK_PRESENT_MODE_IMMEDIATE_KHR)
        );
        // mesa's strcmp is case-sensitive, and its UI also carries label
        // strings ("Never"/"Normal"/"Always") that are not valid names.
        for bad in ["FIFO", "Immediate", "Never", "Normal", "Always", ""] {
            assert_eq!(parse_present_mode_override(Some(bad)), None, "{bad}");
        }
        assert_eq!(parse_present_mode_override(None), None);
    }

    /// The offered list leads with a served override, keeps the served order
    /// otherwise, drops an override the ring cannot serve, and always offers
    /// FIFO.
    #[test]
    fn offered_list_prefers_override_and_always_offers_fifo() {
        assert_eq!(
            offered_present_modes(&SERVED_PRESENT_MODES, None),
            [
                VK_PRESENT_MODE_IMMEDIATE_KHR,
                VK_PRESENT_MODE_MAILBOX_KHR,
                VK_PRESENT_MODE_FIFO_KHR
            ]
        );
        assert_eq!(
            offered_present_modes(&SERVED_PRESENT_MODES, Some(VK_PRESENT_MODE_IMMEDIATE_KHR)),
            [
                VK_PRESENT_MODE_IMMEDIATE_KHR,
                VK_PRESENT_MODE_MAILBOX_KHR,
                VK_PRESENT_MODE_FIFO_KHR
            ]
        );
        // FIFO leads when it is the override, and is not duplicated.
        assert_eq!(
            offered_present_modes(&SERVED_PRESENT_MODES, Some(VK_PRESENT_MODE_FIFO_KHR)),
            [
                VK_PRESENT_MODE_FIFO_KHR,
                VK_PRESENT_MODE_IMMEDIATE_KHR,
                VK_PRESENT_MODE_MAILBOX_KHR
            ]
        );
        // An override the ring cannot serve is dropped, not advertised.
        assert_eq!(
            offered_present_modes(
                &[VK_PRESENT_MODE_MAILBOX_KHR, VK_PRESENT_MODE_FIFO_KHR],
                Some(VK_PRESENT_MODE_FIFO_RELAXED_KHR)
            ),
            [VK_PRESENT_MODE_MAILBOX_KHR, VK_PRESENT_MODE_FIFO_KHR]
        );
        // FIFO is appended when the served set lacks it.
        assert_eq!(
            offered_present_modes(
                &[VK_PRESENT_MODE_MAILBOX_KHR, VK_PRESENT_MODE_IMMEDIATE_KHR],
                None
            ),
            [
                VK_PRESENT_MODE_MAILBOX_KHR,
                VK_PRESENT_MODE_IMMEDIATE_KHR,
                VK_PRESENT_MODE_FIFO_KHR
            ]
        );
        assert_eq!(
            offered_present_modes(&[VK_PRESENT_MODE_FIFO_KHR], None),
            [VK_PRESENT_MODE_FIFO_KHR]
        );
    }
}
