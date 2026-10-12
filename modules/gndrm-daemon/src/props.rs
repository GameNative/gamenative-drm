//! Property table: every KMS property the daemon advertises, matching doc
//! `02-gamenative-drm.md` §3.2. Anything gamescope merely *probes* is simply
//! absent — it then takes its own fallback path (doc `01` finding 5).
//!
//! The connector carries `EDID`, `Colorspace` and `HDR_OUTPUT_METADATA`
//! because gamescope's HDR10 path is gated on their *existence*:
//! `CDRMConnector::SupportsHDR10()` is
//! `!!Colorspace && !!HDR_OUTPUT_METADATA && GetHDRInfo().IsHDR10()`, and
//! `ParseEDID()` early-returns without an `EDID` property. Their *content* is
//! per-session: the EDID is built from the display capability (no HDR static
//! metadata block on an SDR panel), so the properties existing on an SDR
//! session is harmless — `IsHDR10()` stays false.
//!
//! Deliberately **not** exposed:
//! - `OUT_FENCE_PTR`, `VRR_ENABLED`, `CONTENT PROTECTION` (gamescope takes its
//!   own fallbacks when absent)
//! - plane `rotation` (omitted ⇒ gamescope composites rotation itself and the
//!   daemon always presents panel-orientation buffers — doc `02` open Q5)
//! - plane `zpos`/`alpha` (single primary + cursor; compositing order fixed)

use gndrm_uapi::mode::*;

use crate::objects::{CONNECTOR_ID, CRTC_ID, PLANE_CURSOR_ID, PLANE_PRIMARY_ID};

/// Property ids: allocated sequentially from this base at table build time.
const FIRST_PROP_ID: u32 = 0x3000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    /// `DRM_MODE_PROP_RANGE`, values = \[min, max\].
    Range(u64, u64),
    /// `DRM_MODE_PROP_SIGNED_RANGE`, values = \[min, max\] as i64
    /// bit-patterns (kernel: `IN_FENCE_FD` is `(-1, INT_MAX)`).
    SignedRange(i64, i64),
    /// `DRM_MODE_PROP_ENUM`, values + names.
    Enum(&'static [(u64, &'static str)]),
    /// `DRM_MODE_PROP_BLOB`.
    Blob,
    /// `DRM_MODE_PROP_OBJECT`, values = \[0, u32::MAX\].
    Object,
}

/// The kernel's connector `Colorspace` enum, names included (kernel
/// `drm_connector.c`). Only the numeric values matter to gamescope; the names
/// keep `GETPROPERTY` truthful for any other client.
pub static COLORSPACE_VALUES: &[(u64, &str)] = &[
    (DRM_MODE_COLORIMETRY_DEFAULT, "Default"),
    (DRM_MODE_COLORIMETRY_SMPTE_170M_YCC, "SMPTE_170M_YCC"),
    (DRM_MODE_COLORIMETRY_BT709_YCC, "BT709_YCC"),
    (DRM_MODE_COLORIMETRY_XVYCC_601, "XVYCC_601"),
    (DRM_MODE_COLORIMETRY_XVYCC_709, "XVYCC_709"),
    (DRM_MODE_COLORIMETRY_SYCC_601, "SYCC_601"),
    (DRM_MODE_COLORIMETRY_OPYCC_601, "opYCC_601"),
    (DRM_MODE_COLORIMETRY_OPRGB, "opRGB"),
    (DRM_MODE_COLORIMETRY_BT2020_CYCC, "BT2020_CYCC"),
    (DRM_MODE_COLORIMETRY_BT2020_RGB, "BT2020_RGB"),
    (DRM_MODE_COLORIMETRY_BT2020_YCC, "BT2020_YCC"),
    (DRM_MODE_COLORIMETRY_DCI_P3_RGB_D65, "DCI-P3_RGB_D65"),
    (
        DRM_MODE_COLORIMETRY_DCI_P3_RGB_THEATER,
        "DCI-P3_RGB_Theater",
    ),
    (DRM_MODE_COLORIMETRY_RGB_WIDE_FIXED, "RGB_WIDE_FIXED"),
    (DRM_MODE_COLORIMETRY_RGB_WIDE_FLOAT, "RGB_WIDE_FLOAT"),
    (DRM_MODE_COLORIMETRY_BT601_YCC, "BT601_YCC"),
];

/// The static property table. Indices are load-bearing: the `*_PROPS`
/// index lists below refer into this slice, and ids are `FIRST_PROP_ID + index`.
#[rustfmt::skip]
pub static PROPS: &[(&str, PropKind, bool)] = &[
    // --- connector ---
    ("CRTC_ID",           PropKind::Object, false),
    ("panel orientation", PropKind::Enum(&[(0, "Normal"), (1, "Upside Down"), (2, "Left Side Up"), (3, "Right Side Up")]), true),
    // --- CRTC ---
    ("ACTIVE",            PropKind::Range(0, 1), false),
    ("MODE_ID",           PropKind::Blob, false),
    // --- planes (both) ---
    ("type",              PropKind::Enum(&[(DRM_PLANE_TYPE_OVERLAY as u64, "Overlay"), (DRM_PLANE_TYPE_PRIMARY as u64, "Primary"), (DRM_PLANE_TYPE_CURSOR as u64, "Cursor")]), true),
    ("FB_ID",             PropKind::Object, false),
    ("CRTC_ID",           PropKind::Object, false),
    ("SRC_X",             PropKind::Range(0, u32::MAX as u64), false),
    ("SRC_Y",             PropKind::Range(0, u32::MAX as u64), false),
    ("SRC_W",             PropKind::Range(0, u32::MAX as u64), false),
    ("SRC_H",             PropKind::Range(0, u32::MAX as u64), false),
    ("CRTC_X",            PropKind::Range(0, u32::MAX as u64), false),
    ("CRTC_Y",            PropKind::Range(0, u32::MAX as u64), false),
    ("CRTC_W",            PropKind::Range(0, u32::MAX as u64), false),
    ("CRTC_H",            PropKind::Range(0, u32::MAX as u64), false),
    ("IN_FENCE_FD",       PropKind::SignedRange(-1, i32::MAX as i64), false),
    ("IN_FORMATS",        PropKind::Blob, true),
    // --- connector HDR (must exist for gamescope to consider HDR10 at all) ---
    ("EDID",              PropKind::Blob, true),
    ("Colorspace",        PropKind::Enum(COLORSPACE_VALUES), false),
    ("HDR_OUTPUT_METADATA", PropKind::Blob, false),
];

// Index constants matching the table above (compile-time checked below).
pub const P_CONN_CRTC_ID: usize = 0;
pub const P_CONN_PANEL_ORIENTATION: usize = 1;
pub const P_CRTC_ACTIVE: usize = 2;
pub const P_CRTC_MODE_ID: usize = 3;
pub const P_PLANE_TYPE: usize = 4;
pub const P_PLANE_FB_ID: usize = 5;
pub const P_PLANE_CRTC_ID: usize = 6;
pub const P_PLANE_SRC_X: usize = 7;
pub const P_PLANE_SRC_Y: usize = 8;
pub const P_PLANE_SRC_W: usize = 9;
pub const P_PLANE_SRC_H: usize = 10;
pub const P_PLANE_CRTC_X: usize = 11;
pub const P_PLANE_CRTC_Y: usize = 12;
pub const P_PLANE_CRTC_W: usize = 13;
pub const P_PLANE_CRTC_H: usize = 14;
pub const P_PLANE_IN_FENCE_FD: usize = 15;
pub const P_PLANE_IN_FORMATS: usize = 16;
pub const P_CONN_EDID: usize = 17;
pub const P_CONN_COLORSPACE: usize = 18;
pub const P_CONN_HDR_METADATA: usize = 19;

pub static CONNECTOR_PROPS: &[usize] = &[
    P_CONN_CRTC_ID,
    P_CONN_PANEL_ORIENTATION,
    P_CONN_EDID,
    P_CONN_COLORSPACE,
    P_CONN_HDR_METADATA,
];
pub static CRTC_PROPS: &[usize] = &[P_CRTC_ACTIVE, P_CRTC_MODE_ID];
pub static PLANE_PROPS: &[usize] = &[
    P_PLANE_TYPE,
    P_PLANE_FB_ID,
    P_PLANE_CRTC_ID,
    P_PLANE_SRC_X,
    P_PLANE_SRC_Y,
    P_PLANE_SRC_W,
    P_PLANE_SRC_H,
    P_PLANE_CRTC_X,
    P_PLANE_CRTC_Y,
    P_PLANE_CRTC_W,
    P_PLANE_CRTC_H,
    P_PLANE_IN_FENCE_FD,
    P_PLANE_IN_FORMATS,
];

/// All objects that carry properties, for `OBJ_GETPROPERTIES` dispatch.
#[must_use]
pub fn obj_list() -> [(u32, u32, &'static [usize]); 4] {
    [
        (CONNECTOR_ID, DRM_MODE_OBJECT_CONNECTOR, CONNECTOR_PROPS),
        (CRTC_ID, DRM_MODE_OBJECT_CRTC, CRTC_PROPS),
        (PLANE_PRIMARY_ID, DRM_MODE_OBJECT_PLANE, PLANE_PROPS),
        (PLANE_CURSOR_ID, DRM_MODE_OBJECT_PLANE, PLANE_PROPS),
    ]
}

/// Property id for a table index.
#[must_use]
pub fn prop_id(index: usize) -> u32 {
    FIRST_PROP_ID + index as u32
}

/// Table index for a property id, if valid.
#[must_use]
pub fn prop_index(id: u32) -> Option<usize> {
    let idx = id.checked_sub(FIRST_PROP_ID)? as usize;
    (idx < PROPS.len()).then_some(idx)
}

/// Resolve `(obj_id, prop_name)` → property id (public: integration tests,
/// the shim contract documentation, and bring-up debugging all key off
/// names, exactly like `drmModeGetProperty` users do).
#[must_use]
pub fn find_prop_id(obj_id: u32, name: &str) -> Option<u32> {
    obj_list()
        .iter()
        .find(|(oid, _, _)| *oid == obj_id)?
        .2
        .iter()
        .find(|&&i| PROPS[i].0 == name)
        .map(|&i| prop_id(i))
}

/// Resolve `(obj_id, prop_id)` → table index, validating the property
/// actually belongs to that object.
#[must_use]
pub fn resolve(obj_id: u32, id: u32) -> Option<usize> {
    let idx = prop_index(id)?;
    obj_list()
        .iter()
        .find(|(oid, _, _)| *oid == obj_id)
        .and_then(|(_, _, props)| props.contains(&idx).then_some(idx))
}

/// Whether `value` is a member of an enum property's value list.
#[must_use]
pub fn enum_contains(index: usize, value: u64) -> bool {
    match PROPS[index].1 {
        PropKind::Enum(entries) => entries.iter().any(|(v, _)| *v == value),
        _ => false,
    }
}

const _: () = assert!(PROPS.len() == 20);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_constants_match_table() {
        // Guards against table/index drift when editing PROPS.
        assert_eq!(PROPS[P_CONN_CRTC_ID].0, "CRTC_ID");
        assert_eq!(PROPS[P_CONN_PANEL_ORIENTATION].0, "panel orientation");
        assert_eq!(PROPS[P_CONN_EDID].0, "EDID");
        assert_eq!(PROPS[P_CONN_COLORSPACE].0, "Colorspace");
        assert_eq!(PROPS[P_CONN_HDR_METADATA].0, "HDR_OUTPUT_METADATA");
        assert_eq!(PROPS[P_CRTC_ACTIVE].0, "ACTIVE");
        assert_eq!(PROPS[P_CRTC_MODE_ID].0, "MODE_ID");
        assert_eq!(PROPS[P_PLANE_TYPE].0, "type");
        assert_eq!(PROPS[P_PLANE_FB_ID].0, "FB_ID");
        assert_eq!(PROPS[P_PLANE_IN_FENCE_FD].0, "IN_FENCE_FD");
    }

    #[test]
    fn colorspace_enum_has_the_values_gamescope_writes() {
        // gamescope writes DEFAULT (SDR) and BT2020_RGB (HDR10), and compares
        // the current value against BT2020_RGB to decide HDR is active.
        assert!(enum_contains(
            P_CONN_COLORSPACE,
            DRM_MODE_COLORIMETRY_DEFAULT
        ));
        assert!(enum_contains(
            P_CONN_COLORSPACE,
            DRM_MODE_COLORIMETRY_BT2020_RGB
        ));
        assert!(!enum_contains(P_CONN_COLORSPACE, 999));
    }
}
