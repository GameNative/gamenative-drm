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
        let present = X11Present {
            lock: Mutex::new(()),
            conn,
            window: target_window,
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
                crate::logf!("x11 window: 0x{w:x} attributes unavailable at level {level}");
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
            crate::logf!(
                "x11 window: level={level} id=0x{w:x} map={ms} override={or} class={class} {gw}x{gh}"
            );
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
            crate::logf!(
                "x11 present target: 0x{:x} (named window is already a root child)",
                self.window
            );
        } else {
            crate::logf!(
                "x11 present target: 0x{:x} offset=({},{}) (root child of named window 0x{:x})",
                self.window,
                self.x_off,
                self.y_off,
                origin
            );
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
    /// slot's fd is dead for every later user, which is exactly how the
    /// one-shot frame dump came to fail with `mmap: Bad file descriptor`.
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
            crate::logf!(
                "x11: dup of slot fd failed: {} - frame dropped",
                std::io::Error::last_os_error()
            );
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
                crate::logf!(
                    "x11: {width}x{height} stride={stride} modifier={modifier:#x} pixmap_from_buffers failed (X error {code}) - frame dropped"
                );
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
                crate::logf!("x11: sync fence create failed (X error {code}) - frame dropped");
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
            crate::logf!("x11: present of pixmap {pixmap} failed (X error {code}) - frame dropped");
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
    }

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
            eprintln!("skip: no X server (DISPLAY unreachable)");
            return;
        }
        let screen = unsafe { host_root(conn) };
        let Some((root, depth)) = screen else {
            eprintln!("skip: no X server (no screen in setup)");
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
        eprintln!(
            "DEBUG root={root:#x} depth={depth} top={top:#x} client={client:#x} from_client={:?} from_top={:?}",
            unsafe { resolve_target(conn, client) },
            unsafe { resolve_target(conn, top) }
        );
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

    /// The DRI3 import must not consume the slot's fd: everything after it (the
    /// one-shot frame dump, the slot's reuse) reads the same fd. This is the
    /// regression guard for the `mmap: Bad file descriptor` dump failure.
    #[test]
    fn dri3_import_keeps_the_slots_fd_open() {
        use std::os::fd::AsRawFd;

        let conn = unsafe { xcb_connect(ptr::null(), ptr::null_mut()) };
        if conn.is_null() || unsafe { xcb_connection_has_error(conn) } != 0 {
            eprintln!("skip: no X server (DISPLAY unreachable)");
            return;
        }
        let screen = unsafe { host_root(conn) };
        let Some((root, depth)) = screen else {
            eprintln!("skip: no X server (no screen in setup)");
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
                eprintln!("skip: host X server has no DRI3/Present ({})", why.reason());
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
        let file = std::fs::File::open("/etc/hostname").expect("open /etc/hostname");
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
            .expect("/proc/self/fd")
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
            eprintln!("skip: no X server (DISPLAY unreachable)");
            return;
        }
        let Some((root, depth)) = (unsafe { host_root(conn) }) else {
            eprintln!("skip: no X server (no screen in setup)");
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
                eprintln!("skip: host X server has no DRI3/Present ({})", why.reason());
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
        eprintln!(
            "DEBUG present-loop: fds before={before} after={after} N={N} xpix={xpix} xfence={xfence}"
        );
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
