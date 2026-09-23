//! `drm.h` subset: device-level structs, constants and ioctl numbers.
//!
//! All structs are `#[repr(C)]` mirrors of the kernel UAPI headers the shim
//! compiles against. Sizes/offsets are asserted against the real C headers in
//! `tests/abi_ground_truth.rs` — never tweak a field without that test.

use crate::ioctl::{iow, iowr};

/// ioctl type char for all DRM ioctls (`DRM_IOCTL_BASE`).
pub const DRM_IOCTL_BASE: u32 = b'd' as u32;

// ---------------------------------------------------------------------------
// structs
// ---------------------------------------------------------------------------

/// `struct drm_version` — two-call length semantics: caller passes buffers
/// (userspace pointers, meaningless across RPC — the daemon marshals the
/// strings into the wire response instead).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVersion {
    pub version_major: i32,
    pub version_minor: i32,
    pub version_patchlevel: i32,
    pub name_len: usize,
    pub name: u64,
    pub date_len: usize,
    pub date: u64,
    pub desc_len: usize,
    pub desc: u64,
}

/// `struct drm_get_cap`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmGetCap {
    pub capability: u64,
    pub value: u64,
}

/// `struct drm_set_client_cap`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmSetClientCap {
    pub capability: u64,
    pub value: u64,
}

/// `struct drm_gem_close`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmGemClose {
    pub handle: u32,
    pub pad: u32,
}

/// `struct drm_prime_handle` (shared by `PRIME_FD_TO_HANDLE` / `HANDLE_TO_FD`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmPrimeHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
}

/// `struct drm_event` — common header of everything readable from the fd.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmEvent {
    pub ty: u32,
    pub length: u32,
}

/// `struct drm_event_vblank` (also used for `DRM_EVENT_FLIP_COMPLETE`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmEventVblank {
    pub base: DrmEvent,
    pub user_data: u64,
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub sequence: u32,
    pub crtc_id: u32,
}

// ---------------------------------------------------------------------------
// device capabilities (`DRM_CAP_*`)
// ---------------------------------------------------------------------------

pub const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
pub const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
pub const DRM_CAP_CURSOR_WIDTH: u64 = 0x8;
pub const DRM_CAP_CURSOR_HEIGHT: u64 = 0x9;
pub const DRM_CAP_ADDFB2_MODIFIERS: u64 = 0x10;
pub const DRM_CAP_SYNCOBJ: u64 = 0x13;

// ---------------------------------------------------------------------------
// client capabilities (`DRM_CLIENT_CAP_*`)
// ---------------------------------------------------------------------------

pub const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
pub const DRM_CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;

// ---------------------------------------------------------------------------
// event types (`DRM_EVENT_*`)
// ---------------------------------------------------------------------------

pub const DRM_EVENT_VBLANK: u32 = 0x01;
pub const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

// ---------------------------------------------------------------------------
// ioctl numbers
// ---------------------------------------------------------------------------

pub const DRM_IOCTL_VERSION: u32 = iowr::<DrmVersion>(DRM_IOCTL_BASE, 0x00);
pub const DRM_IOCTL_GEM_CLOSE: u32 = iow::<DrmGemClose>(DRM_IOCTL_BASE, 0x09);
pub const DRM_IOCTL_GET_CAP: u32 = iowr::<DrmGetCap>(DRM_IOCTL_BASE, 0x0c);
pub const DRM_IOCTL_SET_CLIENT_CAP: u32 = iow::<DrmSetClientCap>(DRM_IOCTL_BASE, 0x0d);
pub const DRM_IOCTL_SET_MASTER: u32 = crate::ioctl::ioc(0, DRM_IOCTL_BASE, 0x1e, 0);
pub const DRM_IOCTL_DROP_MASTER: u32 = crate::ioctl::ioc(0, DRM_IOCTL_BASE, 0x1f, 0);
pub const DRM_IOCTL_PRIME_HANDLE_TO_FD: u32 = iowr::<DrmPrimeHandle>(DRM_IOCTL_BASE, 0x2d);
pub const DRM_IOCTL_PRIME_FD_TO_HANDLE: u32 = iowr::<DrmPrimeHandle>(DRM_IOCTL_BASE, 0x2e);

// ---------------------------------------------------------------------------
// layout assertions (LP64: host x86_64, Android aarch64 — both identical)
// ---------------------------------------------------------------------------

const fn assert_size<T>(expected: usize) {
    assert!(
        core::mem::size_of::<T>() == expected,
        "struct size mismatch"
    );
}

const _: () = {
    assert_size::<DrmVersion>(64);
    assert_size::<DrmGetCap>(16);
    assert_size::<DrmSetClientCap>(16);
    assert_size::<DrmGemClose>(8);
    assert_size::<DrmPrimeHandle>(12);
    assert_size::<DrmEvent>(8);
    assert_size::<DrmEventVblank>(32);
};
