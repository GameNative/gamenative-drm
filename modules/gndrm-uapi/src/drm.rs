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

/// `struct drm_gem_create` — allocate a buffer object and return a handle.
/// The daemon backs each handle with a real, CPU-mappable buffer and exports
/// it as a dma-buf, so a GEM created here is indistinguishable from one
/// imported through `PRIME_FD_TO_HANDLE`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmGemCreate {
    pub size: u64,
    pub handle: u32,
    pub flags: u32,
}

/// `struct drm_gem_mmap` — the legacy handle→mmap-offset request. The returned
/// `offset` is the value the client passes to `mmap(fd, offset)`; a socket
/// cannot be mapped, so the shim recognises the offset and maps the object's
/// dma-buf in its place.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmGemMmap {
    pub handle: u32,
    pub pad: u32,
    pub offset: u64,
    pub size: u64,
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
/// `DRM_CAP_SYNCOBJ_TIMELINE` — queried by Xwayland's
/// `xwl_gbm_supports_syncobjs` before it will use explicit sync. A kernel
/// answers it with 0 (unsupported) rather than an error, so the daemon does
/// too; erroring here is not what a real device does.
pub const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;

/// `DRM_CAP_PRIME` value bits (import and export both supported).
pub const DRM_PRIME_CAP_IMPORT: u64 = 0x1;
pub const DRM_PRIME_CAP_EXPORT: u64 = 0x2;
/// `DRM_CAP_PRIME` is capability 0x5 (its value is a bitmask of
/// [`DRM_PRIME_CAP_IMPORT`]/[`DRM_PRIME_CAP_EXPORT`]).
pub const DRM_CAP_PRIME: u64 = 0x5;

// ---------------------------------------------------------------------------
// client capabilities (`DRM_CLIENT_CAP_*`)
// ---------------------------------------------------------------------------

pub const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
pub const DRM_CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;

/// gndrm private client cap `"GNLG"`: stamp each page-flip event this client
/// receives with a point on the panel's latch grid (the `SurfaceFlinger` latch
/// the published frame targets, minus a lead) instead of the session's 1200 Hz
/// timeline. Not a kernel cap: a real kernel, or a daemon without it, answers
/// `EINVAL`. gamescope sets it as `GNDRM_CLIENT_CAP_LATCH_GRID 0x474E4C47` and
/// keeps going without it. See `docs/11-refresh-model.md`.
pub const GNDRM_CLIENT_CAP_LATCH_GRID: u64 = 0x474E_4C47;

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
/// `DRM_IOCTL_GEM_CREATE = _IOWR('d', 0x1b, struct drm_gem_create[16])`.
pub const DRM_IOCTL_GEM_CREATE: u32 = iowr::<DrmGemCreate>(DRM_IOCTL_BASE, 0x1b);
pub const DRM_IOCTL_GET_CAP: u32 = iowr::<DrmGetCap>(DRM_IOCTL_BASE, 0x0c);
pub const DRM_IOCTL_SET_CLIENT_CAP: u32 = iow::<DrmSetClientCap>(DRM_IOCTL_BASE, 0x0d);
/// `DRM_IOCTL_GEM_MMAP = _IOWR('d', 0x1e, struct drm_gem_mmap[24])`.
pub const DRM_IOCTL_GEM_MMAP: u32 = iowr::<DrmGemMmap>(DRM_IOCTL_BASE, 0x1e);
pub const DRM_IOCTL_SET_MASTER: u32 = crate::ioctl::ioc(0, DRM_IOCTL_BASE, 0x1e, 0);
pub const DRM_IOCTL_DROP_MASTER: u32 = crate::ioctl::ioc(0, DRM_IOCTL_BASE, 0x1f, 0);
pub const DRM_IOCTL_PRIME_HANDLE_TO_FD: u32 = iowr::<DrmPrimeHandle>(DRM_IOCTL_BASE, 0x2d);
pub const DRM_IOCTL_PRIME_FD_TO_HANDLE: u32 = iowr::<DrmPrimeHandle>(DRM_IOCTL_BASE, 0x2e);

// ---------------------------------------------------------------------------
// sync objects (`drm_syncobj_*`)
//
// gamescope advertises `wp_linux_drm_syncobj_v1` and, when `DRM_CAP_SYNCOBJ`
// is set, creates ONE always-signalled syncobj and exports it to a sync file,
// which it then passes as the primary plane's `IN_FENCE_FD` on every commit.
// So the surface a client actually needs is create-signalled + export.
// ---------------------------------------------------------------------------

/// `drm_syncobj_create.flags`: the new sync object starts signalled.
pub const DRM_SYNCOBJ_CREATE_SIGNALED: u32 = 1 << 0;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmSyncobjCreate {
    pub handle: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmSyncobjDestroy {
    pub handle: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmSyncobjHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
    pub pad: u32,
    pub point: u64,
}

pub const DRM_IOCTL_SYNCOBJ_CREATE: u32 = iowr::<DrmSyncobjCreate>(DRM_IOCTL_BASE, 0xbf);
pub const DRM_IOCTL_SYNCOBJ_DESTROY: u32 = iowr::<DrmSyncobjDestroy>(DRM_IOCTL_BASE, 0xc0);
pub const DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD: u32 = iowr::<DrmSyncobjHandle>(DRM_IOCTL_BASE, 0xc1);
pub const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: u32 = iowr::<DrmSyncobjHandle>(DRM_IOCTL_BASE, 0xc2);

// ---------------------------------------------------------------------------
// render-node surface
// ---------------------------------------------------------------------------

/// Connection-kind control opcode: the shim sends one `u32` (a
/// [`NODE_KIND_CARD`]/[`NODE_KIND_RENDER`] value) on the RPC channel right
/// after connecting, telling the daemon which node type it is impersonating.
/// Chosen far outside the DRM ioctl space so it can never collide with one.
pub const GNDRM_IOCTL_NODE_KIND: u32 = 0x474E_0001;

/// The connection impersonates the KMS card node (`/dev/dri/cardN`): the full
/// modesetting surface is available.
pub const NODE_KIND_CARD: u32 = 0;
/// The connection impersonates a render node (`/dev/dri/renderDN`): only the
/// rendering-capable ioctls are allowed, as on a real kernel render node.
pub const NODE_KIND_RENDER: u32 = 1;

/// Magic high half of the mmap offset `GEM_MMAP` mints for a handle: the shim
/// recognises `offset & GEM_MMAP_OFFSET_MASK == GEM_MMAP_OFFSET_BASE` and maps
/// the handle's dma-buf instead of trying to map the socket.
pub const GEM_MMAP_OFFSET_BASE: u64 = 0x474E_0000_0000_0000;
/// Bits of the mmap offset that carry the GEM handle.
pub const GEM_MMAP_HANDLE_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Whether an ioctl is permitted on a render node, mirroring the kernel's
/// `DRM_RENDER_ALLOW` table: rendering (GEM/PRIME/syncobj) and identification
/// (`VERSION`/`GET_CAP`) are allowed; modesetting and master acquisition are
/// not.
/// The node-kind control opcode is connection setup, not a kernel ioctl, and is
/// always allowed.
#[must_use]
pub fn render_node_allows(request: u32) -> bool {
    matches!(
        request,
        DRM_IOCTL_VERSION
            | DRM_IOCTL_GET_CAP
            | DRM_IOCTL_SET_CLIENT_CAP
            | DRM_IOCTL_GEM_CLOSE
            | DRM_IOCTL_GEM_CREATE
            | DRM_IOCTL_GEM_MMAP
            | DRM_IOCTL_PRIME_HANDLE_TO_FD
            | DRM_IOCTL_PRIME_FD_TO_HANDLE
            | DRM_IOCTL_SYNCOBJ_CREATE
            | DRM_IOCTL_SYNCOBJ_DESTROY
            | DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD
            | DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE
            | GNDRM_IOCTL_NODE_KIND
    )
}

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
    assert_size::<DrmGemCreate>(16);
    assert_size::<DrmGemMmap>(24);
    assert_size::<DrmEvent>(8);
    assert_size::<DrmEventVblank>(32);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// `GEM_CREATE` and `GEM_MMAP` are the legacy generic GEM ioctls, dropped
    /// from the modern `drm.h` (which now carries only `GEM_CLOSE`/`FLINK`/
    /// `OPEN`), so they cannot be ground-truthed against the installed header
    /// the way the rest of the UAPI is in `abi_ground_truth.rs`. Their encodings are
    /// therefore pinned here against the historical numbers, so an accidental
    /// edit to a request number or a struct field is caught locally.
    #[test]
    fn legacy_gem_request_numbers_are_pinned() {
        // _IOWR('d', 0x1b, 16) and _IOWR('d', 0x1e, 24).
        assert_eq!(DRM_IOCTL_GEM_CREATE, 0xC010_641B);
        assert_eq!(DRM_IOCTL_GEM_MMAP, 0xC018_641E);
        assert_eq!(crate::ioctl::ioc_size(DRM_IOCTL_GEM_CREATE), 16);
        assert_eq!(crate::ioctl::ioc_size(DRM_IOCTL_GEM_MMAP), 24);
        // Distinct from the same-numbered legacy DRM_IOCTL_RM_MAP (0x1b) and
        // DRM_IOCTL_SET_MASTER (0x1e), which differ in direction/size.
        assert_ne!(DRM_IOCTL_GEM_CREATE, DRM_IOCTL_SET_MASTER);
        assert_ne!(DRM_IOCTL_GEM_MMAP, DRM_IOCTL_SET_MASTER);
    }

    #[test]
    fn render_node_gating_matches_the_kernel_table() {
        // Identification + GEM/PRIME/syncobj are render-capable.
        for nr in [
            DRM_IOCTL_VERSION,
            DRM_IOCTL_GET_CAP,
            DRM_IOCTL_GEM_CREATE,
            DRM_IOCTL_GEM_MMAP,
            DRM_IOCTL_GEM_CLOSE,
            DRM_IOCTL_PRIME_HANDLE_TO_FD,
            DRM_IOCTL_PRIME_FD_TO_HANDLE,
            DRM_IOCTL_SYNCOBJ_CREATE,
        ] {
            assert!(render_node_allows(nr), "0x{nr:08x} must be render-allowed");
        }
        // Modesetting and master acquisition are not.
        for nr in [
            DRM_IOCTL_SET_MASTER,
            DRM_IOCTL_DROP_MASTER,
            crate::mode::DRM_IOCTL_MODE_GETRESOURCES,
            crate::mode::DRM_IOCTL_MODE_ATOMIC,
            crate::mode::DRM_IOCTL_MODE_ADDFB2,
        ] {
            assert!(!render_node_allows(nr), "0x{nr:08x} must be card-only");
        }
        // The connection-setup opcode is not a kernel ioctl and is always
        // accepted, so a render-node client can declare itself.
        assert!(render_node_allows(GNDRM_IOCTL_NODE_KIND));
    }

    #[test]
    fn mmap_offset_round_trips_the_handle() {
        let handle = 0x1234_5678u32;
        let offset = GEM_MMAP_OFFSET_BASE | u64::from(handle);
        assert_eq!(offset & GEM_MMAP_OFFSET_BASE, GEM_MMAP_OFFSET_BASE);
        assert_eq!(offset & GEM_MMAP_HANDLE_MASK, u64::from(handle));
    }
}
