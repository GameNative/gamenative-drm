//! `drm_mode.h` subset: KMS object structs, constants and ioctl numbers.
//!
//! See `drm.rs` for the mirroring/verification policy.

use crate::drm::DRM_IOCTL_BASE;
use crate::ioctl::iowr;

pub const DRM_DISPLAY_MODE_LEN: usize = 32;
pub const DRM_PROP_NAME_LEN: usize = 32;

// ---------------------------------------------------------------------------
// structs
// ---------------------------------------------------------------------------

/// `struct drm_mode_modeinfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeModeinfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub ty: u32,
    pub name: [u8; DRM_DISPLAY_MODE_LEN],
}

/// `struct drm_mode_card_res` (`DRM_IOCTL_MODE_GETRESOURCES`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeCardRes {
    pub fb_id_ptr: u64,
    pub crtc_id_ptr: u64,
    pub connector_id_ptr: u64,
    pub encoder_id_ptr: u64,
    pub count_fbs: u32,
    pub count_crtcs: u32,
    pub count_connectors: u32,
    pub count_encoders: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

/// `struct drm_mode_crtc` (`GETCRTC` / `SETCRTC`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeCrtc {
    pub set_connectors_ptr: u64,
    pub count_connectors: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: DrmModeModeinfo,
}

/// `struct drm_mode_get_encoder`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeGetEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

/// `struct drm_mode_get_connector`. All four `*_ptr` fields are userspace
/// pointers; across RPC the daemon returns the arrays marshalled instead.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeGetConnector {
    pub encoders_ptr: u64,
    pub modes_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_modes: u32,
    pub count_props: u32,
    pub count_encoders: u32,
    pub encoder_id: u32,
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: u32,
    pub pad: u32,
}

/// `struct drm_mode_get_plane_res`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeGetPlaneRes {
    pub plane_id_ptr: u64,
    pub count_planes: u32,
}

/// `struct drm_mode_get_plane`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeGetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
    pub count_format_types: u32,
    pub format_type_ptr: u64,
}

/// `struct drm_mode_obj_get_properties`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeObjGetProperties {
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_props: u32,
    pub obj_id: u32,
    pub obj_type: u32,
}

/// `struct drm_mode_obj_set_property`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeObjSetProperty {
    pub value: u64,
    pub prop_id: u32,
    pub obj_id: u32,
    pub obj_type: u32,
}

/// `struct drm_mode_property_enum` — one enum entry of a `GETPROPERTY` reply.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModePropertyEnum {
    pub value: u64,
    pub name: [u8; DRM_PROP_NAME_LEN],
}

/// `struct drm_mode_get_property`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeGetProperty {
    pub values_ptr: u64,
    pub enum_blob_ptr: u64,
    pub prop_id: u32,
    pub flags: u32,
    pub name: [u8; DRM_PROP_NAME_LEN],
    pub count_values: u32,
    pub count_enum_blobs: u32,
}

/// `struct drm_mode_create_blob`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeCreateBlob {
    pub data: u64,
    pub length: u32,
    pub blob_id: u32,
}

/// `struct drm_mode_destroy_blob`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeDestroyBlob {
    pub blob_id: u32,
}

/// `struct drm_mode_get_blob`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeGetBlob {
    pub blob_id: u32,
    pub length: u32,
    pub data: u64,
}

/// `struct drm_mode_fb_cmd2` (`ADDFB2`; `GETFB2` shares the struct).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeFbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub modifier: [u64; 4],
}

/// `struct drm_mode_atomic`. The four `*_ptr` fields reference userspace
/// arrays of `count_objs` / per-obj `count_props` entries; across RPC the
/// daemon receives them linearized (see `wire.rs`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeAtomic {
    pub flags: u32,
    pub count_objs: u32,
    pub objs_ptr: u64,
    pub count_props_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub reserved: u64,
    pub user_data: u64,
}

// ---------------------------------------------------------------------------
// object types (`DRM_MODE_OBJECT_*`)
// ---------------------------------------------------------------------------

pub const DRM_MODE_OBJECT_CRTC: u32 = 0xcccc_cccc;
pub const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
pub const DRM_MODE_OBJECT_ENCODER: u32 = 0xe0e0_e0e0;
pub const DRM_MODE_OBJECT_PROPERTY: u32 = 0xb0b0_b0b0;
pub const DRM_MODE_OBJECT_FB: u32 = 0xfbfb_fbfb;
pub const DRM_MODE_OBJECT_BLOB: u32 = 0xbbbb_bbbb;
pub const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;

// ---------------------------------------------------------------------------
// property flags (`DRM_MODE_PROP_*`)
// ---------------------------------------------------------------------------

pub const DRM_MODE_PROP_RANGE: u32 = 1 << 1;
pub const DRM_MODE_PROP_IMMUTABLE: u32 = 1 << 2;
pub const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
pub const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
pub const DRM_MODE_PROP_BITMASK: u32 = 1 << 5;
/// `DRM_MODE_PROP_TYPE(1)` — property value is another object's ID.
pub const DRM_MODE_PROP_OBJECT: u32 = 1 << 6;
/// `DRM_MODE_PROP_TYPE(2)` — signed [min, max] range (e.g. `IN_FENCE_FD`).
pub const DRM_MODE_PROP_SIGNED_RANGE: u32 = 2 << 6;
pub const DRM_MODE_PROP_ATOMIC: u32 = 0x8000_0000;

// ---------------------------------------------------------------------------
// mode flags / types
// ---------------------------------------------------------------------------

pub const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
pub const DRM_MODE_TYPE_DRIVER: u32 = 1 << 6;

// ---------------------------------------------------------------------------
// atomic flags (`DRM_MODE_ATOMIC_*`), page-flip flags
// ---------------------------------------------------------------------------

pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
pub const DRM_MODE_PAGE_FLIP_ASYNC: u32 = 0x02;
pub const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;
pub const DRM_MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
pub const DRM_MODE_ATOMIC_ALLOW_MODESET: u32 = 0x0400;

// ---------------------------------------------------------------------------
// connector / encoder types, connection status, plane types, subpixel
// (connection/plane-type/subpixel values come from libdrm's xf86drmMode.h —
// they are userspace conventions over the same u32 fields)
// ---------------------------------------------------------------------------

pub const DRM_MODE_CONNECTOR_HDMIA: u32 = 11;
pub const DRM_MODE_CONNECTOR_EDP: u32 = 14;
pub const DRM_MODE_CONNECTOR_DSI: u32 = 16;

pub const DRM_MODE_ENCODER_VIRTUAL: u32 = 5;

pub const DRM_MODE_CONNECTED: u32 = 1;
pub const DRM_MODE_DISCONNECTED: u32 = 2;
pub const DRM_MODE_UNKNOWNCONNECTION: u32 = 3;

pub const DRM_PLANE_TYPE_OVERLAY: u32 = 0;
pub const DRM_PLANE_TYPE_PRIMARY: u32 = 1;
pub const DRM_PLANE_TYPE_CURSOR: u32 = 2;

pub const DRM_MODE_SUBPIXEL_UNKNOWN: u32 = 1;
pub const DRM_MODE_SUBPIXEL_NONE: u32 = 6;

/// Connector `Colorspace` enum (`enum drm_colorspace`). gamescope writes
/// `BT2020_RGB` for HDR10 output and `DEFAULT` for SDR, and treats the
/// property's current value of `BT2020_RGB` as "HDR is active", so the
/// numeric values are load-bearing.
pub const DRM_MODE_COLORIMETRY_DEFAULT: u64 = 0;
pub const DRM_MODE_COLORIMETRY_SMPTE_170M_YCC: u64 = 1;
pub const DRM_MODE_COLORIMETRY_BT709_YCC: u64 = 2;
pub const DRM_MODE_COLORIMETRY_XVYCC_601: u64 = 3;
pub const DRM_MODE_COLORIMETRY_XVYCC_709: u64 = 4;
pub const DRM_MODE_COLORIMETRY_SYCC_601: u64 = 5;
pub const DRM_MODE_COLORIMETRY_OPYCC_601: u64 = 6;
pub const DRM_MODE_COLORIMETRY_OPRGB: u64 = 7;
pub const DRM_MODE_COLORIMETRY_BT2020_CYCC: u64 = 8;
pub const DRM_MODE_COLORIMETRY_BT2020_RGB: u64 = 9;
pub const DRM_MODE_COLORIMETRY_BT2020_YCC: u64 = 10;
pub const DRM_MODE_COLORIMETRY_DCI_P3_RGB_D65: u64 = 11;
pub const DRM_MODE_COLORIMETRY_DCI_P3_RGB_THEATER: u64 = 12;
pub const DRM_MODE_COLORIMETRY_RGB_WIDE_FIXED: u64 = 13;
pub const DRM_MODE_COLORIMETRY_RGB_WIDE_FLOAT: u64 = 14;
pub const DRM_MODE_COLORIMETRY_BT601_YCC: u64 = 15;

// ---------------------------------------------------------------------------
// FB flags / fourcc
// ---------------------------------------------------------------------------

pub const DRM_MODE_FB_MODIFIERS: u32 = 1 << 1;

/// `fourcc_code('X','R','2','4')` etc. — little-endian four ASCII chars.
#[must_use]
pub const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

pub const DRM_FORMAT_RGB565: u32 = fourcc(b'R', b'G', b'1', b'6');
pub const DRM_FORMAT_XRGB8888: u32 = fourcc(b'X', b'R', b'2', b'4');
pub const DRM_FORMAT_XBGR8888: u32 = fourcc(b'X', b'B', b'2', b'4');
pub const DRM_FORMAT_ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');
pub const DRM_FORMAT_ABGR8888: u32 = fourcc(b'A', b'B', b'2', b'4');
/// 10-bit 2:10:10:10 formats, the HDR output scan-out layout. gamescope asks
/// for `XRGB2101010` first for a 10-bit primary plane (then `XBGR2101010`),
/// exactly as it asks for the 8-bit pair — see `DRMBackend`'s plane-format pick.
/// The daemon deliberately offers only `XBGR2101010`: its R-low/B-high channels
/// match the `R10G10B10A2_UNORM` scan-out buffer, `XRGB2101010` is the opposite
/// order (see `PRIMARY_FORMATS_HDR` in `gndrm-daemon`).
pub const DRM_FORMAT_XRGB2101010: u32 = fourcc(b'X', b'R', b'3', b'0');
pub const DRM_FORMAT_XBGR2101010: u32 = fourcc(b'X', b'B', b'3', b'0');
/// The alpha-bearing 2:10:10:10 siblings (`AR30`/`AB30`) — same 32-bit word as
/// the `X` variants. Not advertised in `PRIMARY_FORMATS_HDR` (gamescope asks
/// for the `X` pair), so they are not reachable today; `format_bpp` names them
/// only so the table stays complete if a plane ever offers one.
pub const DRM_FORMAT_ARGB2101010: u32 = fourcc(b'A', b'R', b'3', b'0');
pub const DRM_FORMAT_ABGR2101010: u32 = fourcc(b'A', b'B', b'3', b'0');

/// `DRM_FORMAT_MOD_LINEAR`.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

// ---------------------------------------------------------------------------
// ioctl numbers
// ---------------------------------------------------------------------------

pub const DRM_IOCTL_MODE_GETRESOURCES: u32 = iowr::<DrmModeCardRes>(DRM_IOCTL_BASE, 0xA0);
pub const DRM_IOCTL_MODE_GETCRTC: u32 = iowr::<DrmModeCrtc>(DRM_IOCTL_BASE, 0xA1);
pub const DRM_IOCTL_MODE_SETCRTC: u32 = iowr::<DrmModeCrtc>(DRM_IOCTL_BASE, 0xA2);
pub const DRM_IOCTL_MODE_GETENCODER: u32 = iowr::<DrmModeGetEncoder>(DRM_IOCTL_BASE, 0xA6);
pub const DRM_IOCTL_MODE_GETCONNECTOR: u32 = iowr::<DrmModeGetConnector>(DRM_IOCTL_BASE, 0xA7);
pub const DRM_IOCTL_MODE_GETPROPERTY: u32 = iowr::<DrmModeGetProperty>(DRM_IOCTL_BASE, 0xAA);
pub const DRM_IOCTL_MODE_GETPROPBLOB: u32 = iowr::<DrmModeGetBlob>(DRM_IOCTL_BASE, 0xAC);
pub const DRM_IOCTL_MODE_RMFB: u32 = iowr::<u32>(DRM_IOCTL_BASE, 0xAF);
pub const DRM_IOCTL_MODE_GETPLANERESOURCES: u32 = iowr::<DrmModeGetPlaneRes>(DRM_IOCTL_BASE, 0xB5);
pub const DRM_IOCTL_MODE_GETPLANE: u32 = iowr::<DrmModeGetPlane>(DRM_IOCTL_BASE, 0xB6);
pub const DRM_IOCTL_MODE_ADDFB2: u32 = iowr::<DrmModeFbCmd2>(DRM_IOCTL_BASE, 0xB8);
pub const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: u32 =
    iowr::<DrmModeObjGetProperties>(DRM_IOCTL_BASE, 0xB9);
pub const DRM_IOCTL_MODE_OBJ_SETPROPERTY: u32 = iowr::<DrmModeObjSetProperty>(DRM_IOCTL_BASE, 0xBA);
pub const DRM_IOCTL_MODE_ATOMIC: u32 = iowr::<DrmModeAtomic>(DRM_IOCTL_BASE, 0xBC);
pub const DRM_IOCTL_MODE_CREATEPROPBLOB: u32 = iowr::<DrmModeCreateBlob>(DRM_IOCTL_BASE, 0xBD);
pub const DRM_IOCTL_MODE_DESTROYPROPBLOB: u32 = iowr::<DrmModeDestroyBlob>(DRM_IOCTL_BASE, 0xBE);
pub const DRM_IOCTL_MODE_GETFB2: u32 = iowr::<DrmModeFbCmd2>(DRM_IOCTL_BASE, 0xCE);

// ---------------------------------------------------------------------------
// layout assertions
// ---------------------------------------------------------------------------

const fn assert_size<T>(expected: usize) {
    assert!(
        core::mem::size_of::<T>() == expected,
        "struct size mismatch"
    );
}

const _: () = {
    assert_size::<DrmModeModeinfo>(68);
    assert_size::<DrmModeCardRes>(64);
    assert_size::<DrmModeCrtc>(104);
    assert_size::<DrmModeGetEncoder>(20);
    assert_size::<DrmModeGetConnector>(80);
    assert_size::<DrmModeGetPlaneRes>(16);
    assert_size::<DrmModeGetPlane>(32);
    assert_size::<DrmModeObjGetProperties>(32);
    assert_size::<DrmModeObjSetProperty>(24);
    assert_size::<DrmModePropertyEnum>(40);
    assert_size::<DrmModeGetProperty>(64);
    assert_size::<DrmModeCreateBlob>(16);
    assert_size::<DrmModeDestroyBlob>(4);
    assert_size::<DrmModeGetBlob>(16);
    assert_size::<DrmModeFbCmd2>(104);
    assert_size::<DrmModeAtomic>(56);
};
