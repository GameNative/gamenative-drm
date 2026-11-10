//! # gndrm-daemon
//!
//! KMS emulator core. Fed decoded wire frames by the socket layer
//! (`gndrm-android`), turns each into the exact byte stream the kernel would
//! have written, plus optional fd returns and flip events.
//!
//! The crate is platform-agnostic (host-testable): the only syscalls it makes
//! are `fstat`/`dup`/`close`/`clock_gettime` on fds the wire delivers, plus
//! `eventfd2` for sync-object exports (a signalled fence is an eventfd holding
//! a count, so `poll` on any dup of it is ready immediately).

// Scoped unsafe lives in `pod` (audited) and in the fd helpers below.
#![deny(clippy::all)]

mod commit;
pub mod cursor;
pub mod diag;
mod edid;
pub mod foreign;
pub mod frame_cap;
mod objects;
#[doc(hidden)] // exposed for integration tests; not public API
pub mod pod;
mod props;
pub mod pushes;
pub mod scanout;
pub mod server;
pub mod vblank;

use std::collections::{HashSet, VecDeque};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use gndrm_uapi::drm::*;
use gndrm_uapi::mode::*;
use gndrm_uapi::wire::{errno, ScanoutLayout, SCANOUT_LAYOUT_ENV};

pub use frame_cap::FrameCap;
pub use objects::{CrtcState, Framebuffer, GemObject, PanelDesc, PlaneState, Stores};
pub use objects::{
    CONNECTOR_ID, CRTC_ID, ENCODER_ID, PLANE_CURSOR_ID, PLANE_PRIMARY_ID, UNKNOWN_PANEL_HZ,
};
pub use props::find_prop_id;

use objects::{Blob, Syncobj};
use pod::*;

use crate::diag::{diag, Role, State};

/// Driver identity reported by `DRM_IOCTL_VERSION`.
const DRIVER_NAME: &[u8] = b"gamenative";
const DRIVER_DATE: &[u8] = b"20260923";
const DRIVER_DESC: &[u8] = b"gamenative userspace KMS";
const DRIVER_VERSION: (i32, i32, i32) = (1, 0, 0);

/// Panel size limits reported by `GETRESOURCES`. The floor is 1, not a plane
/// quirk: the kernel imposes no minimum on a `drm_mode_fb_cmd2`, and gamescope
/// creates its cursor framebuffers from a cropped image (observed 10x16 and
/// 16x16 on device), which a larger floor rejects outright.
const MIN_WIDTH: u32 = 1;
const MIN_HEIGHT: u32 = 1;
const MAX_WIDTH: u32 = 8192;
const MAX_HEIGHT: u32 = 8192;
/// Cursor plane size cap (`DRM_CAP_CURSOR_WIDTH`/`_HEIGHT`): the largest
/// source and destination rectangle a cursor-plane commit may carry, as a
/// kernel driver enforces. Without the limit libliftoff can park a full-size
/// game buffer on the cursor plane whenever its format is listed there, and
/// the presenter publishes only the primary plane.
pub(crate) const CURSOR_SIZE: u32 = 64;
/// Cursor size cap for a client holding [`GNDRM_CLIENT_CAP_CURSOR_LAYER`],
/// whose cursor plane is presented on a layer of its own. Android exposes no
/// query for the composer's cursor limits, so this is the largest cursor
/// gamescope ever builds (`MouseCursor::GetDesiredSize` clamps to 256). Any
/// framebuffer or rectangle larger than this is refused on the cursor plane, so
/// libliftoff can never park a game-sized buffer there.
pub const CURSOR_LAYER_MAX: u32 = 256;

/// Formats on the primary plane for a [`ScanoutLayout::Bgra8888`] pool (the SDR
/// set). `XRGB8888` stays first: gamescope renders its output in the first
/// 8-bit format it prefers, and `XRGB8888` is B,G,R,X in memory like the pool.
const PRIMARY_FORMATS_BGRA_SDR: &[u32] = &[
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_RGB565,
];
/// Formats on the primary plane for a [`ScanoutLayout::Rgba8888`] pool (the SDR
/// set). `XBGR8888` stays first.
///
/// Only R-low 8-bit formats: the pool's AHB is `RGBA_8888` (R,G,B,A in
/// memory), which is `XBGR8888`/`ABGR8888`. `XRGB8888`/`ARGB8888` are B-low and
/// would reach the panel with red and blue exchanged; gamescope falls back to
/// `XBGR8888` for its output when the primary plane offers no `XRGB8888`.
const PRIMARY_FORMATS_RGBA_SDR: &[u32] =
    &[DRM_FORMAT_XBGR8888, DRM_FORMAT_ABGR8888, DRM_FORMAT_RGB565];
/// An SDR set plus the 2:10:10:10 HDR scan-out format, advertised only when the
/// container asked for HDR output.
///
/// gamescope's plane-format pick prefers `XRGB2101010` over `XBGR2101010` and
/// does NOT gate that on `--hdr-enabled`: once the plane offers a 10-bit format
/// gamescope scans out in it unconditionally. Two consequences shape this list:
///
/// * the 10-bit entry cannot be appended to a flat list — that moves the SDR
///   path onto a 10-bit buffer too, which the presenter/pool describe as
///   the 8-bit layout. So the set is gated on [`hdr_output_enabled`].
/// * only `XBGR2101010` is offered, never `XRGB2101010`. The pool's AHB is
///   `R10G10B10A2_UNORM` (`RGBA_1010102`), whose channels are R-low/B-high;
///   gamescope's `XBGR2101010` (`xB:G:R`, R-low) matches it, while
///   `XRGB2101010` (`x:R:G:B`, B-low) is the opposite order and reaches the
///   panel with red and blue exchanged. Offering both would let gamescope pick
///   the mismatched one.
const PRIMARY_FORMATS_BGRA_HDR: &[u32] = &[
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_RGB565,
    DRM_FORMAT_XBGR2101010,
];
/// [`PRIMARY_FORMATS_RGBA_SDR`] plus `XBGR2101010`; see
/// [`PRIMARY_FORMATS_BGRA_HDR`].
const PRIMARY_FORMATS_RGBA_HDR: &[u32] = &[
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_RGB565,
    DRM_FORMAT_XBGR2101010,
];

/// Cached `GNDRM_LOG_DAEMON` verdict (see [`verbose_logging`]).
static VERBOSE_GATE: AtomicU8 = AtomicU8::new(VERBOSE_UNRESOLVED);
const VERBOSE_UNRESOLVED: u8 = 0;
const VERBOSE_OFF: u8 = 1;
const VERBOSE_ON: u8 = 2;

/// The primary plane's format list for the current session.
fn primary_formats() -> &'static [u32] {
    primary_formats_for(hdr_output_enabled(), scanout_layout())
}

/// Pure core of [`primary_formats`], so the HDR and layout branches are
/// testable without mutating the process environment.
fn primary_formats_for(hdr: bool, layout: ScanoutLayout) -> &'static [u32] {
    match (layout, hdr) {
        (ScanoutLayout::Bgra8888, false) => PRIMARY_FORMATS_BGRA_SDR,
        (ScanoutLayout::Bgra8888, true) => PRIMARY_FORMATS_BGRA_HDR,
        (ScanoutLayout::Rgba8888, false) => PRIMARY_FORMATS_RGBA_SDR,
        (ScanoutLayout::Rgba8888, true) => PRIMARY_FORMATS_RGBA_HDR,
    }
}

/// Cached session [`ScanoutLayout`] (see [`scanout_layout`]).
static LAYOUT_GATE: AtomicU8 = AtomicU8::new(LAYOUT_UNRESOLVED);
/// The layout the presentation layer selected for this session
/// ([`select_scanout_layout`]), used when the environment names none.
static LAYOUT_SELECTED: AtomicU8 = AtomicU8::new(LAYOUT_UNRESOLVED);
const LAYOUT_UNRESOLVED: u8 = 0;
const LAYOUT_BGRA: u8 = 1;
const LAYOUT_RGBA: u8 = 2;

const fn layout_code(layout: ScanoutLayout) -> u8 {
    match layout {
        ScanoutLayout::Bgra8888 => LAYOUT_BGRA,
        ScanoutLayout::Rgba8888 => LAYOUT_RGBA,
    }
}

const fn layout_from_code(code: u8) -> Option<ScanoutLayout> {
    match code {
        LAYOUT_BGRA => Some(ScanoutLayout::Bgra8888),
        LAYOUT_RGBA => Some(ScanoutLayout::Rgba8888),
        _ => None,
    }
}

/// The layout `GNDRM_SCANOUT_LAYOUT` names in this process, if any.
#[must_use]
pub fn scanout_layout_from_env() -> Option<ScanoutLayout> {
    std::env::var(SCANOUT_LAYOUT_ENV)
        .ok()
        .and_then(|v| ScanoutLayout::from_env_value(&v))
}

/// The scan-out pool's layout for this session: `GNDRM_SCANOUT_LAYOUT` when it
/// names one, else the layout the presentation layer selected at session start
/// ([`select_scanout_layout`]), else [`ScanoutLayout::Bgra8888`]. Resolved
/// lazily and cached like [`hdr_output_enabled`]; [`reset_scanout_layout`]
/// drops both slots at session start.
#[must_use]
pub fn scanout_layout() -> ScanoutLayout {
    if let Some(layout) = layout_from_code(LAYOUT_GATE.load(Ordering::Relaxed)) {
        return layout;
    }
    let layout = scanout_layout_from_env()
        .or_else(|| layout_from_code(LAYOUT_SELECTED.load(Ordering::Relaxed)))
        .unwrap_or(ScanoutLayout::Bgra8888);
    LAYOUT_GATE.store(layout_code(layout), Ordering::Relaxed);
    layout
}

/// Record the layout the presentation layer chose for this session. Takes
/// effect unless `GNDRM_SCANOUT_LAYOUT` names one.
pub fn select_scanout_layout(layout: ScanoutLayout) {
    LAYOUT_SELECTED.store(layout_code(layout), Ordering::Relaxed);
    LAYOUT_GATE.store(LAYOUT_UNRESOLVED, Ordering::Relaxed);
}

/// Drop the cached [`scanout_layout`] and the selected layout at session start.
pub fn reset_scanout_layout() {
    LAYOUT_SELECTED.store(LAYOUT_UNRESOLVED, Ordering::Relaxed);
    LAYOUT_GATE.store(LAYOUT_UNRESOLVED, Ordering::Relaxed);
}

/// Whether the session asked for HDR output, so the plane may offer the 10-bit
/// scan-out formats.
///
/// The environment (`GNDRM_HDR`) is the source of the flag: the launcher sets it
/// in this process (the daemon, the pool and the presenter share one address
/// space) when it builds the gamescope command. That happens after the daemon
/// process starts but before the guest forks, and the first call that reads this
/// (the plane's format list, built with the guest's first plane query) lands
/// after the set, so the value is resolved lazily on first use rather than
/// cached at init. It is served from a cached slot thereafter, because the
/// environment lookup takes a lock and one caller is on the per-published-buffer
/// path. [`reset_hdr_output_cache`] drops the slot at session start so a later
/// session re-reads the environment. Any value other than a set/`1`/`true` is
/// treated as off.
#[must_use]
pub fn hdr_output_enabled() -> bool {
    match HDR_GATE.load(Ordering::Relaxed) {
        HDR_ON => true,
        HDR_OFF => false,
        _ => {
            let on = matches!(
                std::env::var_os("GNDRM_HDR"),
                Some(v) if !v.is_empty() && v != "0"
            );
            HDR_GATE.store(if on { HDR_ON } else { HDR_OFF }, Ordering::Relaxed);
            on
        }
    }
}

/// Drop the cached [`hdr_output_enabled`] verdict so the next call re-reads
/// `GNDRM_HDR`. Called at session start: the launcher sets the variable before
/// each session's guest forks and it can change between sessions, so a value
/// carried over would advertise the HDR format table to an SDR session.
///
/// Also clears the connector HDR-output mirror ([`hdr_output_active`]): a new
/// session's connector starts un-driven, so a value left over from the previous
/// session would tag this session's first frames with the previous one's state.
/// Whether the per-frame daemon diagnostics are enabled for this session.
///
/// They are off unless the launcher sets `GNDRM_LOG_DAEMON` in this process:
/// the daemon shares the app's address space, so a container's `GNDRM_LOG_*`
/// cannot reach it, and a shipped session must not narrate how the presenter
/// works. Resolved once per session like [`hdr_output_enabled`].
#[must_use]
pub fn verbose_logging() -> bool {
    match VERBOSE_GATE.load(Ordering::Relaxed) {
        VERBOSE_ON => true,
        VERBOSE_OFF => false,
        _ => {
            let on = matches!(
                std::env::var_os("GNDRM_LOG_DAEMON"),
                Some(v) if !v.is_empty() && v != "0"
            );
            VERBOSE_GATE.store(if on { VERBOSE_ON } else { VERBOSE_OFF }, Ordering::Relaxed);
            on
        }
    }
}

/// Drop the cached [`verbose_logging`] verdict at session start, so a session
/// launched without the variable does not inherit the previous one's.
pub fn reset_verbose_logging() {
    VERBOSE_GATE.store(VERBOSE_UNRESOLVED, Ordering::Relaxed);
}

pub fn reset_hdr_output_cache() {
    HDR_GATE.store(HDR_UNRESOLVED, Ordering::Relaxed);
    CONNECTOR_HDR_ACTIVE.store(false, Ordering::Relaxed);
    *HDR_LUMINANCE.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// The display's HDR capability for this session: the panel's real luminances
/// in cd/m². `None` when the session is not HDR — either the launcher never
/// asked for it, or the panel reported no HDR types (the launcher only sets
/// `GNDRM_HDR` when `Display.getHdrCapabilities()` is non-empty).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HdrCapability {
    /// Peak luminance (CTA "desired content max luminance").
    pub max_nits: f32,
    /// Peak frame-average luminance.
    pub fall_nits: f32,
    /// Floor luminance.
    pub min_nits: f32,
}

/// Whether the session asked for HDR *and* the panel can take it, with the
/// panel's luminances. The gate is [`hdr_output_enabled`]; the luminances come
/// from `GNDRM_HDR_MAX_NITS` / `GNDRM_HDR_FALL_NITS` / `GNDRM_HDR_MIN_NITS`
/// (the launcher passes `Display.getHdrCapabilities()` values). Missing
/// luminances fall back to a generic HDR10 profile rather than losing HDR: the
/// gate already proves the panel is HDR-capable.
///
/// Resolved from the environment once per session and cached (the environment
/// lookup takes a lock), mirroring [`hdr_output_enabled`]; invalidation is the
/// same [`reset_hdr_output_cache`].
#[must_use]
pub fn hdr_output_capability() -> Option<HdrCapability> {
    if !hdr_output_enabled() {
        return None;
    }
    let mut slot = HDR_LUMINANCE.lock().unwrap_or_else(PoisonError::into_inner);
    let (max_nits, fall_nits, min_nits) = *slot.get_or_insert_with(|| {
        (
            env_f32("GNDRM_HDR_MAX_NITS", 1000.0),
            env_f32("GNDRM_HDR_FALL_NITS", 1000.0),
            env_f32("GNDRM_HDR_MIN_NITS", 0.05),
        )
    });
    Some(HdrCapability {
        max_nits,
        fall_nits,
        min_nits,
    })
}

/// Read a positive `f32` from the environment, or `default`.
fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|v| *v > 0.0 && v.is_finite())
        .unwrap_or(default)
}

/// Cached panel luminances, resolved with [`hdr_output_capability`].
static HDR_LUMINANCE: Mutex<Option<(f32, f32, f32)>> = Mutex::new(None);

/// Cached `GNDRM_HDR` verdict: [`HDR_UNRESOLVED`] until the first read, then
/// [`HDR_OFF`] or [`HDR_ON`].
static HDR_GATE: AtomicU8 = AtomicU8::new(HDR_UNRESOLVED);
const HDR_UNRESOLVED: u8 = 0;
const HDR_OFF: u8 = 1;
const HDR_ON: u8 = 2;

/// Whether the connector has been driven for HDR10 output this session: the HDR
/// **content** signal, mirrored process-wide from the connector state the daemon
/// serves.
///
/// gamescope's HDR10 commit writes the connector's `Colorspace` as
/// `DRM_MODE_COLORIMETRY_BT2020_RGB` and sets the `HDR_OUTPUT_METADATA` blob (the
/// DRM backend writes both, gated on `bWantsHDR10 && SupportsHDR10()`); its SDR
/// path writes `Colorspace = DEFAULT` and clears the blob. The daemon records
/// both writes (`Device::set_conn_colorspace`, `Device::set_conn_hdr_metadata`)
/// and this is the process-global mirror of them, read by the presenter at
/// publish time (the presenter runs on the commit worker, which holds no
/// `Device`).
///
/// This is deliberately **not** [`hdr_output_enabled`], which only reports that
/// the launcher asked for HDR: asking makes the plane advertise a 10-bit format,
/// so gamescope scans out `XBGR2101010` for the whole session, but it does not
/// mean the connector was ever driven for HDR10. Conflating the two is what
/// tags a composite output image as BT.2020/PQ — the composited framebuffer is
/// 10-bit whatever its content.
static CONNECTOR_HDR_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether the connector's HDR10 output is genuinely active (see
/// [`CONNECTOR_HDR_ACTIVE`]). The presenter reports it beside the published
/// this, never on the committed framebuffer's fourcc.
///
/// AWAITS DEVICE CONFIRMATION: the mechanism is read from gamescope's DRM
/// backend, not measured. The read that confirms or refutes it is the daemon's
/// committed-fourcc line in an HDR-on FSR/SGSR session: a 10-bit fourcc with
/// this signal false is the failure this discriminates; this true there would
/// refute it.
#[must_use]
pub fn hdr_output_active() -> bool {
    CONNECTOR_HDR_ACTIVE.load(Ordering::Relaxed)
}

/// Pure core of the mirror the connector setters write: the connector is
/// HDR-active when its `Colorspace` is `BT2020_RGB`.
///
/// The metadata blob deliberately does NOT count. gamescope writes
/// `HDR_OUTPUT_METADATA` on both paths — the SDR static metadata blob when HDR
/// is off (`DRMBackend.cpp:3281`) — so a set blob says nothing about the output
/// encoding, and reading it as a signal made this true in every session.
fn hdr_output_active_for(colorspace: u64) -> bool {
    colorspace == DRM_MODE_COLORIMETRY_BT2020_RGB
}

/// Cursor plane formats (doc §3.2: ARGB8888, 64×64).
///
/// `ARGB8888`/`XRGB8888` live here because the [`ScanoutLayout::Rgba8888`]
/// primary plane does not offer them, yet gamescope needs them in the union of
/// all plane formats: it builds
/// the client dmabuf sampling list from that union (Xwayland allocates depth-24
/// pixmaps as `XR24`), and its blank texture is `ARGB8888`. The cursor plane is
/// never published, so its formats never meet the pool.
const CURSOR_FORMATS: &[u32] = &[DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888];

/// What the socket layer does with one dispatch call.
#[derive(Debug)]
pub struct DispatchResult {
    /// ioctl return value: 0 or `-errno`.
    pub ret: i32,
    /// Response payload (out-struct + linearized extensions).
    pub body: Vec<u8>,
    /// fd to attach via `SCM_RIGHTS` (`PRIME_HANDLE_TO_FD`).
    pub fd_out: Option<RawFd>,
}

impl DispatchResult {
    fn err(e: i32) -> Self {
        Self {
            ret: -e,
            body: Vec::new(),
            fd_out: None,
        }
    }

    fn ok(body: Vec<u8>) -> Self {
        Self {
            ret: 0,
            body,
            fd_out: None,
        }
    }
}

/// Sink for committed frames. The Android side implements this with
/// the native SurfaceControl presenter; tests use [`NullPresenter`] or a
/// controllable mock.
///
/// `publish` is called on the commit worker thread (never the serve thread) and
/// must not block the serve loop: it hands the frame to the compositor and
/// returns at once. The worker never waits for the presenter's completion —
/// buffer reuse is gated on the compositor's own release signal (the previous
/// release fence the presenter waits on before re-using a slot), not on the
/// commit worker's progress.
///
/// `ino` is the committed framebuffer's buffer identity, resolved from the
/// object store by the caller before the commit was queued. `live_inos` is the
/// set of identities still backed by a GEM object at commit time, which the
/// presenter uses to decide which pool buffers to retire.
///
/// `fence` is the acquire fence from the commit's `IN_FENCE_FD` (ownership
/// transferred; the presenter must close it or hand it to the composer).
pub trait Presenter: Send {
    fn publish(
        &mut self,
        fb: &Framebuffer,
        plane: &PlaneState,
        ino: Option<u64>,
        live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    );

    /// [`Self::publish`] with the committed buffer's dma-buf, so a presenter
    /// that cannot show `ino` directly can copy it. `src_fd` is borrowed for
    /// the call: the worker closes it afterwards.
    fn publish_with_source(
        &mut self,
        fb: &Framebuffer,
        plane: &PlaneState,
        ino: Option<u64>,
        src_fd: Option<RawFd>,
        live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    ) {
        let _ = src_fd;
        self.publish(fb, plane, ino, live_inos, fence);
    }

    /// Put the cursor plane's new state on the cursor layer (a client holding
    /// [`GNDRM_CLIENT_CAP_CURSOR_LAYER`] only). Called on the commit worker
    /// after the commit's primary publish, for every commit that changed the
    /// cursor plane. `src_fd` is the cursor buffer's dma-buf, borrowed for the
    /// call. The default drops it: a presenter without a cursor layer shows no
    /// cursor plane, as before.
    fn publish_cursor(&mut self, cursor: &CursorUpdate, src_fd: Option<RawFd>) {
        let _ = (cursor, src_fd);
    }
}

/// What the cursor layer last showed: the plane state, the buffer's identity
/// and its layout (`width, height, fourcc, pitch, offset`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CursorKey {
    pub(crate) plane: PlaneState,
    pub(crate) ino: u64,
    pub(crate) layout: (u32, u32, u32, u32, u32),
}

/// One cursor-plane change handed to [`Presenter::publish_cursor`].
#[derive(Debug, Clone)]
pub struct CursorUpdate {
    /// The cursor framebuffer to show, or `None` to hide the cursor layer
    /// (`FB_ID` 0, or the CRTC is off).
    pub fb: Option<Framebuffer>,
    /// The cursor plane's committed state (rects in mode pixels).
    pub plane: PlaneState,
    /// The cursor buffer's identity (`st_ino`), resolved at commit time.
    pub ino: Option<u64>,
}

/// Default presenter: drops frames and completes them immediately. Phase-1
/// unit tests + headless runs.
#[derive(Debug, Default)]
pub struct NullPresenter {
    /// Number of frames presented (observability for tests).
    pub frames: u64,
}

impl Presenter for NullPresenter {
    fn publish(
        &mut self,
        _fb: &Framebuffer,
        _plane: &PlaneState,
        _ino: Option<u64>,
        _live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    ) {
        self.frames += 1;
        if let Some(fd) = fence {
            close_fd(fd);
        }
    }
}

type ClockFn = Box<dyn Fn() -> (u32, u32) + Send + Sync>;
/// The event clock, shared between the [`Device`] and the commit worker, which
/// re-stamps a page-flip event at emission (see [`commit_worker_loop`]); the
/// indirection lets [`Device::set_clock`] (tests) reach the worker too.
type SharedClock = Arc<Mutex<ClockFn>>;

/// The rate the connector advertises, in millihertz; 0 while the launcher has
/// not reported one.
///
/// Process state, not per-connection: gamescope reads the connector's modes
/// again whenever its backend is dirtied, and a display that changes mid-run has
/// to be able to change the mode the session runs on. A snapshot taken when the
/// session started could not. The cap and the frame-generation multiplier never
/// reach it: they move what the session produces, not what the compositor runs at.
static ADVERTISED_REFRESH_MHZ: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Set the rate the connector advertises. Called before a session starts and
/// again whenever the launcher's answer changes.
pub fn set_advertised_refresh_mhz(mhz: u32) {
    ADVERTISED_REFRESH_MHZ.store(mhz, Ordering::Relaxed);
}

/// The rate the connector advertises, 0 when the launcher never reported one.
#[must_use]
pub fn advertised_refresh_mhz() -> u32 {
    ADVERTISED_REFRESH_MHZ.load(Ordering::Relaxed)
}

/// The rate a mode is built at: the advertised one, or [`objects::UNKNOWN_PANEL_HZ`]
/// when the launcher never reported one.
#[must_use]
fn mode_refresh_mhz() -> u32 {
    match advertised_refresh_mhz() {
        0 => objects::UNKNOWN_PANEL_HZ,
        mhz => mhz,
    }
}

/// Diagnostics sink for rejections (see `Device::set_logger`).
pub type LogFn = Box<dyn Fn(&str) + Send>;
/// Sink for a framebuffer geometry the client registered (`ADDFB2`), so a
/// length-only scan-out request can resolve a window geometry the pool has
/// never owned (see `Device::set_addfb_geometry`). Called on the serve thread
/// with `(width, height, alloc_len)`, where `alloc_len` is the backing GEM's
/// real allocation length (`st_size`) — row-padded and page-rounded, the exact
/// length a length-only request carries. The presentation layer notes it in its
/// pool under the pool's own layout, never the DRM fourcc.
pub type AddFbGeometryFn = Box<dyn Fn(u32, u32, u64) + Send>;

/// The emulated device (one per connected client / open file description).
pub struct Device {
    panel: PanelDesc,
    stores: Stores,

    // client caps
    cap_atomic: bool,
    cap_universal_planes: bool,
    cap_writeback: bool,
    /// [`GNDRM_CLIENT_CAP_CURSOR_LAYER`]: the cursor plane is presented on a
    /// layer of its own and capped at [`CURSOR_LAYER_MAX`]. Card node only.
    cap_cursor_layer: bool,
    /// The cursor plane state last handed to the presenter (`None` = hidden or
    /// never shown), so only a commit that changes it reaches the cursor layer.
    pub(crate) presented_cursor: Option<CursorKey>,

    /// Which node type this connection impersonates. A card node carries the
    /// full KMS surface (gamescope); a render node carries only the
    /// rendering-capable ioctls (Xwayland's glamor/dri3/drm). Set once by the
    /// shim's connection-setup frame; defaults to card for direct/legacy
    /// clients and tests.
    node_kind: u32,

    // live software state
    conn_crtc_id: u32,
    /// Connector `Colorspace` current value. gamescope writes `BT2020_RGB` for
    /// HDR10 and `DEFAULT` for SDR.
    conn_colorspace: u64,
    /// Connector `HDR_OUTPUT_METADATA` current value: a property-blob id, or 0.
    conn_hdr_metadata: u64,
    crtc: CrtcState,
    primary: PlaneState,
    cursor: PlaneState,
    /// Primary-plane `IN_FORMATS` blob id (created at init: every
    /// `PRIMARY_FORMATS` fourcc × `DRM_FORMAT_MOD_LINEAR`).
    in_formats_blob: u32,
    /// Cursor-plane `IN_FORMATS` blob id (every `CURSOR_FORMATS` fourcc ×
    /// `DRM_FORMAT_MOD_LINEAR`).
    cursor_in_formats_blob: u32,
    /// Connector `EDID` blob id, built at session start from the display
    /// capability (no HDR static metadata block on an SDR panel).
    edid_blob: u32,

    clock: SharedClock,
    flip_sequence: u32,

    /// Outbound commit jobs: the serve thread queues, the worker pops. `None`
    /// only in the narrow window during [`Drop`], where taking it ends the
    /// worker's `recv`.
    commit_tx: Option<mpsc::Sender<CommitJob>>,
    /// Flip events the worker produced, drained by the serve loop / tests.
    flip_queue: Arc<Mutex<VecDeque<DrmEventVblank>>>,
    /// eventfd the worker writes after queueing a flip event; poll this to
    /// learn one is pending (then [`Device::drain_flip_events`]).
    flip_event_fd: RawFd,
    /// Primary fbs committed but whose flip event has not fired yet — a buffer
    /// is in-flight from commit until its flip event fires (real GEM semantics).
    in_flight: Arc<Mutex<HashSet<u32>>>,
    /// Set on drop; the worker checks it before servicing each commit.
    shutdown: Arc<AtomicBool>,
    /// The commit worker (owns the presenter for its whole lifetime).
    worker: Option<thread::JoinHandle<()>>,

    /// Optional diagnostics sink (Android glue → GN_DRM logcat, hostd →
    /// stderr). Rejections are always reported; successes stay silent. Shared
    /// behind an `Arc<Mutex<_>>` with the commit worker, which is spawned
    /// before `set_logger` runs and must observe it.
    logger: Arc<Mutex<Option<LogFn>>>,
    /// Optional sink for the commit probe, which is **ungated** — it prints
    /// whenever it is installed, whatever `GNDRM_LOG_*` says (see
    /// [`Device::set_always_logger`]). The container's environment reaches the
    /// guest, never the app process the daemon runs in, so a gated line cannot
    /// be turned on for a device pass at all. Serve thread only.
    always_logger: Option<LogFn>,
    /// Optional sink for framebuffer geometries (`ADDFB2`); see
    /// [`Device::set_addfb_geometry`]. Called on the serve thread only.
    addfb_geometry: Option<AddFbGeometryFn>,
    /// `stat.st_ino` of every eventfd handed out as an always-signalled
    /// sync-object fence (see [`Device::is_emulated_fence`]).
    emulated_fences: HashSet<u64>,
    /// Request numbers already announced via `diag` (one-shot each).
    seen_ioctls: Vec<u32>,
    /// Id/handle spaces already reported exhausted. One line per space: a
    /// sustained leak must surface once, not flood logcat.
    logged_exhaustion: Vec<&'static str>,
    /// Whether an `ADDFB2` refused for a buffer shorter than the image has
    /// been reported. One line per session: gamescope retries every frame.
    logged_addfb_short: bool,
    /// Distinct modifiers already reported by the `ADDFB2` feed, so a
    /// compressed framebuffer's geometry registration (and the modifier it
    /// carried) surfaces once per modifier rather than once per registration.
    addfb_mods: Vec<u64>,
    pub(crate) logged_first_commit: bool,
    /// The last (primary fb, previous fb, crtc active) triple the opt-in commit
    /// diagnostic printed. A commit that repeats it is not worth a line.
    pub(crate) logged_commit_state: Option<(u32, u32, bool)>,
    pub(crate) commit_count: u32,
    /// Last plane set the census logged (see `commit::plane_census_text`):
    /// comparing against it logs every change, not just the rate policy's
    /// samples. Empty until the first commit.
    pub(crate) plane_census: String,
}

impl Device {
    /// The connector's mode at the rate in force right now.
    ///
    /// Read per request rather than stored: the launcher can change the rate
    /// while the session runs, and gamescope re-reads the connector when it is
    /// told the backend is dirty.
    #[must_use]
    fn mode(&self) -> DrmModeModeinfo {
        self.panel.modeinfo(mode_refresh_mhz())
    }

    /// New device for `panel`; presents to `presenter` on a dedicated commit
    /// worker thread.
    pub fn new(panel: PanelDesc, presenter: Box<dyn Presenter>) -> Self {
        let mut stores = Stores::default();
        let mode = panel.modeinfo(mode_refresh_mhz());
        // Infallible: a fresh store has the whole blob space free.
        let in_formats_blob = stores
            .alloc_blob()
            .expect(&gndrm_obf::obf!("in_formats blob id"));
        stores.blobs.insert(
            in_formats_blob,
            Blob {
                id: in_formats_blob,
                data: in_formats_blob_data(primary_formats()),
            },
        );
        let cursor_in_formats_blob = stores
            .alloc_blob()
            .expect(&gndrm_obf::obf!("in_formats blob id"));
        stores.blobs.insert(
            cursor_in_formats_blob,
            Blob {
                id: cursor_in_formats_blob,
                data: in_formats_blob_data(CURSOR_FORMATS),
            },
        );
        // The EDID tells gamescope whether this display is HDR10. Built once
        // per session from the capability the launcher passes in, so an SDR
        // panel gets no HDR static metadata block and gamescope stays in SDR.
        let edid_blob = stores.alloc_blob().expect(&gndrm_obf::obf!("edid blob id"));
        stores.blobs.insert(
            edid_blob,
            Blob {
                id: edid_blob,
                data: edid::build(hdr_output_capability(), &mode, &panel),
            },
        );
        let (commit_tx, commit_rx) = mpsc::channel();
        let flip_queue = Arc::new(Mutex::new(VecDeque::new()));
        let flip_event_fd = new_eventfd(0).expect(&gndrm_obf::obf!("flip eventfd"));
        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let logger: Arc<Mutex<Option<LogFn>>> = Arc::new(Mutex::new(None));
        // The worker owns the presenter and signals a dup of the flip eventfd.
        let worker_fd = dup_fd(flip_event_fd, 0).expect(&gndrm_obf::obf!("dup flip eventfd"));
        // The event clock is shared with the commit worker, which re-stamps a
        // page-flip event when it emits it (see `commit_worker_loop`).
        let clock: SharedClock = Arc::new(Mutex::new(Box::new(monotonic_now)));
        let worker = thread::Builder::new()
            .name("gndrm-commit".into())
            .spawn({
                let flip_queue = Arc::clone(&flip_queue);
                let in_flight = Arc::clone(&in_flight);
                let shutdown = Arc::clone(&shutdown);
                let logger = Arc::clone(&logger);
                let clock = Arc::clone(&clock);
                move || {
                    commit_worker(
                        presenter, commit_rx, flip_queue, worker_fd, in_flight, shutdown, logger,
                        clock,
                    )
                }
            })
            .ok();
        Self {
            panel,
            stores,
            cap_atomic: false,
            cap_universal_planes: false,
            cap_writeback: false,
            cap_cursor_layer: false,
            presented_cursor: None,
            node_kind: NODE_KIND_CARD,
            conn_crtc_id: 0,
            conn_colorspace: DRM_MODE_COLORIMETRY_DEFAULT,
            conn_hdr_metadata: 0,
            crtc: CrtcState::default(),
            primary: PlaneState::default(),
            cursor: PlaneState::default(),
            in_formats_blob,
            cursor_in_formats_blob,
            edid_blob,
            clock,
            flip_sequence: 0,
            commit_tx: Some(commit_tx),
            flip_queue,
            flip_event_fd,
            in_flight,
            shutdown,
            worker,
            logger,
            always_logger: None,
            addfb_geometry: None,
            emulated_fences: HashSet::new(),
            seen_ioctls: Vec::new(),
            logged_exhaustion: Vec::new(),
            logged_addfb_short: false,
            addfb_mods: Vec::new(),
            logged_first_commit: false,
            logged_commit_state: None,
            commit_count: 0,
            plane_census: String::new(),
        }
    }

    /// Install a diagnostics logger (rejections only; see commit.rs).
    pub fn set_logger(&mut self, logger: LogFn) {
        *self.logger.lock().unwrap_or_else(PoisonError::into_inner) = Some(logger);
    }

    /// Install the **ungated** sink the commit probe prints through
    /// ([`Device::always_log`]). Unlike [`Device::set_logger`] this is installed
    /// unconditionally: the probe exists precisely for the sessions where no
    /// gate can be set — the daemon runs inside the app process, and the
    /// container's `GNDRM_LOG_*` reaches only the guest.
    pub fn set_always_logger(&mut self, logger: LogFn) {
        self.always_logger = Some(logger);
    }

    /// Emit the commit probe through the ungated sink, if one is installed.
    ///
    /// `msg` is a thunk so the message (and any `obf!` literal in it, which is a
    /// stack decode) is built only at the probe's own rate limit, never per
    /// frame. See [`Device::set_always_logger`].
    pub(crate) fn always_log(&self, msg: impl FnOnce() -> String) {
        if let Some(log) = &self.always_logger {
            log(&msg());
        }
    }

    /// Install a sink for framebuffer geometries the client registers
    /// (`ADDFB2`). The scan-out pool resolves a length-only allocation against
    /// geometries it knows, and a window-sized buffer the client allocates
    /// before its first commit is resolvable only once the pool has been told
    /// that geometry; this is where the daemon learns it. The sink runs on the
    /// serve thread and must not block (see the Android glue).
    pub fn set_addfb_geometry(&mut self, addfb_geometry: AddFbGeometryFn) {
        self.addfb_geometry = Some(addfb_geometry);
    }

    /// Declare which node type this connection impersonates (`NODE_KIND_CARD`
    /// or `NODE_KIND_RENDER`). Idempotent; the last value wins, matching a
    /// re-declaring client.
    pub fn set_node_kind(&mut self, kind: u32) {
        self.node_kind = kind;
    }

    /// The node type this connection impersonates.
    #[must_use]
    pub fn node_kind(&self) -> u32 {
        self.node_kind
    }

    /// Whether this connection is a render node (only render-capable ioctls).
    #[must_use]
    pub fn is_render_node(&self) -> bool {
        self.node_kind == NODE_KIND_RENDER
    }

    /// The id-space high-water marks (`[gem, fb, blob, syncobj]`), for tests
    /// and diagnostics: a bounded space is the property the leak fix holds.
    #[must_use]
    pub fn id_watermarks(&self) -> [u32; 4] {
        self.stores.watermarks()
    }

    /// Report an id/handle space as exhausted, once per space, and return the
    /// errno the client sees. Returning an errno (not aborting) is what keeps
    /// `app.gamenative` alive when a resource runs out: the display path stops
    /// accepting new resources, the app keeps running.
    fn note_exhausted(&mut self, space: &'static str) -> i32 {
        if !self.logged_exhaustion.contains(&space) {
            self.logged_exhaustion.push(space);
            self.diag(|| format!("{}", gndrm_obf::obf!(
                "resource: {space} exhausted; returning ENOSPC (display path degraded, app continues)"
            )));
        }
        errno::ENOSPC
    }

    /// Emit a diagnostic through the installed logger, if any.
    ///
    /// `msg` is a thunk: the message (and any `obf!` literal in it, which is a
    /// stack decode) is built only when a logger is installed. The app installs
    /// the logger only when `GNDRM_LOG_DAEMON` is on, so a disabled daemon never
    /// decodes a line it would drop.
    pub(crate) fn diag(&self, msg: impl FnOnce() -> String) {
        if let Some(log) = self
            .logger
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            log(&msg());
        }
    }

    /// Whether a diagnostics logger is installed. A call site that builds a
    /// message outside [`Self::diag`] (the plane census, which also tracks
    /// changes) tests this first so the work is skipped when diagnostics are
    /// off.
    #[must_use]
    pub(crate) fn diag_on(&self) -> bool {
        self.logger
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// Override the event clock (tests).
    pub fn set_clock(&mut self, clock: ClockFn) {
        *self.clock.lock().unwrap_or_else(PoisonError::into_inner) = clock;
    }

    /// Access the object stores (read-only; for the socket/presenter layer).
    #[must_use]
    pub fn stores(&self) -> &Stores {
        &self.stores
    }

    // -- state accessors used by the commit path -------------------------

    pub(crate) fn primary_state(&self) -> PlaneState {
        self.primary.clone()
    }

    pub(crate) fn cursor_state(&self) -> PlaneState {
        self.cursor.clone()
    }

    /// The largest cursor-plane framebuffer and rectangle this client may
    /// commit: [`CURSOR_LAYER_MAX`] with [`GNDRM_CLIENT_CAP_CURSOR_LAYER`],
    /// the legacy [`CURSOR_SIZE`] without it.
    pub(crate) fn cursor_limit(&self) -> u32 {
        if self.cap_cursor_layer {
            CURSOR_LAYER_MAX
        } else {
            CURSOR_SIZE
        }
    }

    /// Whether this client drives the cursor plane as a presented layer.
    pub(crate) fn cursor_layer_on(&self) -> bool {
        self.cap_cursor_layer
    }

    pub(crate) fn crtc_state(&self) -> CrtcState {
        self.crtc.clone()
    }

    pub(crate) fn set_conn_crtc(&mut self, crtc: u32) {
        self.conn_crtc_id = crtc;
    }

    pub(crate) fn set_conn_colorspace(&mut self, value: u64) {
        self.conn_colorspace = value;
        self.mirror_connector_hdr();
    }

    pub(crate) fn set_conn_hdr_metadata(&mut self, value: u64) {
        self.conn_hdr_metadata = value;
        self.mirror_connector_hdr();
    }

    /// Publish the connector's HDR-output state into the process-global signal
    /// the presenter reads (see `hdr_output_active`).
    fn mirror_connector_hdr(&self) {
        CONNECTOR_HDR_ACTIVE.store(
            hdr_output_active_for(self.conn_colorspace),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn set_crtc_active(&mut self, active: bool) {
        self.crtc.active = active;
    }

    pub(crate) fn set_crtc_mode(&mut self, blob_id: u32) {
        self.crtc.mode_id = blob_id;
    }

    /// Apply one property to plane `idx` (0 = primary, 1 = cursor).
    pub(crate) fn apply_plane(&mut self, idx: usize, prop_index: usize, value: u64) {
        let plane = if idx == 0 {
            &mut self.primary
        } else {
            &mut self.cursor
        };
        commit::apply_plane_prop_pub(plane, prop_index, value);
    }

    /// Queue a validated commit for the worker (see [`CommitJob`]). Called from
    /// the commit path after state is applied; the serve thread returns
    /// immediately and never blocks on the presenter.
    pub(crate) fn queue_commit(&mut self, job: CommitJob) {
        if let Some(tx) = self.commit_tx.as_ref() {
            let _ = tx.send(job);
        }
    }

    /// Mark `fb_id` in-flight: committed but its flip event has not fired yet.
    pub(crate) fn mark_in_flight(&self, fb_id: u32) {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(fb_id);
    }

    /// Is `fb_id` still in-flight (committed, flip event not yet fired)?
    pub(crate) fn is_in_flight(&self, fb_id: u32) -> bool {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&fb_id)
    }

    /// fd to poll alongside the RPC socket for pending flip events.
    #[must_use]
    pub fn flip_event_fd(&self) -> RawFd {
        self.flip_event_fd
    }

    /// Drain flip events the worker has produced, in commit order. The serve
    /// loop writes these to the client's event channel; tests read them to
    /// assert async ordering.
    ///
    /// The queue is drained first and the eventfd cleared after, so an event
    /// the worker queued is always returned regardless of whether the caller
    /// was woken by the eventfd (the eventfd only tells the serve loop to poll
    /// here; the queue is the source of truth).
    pub fn drain_flip_events(&mut self) -> Vec<DrmEventVblank> {
        let events: Vec<_> = self
            .flip_queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect();
        read_eventfd(self.flip_event_fd);
        events
    }

    pub(crate) fn now(&self) -> (u32, u32) {
        (self.clock.lock().unwrap_or_else(PoisonError::into_inner))()
    }

    pub(crate) fn next_flip_sequence(&mut self) -> u32 {
        // Wrapping: a sequence number is a monotonic counter the client only
        // compares for ordering; it must never panic on overflow.
        self.flip_sequence = self.flip_sequence.wrapping_add(1);
        self.flip_sequence
    }

    /// Handle one ioctl frame.
    ///
    /// - `ioctl_nr`: full encoded request number (as in the wire header).
    /// - `payload`: request body after `ReqHdr` (ioctl struct + extensions).
    /// - `fd_in`: fd received via `SCM_RIGHTS`, if the frame had `FLAG_FD`.
    pub fn dispatch(
        &mut self,
        ioctl_nr: u32,
        payload: &[u8],
        fd_in: Option<RawFd>,
    ) -> DispatchResult {
        // Any non-fd ioctl arriving with an fd attached must not leak it.
        let mut fd_in = fd_in;
        // One-shot per request number: shows exactly how far a client
        // (gamescope) gets through the uapi surface before stalling.
        if !self.seen_ioctls.contains(&ioctl_nr) {
            self.seen_ioctls.push(ioctl_nr);
            self.diag(|| {
                format!(
                    "{}",
                    gndrm_obf::obf!("ioctl: first {} (0x{ioctl_nr:08x})", ioctl_name(ioctl_nr))
                )
            });
        }
        let mut result = if self.node_kind == NODE_KIND_RENDER && !render_node_allows(ioctl_nr) {
            // A render node exposes only the render-capable ioctls. Refusing
            // modesetting/master here is what a real kernel node does, and it
            // is the observable signal that this connection is a render node.
            self.diag(|| {
                format!(
                    "{}",
                    gndrm_obf::obf!(
                        "render node: refused {} (0x{ioctl_nr:08x})",
                        ioctl_name(ioctl_nr)
                    )
                )
            });
            DispatchResult::err(errno::EACCES)
        } else {
            match ioctl_nr {
                GNDRM_IOCTL_NODE_KIND => self.ioctl_node_kind(payload),
                DRM_IOCTL_VERSION => self.ioctl_version(payload),
                DRM_IOCTL_GET_CAP => self.ioctl_get_cap(payload),
                DRM_IOCTL_SET_CLIENT_CAP => self.ioctl_set_client_cap(payload),
                DRM_IOCTL_SET_MASTER | DRM_IOCTL_DROP_MASTER => DispatchResult::ok(Vec::new()),
                DRM_IOCTL_MODE_GETRESOURCES => self.ioctl_get_resources(payload),
                DRM_IOCTL_MODE_GETCRTC => self.ioctl_get_crtc(payload),
                DRM_IOCTL_MODE_GETCONNECTOR => self.ioctl_get_connector(payload),
                DRM_IOCTL_MODE_GETENCODER => self.ioctl_get_encoder(payload),
                DRM_IOCTL_MODE_GETPLANERESOURCES => self.ioctl_get_plane_res(payload),
                DRM_IOCTL_MODE_GETPLANE => self.ioctl_get_plane(payload),
                DRM_IOCTL_MODE_OBJ_GETPROPERTIES => self.ioctl_obj_get_properties(payload),
                DRM_IOCTL_MODE_GETPROPERTY => self.ioctl_get_property(payload),
                DRM_IOCTL_MODE_OBJ_SETPROPERTY => self.ioctl_obj_set_property(payload),
                DRM_IOCTL_MODE_CREATEPROPBLOB => self.ioctl_create_blob(payload),
                DRM_IOCTL_MODE_GETPROPBLOB => self.ioctl_get_blob(payload),
                DRM_IOCTL_MODE_DESTROYPROPBLOB => self.ioctl_destroy_blob(payload),
                DRM_IOCTL_GEM_CREATE => self.ioctl_gem_create(payload),
                DRM_IOCTL_GEM_MMAP => self.ioctl_gem_mmap(payload),
                DRM_IOCTL_PRIME_FD_TO_HANDLE => {
                    self.ioctl_prime_fd_to_handle(payload, fd_in.take())
                }
                DRM_IOCTL_PRIME_HANDLE_TO_FD => self.ioctl_prime_handle_to_fd(payload),
                DRM_IOCTL_GEM_CLOSE => self.ioctl_gem_close(payload),
                DRM_IOCTL_MODE_ADDFB2 => self.ioctl_addfb2(payload),
                DRM_IOCTL_MODE_GETFB2 => self.ioctl_getfb2(payload),
                DRM_IOCTL_MODE_RMFB => self.ioctl_rmfb(payload),
                DRM_IOCTL_SYNCOBJ_CREATE => self.ioctl_syncobj_create(payload),
                DRM_IOCTL_SYNCOBJ_DESTROY => self.ioctl_syncobj_destroy(payload),
                DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD => self.ioctl_syncobj_handle_to_fd(payload),
                DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE => self.ioctl_syncobj_fd_to_handle(payload),
                DRM_IOCTL_MODE_ATOMIC => self.ioctl_atomic(payload, fd_in.take()),
                _ => DispatchResult::err(errno::EINVAL),
            }
        };
        // Drop an unclaimed fd exactly like an ioctl that ignores it would
        // (the kernel simply doesn't consume the reference — but the wire
        // gave *us* one, so we must close it to avoid leaking).
        if let Some(fd) = fd_in.take() {
            close_fd(fd);
        }
        if result.ret != 0 {
            result.fd_out = None;
        }
        result
    }

    // -----------------------------------------------------------------
    // device ioctls
    // -----------------------------------------------------------------

    fn ioctl_version(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut v) = pod_read::<DrmVersion>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        // Kernel semantics: *_len out = full string length; the shim truncates
        // the copy into the caller's buffers, so always marshal full strings.
        v.version_major = DRIVER_VERSION.0;
        v.version_minor = DRIVER_VERSION.1;
        v.version_patchlevel = DRIVER_VERSION.2;
        v.name_len = DRIVER_NAME.len();
        v.date_len = DRIVER_DATE.len();
        v.desc_len = DRIVER_DESC.len();
        let mut body = Vec::new();
        pod_write(&mut body, &v);
        body.extend_from_slice(DRIVER_NAME);
        body.extend_from_slice(DRIVER_DATE);
        body.extend_from_slice(DRIVER_DESC);
        DispatchResult::ok(body)
    }

    fn ioctl_get_cap(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut c) = pod_read::<DrmGetCap>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        c.value = match c.capability {
            DRM_CAP_DUMB_BUFFER => 1,
            // Export and import are both real: GEM objects carry dma-buf fds
            // out (HANDLE_TO_FD) and in (FD_TO_HANDLE). Mesa's GBM and
            // glamor's device probe read this cap before allocating.
            DRM_CAP_PRIME => DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT,
            DRM_CAP_TIMESTAMP_MONOTONIC => 1,
            DRM_CAP_CURSOR_WIDTH | DRM_CAP_CURSOR_HEIGHT => u64::from(self.cursor_limit()),
            // Advertised: gamescope's DRM backend creates an always-signalled
            // sync object here and exports it as the in-fence it rides on every
            // atomic commit. With this 0 it logs "Syncobjs are not supported"
            // and never commits at all, which is a black screen, not a
            // fallback.
            DRM_CAP_SYNCOBJ => 1,
            // Not supported, and a kernel reports that as value 0 with success
            // (not an error). Xwayland's `xwl_gbm_supports_syncobjs` reads this
            // cap first; answering it the kernel's way keeps explicit sync off
            // without a spurious error.
            DRM_CAP_SYNCOBJ_TIMELINE => 0,
            // Explicit modifiers accepted (LINEAR only; see IN_FORMATS).
            DRM_CAP_ADDFB2_MODIFIERS => 1,
            _ => return DispatchResult::err(errno::EINVAL),
        };
        let mut body = Vec::new();
        pod_write(&mut body, &c);
        DispatchResult::ok(body)
    }

    fn ioctl_set_client_cap(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(c) = pod_read::<DrmSetClientCap>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        if c.value > 1 {
            return DispatchResult::err(errno::EINVAL);
        }
        match c.capability {
            DRM_CLIENT_CAP_ATOMIC => self.cap_atomic = c.value == 1,
            DRM_CLIENT_CAP_UNIVERSAL_PLANES => self.cap_universal_planes = c.value == 1,
            DRM_CLIENT_CAP_WRITEBACK_CONNECTORS => self.cap_writeback = c.value == 1,
            GNDRM_CLIENT_CAP_CURSOR_LAYER if self.node_kind == NODE_KIND_CARD => {
                let on = c.value == 1;
                if on != self.cap_cursor_layer {
                    self.cap_cursor_layer = on;
                    // Once per session (the client sets it at bring-up).
                    self.always_log(|| {
                        format!(
                            "{}",
                            gndrm_obf::obf!(
                                "cursor: cursor layer {} (max {}x{})",
                                if on { "on" } else { "off" },
                                self.cursor_limit(),
                                self.cursor_limit()
                            )
                        )
                    });
                }
            }
            _ => return DispatchResult::err(errno::EINVAL),
        }
        DispatchResult::ok(Vec::new())
    }

    /// Connection setup (not a kernel ioctl): record whether this connection
    /// impersonates the card node or a render node.
    fn ioctl_node_kind(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(kind) = pod_read::<u32>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        if kind != NODE_KIND_CARD && kind != NODE_KIND_RENDER {
            return DispatchResult::err(errno::EINVAL);
        }
        self.node_kind = kind;
        DispatchResult::ok(Vec::new())
    }

    // -----------------------------------------------------------------
    // discovery (task A3/A4): two-call count/array semantics
    // -----------------------------------------------------------------

    /// Two-call helper: append `items` truncated to `offered`, return full count.
    fn two_call<T: Copy>(body: &mut Vec<u8>, ptr: u64, offered: u32, items: &[T]) -> u32 {
        if ptr != 0 && offered > 0 {
            let n = (offered as usize).min(items.len());
            pod_write_array(body, &items[..n]);
        }
        items.len() as u32
    }

    fn ioctl_get_resources(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut r) = pod_read::<DrmModeCardRes>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let mut body = Vec::new();
        // Struct first; arrays after, in field order, only where ptr != 0.
        let fb_ids: Vec<u32> = self.stores.fbs.keys().copied().collect();
        let crtcs = [CRTC_ID];
        let connectors = [CONNECTOR_ID];
        let encoders = [ENCODER_ID];
        let count_fbs = Self::two_call(&mut body, r.fb_id_ptr, r.count_fbs, &fb_ids);
        let count_crtcs = Self::two_call(&mut body, r.crtc_id_ptr, r.count_crtcs, &crtcs);
        let count_conn = Self::two_call(
            &mut body,
            r.connector_id_ptr,
            r.count_connectors,
            &connectors,
        );
        let count_enc = Self::two_call(&mut body, r.encoder_id_ptr, r.count_encoders, &encoders);
        r.count_fbs = count_fbs;
        r.count_crtcs = count_crtcs;
        r.count_connectors = count_conn;
        r.count_encoders = count_enc;
        r.min_width = MIN_WIDTH;
        r.max_width = MAX_WIDTH;
        r.min_height = MIN_HEIGHT;
        r.max_height = MAX_HEIGHT;
        // Struct must be FIRST in the frame: rebuild in order.
        let mut ordered = Vec::new();
        pod_write(&mut ordered, &r);
        ordered.extend_from_slice(&body);
        DispatchResult::ok(ordered)
    }

    fn ioctl_get_crtc(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut c) = pod_read::<DrmModeCrtc>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        if c.crtc_id != CRTC_ID {
            return DispatchResult::err(errno::ENOENT);
        }
        let mut body = Vec::new();
        pod_write(&mut body, &c); // placeholder, rebuilt below
        body.clear();
        let routed = if self.conn_crtc_id == CRTC_ID { 1 } else { 0 };
        let conns = [CONNECTOR_ID];
        let count = Self::two_call(
            &mut body,
            c.set_connectors_ptr,
            c.count_connectors,
            &conns[..routed as usize],
        );
        c.count_connectors = count;
        c.fb_id = self.primary.fb_id;
        c.x = 0;
        c.y = 0;
        c.gamma_size = 0;
        c.mode_valid = u32::from(self.crtc.mode_id != 0);
        c.mode = self.mode();
        let mut ordered = Vec::new();
        pod_write(&mut ordered, &c);
        ordered.extend_from_slice(&body);
        DispatchResult::ok(ordered)
    }

    fn ioctl_get_connector(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut c) = pod_read::<DrmModeGetConnector>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        if c.connector_id != CONNECTOR_ID {
            return DispatchResult::err(errno::ENOENT);
        }
        let mut body = Vec::new();
        let encoders = [ENCODER_ID];
        let modes = [self.mode()];
        let (prop_ids, prop_vals) = self.connector_props();
        let count_enc = Self::two_call(&mut body, c.encoders_ptr, c.count_encoders, &encoders);
        let count_modes = Self::two_call(&mut body, c.modes_ptr, c.count_modes, &modes);
        let count_props = Self::two_call(&mut body, c.props_ptr, c.count_props, &prop_ids);
        let _ = Self::two_call(&mut body, c.prop_values_ptr, c.count_props, &prop_vals);
        c.count_encoders = count_enc;
        c.count_modes = count_modes;
        c.count_props = count_props;
        c.encoder_id = ENCODER_ID;
        c.connector_type = DRM_MODE_CONNECTOR_DSI;
        c.connector_type_id = 1;
        c.connection = DRM_MODE_CONNECTED;
        c.mm_width = self.panel.mm_width;
        c.mm_height = self.panel.mm_height;
        c.subpixel = DRM_MODE_SUBPIXEL_UNKNOWN;
        c.pad = 0;
        let mut ordered = Vec::new();
        pod_write(&mut ordered, &c);
        ordered.extend_from_slice(&body);
        DispatchResult::ok(ordered)
    }

    fn connector_props(&self) -> (Vec<u32>, Vec<u64>) {
        let ids = props::CONNECTOR_PROPS
            .iter()
            .map(|&i| props::prop_id(i))
            .collect::<Vec<_>>();
        let vals = props::CONNECTOR_PROPS
            .iter()
            .map(|&i| self.current_value(CONNECTOR_ID, i))
            .collect::<Vec<_>>();
        (ids, vals)
    }

    fn ioctl_get_encoder(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut e) = pod_read::<DrmModeGetEncoder>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        if e.encoder_id != ENCODER_ID {
            return DispatchResult::err(errno::ENOENT);
        }
        e.encoder_type = DRM_MODE_ENCODER_VIRTUAL;
        e.crtc_id = CRTC_ID;
        e.possible_crtcs = 1;
        e.possible_clones = 0;
        let mut body = Vec::new();
        pod_write(&mut body, &e);
        DispatchResult::ok(body)
    }

    fn ioctl_get_plane_res(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut r) = pod_read::<DrmModeGetPlaneRes>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let mut body = Vec::new();
        let planes = [PLANE_PRIMARY_ID, PLANE_CURSOR_ID];
        let count = Self::two_call(&mut body, r.plane_id_ptr, r.count_planes, &planes);
        r.count_planes = count;
        let mut ordered = Vec::new();
        pod_write(&mut ordered, &r);
        ordered.extend_from_slice(&body);
        DispatchResult::ok(ordered)
    }

    fn ioctl_get_plane(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut p) = pod_read::<DrmModeGetPlane>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let (state, formats) = match p.plane_id {
            PLANE_PRIMARY_ID => (&self.primary, primary_formats()),
            PLANE_CURSOR_ID => (&self.cursor, CURSOR_FORMATS),
            _ => return DispatchResult::err(errno::ENOENT),
        };
        let mut body = Vec::new();
        let count = Self::two_call(&mut body, p.format_type_ptr, p.count_format_types, formats);
        p.count_format_types = count;
        p.crtc_id = state.crtc_id;
        p.fb_id = state.fb_id;
        p.possible_crtcs = 1;
        p.gamma_size = 0;
        let mut ordered = Vec::new();
        pod_write(&mut ordered, &p);
        ordered.extend_from_slice(&body);
        DispatchResult::ok(ordered)
    }

    // -----------------------------------------------------------------
    // properties & blobs (task A4)
    // -----------------------------------------------------------------

    fn current_value(&self, obj_id: u32, index: usize) -> u64 {
        use props::*;
        match (obj_id, index) {
            (CONNECTOR_ID, P_CONN_CRTC_ID) => u64::from(self.conn_crtc_id),
            (CONNECTOR_ID, P_CONN_PANEL_ORIENTATION) => 0,
            (CONNECTOR_ID, P_CONN_EDID) => u64::from(self.edid_blob),
            (CONNECTOR_ID, P_CONN_COLORSPACE) => self.conn_colorspace,
            (CONNECTOR_ID, P_CONN_HDR_METADATA) => self.conn_hdr_metadata,
            (CRTC_ID, P_CRTC_ACTIVE) => u64::from(self.crtc.active),
            (CRTC_ID, P_CRTC_MODE_ID) => u64::from(self.crtc.mode_id),
            (PLANE_PRIMARY_ID, P_PLANE_TYPE) => u64::from(DRM_PLANE_TYPE_PRIMARY),
            (PLANE_CURSOR_ID, P_PLANE_TYPE) => u64::from(DRM_PLANE_TYPE_CURSOR),
            (PLANE_PRIMARY_ID, P_PLANE_FB_ID) => u64::from(self.primary.fb_id),
            (PLANE_CURSOR_ID, P_PLANE_FB_ID) => u64::from(self.cursor.fb_id),
            (PLANE_PRIMARY_ID, P_PLANE_CRTC_ID) => u64::from(self.primary.crtc_id),
            (PLANE_CURSOR_ID, P_PLANE_CRTC_ID) => u64::from(self.cursor.crtc_id),
            (PLANE_PRIMARY_ID, P_PLANE_SRC_X) => u64::from(self.primary.src_x),
            (PLANE_CURSOR_ID, P_PLANE_SRC_X) => u64::from(self.cursor.src_x),
            (PLANE_PRIMARY_ID, P_PLANE_SRC_Y) => u64::from(self.primary.src_y),
            (PLANE_CURSOR_ID, P_PLANE_SRC_Y) => u64::from(self.cursor.src_y),
            (PLANE_PRIMARY_ID, P_PLANE_SRC_W) => u64::from(self.primary.src_w),
            (PLANE_CURSOR_ID, P_PLANE_SRC_W) => u64::from(self.cursor.src_w),
            (PLANE_PRIMARY_ID, P_PLANE_SRC_H) => u64::from(self.primary.src_h),
            (PLANE_CURSOR_ID, P_PLANE_SRC_H) => u64::from(self.cursor.src_h),
            (PLANE_PRIMARY_ID, P_PLANE_CRTC_X) => self.primary.crtc_x as u32 as u64,
            (PLANE_CURSOR_ID, P_PLANE_CRTC_X) => self.cursor.crtc_x as u32 as u64,
            (PLANE_PRIMARY_ID, P_PLANE_CRTC_Y) => self.primary.crtc_y as u32 as u64,
            (PLANE_CURSOR_ID, P_PLANE_CRTC_Y) => self.cursor.crtc_y as u32 as u64,
            (PLANE_PRIMARY_ID, P_PLANE_CRTC_W) => self.primary.crtc_w as u32 as u64,
            (PLANE_CURSOR_ID, P_PLANE_CRTC_W) => self.cursor.crtc_w as u32 as u64,
            (PLANE_PRIMARY_ID, P_PLANE_CRTC_H) => self.primary.crtc_h as u32 as u64,
            (PLANE_CURSOR_ID, P_PLANE_CRTC_H) => self.cursor.crtc_h as u32 as u64,
            (_, P_PLANE_IN_FENCE_FD) => u64::MAX, // -1: no fence
            (PLANE_PRIMARY_ID, P_PLANE_IN_FORMATS) => u64::from(self.in_formats_blob),
            // Kernel exposes IN_FORMATS on every plane, and gamescope reads
            // each plane's formats from it. A zero value here makes
            // libliftoff's plane init fail the whole DRM backend.
            (PLANE_CURSOR_ID, P_PLANE_IN_FORMATS) => u64::from(self.cursor_in_formats_blob),
            _ => 0,
        }
    }

    /// `OBJ_SETPROPERTY`: the legacy (non-atomic) property write. gamescope is
    /// atomic-only, but the shim forwards the ioctl and a client that uses it
    /// (the b9 harness, libdrm's older helpers) must not get a blanket
    /// `EINVAL`. Validates and stores exactly like the atomic path — see
    /// [`Device::validate_property`].
    fn ioctl_obj_set_property(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(s) = pod_read::<DrmModeObjSetProperty>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        // The object type must agree with the id, like the kernel.
        let Some((_, ty, _)) = props::obj_list()
            .into_iter()
            .find(|(id, _, _)| *id == s.obj_id)
        else {
            return DispatchResult::err(errno::ENOENT);
        };
        if ty != s.obj_type {
            return DispatchResult::err(errno::ENOENT);
        }
        let Some(index) = props::resolve(s.obj_id, s.prop_id) else {
            return DispatchResult::err(errno::ENOENT);
        };
        if props::PROPS[index].2 {
            return DispatchResult::err(errno::EPERM);
        }
        if let Err(e) = self.validate_property(index, s.value) {
            return DispatchResult::err(e);
        }
        self.apply_property(s.obj_id, index, s.value);
        DispatchResult::ok(Vec::new())
    }

    fn ioctl_obj_get_properties(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut g) = pod_read::<DrmModeObjGetProperties>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let Some((_, _, indices)) = props::obj_list()
            .into_iter()
            .find(|(id, ty, _)| *id == g.obj_id && *ty == g.obj_type)
        else {
            return DispatchResult::err(errno::ENOENT);
        };
        let ids: Vec<u32> = indices.iter().map(|&i| props::prop_id(i)).collect();
        let vals: Vec<u64> = indices
            .iter()
            .map(|&i| self.current_value(g.obj_id, i))
            .collect();
        let mut body = Vec::new();
        let count = Self::two_call(&mut body, g.props_ptr, g.count_props, &ids);
        let _ = Self::two_call(&mut body, g.prop_values_ptr, g.count_props, &vals);
        g.count_props = count;
        let mut ordered = Vec::new();
        pod_write(&mut ordered, &g);
        ordered.extend_from_slice(&body);
        DispatchResult::ok(ordered)
    }

    fn ioctl_get_property(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut g) = pod_read::<DrmModeGetProperty>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let Some(idx) = props::prop_index(g.prop_id) else {
            return DispatchResult::err(errno::ENOENT);
        };
        let (name, kind, immutable) = props::PROPS[idx];
        let mut flags = DRM_MODE_PROP_ATOMIC;
        if immutable {
            flags |= DRM_MODE_PROP_IMMUTABLE;
        }
        let mut values: Vec<u64> = Vec::new();
        let mut enums: Vec<DrmModePropertyEnum> = Vec::new();
        match kind {
            props::PropKind::Range(min, max) => {
                flags |= DRM_MODE_PROP_RANGE;
                values = vec![min, max];
            }
            props::PropKind::SignedRange(min, max) => {
                flags |= DRM_MODE_PROP_SIGNED_RANGE;
                // i64 bit patterns — the kernel marshals signed bounds the
                // same way (IN_FENCE_FD = [-1, INT_MAX]).
                values = vec![min as u64, max as u64];
            }
            props::PropKind::Object => {
                flags |= DRM_MODE_PROP_OBJECT;
                values = vec![0, u64::from(u32::MAX)];
            }
            props::PropKind::Blob => flags |= DRM_MODE_PROP_BLOB,
            props::PropKind::Enum(entries) => {
                flags |= DRM_MODE_PROP_ENUM;
                values = entries.iter().map(|(v, _)| *v).collect();
                enums = entries
                    .iter()
                    .map(|(v, n)| {
                        let mut name = [0u8; DRM_PROP_NAME_LEN];
                        name[..n.len()].copy_from_slice(n.as_bytes());
                        DrmModePropertyEnum { value: *v, name }
                    })
                    .collect();
            }
        }
        let mut body = Vec::new();
        let count_values = Self::two_call(&mut body, g.values_ptr, g.count_values, &values);
        let count_enums = Self::two_call(&mut body, g.enum_blob_ptr, g.count_enum_blobs, &enums);
        g.count_values = count_values;
        g.count_enum_blobs = count_enums;
        g.flags = flags;
        let mut name_buf = [0u8; DRM_PROP_NAME_LEN];
        name_buf[..name.len()].copy_from_slice(name.as_bytes());
        g.name = name_buf;
        let mut ordered = Vec::new();
        pod_write(&mut ordered, &g);
        ordered.extend_from_slice(&body);
        DispatchResult::ok(ordered)
    }

    fn ioctl_create_blob(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut c) = pod_read::<DrmModeCreateBlob>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let data_off = core::mem::size_of::<DrmModeCreateBlob>();
        let Some(data) = payload.get(data_off..data_off + c.length as usize) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let id = match self.stores.alloc_blob() {
            Ok(id) => id,
            Err(_) => return DispatchResult::err(self.note_exhausted("property-blob id space")),
        };
        self.stores.blobs.insert(
            id,
            Blob {
                id,
                data: data.to_vec(),
            },
        );
        c.blob_id = id;
        let mut body = Vec::new();
        pod_write(&mut body, &c);
        DispatchResult::ok(body)
    }

    fn ioctl_get_blob(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut g) = pod_read::<DrmModeGetBlob>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let Some(blob) = self.stores.blobs.get(&g.blob_id) else {
            return DispatchResult::err(errno::ENOENT);
        };
        let mut body = Vec::new();
        pod_write(&mut body, &g); // placeholder
        body.clear();
        if g.data != 0 && g.length > 0 {
            let n = (g.length as usize).min(blob.data.len());
            body.extend_from_slice(&blob.data[..n]);
        }
        g.length = blob.data.len() as u32;
        let mut ordered = Vec::new();
        pod_write(&mut ordered, &g);
        ordered.extend_from_slice(&body);
        DispatchResult::ok(ordered)
    }

    fn ioctl_destroy_blob(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(d) = pod_read::<DrmModeDestroyBlob>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        if self.stores.blobs.remove(&d.blob_id).is_none() {
            return DispatchResult::err(errno::ENOENT);
        }
        // Recycle the id: a client that recreates its MODE_ID blob every
        // modeset must not exhaust the blob space.
        self.stores.free_blob(d.blob_id);
        DispatchResult::ok(Vec::new())
    }

    // -----------------------------------------------------------------
    // GEM / FB lifecycle (task A5)
    // -----------------------------------------------------------------

    /// `drm_gem_create`: allocate a buffer object and return a handle.
    ///
    /// The buffer is a real, CPU-mappable allocation (an anonymous memfd) that
    /// the daemon owns and can export as a dma-buf, so a GEM created here is
    /// indistinguishable from one imported through `PRIME_FD_TO_HANDLE`: both
    /// live in the same store, dedup by `st_ino`, and resolve to the same
    /// `HANDLE_TO_FD`/`ADDFB2` behaviour. This is the primitive a render-node
    /// client (Mesa/GBM, glamor) allocates with before it maps or exports.
    fn ioctl_gem_create(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut c) = pod_read::<DrmGemCreate>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        if c.size == 0 {
            return DispatchResult::err(errno::EINVAL);
        }
        let Some(fd) = new_anonymous_buffer(c.size) else {
            return DispatchResult::err(errno::ENOMEM);
        };
        let Some((ino, size)) = fstat_fd(fd) else {
            close_fd(fd);
            return DispatchResult::err(errno::EIO);
        };
        // A memfd is inode-unique, so an accidental `st_ino` collision with an
        // imported dma-buf cannot alias an unrelated object.
        if self.stores.gem_by_ino.contains_key(&ino) {
            close_fd(fd);
            return DispatchResult::err(errno::EINVAL);
        }
        let handle = match self.stores.alloc_gem() {
            Ok(h) => h,
            Err(_) => {
                close_fd(fd);
                return DispatchResult::err(self.note_exhausted("GEM handle space"));
            }
        };
        self.stores
            .gems
            .insert(handle, objects::GemObject { fd, ino, size });
        self.stores.gem_ino_ref(ino);
        self.stores.gem_by_ino.insert(ino, handle);
        c.handle = handle;
        let mut body = Vec::new();
        pod_write(&mut body, &c);
        DispatchResult::ok(body)
    }

    /// `drm_gem_mmap`: hand back the offset the client passes to `mmap`.
    ///
    /// A socket cannot be mapped, so the offset is a device-minted token that
    /// carries the GEM handle ([`GEM_MMAP_OFFSET_BASE`] in the high half). The
    /// shim recognises the token and maps the handle's dma-buf in place of the
    /// device fd, so the legacy `GEM_MMAP` + `mmap` sequence works unchanged.
    fn ioctl_gem_mmap(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut m) = pod_read::<DrmGemMmap>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let Some(gem) = self
            .stores
            .gems
            .get(&m.handle)
            .or_else(|| self.stores.gems_closed.get(&m.handle))
        else {
            return DispatchResult::err(errno::ENOENT);
        };
        m.size = gem.size;
        m.offset = GEM_MMAP_OFFSET_BASE | u64::from(m.handle);
        let mut body = Vec::new();
        pod_write(&mut body, &m);
        DispatchResult::ok(body)
    }

    fn ioctl_prime_fd_to_handle(&mut self, payload: &[u8], fd_in: Option<RawFd>) -> DispatchResult {
        if pod_read::<DrmPrimeHandle>(payload).is_none() {
            return DispatchResult::err(errno::EINVAL);
        }
        let Some(fd) = fd_in else {
            return DispatchResult::err(errno::EINVAL);
        };
        let Some((ino, size)) = fstat_fd(fd) else {
            close_fd(fd);
            return DispatchResult::err(errno::EBADF);
        };
        let size = if size == 0 { seek_end(fd) } else { size };
        // Dedup by st_ino (doc §5): same dma-buf re-imported → same handle;
        // the wire gave us a redundant reference, so close it.
        if let Some(&handle) = self.stores.gem_by_ino.get(&ino) {
            close_fd(fd);
            let mut body = Vec::new();
            pod_write(
                &mut body,
                &DrmPrimeHandle {
                    handle,
                    flags: 0,
                    fd: -1,
                },
            );
            return DispatchResult::ok(body);
        }
        let handle = match self.stores.alloc_gem() {
            Ok(h) => h,
            Err(_) => {
                // The wire handed us this reference; an unclaimed fd is closed
                // here, exactly as an ioctl that ignores its fd would.
                close_fd(fd);
                return DispatchResult::err(self.note_exhausted("GEM handle space"));
            }
        };
        self.stores
            .gems
            .insert(handle, objects::GemObject { fd, ino, size });
        self.stores.gem_ino_ref(ino);
        self.stores.gem_by_ino.insert(ino, handle);
        let mut body = Vec::new();
        pod_write(
            &mut body,
            &DrmPrimeHandle {
                handle,
                flags: 0,
                fd: -1,
            },
        );
        DispatchResult::ok(body)
    }

    fn ioctl_prime_handle_to_fd(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(h) = pod_read::<DrmPrimeHandle>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let Some(gem) = self.stores.gems.get(&h.handle) else {
            return DispatchResult::err(errno::ENOENT);
        };
        let Some(new_fd) = dup_fd(gem.fd, h.flags) else {
            return DispatchResult::err(errno::EBADF);
        };
        let mut body = Vec::new();
        pod_write(
            &mut body,
            &DrmPrimeHandle {
                handle: h.handle,
                flags: h.flags,
                fd: 0, // shim replaces with its local fd
            },
        );
        let mut result = DispatchResult::ok(body);
        result.fd_out = Some(new_fd);
        result
    }

    /// `drm_syncobj_create`. Only the signalled flavour is modelled (see the
    /// UAPI comment), so the flag is recorded rather than acted on.
    fn ioctl_syncobj_create(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut c) = pod_read::<DrmSyncobjCreate>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let signaled = c.flags & DRM_SYNCOBJ_CREATE_SIGNALED != 0;
        c.handle = match self.stores.alloc_syncobj() {
            Ok(h) => h,
            Err(_) => return DispatchResult::err(self.note_exhausted("syncobj handle space")),
        };
        self.stores.syncobjs.insert(
            c.handle,
            Syncobj {
                signaled,
                export_fd: None,
            },
        );
        let mut body = Vec::new();
        pod_write(&mut body, &c);
        DispatchResult::ok(body)
    }

    /// `drm_syncobj_destroy`.
    fn ioctl_syncobj_destroy(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(d) = pod_read::<DrmSyncobjDestroy>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        match self.stores.syncobjs.remove(&d.handle) {
            Some(s) => {
                if let Some(fd) = s.export_fd {
                    close_fd(fd);
                }
                // Recycle the handle id so a client creating/destroying fences
                // per frame cannot exhaust the space.
                self.stores.free_syncobj(d.handle);
                DispatchResult::ok(Vec::new())
            }
            None => DispatchResult::err(errno::ENOENT),
        }
    }

    /// `drm_syncobj_handle_to_fd`: hand out a dup of a `poll`-ready fd for a
    /// signalled object. The presenter polls the in-fence before sampling, so
    /// being poll-ready is what "always signalled" has to mean here.
    fn ioctl_syncobj_handle_to_fd(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut h) = pod_read::<DrmSyncobjHandle>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        // The exported fd is an eventfd standing in for an always-signalled
        // fence (there is no way to mint a real `sync_file` from userspace).
        // Record its identity so an `IN_FENCE_FD` that rides it back is
        // recognised as this device's own emulated fence.
        let export_ino = {
            let Some(s) = self.stores.syncobjs.get_mut(&h.handle) else {
                return DispatchResult::err(errno::ENOENT);
            };
            if !s.signaled {
                return DispatchResult::err(errno::EINVAL);
            }
            if s.export_fd.is_none() {
                s.export_fd = signaled_fence_fd();
            }
            s.export_fd.and_then(fstat_fd).map(|(ino, _)| ino)
        };
        if let Some(ino) = export_ino {
            self.emulated_fences.insert(ino);
        }
        let Some(fd) = self
            .stores
            .syncobjs
            .get(&h.handle)
            .and_then(|s| s.export_fd)
            .and_then(|f| dup_fd(f, 0))
        else {
            return DispatchResult::err(errno::ENOMEM);
        };
        h.fd = 0; // the shim replaces this with its local fd
        let mut body = Vec::new();
        pod_write(&mut body, &h);
        let mut result = DispatchResult::ok(body);
        result.fd_out = Some(fd);
        result
    }

    /// Importing a sync file as a sync object would need real dma-fence
    /// plumbing; the modelled clients only export.
    fn ioctl_syncobj_fd_to_handle(&mut self, _payload: &[u8]) -> DispatchResult {
        DispatchResult::err(errno::EINVAL)
    }

    /// Is `fd` one of the eventfds this device handed out as an
    /// always-signalled sync-object fence?
    ///
    /// Identity is `stat.st_ino`, the same key the GEM store dedups dma-bufs
    /// by: `SCM_RIGHTS` and `dup` keep the inode, so the copy the client rides
    /// back in `IN_FENCE_FD` still names the original eventfd. Such a fence is
    /// already signalled, so it is equivalent to *no* fence — and it is not a
    /// `sync_file`, so it must never be forwarded to the presenter (see
    /// `commit::ioctl_atomic`).
    fn is_emulated_fence(&self, fd: RawFd) -> bool {
        if self.emulated_fences.is_empty() {
            return false;
        }
        fstat_fd(fd).is_some_and(|(ino, _)| self.emulated_fences.contains(&ino))
    }

    fn ioctl_gem_close(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(c) = pod_read::<DrmGemClose>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        // Kernel semantics: closing a handle an FB still uses is ALLOWED —
        // the FB holds its own reference and the object dies with the last
        // reference. gamescope relies on this: it closes dumb-buffer handles
        // right after ADDFB2. Move FB-referenced GEMs to the orphan table;
        // ioctl_rmfb frees them once no FB references remain.
        let fb_referenced = self
            .stores
            .fbs
            .values()
            .any(|fb| fb.handles.contains(&c.handle));
        match self.stores.gems.remove(&c.handle) {
            Some(gem) => {
                self.stores.gem_by_ino.remove(&gem.ino);
                if fb_referenced {
                    // The orphan table names the same inode: the object only
                    // moves, so its reference count is unchanged.
                    self.stores.gems_closed.insert(c.handle, gem);
                } else {
                    // The last reference to the object: one inode leaves the
                    // live set (`Stores::live_inos`).
                    self.stores.gem_ino_unref(gem.ino);
                    close_fd(gem.fd);
                    // No FB references it: the handle is truly gone, so its id
                    // returns to the space (gamescope opens/closes a handle per
                    // buffer; without this the id space leaks one per frame).
                    self.stores.free_gem(c.handle);
                }
                DispatchResult::ok(Vec::new())
            }
            None => DispatchResult::err(errno::EINVAL),
        }
    }

    fn ioctl_addfb2(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(mut cmd) = pod_read::<DrmModeFbCmd2>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        if cmd.width < MIN_WIDTH
            || cmd.width > MAX_WIDTH
            || cmd.height < MIN_HEIGHT
            || cmd.height > MAX_HEIGHT
        {
            return DispatchResult::err(errno::EINVAL);
        }
        // Modifiers are recorded, not gated on: gamescope scans out the client
        // dma-bufs directly, and a compressed (UBWC) slot is one of them. The
        // primary plane's `IN_FORMATS` blob advertises LINEAR, but a framebuffer
        // registered with a modifier the plane did not advertise is still a
        // buffer the presenter can publish — refusing it here is what kept the
        // compressed image's real allocation length (4947968) out of the
        // geometry feed, so a later length-only scan-out request for it could
        // not resolve. Accepting it lets the note below register that length.
        let Some(bpp) = format_bpp(cmd.pixel_format) else {
            return DispatchResult::err(errno::EINVAL);
        };
        if cmd.handles[0] == 0 || !self.stores.gems.contains_key(&cmd.handles[0]) {
            return DispatchResult::err(errno::ENOENT);
        }
        let min_pitch = u64::from(cmd.width) * u64::from(bpp) / 8;
        if u64::from(cmd.pitches[0]) < min_pitch {
            return DispatchResult::err(errno::EINVAL);
        }
        // Buffer must actually span the image (kernel validates similarly).
        let need = u64::from(cmd.offsets[0])
            + u64::from(cmd.pitches[0]) * u64::from(cmd.height - 1)
            + u64::from(cmd.width) * u64::from(bpp) / 8;
        // A length of 0 is a descriptor whose size the daemon could not learn
        // (neither `st_size` nor its end offset), not an empty buffer: the
        // span cannot be checked, so it is not refused for it.
        let gem_size = self.stores.gems[&cmd.handles[0]].size;
        if gem_size != 0 && need > gem_size {
            if !self.logged_addfb_short {
                self.logged_addfb_short = true;
                let ino = self.stores.gems[&cmd.handles[0]].ino;
                self.diag(|| {
                    format!(
                        "{}",
                        gndrm_obf::obf!(
                            "addfb2: refused {}x{} pitch={} offset={}: buffer ino {ino} is {gem_size} bytes, the image needs {need}",
                            cmd.width, cmd.height, cmd.pitches[0], cmd.offsets[0]
                        )
                    )
                });
            }
            return DispatchResult::err(errno::EINVAL);
        }
        for &h in &cmd.handles {
            if h != 0 && !self.stores.gems.contains_key(&h) {
                return DispatchResult::err(errno::ENOENT);
            }
        }
        let id = match self.stores.alloc_fb() {
            Ok(id) => id,
            Err(_) => return DispatchResult::err(self.note_exhausted("FB id space")),
        };
        cmd.fb_id = id;
        self.stores.fbs.insert(
            id,
            Framebuffer {
                id,
                width: cmd.width,
                height: cmd.height,
                pixel_format: cmd.pixel_format,
                flags: cmd.flags,
                handles: cmd.handles,
                pitches: cmd.pitches,
                offsets: cmd.offsets,
                modifier: cmd.modifier,
            },
        );
        // Teach the presentation layer this geometry, so a length-only scan-out
        // request of the same pixel size resolves to it. Only a 32-bpp
        // framebuffer is a buffer of the pixel size the pool serves; the pool's
        // own layout is noted, never the DRM fourcc (the length index is
        // keyed by `(alloc_len, format)`, so a fourcc entry would poison a
        // pool lookup).
        if bpp == 32 {
            if let Some(note) = &self.addfb_geometry {
                note(cmd.width, cmd.height, gem_size);
            }
        }
        // One line per distinct explicit modifier: this is where a compressed
        // (UBWC) framebuffer's real allocation length enters the geometry feed,
        // so its registration is visible without per-frame logging.
        let modifier = cmd.modifier[0];
        if cmd.flags & DRM_MODE_FB_MODIFIERS != 0 && !self.addfb_mods.contains(&modifier) {
            self.addfb_mods.push(modifier);
            self.diag(|| {
                format!(
                    "{}",
                    gndrm_obf::obf!(
                "addfb2: fb {}x{} fmt={:#x} gem={} modifier={:#x} registered into the length index",
                cmd.width, cmd.height, cmd.pixel_format, gem_size, modifier
            )
                )
            });
        }
        let mut body = Vec::new();
        pod_write(&mut body, &cmd);
        DispatchResult::ok(body)
    }

    fn ioctl_getfb2(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(cmd) = pod_read::<DrmModeFbCmd2>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        let Some(fb) = self.stores.fbs.get(&cmd.fb_id).cloned() else {
            return DispatchResult::err(errno::ENOENT);
        };
        // Kernel semantics (drm_mode_getfb2): each returned handle is a FRESH
        // GEM reference owned by the caller, closable via GEM_CLOSE. gamescope
        // closes its own handle right after ADDFB2 (orphaning the GEM), so
        // handing out the raw ids makes libliftoff's drmCloseBufferHandle fail
        // with EINVAL (device-proven). Dup the fd into a new handle instead;
        // the fresh handle deliberately bypasses gem_by_ino (import dedup must
        // keep pointing at importer-held handles, not these transient refs).
        let mut handles = fb.handles;
        for h in &mut handles {
            if *h == 0 {
                continue;
            }
            let gem = self
                .stores
                .gems
                .get(h)
                .or_else(|| self.stores.gems_closed.get(h));
            *h = match gem {
                Some(g) => match dup_fd(g.fd, 0) {
                    Some(new_fd) => {
                        let (ino, size) = (g.ino, g.size);
                        match self.stores.alloc_gem() {
                            Ok(new_handle) => {
                                self.stores.gems.insert(
                                    new_handle,
                                    objects::GemObject {
                                        fd: new_fd,
                                        ino,
                                        size,
                                    },
                                );
                                // A fresh handle for an inode that may already
                                // be named: the census counts entries.
                                self.stores.gem_ino_ref(ino);
                                new_handle
                            }
                            // Space exhausted: release the dup we just made and
                            // report 0 for this handle rather than abort. The
                            // caller (libliftoff) sees a 0 handle and skips it.
                            Err(_) => {
                                close_fd(new_fd);
                                self.note_exhausted("GEM handle space");
                                0
                            }
                        }
                    }
                    None => 0,
                },
                None => 0,
            };
        }
        let mut body = Vec::new();
        pod_write(
            &mut body,
            &DrmModeFbCmd2 {
                fb_id: fb.id,
                width: fb.width,
                height: fb.height,
                pixel_format: fb.pixel_format,
                flags: fb.flags,
                handles,
                pitches: fb.pitches,
                offsets: fb.offsets,
                modifier: fb.modifier,
            },
        );
        DispatchResult::ok(body)
    }

    fn ioctl_rmfb(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(fb_id) = pod_read::<u32>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        // Kernel refuses to remove a framebuffer that is still scanned out —
        // and, now that commits are async, one that is in-flight (committed but
        // whose flip event has not fired): the worker may still be presenting
        // it, and releasing its GEM under it would corrupt the frame.
        if self.primary.fb_id == fb_id || self.cursor.fb_id == fb_id || self.is_in_flight(fb_id) {
            return DispatchResult::err(errno::EBUSY);
        }
        let Some(fb) = self.stores.fbs.remove(&fb_id) else {
            return DispatchResult::err(errno::EINVAL);
        };
        // Free orphaned (handle-closed) GEMs this FB was the last to hold.
        for h in fb.handles {
            if h == 0 || self.stores.fbs.values().any(|f| f.handles.contains(&h)) {
                continue;
            }
            if let Some(gem) = self.stores.gems_closed.remove(&h) {
                self.stores.gem_ino_unref(gem.ino);
                close_fd(gem.fd);
                // The last reference is gone: recycle the handle id too.
                self.stores.free_gem(h);
            }
        }
        // Recycle the fb id: gamescope adds/removes a framebuffer per frame, so
        // without this the fb id space leaks one per frame.
        self.stores.free_fb(fb_id);
        DispatchResult::ok(Vec::new())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // End the worker's recv, then join it so the presenter (SurfaceControl
        // layer + buffers) is released on the thread that owns it before we go
        // away.
        self.shutdown.store(true, Ordering::Relaxed);
        drop(self.commit_tx.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        close_fd(self.flip_event_fd);
    }
}

/// One validated atomic commit handed to the worker thread.
///
/// The serve thread resolves everything the presenter needs from the object
/// store *before* queueing (the store lives on the serve thread), so the job is
/// self-contained and the worker never touches `Stores`.
struct CommitJob {
    /// The primary framebuffer this commit puts on the panel; `None` when there
    /// is nothing to publish (cursor-only update, plane off, or an unchanged
    /// FB). When `Some`, the buffer is in-flight until the worker finishes it.
    fb: Option<Framebuffer>,
    plane: PlaneState,
    /// Buffer identity resolved from the GEM store at commit time.
    ino: Option<u64>,
    /// A duplicate of that GEM's dma-buf, owned by the job, so the presenter
    /// can read a buffer it does not own even if the client closes the GEM
    /// while the commit is queued.
    src_fd: Option<RawFd>,
    /// Inodes still backed by a GEM object at commit time (the presenter's
    /// retire signal).
    ///
    /// Shared with the object store's cached snapshot (`Stores::live_inos`), so
    /// queuing a commit copies an `Arc`, not the set.
    live_inos: Arc<HashSet<u64>>,
    /// Acquire fence from `IN_FENCE_FD`.
    fence: Option<RawFd>,
    /// Built at commit time when `PAGE_FLIP_EVENT` was requested; the worker
    /// stamps its vblank and emits it with the publish.
    event: Option<DrmEventVblank>,
    /// The cursor plane's change in this commit, for a client holding
    /// [`GNDRM_CLIENT_CAP_CURSOR_LAYER`]; `None` when it did not change.
    cursor: Option<CursorUpdate>,
    /// A duplicate of the cursor buffer's dma-buf, owned by the job.
    cursor_src_fd: Option<RawFd>,
}

impl Drop for CommitJob {
    fn drop(&mut self) {
        // A job dropped without being processed (teardown, or a worker that
        // already exited) still owns its fence.
        if let Some(fd) = self.fence.take() {
            close_fd(fd);
        }
        if let Some(fd) = self.src_fd.take() {
            close_fd(fd);
        }
        if let Some(fd) = self.cursor_src_fd.take() {
            close_fd(fd);
        }
    }
}

/// The commit worker: a thin panic barrier around [`commit_worker_loop`].
///
/// A panic anywhere on this worker — most plausibly inside the presenter — is
/// caught here, reported once, and ends the worker. The subsystem then stops
/// (no further frames are presented) while `app.gamenative` stays alive: this
/// replaces the old behaviour where a worker panic aborted the whole process.
#[allow(clippy::too_many_arguments)]
fn commit_worker(
    presenter: Box<dyn Presenter>,
    commit_rx: mpsc::Receiver<CommitJob>,
    flip_queue: Arc<Mutex<VecDeque<DrmEventVblank>>>,
    flip_event_fd: RawFd,
    in_flight: Arc<Mutex<HashSet<u32>>>,
    shutdown: Arc<AtomicBool>,
    logger: Arc<Mutex<Option<LogFn>>>,
    clock: SharedClock,
) {
    let report = Arc::clone(&logger);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        commit_worker_loop(
            presenter,
            commit_rx,
            flip_queue,
            flip_event_fd,
            in_flight,
            shutdown,
            clock,
        );
    }));
    if caught.is_err() {
        if let Some(log) = report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            log("commit: worker PANICKED; display path stopped, app continues");
        }
    }
    // Always released, whether the loop returned or unwound through it.
    close_fd(flip_event_fd);
}

/// The commit worker: one serial loop that pops a commit, publishes it, and
/// emits the flip event. It never waits for the presenter's completion: the
/// flip event is emitted with the publish (a real KMS page-flip event is
/// delivered when the flip is queued for the next vblank), and a buffer's
/// lifetime is gated by the presenter's own release-fence wait, not by this
/// loop.
#[allow(clippy::too_many_arguments)]
fn commit_worker_loop(
    mut presenter: Box<dyn Presenter>,
    commit_rx: mpsc::Receiver<CommitJob>,
    flip_queue: Arc<Mutex<VecDeque<DrmEventVblank>>>,
    flip_event_fd: RawFd,
    in_flight: Arc<Mutex<HashSet<u32>>>,
    shutdown: Arc<AtomicBool>,
    clock: SharedClock,
) {
    // Publish what this worker is doing for the scan-out snapshot: "the commit
    // worker was <state>" is one of the facts a stalled request has to explain.
    // Atomics only, so this cannot perturb the flip path.
    let d = diag();
    d.set_role(Role::Commit, State::CommitWaitingJob);
    // The vblank schedule the flip events claim (see `vblank::VblankTimeline`).
    let mut timeline = vblank::VblankTimeline::default();
    while let Ok(mut job) = commit_rx.recv() {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        // The fb id this commit published, if any.
        let in_flight_fb = job.fb.as_ref().map(|fb| fb.id);
        // Publish the buffer when there is one; otherwise the commit carries no
        // presentation (the frame is dropped/hidden) and only its fence is
        // released.
        d.set_role(Role::Commit, State::CommitPublishing);
        match job.fb.take() {
            Some(fb) => {
                let fence = job.fence.take();
                presenter.publish_with_source(
                    &fb,
                    &job.plane,
                    job.ino,
                    job.src_fd,
                    &job.live_inos,
                    fence,
                );
            }
            None => {
                if let Some(fd) = job.fence.take() {
                    close_fd(fd);
                }
            }
        }
        // The cursor layer, after the primary publish: a cursor-only commit
        // reaches the presenter here and nowhere else, so it never re-flips
        // the scan-out layer (nor feeds a frame generator on it).
        if let Some(cursor) = job.cursor.take() {
            let src_fd = job.cursor_src_fd.take();
            presenter.publish_cursor(&cursor, src_fd);
            if let Some(fd) = src_fd {
                close_fd(fd);
            }
        }
        // The frame is now queued to SurfaceFlinger. Emit the page-flip event
        // here, with the publish: a real KMS page-flip event is delivered when
        // the flip is *queued* for the next vblank. gamescope's DRM backend
        // blocks its present loop on this event
        // (`drm->uPendingFlipCount.wait`, Backends/DRMBackend.cpp), so gating it
        // on SurfaceFlinger's completion round trip would serialise that round
        // trip (~8.4 ms median here) into every paced frame.
        //
        // Correctness is unchanged: nothing is reusable before the compositor
        // has released it. A buffer is handed back out only once the client has
        // stopped importing it (the pool's liveness gate), and the next publish
        // onto that slot still waits on the previous release fence the
        // presenter's completion stored. This loop never blocks on the
        // completion itself — the flip event is a client pacing signal, and the
        // buffer's lifetime is pinned to the compositor's release signal.
        d.set_role(Role::Commit, State::CommitFlushing);
        if let Some(fb_id) = in_flight_fb {
            in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&fb_id);
        }
        if let Some(mut ev) = job.event.take() {
            // Stamp the vblank this frame lands on, not the instant the event is
            // built or emitted: the display's latch deadline when its timeline
            // is known, else a walk of the mode's interval (see
            // `vblank::VblankTimeline`). Delivery stays immediate, so
            // gamescope's present loop never waits on a display round trip.
            let (sec, usec) = (clock.lock().unwrap_or_else(PoisonError::into_inner))();
            let now = u64::from(sec) * 1_000_000_000 + u64::from(usec) * 1_000;
            let claimed = timeline.claim(
                now,
                vblank::display_grid(now),
                1_000_000_000_000 / u64::from(mode_refresh_mhz()),
            );
            ev.tv_sec = (claimed / 1_000_000_000) as u32;
            ev.tv_usec = ((claimed % 1_000_000_000) / 1_000) as u32;
            flip_queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_back(ev);
            write_eventfd(flip_event_fd);
        }
        d.set_role(Role::Commit, State::CommitWaitingJob);
    }
}

/// `drm_format_modifier_blob` for a plane's `IN_FORMATS`: every format in
/// `formats` (for the primary plane the SDR set, or the HDR set when HDR output
/// is on) scanned out with `DRM_FORMAT_MOD_LINEAR`.
fn in_formats_blob_data(formats: &[u32]) -> Vec<u8> {
    const HEADER: u32 = 24; // drm_format_modifier_blob header size
    let count_formats = formats.len() as u32;
    let formats_offset = HEADER;
    let modifiers_offset = formats_offset + count_formats * 4;
    let mut data = Vec::with_capacity(modifiers_offset as usize + 24);
    data.extend_from_slice(&1u32.to_le_bytes()); // version: FORMAT_BLOB_CURRENT
    data.extend_from_slice(&0u32.to_le_bytes()); // flags
    data.extend_from_slice(&count_formats.to_le_bytes());
    data.extend_from_slice(&formats_offset.to_le_bytes());
    data.extend_from_slice(&1u32.to_le_bytes()); // count_modifiers
    data.extend_from_slice(&modifiers_offset.to_le_bytes());
    for &f in formats {
        data.extend_from_slice(&f.to_le_bytes());
    }
    // drm_format_modifier { formats, offset, pad, modifier }
    let mask = (1u64 << count_formats) - 1;
    data.extend_from_slice(&mask.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes()); // offset
    data.extend_from_slice(&0u32.to_le_bytes()); // pad
    data.extend_from_slice(&DRM_FORMAT_MOD_LINEAR.to_le_bytes());
    data
}

/// Bits-per-pixel for the formats we accept (single-plane only in phase 1).
fn format_bpp(fourcc: u32) -> Option<u32> {
    match fourcc {
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 | DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => {
            Some(32)
        }
        // 2:10:10:10 is still a 32-bit word, so buffer sizing and stride are
        // unchanged from the 8-bit formats above.
        DRM_FORMAT_XRGB2101010
        | DRM_FORMAT_XBGR2101010
        | DRM_FORMAT_ARGB2101010
        | DRM_FORMAT_ABGR2101010 => Some(32),
        DRM_FORMAT_RGB565 => Some(16),
        _ => None,
    }
}

/// `clock_gettime(CLOCK_MONOTONIC)` → (sec, usec) for event timestamps.
fn monotonic_now() -> (u32, u32) {
    // SAFETY: clock_gettime with a valid clock id and a valid out-pointer.
    let ts = unsafe {
        let mut ts: libc::timespec = core::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        ts
    };
    (ts.tv_sec as u32, (ts.tv_nsec / 1000) as u32)
}

/// `fstat(fd)` → (st_ino, st_size).
fn fstat_fd(fd: RawFd) -> Option<(u64, u64)> {
    // SAFETY: fstat into a zeroed struct; fd validity is checked by the call.
    let st = unsafe {
        let mut st: libc::stat = core::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return None;
        }
        st
    };
    Some((st.st_ino, st.st_size as u64))
}

/// The end offset of `fd`, i.e. a dma-buf's length on a kernel that leaves its
/// inode size at zero; 0 when the descriptor cannot seek.
fn seek_end(fd: RawFd) -> u64 {
    // SAFETY: lseek on a descriptor this device owns; a dma-buf only moves its
    // own offset, which nothing here reads through.
    let end = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    if end <= 0 {
        return 0;
    }
    // SAFETY: as above.
    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
    end as u64
}

/// `dup(fd)`, honouring `DRM_CLOEXEC`/`DRM_RDWR` in `flags` (as `HANDLE_TO_FD`).
/// An eventfd whose count is already 1, so `poll` reports it readable — what an
/// always-signalled fence looks like to a poll-based wait. `eventfd` itself is
/// not in bionic's libc, so the syscall is issued directly.
fn signaled_fence_fd() -> Option<RawFd> {
    let fd = unsafe { libc::syscall(libc::SYS_eventfd2, 1u32, libc::EFD_CLOEXEC) } as RawFd;
    if fd < 0 {
        None
    } else {
        Some(fd)
    }
}

/// A fresh, unsignalled eventfd (count 0, non-blocking so a `read` on an empty
/// counter returns `EAGAIN` instead of blocking). `eventfd` itself is not in
/// bionic's libc, so the syscall is issued directly, matching
/// [`signaled_fence_fd`].
fn new_eventfd(count: u32) -> Option<RawFd> {
    let flags = libc::EFD_CLOEXEC | libc::EFD_NONBLOCK;
    let fd = unsafe { libc::syscall(libc::SYS_eventfd2, count, flags) } as RawFd;
    (fd >= 0).then_some(fd)
}

/// A fresh anonymous, sized, CPU-mappable buffer (memfd) for `GEM_CREATE`. The
/// fd is mappable and exportable as a dma-buf, exactly like the buffers the
/// scan-out allocator hands out. `MFD_CLOEXEC` keeps it from leaking into a
/// child across exec (the daemon and its clients do not share an address
/// space anyway, but the flag matches the fd hygiene of the rest of the
/// module).
fn new_anonymous_buffer(size: u64) -> Option<RawFd> {
    // SAFETY: memfd_create takes a NUL-terminated name and a flags word; it
    // returns a fresh descriptor this path owns, or -1 with errno set.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"gndrm-gem".as_ptr(),
            libc::MFD_CLOEXEC,
        ) as RawFd
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is a live memfd this path owns; ftruncate sets its size.
    if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
        close_fd(fd);
        return None;
    }
    Some(fd)
}

/// Write one to `fd`, waking any `poll` on it. Used by the worker to signal
/// that a flip event is queued.
fn write_eventfd(fd: RawFd) {
    let one: u64 = 1;
    // SAFETY: `fd` is a live eventfd; a partial 8-byte write cannot happen.
    unsafe {
        libc::write(fd, (&raw const one).cast(), 8);
    }
}

/// Drain `fd`'s counter so the next `poll` only fires for new events.
fn read_eventfd(fd: RawFd) {
    let mut buf = [0u8; 8];
    // SAFETY: `fd` is a live eventfd; reads of fewer than 8 bytes are ignored.
    unsafe {
        libc::read(fd, buf.as_mut_ptr().cast(), 8);
    }
}

fn dup_fd(fd: RawFd, flags: u32) -> Option<RawFd> {
    const DRM_CLOEXEC: u32 = 0x02; // O_CLOEXEC passed through PRIME_HANDLE_TO_FD
                                   // SAFETY: F_DUPFD_CLOEXEC/F_DUPFD on a presumably-valid fd; errors checked.

    let cmd = if flags & DRM_CLOEXEC != 0 {
        libc::F_DUPFD_CLOEXEC
    } else {
        libc::F_DUPFD
    };
    let newfd = unsafe { libc::fcntl(fd, cmd, 0) };
    (newfd >= 0).then_some(newfd)
}

fn close_fd(fd: RawFd) {
    // SAFETY: fd is owned by the caller path (wire reference or store entry).
    unsafe { libc::close(fd) };
}

/// Human-readable name for the one-shot ioctl arrival log.
fn ioctl_name(nr: u32) -> &'static str {
    match nr {
        DRM_IOCTL_VERSION => "VERSION",
        GNDRM_IOCTL_NODE_KIND => "GNDRM_NODE_KIND",
        DRM_IOCTL_GET_CAP => "GET_CAP",
        DRM_IOCTL_SET_CLIENT_CAP => "SET_CLIENT_CAP",
        DRM_IOCTL_SET_MASTER => "SET_MASTER",
        DRM_IOCTL_DROP_MASTER => "DROP_MASTER",
        DRM_IOCTL_GEM_CREATE => "GEM_CREATE",
        DRM_IOCTL_GEM_MMAP => "GEM_MMAP",
        DRM_IOCTL_MODE_GETRESOURCES => "MODE_GETRESOURCES",
        DRM_IOCTL_MODE_GETCRTC => "MODE_GETCRTC",
        DRM_IOCTL_MODE_GETCONNECTOR => "MODE_GETCONNECTOR",
        DRM_IOCTL_MODE_GETENCODER => "MODE_GETENCODER",
        DRM_IOCTL_MODE_GETPLANERESOURCES => "MODE_GETPLANERESOURCES",
        DRM_IOCTL_MODE_GETPLANE => "MODE_GETPLANE",
        DRM_IOCTL_MODE_OBJ_GETPROPERTIES => "MODE_OBJ_GETPROPERTIES",
        DRM_IOCTL_MODE_GETPROPERTY => "MODE_GETPROPERTY",
        DRM_IOCTL_MODE_OBJ_SETPROPERTY => "MODE_OBJ_SETPROPERTY",
        DRM_IOCTL_MODE_CREATEPROPBLOB => "MODE_CREATEPROPBLOB",
        DRM_IOCTL_MODE_GETPROPBLOB => "MODE_GETPROPBLOB",
        DRM_IOCTL_MODE_DESTROYPROPBLOB => "MODE_DESTROYPROPBLOB",
        DRM_IOCTL_PRIME_FD_TO_HANDLE => "PRIME_FD_TO_HANDLE",
        DRM_IOCTL_PRIME_HANDLE_TO_FD => "PRIME_HANDLE_TO_FD",
        DRM_IOCTL_GEM_CLOSE => "GEM_CLOSE",
        DRM_IOCTL_MODE_ADDFB2 => "MODE_ADDFB2",
        DRM_IOCTL_MODE_GETFB2 => "MODE_GETFB2",
        DRM_IOCTL_MODE_RMFB => "MODE_RMFB",
        DRM_IOCTL_MODE_ATOMIC => "MODE_ATOMIC",
        DRM_IOCTL_SYNCOBJ_CREATE => "SYNCOBJ_CREATE",
        DRM_IOCTL_SYNCOBJ_DESTROY => "SYNCOBJ_DESTROY",
        DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD => "SYNCOBJ_HANDLE_TO_FD",
        DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE => "SYNCOBJ_FD_TO_HANDLE",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev() -> Device {
        Device::new(PanelDesc::default(), Box::<NullPresenter>::default())
    }

    /// The HDR flag gates exactly the 10-bit fourcc, keeping each layout's
    /// first format and its SDR set untouched.
    #[test]
    fn primary_formats_are_hdr_gated() {
        for layout in [ScanoutLayout::Bgra8888, ScanoutLayout::Rgba8888] {
            let sdr = primary_formats_for(false, layout);
            assert!(!sdr.contains(&DRM_FORMAT_XRGB2101010));
            let hdr = primary_formats_for(true, layout);
            assert_eq!(hdr.len(), sdr.len() + 1);
            assert!(hdr.contains(&DRM_FORMAT_XBGR2101010));
            // XRGB2101010 is deliberately absent: its channel order does not
            // match the R10G10B10A2 pool layout, and gamescope would prefer it.
            assert!(!hdr.contains(&DRM_FORMAT_XRGB2101010));
            // The SDR set is a prefix: enabling HDR only appends.
            assert_eq!(&hdr[..sdr.len()], sdr);
        }
    }

    /// The BGRA layout offers the list shipped before the RGBA layout existed;
    /// the RGBA layout offers only the R-low 8-bit formats.
    #[test]
    fn primary_formats_follow_the_layout() {
        assert_eq!(
            primary_formats_for(false, ScanoutLayout::Bgra8888),
            &[
                DRM_FORMAT_XRGB8888,
                DRM_FORMAT_ARGB8888,
                DRM_FORMAT_XBGR8888,
                DRM_FORMAT_ABGR8888,
                DRM_FORMAT_RGB565,
            ]
        );
        assert_eq!(
            primary_formats_for(false, ScanoutLayout::Rgba8888),
            &[DRM_FORMAT_XBGR8888, DRM_FORMAT_ABGR8888, DRM_FORMAT_RGB565]
        );
        assert_eq!(primary_formats_for(true, ScanoutLayout::Bgra8888).len(), 6);
        assert_eq!(primary_formats_for(true, ScanoutLayout::Rgba8888).len(), 4);
    }

    /// The selected layout applies until the next reset, and an unselected
    /// session is BGRA.
    #[test]
    fn scanout_layout_selection_and_reset() {
        if scanout_layout_from_env().is_some() {
            return;
        }
        reset_scanout_layout();
        assert_eq!(scanout_layout(), ScanoutLayout::Bgra8888);
        select_scanout_layout(ScanoutLayout::Rgba8888);
        assert_eq!(scanout_layout(), ScanoutLayout::Rgba8888);
        reset_scanout_layout();
        assert_eq!(scanout_layout(), ScanoutLayout::Bgra8888);
    }

    /// The connector's HDR-output signal is its colorspace alone. A metadata
    /// blob is always written — the SDR static metadata one when HDR is off —
    /// so it cannot be part of the test.
    #[test]
    fn connector_hdr_state_follows_colorspace() {
        assert!(!hdr_output_active_for(DRM_MODE_COLORIMETRY_DEFAULT));
        assert!(hdr_output_active_for(DRM_MODE_COLORIMETRY_BT2020_RGB));
        assert!(!hdr_output_active_for(DRM_MODE_COLORIMETRY_BT709_YCC));
    }

    /// Encode a payload the way the shim's `rpc()` does for a fixed struct.
    fn body_of<T: Copy>(v: &T) -> Vec<u8> {
        let mut b = Vec::new();
        pod_write(&mut b, v);
        b
    }

    fn get_cap(dev: &mut Device, cap: u64) -> u64 {
        let mut c = DrmGetCap {
            capability: cap,
            value: 0,
        };
        let r = dev.dispatch(DRM_IOCTL_GET_CAP, &body_of(&c), None);
        assert_eq!(r.ret, 0, "GET_CAP 0x{cap:x} failed");
        c = pod_read::<DrmGetCap>(&r.body).expect(&gndrm_obf::obf!("GET_CAP body"));
        c.value
    }

    /// The node-kind setup frame is what makes a connection a render node, and
    /// a render node refuses modesetting/master exactly like the kernel.
    #[test]
    fn node_kind_marks_a_render_connection_and_gates_modeset() {
        let mut d = dev();
        assert!(!d.is_render_node());
        assert_eq!(d.node_kind(), NODE_KIND_CARD);

        // Declare render node.
        let r = d.dispatch(GNDRM_IOCTL_NODE_KIND, &NODE_KIND_RENDER.to_le_bytes(), None);
        assert_eq!(r.ret, 0);
        assert!(d.is_render_node());

        // Identification still works on a render node.
        let mut v = DrmVersion {
            name_len: 0,
            ..Default::default()
        };
        let r = d.dispatch(DRM_IOCTL_VERSION, &body_of(&v), None);
        assert_eq!(r.ret, 0);
        v = pod_read::<DrmVersion>(&r.body).expect(&gndrm_obf::obf!("VERSION body"));
        assert_eq!(v.version_major, DRIVER_VERSION.0);
        assert!(v.name_len > 0);

        // Modesetting and master are refused with EACCES.
        let res = DrmModeCardRes::default();
        let r = d.dispatch(DRM_IOCTL_MODE_GETRESOURCES, &body_of(&res), None);
        assert_eq!(r.ret, -errno::EACCES);
        assert!(r.body.is_empty(), "a refused ioctl carries no body");
        assert_eq!(
            d.dispatch(DRM_IOCTL_SET_MASTER, &[], None).ret,
            -errno::EACCES
        );

        // An invalid kind is rejected and leaves the node type unchanged.
        assert_eq!(
            d.dispatch(GNDRM_IOCTL_NODE_KIND, &7u32.to_le_bytes(), None)
                .ret,
            -errno::EINVAL
        );
        assert!(d.is_render_node());
    }

    /// GET_CAP reports PRIME import+export, which is what Mesa/GBM probe.
    #[test]
    fn get_cap_reports_prime_import_and_export() {
        let mut d = dev();
        assert_eq!(
            get_cap(&mut d, DRM_CAP_PRIME),
            DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT
        );
        assert_eq!(get_cap(&mut d, DRM_CAP_DUMB_BUFFER), 1);
        // An unadvertised cap must be an error, not a silent zero.
        let r = d.dispatch(
            DRM_IOCTL_GET_CAP,
            &body_of(&DrmGetCap {
                capability: 0x9999,
                value: 0,
            }),
            None,
        );
        assert_eq!(r.ret, -errno::EINVAL);
    }

    /// GEM_CREATE allocates a real, exportable buffer; GEM_MMAP mints the
    /// handle-carrying offset; PRIME_HANDLE_TO_FD hands out its dma-buf;
    /// GEM_CLOSE releases the handle.
    #[test]
    fn gem_create_mmap_and_prime_export_form_a_lifecycle() {
        let mut d = dev();
        d.set_node_kind(NODE_KIND_RENDER);

        let req = DrmGemCreate {
            size: 4096,
            handle: 0,
            flags: 0,
        };
        let r = d.dispatch(DRM_IOCTL_GEM_CREATE, &body_of(&req), None);
        assert_eq!(r.ret, 0, "GEM_CREATE failed");
        let created = pod_read::<DrmGemCreate>(&r.body).expect(&gndrm_obf::obf!("GEM_CREATE body"));
        assert!(created.handle > 0, "GEM_CREATE must return a handle");
        assert_eq!(created.size, 4096);

        // GEM_MMAP echoes the object size and a token carrying the handle.
        let mut m = DrmGemMmap {
            handle: created.handle,
            pad: 0,
            offset: 0,
            size: 0,
        };
        let r = d.dispatch(DRM_IOCTL_GEM_MMAP, &body_of(&m), None);
        assert_eq!(r.ret, 0, "GEM_MMAP failed");
        m = pod_read::<DrmGemMmap>(&r.body).expect(&gndrm_obf::obf!("GEM_MMAP body"));
        assert_eq!(m.size, 4096);
        assert_eq!(m.offset & GEM_MMAP_OFFSET_BASE, GEM_MMAP_OFFSET_BASE);
        assert_eq!(m.offset & GEM_MMAP_HANDLE_MASK, u64::from(created.handle));

        // PRIME_HANDLE_TO_FD exports a real fd.
        let h = DrmPrimeHandle {
            handle: created.handle,
            flags: 0,
            fd: -1,
        };
        let r = d.dispatch(DRM_IOCTL_PRIME_HANDLE_TO_FD, &body_of(&h), None);
        assert_eq!(r.ret, 0, "PRIME_HANDLE_TO_FD failed");
        let exported = r
            .fd_out
            .expect(&gndrm_obf::obf!("export must attach an fd"));
        assert!(exported >= 0);
        // It is a real, sized file: fstat succeeds and reports the size.
        assert_eq!(fstat_fd(exported).map(|(_, sz)| sz), Some(4096));
        close_fd(exported);

        // GEM_CLOSE drops the handle; a second export then finds nothing.
        let c = DrmGemClose {
            handle: created.handle,
            pad: 0,
        };
        assert_eq!(d.dispatch(DRM_IOCTL_GEM_CLOSE, &body_of(&c), None).ret, 0);
        assert_eq!(
            d.dispatch(DRM_IOCTL_PRIME_HANDLE_TO_FD, &body_of(&h), None)
                .ret,
            -errno::ENOENT
        );
    }

    /// GEM_CREATE rejects a zero-sized request and an unknown handle for MMAP.
    #[test]
    fn gem_create_and_mmap_reject_bad_input() {
        let mut d = dev();
        let zero = DrmGemCreate {
            size: 0,
            handle: 0,
            flags: 0,
        };
        assert_eq!(
            d.dispatch(DRM_IOCTL_GEM_CREATE, &body_of(&zero), None).ret,
            -errno::EINVAL
        );
        let missing = DrmGemMmap {
            handle: 0xDEAD,
            pad: 0,
            offset: 0,
            size: 0,
        };
        assert_eq!(
            d.dispatch(DRM_IOCTL_GEM_MMAP, &body_of(&missing), None).ret,
            -errno::ENOENT
        );
    }
}
