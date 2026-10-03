//! Wayland client FFI + zwp_linux_dmabuf_v1 statics.
//!
//! Raw libwayland-client declarations (linked, NEEDED — same as the C shim).
//! The core interface statics (wl_registry/wl_compositor/wl_buffer/…) are
//! data symbols exported by libwayland-client.so; the dmabuf protocol
//! statics are transliterated from the wayland-scanner output for
//! linux-dmabuf-v1 (identical wire layout to the unstable v1 the
//! compositor advertises at version 3).

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
    pub f: i32, // wl_fixed_t
    pub s: *const c_char,
    pub o: *mut wl_proxy,
    pub n: u32,
    pub a: *mut wl_array,
    pub h: i32, // fd
}

pub type WlEventFn = extern "C" fn();
// va_list decays to a plain pointer at the ABI level on both aarch64 and
// x86_64; *mut c_void keeps this on stable Rust (core::ffi::VaList in fn
// signatures is still unstable).
pub type WlLogFn = extern "C" fn(fmt: *const c_char, ap: *mut c_void);

extern "C" {
    pub fn vsnprintf(s: *mut c_char, n: usize, fmt: *const c_char, ap: *mut c_void) -> c_int;
}

#[link(name = "wayland-client")]
extern "C" {
    pub fn wl_display_connect(name: *const c_char) -> *mut wl_display;
    pub fn wl_display_create_queue(dpy: *mut wl_display) -> *mut wl_event_queue;
    pub fn wl_display_roundtrip_queue(dpy: *mut wl_display, queue: *mut wl_event_queue) -> c_int;
    pub fn wl_display_dispatch_queue(dpy: *mut wl_display, queue: *mut wl_event_queue) -> c_int;
    pub fn wl_display_dispatch_queue_pending(
        dpy: *mut wl_display,
        queue: *mut wl_event_queue,
    ) -> c_int;
    pub fn wl_display_prepare_read_queue(dpy: *mut wl_display, queue: *mut wl_event_queue)
        -> c_int;
    pub fn wl_display_read_events(dpy: *mut wl_display) -> c_int;
    pub fn wl_display_cancel_read(dpy: *mut wl_display);
    pub fn wl_display_get_fd(dpy: *mut wl_display) -> c_int;
    pub fn wl_display_get_error(dpy: *mut wl_display) -> c_int;
    pub fn wl_display_flush(dpy: *mut wl_display) -> c_int;
    pub fn wl_log_set_handler_client(handler: WlLogFn);

    pub fn wl_proxy_add_listener(
        proxy: *mut wl_proxy,
        implementation: *mut WlEventFn,
        data: *mut c_void,
    ) -> c_int;
    pub fn wl_proxy_set_queue(proxy: *mut wl_proxy, queue: *mut wl_event_queue);
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
    pub static wl_subcompositor_interface: wl_interface;
    pub static wl_surface_interface: wl_interface;
    pub static wl_buffer_interface: wl_interface;
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
        // new_id without a static type: marshal_array_constructor fills it.
        wl_argument { u: 0 },
    ];
    wl_proxy_marshal_array_constructor_versioned(reg, 0, args.as_mut_ptr(), interface, version)
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

// The wayland arm presents on the guest's OWN wl_surface (winewayland hands
// it to `vkCreateWaylandSurfaceKHR`), so no compositor-specific swapchain
// protocol is declared here: the X11 window route is DRI3 + Present in
// `crate::x11`, and it no longer binds a gamescope override surface.

// ------------------------------------------- zwp_linux_dmabuf_v1 statics
// Transliterated from the wayland-scanner output (linux-dmabuf-v1); only
// the members a version-3 bind can speak are referenced at runtime.

#[allow(clippy::missing_transmute_annotations)]
mod dmabuf_statics {
    use super::{wl_interface, wl_message};
    use std::os::raw::c_char;

    /// NUL-terminated static string as a C pointer (interface/message names
    /// and signatures live in .rodata for the process lifetime).
    const fn cs(s: &'static [u8]) -> *const c_char {
        s.as_ptr() as *const c_char
    }

    // types[] entries beyond index 6 need zwp_linux_dmabuf_feedback_v1
    // (version 4+) — never referenced at v3, so keep them NULL: libwayland
    // only dereferences types[] for object args of messages actually sent.
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
        std::ptr::null(), // wl_surface_interface (v4+)
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
        version: 3,
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
        version: 3,
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
        wl_argument { u: 0 }, // new_id filled by the constructor
        wl_argument { i: width },
        wl_argument { i: height },
        wl_argument { u: format },
        wl_argument { u: flags },
    ];
    wl_proxy_marshal_array_constructor(params, 3, args.as_mut_ptr(), unsafe {
        &wl_buffer_interface
    })
}
