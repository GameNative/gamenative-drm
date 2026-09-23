//! KMS object model: the fixed topology the daemon emulates, plus the
//! per-file object stores (GEM handles, framebuffers, property blobs).
//!
//! Topology (single-panel handheld, matching doc `02-gamenative-drm.md`):
//!
//! ```text
//! connector DSI-1 (CONNECTED) ── encoder (VIRTUAL) ── CRTC
//!                                                        ├── plane 1 (PRIMARY)
//!                                                        └── plane 2 (CURSOR)
//! ```
//!
//! Object-ID spaces are partitioned so an ID always identifies its kind
//! (kernel makes no such guarantee; it's a debugging convenience, and
//! `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` only requires device-wide uniqueness):
//!
//! | range        | kind                        |
//! |--------------|-----------------------------|
//! | 1..=0xFFF    | GEM handles                 |
//! | 0x1000..     | FB ids                      |
//! | 0x2000..     | property-blob ids           |
//! | 0x3000..     | property ids                |
//! | 0x40..=0x4F  | static topology object ids  |

use std::collections::HashMap;
use std::os::fd::RawFd;

use gndrm_uapi::mode::*;

pub const CONNECTOR_ID: u32 = 0x40;
pub const ENCODER_ID: u32 = 0x41;
pub const CRTC_ID: u32 = 0x42;
pub const PLANE_PRIMARY_ID: u32 = 0x43;
pub const PLANE_CURSOR_ID: u32 = 0x44;

const FIRST_GEM_HANDLE: u32 = 1;
const FIRST_FB_ID: u32 = 0x1000;
const FIRST_BLOB_ID: u32 = 0x2000;

/// Physical panel description, supplied by the Android side (surface size +
/// refresh) — phase 3 wires this to `ANativeWindow` metrics.
#[derive(Debug, Clone, Copy)]
pub struct PanelDesc {
    pub width: u16,
    pub height: u16,
    /// Refresh in millihertz (60 Hz = 60_000).
    pub refresh_mhz: u32,
    /// Physical size in millimetres (0 = unknown).
    pub mm_width: u32,
    pub mm_height: u32,
}

impl Default for PanelDesc {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            refresh_mhz: 60_000,
            mm_width: 0,
            mm_height: 0,
        }
    }
}

impl PanelDesc {
    /// Build the single preferred mode exposed on the connector.
    #[must_use]
    pub fn modeinfo(&self) -> DrmModeModeinfo {
        let hdisplay = self.width;
        let vdisplay = self.height;
        // Generic CVT-ish timings: porch values only matter for real panels;
        // gamescope reads hdisplay/vdisplay/vrefresh and nothing else.
        let hsync_start = hdisplay + 40;
        let hsync_end = hdisplay + 48;
        let htotal = hdisplay + 80;
        let vsync_start = vdisplay + 3;
        let vsync_end = vdisplay + 6;
        let vtotal = vdisplay + 20;
        // clock[kHz] = total_px * refresh / 1000, rounded.
        let clock = (u64::from(htotal) * u64::from(vtotal) * u64::from(self.refresh_mhz)
            / 1_000_000) as u32;
        let mut name = [0u8; DRM_DISPLAY_MODE_LEN];
        let label = format!("{}x{}", self.width, self.height);
        name[..label.len()].copy_from_slice(label.as_bytes());
        DrmModeModeinfo {
            clock,
            hdisplay,
            hsync_start,
            hsync_end,
            htotal,
            hskew: 0,
            vdisplay,
            vsync_start,
            vsync_end,
            vtotal,
            vscan: 0,
            vrefresh: self.refresh_mhz.div_ceil(1000),
            flags: 0,
            ty: DRM_MODE_TYPE_PREFERRED | DRM_MODE_TYPE_DRIVER,
            name,
        }
    }
}

/// One imported dma-buf (`PRIME_FD_TO_HANDLE`): we keep a dup of the fd and
/// the identity needed for dedup (`stat.st_ino`, per doc §5 — same buffer
/// re-imported by gamescope must return the same handle).
#[derive(Debug)]
pub struct GemObject {
    pub fd: RawFd,
    pub ino: u64,
    pub size: u64,
}

/// A registered framebuffer (`ADDFB2`).
#[derive(Debug, Clone)]
pub struct Framebuffer {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub modifier: [u64; 4],
}

/// A property blob (`CREATEPROPBLOB`); phase 1: arbitrary bytes + the
/// MODE_ID `DrmModeModeinfo` payload.
#[derive(Debug, Clone)]
pub struct Blob {
    pub id: u32,
    pub data: Vec<u8>,
}

/// Plane software state updated by commits.
#[derive(Debug, Clone, Default)]
pub struct PlaneState {
    pub fb_id: u32,
    pub crtc_id: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: i32,
    pub crtc_h: i32,
}

/// CRTC software state.
#[derive(Debug, Clone, Default)]
pub struct CrtcState {
    pub active: bool,
    pub mode_id: u32,
}

/// All mutable object stores. Split from `Device` so the commit path can
/// borrow stores while the presenter is called.
#[derive(Debug)]
pub struct Stores {
    pub gems: HashMap<u32, GemObject>,
    /// `stat.st_ino` → GEM handle (dedup index).
    pub gem_by_ino: HashMap<u64, u32>,
    pub fbs: HashMap<u32, Framebuffer>,
    pub blobs: HashMap<u32, Blob>,
    next_gem: u32,
    next_fb: u32,
    next_blob: u32,
}

impl Default for Stores {
    fn default() -> Self {
        Self {
            gems: HashMap::new(),
            gem_by_ino: HashMap::new(),
            fbs: HashMap::new(),
            blobs: HashMap::new(),
            next_gem: FIRST_GEM_HANDLE,
            next_fb: FIRST_FB_ID,
            next_blob: FIRST_BLOB_ID,
        }
    }
}

impl Stores {
    pub fn alloc_gem(&mut self) -> u32 {
        let h = self.next_gem;
        self.next_gem += 1;
        assert!(self.next_gem < FIRST_FB_ID, "GEM handle space exhausted");
        h
    }

    pub fn alloc_fb(&mut self) -> u32 {
        let id = self.next_fb;
        self.next_fb += 1;
        assert!(self.next_fb < FIRST_BLOB_ID, "FB id space exhausted");
        id
    }

    pub fn alloc_blob(&mut self) -> u32 {
        let id = self.next_blob;
        self.next_blob += 1;
        id
    }
}
