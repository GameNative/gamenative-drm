//! # gndrm-daemon
//!
//! KMS emulator core. Fed decoded wire frames by the socket layer
//! (`gndrm-android`), turns each into the exact byte stream the kernel would
//! have written, plus optional fd returns and flip events.
//!
//! The crate is platform-agnostic (host-testable): the only syscalls it
//! makes are `fstat`/`dup`/`close`/`clock_gettime` on fds the wire delivers.

// Scoped unsafe lives in `pod` (audited) and in the fd helpers below.
#![deny(clippy::all)]

mod commit;
mod objects;
#[doc(hidden)] // exposed for integration tests; not public API
pub mod pod;
mod props;
pub mod server;

use std::os::fd::RawFd;

use gndrm_uapi::drm::*;
use gndrm_uapi::mode::*;
use gndrm_uapi::wire::errno;

pub use objects::{CrtcState, Framebuffer, PanelDesc, PlaneState, Stores};
pub use objects::{CONNECTOR_ID, CRTC_ID, ENCODER_ID, PLANE_CURSOR_ID, PLANE_PRIMARY_ID};
pub use props::find_prop_id;

use objects::Blob;
use pod::*;

/// Driver identity reported by `DRM_IOCTL_VERSION`.
const DRIVER_NAME: &[u8] = b"gamenative";
const DRIVER_DATE: &[u8] = b"20260923";
const DRIVER_DESC: &[u8] = b"gamenative userspace KMS";
const DRIVER_VERSION: (i32, i32, i32) = (1, 0, 0);

/// Panel size limits reported by `GETRESOURCES`.
const MIN_WIDTH: u32 = 64;
const MIN_HEIGHT: u32 = 64;
const MAX_WIDTH: u32 = 8192;
const MAX_HEIGHT: u32 = 8192;

/// Formats on the primary plane (gamescope renders XRGB8888/ARGB8888).
const PRIMARY_FORMATS: &[u32] = &[
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_RGB565,
];
/// Cursor plane formats (doc §3.2: ARGB8888, 64×64).
const CURSOR_FORMATS: &[u32] = &[DRM_FORMAT_ARGB8888];

/// What the socket layer does with one dispatch call.
#[derive(Debug)]
pub struct DispatchResult {
    /// ioctl return value: 0 or `-errno`.
    pub ret: i32,
    /// Response payload (out-struct + linearized extensions).
    pub body: Vec<u8>,
    /// fd to attach via `SCM_RIGHTS` (`PRIME_HANDLE_TO_FD`).
    pub fd_out: Option<RawFd>,
    /// Flip events to write into the read channel, in order.
    pub events: Vec<DrmEventVblank>,
}

impl DispatchResult {
    fn err(e: i32) -> Self {
        Self {
            ret: -e,
            body: Vec::new(),
            fd_out: None,
            events: Vec::new(),
        }
    }

    fn ok(body: Vec<u8>) -> Self {
        Self {
            ret: 0,
            body,
            fd_out: None,
            events: Vec::new(),
        }
    }
}

/// Sink for committed frames. The Android side implements this with
/// EGL/GLES; tests use [`NullPresenter`].
pub trait Presenter {
    /// Present the primary-plane FB of a commit. Called at most once per
    /// non-test commit that has an active CRTC. `stores` gives access to the
    /// backing dma-buf fds.
    ///
    /// `fence` is the acquire fence from the commit's `IN_FENCE_FD`
    /// (ownership transferred; the presenter must close it). Real presenters
    /// `poll()` it readable before sampling the buffer; `None` means the
    /// client passed `-1` (no fence).
    fn present(&mut self, fb: &Framebuffer, stores: &Stores, fence: Option<RawFd>);
}

/// Default presenter: drops frames. Phase-1 unit tests + headless runs.
#[derive(Debug, Default)]
pub struct NullPresenter {
    /// Number of frames presented (observability for tests).
    pub frames: u64,
}

impl Presenter for NullPresenter {
    fn present(&mut self, _fb: &Framebuffer, _stores: &Stores, fence: Option<RawFd>) {
        self.frames += 1;
        if let Some(fd) = fence {
            close_fd(fd);
        }
    }
}

type ClockFn = Box<dyn Fn() -> (u32, u32) + Send>;

/// The emulated device (one per connected client / open file description).
pub struct Device {
    panel: PanelDesc,
    mode: DrmModeModeinfo,
    stores: Stores,

    // client caps
    cap_atomic: bool,
    cap_universal_planes: bool,
    cap_writeback: bool,

    // live software state
    conn_crtc_id: u32,
    crtc: CrtcState,
    primary: PlaneState,
    cursor: PlaneState,

    presenter: Box<dyn Presenter>,
    clock: ClockFn,
    flip_sequence: u32,
}

impl Device {
    /// New device for `panel`; presents to `presenter`.
    pub fn new(panel: PanelDesc, presenter: Box<dyn Presenter>) -> Self {
        Self {
            panel,
            mode: panel.modeinfo(),
            stores: Stores::default(),
            cap_atomic: false,
            cap_universal_planes: false,
            cap_writeback: false,
            conn_crtc_id: 0,
            crtc: CrtcState::default(),
            primary: PlaneState::default(),
            cursor: PlaneState::default(),
            presenter,
            clock: Box::new(monotonic_now),
            flip_sequence: 0,
        }
    }

    /// Override the event clock (tests).
    pub fn set_clock(&mut self, clock: ClockFn) {
        self.clock = clock;
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

    pub(crate) fn crtc_state(&self) -> CrtcState {
        self.crtc.clone()
    }

    pub(crate) fn set_conn_crtc(&mut self, crtc: u32) {
        self.conn_crtc_id = crtc;
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

    /// Hand the current primary FB to the presenter. Ownership of
    /// `fence` moves to the presenter (see [`Presenter::present`]).
    pub(crate) fn present_primary(&mut self, fb_id: u32, fence: Option<RawFd>) {
        if let Some(fb) = self.stores.fbs.get(&fb_id).cloned() {
            self.presenter.present(&fb, &self.stores, fence);
        } else if let Some(fd) = fence {
            close_fd(fd);
        }
    }

    pub(crate) fn now(&self) -> (u32, u32) {
        (self.clock)()
    }

    pub(crate) fn next_flip_sequence(&mut self) -> u32 {
        self.flip_sequence += 1;
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
        let mut result = match ioctl_nr {
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
            DRM_IOCTL_MODE_OBJ_SETPROPERTY => DispatchResult::err(errno::EINVAL),
            DRM_IOCTL_MODE_CREATEPROPBLOB => self.ioctl_create_blob(payload),
            DRM_IOCTL_MODE_GETPROPBLOB => self.ioctl_get_blob(payload),
            DRM_IOCTL_MODE_DESTROYPROPBLOB => self.ioctl_destroy_blob(payload),
            DRM_IOCTL_PRIME_FD_TO_HANDLE => self.ioctl_prime_fd_to_handle(payload, fd_in.take()),
            DRM_IOCTL_PRIME_HANDLE_TO_FD => self.ioctl_prime_handle_to_fd(payload),
            DRM_IOCTL_GEM_CLOSE => self.ioctl_gem_close(payload),
            DRM_IOCTL_MODE_ADDFB2 => self.ioctl_addfb2(payload),
            DRM_IOCTL_MODE_GETFB2 => self.ioctl_getfb2(payload),
            DRM_IOCTL_MODE_RMFB => self.ioctl_rmfb(payload),
            DRM_IOCTL_MODE_ATOMIC => self.ioctl_atomic(payload, fd_in.take()),
            _ => DispatchResult::err(errno::EINVAL),
        };
        // Drop an unclaimed fd exactly like an ioctl that ignores it would
        // (the kernel simply doesn't consume the reference — but the wire
        // gave *us* one, so we must close it to avoid leaking).
        if let Some(fd) = fd_in.take() {
            close_fd(fd);
        }
        if result.ret != 0 {
            result.fd_out = None;
            result.events.clear();
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
            DRM_CAP_TIMESTAMP_MONOTONIC => 1,
            DRM_CAP_CURSOR_WIDTH | DRM_CAP_CURSOR_HEIGHT => 64,
            // Deliberately absent (doc §3.1): gamescope takes fallbacks.
            DRM_CAP_SYNCOBJ | DRM_CAP_ADDFB2_MODIFIERS => 0,
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
            _ => return DispatchResult::err(errno::EINVAL),
        }
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
        c.mode = self.mode;
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
        let modes = [self.mode];
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
        let vals = vec![
            u64::from(self.conn_crtc_id),
            0, // panel orientation: Normal (phase 3: from display rotation)
        ];
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
            PLANE_PRIMARY_ID => (&self.primary, PRIMARY_FORMATS),
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
            (_, P_PLANE_IN_FENCE_FD) => u64::from(u32::MAX), // -1: no fence
            _ => 0,
        }
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
        let id = self.stores.alloc_blob();
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
        DispatchResult::ok(Vec::new())
    }

    // -----------------------------------------------------------------
    // GEM / FB lifecycle (task A5)
    // -----------------------------------------------------------------

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
        let handle = self.stores.alloc_gem();
        self.stores
            .gems
            .insert(handle, objects::GemObject { fd, ino, size });
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

    fn ioctl_gem_close(&mut self, payload: &[u8]) -> DispatchResult {
        let Some(c) = pod_read::<DrmGemClose>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        // Kernel: closing a handle an FB still uses is allowed — the FB keeps
        // the buffer alive. Our FBs reference GEM handles, so refuse instead
        // of dangling (gamescope never does this; EINVAL surfaces bugs early).
        if self
            .stores
            .fbs
            .values()
            .any(|fb| fb.handles.contains(&c.handle))
        {
            return DispatchResult::err(errno::EBUSY);
        }
        match self.stores.gems.remove(&c.handle) {
            Some(gem) => {
                self.stores.gem_by_ino.remove(&gem.ino);
                close_fd(gem.fd);
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
        // ADDFB2_MODIFIERS cap is 0: reject modifier-flagged requests so
        // gamescope sticks to the implicit-linear path it probed for.
        if cmd.flags & DRM_MODE_FB_MODIFIERS != 0 {
            return DispatchResult::err(errno::EINVAL);
        }
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
        let gem_size = self.stores.gems[&cmd.handles[0]].size;
        if need > gem_size {
            return DispatchResult::err(errno::EINVAL);
        }
        for &h in &cmd.handles {
            if h != 0 && !self.stores.gems.contains_key(&h) {
                return DispatchResult::err(errno::ENOENT);
            }
        }
        let id = self.stores.alloc_fb();
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
        let mut body = Vec::new();
        pod_write(
            &mut body,
            &DrmModeFbCmd2 {
                fb_id: fb.id,
                width: fb.width,
                height: fb.height,
                pixel_format: fb.pixel_format,
                flags: fb.flags,
                handles: fb.handles,
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
        // Kernel refuses to remove a framebuffer that is still scanned out.
        if self.primary.fb_id == fb_id || self.cursor.fb_id == fb_id {
            return DispatchResult::err(errno::EBUSY);
        }
        if self.stores.fbs.remove(&fb_id).is_none() {
            return DispatchResult::err(errno::EINVAL);
        }
        DispatchResult::ok(Vec::new())
    }
}

/// Bits-per-pixel for the formats we accept (single-plane only in phase 1).
fn format_bpp(fourcc: u32) -> Option<u32> {
    match fourcc {
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 | DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => {
            Some(32)
        }
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

/// `dup(fd)`, honouring `DRM_CLOEXEC`/`DRM_RDWR` in `flags` (as `HANDLE_TO_FD`).
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
