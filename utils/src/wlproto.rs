//! Wayland client FFI + zwp_linux_dmabuf_v1 / xdg-shell statics for the
//! smoke-test clients. Same linking model as the shim (NEEDED
//! libwayland-client); protocol statics transliterated from the
//! wayland-scanner output.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::os::raw::{c_char, c_int, c_void};

pub enum wl_proxy {}
pub enum wl_display {}
pub enum wl_event_queue {}

#[repr(C)]
pub struct wl_message {
    pub name: *const c_char,
    pub signature: *const c_char,
    pub types: *const *const wl_interface,
}
unsafe impl Sync for wl_message {}

#[repr(C)]
pub struct wl_interface {
    pub name: *const c_char,
    pub version: c_int,
    pub method_count: c_int,
    pub methods: *const wl_message,
    pub event_count: c_int,
    pub events: *const wl_message,
}
unsafe impl Sync for wl_interface {}

#[repr(C)]
pub struct wl_array {
    pub size: usize,
    pub alloc: usize,
    pub data: *mut c_void,
}

#[repr(C)]
pub union wl_argument {
    pub i: i32,
    pub u: u32,
    pub f: i32,
    pub s: *const c_char,
    pub o: *mut wl_proxy,
    pub n: u32,
    pub a: *mut wl_array,
    pub h: i32,
}

pub type WlEventFn = extern "C" fn();

#[link(name = "wayland-client")]
extern "C" {
    pub fn wl_display_connect(name: *const c_char) -> *mut wl_display;
    pub fn wl_display_roundtrip(dpy: *mut wl_display) -> c_int;
    pub fn wl_display_dispatch(dpy: *mut wl_display) -> c_int;
    pub fn wl_display_dispatch_pending(dpy: *mut wl_display) -> c_int;
    pub fn wl_display_flush(dpy: *mut wl_display) -> c_int;

    pub fn wl_proxy_add_listener(
        proxy: *mut wl_proxy,
        implementation: *mut WlEventFn,
        data: *mut c_void,
    ) -> c_int;
    pub fn wl_proxy_destroy(proxy: *mut wl_proxy);
    pub fn wl_proxy_marshal_array(proxy: *mut wl_proxy, opcode: u32, args: *mut wl_argument);
    pub fn wl_proxy_marshal_array_constructor(
        proxy: *mut wl_proxy,
        opcode: u32,
        args: *mut wl_argument,
        interface: *const wl_interface,
    ) -> *mut wl_proxy;
    pub fn wl_proxy_marshal_array_constructor_versioned(
        proxy: *mut wl_proxy,
        opcode: u32,
        args: *mut wl_argument,
        interface: *const wl_interface,
        version: u32,
    ) -> *mut wl_proxy;

    pub static wl_registry_interface: wl_interface;
    pub static wl_compositor_interface: wl_interface;
    pub static wl_surface_interface: wl_interface;
    pub static wl_buffer_interface: wl_interface;
    pub static wl_seat_interface: wl_interface;
    pub static wl_output_interface: wl_interface;
}

/// NUL-terminated static string as a C pointer (interface/message names and
/// signatures live in .rodata for the process lifetime).
const fn cs(s: &'static [u8]) -> *const c_char {
    s.as_ptr() as *const c_char
}

// ----------------------------------------------------------- core requests

/// wl_display_get_registry (opcode 1, version 1; wl_display.sync is 0).
pub unsafe fn display_get_registry(dpy: *mut wl_display) -> *mut wl_proxy {
    // libwayland writes the newly created proxy back into args['n' slot];
    // a NULL args array segfaults inside wl_proxy_marshal_array_flags for
    // any message whose signature contains 'n'.
    let mut args = [wl_argument { u: 0 }];
    wl_proxy_marshal_array_constructor_versioned(
        dpy.cast(),
        1,
        args.as_mut_ptr(),
        &wl_registry_interface,
        1,
    )
}

/// wl_registry_bind (opcode 0): name, interface@version.
pub unsafe fn registry_bind(
    reg: *mut wl_proxy,
    name: u32,
    interface: *const wl_interface,
    version: u32,
) -> *mut wl_proxy {
    let mut args = [
        wl_argument { n: name },
        wl_argument {
            s: (*interface).name,
        },
        wl_argument { u: version },
        wl_argument { u: 0 },
    ];
    wl_proxy_marshal_array_constructor_versioned(reg, 0, args.as_mut_ptr(), interface, version)
}

/// wl_compositor_create_surface (opcode 0).
pub unsafe fn compositor_create_surface(comp: *mut wl_proxy) -> *mut wl_proxy {
    // 'n' slot: libwayland writes the created proxy back into the args array.
    let mut args = [wl_argument { u: 0 }];
    wl_proxy_marshal_array_constructor(comp, 0, args.as_mut_ptr(), &wl_surface_interface)
}

/// wl_surface_attach (opcode 1): buffer, x, y.
pub unsafe fn surface_attach(surface: *mut wl_proxy, buffer: *mut wl_proxy, x: i32, y: i32) {
    let mut args = [
        wl_argument { o: buffer },
        wl_argument { i: x },
        wl_argument { i: y },
    ];
    wl_proxy_marshal_array(surface, 1, args.as_mut_ptr());
}

/// wl_surface_damage_buffer (opcode 9, since version 4): x, y, w, h.
pub unsafe fn surface_damage_buffer(surface: *mut wl_proxy, x: i32, y: i32, w: i32, h: i32) {
    let mut args = [
        wl_argument { i: x },
        wl_argument { i: y },
        wl_argument { i: w },
        wl_argument { i: h },
    ];
    wl_proxy_marshal_array(surface, 9, args.as_mut_ptr());
}

/// wl_surface_commit (opcode 6).
pub unsafe fn surface_commit(surface: *mut wl_proxy) {
    wl_proxy_marshal_array(surface, 6, std::ptr::null_mut());
}

// ------------------------------------------------- gamescope_swapchain statics
// Transliterated from gamescope's protocol/gamescope-swapchain.xml (the
// upstream WSI-override mechanism): only the requests the shim sends.

#[allow(clippy::missing_transmute_annotations)]
mod swapchain_statics {
    use super::{cs, wl_interface, wl_message};

    // create_swapchain args: wl_surface (object), gamescope_swapchain (new_id).
    struct SyncIfaces([*const wl_interface; 2]);
    unsafe impl Sync for SyncIfaces {}
    static TYPES: SyncIfaces = SyncIfaces([
        &raw const super::wl_surface_interface,
        &raw const GAMESCOPE_SWAPCHAIN_INTERFACE,
    ]);

    static FACTORY_REQUESTS: [wl_message; 2] = [
        wl_message {
            name: cs(b"destroy\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"create_swapchain\0"),
            signature: cs(b"on\0"),
            types: TYPES.0.as_ptr(),
        },
    ];

    pub static GAMESCOPE_SWAPCHAIN_FACTORY_V2_INTERFACE: wl_interface = wl_interface {
        name: cs(b"gamescope_swapchain_factory_v2\0"),
        version: 1,
        method_count: 2,
        methods: FACTORY_REQUESTS.as_ptr(),
        event_count: 0,
        events: std::ptr::null(),
    };

    static SWAPCHAIN_REQUESTS: [wl_message; 2] = [
        wl_message {
            name: cs(b"destroy\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"override_window_content\0"),
            signature: cs(b"uu\0"),
            types: std::ptr::null(),
        },
    ];

    pub static GAMESCOPE_SWAPCHAIN_INTERFACE: wl_interface = wl_interface {
        name: cs(b"gamescope_swapchain\0"),
        version: 1,
        method_count: 2,
        methods: SWAPCHAIN_REQUESTS.as_ptr(),
        event_count: 0,
        events: std::ptr::null(),
    };
}

pub use swapchain_statics::{
    GAMESCOPE_SWAPCHAIN_FACTORY_V2_INTERFACE, GAMESCOPE_SWAPCHAIN_INTERFACE,
};

/// gamescope_swapchain_factory_v2.create_swapchain (opcode 1): surface, new_id.
pub unsafe fn swapchain_factory_create_swapchain(
    factory: *mut wl_proxy,
    surface: *mut wl_proxy,
) -> *mut wl_proxy {
    let mut args = [wl_argument { o: surface }, wl_argument { u: 0 }];
    wl_proxy_marshal_array_constructor(
        factory,
        1,
        args.as_mut_ptr(),
        &GAMESCOPE_SWAPCHAIN_INTERFACE,
    )
}

/// gamescope_swapchain.override_window_content (opcode 1): server id, XID.
pub unsafe fn swapchain_override_window_content(sc: *mut wl_proxy, server: u32, xid: u32) {
    let mut args = [wl_argument { u: server }, wl_argument { u: xid }];
    wl_proxy_marshal_array(sc, 1, args.as_mut_ptr());
}

// ------------------------------------------------- linux-dmabuf-v1 statics

mod dmabuf_statics {
    use super::{cs, wl_interface, wl_message};

    struct SyncIfaces([*const wl_interface; 16]);
    unsafe impl Sync for SyncIfaces {}

    static TYPES: SyncIfaces = SyncIfaces([
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        &raw const ZWP_LINUX_BUFFER_PARAMS_V1_INTERFACE,
        std::ptr::null(), // zwp_linux_dmabuf_feedback_v1_interface (v4+)
        std::ptr::null(), // "
        &raw const super::wl_surface_interface,
        &raw const super::wl_buffer_interface,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        &raw const super::wl_buffer_interface,
    ]);

    static DMABUF_REQUESTS: [wl_message; 4] = [
        wl_message {
            name: cs(b"destroy\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"create_params\0"),
            signature: cs(b"n\0"),
            types: unsafe { TYPES.0.as_ptr().add(6) },
        },
        wl_message {
            name: cs(b"get_default_feedback\0"),
            signature: cs(b"4n\0"),
            types: unsafe { TYPES.0.as_ptr().add(7) },
        },
        wl_message {
            name: cs(b"get_surface_feedback\0"),
            signature: cs(b"4no\0"),
            types: unsafe { TYPES.0.as_ptr().add(8) },
        },
    ];

    static DMABUF_EVENTS: [wl_message; 2] = [
        wl_message {
            name: cs(b"format\0"),
            signature: cs(b"u\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"modifier\0"),
            signature: cs(b"3uuu\0"),
            types: std::ptr::null(),
        },
    ];

    pub static ZWP_LINUX_DMABUF_V1_INTERFACE: wl_interface = wl_interface {
        name: cs(b"zwp_linux_dmabuf_v1\0"),
        version: 4,
        method_count: 4,
        methods: DMABUF_REQUESTS.as_ptr(),
        event_count: 2,
        events: DMABUF_EVENTS.as_ptr(),
    };

    static PARAMS_REQUESTS: [wl_message; 5] = [
        wl_message {
            name: cs(b"destroy\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"add\0"),
            signature: cs(b"huuuuu\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"create\0"),
            signature: cs(b"iiuu\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"create_immed\0"),
            signature: cs(b"2niiuu\0"),
            types: unsafe { TYPES.0.as_ptr().add(10) },
        },
        wl_message {
            name: cs(b"set_sampling_device\0"),
            signature: cs(b"6a\0"),
            types: std::ptr::null(),
        },
    ];

    static PARAMS_EVENTS: [wl_message; 2] = [
        wl_message {
            name: cs(b"created\0"),
            signature: cs(b"n\0"),
            types: unsafe { TYPES.0.as_ptr().add(15) },
        },
        wl_message {
            name: cs(b"failed\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
    ];

    pub static ZWP_LINUX_BUFFER_PARAMS_V1_INTERFACE: wl_interface = wl_interface {
        name: cs(b"zwp_linux_buffer_params_v1\0"),
        version: 4,
        method_count: 5,
        methods: PARAMS_REQUESTS.as_ptr(),
        event_count: 2,
        events: PARAMS_EVENTS.as_ptr(),
    };
}

pub use dmabuf_statics::{ZWP_LINUX_BUFFER_PARAMS_V1_INTERFACE, ZWP_LINUX_DMABUF_V1_INTERFACE};

/// zwp_linux_dmabuf_v1_create_params (opcode 1).
pub unsafe fn dmabuf_create_params(dmabuf: *mut wl_proxy) -> *mut wl_proxy {
    // 'n' slot: libwayland writes the created proxy back into the args array.
    let mut args = [wl_argument { u: 0 }];
    wl_proxy_marshal_array_constructor(
        dmabuf,
        1,
        args.as_mut_ptr(),
        &ZWP_LINUX_BUFFER_PARAMS_V1_INTERFACE,
    )
}

/// zwp_linux_buffer_params_v1_add (opcode 1): fd, plane, offset, stride, mod.
pub unsafe fn params_add(
    params: *mut wl_proxy,
    fd: i32,
    plane: u32,
    offset: u32,
    stride: u32,
    modifier: u64,
) {
    let mut args = [
        wl_argument { h: fd },
        wl_argument { u: plane },
        wl_argument { u: offset },
        wl_argument { u: stride },
        wl_argument {
            u: (modifier >> 32) as u32,
        },
        wl_argument { u: modifier as u32 },
    ];
    wl_proxy_marshal_array(params, 1, args.as_mut_ptr());
}

/// zwp_linux_buffer_params_v1_create_immed (opcode 3, since 2): w, h, fourcc.
pub unsafe fn params_create_immed(
    params: *mut wl_proxy,
    width: i32,
    height: i32,
    format: u32,
    flags: u32,
) -> *mut wl_proxy {
    let mut args = [
        wl_argument { u: 0 },
        wl_argument { i: width },
        wl_argument { i: height },
        wl_argument { u: format },
        wl_argument { u: flags },
    ];
    wl_proxy_marshal_array_constructor(params, 3, args.as_mut_ptr(), unsafe {
        &wl_buffer_interface
    })
}

// ------------------------------------------------------- xdg-shell statics

mod xdg_shell_statics {
    use super::{cs, wl_interface, wl_message};

    struct SyncIfaces([*const wl_interface; 26]);
    unsafe impl Sync for SyncIfaces {}

    // Matches the wayland-scanner xdg_shell_types[] layout; entries for
    // messages never sent stay NULL (libwayland only dereferences types[]
    // for object args of messages actually marshalled).
    static TYPES: SyncIfaces = SyncIfaces([
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(), // xdg_positioner_interface (unused)
        &raw const XDG_SURFACE_INTERFACE,
        &raw const super::wl_surface_interface,
        &raw const XDG_TOPLEVEL_INTERFACE,
        std::ptr::null(), // xdg_popup_interface (unused)
        &raw const XDG_SURFACE_INTERFACE,
        std::ptr::null(), // xdg_positioner_interface
        &raw const XDG_TOPLEVEL_INTERFACE,
        &raw const super::wl_seat_interface,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        &raw const super::wl_seat_interface,
        std::ptr::null(),
        &raw const super::wl_seat_interface,
        std::ptr::null(),
        std::ptr::null(),
        &raw const super::wl_output_interface,
        &raw const super::wl_seat_interface,
        std::ptr::null(),
        std::ptr::null(), // xdg_positioner_interface
        std::ptr::null(),
    ]);

    static WM_BASE_REQUESTS: [wl_message; 4] = [
        wl_message {
            name: cs(b"destroy\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"create_positioner\0"),
            signature: cs(b"n\0"),
            types: unsafe { TYPES.0.as_ptr().add(4) },
        },
        wl_message {
            name: cs(b"get_xdg_surface\0"),
            signature: cs(b"no\0"),
            types: unsafe { TYPES.0.as_ptr().add(5) },
        },
        wl_message {
            name: cs(b"pong\0"),
            signature: cs(b"u\0"),
            types: std::ptr::null(),
        },
    ];

    static WM_BASE_EVENTS: [wl_message; 1] = [wl_message {
        name: cs(b"ping\0"),
        signature: cs(b"u\0"),
        types: std::ptr::null(),
    }];

    pub static XDG_WM_BASE_INTERFACE: wl_interface = wl_interface {
        name: cs(b"xdg_wm_base\0"),
        version: 1,
        method_count: 4,
        methods: WM_BASE_REQUESTS.as_ptr(),
        event_count: 1,
        events: WM_BASE_EVENTS.as_ptr(),
    };

    static XDG_SURFACE_REQUESTS: [wl_message; 5] = [
        wl_message {
            name: cs(b"destroy\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"get_toplevel\0"),
            signature: cs(b"n\0"),
            types: unsafe { TYPES.0.as_ptr().add(7) },
        },
        wl_message {
            name: cs(b"get_popup\0"),
            signature: cs(b"n?oo\0"),
            types: unsafe { TYPES.0.as_ptr().add(8) },
        },
        wl_message {
            name: cs(b"set_window_geometry\0"),
            signature: cs(b"iiii\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"ack_configure\0"),
            signature: cs(b"u\0"),
            types: std::ptr::null(),
        },
    ];

    static XDG_SURFACE_EVENTS: [wl_message; 1] = [wl_message {
        name: cs(b"configure\0"),
        signature: cs(b"u\0"),
        types: std::ptr::null(),
    }];

    pub static XDG_SURFACE_INTERFACE: wl_interface = wl_interface {
        name: cs(b"xdg_surface\0"),
        version: 1,
        method_count: 5,
        methods: XDG_SURFACE_REQUESTS.as_ptr(),
        event_count: 1,
        events: XDG_SURFACE_EVENTS.as_ptr(),
    };

    static TOPLEVEL_REQUESTS: [wl_message; 14] = [
        wl_message {
            name: cs(b"destroy\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"set_parent\0"),
            signature: cs(b"?o\0"),
            types: unsafe { TYPES.0.as_ptr().add(11) },
        },
        wl_message {
            name: cs(b"set_title\0"),
            signature: cs(b"s\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"set_app_id\0"),
            signature: cs(b"s\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"show_window_menu\0"),
            signature: cs(b"ouii\0"),
            types: unsafe { TYPES.0.as_ptr().add(12) },
        },
        wl_message {
            name: cs(b"move\0"),
            signature: cs(b"ou\0"),
            types: unsafe { TYPES.0.as_ptr().add(16) },
        },
        wl_message {
            name: cs(b"resize\0"),
            signature: cs(b"ouu\0"),
            types: unsafe { TYPES.0.as_ptr().add(18) },
        },
        wl_message {
            name: cs(b"set_max_size\0"),
            signature: cs(b"ii\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"set_min_size\0"),
            signature: cs(b"ii\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"set_maximized\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"unset_maximized\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"set_fullscreen\0"),
            signature: cs(b"?o\0"),
            types: unsafe { TYPES.0.as_ptr().add(21) },
        },
        wl_message {
            name: cs(b"unset_fullscreen\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"set_minimized\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
    ];

    static TOPLEVEL_EVENTS: [wl_message; 4] = [
        wl_message {
            name: cs(b"configure\0"),
            signature: cs(b"iia\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"close\0"),
            signature: cs(b"\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"configure_bounds\0"),
            signature: cs(b"4ii\0"),
            types: std::ptr::null(),
        },
        wl_message {
            name: cs(b"wm_capabilities\0"),
            signature: cs(b"5a\0"),
            types: std::ptr::null(),
        },
    ];

    pub static XDG_TOPLEVEL_INTERFACE: wl_interface = wl_interface {
        name: cs(b"xdg_toplevel\0"),
        version: 1,
        method_count: 14,
        methods: TOPLEVEL_REQUESTS.as_ptr(),
        event_count: 4,
        events: TOPLEVEL_EVENTS.as_ptr(),
    };
}

pub use xdg_shell_statics::{XDG_SURFACE_INTERFACE, XDG_TOPLEVEL_INTERFACE, XDG_WM_BASE_INTERFACE};

/// xdg_wm_base_get_xdg_surface (opcode 2): surface.
pub unsafe fn wm_base_get_xdg_surface(wm: *mut wl_proxy, surface: *mut wl_proxy) -> *mut wl_proxy {
    let mut args = [wl_argument { u: 0 }, wl_argument { o: surface }];
    wl_proxy_marshal_array_constructor(wm, 2, args.as_mut_ptr(), &XDG_SURFACE_INTERFACE)
}

/// xdg_wm_base_pong (opcode 3): serial.
pub unsafe fn wm_base_pong(wm: *mut wl_proxy, serial: u32) {
    let mut args = [wl_argument { u: serial }];
    wl_proxy_marshal_array(wm, 3, args.as_mut_ptr());
}

/// xdg_surface_get_toplevel (opcode 1).
pub unsafe fn xdg_surface_get_toplevel(xsurf: *mut wl_proxy) -> *mut wl_proxy {
    // 'n' slot: libwayland writes the created proxy back into the args array.
    let mut args = [wl_argument { u: 0 }];
    wl_proxy_marshal_array_constructor(xsurf, 1, args.as_mut_ptr(), &XDG_TOPLEVEL_INTERFACE)
}

/// xdg_surface_ack_configure (opcode 4): serial.
pub unsafe fn xdg_surface_ack_configure(xsurf: *mut wl_proxy, serial: u32) {
    let mut args = [wl_argument { u: serial }];
    wl_proxy_marshal_array(xsurf, 4, args.as_mut_ptr());
}

/// xdg_toplevel_set_title (opcode 2): title.
pub unsafe fn toplevel_set_title(toplevel: *mut wl_proxy, title: *const c_char) {
    let mut args = [wl_argument { s: title }];
    wl_proxy_marshal_array(toplevel, 2, args.as_mut_ptr());
}
