//! X11 DRI3 + Present present route: the guest's window content.
//!
//! The entry point the guest reaches is `vkCreateXlibSurfaceKHR`: wine's
//! winex11.drv creates a host Xlib surface for the app window and hands the
//! shim that window's XID (`dlls/winex11.drv/vulkan.c`, X11DRV_vulkan_surface_create
//! -> `x11drv_client_surface_create(hwnd, ...)` -> `p_vkCreateXlibSurfaceKHR`).
//! `VK_KHR_win32_surface` never reaches an ICD: it is winevulkan's client-side
//! name for the app, and `wine_vulkan_map_instance_extensions` maps it onto the
//! active display driver's host extension (winex11 -> xlib, winewayland ->
//! wayland) in `dlls/win32u/vulkan.c` (vulkan_init_once -> p_map_instance_extensions).
//! So the HWND -> X window mapping is wine's, and what arrives here is already
//! the X window.
//!
//! The guest's swapped dma-buf becomes that window's own content through a
//! normal X11 present, so one window carries the guest's frame and wine's GDI
//! 2D together. The X connection is the guest's own X server (Xwayland,
//! `DISPLAY`) - no compositor-specific protocol is involved.
//!
//! The window the present lands in is the ROOT CHILD that contains the window
//! wine named, not necessarily that window itself. A compositor's window
//! manager enumerates only direct children of the X root (gamescope's
//! `steamcompmgr` `CreateNotify` handler is gated on `parent == ctx->root`),
//! and under rootless Xwayland only a window redirected as a direct root
//! child gets a `wl_surface` (`xwl_window_is_toplevel` /
//! `ensure_surface_for_window`). winex11.drv hands the ICD its *client*
//! window - a child of the WM-managed toplevel (`create_client_window` nests
//! it under `whole_window`) - so presenting into that XID verbatim would
//! target a window the compositor does not enumerate. `resolve_target` walks
//! from the named window to its root-child ancestor, mirroring the walk the
//! compositor's own `x11_find_toplevel_for_xid` performs, and reports the
//! ancestor-relative offset so `Present`'s own x/y placement stays correct.
//!
//! Per present: `xcb_dri3_pixmap_from_buffers` imports the slot's dma-buf as an
//! X pixmap (no CPU copy, the fd stays the shim's), and `xcb_present_pixmap`
//! makes that pixmap the window's content. Release rides an X sync fence passed
//! as Present's idle fence: the server triggers it when it is done with the
//! pixmap, which is the same slot-reuse contract `wl_buffer.release` provides
//! on the wayland route. A pixmap and a fence are created once per swapchain
//! slot and destroyed with it; the slot stays in flight until its idle fence
//! triggers.
//!
//! GPU ordering is carried by the dma-buf's implicit reservation (the present
//! worker has already CPU-waited the slot's present fence), which is what the
//! wayland route relied on too. A driver-exported sync_file would be needed for
//! Present's acquire fence; this shim's real driver exports none.

use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

// --------------------------------------------------------------- xcb ABI
//
// Only the subset this route uses. Every declaration is linked (the payload
// ships the libxcb set), so a missing symbol is a load error, not a silent
// runtime miss.

#[allow(non_camel_case_types)]
type xcb_connection_t = c_void;

/// `xcb_void_cookie_t` and the geometry/query cookies are a single `unsigned
/// int` sequence returned by value.
#[repr(C)]
#[derive(Clone, Copy)]
struct cookie_t {
    sequence: u32,
}

#[repr(C)]
struct xcb_query_extension_reply_t {
    response_type: u8,
    pad0: u8,
    sequence: u16,
    length: u32,
    present: u8,
    major_opcode: u8,
    first_event: u8,
    first_error: u8,
}

#[repr(C)]
struct xcb_extension_t {
    name: *const c_char,
    present: c_int,
}

#[repr(C)]
struct xcb_get_geometry_reply_t {
    response_type: u8,
    depth: u8,
    sequence: u16,
    length: u32,
    root: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
    pad0: [u8; 2],
}

#[repr(C)]
struct xcb_generic_error_t {
    response_type: u8,
    error_code: u8,
    sequence: u16,
    resource_id: u32,
    minor_code: u16,
    major_code: u8,
    pad0: u8,
    pad1: [u8; 4],
}

#[repr(C)]
struct xcb_dri3_query_version_reply_t {
    response_type: u8,
    pad0: u8,
    sequence: u16,
    length: u32,
    major_version: u32,
    minor_version: u32,
}

#[repr(C)]
struct xcb_present_query_version_reply_t {
    response_type: u8,
    pad0: u8,
    sequence: u16,
    length: u32,
    major_version: u32,
    minor_version: u32,
}

#[repr(C)]
struct xcb_sync_query_fence_reply_t {
    response_type: u8,
    pad0: u8,
    sequence: u16,
    length: u32,
    triggered: u8,
    pad1: [u8; 23],
}

#[repr(C)]
struct xcb_query_tree_reply_t {
    response_type: u8,
    pad0: u8,
    sequence: u16,
    length: u32,
    root: u32,
    parent: u32,
    children_len: u16,
    pad1: [u8; 14],
}

#[repr(C)]
struct xcb_get_window_attributes_reply_t {
    response_type: u8,
    backing_store: u8,
    sequence: u16,
    length: u32,
    visual: u32,
    class_: u16,
    bit_gravity: u8,
    win_gravity: u8,
    backing_planes: u32,
    backing_pixel: u32,
    save_under: u8,
    map_is_installed: u8,
    map_state: u8,
    override_redirect: u8,
    colormap: u32,
    all_event_masks: u32,
    your_event_mask: u32,
    do_not_propagate_mask: u16,
    pad0: [u8; 2],
}

#[repr(C)]
struct xcb_intern_atom_reply_t {
    response_type: u8,
    pad0: u8,
    sequence: u16,
    length: u32,
    atom: u32,
}

#[repr(C)]
struct xcb_get_property_reply_t {
    response_type: u8,
    format: u8,
    sequence: u16,
    length: u32,
    type_: u32,
    bytes_after: u32,
    value_len: u32,
    pad0: [u8; 12],
}

#[repr(C)]
struct xcb_get_input_focus_reply_t {
    response_type: u8,
    pad0: u8,
    sequence: u16,
    length: u32,
    focus: u32,
}

#[link(name = "xcb")]
extern "C" {
    fn xcb_connect(displayname: *const c_char, screenp: *mut c_int) -> *mut xcb_connection_t;
    fn xcb_disconnect(c: *mut xcb_connection_t);
    fn xcb_connection_has_error(c: *mut xcb_connection_t) -> c_int;
    fn xcb_generate_id(c: *mut xcb_connection_t) -> u32;
    fn xcb_flush(c: *mut xcb_connection_t) -> c_int;
    fn xcb_get_extension_data(
        c: *mut xcb_connection_t,
        ext: *const xcb_extension_t,
    ) -> *const xcb_query_extension_reply_t;
    fn xcb_get_geometry(c: *mut xcb_connection_t, drawable: u32) -> cookie_t;
    fn xcb_get_geometry_reply(
        c: *mut xcb_connection_t,
        cookie: cookie_t,
        e: *mut *mut xcb_generic_error_t,
    ) -> *mut xcb_get_geometry_reply_t;
    fn xcb_free_pixmap(c: *mut xcb_connection_t, pixmap: u32) -> cookie_t;
    fn xcb_request_check(c: *mut xcb_connection_t, cookie: cookie_t) -> *mut xcb_generic_error_t;
    fn xcb_query_tree(c: *mut xcb_connection_t, window: u32) -> cookie_t;
    fn xcb_query_tree_reply(
        c: *mut xcb_connection_t,
        cookie: cookie_t,
        e: *mut *mut xcb_generic_error_t,
    ) -> *mut xcb_query_tree_reply_t;
    fn xcb_get_window_attributes(c: *mut xcb_connection_t, window: u32) -> cookie_t;
    fn xcb_get_window_attributes_reply(
        c: *mut xcb_connection_t,
        cookie: cookie_t,
        e: *mut *mut xcb_generic_error_t,
    ) -> *mut xcb_get_window_attributes_reply_t;
    fn xcb_intern_atom(
        c: *mut xcb_connection_t,
        only_if_exists: u8,
        name_len: u16,
        name: *const c_char,
    ) -> cookie_t;
    fn xcb_intern_atom_reply(
        c: *mut xcb_connection_t,
        cookie: cookie_t,
        e: *mut *mut xcb_generic_error_t,
    ) -> *mut xcb_intern_atom_reply_t;
    fn xcb_get_property(
        c: *mut xcb_connection_t,
        delete: u8,
        window: u32,
        property: u32,
        type_: u32,
        long_offset: u32,
        long_length: u32,
    ) -> cookie_t;
    fn xcb_get_property_reply(
        c: *mut xcb_connection_t,
        cookie: cookie_t,
        e: *mut *mut xcb_generic_error_t,
    ) -> *mut xcb_get_property_reply_t;
    fn xcb_get_input_focus(c: *mut xcb_connection_t) -> cookie_t;
    fn xcb_get_input_focus_reply(
        c: *mut xcb_connection_t,
        cookie: cookie_t,
        e: *mut *mut xcb_generic_error_t,
    ) -> *mut xcb_get_input_focus_reply_t;
}

#[link(name = "xcb-dri3")]
extern "C" {
    static xcb_dri3_id: xcb_extension_t;
    fn xcb_dri3_query_version(c: *mut xcb_connection_t, major: u32, minor: u32) -> cookie_t;
    fn xcb_dri3_query_version_reply(
        c: *mut xcb_connection_t,
        cookie: cookie_t,
        e: *mut *mut xcb_generic_error_t,
    ) -> *mut xcb_dri3_query_version_reply_t;
    // DRI3 1.2's modifier-carrying import. `num_buffers` buffers are handed in
    // as fds with their own strides/offsets, plus the DRM format modifier; the
    // 1.0 singular `PixmapFromBuffer` (no modifier field) is its degenerate
    // single-plane LINEAR case, so this call supersedes it entirely.
    #[allow(clippy::too_many_arguments)]
    fn xcb_dri3_pixmap_from_buffers_checked(
        c: *mut xcb_connection_t,
        pixmap: u32,
        window: u32,
        num_buffers: u8,
        width: u16,
        height: u16,
        stride0: u32,
        offset0: u32,
        stride1: u32,
        offset1: u32,
        stride2: u32,
        offset2: u32,
        stride3: u32,
        offset3: u32,
        depth: u8,
        bpp: u8,
        modifier: u64,
        buffers: *const i32,
    ) -> cookie_t;
}

#[link(name = "xcb-present")]
extern "C" {
    static xcb_present_id: xcb_extension_t;
    fn xcb_present_query_version(c: *mut xcb_connection_t, major: u32, minor: u32) -> cookie_t;
    fn xcb_present_query_version_reply(
        c: *mut xcb_connection_t,
        cookie: cookie_t,
        e: *mut *mut xcb_generic_error_t,
    ) -> *mut xcb_present_query_version_reply_t;
    #[allow(clippy::too_many_arguments)]
    fn xcb_present_pixmap_checked(
        c: *mut xcb_connection_t,
        window: u32,
        pixmap: u32,
        serial: u32,
        valid: u32,
        update: u32,
        x_off: i16,
        y_off: i16,
        target_crtc: u32,
        wait_fence: u32,
        idle_fence: u32,
        options: u32,
        target_msc: u64,
        divisor: u64,
        remainder: u64,
        notifies_len: u32,
        notifies: *const c_void,
    ) -> cookie_t;
}

#[link(name = "xcb-sync")]
extern "C" {
    fn xcb_sync_create_fence_checked(
        c: *mut xcb_connection_t,
        drawable: u32,
        fence: u32,
        initially_triggered: u8,
    ) -> cookie_t;
    fn xcb_sync_reset_fence(c: *mut xcb_connection_t, fence: u32) -> cookie_t;
    fn xcb_sync_destroy_fence(c: *mut xcb_connection_t, fence: u32) -> cookie_t;
    fn xcb_sync_query_fence(c: *mut xcb_connection_t, fence: u32) -> cookie_t;
    fn xcb_sync_query_fence_reply(
        c: *mut xcb_connection_t,
        cookie: cookie_t,
        e: *mut *mut xcb_generic_error_t,
    ) -> *mut xcb_sync_query_fence_reply_t;
}

// ------------------------------------------------- the present target
//
// The window the compositor enumerates is the direct child of the X root that
// contains the window wine named. winex11.drv names its *client* window, which
// is nested under the WM-managed toplevel, so the present must be aimed at the
// root-child ancestor or it lands where the compositor never looks.

/// Bound on the ancestor walk: a window at this depth is already pathological,
/// and a bounded walk can never hang. Wine nests at most desktop -> whole ->
/// client (3 levels); 32 leaves room without being unbounded.
const MAX_ANCESTOR_DEPTH: u32 = 32;

/// Where a present must land: the root-child ancestor of the window wine
/// named, plus that window's offset within it (Present places the pixmap at
/// this offset, so a decorated or repositioned client window stays placed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PresentTarget {
    window: u32,
    x_off: i16,
    y_off: i16,
}

fn clamp_i16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Walks `window` up to its direct child of the X root, accumulating the
/// position of each level relative to its parent. `None` when any step's
/// window is gone, the walk would enter the root, or the depth bound is hit -
/// named by the caller, never a hang. Mirrors the compositor's own
/// `x11_find_toplevel_for_xid`.
unsafe fn resolve_target(conn: *mut xcb_connection_t, window: u32) -> Option<PresentTarget> {
    let mut cur = window;
    let mut ox: i32 = 0;
    let mut oy: i32 = 0;
    for _ in 0..MAX_ANCESTOR_DEPTH {
        let tree =
            unsafe { xcb_query_tree_reply(conn, xcb_query_tree(conn, cur), ptr::null_mut()) };
        if tree.is_null() {
            return None;
        }
        let (root, parent) = unsafe { ((*tree).root, (*tree).parent) };
        unsafe { libc::free(tree.cast()) };
        if cur == root {
            return None; // walked into the root: there is no containing toplevel
        }
        if parent == root {
            return Some(PresentTarget {
                window: cur,
                x_off: clamp_i16(ox),
                y_off: clamp_i16(oy),
            });
        }
        if parent == 0 || parent == cur {
            return None; // detached or self-parented: no toplevel to present into
        }
        // `cur` is an intermediate: add its position relative to its parent.
        let geom =
            unsafe { xcb_get_geometry_reply(conn, xcb_get_geometry(conn, cur), ptr::null_mut()) };
        if geom.is_null() {
            return None;
        }
        ox += i32::from(unsafe { (*geom).x });
        oy += i32::from(unsafe { (*geom).y });
        unsafe { libc::free(geom.cast()) };
        cur = parent;
    }
    None
}

// ------------------------------------- the override target (gamescope route)
//
// The override/Wayland present route (the default; proxy.rs
// `xlib_override_surface`, `GN_VKSHIM_GUEST_SURFACE` unset or not exactly "1")
// binds the shim's own
// wl_surface to an X window with gamescope's `override_window_content`. gamescope then runs its own
// `x11_find_toplevel_for_xid` on whatever XID it is handed, so handing it the
// window winex11 named lands the override on that window's root-child
// ancestor. For Dishonored that ancestor is a 1x1 override-redirect wine
// helper, while the window gamescope actually paints is a *different*,
// full-size root child of the same client: the override frame is committed
// correctly (the shim's `wlcommit ... buf=1152x864 route=x11`) but never
// composed. `resolve_override_target` fixes that by selecting the window
// gamescope will paint whenever the resolved ancestor is degenerate, and by
// returning the ancestor unchanged otherwise, so already-working targets
// (Dave the Diver, Nidhogg) bind the exact XID they bound before.

/// XCB atom ids that are fixed for every server (`XCB_ATOM_*`).
const XCB_ATOM_STRING: u32 = 31;
const XCB_ATOM_WM_CLASS: u32 = 67;
/// `XCB_MAP_STATE_VIEWABLE`.
const MAP_STATE_VIEWABLE: u8 = 2;
/// `XCB_WINDOW_CLASS_INPUT_OUTPUT`.
const WINDOW_CLASS_INPUT_OUTPUT: u16 = 1;

/// A window this small carries no frame - the shape wine leaves its nominated
/// window, and the 1x1 override-redirect helper ancestor, in.
fn is_degenerate(w: u16, h: u16) -> bool {
    w <= 1 || h <= 1
}

unsafe fn window_size(conn: *mut xcb_connection_t, w: u32) -> (u16, u16) {
    let geom = unsafe { xcb_get_geometry_reply(conn, xcb_get_geometry(conn, w), ptr::null_mut()) };
    if geom.is_null() {
        return (0, 0);
    }
    let d = unsafe { ((*geom).width, (*geom).height) };
    unsafe { libc::free(geom.cast()) };
    d
}

unsafe fn window_root(conn: *mut xcb_connection_t, w: u32) -> u32 {
    let tree = unsafe { xcb_query_tree_reply(conn, xcb_query_tree(conn, w), ptr::null_mut()) };
    if tree.is_null() {
        return 0;
    }
    let root = unsafe { (*tree).root };
    unsafe { libc::free(tree.cast()) };
    root
}

unsafe fn intern_atom(conn: *mut xcb_connection_t, name: &[u8]) -> u32 {
    let cookie =
        unsafe { xcb_intern_atom(conn, 1, name.len() as u16, name.as_ptr().cast::<c_char>()) };
    let reply = unsafe { xcb_intern_atom_reply(conn, cookie, ptr::null_mut()) };
    if reply.is_null() {
        return 0;
    }
    let a = unsafe { (*reply).atom };
    unsafe { libc::free(reply.cast()) };
    a
}

/// Reads the first 32-bit word of a window property (type-unfiltered, so it
/// reads both `CARDINAL` and `WINDOW` properties). 0 when absent.
unsafe fn window_u32_prop(conn: *mut xcb_connection_t, w: u32, prop: u32) -> u32 {
    if prop == 0 {
        return 0;
    }
    let cookie = unsafe { xcb_get_property(conn, 0, w, prop, 0, 0, 1) };
    let reply = unsafe { xcb_get_property_reply(conn, cookie, ptr::null_mut()) };
    if reply.is_null() {
        return 0;
    }
    let mut v = 0u32;
    let r = unsafe { &*reply };
    if r.format == 32 && r.value_len >= 1 {
        let p = unsafe { (reply as *const u8).add(32).cast::<u32>() };
        v = unsafe { ptr::read_unaligned(p) };
    }
    unsafe { libc::free(reply.cast()) };
    v
}

/// The `WM_CLASS` instance string (the first NUL-terminated field), copied
/// into a fixed buffer; all-zero when the property is absent.
unsafe fn window_class_instance(conn: *mut xcb_connection_t, w: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    let cookie = unsafe { xcb_get_property(conn, 0, w, XCB_ATOM_WM_CLASS, XCB_ATOM_STRING, 0, 8) };
    let reply = unsafe { xcb_get_property_reply(conn, cookie, ptr::null_mut()) };
    if reply.is_null() {
        return out;
    }
    let r = unsafe { &*reply };
    if r.format == 8 && r.value_len > 0 {
        let bytes = unsafe {
            std::slice::from_raw_parts((reply as *const u8).add(32), r.value_len as usize)
        };
        let n = bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(bytes.len())
            .min(31);
        out[..n].copy_from_slice(&bytes[..n]);
    }
    unsafe { libc::free(reply.cast()) };
    out
}

unsafe fn window_attr(conn: *mut xcb_connection_t, w: u32) -> Option<(u8, u16, u8)> {
    let attr = unsafe {
        xcb_get_window_attributes_reply(conn, xcb_get_window_attributes(conn, w), ptr::null_mut())
    };
    if attr.is_null() {
        return None;
    }
    let v = unsafe { ((*attr).map_state, (*attr).class_, (*attr).override_redirect) };
    unsafe { libc::free(attr.cast()) };
    Some(v)
}

unsafe fn input_focus(conn: *mut xcb_connection_t) -> u32 {
    let cookie = unsafe { xcb_get_input_focus(conn) };
    let reply = unsafe { xcb_get_input_focus_reply(conn, cookie, ptr::null_mut()) };
    if reply.is_null() {
        return 0;
    }
    let f = unsafe { (*reply).focus };
    unsafe { libc::free(reply.cast()) };
    f
}

/// The named window's client identity, read from the window itself or, when it
/// carries neither `_NET_WM_PID` nor `WM_CLASS` (wine sets them on managed
/// windows), the first ancestor that does. Returns (pid, class_instance,
/// class_len).
unsafe fn named_identity(
    conn: *mut xcb_connection_t,
    named: u32,
    ancestor: u32,
    pid_atom: u32,
) -> (u32, [u8; 32], usize) {
    let mut pid = 0u32;
    let mut cls = [0u8; 32];
    let mut cls_len = 0usize;
    let mut cur = named;
    for _ in 0..MAX_ANCESTOR_DEPTH {
        if pid == 0 {
            pid = unsafe { window_u32_prop(conn, cur, pid_atom) };
        }
        if cls_len == 0 {
            let c = unsafe { window_class_instance(conn, cur) };
            let n = c.iter().position(|&b| b == 0).unwrap_or(c.len());
            if n > 0 {
                cls = c;
                cls_len = n.min(31);
            }
        }
        if (pid != 0 && cls_len != 0) || cur == ancestor {
            break;
        }
        let tree =
            unsafe { xcb_query_tree_reply(conn, xcb_query_tree(conn, cur), ptr::null_mut()) };
        if tree.is_null() {
            break;
        }
        let parent = unsafe { (*tree).parent };
        unsafe { libc::free(tree.cast()) };
        if parent == 0 || parent == cur {
            break;
        }
        cur = parent;
    }
    (pid, cls, cls_len)
}

/// The window the content override must bind to (see the section comment).
///
/// Opens its own X connection; returns `named` unchanged when it cannot
/// connect, so a failed resolve degrades to the previous behaviour rather than
/// binding nothing.
pub unsafe fn resolve_override_target(named: u32) -> u32 {
    let conn = unsafe { xcb_connect(ptr::null(), ptr::null_mut()) };
    if conn.is_null() || unsafe { xcb_connection_has_error(conn) } != 0 {
        if !conn.is_null() {
            unsafe { xcb_disconnect(conn) };
        }
        return named;
    }
    let target = unsafe { resolve_override_target_on(conn, named) };
    unsafe { xcb_disconnect(conn) };
    target
}

/// The pixel size of an X window, or `None` when it cannot be read or is
/// degenerate (0 or 1 in either axis). Opens its own X connection, matching
/// [`resolve_override_target`].
///
/// This is the guest's own window size — under winex11 the window is the
/// container's desktop, so it is the size gamescope must see the presented
/// layer at (see the swapchain-extent note in `proxy`).
pub unsafe fn window_extent(window: u32) -> Option<(u32, u32)> {
    if window == 0 {
        return None;
    }
    let conn = unsafe { xcb_connect(ptr::null(), ptr::null_mut()) };
    if conn.is_null() || unsafe { xcb_connection_has_error(conn) } != 0 {
        if !conn.is_null() {
            unsafe { xcb_disconnect(conn) };
        }
        return None;
    }
    let (w, h) = unsafe { window_size(conn, window) };
    unsafe { xcb_disconnect(conn) };
    if is_degenerate(w, h) {
        None
    } else {
        Some((u32::from(w), u32::from(h)))
    }
}

/// The selection rule, on a caller-provided connection (host tests drive it).
///
///  1. Resolve the named window's root-child ancestor, as gamescope does.
///  2. Not degenerate (both extents > 1) -> return the ancestor, unchanged.
///  3. Degenerate -> select among the root's children the window gamescope
///     actually paints: viewable, InputOutput, non-degenerate, and of the same
///     client as the named window (same `_NET_WM_PID` as the named window's
///     chain or as this process, or the same `WM_CLASS` instance). The X input
///     focus / `_NET_ACTIVE_WINDOW` are accepted as that window when no
///     pid/class match is found; otherwise the largest match wins.
///  4. No candidate -> return the ancestor (the caller logs the empty set).
unsafe fn resolve_override_target_on(conn: *mut xcb_connection_t, named: u32) -> u32 {
    let Some(resolved) = (unsafe { resolve_target(conn, named) }) else {
        return named;
    };
    let ancestor = resolved.window;
    let (aw, ah) = unsafe { window_size(conn, ancestor) };
    if !is_degenerate(aw, ah) {
        crate::logf!(gndrm_obf::obf!(
            "override target: named=0x{named:x} ancestor=0x{ancestor:x} {aw}x{ah} -> 0x{ancestor:x} (legitimate; unchanged)"
        ));
        return ancestor;
    }

    let pid_atom = unsafe { intern_atom(conn, b"_NET_WM_PID") };
    let active_atom = unsafe { intern_atom(conn, b"_NET_ACTIVE_WINDOW") };
    let self_pid = unsafe { libc::getpid() } as u32;
    let (named_pid, named_cls, named_cls_len) =
        unsafe { named_identity(conn, named, ancestor, pid_atom) };

    let root = unsafe { window_root(conn, ancestor) };
    if root == 0 {
        return ancestor;
    }
    let active = unsafe { window_u32_prop(conn, root, active_atom) };
    let focus = unsafe { input_focus(conn) };

    let tree = unsafe { xcb_query_tree_reply(conn, xcb_query_tree(conn, root), ptr::null_mut()) };
    if tree.is_null() {
        return ancestor;
    }
    let n = unsafe { (*tree).children_len as usize };
    let kids = unsafe { (tree as *const u8).add(32).cast::<u32>() };

    let mut best: u32 = 0;
    let mut best_area: u64 = 0;
    let mut focus_viable = false;
    let mut active_viable = false;
    let mut report = String::new();
    let mut seen = 0u32;
    for i in 0..n {
        let c = unsafe { ptr::read_unaligned(kids.add(i)) };
        if c == ancestor {
            continue;
        }
        let Some((map_state, class_, _)) = (unsafe { window_attr(conn, c) }) else {
            continue;
        };
        if map_state != MAP_STATE_VIEWABLE || class_ != WINDOW_CLASS_INPUT_OUTPUT {
            continue;
        }
        let (w, h) = unsafe { window_size(conn, c) };
        if is_degenerate(w, h) {
            continue;
        }
        let cpid = unsafe { window_u32_prop(conn, c, pid_atom) };
        let ccls = unsafe { window_class_instance(conn, c) };
        let pid_ok = cpid != 0 && (cpid == named_pid || cpid == self_pid);
        let class_ok = named_cls_len != 0 && ccls[..named_cls_len] == named_cls[..named_cls_len];
        let matched = pid_ok || class_ok;
        if c == focus {
            focus_viable = true;
        }
        if c == active {
            active_viable = true;
        }
        seen += 1;
        if seen <= 12 {
            let cn = ccls.iter().position(|&b| b == 0).unwrap_or(ccls.len());
            let cstr = String::from_utf8_lossy(&ccls[..cn]);
            report.push_str(&format!(
                "{}",
                gndrm_obf::obf!(
                    " 0x{c:x} {w}x{h} pid={cpid} class=\"{cstr}\"{}{}{}",
                    if c == focus { " <focus>" } else { "" },
                    if c == active { " <active>" } else { "" },
                    if matched { " <match>" } else { "" },
                )
            ));
        }
        if matched {
            let area = u64::from(w) * u64::from(h);
            if area > best_area {
                best_area = area;
                best = c;
            }
        }
    }
    unsafe { libc::free(tree.cast()) };

    let (chosen, why) = if best != 0 {
        (best, "client-matched")
    } else if focus_viable && focus != ancestor {
        (focus, "input-focus")
    } else if active_viable && active != ancestor {
        (active, "active-window")
    } else {
        (ancestor, "no-candidate")
    };
    crate::logf!(gndrm_obf::obf!(
        "override target: named=0x{named:x} ancestor=0x{ancestor:x} {aw}x{ah} -> 0x{chosen:x} ({why}; self_pid={self_pid} named_pid={named_pid} named_class=\"{}\"; root_children:{report})",
        String::from_utf8_lossy(&named_cls[..named_cls_len])
    ));
    chosen
}

// ---------------------------------------------------------------- the route

/// Why a window has no X11 present route. Each variant names one check, so a
/// session that shows nothing says which one failed instead of only going
/// black; the caller prints it unconditionally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteUnavailable {
    /// `vkCreateXlibSurfaceKHR` was handed a null window id.
    NullXid,
    /// The guest's X connection could not be opened (`DISPLAY` unreachable).
    Connect,
    /// The X server has no DRI3 extension, so `PixmapFromBuffer` cannot work.
    NoDri3,
    /// The X server has no Present extension.
    NoPresent,
    /// The window answers no geometry query (stale or foreign XID).
    Geometry,
}

impl RouteUnavailable {
    pub fn reason(self) -> &'static str {
        match self {
            RouteUnavailable::NullXid => "null XID",
            RouteUnavailable::Connect => "guest X server unreachable (DISPLAY)",
            RouteUnavailable::NoDri3 => "X server has no DRI3",
            RouteUnavailable::NoPresent => "X server has no Present",
            RouteUnavailable::Geometry => "window has no geometry (stale or foreign XID)",
        }
    }
}

/// A live X11 present target: the root-child window the compositor enumerates,
/// and the extensions it needs.
pub struct X11Present {
    /// Serializes every request/reply exchange on `conn`. The present worker
    /// and the app's own present path can both reach one `X11Present`, and an
    /// xcb connection is not safe for concurrent requests: interleaved
    /// requests misalign `xcb_request_check`'s reply consumption.
    lock: Mutex<()>,
    conn: *mut xcb_connection_t,
    /// The present target: the direct child of the X root that contains the
    /// window wine named (see `resolve_target`). Equal to the named window when
    /// that window is already a root child.
    window: u32,
    /// The target window's `WM_CLASS` instance, read once at bring-up: the
    /// desktop reference [`X11Present::window_present`] compares the bound
    /// window against. Under `desktop=shell` the target is the wine desktop
    /// window and this is its class (wine's `explorer.exe`), which the game's
    /// own window differs from while wine's shell/taskbar children share it.
    desktop_class: [u8; 32],
    desktop_class_len: usize,
    /// The named window's offset within `window`; `Present` places the pixmap
    /// here, so a decorated or repositioned client window stays placed.
    x_off: i16,
    y_off: i16,
    depth: u8,
    dri3_version: (u32, u32),
    present_version: (u32, u32),
    next_serial: u32,
}

/// Reads the server error (or null) of a checked void request and frees it,
/// returning the error code when the request failed.
unsafe fn check_err(c: *mut xcb_connection_t, cookie: cookie_t) -> Option<u8> {
    let e = unsafe { xcb_request_check(c, cookie) };
    if e.is_null() {
        return None;
    }
    let code = unsafe { (*e).error_code };
    unsafe { libc::free(e.cast()) };
    Some(code)
}

/// The child half of the game-window predicate: whether `bound` has a child
/// that is a real window of its own — viewable, `InputOutput`, non-degenerate,
/// and carrying a `WM_CLASS` instance that differs from `bound`'s own
/// (`own_class`).
///
/// Shared by both routes through [`window_present`]: on the override/Wayland
/// route (the default) the bound window is the wine desktop window and this is
/// the whole signal, while on the guest-surface route (the opt-in) it covers the
/// case where the bound window is a container too.
///
/// A class read that fails (an empty class) is treated as **not** a match:
/// wine's own shell and taskbar share the desktop's class, but an unreadable
/// class must never let one of them clear the splash over an empty desktop.
unsafe fn game_child_present(conn: *mut xcb_connection_t, bound: u32, own_class: &[u8]) -> bool {
    if own_class.is_empty() {
        return false;
    }
    let tree = unsafe { xcb_query_tree_reply(conn, xcb_query_tree(conn, bound), ptr::null_mut()) };
    if tree.is_null() {
        return false;
    }
    let n = unsafe { (*tree).children_len as usize };
    let kids = unsafe { (tree as *const u8).add(32).cast::<u32>() };
    let mut found = false;
    for i in 0..n {
        let c = unsafe { ptr::read_unaligned(kids.add(i)) };
        let Some((map_state, class_, _)) = (unsafe { window_attr(conn, c) }) else {
            continue;
        };
        if map_state != MAP_STATE_VIEWABLE || class_ != WINDOW_CLASS_INPUT_OUTPUT {
            continue;
        }
        let (w, h) = unsafe { window_size(conn, c) };
        if is_degenerate(w, h) {
            continue;
        }
        let ccls = unsafe { window_class_instance(conn, c) };
        let clen = ccls.iter().position(|&b| b == 0).unwrap_or(ccls.len());
        // An unreadable class is unknown, never a match.
        if clen == 0 {
            continue;
        }
        if ccls[..clen] != own_class[..] {
            found = true;
            break;
        }
    }
    unsafe { libc::free(tree.cast()) };
    found
}

/// The nearest ancestor-or-self of `bound` that carries a readable `WM_CLASS`
/// instance, with that instance (the bound window itself and length 0 when the
/// walk finds none).
///
/// wine hands the guest-surface route its *client* window, which carries no
/// `WM_CLASS` at all: the class sits on the frame one level up (`hades2.exe`),
/// one level above that the wine desktop window (`explorer.exe`, the root
/// child). So the predicate resolves the class through the ancestor chain
/// rather than insisting the bound window itself be classed.
///
/// The walk climbs at most to the bound window's root child — under wine's
/// `/desktop=` that is the desktop window — and stops there, so it can never
/// cross into the desktop's siblings or the root. A window with no readable
/// class is simply skipped; an unreadable class is "unknown", never a match.
unsafe fn nearest_classed_ancestor(
    conn: *mut xcb_connection_t,
    bound: u32,
) -> (u32, [u8; 32], usize) {
    let mut cur = bound;
    for _ in 0..MAX_ANCESTOR_DEPTH {
        let cls = unsafe { window_class_instance(conn, cur) };
        let clen = cls.iter().position(|&b| b == 0).unwrap_or(cls.len());
        if clen > 0 {
            return (cur, cls, clen);
        }
        let tree =
            unsafe { xcb_query_tree_reply(conn, xcb_query_tree(conn, cur), ptr::null_mut()) };
        if tree.is_null() {
            break;
        }
        let (root, parent) = unsafe { ((*tree).root, (*tree).parent) };
        unsafe { libc::free(tree.cast()) };
        // The root child — the wine desktop window — is the boundary: read it,
        // but never walk past it (into the root or a sibling subtree).
        if cur == root || parent == root || parent == 0 || parent == cur {
            break;
        }
        cur = parent;
    }
    (bound, [0u8; 32], 0)
}

/// Whether the window a present route is **bound to** is the game's own window —
/// one predicate for both route shapes.
///
/// The bound window is `bound`, and the desktop window's `WM_CLASS` instance is
/// `desktop_class` (an empty slice when it could not be read). The window shapes
/// the two routes bind are different, which is why the rule resolves the class
/// through the ancestor chain and then has two arms:
///  - guest-surface (the opt-in): the bound window is the guest's *own* window,
///    which wine leaves **classless** (its `WM_CLASS` sits on the frame one
///    level up), so the *nearest classed ancestor-or-self* is the signal.
///  - override/Wayland (the DEFAULT): the bound window is the wine *desktop* window,
///    whose own class *is* the desktop's, and the game appears as a differing
///    class child of it — nothing about the bound window itself says "game". The
///    degenerate-ancestor case (Dishonored, with A29 parked) binds a classless
///    window that is not the desktop and is decided here too, by the same walk.
///
/// So the bound window counts as present when the **nearest classed
/// ancestor-or-self's `WM_CLASS` instance differs from the desktop's**, or when
/// it has a **non-degenerate, viewable child whose class differs** (see
/// [`game_child_present`]); a classless bound window is skipped by the walk, not
/// treated as absent.
///
/// An unreadable class is never a match, on either side of either comparison:
/// wine's own shell and taskbar carry the desktop's class, but an unknown class
/// must never let one of them clear the splash over an empty desktop.
unsafe fn window_present(conn: *mut xcb_connection_t, bound: u32, desktop_class: &[u8]) -> bool {
    // The nearest classed ancestor-or-self (the bound window itself when it is
    // classed). An unreadable class is unknown: the walk skips it, so it never
    // makes the first comparison pass, and it never short-circuits the second
    // arm either — `own` stays empty and the child scan simply declines.
    let (ancestor, cls, clen) = unsafe { nearest_classed_ancestor(conn, bound) };
    let own = &cls[..clen];
    let present = if !own.is_empty() && !desktop_class.is_empty() && own != desktop_class {
        true
    } else {
        unsafe { game_child_present(conn, bound, own) }
    };
    unsafe { log_predicate_once(conn, bound, ancestor, own, desktop_class, present) };
    present
}

/// One-shot, ungated line naming how [`window_present`] resolved the bound
/// window: its own `WM_CLASS` length, the nearest classed ancestor-or-self the
/// walk found and that class, the desktop reference, and the verdict.
///
/// The splash-clear predicate failed on the guest-surface route because the
/// bound window carries no `WM_CLASS` and an early guard returned before either
/// arm ran; nothing in logcat said so, and the investigation needed an external
/// X probe to see it. This fires once per session (the swap happens before any
/// read or format), off the per-frame path — [`window_present`] runs at the
/// tree-walk cadence, never per frame — and is deliberately **not** behind
/// `GNDRM_LOG_VKSHIM`, so a real session shows it without a knob.
unsafe fn log_predicate_once(
    conn: *mut xcb_connection_t,
    bound: u32,
    ancestor: u32,
    own: &[u8],
    desktop_class: &[u8],
    present: bool,
) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    let bound_cls = unsafe { window_class_instance(conn, bound) };
    let bound_len = bound_cls.iter().position(|&b| b == 0).unwrap_or(0);
    eprintln!(
        "{} {}",
        gndrm_obf::obf!("gn-vkshim:"),
        gndrm_obf::obf!(
            "x11 game-window predicate: bound=0x{:x} bound_class_len={} nearest_classed=0x{:x} class={} desktop_class={} present={}",
            bound,
            bound_len,
            ancestor,
            String::from_utf8_lossy(own),
            String::from_utf8_lossy(desktop_class),
            present
        )
    );
}

/// Whether the window the override/Wayland route is bound to is the game's own
/// window (see [`window_present`]).
///
/// That route (the default; `proxy::xlib_override_surface`,
/// `GN_VKSHIM_GUEST_SURFACE` unset or not exactly "1") holds no persistent X
/// connection, so — like [`resolve_override_target`] and
/// [`window_extent`] — the walk opens its own connection, reads the bound
/// window's `WM_CLASS`, runs [`window_present`] and closes it. On this route the
/// bound window is the wine desktop window, so its own class *is* the desktop's
/// and the class comparison falls through to the child scan; the same child scan
/// is what decides the degenerate-ancestor case, where the bound window is not
/// the desktop. The connection is live for the walk; nothing has to be kept
/// between walks.
///
/// This is an X round trip, so the caller runs it at the tree-walk cadence
/// (every *N* presents, until the game is first reported), never per frame. The
/// transition it detects is one-shot, so tens of milliseconds of latency — and a
/// fresh connection per walk, which stops entirely once reported — cost nothing.
pub unsafe fn game_window_present(window: u32) -> bool {
    if window == 0 {
        return false;
    }
    let conn = unsafe { xcb_connect(ptr::null(), ptr::null_mut()) };
    if conn.is_null() || unsafe { xcb_connection_has_error(conn) } != 0 {
        if !conn.is_null() {
            unsafe { xcb_disconnect(conn) };
        }
        return false;
    }
    let cls = unsafe { window_class_instance(conn, window) };
    let clen = cls.iter().position(|&b| b == 0).unwrap_or(cls.len());
    let present = unsafe { window_present(conn, window, &cls[..clen]) };
    unsafe { xcb_disconnect(conn) };
    present
}

impl X11Present {
    /// Opens the guest's X connection and validates the window + DRI3/Present.
    /// `Err` with a named reason when the route cannot work; the caller then
    /// drops frames (never blocks) until the surface goes away.
    pub unsafe fn new(window: u32) -> Result<X11Present, RouteUnavailable> {
        if window == 0 {
            return Err(RouteUnavailable::NullXid);
        }
        let conn = unsafe { xcb_connect(ptr::null(), ptr::null_mut()) };
        if conn.is_null() || unsafe { xcb_connection_has_error(conn) } != 0 {
            if !conn.is_null() {
                unsafe { xcb_disconnect(conn) };
            }
            return Err(RouteUnavailable::Connect);
        }
        // Extension presence is read from the setup's extension list; the
        // geometry query is the first round trip and also proves the window
        // exists (a stale XID answers a BadWindow error).
        let dri3 = unsafe { xcb_get_extension_data(conn, &xcb_dri3_id) };
        let present = unsafe { xcb_get_extension_data(conn, &xcb_present_id) };
        if dri3.is_null() || unsafe { (*dri3).present } == 0 {
            unsafe { xcb_disconnect(conn) };
            return Err(RouteUnavailable::NoDri3);
        }
        if present.is_null() || unsafe { (*present).present } == 0 {
            unsafe { xcb_disconnect(conn) };
            return Err(RouteUnavailable::NoPresent);
        }
        // The compositor enumerates only direct children of the X root, so aim
        // the present at the root-child ancestor of the window wine named. A
        // failed walk (stale XID, depth bound) keeps the named window, whose
        // own geometry query then names the failure below.
        let target = unsafe { resolve_target(conn, window) };
        let (target_window, x_off, y_off) = match target {
            Some(t) => (t.window, t.x_off, t.y_off),
            None => (window, 0, 0),
        };
        // The geometry query is the first round trip and also proves the
        // target exists (a stale XID answers a BadWindow error).
        let geom = unsafe { xcb_get_geometry(conn, target_window) };
        let geom = unsafe { xcb_get_geometry_reply(conn, geom, ptr::null_mut()) };
        if geom.is_null() {
            unsafe { xcb_disconnect(conn) };
            return Err(RouteUnavailable::Geometry);
        }
        let depth = unsafe { (*geom).depth };
        unsafe { libc::free(geom.cast()) };

        let dv = unsafe {
            xcb_dri3_query_version_reply(conn, xcb_dri3_query_version(conn, 1, 2), ptr::null_mut())
        };
        let dri3_version = if dv.is_null() {
            (1, 0)
        } else {
            let v = (unsafe { (*dv).major_version }, unsafe {
                (*dv).minor_version
            });
            unsafe { libc::free(dv.cast()) };
            v
        };
        let pv = unsafe {
            xcb_present_query_version_reply(
                conn,
                xcb_present_query_version(conn, 1, 2),
                ptr::null_mut(),
            )
        };
        let present_version = if pv.is_null() {
            (1, 0)
        } else {
            let v = (unsafe { (*pv).major_version }, unsafe {
                (*pv).minor_version
            });
            unsafe { libc::free(pv.cast()) };
            v
        };
        // The desktop window's own `WM_CLASS`, read once: the game's window is
        // told apart from wine's shell/taskbar by carrying a different one (see
        // `window_present`). Length 0 means the class could not be read, which
        // is treated as "unknown", never a match.
        let desktop_class = unsafe { window_class_instance(conn, target_window) };
        let desktop_class_len = desktop_class
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(desktop_class.len());
        let present = X11Present {
            lock: Mutex::new(()),
            conn,
            window: target_window,
            desktop_class,
            desktop_class_len,
            x_off,
            y_off,
            depth,
            dri3_version,
            present_version,
            next_serial: 1,
        };
        unsafe { present.log_chain(window) };
        Ok(present)
    }

    /// Serializes one exchange on the shared X connection.
    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap()
    }

    pub fn window(&self) -> u32 {
        self.window
    }

    /// Whether the window the guest-surface route is bound to is the game's own
    /// window, on this present's own live connection (see [`window_present`] for
    /// the rule).
    ///
    /// `bound` is the window the route bound — `GnSurface::window`, i.e. the
    /// guest's own X window on this route — and the desktop reference is this
    /// present's target window's class, read once at bring-up. Under
    /// `desktop=shell` the tree is depth 3 — root -> the wine desktop window ->
    /// the game's whole_window (override-redirect) -> the named client window
    /// the shim was handed — and that named client window is **classless**, so
    /// the predicate resolves the class from its nearest classed ancestor (the
    /// game's frame) before comparing; a bound window that is a container
    /// instead falls through to the child scan.
    ///
    /// This is an X round trip, so the caller runs it at the tree-walk cadence
    /// (every *N* presents, until it first reports), never per frame. The
    /// transition it detects is one-shot, so tens of milliseconds of latency
    /// cost nothing.
    pub unsafe fn window_present(&self, bound: u32) -> bool {
        if bound == 0 {
            return false;
        }
        let _g = self.lock();
        unsafe {
            window_present(
                self.conn,
                bound,
                &self.desktop_class[..self.desktop_class_len],
            )
        }
    }

    /// One-shot dump of the named window, its parent chain up to the root, and
    /// the resolved present target, printed unconditionally like the route
    /// line.
    ///
    /// gamescope's steamcompmgr only tracks X windows that are direct children
    /// of the X root, so this is the evidence that says whether the compositor
    /// can ever see the window the DRI3 presents land in: a presenting window
    /// whose top-level ancestor is not a root child is invisible no matter how
    /// well the present itself works. `map_state` 0/1/2 is
    /// IsUnmapped/IsUnviewable/IsViewable, and IsUnviewable means an ancestor
    /// is unmapped - X will not display a child of an unmapped window. The
    /// final line states the target the present actually uses, so a session
    /// that presents into a window the compositor does not enumerate says so
    /// instead of only going black.
    unsafe fn log_chain(&self, origin: u32) {
        let mut w = origin;
        for level in 0..16u32 {
            let attr = unsafe {
                xcb_get_window_attributes_reply(
                    self.conn,
                    xcb_get_window_attributes(self.conn, w),
                    ptr::null_mut(),
                )
            };
            if attr.is_null() {
                crate::logf!(gndrm_obf::obf!(
                    "x11 window: 0x{w:x} attributes unavailable at level {level}"
                ));
                return;
            }
            let (map, or, class) =
                unsafe { ((*attr).map_state, (*attr).override_redirect, (*attr).class_) };
            unsafe { libc::free(attr.cast()) };
            let geom = unsafe {
                xcb_get_geometry_reply(self.conn, xcb_get_geometry(self.conn, w), ptr::null_mut())
            };
            let (gw, gh) = if geom.is_null() {
                (0u16, 0u16)
            } else {
                let d = unsafe { ((*geom).width, (*geom).height) };
                unsafe { libc::free(geom.cast()) };
                d
            };
            let ms = match map {
                0 => "IsUnmapped",
                1 => "IsUnviewable",
                _ => "IsViewable",
            };
            crate::logf!(gndrm_obf::obf!(
                "x11 window: level={level} id=0x{w:x} map={ms} override={or} class={class} {gw}x{gh}"
            ));
            let tree = unsafe {
                xcb_query_tree_reply(self.conn, xcb_query_tree(self.conn, w), ptr::null_mut())
            };
            if tree.is_null() {
                return;
            }
            let parent = unsafe { (*tree).parent };
            unsafe { libc::free(tree.cast()) };
            if parent == 0 || parent == w {
                break;
            }
            w = parent;
        }
        // The outcome the presenter depends on: the root child the present
        // lands in. Named once, unconditionally.
        if self.window == origin {
            crate::logf!(gndrm_obf::obf!(
                "x11 present target: 0x{:x} (named window is already a root child)",
                self.window
            ));
        } else {
            crate::logf!(gndrm_obf::obf!(
                "x11 present target: 0x{:x} offset=({},{}) (root child of named window 0x{:x})",
                self.window,
                self.x_off,
                self.y_off,
                origin
            ));
        }
    }

    /// Imports a slot's dma-buf as an X pixmap. `None` (with the server
    /// error when it has one) keeps the frame off screen instead of presenting
    /// a wrong one.
    ///
    /// Uses DRI3 1.2's `PixmapFromBuffers` (the modifier-carrying plural form)
    /// with a single buffer, so a tiled/OPTIMAL slot - any DRM format modifier -
    /// can be expressed, not only LINEAR. The DRI3 spec calls a one-buffer
    /// `PixmapFromBuffers` "akin to `PixmapFromBuffer`"; for the current
    /// single-plane LINEAR slot the same buffer, stride, offset (0), depth and
    /// bpp describe the same genuinely-linear image, so the server resolves the
    /// identical pixmap (device: 48/48 presents land with no import failure).
    ///
    /// libxcb's DRI3 import takes ownership of the fd and closes it
    /// once the request is sent (verified against libxcb: the fd answers EBADF
    /// immediately after the checked request). The slot owns its fd for the
    /// swapchain's whole life, so a dup is handed over instead - otherwise the
    /// slot's fd is dead for every later user of it.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn make_pixmap(
        &self,
        fd: i32,
        width: u32,
        height: u32,
        stride: u32,
        modifier: u64,
    ) -> Option<u32> {
        if fd < 0 {
            return None;
        }
        let _g = self.lock();
        // The dup is what libxcb closes; the slot keeps `fd`.
        let import_fd = unsafe { libc::dup(fd) };
        if import_fd < 0 {
            crate::logf!(gndrm_obf::obf!(
                "x11: dup of slot fd failed: {} - frame dropped",
                std::io::Error::last_os_error()
            ));
            return None;
        }
        let pixmap = unsafe { xcb_generate_id(self.conn) };
        // One plane: the plane-0 fd carries the modifier, planes 1..3 are the
        // request's unused slots (num_buffers=1 leaves them ignored).
        let buffers: [i32; 1] = [import_fd];
        let cookie = unsafe {
            xcb_dri3_pixmap_from_buffers_checked(
                self.conn,
                pixmap,
                self.window,
                1,
                width as u16,
                height as u16,
                stride,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                self.depth,
                32,
                modifier,
                buffers.as_ptr(),
            )
        };
        match unsafe { check_err(self.conn, cookie) } {
            None => Some(pixmap),
            Some(code) => {
                crate::logf!(gndrm_obf::obf!(
                    "x11: {width}x{height} stride={stride} modifier={modifier:#x} pixmap_from_buffers failed (X error {code}) - frame dropped"
                ));
                None
            }
        }
    }

    /// Releases our reference to a slot's pixmap. The server keeps it until its
    /// own reference (the pending present) is gone.
    pub unsafe fn destroy_pixmap(&self, pixmap: u32) {
        if pixmap != 0 {
            let _g = self.lock();
            unsafe { xcb_free_pixmap(self.conn, pixmap) };
        }
    }

    /// Creates the idle fence that reports when a pixmap is free again.
    pub unsafe fn make_fence(&self) -> Option<u32> {
        let _g = self.lock();
        let fence = unsafe { xcb_generate_id(self.conn) };
        let cookie = unsafe { xcb_sync_create_fence_checked(self.conn, self.window, fence, 0) };
        match unsafe { check_err(self.conn, cookie) } {
            None => Some(fence),
            Some(code) => {
                crate::logf!(gndrm_obf::obf!(
                    "x11: sync fence create failed (X error {code}) - frame dropped"
                ));
                None
            }
        }
    }

    pub unsafe fn destroy_fence(&self, fence: u32) {
        if fence != 0 {
            let _g = self.lock();
            unsafe { xcb_sync_destroy_fence(self.conn, fence) };
        }
    }

    /// Makes `pixmap` the target window's content at the named window's offset,
    /// with `fence` reporting the release. The fence is reset first so the
    /// server's trigger is observable. Returns the present serial, or `None`
    /// when the server refused the request.
    pub unsafe fn present(&mut self, pixmap: u32, fence: u32) -> Option<u32> {
        if pixmap == 0 || fence == 0 {
            return None;
        }
        // The serial is the route's own bookkeeping, not part of the exchange,
        // and it is read before the lock so the guard never aliases this
        // mutable field.
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        let _g = self.lock();
        unsafe {
            xcb_sync_reset_fence(self.conn, fence);
        }
        let cookie = unsafe {
            xcb_present_pixmap_checked(
                self.conn,
                self.window,
                pixmap,
                serial,
                0,
                0,
                self.x_off,
                self.y_off,
                0,
                0,
                fence,
                0,
                0,
                0,
                0,
                0,
                ptr::null(),
            )
        };
        if let Some(code) = unsafe { check_err(self.conn, cookie) } {
            crate::logf!(gndrm_obf::obf!(
                "x11: present of pixmap {pixmap} failed (X error {code}) - frame dropped"
            ));
            return None;
        }
        Some(serial)
    }

    /// Whether the idle fence has triggered - i.e. the server is done with the
    /// pixmap and its slot is reusable. A checked local round trip whose reply
    /// is always consumed, so replies never misalign or accumulate.
    pub unsafe fn fence_idle(&self, fence: u32) -> bool {
        if fence == 0 {
            return true;
        }
        let _g = self.lock();
        let reply = unsafe {
            xcb_sync_query_fence_reply(
                self.conn,
                xcb_sync_query_fence(self.conn, fence),
                ptr::null_mut(),
            )
        };
        if reply.is_null() {
            return false;
        }
        let triggered = unsafe { (*reply).triggered } != 0;
        unsafe { libc::free(reply.cast()) };
        triggered
    }
}

impl Drop for X11Present {
    fn drop(&mut self) {
        if !self.conn.is_null() {
            unsafe {
                xcb_flush(self.conn);
                xcb_disconnect(self.conn);
            }
            self.conn = ptr::null_mut();
        }
    }
}

/// The extension versions the route negotiated, for the route line the caller
/// prints when the route comes up.
impl X11Present {
    pub fn dri3_version(&self) -> (u32, u32) {
        self.dri3_version
    }

    pub fn present_version(&self) -> (u32, u32) {
        self.present_version
    }
}

#[cfg(test)]
mod tests {
    //! Host coverage for the ancestor walk. Needs a reachable X server
    //! (`DISPLAY`); it skips silently when there is none, like the shell
    //! checks, so `cargo test` stays green on a headless host.

    use super::*;

    #[link(name = "xcb")]
    extern "C" {
        fn xcb_get_setup(c: *mut xcb_connection_t) -> *const c_void;
        fn xcb_create_window(
            c: *mut xcb_connection_t,
            depth: u8,
            wid: u32,
            parent: u32,
            x: i16,
            y: i16,
            width: u16,
            height: u16,
            border_width: u16,
            class_: u16,
            visual: u32,
            value_mask: u32,
            value_list: *const c_void,
        ) -> cookie_t;
        fn xcb_destroy_window(c: *mut xcb_connection_t, window: u32) -> cookie_t;
        fn xcb_map_window(c: *mut xcb_connection_t, window: u32) -> cookie_t;
        fn xcb_change_property(
            c: *mut xcb_connection_t,
            mode: u8,
            window: u32,
            property: u32,
            type_: u32,
            format: u8,
            data_len: u32,
            data: *const c_void,
        ) -> cookie_t;
    }
    const XCB_ATOM_CARDINAL_T: u32 = 6;
    const PROP_MODE_REPLACE: u8 = 0;
    /// `CWOverrideRedirect`: keep the host WM from reparenting the test windows
    /// (a reparented window is no longer a root child, which would defeat the
    /// enumeration the test exercises).
    const CW_OVERRIDE_REDIRECT: u32 = 0x0200;

    // Layout of xcb's setup reply: a fixed 40-byte header, then the vendor
    // string (padded to 4 bytes), then the pixmap formats (8 bytes each), then
    // the screen array. Only the default screen's root window id and root
    // depth are needed. This mirrors what xcb's `xcb_setup_roots_iterator`
    // walks; getting the skip wrong reads the vendor string as a screen.
    const SETUP_HEADER_LEN: usize = 40;
    const SETUP_VENDOR_LEN_OFF: usize = 24; // uint16_t vendor_len
    const SETUP_PIXMAP_FORMATS_LEN_OFF: usize = 29; // uint8_t pixmap_formats_len
    const PIXMAP_FORMAT_LEN: usize = 8;
    const SCREEN_ROOT_OFF: usize = 0;
    const SCREEN_ROOT_DEPTH_OFF: usize = 38;
    const WINDOW_CLASS_INPUT_OUTPUT: u16 = 1; // XCB_WINDOW_CLASS_INPUT_OUTPUT
    const COPY_FROM_PARENT: u32 = 0;

    /// The default screen's root window and depth, read out of the setup reply.
    unsafe fn host_root(conn: *mut xcb_connection_t) -> Option<(u32, u8)> {
        let setup = unsafe { xcb_get_setup(conn) };
        if setup.is_null() {
            return None;
        }
        let header = setup.cast::<u8>();
        let vendor_len = unsafe {
            std::ptr::read_unaligned(header.add(SETUP_VENDOR_LEN_OFF).cast::<u16>()) as usize
        };
        let forms_len = unsafe { *header.add(SETUP_PIXMAP_FORMATS_LEN_OFF) as usize };
        let screen = unsafe {
            header.add(SETUP_HEADER_LEN + ((vendor_len + 3) & !3) + forms_len * PIXMAP_FORMAT_LEN)
        };
        let root = unsafe { std::ptr::read_unaligned(screen.add(SCREEN_ROOT_OFF).cast::<u32>()) };
        let depth = unsafe { *screen.add(SCREEN_ROOT_DEPTH_OFF) };
        (root != 0).then_some((root, depth))
    }

    #[test]
    fn walk_reaches_the_root_child_and_carries_the_offset() {
        let conn = unsafe { xcb_connect(ptr::null(), ptr::null_mut()) };
        if conn.is_null() || unsafe { xcb_connection_has_error(conn) } != 0 {
            eprintln!(
                "{}",
                gndrm_obf::obf!("skip: no X server (DISPLAY unreachable)")
            );
            return;
        }
        let screen = unsafe { host_root(conn) };
        let Some((root, depth)) = screen else {
            eprintln!(
                "{}",
                gndrm_obf::obf!("skip: no X server (no screen in setup)")
            );
            unsafe { xcb_disconnect(conn) };
            return;
        };

        // A root-child toplevel and a client window nested under it at a known
        // offset - the shape winex11.drv produces. Neither is mapped:
        // query_tree/get_geometry answer for unmapped windows, and the host
        // session must not be disturbed.
        let top = unsafe { xcb_generate_id(conn) };
        unsafe {
            xcb_create_window(
                conn,
                depth,
                top,
                root,
                0,
                0,
                64,
                48,
                0,
                WINDOW_CLASS_INPUT_OUTPUT,
                COPY_FROM_PARENT,
                0,
                ptr::null(),
            )
        };
        let client = unsafe { xcb_generate_id(conn) };
        unsafe {
            xcb_create_window(
                conn,
                depth,
                client,
                top,
                7,
                11,
                32,
                24,
                0,
                WINDOW_CLASS_INPUT_OUTPUT,
                COPY_FROM_PARENT,
                0,
                ptr::null(),
            )
        };

        assert_eq!(
            unsafe { resolve_target(conn, client) },
            Some(PresentTarget {
                window: top,
                x_off: 7,
                y_off: 11
            }),
            "a nested client window resolves to its root-child ancestor with its offset"
        );
        eprintln!("{}", gndrm_obf::obf!(
            "DEBUG root={root:#x} depth={depth} top={top:#x} client={client:#x} from_client={:?} from_top={:?}",
            unsafe { resolve_target(conn, client) },
            unsafe { resolve_target(conn, top) }
        ));
        assert_eq!(
            unsafe { resolve_target(conn, top) },
            Some(PresentTarget {
                window: top,
                x_off: 0,
                y_off: 0
            }),
            "a root child resolves to itself at offset zero"
        );
        assert_eq!(
            unsafe { resolve_target(conn, root) },
            None,
            "the root has no containing toplevel"
        );

        unsafe {
            xcb_destroy_window(conn, client);
            xcb_destroy_window(conn, top);
            xcb_flush(conn);
            xcb_disconnect(conn);
        }
    }

    /// The content-override selection rule. A degenerate (1x1) ancestor must be
    /// swapped for the full-size viewable root child of the same client, and a
    /// legitimate ancestor must be returned untouched. Host/DISPLAY-gated.
    #[test]
    fn override_target_swaps_a_degenerate_ancestor_for_the_painted_window() {
        let conn = unsafe { xcb_connect(ptr::null(), ptr::null_mut()) };
        if conn.is_null() || unsafe { xcb_connection_has_error(conn) } != 0 {
            eprintln!(
                "{}",
                gndrm_obf::obf!("skip: no X server (DISPLAY unreachable)")
            );
            return;
        }
        let screen = unsafe { host_root(conn) };
        let Some((root, depth)) = screen else {
            eprintln!(
                "{}",
                gndrm_obf::obf!("skip: no X server (no screen in setup)")
            );
            unsafe { xcb_disconnect(conn) };
            return;
        };

        // A 1x1 root-child helper with a 1x1 named client under it: the shape
        // Dishonored's nominated window resolves to.
        let helper = unsafe { xcb_generate_id(conn) };
        let named = unsafe { xcb_generate_id(conn) };
        // The window gamescope would paint: a full-size root child of the same
        // client (same _NET_WM_PID).
        let game = unsafe { xcb_generate_id(conn) };
        let mk = |w: u32, parent: u32, width: u16, height: u16| unsafe {
            xcb_create_window(
                conn,
                depth,
                w,
                parent,
                0,
                0,
                width,
                height,
                0,
                WINDOW_CLASS_INPUT_OUTPUT,
                COPY_FROM_PARENT,
                0,
                ptr::null(),
            )
        };
        // override-redirect so the host WM does not reparent the root children.
        let or_val: u32 = 1;
        unsafe {
            xcb_create_window(
                conn,
                depth,
                helper,
                root,
                0,
                0,
                1,
                1,
                0,
                WINDOW_CLASS_INPUT_OUTPUT,
                COPY_FROM_PARENT,
                CW_OVERRIDE_REDIRECT,
                (&raw const or_val).cast(),
            )
        };
        mk(named, helper, 1, 1);
        unsafe {
            xcb_create_window(
                conn,
                depth,
                game,
                root,
                0,
                0,
                640,
                480,
                0,
                WINDOW_CLASS_INPUT_OUTPUT,
                COPY_FROM_PARENT,
                CW_OVERRIDE_REDIRECT,
                (&raw const or_val).cast(),
            )
        };
        // Identity: give both the helper (which the named window inherits) and
        // the painted window this process's pid.
        let pid_atom = unsafe { intern_atom(conn, b"_NET_WM_PID") };
        let my_pid = unsafe { libc::getpid() } as u32;
        for w in [helper, game] {
            unsafe {
                xcb_change_property(
                    conn,
                    PROP_MODE_REPLACE,
                    w,
                    pid_atom,
                    XCB_ATOM_CARDINAL_T,
                    32,
                    1,
                    (&raw const my_pid).cast(),
                )
            };
        }
        unsafe {
            xcb_map_window(conn, helper);
            xcb_map_window(conn, game);
            xcb_flush(conn);
        }
        // Let the server process the maps; if the WM interfered, skip.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let game_attr = unsafe { window_attr(conn, game) };
        if game_attr.map(|(m, _, _)| m) != Some(MAP_STATE_VIEWABLE) {
            eprintln!(
                "{}",
                gndrm_obf::obf!("skip: test window not viewable (host WM interference)")
            );
            unsafe {
                xcb_destroy_window(conn, named);
                xcb_destroy_window(conn, helper);
                xcb_destroy_window(conn, game);
                xcb_flush(conn);
                xcb_disconnect(conn);
            }
            return;
        }

        let chosen = unsafe { resolve_override_target_on(conn, named) };
        assert_eq!(
            chosen, game,
            "a degenerate 1x1 ancestor is swapped for the painted 640x480 window"
        );

        // A legitimate (full-size) ancestor is returned unchanged.
        let legit = unsafe { xcb_generate_id(conn) };
        let lclient = unsafe { xcb_generate_id(conn) };
        mk(legit, root, 320, 240);
        mk(lclient, legit, 160, 120);
        unsafe { xcb_flush(conn) };
        assert_eq!(
            unsafe { resolve_override_target_on(conn, lclient) },
            legit,
            "a non-degenerate ancestor is returned unchanged"
        );

        unsafe {
            xcb_destroy_window(conn, lclient);
            xcb_destroy_window(conn, legit);
            xcb_destroy_window(conn, named);
            xcb_destroy_window(conn, helper);
            xcb_destroy_window(conn, game);
            xcb_flush(conn);
            xcb_disconnect(conn);
        }
    }

    /// The DRI3 import must not consume the slot's fd: everything after it (the
    /// present, the slot's reuse) reads the same fd.
    #[test]
    fn dri3_import_keeps_the_slots_fd_open() {
        use std::os::fd::AsRawFd;

        let conn = unsafe { xcb_connect(ptr::null(), ptr::null_mut()) };
        if conn.is_null() || unsafe { xcb_connection_has_error(conn) } != 0 {
            eprintln!(
                "{}",
                gndrm_obf::obf!("skip: no X server (DISPLAY unreachable)")
            );
            return;
        }
        let screen = unsafe { host_root(conn) };
        let Some((root, depth)) = screen else {
            eprintln!(
                "{}",
                gndrm_obf::obf!("skip: no X server (no screen in setup)")
            );
            unsafe { xcb_disconnect(conn) };
            return;
        };
        let window = unsafe { xcb_generate_id(conn) };
        unsafe {
            xcb_create_window(
                conn,
                depth,
                window,
                root,
                0,
                0,
                64,
                48,
                0,
                WINDOW_CLASS_INPUT_OUTPUT,
                COPY_FROM_PARENT,
                0,
                ptr::null(),
            )
        };
        // Flush so the shim's own connection (opened by `X11Present::new`) can
        // see the window before it queries it.
        unsafe { xcb_flush(conn) };

        let present = match unsafe { X11Present::new(window) } {
            Ok(p) => p,
            Err(why) => {
                eprintln!(
                    "{}",
                    gndrm_obf::obf!("skip: host X server has no DRI3/Present ({})", why.reason())
                );
                unsafe {
                    xcb_destroy_window(conn, window);
                    xcb_disconnect(conn);
                }
                return;
            }
        };

        // Any open fd will do: libxcb hands the fd to the server and closes it
        // regardless of whether the server accepts the buffer, so the slot's
        // ownership is what is under test, not the payload.
        let file =
            std::fs::File::open("/etc/hostname").expect(&gndrm_obf::obf!("open /etc/hostname"));
        let fd = file.as_raw_fd();
        assert!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0,
            "precondition: the fd starts open"
        );

        let _ = unsafe { present.make_pixmap(fd, 8, 8, 32, 0) };

        assert!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0,
            "make_pixmap must not close the slot's fd (libxcb's DRI3 import closes the fd it is handed)"
        );

        drop(present);
        unsafe {
            xcb_destroy_window(conn, window);
            xcb_flush(conn);
            xcb_disconnect(conn);
        }
    }

    fn open_fd_count() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .expect(&gndrm_obf::obf!("/proc/self/fd"))
            .count()
    }

    /// The per-frame present path must acquire no descriptor per frame. On the
    /// X11 route the only fd it ever touches is the slot's dma-buf, duped once
    /// by `make_pixmap` and consumed by libxcb when the request is sent (the
    /// C probe `tools/…`/this test's own loop shows the count is flat). This
    /// drives the same per-frame sequence the worker performs - a pixmap and a
    /// fence made once per slot, then present + fence poll per frame - and
    /// asserts the descriptor count does not grow. Skips without DRI3/Present.
    #[test]
    fn present_loop_is_fd_neutral() {
        let conn = unsafe { xcb_connect(ptr::null(), ptr::null_mut()) };
        if conn.is_null() || unsafe { xcb_connection_has_error(conn) } != 0 {
            eprintln!(
                "{}",
                gndrm_obf::obf!("skip: no X server (DISPLAY unreachable)")
            );
            return;
        }
        let Some((root, depth)) = (unsafe { host_root(conn) }) else {
            eprintln!(
                "{}",
                gndrm_obf::obf!("skip: no X server (no screen in setup)")
            );
            unsafe { xcb_disconnect(conn) };
            return;
        };
        let window = unsafe { xcb_generate_id(conn) };
        unsafe {
            xcb_create_window(
                conn,
                depth,
                window,
                root,
                0,
                0,
                64,
                48,
                0,
                WINDOW_CLASS_INPUT_OUTPUT,
                COPY_FROM_PARENT,
                0,
                ptr::null(),
            );
            xcb_flush(conn);
        }
        let mut present = match unsafe { X11Present::new(window) } {
            Ok(p) => p,
            Err(why) => {
                eprintln!(
                    "{}",
                    gndrm_obf::obf!("skip: host X server has no DRI3/Present ({})", why.reason())
                );
                unsafe {
                    xcb_destroy_window(conn, window);
                    xcb_disconnect(conn);
                }
                return;
            }
        };

        // A slot-backed descriptor stand-in. The server may or may not accept
        // it as a pixmap; either way the shim's own descriptor accounting is
        // what is under test, and a refused import is the retried case.
        let name = c"gn-slot";
        let slot_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(
            slot_fd >= 0,
            "memfd_create: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(unsafe { libc::ftruncate(slot_fd, 64 * 48 * 4) }, 0);

        let mut xpix = 0u32;
        let mut xfence = 0u32;
        // Warm up the once-per-slot resources, exactly as the worker does.
        for _ in 0..2 {
            if xpix == 0 {
                if let Some(p) = unsafe { present.make_pixmap(slot_fd, 64, 48, 256, 0) } {
                    xpix = p;
                }
            }
            if xfence == 0 {
                if let Some(f) = unsafe { present.make_fence() } {
                    xfence = f;
                }
            }
        }

        let before = open_fd_count();
        const N: usize = 1000;
        for _ in 0..N {
            if xpix == 0 {
                if let Some(p) = unsafe { present.make_pixmap(slot_fd, 64, 48, 256, 0) } {
                    xpix = p;
                }
            }
            if xpix != 0 {
                let _ = unsafe { present.present(xpix, xfence) };
            }
            if xfence != 0 {
                let _ = unsafe { present.fence_idle(xfence) };
            }
        }
        let after = open_fd_count();
        eprintln!("{}", gndrm_obf::obf!(
            "DEBUG present-loop: fds before={before} after={after} N={N} xpix={xpix} xfence={xfence}"
        ));
        assert!(
            after <= before + 1,
            "the present path acquired descriptors per frame: {before} -> {after} over {N} frames"
        );

        unsafe { libc::close(slot_fd) };
        drop(present);
        unsafe {
            xcb_destroy_window(conn, window);
            xcb_flush(conn);
            xcb_disconnect(conn);
        }
    }
}
