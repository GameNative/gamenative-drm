//! Golden tests: replay gamescope's discovery → modeset → flip sequence
//! against the daemon exactly as a libdrm client would drive it, asserting on
//! the *bytes* returned (doc `02-gamenative-drm.md` §8, tasks A3–A7).

use std::collections::HashSet;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gndrm_daemon::pod::{pod_read, pod_write};
use gndrm_daemon::{
    find_prop_id, Device, DispatchResult, Framebuffer, NullPresenter, PanelDesc, PlaneState,
    Presenter, CONNECTOR_ID, CRTC_ID, ENCODER_ID, PLANE_CURSOR_ID, PLANE_PRIMARY_ID,
};
use gndrm_uapi::drm::*;
use gndrm_uapi::mode::*;
use gndrm_uapi::wire::errno;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn dev() -> Device {
    let mut d = Device::new(PanelDesc::default(), Box::<NullPresenter>::default());
    d.set_clock(Box::new(|| (1000, 500))); // deterministic event timestamps
    d
}

/// A presenter whose only job is to count the frames it is handed. The device
/// never waits on a presenter completion, so the mocks need only accept the
/// publish.
struct CountPresenter {
    frames: Arc<AtomicU64>,
}

impl CountPresenter {
    fn new(frames: Arc<AtomicU64>) -> Self {
        Self { frames }
    }
}

impl Presenter for CountPresenter {
    fn publish(
        &mut self,
        _fb: &Framebuffer,
        _plane: &PlaneState,
        _ino: Option<u64>,
        _live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    ) {
        if let Some(fd) = fence {
            // SAFETY: the mock owns the fence the device handed it.
            unsafe { libc::close(fd) };
        }
        self.frames.fetch_add(1, Ordering::Relaxed);
    }
}

/// A device with a counting presenter.
fn dev_async() -> Device {
    let mut d = Device::new(
        PanelDesc::default(),
        Box::new(CountPresenter::new(Arc::new(AtomicU64::new(0)))),
    );
    d.set_clock(Box::new(|| (1000, 500)));
    d
}

/// Block (up to `timeout_ms`) until the worker has produced at least one flip
/// event, then drain and return them in commit order. Polls the queue directly
/// rather than the eventfd, so a wake that races the queue's visibility cannot
/// return an empty batch.
fn wait_flip(d: &mut Device, timeout_ms: u64) -> Vec<DrmEventVblank> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let events = d.drain_flip_events();
        if !events.is_empty() {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "no flip event within {timeout_ms}ms"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Block until at least `want` flip events have arrived, then drain and return
/// them in commit order. Two back-to-back async commits legitimately queue two
/// events before a reader looks — the worker emits each event with its own
/// publish and never waits for a latch — so a caller asserting one event per
/// commit must collect to a count, not assume a lone event per drain.
fn wait_flips(d: &mut Device, want: usize, timeout_ms: u64) -> Vec<DrmEventVblank> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut events = Vec::new();
    while events.len() < want {
        events.extend(d.drain_flip_events());
        if events.len() >= want {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "only {} of {want} flip events within {timeout_ms}ms",
            events.len()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    events
}

fn call(d: &mut Device, nr: u32, payload: &[u8]) -> DispatchResult {
    d.dispatch(nr, payload, None)
}

fn call_fd(d: &mut Device, nr: u32, payload: &[u8], fd: RawFd) -> DispatchResult {
    // The wire's SCM_RIGHTS duplicates the descriptor into the daemon; the
    // client keeps its own reference. Simulate that so the daemon and the test
    // never share (and double-close) a single fd. SAFETY: dup of a live fd.
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    assert!(dup >= 0, "dup fd for SCM_RIGHTS");
    d.dispatch(nr, payload, Some(dup))
}

fn ok(r: &DispatchResult) {
    assert_eq!(r.ret, 0, "expected success, got {}", r.ret);
}

fn is_err(r: &DispatchResult, e: i32) {
    assert_eq!(r.ret, -e, "expected -{e}, got {}", r.ret);
}

fn pod<T: Copy>(v: &T) -> Vec<u8> {
    let mut b = Vec::new();
    pod_write(&mut b, v);
    b
}

/// `memfd_create` — stands in for a dma-buf in lifecycle tests (fstat-able,
/// dup-able, uniquely `st_ino`-identifiable, same as the real thing).
fn memfd(size: i64) -> RawFd {
    // SAFETY: valid name, standard flags.
    let fd = unsafe { libc::memfd_create(c"gndrm-test".as_ptr(), 0) };
    assert!(fd >= 0);
    // SAFETY: fd is valid.
    assert_eq!(unsafe { libc::ftruncate(fd, size) }, 0);
    fd
}

/// Build a linearized ATOMIC request payload (struct + extension).
fn atomic_req(sets: &[(u32, u32, u64)], flags: u32, user_data: u64) -> Vec<u8> {
    let mut objs: Vec<u32> = Vec::new();
    let mut counts: Vec<u32> = Vec::new();
    for (obj, _, _) in sets {
        match objs.iter().position(|o| o == obj) {
            Some(i) => counts[i] += 1,
            None => {
                objs.push(*obj);
                counts.push(1);
            }
        }
    }
    let mut out = pod(&DrmModeAtomic {
        flags,
        count_objs: objs.len() as u32,
        user_data,
        ..Default::default()
    });
    for o in &objs {
        out.extend_from_slice(&o.to_le_bytes());
    }
    for c in &counts {
        out.extend_from_slice(&c.to_le_bytes());
    }
    for (_, p, _) in sets {
        out.extend_from_slice(&p.to_le_bytes());
    }
    for (_, _, v) in sets {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// A3: device ioctls + GETRESOURCES
// ---------------------------------------------------------------------------

#[test]
fn version_two_call_semantics() {
    let mut d = dev();
    // Call 1: all lengths 0 → daemon reports full string lengths.
    let r = call(&mut d, DRM_IOCTL_VERSION, &pod(&DrmVersion::default()));
    ok(&r);
    let v: DrmVersion = pod_read(&r.body).unwrap();
    assert_eq!((v.version_major, v.version_minor), (1, 0));
    assert_eq!(v.name_len, b"gamenative".len());
    let off = core::mem::size_of::<DrmVersion>();
    let strings = &r.body[off..];
    assert!(strings
        .windows(b"gamenative".len())
        .any(|w| w == b"gamenative"));

    // Call 2: buffers offered (as nonzero ptrs) → same lengths + strings.
    let r2 = call(
        &mut d,
        DRM_IOCTL_VERSION,
        &pod(&DrmVersion {
            name_len: 64,
            name: 0xDEAD0000,
            date_len: 64,
            date: 0xDEAD1000,
            desc_len: 64,
            desc: 0xDEAD2000,
            ..Default::default()
        }),
    );
    ok(&r2);
    // Pointer fields echo the caller's values (kernel behaviour); compare the
    // meaningful output: version, lengths, and the marshalled strings.
    let v2: DrmVersion = pod_read(&r2.body).unwrap();
    let v1: DrmVersion = pod_read(&r.body).unwrap();
    assert_eq!(v2.version_major, v1.version_major);
    assert_eq!(v2.name_len, v1.name_len);
    assert_eq!(v2.date_len, v1.date_len);
    assert_eq!(v2.desc_len, v1.desc_len);
    let off = core::mem::size_of::<DrmVersion>();
    assert_eq!(r2.body[off..], r.body[off..]);
}

#[test]
fn get_cap_table() {
    let mut d = dev();
    let mut cap = |c: u64| {
        let r = call(
            &mut d,
            DRM_IOCTL_GET_CAP,
            &pod(&DrmGetCap {
                capability: c,
                value: 0,
            }),
        );
        (r.ret, pod_read::<DrmGetCap>(&r.body).map(|g| g.value))
    };
    assert_eq!(cap(DRM_CAP_DUMB_BUFFER), (0, Some(1)));
    assert_eq!(cap(DRM_CAP_TIMESTAMP_MONOTONIC), (0, Some(1)));
    assert_eq!(cap(DRM_CAP_CURSOR_WIDTH), (0, Some(64)));
    assert_eq!(cap(DRM_CAP_CURSOR_HEIGHT), (0, Some(64)));
    assert_eq!(cap(DRM_CAP_SYNCOBJ), (0, Some(1)));
    assert_eq!(cap(DRM_CAP_ADDFB2_MODIFIERS), (0, Some(1)));
    assert_eq!(cap(0xDEAD).0, -errno::EINVAL);
}

/// gamescope's commit path: one always-signalled sync object, exported once and
/// then ridden as the primary plane's in-fence on every commit. The export has
/// to be poll-ready, since the presenter polls the in-fence before sampling.
#[test]
fn syncobj_create_signalled_export_is_poll_ready() {
    let mut d = dev();
    let mut c = DrmSyncobjCreate {
        handle: 0,
        flags: DRM_SYNCOBJ_CREATE_SIGNALED,
    };
    let r = call(&mut d, DRM_IOCTL_SYNCOBJ_CREATE, &pod(&c));
    ok(&r);
    c = pod_read(&r.body).expect("create reply");
    assert_ne!(c.handle, 0, "handle allocated");

    let h = DrmSyncobjHandle {
        handle: c.handle,
        flags: 0,
        fd: 0,
        pad: 0,
        point: 0,
    };
    let r = call(&mut d, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &pod(&h));
    ok(&r);
    let fd = r.fd_out.expect("export fd");

    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `p` is a valid single pollfd.
    assert_eq!(unsafe { libc::poll(&mut p, 1, 0) }, 1, "fence is readable");
    assert_ne!(p.revents & libc::POLLIN, 0);
    // SAFETY: the test owns `fd`.
    assert_eq!(unsafe { libc::close(fd) }, 0);

    let r = call(
        &mut d,
        DRM_IOCTL_SYNCOBJ_DESTROY,
        &pod(&DrmSyncobjDestroy {
            handle: c.handle,
            pad: 0,
        }),
    );
    ok(&r);
    is_err(
        &call(&mut d, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &pod(&h)),
        errno::ENOENT,
    );
}

/// Only signalled objects can be exported: an unsignalled one would need a real
/// dma-fence behind the fd, which this device does not model.
#[test]
fn syncobj_unsignalled_export_is_rejected() {
    let mut d = dev();
    let r = call(
        &mut d,
        DRM_IOCTL_SYNCOBJ_CREATE,
        &pod(&DrmSyncobjCreate {
            handle: 0,
            flags: 0,
        }),
    );
    ok(&r);
    let c = pod_read::<DrmSyncobjCreate>(&r.body).expect("create reply");
    is_err(
        &call(
            &mut d,
            DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD,
            &pod(&DrmSyncobjHandle {
                handle: c.handle,
                flags: 0,
                fd: 0,
                pad: 0,
                point: 0,
            }),
        ),
        errno::EINVAL,
    );
}

#[test]
fn set_client_cap_gating() {
    let mut d = dev();
    for c in [
        DRM_CLIENT_CAP_ATOMIC,
        DRM_CLIENT_CAP_UNIVERSAL_PLANES,
        DRM_CLIENT_CAP_WRITEBACK_CONNECTORS,
    ] {
        ok(&call(
            &mut d,
            DRM_IOCTL_SET_CLIENT_CAP,
            &pod(&DrmSetClientCap {
                capability: c,
                value: 1,
            }),
        ));
    }
    // value > 1 → EINVAL; unknown cap → EINVAL (kernel behaviour).
    is_err(
        &call(
            &mut d,
            DRM_IOCTL_SET_CLIENT_CAP,
            &pod(&DrmSetClientCap {
                capability: DRM_CLIENT_CAP_ATOMIC,
                value: 2,
            }),
        ),
        errno::EINVAL,
    );
    is_err(
        &call(
            &mut d,
            DRM_IOCTL_SET_CLIENT_CAP,
            &pod(&DrmSetClientCap {
                capability: 99,
                value: 1,
            }),
        ),
        errno::EINVAL,
    );
}

#[test]
fn get_resources_golden() {
    let mut d = dev();
    // Discovery call: counts only.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETRESOURCES,
        &pod(&DrmModeCardRes::default()),
    );
    ok(&r);
    let res: DrmModeCardRes = pod_read(&r.body).unwrap();
    assert_eq!(res.count_crtcs, 1);
    assert_eq!(res.count_connectors, 1);
    assert_eq!(res.count_encoders, 1);
    assert_eq!(res.count_fbs, 0);
    assert!(res.min_width <= 1280 && res.max_width >= 1280);
    assert_eq!(r.body.len(), core::mem::size_of::<DrmModeCardRes>()); // no arrays

    // Fetch call: array ptrs set, capacities offered.
    let r2 = call(
        &mut d,
        DRM_IOCTL_MODE_GETRESOURCES,
        &pod(&DrmModeCardRes {
            crtc_id_ptr: 0x1000,
            connector_id_ptr: 0x2000,
            encoder_id_ptr: 0x3000,
            count_crtcs: 1,
            count_connectors: 1,
            count_encoders: 1,
            ..Default::default()
        }),
    );
    ok(&r2);
    let res2: DrmModeCardRes = pod_read(&r2.body).unwrap();
    assert_eq!(res2.count_crtcs, 1);
    let off = core::mem::size_of::<DrmModeCardRes>();
    let ids: &[u8] = &r2.body[off..];
    assert_eq!(ids.len(), 12); // 1 crtc + 1 conn + 1 enc, u32 each
    assert_eq!(u32::from_le_bytes(ids[0..4].try_into().unwrap()), CRTC_ID);
    assert_eq!(
        u32::from_le_bytes(ids[4..8].try_into().unwrap()),
        CONNECTOR_ID
    );
    assert_eq!(
        u32::from_le_bytes(ids[8..12].try_into().unwrap()),
        ENCODER_ID
    );
}

// ---------------------------------------------------------------------------
// A4: object queries
// ---------------------------------------------------------------------------

#[test]
fn connector_encoder_plane_queries() {
    let mut d = dev();

    // Connector, two-call.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETCONNECTOR,
        &pod(&DrmModeGetConnector {
            connector_id: CONNECTOR_ID,
            ..Default::default()
        }),
    );
    ok(&r);
    let c: DrmModeGetConnector = pod_read(&r.body).unwrap();
    assert_eq!(c.connection, DRM_MODE_CONNECTED);
    assert_eq!(c.connector_type, DRM_MODE_CONNECTOR_DSI);
    assert_eq!(c.count_modes, 1);
    assert_eq!(c.count_encoders, 1);
    assert_eq!(c.count_props, 5); // CRTC_ID, panel orientation, EDID, Colorspace, HDR_OUTPUT_METADATA
    assert_eq!(c.encoder_id, ENCODER_ID);

    // With arrays.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETCONNECTOR,
        &pod(&DrmModeGetConnector {
            connector_id: CONNECTOR_ID,
            encoders_ptr: 1,
            modes_ptr: 1,
            props_ptr: 1,
            prop_values_ptr: 1,
            count_modes: 1,
            count_encoders: 1,
            count_props: 2,
            ..Default::default()
        }),
    );
    ok(&r);
    let off = core::mem::size_of::<DrmModeGetConnector>();
    let ext = &r.body[off..];
    // encoders[1] | modes[1] | props[2] | values[2]
    let want = 4 + core::mem::size_of::<DrmModeModeinfo>() + 8 + 16;
    assert_eq!(ext.len(), want);
    let m: DrmModeModeinfo = pod_read(&ext[4..]).unwrap();
    assert_eq!((m.hdisplay, m.vdisplay), (1280, 720));
    assert_eq!(m.vrefresh, 1200);
    assert_ne!(m.ty & DRM_MODE_TYPE_PREFERRED, 0);

    // Unknown connector id.
    is_err(
        &call(
            &mut d,
            DRM_IOCTL_MODE_GETCONNECTOR,
            &pod(&DrmModeGetConnector {
                connector_id: 0xFFFF,
                ..Default::default()
            }),
        ),
        errno::ENOENT,
    );

    // Encoder.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETENCODER,
        &pod(&DrmModeGetEncoder {
            encoder_id: ENCODER_ID,
            ..Default::default()
        }),
    );
    ok(&r);
    let e: DrmModeGetEncoder = pod_read(&r.body).unwrap();
    assert_eq!(e.encoder_type, DRM_MODE_ENCODER_VIRTUAL);
    assert_eq!(e.crtc_id, CRTC_ID);
    assert_eq!(e.possible_crtcs, 1);

    // Plane resources.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETPLANERESOURCES,
        &pod(&DrmModeGetPlaneRes {
            plane_id_ptr: 1,
            count_planes: 2,
        }),
    );
    ok(&r);
    let off = core::mem::size_of::<DrmModeGetPlaneRes>();
    let ids = &r.body[off..];
    assert_eq!(
        u32::from_le_bytes(ids[0..4].try_into().unwrap()),
        PLANE_PRIMARY_ID
    );
    assert_eq!(
        u32::from_le_bytes(ids[4..8].try_into().unwrap()),
        PLANE_CURSOR_ID
    );

    // Primary plane: formats.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETPLANE,
        &pod(&DrmModeGetPlane {
            plane_id: PLANE_PRIMARY_ID,
            format_type_ptr: 1,
            count_format_types: 16,
            ..Default::default()
        }),
    );
    ok(&r);
    let p: DrmModeGetPlane = pod_read(&r.body).unwrap();
    assert_eq!(p.count_format_types, 3);
    assert_eq!(p.possible_crtcs, 1);
    let off = core::mem::size_of::<DrmModeGetPlane>();
    let fmts: Vec<u32> = r.body[off..off + 12]
        .chunks(4)
        .map(|f| u32::from_le_bytes(f.try_into().unwrap()))
        .collect();
    assert_eq!(
        fmts,
        [DRM_FORMAT_XBGR8888, DRM_FORMAT_ABGR8888, DRM_FORMAT_RGB565]
    );

    // Cursor plane: the B-low pair the primary plane leaves out.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETPLANE,
        &pod(&DrmModeGetPlane {
            plane_id: PLANE_CURSOR_ID,
            format_type_ptr: 1,
            count_format_types: 16,
            ..Default::default()
        }),
    );
    ok(&r);
    let p: DrmModeGetPlane = pod_read(&r.body).unwrap();
    assert_eq!(p.count_format_types, 2);
    let fmts: Vec<u32> = r.body[off..off + 8]
        .chunks(4)
        .map(|f| u32::from_le_bytes(f.try_into().unwrap()))
        .collect();
    assert_eq!(fmts, [DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888]);
}

#[test]
fn property_queries_golden() {
    let mut d = dev();

    // OBJ_GETPROPERTIES on the CRTC.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
        &pod(&DrmModeObjGetProperties {
            obj_id: CRTC_ID,
            obj_type: DRM_MODE_OBJECT_CRTC,
            props_ptr: 1,
            prop_values_ptr: 1,
            count_props: 8,
        }),
    );
    ok(&r);
    let g: DrmModeObjGetProperties = pod_read(&r.body).unwrap();
    assert_eq!(g.count_props, 2); // ACTIVE, MODE_ID
    let off = core::mem::size_of::<DrmModeObjGetProperties>();
    let ids = &r.body[off..off + 8];
    let mode_id_prop = find_prop_id(CRTC_ID, "MODE_ID").unwrap();
    let active_prop = find_prop_id(CRTC_ID, "ACTIVE").unwrap();
    let got: Vec<u32> = ids
        .chunks(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert!(got.contains(&mode_id_prop) && got.contains(&active_prop));

    // Wrong obj_type → ENOENT.
    is_err(
        &call(
            &mut d,
            DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
            &pod(&DrmModeObjGetProperties {
                obj_id: CRTC_ID,
                obj_type: DRM_MODE_OBJECT_CONNECTOR,
                ..Default::default()
            }),
        ),
        errno::ENOENT,
    );

    // GETPROPERTY: MODE_ID is ATOMIC|BLOB, no values.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETPROPERTY,
        &pod(&DrmModeGetProperty {
            prop_id: mode_id_prop,
            ..Default::default()
        }),
    );
    ok(&r);
    let p: DrmModeGetProperty = pod_read(&r.body).unwrap();
    assert_ne!(p.flags & DRM_MODE_PROP_ATOMIC, 0);
    assert_ne!(p.flags & DRM_MODE_PROP_BLOB, 0);
    assert_eq!(&p.name[..7], b"MODE_ID");
    assert_eq!(p.count_values, 0);
    assert_eq!(p.count_enum_blobs, 0);

    // GETPROPERTY: plane "type" is IMMUTABLE|ENUM with 3 entries.
    let type_prop = find_prop_id(PLANE_PRIMARY_ID, "type").unwrap();
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETPROPERTY,
        &pod(&DrmModeGetProperty {
            prop_id: type_prop,
            values_ptr: 1,
            enum_blob_ptr: 1,
            count_values: 4,
            count_enum_blobs: 4,
            ..Default::default()
        }),
    );
    ok(&r);
    let p: DrmModeGetProperty = pod_read(&r.body).unwrap();
    assert_ne!(p.flags & DRM_MODE_PROP_IMMUTABLE, 0);
    assert_ne!(p.flags & DRM_MODE_PROP_ENUM, 0);
    assert_eq!(p.count_enum_blobs, 3);
    let off = core::mem::size_of::<DrmModeGetProperty>();
    let vals = &r.body[off..off + 24]; // 3 u64 values
    let enums = &r.body[off + 24..off + 24 + 3 * core::mem::size_of::<DrmModePropertyEnum>()];
    assert_eq!(u64::from_le_bytes(vals[8..16].try_into().unwrap()), 1); // PRIMARY
    let e1: DrmModePropertyEnum = pod_read(&enums[40..]).unwrap();
    assert_eq!(e1.value, 1);
    assert_eq!(&e1.name[..7], b"Primary");

    // GETPROPERTY: plane IN_FENCE_FD is ATOMIC|SIGNED_RANGE [-1, INT_MAX]
    // (kernel drm_mode_config.c:488). libliftoff range-checks this
    // client-side; an unsigned range rejects the -1 "no fence" sentinel
    // and silently kills every plane allocation (failure #17).
    let fence_prop = find_prop_id(PLANE_PRIMARY_ID, "IN_FENCE_FD").unwrap();
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETPROPERTY,
        &pod(&DrmModeGetProperty {
            prop_id: fence_prop,
            values_ptr: 1,
            count_values: 2,
            ..Default::default()
        }),
    );
    ok(&r);
    let p: DrmModeGetProperty = pod_read(&r.body).unwrap();
    assert_ne!(p.flags & DRM_MODE_PROP_ATOMIC, 0);
    assert_ne!(p.flags & DRM_MODE_PROP_SIGNED_RANGE, 0);
    assert_eq!(p.flags & DRM_MODE_PROP_RANGE, 0);
    assert_eq!(p.count_values, 2);
    let off = core::mem::size_of::<DrmModeGetProperty>();
    let vals = &r.body[off..off + 16];
    let min = u64::from_le_bytes(vals[0..8].try_into().unwrap()) as i64;
    let max = u64::from_le_bytes(vals[8..16].try_into().unwrap()) as i64;
    assert_eq!(min, -1);
    assert_eq!(max, i64::from(i32::MAX));

    // Unknown property.
    is_err(
        &call(
            &mut d,
            DRM_IOCTL_MODE_GETPROPERTY,
            &pod(&DrmModeGetProperty {
                prop_id: 0xDEAD,
                ..Default::default()
            }),
        ),
        errno::ENOENT,
    );
}

#[test]
fn blob_lifecycle() {
    let mut d = dev();
    let mode = PanelDesc::default().modeinfo();
    let mut req = pod(&DrmModeCreateBlob {
        length: core::mem::size_of::<DrmModeModeinfo>() as u32,
        ..Default::default()
    });
    req.extend_from_slice(&pod(&mode));
    let r = call(&mut d, DRM_IOCTL_MODE_CREATEPROPBLOB, &req);
    ok(&r);
    let c: DrmModeCreateBlob = pod_read(&r.body).unwrap();
    assert_ne!(c.blob_id, 0);

    // Two-call GETPROPBLOB.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETPROPBLOB,
        &pod(&DrmModeGetBlob {
            blob_id: c.blob_id,
            ..Default::default()
        }),
    );
    ok(&r);
    let g: DrmModeGetBlob = pod_read(&r.body).unwrap();
    assert_eq!(g.length as usize, core::mem::size_of::<DrmModeModeinfo>());

    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETPROPBLOB,
        &pod(&DrmModeGetBlob {
            blob_id: c.blob_id,
            length: g.length,
            data: 1,
        }),
    );
    ok(&r);
    let off = core::mem::size_of::<DrmModeGetBlob>();
    let m: DrmModeModeinfo = pod_read(&r.body[off..]).unwrap();
    assert_eq!((m.hdisplay, m.vdisplay), (1280, 720));

    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_DESTROYPROPBLOB,
        &pod(&DrmModeDestroyBlob { blob_id: c.blob_id }),
    ));
    is_err(
        &call(
            &mut d,
            DRM_IOCTL_MODE_GETPROPBLOB,
            &pod(&DrmModeGetBlob {
                blob_id: c.blob_id,
                ..Default::default()
            }),
        ),
        errno::ENOENT,
    );
}

/// Both planes must serve a fetchable `IN_FORMATS` blob: libliftoff creates
/// a plane object for *every* plane and aborts DRM backend creation when the
/// blob lookup fails (a zero/garbage value yields ENOENT).
#[test]
fn in_formats_blob_on_both_planes() {
    let mut d = dev();
    for (plane, want) in [
        (
            PLANE_PRIMARY_ID,
            &[DRM_FORMAT_XBGR8888, DRM_FORMAT_ABGR8888, DRM_FORMAT_RGB565][..],
        ),
        (
            PLANE_CURSOR_ID,
            &[DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888][..],
        ),
    ] {
        let r = call(
            &mut d,
            DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
            &pod(&DrmModeObjGetProperties {
                obj_id: plane,
                obj_type: DRM_MODE_OBJECT_PLANE,
                props_ptr: 1,
                prop_values_ptr: 1,
                count_props: 32,
            }),
        );
        ok(&r);
        let g: DrmModeObjGetProperties = pod_read(&r.body).unwrap();
        let off = core::mem::size_of::<DrmModeObjGetProperties>();
        let n = g.count_props as usize;
        let ids: Vec<u32> = r.body[off..off + 4 * n]
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let vals: Vec<u64> = r.body[off + 4 * n..off + 12 * n]
            .chunks(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let in_formats = find_prop_id(plane, "IN_FORMATS").unwrap();
        let pos = ids.iter().position(|&id| id == in_formats).unwrap();
        let blob_id = u32::try_from(vals[pos]).unwrap();
        assert_ne!(blob_id, 0, "plane {plane}: IN_FORMATS must name a blob");

        let r = call(
            &mut d,
            DRM_IOCTL_MODE_GETPROPBLOB,
            &pod(&DrmModeGetBlob {
                blob_id,
                ..Default::default()
            }),
        );
        ok(&r);
        let g: DrmModeGetBlob = pod_read(&r.body).unwrap();
        let r = call(
            &mut d,
            DRM_IOCTL_MODE_GETPROPBLOB,
            &pod(&DrmModeGetBlob {
                blob_id,
                length: g.length,
                data: 1,
            }),
        );
        ok(&r);
        let off = core::mem::size_of::<DrmModeGetBlob>();
        let blob = &r.body[off..];
        let u32at = |i: usize| u32::from_le_bytes(blob[4 * i..4 * i + 4].try_into().unwrap());
        assert_eq!(u32at(0), 1, "drm_format_modifier_blob version");
        let n = want.len();
        assert_eq!(u32at(2) as usize, n, "count_formats");
        assert_eq!(u32at(3), 24, "formats_offset");
        assert_eq!(u32at(4), 1, "count_modifiers");
        assert_eq!(u32at(5) as usize, 24 + 4 * n, "modifiers_offset");
        let fmts: Vec<u32> = (0..n).map(|i| u32at(6 + i)).collect();
        assert_eq!(fmts, want, "plane {plane}: IN_FORMATS formats");
        // Modifier entry: every format at DRM_FORMAT_MOD_LINEAR (0).
        let m = 24 + 4 * n;
        assert_eq!(
            u64::from_le_bytes(blob[m..m + 8].try_into().unwrap()),
            (1u64 << n) - 1
        );
        assert_eq!(
            u64::from_le_bytes(blob[m + 16..m + 24].try_into().unwrap()),
            DRM_FORMAT_MOD_LINEAR
        );
    }
}

// ---------------------------------------------------------------------------
// A5: PRIME / FB lifecycle
// ---------------------------------------------------------------------------

#[test]
fn prime_import_dedup_and_close() {
    let mut d = dev();
    let fd = memfd(4096);

    // First import → new handle.
    let r = call_fd(
        &mut d,
        DRM_IOCTL_PRIME_FD_TO_HANDLE,
        &pod(&DrmPrimeHandle::default()),
        fd,
    );
    ok(&r);
    let h1 = pod_read::<DrmPrimeHandle>(&r.body).unwrap().handle;
    assert_ne!(h1, 0);

    // Re-import a dup of the same buffer → SAME handle (dedup by st_ino).
    // SAFETY: fd valid.
    let dup = unsafe { libc::dup(fd) };
    let r = call_fd(
        &mut d,
        DRM_IOCTL_PRIME_FD_TO_HANDLE,
        &pod(&DrmPrimeHandle::default()),
        dup,
    );
    ok(&r);
    let h2 = pod_read::<DrmPrimeHandle>(&r.body).unwrap().handle;
    assert_eq!(h1, h2);

    // A different buffer → different handle.
    let fd2 = memfd(4096);
    let r = call_fd(
        &mut d,
        DRM_IOCTL_PRIME_FD_TO_HANDLE,
        &pod(&DrmPrimeHandle::default()),
        fd2,
    );
    ok(&r);
    let h3 = pod_read::<DrmPrimeHandle>(&r.body).unwrap().handle;
    assert_ne!(h1, h3);

    // HANDLE_TO_FD round-trips through fd_out.
    let r = call(
        &mut d,
        DRM_IOCTL_PRIME_HANDLE_TO_FD,
        &pod(&DrmPrimeHandle {
            handle: h3,
            flags: 0,
            fd: 0,
        }),
    );
    ok(&r);
    let out_fd = r.fd_out.expect("must return an fd");
    // SAFETY: valid fd, checked non-negative.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    assert_eq!(unsafe { libc::fstat(out_fd, &mut st) }, 0);
    assert_eq!(st.st_size, 4096);
    // SAFETY: valid fd.
    unsafe { libc::close(out_fd) };

    // Close both handles.
    for h in [h1, h3] {
        ok(&call(
            &mut d,
            DRM_IOCTL_GEM_CLOSE,
            &pod(&DrmGemClose { handle: h, pad: 0 }),
        ));
    }
    is_err(
        &call(
            &mut d,
            DRM_IOCTL_GEM_CLOSE,
            &pod(&DrmGemClose { handle: h1, pad: 0 }),
        ),
        errno::EINVAL,
    );
    // SAFETY: caller still owns the original fd (kernel semantics).
    unsafe { libc::close(fd) };
}

#[test]
fn addfb2_validation_and_rmfb() {
    let mut d = dev();
    let fd = memfd(1280 * 720 * 4);
    let r = call_fd(
        &mut d,
        DRM_IOCTL_PRIME_FD_TO_HANDLE,
        &pod(&DrmPrimeHandle::default()),
        fd,
    );
    let handle = pod_read::<DrmPrimeHandle>(&r.body).unwrap().handle;

    let base = DrmModeFbCmd2 {
        width: 1280,
        height: 720,
        pixel_format: DRM_FORMAT_XBGR8888,
        handles: [handle, 0, 0, 0],
        pitches: [1280 * 4, 0, 0, 0],
        offsets: [0; 4],
        modifier: [0; 4],
        ..Default::default()
    };

    // Happy path.
    let r = call(&mut d, DRM_IOCTL_MODE_ADDFB2, &pod(&base));
    ok(&r);
    let fb_id = pod_read::<DrmModeFbCmd2>(&r.body).unwrap().fb_id;
    assert_ne!(fb_id, 0);

    // Pitch too small.
    let mut bad = base;
    bad.pitches[0] = 100;
    is_err(
        &call(&mut d, DRM_IOCTL_MODE_ADDFB2, &pod(&bad)),
        errno::EINVAL,
    );

    // Unknown GEM handle.
    let mut bad = base;
    bad.handles[0] = 0x777;
    is_err(
        &call(&mut d, DRM_IOCTL_MODE_ADDFB2, &pod(&bad)),
        errno::ENOENT,
    );

    // A modifier-flagged request is accepted and registered. A compressed
    // (UBWC) scan-out framebuffer carries a non-LINEAR modifier; its real
    // allocation length must enter the pool's length index so a later
    // length-only scan-out request for that length resolves (the round-U
    // refusal was this framebuffer being rejected before the note).
    let mut bad = base;
    bad.flags = DRM_MODE_FB_MODIFIERS;
    bad.modifier[0] = 0x0100_0000_0000_0002; // some tiling modifier
    ok(&call(&mut d, DRM_IOCTL_MODE_ADDFB2, &pod(&bad)));

    // Unknown format.
    let mut bad = base;
    bad.pixel_format = gndrm_uapi::mode::fourcc(b'N', b'V', b'1', b'2');
    is_err(
        &call(&mut d, DRM_IOCTL_MODE_ADDFB2, &pod(&bad)),
        errno::EINVAL,
    );

    // RMFB works; double-RMFB fails.
    ok(&call(&mut d, DRM_IOCTL_MODE_RMFB, &pod(&fb_id)));
    is_err(
        &call(&mut d, DRM_IOCTL_MODE_RMFB, &pod(&fb_id)),
        errno::EINVAL,
    );

    // Now the GEM can close.
    ok(&call(
        &mut d,
        DRM_IOCTL_GEM_CLOSE,
        &pod(&DrmGemClose { handle, pad: 0 }),
    ));
    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
}

#[test]
fn getfb2_hands_fresh_closable_handles() {
    // Kernel semantics (drm_mode_getfb2): returned handles are FRESH GEM
    // references owned by the caller. libliftoff does GETFB2 then
    // drmCloseBufferHandle — with raw passthrough ids that close failed
    // EINVAL on device (gamescope had already closed its own handle).
    let mut d = dev();
    let (handle, fb, _fd) = import_fb(&mut d);

    // gamescope pattern: close the importer handle right after ADDFB2.
    ok(&call(
        &mut d,
        DRM_IOCTL_GEM_CLOSE,
        &pod(&DrmGemClose { handle, pad: 0 }),
    ));

    // GETFB2 must still succeed and return a *different*, closable handle.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETFB2,
        &pod(&DrmModeFbCmd2 {
            fb_id: fb,
            ..Default::default()
        }),
    );
    ok(&r);
    let g = pod_read::<DrmModeFbCmd2>(&r.body).unwrap();
    assert_ne!(g.handles[0], 0, "GETFB2 must return a live handle");
    assert_ne!(
        g.handles[0], handle,
        "GETFB2 must mint a fresh reference, not the importer's id"
    );
    ok(&call(
        &mut d,
        DRM_IOCTL_GEM_CLOSE,
        &pod(&DrmGemClose {
            handle: g.handles[0],
            pad: 0,
        }),
    ));
}

#[test]
fn gem_close_with_live_fb_then_modeset_and_rmfb() {
    // gamescope's real lifecycle: ADDFB2, then GEM_CLOSE immediately (kernel
    // semantics — the FB holds its own reference). Day-two on-device: the
    // daemon refused with EBUSY ("drmIoctl(GEM_CLOSE) failed: Device or
    // resource busy" in gamescope's log). The FB must stay fully usable for
    // modeset + flips; teardown + RMFB then frees the orphaned GEM.
    let mut d = dev();
    let (handle, fb, fd) = import_fb(&mut d);

    // Close right after ADDFB2 — allowed; the handle is gone for new ioctls.
    ok(&call(
        &mut d,
        DRM_IOCTL_GEM_CLOSE,
        &pod(&DrmGemClose { handle, pad: 0 }),
    ));
    is_err(
        &call(
            &mut d,
            DRM_IOCTL_GEM_CLOSE,
            &pod(&DrmGemClose { handle, pad: 0 }),
        ),
        errno::EINVAL,
    );

    // The closed-handle FB modesets + flips fine (orphan GEM still resolves).
    let blob = create_mode_blob(&mut d).0;
    let sets = modeset_sets(&d, blob, fb);
    let req = atomic_req(
        &sets,
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT,
        0xF00D,
    );
    let r = call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req);
    ok(&r);
    // The commit has been published; the flip event is emitted asynchronously
    // once the worker has run.
    assert_eq!(
        wait_flip(&mut d, 2000).len(),
        1,
        "modeset must emit the flip event"
    );

    // finish_drm-style teardown (plane/CRTC/connector to zero — RMFB of a
    // scanned-out FB is EBUSY), then RMFB frees the orphaned GEM.
    let zero = [
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "FB_ID").unwrap(),
            0,
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "CRTC_ID").unwrap(),
            0,
        ),
        (CRTC_ID, find_prop_id(CRTC_ID, "ACTIVE").unwrap(), 0),
        (CRTC_ID, find_prop_id(CRTC_ID, "MODE_ID").unwrap(), 0),
        (
            CONNECTOR_ID,
            find_prop_id(CONNECTOR_ID, "CRTC_ID").unwrap(),
            0,
        ),
    ];
    let req = atomic_req(&zero, DRM_MODE_ATOMIC_ALLOW_MODESET, 0);
    ok(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req));
    ok(&call(&mut d, DRM_IOCTL_MODE_RMFB, &pod(&fb)));
    is_err(&call(&mut d, DRM_IOCTL_MODE_RMFB, &pod(&fb)), errno::EINVAL);

    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
}

// ---------------------------------------------------------------------------
// resource churn / exhaustion (the ~216 s on-device leak)
// ---------------------------------------------------------------------------

/// One frame of the churn a framebuffer-heavy game (Nidhogg) produces, driven
/// end-to-end through the daemon: create a MODE_ID blob, import a fresh dma-buf,
/// ADDFB2, GETFB2 (libliftoff's fresh closable handle) then close that handle,
/// close the importer handle (orphaning the GEM), modeset + flip, tear the plane
/// down, RMFB (frees the orphan GEM and the fb id), destroy the blob, and churn
/// one sync object.
///
/// Every allocation has a matching free, so the id-space high-water marks must
/// not move once the working set has settled.
fn churn_one_frame(d: &mut Device) {
    let blob = create_mode_blob(d).0;
    let (handle, fb, fd) = import_fb(d);

    // libliftoff: GETFB2 then drmCloseBufferHandle on the returned handle.
    let r = call(
        d,
        DRM_IOCTL_MODE_GETFB2,
        &pod(&DrmModeFbCmd2 {
            fb_id: fb,
            ..Default::default()
        }),
    );
    ok(&r);
    let fresh = pod_read::<DrmModeFbCmd2>(&r.body).unwrap().handles[0];
    assert_ne!(fresh, 0, "GETFB2 must return a live handle");
    ok(&call(
        d,
        DRM_IOCTL_GEM_CLOSE,
        &pod(&DrmGemClose {
            handle: fresh,
            pad: 0,
        }),
    ));

    // gamescope closes its importer handle right after ADDFB2 (orphan GEM).
    ok(&call(
        d,
        DRM_IOCTL_GEM_CLOSE,
        &pod(&DrmGemClose { handle, pad: 0 }),
    ));

    // Modeset + flip, then tear the plane down so RMFB is allowed.
    let sets = modeset_sets(d, blob, fb);
    let req = atomic_req(
        &sets,
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT,
        0xF00D,
    );
    ok(&call(d, DRM_IOCTL_MODE_ATOMIC, &req));
    assert!(
        !wait_flip(d, 2000).is_empty(),
        "churn frame must emit its flip event"
    );
    let zero = [
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "FB_ID").unwrap(),
            0,
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "CRTC_ID").unwrap(),
            0,
        ),
        (CRTC_ID, find_prop_id(CRTC_ID, "ACTIVE").unwrap(), 0),
        (CRTC_ID, find_prop_id(CRTC_ID, "MODE_ID").unwrap(), 0),
        (
            CONNECTOR_ID,
            find_prop_id(CONNECTOR_ID, "CRTC_ID").unwrap(),
            0,
        ),
    ];
    ok(&call(
        d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(&zero, DRM_MODE_ATOMIC_ALLOW_MODESET, 0),
    ));
    ok(&call(d, DRM_IOCTL_MODE_RMFB, &pod(&fb)));
    ok(&call(
        d,
        DRM_IOCTL_MODE_DESTROYPROPBLOB,
        &pod(&DrmModeDestroyBlob { blob_id: blob }),
    ));

    // Sync-object churn (gamescope's always-signalled in-fence object).
    let r = call(
        d,
        DRM_IOCTL_SYNCOBJ_CREATE,
        &pod(&DrmSyncobjCreate {
            handle: 0,
            flags: DRM_SYNCOBJ_CREATE_SIGNALED,
        }),
    );
    ok(&r);
    let sh = pod_read::<DrmSyncobjCreate>(&r.body).unwrap().handle;
    ok(&call(
        d,
        DRM_IOCTL_SYNCOBJ_DESTROY,
        &pod(&DrmSyncobjDestroy { handle: sh, pad: 0 }),
    ));

    // SAFETY: caller's fd (the daemon holds its own dup).
    unsafe { libc::close(fd) };
}

/// The leak regression: many frames of `commit + ADDFB2 + RMFB + import +
/// close` must not grow any id space. Before the fix each frame advanced the
/// monotonic GEM/FB/blob/syncobj counters by one (freed ids were never
/// reused), so the 0x1000-wide GEM handle space ran out after a few thousand
/// frames and `alloc_gem`'s `assert!` aborted the whole app.
#[test]
fn churn_keeps_id_spaces_bounded() {
    let mut d = dev();
    // Warm up so every space reaches its steady-state high-water mark.
    for _ in 0..32 {
        churn_one_frame(&mut d);
    }
    let warm = d.id_watermarks();
    // A long run (well past the 0x1000 handle space, so the old code would
    // have aborted) must not advance any high-water mark.
    for _ in 0..3000 {
        churn_one_frame(&mut d);
    }
    let after = d.id_watermarks();
    assert_eq!(
        after, warm,
        "id spaces grew across 3000 churn frames: {warm:?} -> {after:?}"
    );
    // Watermarks are the next *unallocated* id, so growth is measured above
    // each space's base (1, 0x1000, 0x2000, 0x4000). The steady-state working
    // set is tiny; the old code advanced these by one per allocation, i.e.
    // ~1 per frame, without bound.
    let bases = [1u32, 0x1000, 0x2000, 0x4000];
    let used: Vec<u32> = warm.iter().zip(bases).map(|(&w, b)| w - b).collect();
    assert!(
        used.iter().all(|&u| u < 8),
        "steady-state id usage must be small, got {used:?} (watermarks {warm:?})"
    );
}

/// Exhaustion is an errno, not an abort: filling the FB id space must make the
/// next ADDFB2 return `-ENOSPC` and leave the device usable — where the old
/// `assert!` would have SIGABRT'd the host app.
#[test]
fn fb_space_exhaustion_returns_enospc_not_abort() {
    let mut d = dev();
    let fd = memfd(1280 * 720 * 4);
    let r = call_fd(
        &mut d,
        DRM_IOCTL_PRIME_FD_TO_HANDLE,
        &pod(&DrmPrimeHandle::default()),
        fd,
    );
    let handle = pod_read::<DrmPrimeHandle>(&r.body).unwrap().handle;
    let cmd = DrmModeFbCmd2 {
        width: 1280,
        height: 720,
        pixel_format: DRM_FORMAT_XBGR8888,
        handles: [handle, 0, 0, 0],
        pitches: [1280 * 4, 0, 0, 0],
        offsets: [0; 4],
        modifier: [0; 4],
        ..Default::default()
    };
    // Fill the whole fb id space (0x1000..0x2000) with the same buffer.
    let mut allocated = 0u32;
    loop {
        let r = call(&mut d, DRM_IOCTL_MODE_ADDFB2, &pod(&cmd));
        if r.ret == -errno::ENOSPC {
            break;
        }
        ok(&r);
        allocated += 1;
        assert!(allocated < 0x2000, "ADDFB2 never reported exhaustion");
    }
    assert_eq!(
        allocated, 0x1000,
        "the fb id space is exactly 0x1000 ids wide"
    );
    // Still usable: the failed ADDFB2 did not abort the process or wedge it.
    ok(&call(
        &mut d,
        DRM_IOCTL_GEM_CLOSE,
        &pod(&DrmGemClose { handle, pad: 0 }),
    ));
    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
}

/// A presenter that panics on its first publish, to prove the commit worker's
/// panic barrier keeps the host process alive.
struct PanicPresenter;

impl Presenter for PanicPresenter {
    fn publish(
        &mut self,
        _fb: &Framebuffer,
        _plane: &PlaneState,
        _ino: Option<u64>,
        _live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    ) {
        if let Some(fd) = fence {
            // SAFETY: the mock owns the fence the device handed it.
            unsafe { libc::close(fd) };
        }
        panic!("presenter exploded (containment test)");
    }
}

/// A panic on the commit worker must be reported and must stop only that
/// subsystem: the device (and therefore the host app) stays alive and keeps
/// serving ioctls. Without the barrier this panic would unwind out of the
/// worker; with `panic = "abort"` it would SIGABRT the whole app.
#[test]
fn commit_worker_panic_is_contained_and_non_fatal() {
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&logs);
    let mut d = Device::new(PanelDesc::default(), Box::new(PanicPresenter));
    d.set_clock(Box::new(|| (1000, 500)));
    d.set_logger(Box::new(move |m: &str| {
        sink.lock().unwrap().push(m.to_string());
    }));

    let blob = create_mode_blob(&mut d).0;
    let (handle, fb, fd) = import_fb(&mut d);
    let sets = modeset_sets(&d, blob, fb);
    let req = atomic_req(
        &sets,
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT,
        1,
    );
    ok(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req));

    // The worker panics inside the presenter; the barrier reports it.
    let mut seen = false;
    for _ in 0..2000 {
        if logs.lock().unwrap().iter().any(|l| l.contains("PANICKED")) {
            seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(seen, "the worker panic must be reported");

    // The device is still alive and serving ioctls on this (app) thread.
    ok(&call(
        &mut d,
        DRM_IOCTL_VERSION,
        &pod(&DrmVersion::default()),
    ));

    let _ = (handle, blob);
    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
}

// ---------------------------------------------------------------------------
// A6/A7: atomic commits
// ---------------------------------------------------------------------------

struct ModeBlob(u32);

fn create_mode_blob(d: &mut Device) -> ModeBlob {
    let mode = PanelDesc::default().modeinfo();
    let mut req = pod(&DrmModeCreateBlob {
        length: core::mem::size_of::<DrmModeModeinfo>() as u32,
        ..Default::default()
    });
    req.extend_from_slice(&pod(&mode));
    let r = call(d, DRM_IOCTL_MODE_CREATEPROPBLOB, &req);
    ok(&r);
    ModeBlob(pod_read::<DrmModeCreateBlob>(&r.body).unwrap().blob_id)
}

fn import_fb(d: &mut Device) -> (u32, u32, RawFd) {
    let fd = memfd(1280 * 720 * 4);
    // `call_fd` duplicates the descriptor into the daemon (SCM_RIGHTS), so the
    // test keeps this reference and closes it at the end while the daemon owns
    // its own copy.
    let r = call_fd(
        d,
        DRM_IOCTL_PRIME_FD_TO_HANDLE,
        &pod(&DrmPrimeHandle::default()),
        fd,
    );
    let handle = pod_read::<DrmPrimeHandle>(&r.body).unwrap().handle;
    let r = call(
        d,
        DRM_IOCTL_MODE_ADDFB2,
        &pod(&DrmModeFbCmd2 {
            width: 1280,
            height: 720,
            pixel_format: DRM_FORMAT_XBGR8888,
            handles: [handle, 0, 0, 0],
            pitches: [1280 * 4, 0, 0, 0],
            offsets: [0; 4],
            modifier: [0; 4],
            ..Default::default()
        }),
    );
    let fb_id = pod_read::<DrmModeFbCmd2>(&r.body).unwrap().fb_id;
    (handle, fb_id, fd)
}

/// The full gamescope modeset commit (connector+CRTC+plane in one req).
fn modeset_sets(d: &Device, blob: u32, fb: u32) -> Vec<(u32, u32, u64)> {
    let _ = d;
    vec![
        (
            CONNECTOR_ID,
            find_prop_id(CONNECTOR_ID, "CRTC_ID").unwrap(),
            u64::from(CRTC_ID),
        ),
        (
            CRTC_ID,
            find_prop_id(CRTC_ID, "MODE_ID").unwrap(),
            u64::from(blob),
        ),
        (CRTC_ID, find_prop_id(CRTC_ID, "ACTIVE").unwrap(), 1),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "FB_ID").unwrap(),
            u64::from(fb),
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "CRTC_ID").unwrap(),
            u64::from(CRTC_ID),
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "SRC_X").unwrap(),
            0,
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "SRC_Y").unwrap(),
            0,
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "SRC_W").unwrap(),
            1280 << 16,
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "SRC_H").unwrap(),
            720 << 16,
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "CRTC_X").unwrap(),
            0,
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "CRTC_Y").unwrap(),
            0,
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "CRTC_W").unwrap(),
            1280,
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "CRTC_H").unwrap(),
            720,
        ),
    ]
}

#[test]
fn atomic_test_only_then_modeset_then_flip() {
    let mut d = dev_async();
    let blob = create_mode_blob(&mut d).0;
    let (_handle, fb, fd) = import_fb(&mut d);

    // TEST_ONLY must validate but not apply: no event, no state change.
    let sets = modeset_sets(&d, blob, fb);
    let req = atomic_req(
        &sets,
        DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_ATOMIC_ALLOW_MODESET,
        0,
    );
    let r = call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req);
    ok(&r);
    assert!(d.drain_flip_events().is_empty());
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETCRTC,
        &pod(&DrmModeCrtc {
            crtc_id: CRTC_ID,
            ..Default::default()
        }),
    );
    let c: DrmModeCrtc = pod_read(&r.body).unwrap();
    assert_eq!(c.mode_valid, 0);
    assert_eq!(c.fb_id, 0);

    // Real modeset: queues and returns immediately. The flip event is emitted
    // with the publish, not after the presenter's latch — gamescope's DRM
    // backend blocks its present loop on this event, so gating it on
    // SurfaceFlinger's completion would serialise that round trip into every
    // paced frame. It must therefore be observable *without* any latch firing.
    let req = atomic_req(
        &sets,
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
        0xCAFE,
    );
    let r = call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req);
    ok(&r);
    let events = wait_flip(&mut d, 2000);
    assert_eq!(
        events.len(),
        1,
        "one event per commit, emitted with the publish"
    );
    let ev = events[0];
    assert_eq!(ev.base.ty, DRM_EVENT_FLIP_COMPLETE);
    assert_eq!(
        ev.base.length as usize,
        core::mem::size_of::<DrmEventVblank>()
    );
    assert_eq!(ev.user_data, 0xCAFE);
    assert_eq!(ev.crtc_id, CRTC_ID);
    assert_eq!(ev.sequence, 1);
    // The timestamp is the vblank this frame claims, not the emit instant: the
    // first claim is the clock's own reading (see `VblankTimeline`).
    assert_eq!((ev.tv_sec, ev.tv_usec), (1000, 500));

    // State is already visible to queries (applied synchronously on commit).
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETCRTC,
        &pod(&DrmModeCrtc {
            crtc_id: CRTC_ID,
            ..Default::default()
        }),
    );
    let c: DrmModeCrtc = pod_read(&r.body).unwrap();
    assert_eq!(c.mode_valid, 1);
    assert_eq!(c.fb_id, fb);
    assert_eq!((c.mode.hdisplay, c.mode.vdisplay), (1280, 720));

    // Page-flip-only commit (no ALLOW_MODESET, same FB → no re-present) emits
    // the next event immediately, still ordered after the first.
    let req = atomic_req(
        &[(
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "FB_ID").unwrap(),
            u64::from(fb),
        )],
        DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
        0xBEEF,
    );
    let r = call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req);
    ok(&r);
    let events = wait_flip(&mut d, 2000);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, 2);
    assert_eq!(events[0].user_data, 0xBEEF);
    // The claimed vblank advanced by exactly one mode interval (the mocked clock
    // does not move), which is what keeps gamescope's grid on the mode's rate:
    // the 1200 Hz session constant is 833 333 ns per frame.
    assert_eq!(
        (events[0].tv_sec, events[0].tv_usec),
        (1000, 1_333),
        "the next claim is one interval after the previous vblank"
    );

    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
}

#[test]
fn cursor_plane_rejects_a_full_size_rectangle() {
    let mut d = dev();
    let blob = create_mode_blob(&mut d).0;
    let (_h1, primary_fb, fd1) = import_fb(&mut d);
    let (_h2, cursor_fb, fd2) = import_fb(&mut d);

    let base = modeset_sets(&d, blob, primary_fb);
    let cursor_sets = |src_w: u64, src_h: u64, crtc_w: u64, crtc_h: u64| {
        let mut sets = base.clone();
        sets.extend([
            (
                PLANE_CURSOR_ID,
                find_prop_id(PLANE_CURSOR_ID, "FB_ID").unwrap(),
                u64::from(cursor_fb),
            ),
            (
                PLANE_CURSOR_ID,
                find_prop_id(PLANE_CURSOR_ID, "CRTC_ID").unwrap(),
                u64::from(CRTC_ID),
            ),
            (
                PLANE_CURSOR_ID,
                find_prop_id(PLANE_CURSOR_ID, "SRC_W").unwrap(),
                src_w << 16,
            ),
            (
                PLANE_CURSOR_ID,
                find_prop_id(PLANE_CURSOR_ID, "SRC_H").unwrap(),
                src_h << 16,
            ),
            (
                PLANE_CURSOR_ID,
                find_prop_id(PLANE_CURSOR_ID, "CRTC_W").unwrap(),
                crtc_w,
            ),
            (
                PLANE_CURSOR_ID,
                find_prop_id(PLANE_CURSOR_ID, "CRTC_H").unwrap(),
                crtc_h,
            ),
        ]);
        sets
    };

    // A game-sized layer on the cursor plane (what libliftoff tries when the
    // plane lists the layer's format) is refused, in TEST_ONLY and for real.
    for flags in [
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_ATOMIC_TEST_ONLY,
        DRM_MODE_ATOMIC_ALLOW_MODESET,
    ] {
        let req = atomic_req(&cursor_sets(1280, 720, 1280, 720), flags, 0);
        is_err(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req), errno::EINVAL);
    }
    let req = atomic_req(
        &cursor_sets(64, 64, 65, 64),
        DRM_MODE_ATOMIC_ALLOW_MODESET,
        0,
    );
    is_err(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req), errno::EINVAL);

    // A cursor-sized rectangle goes through.
    let req = atomic_req(&cursor_sets(64, 64, 64, 64), DRM_MODE_ATOMIC_ALLOW_MODESET, 0);
    ok(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req));

    // SAFETY: caller's fds.
    unsafe {
        libc::close(fd1);
        libc::close(fd2);
    }
}

#[test]
fn atomic_rejections() {
    let mut d = dev();
    let blob = create_mode_blob(&mut d).0;
    let (_handle, fb, fd) = import_fb(&mut d);
    let sets = modeset_sets(&d, blob, fb);

    // Modeset props without ALLOW_MODESET.
    let req = atomic_req(&sets, 0, 0);
    is_err(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req), errno::EINVAL);

    // Unknown flag bit.
    let req = atomic_req(&sets, DRM_MODE_ATOMIC_ALLOW_MODESET | 0x8000, 0);
    is_err(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req), errno::EINVAL);

    // PAGE_FLIP_ASYNC is accepted (libliftoff leaks it into TEST_ONLY
    // commits — device-proven EINVAL before the fix).
    let req = atomic_req(
        &sets,
        DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_ASYNC,
        0,
    );
    ok(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req));

    // TEST_ONLY + EVENT.
    let req = atomic_req(
        &sets,
        DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT,
        0,
    );
    is_err(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req), errno::EINVAL);

    // Immutable property (plane "type").
    let req = atomic_req(
        &[(
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "type").unwrap(),
            1,
        )],
        0,
        0,
    );
    is_err(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req), errno::EPERM);

    // Unknown FB.
    let req = atomic_req(
        &[(
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "FB_ID").unwrap(),
            0x999,
        )],
        DRM_MODE_ATOMIC_ALLOW_MODESET,
        0,
    );
    is_err(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req), errno::ENOENT);

    // Property on the wrong object (CRTC's MODE_ID on the connector).
    let req = atomic_req(
        &[(
            CONNECTOR_ID,
            find_prop_id(CRTC_ID, "MODE_ID").unwrap(),
            u64::from(blob),
        )],
        DRM_MODE_ATOMIC_ALLOW_MODESET,
        0,
    );
    is_err(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req), errno::ENOENT);

    // Truncated extension.
    let req = atomic_req(&sets, DRM_MODE_ATOMIC_ALLOW_MODESET, 0);
    let truncated = &req[..req.len() - 8];
    is_err(
        &call(&mut d, DRM_IOCTL_MODE_ATOMIC, truncated),
        errno::EINVAL,
    );

    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
}

#[test]
fn atomic_in_fence_fd_contract() {
    let mut d = dev();
    let blob = create_mode_blob(&mut d).0;
    let (_handle, fb, fd) = import_fb(&mut d);
    let mut sets = modeset_sets(&d, blob, fb);
    sets.push((
        PLANE_PRIMARY_ID,
        find_prop_id(PLANE_PRIMARY_ID, "IN_FENCE_FD").unwrap(),
        3, // arbitrary fd number in the client — value is opaque over RPC
    ));

    // Fence value present but no fd attached → EINVAL.
    let req = atomic_req(&sets, DRM_MODE_ATOMIC_ALLOW_MODESET, 0);
    is_err(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req), errno::EINVAL);

    // With the fence fd attached via SCM_RIGHTS → accepted.
    let fence = memfd(1); // stands in for a sync_file fence fd
    let req = atomic_req(&sets, DRM_MODE_ATOMIC_ALLOW_MODESET, 0);
    let r = call_fd(&mut d, DRM_IOCTL_MODE_ATOMIC, &req, fence);
    ok(&r);

    // -1 ("no fence") must NOT attach an fd. Userspace sends the u64 sign
    // extension of -1 (kernel convention), not u32::MAX — day-two on-device:
    // gamescope's finish_drm teardown commit got EINVAL from this mismatch.
    let mut sets = modeset_sets(&d, blob, fb);
    sets.push((
        PLANE_PRIMARY_ID,
        find_prop_id(PLANE_PRIMARY_ID, "IN_FENCE_FD").unwrap(),
        u64::MAX,
    ));
    let req = atomic_req(&sets, DRM_MODE_ATOMIC_ALLOW_MODESET, 0);
    let r = call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req);
    ok(&r);

    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
}

// ---------------------------------------------------------------------------
// async commit semantics (doc 07): commit → queue → latch → flip event
// ---------------------------------------------------------------------------

/// An unsignalled fence (eventfd count 0): `poll` reports it not ready, which
/// is exactly what the NONBLOCK/EBUSY path keys on.
fn unsignalled_fence() -> RawFd {
    // SAFETY: eventfd(2) with count 0, standard flags.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    assert!(fd >= 0);
    fd
}

/// The core async contract: a commit queues and returns immediately; the flip
/// event is emitted with the *publish* (before the presenter's latch), because
/// the event paces the client and must not carry the compositor round trip; the
/// latch is still awaited as the completion acknowledgement, and the current
/// primary still refuses RMFB.
#[test]
fn async_commit_returns_before_latch_event_follows_publish() {
    let mut d = dev_async();
    let blob = create_mode_blob(&mut d).0;
    let (_handle, fb, fd) = import_fb(&mut d);

    let sets = modeset_sets(&d, blob, fb);
    let req = atomic_req(
        &sets,
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
        0xC0DE,
    );
    let r = call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req);
    // (a) the commit returned 0 immediately.
    ok(&r);
    // (b) the flip event is emitted with the publish, before any latch: it means
    // "queued to the compositor", not "compositor finished".
    let events = wait_flip(&mut d, 2000);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].base.ty, DRM_EVENT_FLIP_COMPLETE);
    assert_eq!(events[0].user_data, 0xC0DE);
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[0].crtc_id, CRTC_ID);

    // (e) TEST_ONLY validates without side effects (no second event).
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(
            &sets,
            DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_ATOMIC_ALLOW_MODESET,
            0,
        ),
    ));
    assert!(d.drain_flip_events().is_empty());

    // The committed fb is the current primary, still scanned out: RMFB refuses.
    is_err(&call(&mut d, DRM_IOCTL_MODE_RMFB, &pod(&fb)), errno::EBUSY);

    // The buffer becomes reusable once it is moved off the plane: RMFB succeeds.
    let zero = [
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "FB_ID").unwrap(),
            0,
        ),
        (
            PLANE_PRIMARY_ID,
            find_prop_id(PLANE_PRIMARY_ID, "CRTC_ID").unwrap(),
            0,
        ),
        (CRTC_ID, find_prop_id(CRTC_ID, "ACTIVE").unwrap(), 0),
        (CRTC_ID, find_prop_id(CRTC_ID, "MODE_ID").unwrap(), 0),
        (
            CONNECTOR_ID,
            find_prop_id(CONNECTOR_ID, "CRTC_ID").unwrap(),
            0,
        ),
    ];
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(&zero, DRM_MODE_ATOMIC_ALLOW_MODESET, 0),
    ));
    ok(&call(&mut d, DRM_IOCTL_MODE_RMFB, &pod(&fb)));

    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
}

/// Two commits against different buffers stay ordered: the worker presents and
/// emits in commit order, each event following its own publish. The current
/// primary refuses RMFB; a superseded buffer is no longer in flight.
#[test]
fn async_commits_stay_ordered_and_events_follow_publish() {
    let mut d = dev_async();
    let blob = create_mode_blob(&mut d).0;
    let (_ha, fb_a, fd_a) = import_fb(&mut d);
    let (_hb, fb_b, fd_b) = import_fb(&mut d);

    // Modeset fb_a.
    let sets_a = modeset_sets(&d, blob, fb_a);
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(
            &sets_a,
            DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
            0xA1,
        ),
    ));
    // Page-flip to fb_b (no ALLOW_MODESET), queued behind fb_a.
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(
            &[(
                PLANE_PRIMARY_ID,
                find_prop_id(PLANE_PRIMARY_ID, "FB_ID").unwrap(),
                u64::from(fb_b),
            )],
            DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
            0xA2,
        ),
    ));

    // Both commits' events, in commit order. A single drain may return both:
    // the worker emits each event with its own publish and never waits for a
    // latch, so collect to a count rather than assuming one event per read.
    let events = wait_flips(&mut d, 2, 2000);
    assert_eq!(events.len(), 2, "one flip event per commit");
    assert_eq!(events[0].user_data, 0xA1);
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[1].user_data, 0xA2);
    assert_eq!(events[1].sequence, 2);
    // Exactly one event per commit: nothing extra — no duplicate, and no event
    // for a commit that did not ask for one — remains queued. Both jobs have
    // already left the serial worker (the second event is in hand) and no
    // further commit was issued, so this is a contract assertion, not a race.
    assert!(d.drain_flip_events().is_empty());

    // fb_b is the current primary → RMFB refused. fb_a was superseded and is no
    // longer in flight (its event already fired) → RMFB succeeds.
    is_err(
        &call(&mut d, DRM_IOCTL_MODE_RMFB, &pod(&fb_b)),
        errno::EBUSY,
    );
    ok(&call(&mut d, DRM_IOCTL_MODE_RMFB, &pod(&fb_a)));

    // SAFETY: caller's fds.
    unsafe {
        libc::close(fd_a);
        libc::close(fd_b);
    }
}

/// A non-blocking commit whose acquire fence is not signalled returns EBUSY
/// rather than stalling; a signalled fence (or no fence) queues normally.
#[test]
fn nonblocking_unsignalled_fence_returns_ebusy() {
    let mut d = dev_async();
    let blob = create_mode_blob(&mut d).0;
    let (_handle, fb, fd) = import_fb(&mut d);
    let mut sets = modeset_sets(&d, blob, fb);
    sets.push((
        PLANE_PRIMARY_ID,
        find_prop_id(PLANE_PRIMARY_ID, "IN_FENCE_FD").unwrap(),
        1, // opaque fd number in the client
    ));

    // Unsignalled fence + NONBLOCK → EBUSY.
    let fence = unsignalled_fence();
    let req = atomic_req(
        &sets,
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_ATOMIC_NONBLOCK,
        0,
    );
    is_err(
        &call_fd(&mut d, DRM_IOCTL_MODE_ATOMIC, &req, fence),
        errno::EBUSY,
    );

    // A signalled fence queues normally. On the host a memfd stands in (it is
    // always poll-ready, like the always-signalled sync-file the daemon
    // exports), and is deterministic under parallel test execution.
    let fence = memfd(1);
    let req = atomic_req(
        &sets,
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
        0xF00D,
    );
    ok(&call_fd(&mut d, DRM_IOCTL_MODE_ATOMIC, &req, fence));
    let events = wait_flip(&mut d, 2000);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].user_data, 0xF00D);

    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
}

/// A presenter that records whether each publish was handed an acquire fence.
struct FenceSpyPresenter {
    fences: Arc<Mutex<Vec<bool>>>,
}

impl Presenter for FenceSpyPresenter {
    fn publish(
        &mut self,
        _fb: &Framebuffer,
        _plane: &PlaneState,
        _ino: Option<u64>,
        _live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    ) {
        self.fences.lock().unwrap().push(fence.is_some());
        if let Some(fd) = fence {
            // SAFETY: the mock owns the fence the device handed it.
            unsafe { libc::close(fd) };
        }
    }
}

/// A commit that rides the device's own always-signalled sync-object fence back
/// as its `IN_FENCE_FD` must reach the presenter as *no fence*. That export is a
/// bare eventfd (there is no userspace way to mint a real `sync_file`), it is
/// already signalled, and SurfaceFlinger cannot inspect it — it logs
/// `sync_file_info returned NULL for fd N`. A fence the device did not mint is
/// passed through untouched.
#[test]
fn emulated_signalled_fence_is_not_forwarded_to_the_presenter() {
    let fences = Arc::new(Mutex::new(Vec::new()));
    let mut d = Device::new(
        PanelDesc::default(),
        Box::new(FenceSpyPresenter {
            fences: Arc::clone(&fences),
        }),
    );
    d.set_clock(Box::new(|| (1000, 500)));
    let blob = create_mode_blob(&mut d).0;
    let (_h1, fb1, fb1_fd) = import_fb(&mut d);
    let (_h2, fb2, fb2_fd) = import_fb(&mut d);

    // The device's own always-signalled sync object, exported like gamescope
    // does once at init.
    let r = call(
        &mut d,
        DRM_IOCTL_SYNCOBJ_CREATE,
        &pod(&DrmSyncobjCreate {
            handle: 0,
            flags: DRM_SYNCOBJ_CREATE_SIGNALED,
        }),
    );
    ok(&r);
    let handle = pod_read::<DrmSyncobjCreate>(&r.body).unwrap().handle;
    let r = call(
        &mut d,
        DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD,
        &pod(&DrmSyncobjHandle {
            handle,
            flags: 0,
            fd: 0,
            pad: 0,
            point: 0,
        }),
    );
    ok(&r);
    let export = r.fd_out.expect("exported fence fd");

    let prop = find_prop_id(PLANE_PRIMARY_ID, "IN_FENCE_FD").unwrap();
    let mut sets = modeset_sets(&d, blob, fb1);
    sets.push((PLANE_PRIMARY_ID, prop, 1)); // opaque client fd number
    ok(&call_fd(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(
            &sets,
            DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
            0xF1,
        ),
        export,
    ));
    let _ = wait_flip(&mut d, 2000);
    assert!(
        !fences.lock().unwrap()[0],
        "the emulated always-signalled fence must be dropped, not forwarded"
    );

    // A fence the device did not mint is forwarded unchanged.
    let mut sets2 = vec![(
        PLANE_PRIMARY_ID,
        find_prop_id(PLANE_PRIMARY_ID, "FB_ID").unwrap(),
        u64::from(fb2),
    )];
    sets2.push((PLANE_PRIMARY_ID, prop, 1));
    let other = memfd(1);
    ok(&call_fd(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(
            &sets2,
            DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
            0xF2,
        ),
        other,
    ));
    let _ = wait_flip(&mut d, 2000);
    assert!(
        fences.lock().unwrap()[1],
        "a fence the device did not mint must be forwarded unchanged"
    );

    // SAFETY: caller's fds.
    unsafe {
        libc::close(export);
        libc::close(other);
        libc::close(fb1_fd);
        libc::close(fb2_fd);
    }
}

/// The full gamescope call chain, driven in order: discovery → buffer import →
/// modeset → page flips → syncobj, with every flip event observed after its
/// latch.
#[test]
fn full_gamescope_chain_discovery_to_syncobj_async() {
    let mut d = dev_async();

    // 1. Discovery — assert a coherent topology.
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETRESOURCES,
        &pod(&DrmModeCardRes::default()),
    );
    ok(&r);
    let res: DrmModeCardRes = pod_read(&r.body).unwrap();
    assert_eq!(res.count_crtcs, 1);
    assert_eq!(res.count_connectors, 1);
    assert_eq!(res.count_encoders, 1);

    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETCONNECTOR,
        &pod(&DrmModeGetConnector {
            connector_id: CONNECTOR_ID,
            ..Default::default()
        }),
    );
    ok(&r);
    let conn: DrmModeGetConnector = pod_read(&r.body).unwrap();
    assert_eq!(conn.connection, DRM_MODE_CONNECTED);
    assert_eq!(conn.count_modes, 1);
    assert_eq!(conn.encoder_id, ENCODER_ID);

    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETENCODER,
        &pod(&DrmModeGetEncoder {
            encoder_id: ENCODER_ID,
            ..Default::default()
        }),
    );
    ok(&r);
    let enc: DrmModeGetEncoder = pod_read(&r.body).unwrap();
    assert_eq!(enc.crtc_id, CRTC_ID);
    assert_eq!(enc.possible_crtcs, 1);

    let r = call(
        &mut d,
        DRM_IOCTL_MODE_GETPLANE,
        &pod(&DrmModeGetPlane {
            plane_id: PLANE_PRIMARY_ID,
            format_type_ptr: 1,
            count_format_types: 16,
            ..Default::default()
        }),
    );
    ok(&r);
    let plane: DrmModeGetPlane = pod_read(&r.body).unwrap();
    assert_eq!(plane.possible_crtcs, 1);
    let off = core::mem::size_of::<DrmModeGetPlane>();
    let fmts: Vec<u32> = r.body[off..]
        .chunks(4)
        .take(plane.count_format_types as usize)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert!(fmts.contains(&DRM_FORMAT_XBGR8888));

    // 2. Buffer: import a memfd, ADDFB2, GETFB2 echoes pitch/format.
    let fd = memfd(1280 * 720 * 4);
    let r = call_fd(
        &mut d,
        DRM_IOCTL_PRIME_FD_TO_HANDLE,
        &pod(&DrmPrimeHandle::default()),
        fd,
    );
    ok(&r);
    let handle = pod_read::<DrmPrimeHandle>(&r.body).unwrap().handle;
    let r = call(
        &mut d,
        DRM_IOCTL_MODE_ADDFB2,
        &pod(&DrmModeFbCmd2 {
            width: 1280,
            height: 720,
            pixel_format: DRM_FORMAT_XBGR8888,
            handles: [handle, 0, 0, 0],
            pitches: [1280 * 4, 0, 0, 0],
            ..Default::default()
        }),
    );
    ok(&r);
    let fb = pod_read::<DrmModeFbCmd2>(&r.body).unwrap().fb_id;

    // 3. Modeset → flip event (after the latch).
    let blob = create_mode_blob(&mut d).0;
    let sets = modeset_sets(&d, blob, fb);
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(
            &sets,
            DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
            0x100,
        ),
    ));
    let events = wait_flip(&mut d, 2000);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].user_data, 0x100);
    assert_eq!(events[0].sequence, 1);

    // Plain page-flip (same FB) → next ordered event, no re-present.
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(
            &[(
                PLANE_PRIMARY_ID,
                find_prop_id(PLANE_PRIMARY_ID, "FB_ID").unwrap(),
                u64::from(fb),
            )],
            DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
            0x200,
        ),
    ));
    let events = wait_flip(&mut d, 2000);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].user_data, 0x200);
    assert_eq!(events[0].sequence, 2);

    // 5. Syncobj: create-signalled → export (poll-ready) → destroy.
    let r = call(
        &mut d,
        DRM_IOCTL_SYNCOBJ_CREATE,
        &pod(&DrmSyncobjCreate {
            handle: 0,
            flags: DRM_SYNCOBJ_CREATE_SIGNALED,
        }),
    );
    ok(&r);
    let syncobj = pod_read::<DrmSyncobjCreate>(&r.body).unwrap().handle;
    let r = call(
        &mut d,
        DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD,
        &pod(&DrmSyncobjHandle {
            handle: syncobj,
            ..Default::default()
        }),
    );
    ok(&r);
    let sync_fd = r.fd_out.expect("exported sync fd");
    let mut pfd = libc::pollfd {
        fd: sync_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    assert_eq!(unsafe { libc::poll(&mut pfd, 1, 0) }, 1);
    // SAFETY: caller's fd.
    unsafe { libc::close(sync_fd) };
    ok(&call(
        &mut d,
        DRM_IOCTL_SYNCOBJ_DESTROY,
        &pod(&DrmSyncobjDestroy {
            handle: syncobj,
            pad: 0,
        }),
    ));

    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
}

/// The plane census must name every plane the commit drives, in z-order, and
/// must show a plane turned off as inactive rather than silently dropping it —
/// the evidence for "which KMS planes gamescope actually uses".
#[test]
fn plane_census_names_every_plane_in_z_order() {
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&logs);
    let mut d = dev();
    d.set_logger(Box::new(move |m: &str| {
        sink.lock().unwrap().push(m.to_string());
    }));

    let blob = create_mode_blob(&mut d).0;
    let (_h1, primary_fb, fd1) = import_fb(&mut d);
    let (_h2, cursor_fb, fd2) = import_fb(&mut d);

    // One commit driving both planes: primary (bottom) + cursor (top).
    let mut sets = modeset_sets(&d, blob, primary_fb);
    sets.extend([
        (
            PLANE_CURSOR_ID,
            find_prop_id(PLANE_CURSOR_ID, "FB_ID").unwrap(),
            u64::from(cursor_fb),
        ),
        (
            PLANE_CURSOR_ID,
            find_prop_id(PLANE_CURSOR_ID, "CRTC_ID").unwrap(),
            u64::from(CRTC_ID),
        ),
        (
            PLANE_CURSOR_ID,
            find_prop_id(PLANE_CURSOR_ID, "SRC_W").unwrap(),
            64 << 16,
        ),
        (
            PLANE_CURSOR_ID,
            find_prop_id(PLANE_CURSOR_ID, "SRC_H").unwrap(),
            64 << 16,
        ),
        (
            PLANE_CURSOR_ID,
            find_prop_id(PLANE_CURSOR_ID, "CRTC_W").unwrap(),
            64,
        ),
        (
            PLANE_CURSOR_ID,
            find_prop_id(PLANE_CURSOR_ID, "CRTC_H").unwrap(),
            64,
        ),
    ]);
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(&sets, DRM_MODE_ATOMIC_ALLOW_MODESET, 0),
    ));

    let census = logs
        .lock()
        .unwrap()
        .iter()
        .find(|l| l.starts_with("planes #"))
        .cloned()
        .expect("a plane-census line was emitted");
    assert!(census.contains("count=2"), "{census}");
    assert!(
        census.contains(&format!("[primary id={PLANE_PRIMARY_ID} fb={primary_fb}")),
        "{census}"
    );
    assert!(
        census.contains(&format!("[cursor id={PLANE_CURSOR_ID} fb={cursor_fb}")),
        "{census}"
    );
    // Listed bottom-to-top: primary before cursor.
    assert!(
        census.find("[primary").unwrap() < census.find("[cursor").unwrap(),
        "{census}"
    );
    // The cursor's 64x64 source rect is reported from its plane state.
    assert!(census.contains("src=64x64+(0,0)"), "{census}");

    // Turning the cursor plane off must be visible as an inactive plane.
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(
            &[(
                PLANE_CURSOR_ID,
                find_prop_id(PLANE_CURSOR_ID, "FB_ID").unwrap(),
                0,
            )],
            DRM_MODE_ATOMIC_ALLOW_MODESET,
            0,
        ),
    ));
    let last = logs
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|l| l.starts_with("planes #"))
        .cloned()
        .unwrap();
    assert!(
        last.contains(&format!("[cursor id={PLANE_CURSOR_ID} fb=0")),
        "{last}"
    );

    // SAFETY: caller's fds.
    unsafe {
        libc::close(fd1);
        libc::close(fd2);
    }
}

/// The commit probe is the daemon-side line a device pass can rely on: it is
/// emitted through the **always** sink, which is installed with no gate at all,
/// and it carries the commit's primary fb id together with a running commit
/// count. Both halves matter on the single-layer path, where the presenter's
/// splash-clear latch and its frame counter sit behind `fb.id != 0`: "commits
/// arrive, and the plane is blank (fb=0)" and "no commit arrives at all" are
/// otherwise indistinguishable, because the absence of any other daemon line
/// means the same thing in both cases.
/// `GNDRM_LOG_DAEMON` is process-wide and the gate caches it, so the two
/// session-flag tests must not overlap.
static SESSION_FLAG: Mutex<()> = Mutex::new(());

#[test]
fn commit_probe_reports_fb_id_and_running_count_when_enabled() {
    let _serial = SESSION_FLAG.lock().unwrap();
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&logs);
    // The probe is opt-in per session: the launcher sets this in the app's
    // process, which the daemon shares.
    std::env::set_var("GNDRM_LOG_DAEMON", "1");
    gndrm_daemon::reset_verbose_logging();
    let mut d = Device::new(PanelDesc::default(), Box::<NullPresenter>::default());
    d.set_clock(Box::new(|| (1000, 500)));
    // The always sink only; `set_logger` is deliberately not called, so the
    // gated diagnostics stay off and anything the probe prints is the opt-in
    // probe rather than a `GNDRM_LOG_*`-gated line.
    d.set_always_logger(Box::new(move |m: &str| {
        sink.lock().unwrap().push(m.to_string());
    }));

    let blob = create_mode_blob(&mut d).0;
    let (handle, fb, fd) = import_fb(&mut d);
    let sets = modeset_sets(&d, blob, fb);
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(
            &sets,
            DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT,
            1,
        ),
    ));
    // A re-commit of the same primary fb: the daemon publishes only a *changed*
    // non-zero fb, so this one is carried by the probe and not by the presenter.
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(&sets, DRM_MODE_ATOMIC_ALLOW_MODESET, 2),
    ));

    let lines = logs.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with(&format!("gndrm-daemon: commit #1 fb={fb} prev_fb=0 "))),
        "probe must report the modeset's fb id and a running count: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with(&format!("gndrm-daemon: commit #2 fb={fb} prev_fb={fb} "))),
        "probe must report a re-commit of the same fb, which the presenter never sees: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("atomic #")),
        "only the ungated probe may print when no gated sink is installed: {lines:?}"
    );

    // SAFETY: caller's fds.
    unsafe {
        libc::close(fd);
    }
    let _ = handle;
    std::env::remove_var("GNDRM_LOG_DAEMON");
    gndrm_daemon::reset_verbose_logging();
}

/// The same probe with the session's opt-in absent: a shipped session says
/// nothing about the presenter's per-commit state.
#[test]
fn commit_probe_is_silent_without_the_session_flag() {
    let _serial = SESSION_FLAG.lock().unwrap();
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&logs);
    std::env::remove_var("GNDRM_LOG_DAEMON");
    gndrm_daemon::reset_verbose_logging();
    let mut d = Device::new(PanelDesc::default(), Box::<NullPresenter>::default());
    d.set_clock(Box::new(|| (1000, 500)));
    d.set_always_logger(Box::new(move |m: &str| {
        sink.lock().unwrap().push(m.to_string());
    }));

    let blob = create_mode_blob(&mut d).0;
    let (handle, fb, fd) = import_fb(&mut d);
    let sets = modeset_sets(&d, blob, fb);
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(
            &sets,
            DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT,
            1,
        ),
    ));
    ok(&call(
        &mut d,
        DRM_IOCTL_MODE_ATOMIC,
        &atomic_req(&sets, DRM_MODE_ATOMIC_ALLOW_MODESET, 2),
    ));

    let lines = logs.lock().unwrap().clone();
    assert!(
        !lines.iter().any(|l| l.contains("commit #")),
        "no commit probe may print without GNDRM_LOG_DAEMON: {lines:?}"
    );

    // SAFETY: caller's fds.
    unsafe {
        libc::close(fd);
    }
    let _ = handle;
}

/// Per publish: the committed inode and the inode of the source descriptor.
type SourceSeen = Arc<Mutex<Vec<(Option<u64>, Option<u64>)>>>;

/// A presenter that records which source descriptor each publish was handed.
struct SourceSpyPresenter {
    seen: SourceSeen,
}

impl Presenter for SourceSpyPresenter {
    fn publish(
        &mut self,
        _fb: &Framebuffer,
        _plane: &PlaneState,
        _ino: Option<u64>,
        _live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    ) {
        if let Some(fd) = fence {
            // SAFETY: the mock owns the fence the device handed it.
            unsafe { libc::close(fd) };
        }
    }

    fn publish_with_source(
        &mut self,
        fb: &Framebuffer,
        plane: &PlaneState,
        ino: Option<u64>,
        src_fd: Option<RawFd>,
        live_inos: &HashSet<u64>,
        fence: Option<RawFd>,
    ) {
        let src_ino = src_fd.map(|fd| {
            let mut st: libc::stat = unsafe { core::mem::zeroed() };
            // SAFETY: the descriptor is live for the call.
            assert_eq!(unsafe { libc::fstat(fd, &mut st) }, 0);
            st.st_ino as u64
        });
        self.seen.lock().unwrap().push((ino, src_ino));
        self.publish(fb, plane, ino, live_inos, fence);
    }
}

/// The presenter is handed a live descriptor for the committed buffer, even
/// after the client closed its GEM handle, so it can copy a buffer it does
/// not own.
#[test]
fn presenter_gets_the_committed_buffer_as_a_source() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut d = Device::new(
        PanelDesc::default(),
        Box::new(SourceSpyPresenter {
            seen: Arc::clone(&seen),
        }),
    );
    d.set_clock(Box::new(|| (1000, 500)));
    let blob = create_mode_blob(&mut d).0;
    let (handle, fb, fd) = import_fb(&mut d);
    ok(&call(
        &mut d,
        DRM_IOCTL_GEM_CLOSE,
        &pod(&DrmGemClose { handle, pad: 0 }),
    ));
    let req = atomic_req(
        &modeset_sets(&d, blob, fb),
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT,
        0xA1,
    );
    ok(&call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req));
    let _ = wait_flip(&mut d, 2000);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (ino, src_ino) = seen[0];
    assert!(ino.is_some());
    assert_eq!(ino, src_ino);
    // SAFETY: the test's own reference.
    unsafe { libc::close(fd) };
}
