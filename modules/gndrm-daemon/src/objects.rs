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
//! | 0x1000..0x1FFF | FB ids                    |
//! | 0x2000..0x2FFF | property-blob ids         |
//! | 0x3000..0x3FFF | property ids            |
//! | 0x4000..0x4FFF | syncobj handles         |
//! | 0x40..=0x4F  | static topology object ids  |

use std::collections::HashMap;
use std::os::fd::RawFd;

use gndrm_uapi::mode::*;
use gndrm_uapi::wire::errno;

pub const CONNECTOR_ID: u32 = 0x40;
pub const ENCODER_ID: u32 = 0x41;
pub const CRTC_ID: u32 = 0x42;
pub const PLANE_PRIMARY_ID: u32 = 0x43;
pub const PLANE_CURSOR_ID: u32 = 0x44;

const FIRST_GEM_HANDLE: u32 = 1;
/// Exclusive end of the GEM handle space (== `FIRST_FB_ID`).
const END_GEM_HANDLE: u32 = 0x1000;
const FIRST_FB_ID: u32 = 0x1000;
/// Exclusive end of the FB id space (== `FIRST_BLOB_ID`).
const END_FB_ID: u32 = 0x2000;
const FIRST_BLOB_ID: u32 = 0x2000;
/// Exclusive end of the blob id space (property ids start at 0x3000).
const END_BLOB_ID: u32 = 0x3000;
// Sync objects get their own space: the old 0x3000 base collided with the
// property ids (also 0x3000), which the module's "an id identifies its kind"
// rule does not allow.
const FIRST_SYNCOBJ_HANDLE: u32 = 0x4000;
/// Exclusive end of the syncobj handle space.
const END_SYNCOBJ_HANDLE: u32 = 0x5000;

/// The session's advertised refresh rate, in millihertz (1200 Hz = 1_200_000).
///
/// gamescope builds its entire vblank grid from the exposed mode's `vrefresh`
/// and honours only integer divisors of it for an fps cap, so this one value
/// sets both the guest's uncapped ceiling and every cap's granularity. It is
/// deliberately high: the grid is a timer, so a finite value is required, and
/// 1200 Hz is ~4.6x the present pipeline's measured throughput (~259
/// presents/s), so it cannot bind. The only real cap on the guest is the user's
/// own fps limit in the QuickMenu; the physical panel is downstream and
/// coalesces whatever the session sends.
pub const SESSION_REFRESH_MHZ: u32 = 1_200_000;

/// Physical panel description: the surface size the session presents into. The
/// session refresh rate is the daemon's own constant (`SESSION_REFRESH_MHZ`),
/// not a caller-supplied value.
#[derive(Debug, Clone, Copy)]
pub struct PanelDesc {
    pub width: u16,
    pub height: u16,
    /// Physical size in millimetres (0 = unknown).
    pub mm_width: u32,
    pub mm_height: u32,
}

impl Default for PanelDesc {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
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
        let clock = (u64::from(htotal) * u64::from(vtotal) * u64::from(SESSION_REFRESH_MHZ)
            / 1_000_000) as u32;
        let mut name = [0u8; DRM_DISPLAY_MODE_LEN];
        let label = format!("{}", gndrm_obf::obf!("{}x{}", self.width, self.height));
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
            vrefresh: SESSION_REFRESH_MHZ.div_ceil(1000),
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
/// A sync object. Only the always-signalled form is modelled (see the UAPI
/// comment): the export fd is an eventfd holding a count, so a `poll` on a dup
/// of it is ready immediately — which is exactly what an always-signalled
/// fence means to the presenter.
pub struct Syncobj {
    pub signaled: bool,
    pub export_fd: Option<RawFd>,
}

pub struct Stores {
    pub syncobjs: HashMap<u32, Syncobj>,
    pub gems: HashMap<u32, GemObject>,
    /// `stat.st_ino` → GEM handle (dedup index).
    pub gem_by_ino: HashMap<u64, u32>,
    /// Handle-closed GEMs still referenced by a live FB (kernel semantics:
    /// GEM_CLOSE drops the handle's reference; the object dies with the
    /// last FB referencing it). Keyed by the (now-invalid) handle.
    pub gems_closed: HashMap<u32, GemObject>,
    pub fbs: HashMap<u32, Framebuffer>,
    pub blobs: HashMap<u32, Blob>,
    /// Freed ids, one list per space, handed back out before the monotonic
    /// counter advances. Without this the space is bounded by the number of
    /// ids *ever* allocated, so a game churning framebuffers (Nidhogg) runs it
    /// out and every later allocation fails — the on-device abort after ~216 s.
    /// With it the space is bounded by the number of *simultaneously-live*
    /// objects.
    free_gems: Vec<u32>,
    free_fbs: Vec<u32>,
    free_blobs: Vec<u32>,
    free_syncobjs: Vec<u32>,
    next_gem: u32,
    next_fb: u32,
    next_blob: u32,
    next_syncobj: u32,
}

impl Default for Stores {
    fn default() -> Self {
        Self {
            syncobjs: HashMap::new(),
            gems: HashMap::new(),
            gem_by_ino: HashMap::new(),
            gems_closed: HashMap::new(),
            fbs: HashMap::new(),
            blobs: HashMap::new(),
            free_gems: Vec::new(),
            free_fbs: Vec::new(),
            free_blobs: Vec::new(),
            free_syncobjs: Vec::new(),
            next_gem: FIRST_GEM_HANDLE,
            next_fb: FIRST_FB_ID,
            next_blob: FIRST_BLOB_ID,
            next_syncobj: FIRST_SYNCOBJ_HANDLE,
        }
    }
}

impl Stores {
    /// Next-never-allocated id per space (`[gem, fb, blob, syncobj]`): the
    /// high-water mark of what the session needed at once. A resource leak
    /// shows up here as an unbounded rise across churn.
    #[must_use]
    pub fn watermarks(&self) -> [u32; 4] {
        [
            self.next_gem,
            self.next_fb,
            self.next_blob,
            self.next_syncobj,
        ]
    }

    /// Allocate a GEM handle, reusing a freed id when one is available.
    ///
    /// # Errors
    /// `ENOSPC` when every id in the space is live (the client is not closing
    /// handles). Returning the errno keeps the app alive; the old `assert!`
    /// aborted the whole process.
    pub fn alloc_gem(&mut self) -> Result<u32, i32> {
        alloc_within(&mut self.free_gems, &mut self.next_gem, END_GEM_HANDLE)
    }

    /// Return `h` to the GEM handle space. Call only once the object it named
    /// is truly gone (closed and unreferenced, or its last FB removed).
    pub fn free_gem(&mut self, h: u32) {
        free_within(&mut self.free_gems, h, FIRST_GEM_HANDLE..END_GEM_HANDLE);
    }

    /// Allocate an FB id, reusing a freed id when one is available.
    ///
    /// # Errors
    /// `ENOSPC` when every fb id is live.
    pub fn alloc_fb(&mut self) -> Result<u32, i32> {
        alloc_within(&mut self.free_fbs, &mut self.next_fb, END_FB_ID)
    }

    /// Return `id` to the FB id space (called by `RMFB`).
    pub fn free_fb(&mut self, id: u32) {
        free_within(&mut self.free_fbs, id, FIRST_FB_ID..END_FB_ID);
    }

    /// Allocate a sync-object handle, reusing a freed id when one is available.
    ///
    /// # Errors
    /// `ENOSPC` when every syncobj handle is live.
    pub fn alloc_syncobj(&mut self) -> Result<u32, i32> {
        alloc_within(
            &mut self.free_syncobjs,
            &mut self.next_syncobj,
            END_SYNCOBJ_HANDLE,
        )
    }

    /// Return `id` to the syncobj handle space (`SYNCOBJ_DESTROY`).
    pub fn free_syncobj(&mut self, id: u32) {
        free_within(
            &mut self.free_syncobjs,
            id,
            FIRST_SYNCOBJ_HANDLE..END_SYNCOBJ_HANDLE,
        );
    }

    /// Allocate a property-blob id, reusing a freed id when one is available.
    ///
    /// # Errors
    /// `ENOSPC` when every blob id is live.
    pub fn alloc_blob(&mut self) -> Result<u32, i32> {
        alloc_within(&mut self.free_blobs, &mut self.next_blob, END_BLOB_ID)
    }

    /// Return `id` to the blob id space (`DESTROYPROPBLOB`).
    pub fn free_blob(&mut self, id: u32) {
        free_within(&mut self.free_blobs, id, FIRST_BLOB_ID..END_BLOB_ID);
    }
}

/// Take a freed id if one is queued, else mint the next never-used one.
/// `ENOSPC` once the space is exhausted — never a panic.
fn alloc_within(free: &mut Vec<u32>, next: &mut u32, end: u32) -> Result<u32, i32> {
    if let Some(id) = free.pop() {
        return Ok(id);
    }
    if *next >= end {
        return Err(errno::ENOSPC);
    }
    let id = *next;
    *next += 1;
    Ok(id)
}

/// Queue `id` for reuse, ignoring anything outside the space (a foreign id must
/// never poison the free list and be handed out as a live object id).
fn free_within(free: &mut Vec<u32>, id: u32, range: core::ops::Range<u32>) {
    if range.contains(&id) {
        free.push(id);
    }
}
