//! Hand-rolled FFI surface for the Android system libraries we link:
//! `liblog` and `libandroid` (`ANativeWindow`, `AHardwareBuffer`), plus the
//! dlopen'd `libandroid` SurfaceControl publish surface.
//!
//! Only what the presenter uses is declared. Constants come from the NDK
//! header `android/log.h`; the publish surface resolves its symbols by name
//! in [`ScApi::load`].

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use core::ffi::{c_char, c_int, c_void};

// ---------------------------------------------------------------- liblog
pub const ANDROID_LOG_INFO: c_int = 4;
pub const ANDROID_LOG_WARN: c_int = 5;
pub const ANDROID_LOG_ERROR: c_int = 6;

#[link(name = "log")]
unsafe extern "C" {
    pub fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

// ------------------------------------------------------------ libandroid
/// Opaque `ANativeWindow`.
#[repr(C)]
pub struct ANativeWindow {
    _private: [u8; 0],
}

#[link(name = "android")]
unsafe extern "C" {
    /// Acquires a reference on the window; release with
    /// [`ANativeWindow_release`].
    pub fn ANativeWindow_fromSurface(
        env: *mut jni::sys::JNIEnv,
        surface: jni::sys::jobject,
    ) -> *mut ANativeWindow;
    pub fn ANativeWindow_release(window: *mut ANativeWindow);
    pub fn ANativeWindow_getWidth(window: *mut ANativeWindow) -> i32;
    pub fn ANativeWindow_getHeight(window: *mut ANativeWindow) -> i32;
}

// -------------------------------------------------- AHardwareBuffer (AHB)
// `AHardwareBuffer` is API 26, so the entry points below are linked directly
// (the crate links at API 28). `AHardwareBuffer_getNativeHandle` is the
// exception: it is not in the NDK headers at any level, so it is dlsym'd
// alongside the SurfaceControl surface (see [`ScApi`]).
//
// The daemon allocates the scan-out buffers and keeps them, because it is the
// process that publishes them through `SurfaceControl`: an `AHardwareBuffer`
// cannot be reconstructed from a bare dma-buf fd on API 33 (there is no
// `AHardwareBuffer_createFromHandle` in the NDK surface, and the handle
// round-trip is not a supported transfer), so the process that must call
// `ASurfaceTransaction_setBuffer` has to be the process that called
// `AHardwareBuffer_allocate`.

/// Opaque `AHardwareBuffer`.
#[repr(C)]
pub struct AHardwareBuffer {
    _private: [u8; 0],
}

/// `AHardwareBuffer_Desc` (`<android/hardware_buffer.h>`), the layout the NDK
/// declares: four `u32`, a `u64`, then two spare words.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AHardwareBufferDesc {
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    pub format: u32,
    pub usage: u64,
    pub stride: u32,
    pub rfu0: u32,
    pub rfu1: u64,
}

/// `AHARDWAREBUFFER_USAGE_*` bits the scan-out pool uses.
///
/// `CPU_READ_NEVER`/`CPU_WRITE_NEVER` are both `0`, i.e. absent from `usage`
/// when unset. The pool declares `CPU_READ_OFTEN` because the daemon locks the
/// buffer to dump the published frame (see `asc::dump_published`); gamescope
/// still maps the raw dma-buf to render, it does not go through the lock.
pub const AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN: u64 = 1 << 1;
pub const AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE: u64 = 1 << 8;
pub const AHARDWAREBUFFER_USAGE_GPU_COLOR_OUTPUT: u64 = 1 << 9;
pub const AHARDWAREBUFFER_USAGE_COMPOSER_OVERLAY: u64 = 1 << 11;

/// The scan-out pool's usage: CPU-readable for the panel dump, sampled by the
/// composer, written by the GPU as a colour attachment, and usable as an
/// overlay plane. `GPU_SAMPLED_IMAGE`/`GPU_COLOR_OUTPUT`/`COMPOSER_OVERLAY`
/// are the flags the shim allocated with before the daemon owned the pool;
/// `CPU_READ_OFTEN` was added afterwards for the panel dump, so the
/// linear-and-directly-composable result does not attest this set — gralloc
/// may pick a different layout once a CPU-visible usage is present.
pub const SCANOUT_POOL_USAGE: u64 = AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN
    | AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE
    | AHARDWAREBUFFER_USAGE_GPU_COLOR_OUTPUT
    | AHARDWAREBUFFER_USAGE_COMPOSER_OVERLAY;

/// `native_handle_t` prefix (`<cutils/native_handle.h>`); the NDK does not
/// expose the type. `data` is a flexible array whose first `num_fds` entries
/// are file descriptors; the first fd of an `AHardwareBuffer` handle is the
/// dma-buf that backs it.
#[repr(C)]
pub struct NativeHandle {
    pub version: c_int,
    pub num_fds: c_int,
    pub num_ints: c_int,
}

impl NativeHandle {
    /// The `i`-th file descriptor of `nh` (`data[i]`, right after the three
    /// header ints). `None` when the index is outside `num_fds`.
    ///
    /// # Safety
    /// `nh` must point at a native handle with `num_fds` descriptors.
    #[must_use]
    pub unsafe fn fd(nh: *const NativeHandle, i: usize) -> Option<c_int> {
        // SAFETY: the header is three ints; the caller guarantees the
        // flexible array that follows holds `num_fds` descriptors.
        unsafe {
            let n = (*nh).num_fds;
            if n < 0 || i >= n as usize {
                return None;
            }
            Some(*(nh.cast::<c_int>().add(3).add(i)))
        }
    }
}

#[link(name = "android")]
unsafe extern "C" {
    /// Allocates a gralloc buffer; the caller owns one reference and releases
    /// it with [`AHardwareBuffer_release`]. Returns 0 or negative errno.
    pub fn AHardwareBuffer_allocate(
        desc: *const AHardwareBufferDesc,
        out_buffer: *mut *mut AHardwareBuffer,
    ) -> c_int;
    /// Fills `out_desc` with the buffer's real geometry, stride and usage.
    pub fn AHardwareBuffer_describe(
        buffer: *const AHardwareBuffer,
        out_desc: *mut AHardwareBufferDesc,
    );
    /// Locks `buffer` for CPU access under `usage` (a subset of the allocation
    /// usage) and stores the base address of the mapped pixels in
    /// `out_virtual_address`; release with [`AHardwareBuffer_unlock`]. `fence`
    /// is `-1` for no wait, `rect` null for the whole buffer. Returns 0 or a
    /// negative errno; `out_virtual_address` is null on failure.
    pub fn AHardwareBuffer_lock(
        buffer: *mut AHardwareBuffer,
        usage: u64,
        fence: c_int,
        rect: *const ARect,
        out_virtual_address: *mut *mut c_void,
    ) -> c_int;
    /// Releases a lock taken by [`AHardwareBuffer_lock`]. `fence` is null when
    /// the caller does not want the buffer's release fence.
    pub fn AHardwareBuffer_unlock(buffer: *mut AHardwareBuffer, fence: *mut c_int);
    pub fn AHardwareBuffer_release(buffer: *mut AHardwareBuffer);
}

// ------------------------------------ ASurfaceControl (API 29, dlopen only)
// The publish surface is API 29 and this crate builds at API 28, so the
// symbols cannot be referenced at link time: the table below is filled by
// dlopen+dlsym from `libandroid.so` (mirrors GameNative's proven
// `ASurfaceRendererContext::loadScanoutApi`).

/// Opaque `ASurfaceControl`.
#[repr(C)]
pub struct ASurfaceControl {
    _private: [u8; 0],
}
/// Opaque `ASurfaceTransaction`.
#[repr(C)]
pub struct ASurfaceTransaction {
    _private: [u8; 0],
}
/// Opaque `ASurfaceTransactionStats`.
#[repr(C)]
pub struct ASurfaceTransactionStats {
    _private: [u8; 0],
}

/// `ARect` (`<android/rect.h>`): integer bounds, `left <= right`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ARect {
    pub left: c_int,
    pub top: c_int,
    pub right: c_int,
    pub bottom: c_int,
}

/// `ADataSpace` value for "unspecified"; the background colour below is not
/// colour-managed.
pub const ADATASPACE_UNKNOWN: c_int = 0;

pub type FnScCreateFromWindow = unsafe extern "C" fn(
    window: *mut ANativeWindow,
    debug_name: *const c_char,
) -> *mut ASurfaceControl;
pub type FnScRelease = unsafe extern "C" fn(sc: *mut ASurfaceControl);
pub type FnStCreate = unsafe extern "C" fn() -> *mut ASurfaceTransaction;
pub type FnStDelete = unsafe extern "C" fn(tx: *mut ASurfaceTransaction);
pub type FnStApply = unsafe extern "C" fn(tx: *mut ASurfaceTransaction);
pub type FnStSetBuffer = unsafe extern "C" fn(
    tx: *mut ASurfaceTransaction,
    sc: *mut ASurfaceControl,
    buffer: *mut AHardwareBuffer,
    acquire_fence_fd: c_int,
);
pub type FnStSetZOrder =
    unsafe extern "C" fn(tx: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, z: i32);
pub type FnStSetVisibility =
    unsafe extern "C" fn(tx: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, visible: i8);
pub type FnStSetGeometry = unsafe extern "C" fn(
    tx: *mut ASurfaceTransaction,
    sc: *mut ASurfaceControl,
    source: *const ARect,
    destination: *const ARect,
    transform: i32,
);
pub type FnStSetColor = unsafe extern "C" fn(
    tx: *mut ASurfaceTransaction,
    sc: *mut ASurfaceControl,
    r: f32,
    g: f32,
    b: f32,
    alpha: f32,
    dataspace: c_int,
);
pub type FnStOnComplete =
    unsafe extern "C" fn(context: *mut c_void, stats: *mut ASurfaceTransactionStats);
pub type FnStSetOnComplete = unsafe extern "C" fn(
    tx: *mut ASurfaceTransaction,
    context: *mut c_void,
    callback: FnStOnComplete,
);
pub type FnStStatsGetPreviousReleaseFenceFd =
    unsafe extern "C" fn(stats: *mut ASurfaceTransactionStats, sc: *mut ASurfaceControl) -> c_int;
/// `ASurfaceTransactionStats_getASurfaceControls`: the controls a completed
/// transaction itself updated. The array it stores in `out_controls` is owned by
/// the caller until `FnStStatsReleaseASurfaceControls` is called on it.
pub type FnStStatsGetASurfaceControls = unsafe extern "C" fn(
    stats: *mut ASurfaceTransactionStats,
    out_controls: *mut *mut *mut ASurfaceControl,
    out_count: *mut usize,
);
pub type FnStStatsReleaseASurfaceControls =
    unsafe extern "C" fn(controls: *mut *mut ASurfaceControl);
pub type FnAhbGetNativeHandle =
    unsafe extern "C" fn(buffer: *const AHardwareBuffer) -> *mut NativeHandle;

/// `<android/looper.h>`. Optional in the table below.
///
/// `ASurfaceTransaction` completions are delivered by libgui on its own binder
/// thread (`TransactionCompletedListener::onTransactionCompleted`), so an
/// `ALooper` is *not* on the callback's delivery path. One is still prepared and
/// pumped on the presenting thread because the NDK requires the polling thread
/// to own the looper — polling a looper prepared on another thread logs
/// `ALooper_pollOnce: No looper for this thread!` — and because it drains
/// anything SurfaceFlinger queued to that thread.
pub type FnALooperPrepare = unsafe extern "C" fn(opts: c_int) -> *mut c_void;
pub type FnALooperPollOnce = unsafe extern "C" fn(
    timeout_millis: c_int,
    out_fd: *mut c_int,
    out_events: *mut c_int,
    out_data: *mut *mut c_void,
) -> c_int;

#[derive(Clone, Copy)]
pub struct LooperApi {
    pub prepare: FnALooperPrepare,
    pub poll_once: FnALooperPollOnce,
}

/// The dlopen'd native-publish surface. All entries are required: a partial
/// table would publish with missing geometry (frames land in the wrong place
/// rather than failing), so a missing symbol disables the presenter.
#[derive(Clone, Copy)]
pub struct ScApi {
    pub sc_create_from_window: FnScCreateFromWindow,
    pub sc_release: FnScRelease,
    pub st_create: FnStCreate,
    pub st_delete: FnStDelete,
    pub st_apply: FnStApply,
    pub st_set_buffer: FnStSetBuffer,
    pub st_set_zorder: FnStSetZOrder,
    pub st_set_visibility: FnStSetVisibility,
    pub st_set_geometry: FnStSetGeometry,
    pub st_set_color: FnStSetColor,
    pub st_set_on_complete: FnStSetOnComplete,
    pub st_stats_previous_release_fence_fd: FnStStatsGetPreviousReleaseFenceFd,
    pub st_stats_get_controls: FnStStatsGetASurfaceControls,
    pub st_stats_release_controls: FnStStatsReleaseASurfaceControls,
    pub ahb_get_native_handle: FnAhbGetNativeHandle,
    pub looper: Option<LooperApi>,
}

impl ScApi {
    /// Resolve the table from the already-loaded `libandroid.so`. `None` when
    /// any symbol is absent (API < 29, or a build without the native publish
    /// surface) — the caller logs once and stops publishing.
    #[must_use]
    pub fn load() -> Option<Self> {
        // SAFETY: dlopen/dlsym on a library name that is a valid C string;
        // RTLD_NOLOAD asks for the copy the process already has (libandroid is
        // loaded because ANativeWindow is used), with a plain dlopen as the
        // fallback.
        let handle = unsafe {
            let h = libc::dlopen(
                c"libandroid.so".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_NOLOAD,
            );
            if h.is_null() {
                libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW)
            } else {
                h
            }
        };
        if handle.is_null() {
            return None;
        }
        // SAFETY: `handle` is a live dlopen handle; each symbol is transmuted
        // into the C signature the platform declares for that name.
        unsafe {
            let looper = match (
                sym::<FnALooperPrepare>(handle, c"ALooper_prepare"),
                sym::<FnALooperPollOnce>(handle, c"ALooper_pollOnce"),
            ) {
                (Some(prepare), Some(poll_once)) => Some(LooperApi { prepare, poll_once }),
                _ => None,
            };
            Some(Self {
                sc_create_from_window: sym(handle, c"ASurfaceControl_createFromWindow")?,
                sc_release: sym(handle, c"ASurfaceControl_release")?,
                st_create: sym(handle, c"ASurfaceTransaction_create")?,
                st_delete: sym(handle, c"ASurfaceTransaction_delete")?,
                st_apply: sym(handle, c"ASurfaceTransaction_apply")?,
                st_set_buffer: sym(handle, c"ASurfaceTransaction_setBuffer")?,
                st_set_zorder: sym(handle, c"ASurfaceTransaction_setZOrder")?,
                st_set_visibility: sym(handle, c"ASurfaceTransaction_setVisibility")?,
                st_set_geometry: sym(handle, c"ASurfaceTransaction_setGeometry")?,
                st_set_color: sym(handle, c"ASurfaceTransaction_setColor")?,
                st_set_on_complete: sym(handle, c"ASurfaceTransaction_setOnComplete")?,
                st_stats_previous_release_fence_fd: sym(
                    handle,
                    c"ASurfaceTransactionStats_getPreviousReleaseFenceFd",
                )?,
                st_stats_get_controls: sym(
                    handle,
                    c"ASurfaceTransactionStats_getASurfaceControls",
                )?,
                st_stats_release_controls: sym(
                    handle,
                    c"ASurfaceTransactionStats_releaseASurfaceControls",
                )?,
                ahb_get_native_handle: sym(handle, c"AHardwareBuffer_getNativeHandle")?,
                looper,
            })
        }
    }
}

/// `dlsym` one symbol as `T`.
///
/// # Safety
/// `handle` must be a live `dlopen` handle and `T` must be the exact C
/// signature the platform exports under `name`.
unsafe fn sym<T: Copy>(handle: *mut c_void, name: &core::ffi::CStr) -> Option<T> {
    // SAFETY: caller guarantees the handle; `name` is NUL-terminated.
    let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if p.is_null() {
        None
    } else {
        // SAFETY: function pointers and data pointers are the same width on
        // this ABI, and `T` is the symbol's declared signature (see caller).
        Some(unsafe { core::mem::transmute_copy::<*mut c_void, T>(&p) })
    }
}
