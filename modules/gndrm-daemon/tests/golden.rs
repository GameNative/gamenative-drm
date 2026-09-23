//! Golden tests: replay gamescope's discovery → modeset → flip sequence
//! against the daemon exactly as a libdrm client would drive it, asserting on
//! the *bytes* returned (doc `02-gamenative-drm.md` §8, tasks A3–A7).

use std::os::fd::RawFd;

use gndrm_daemon::pod::{pod_read, pod_write};
use gndrm_daemon::{
    find_prop_id, Device, DispatchResult, NullPresenter, PanelDesc, CONNECTOR_ID, CRTC_ID,
    ENCODER_ID, PLANE_CURSOR_ID, PLANE_PRIMARY_ID,
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

fn call(d: &mut Device, nr: u32, payload: &[u8]) -> DispatchResult {
    d.dispatch(nr, payload, None)
}

fn call_fd(d: &mut Device, nr: u32, payload: &[u8], fd: RawFd) -> DispatchResult {
    d.dispatch(nr, payload, Some(fd))
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
    assert_eq!(cap(DRM_CAP_SYNCOBJ), (0, Some(0)));
    assert_eq!(cap(DRM_CAP_ADDFB2_MODIFIERS), (0, Some(0)));
    assert_eq!(cap(0xDEAD).0, -errno::EINVAL);
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
    assert_eq!(c.count_props, 2);
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
    assert_eq!(m.vrefresh, 60);
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
    assert_eq!(p.count_format_types, 5);
    assert_eq!(p.possible_crtcs, 1);
    let off = core::mem::size_of::<DrmModeGetPlane>();
    let fmts = &r.body[off..off + 20];
    assert!(fmts
        .chunks(4)
        .any(|f| u32::from_le_bytes(f.try_into().unwrap()) == DRM_FORMAT_XRGB8888));
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
        pixel_format: DRM_FORMAT_XRGB8888,
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

    // Modifier-flagged request rejected (cap ADDFB2_MODIFIERS = 0).
    let mut bad = base;
    bad.flags = DRM_MODE_FB_MODIFIERS;
    bad.modifier[0] = 0x0100_0000_0000_0002; // some tiling modifier
    is_err(
        &call(&mut d, DRM_IOCTL_MODE_ADDFB2, &pod(&bad)),
        errno::EINVAL,
    );

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
            pixel_format: DRM_FORMAT_XRGB8888,
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
    let mut d = dev();
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
    assert!(r.events.is_empty());
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

    // Real modeset: allowed, presents the FB, emits the flip event.
    let req = atomic_req(
        &sets,
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
        0xCAFE,
    );
    let r = call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req);
    ok(&r);
    assert_eq!(r.events.len(), 1);
    let ev = r.events[0];
    assert_eq!(ev.base.ty, DRM_EVENT_FLIP_COMPLETE);
    assert_eq!(
        ev.base.length as usize,
        core::mem::size_of::<DrmEventVblank>()
    );
    assert_eq!(ev.user_data, 0xCAFE);
    assert_eq!(ev.crtc_id, CRTC_ID);
    assert_eq!((ev.tv_sec, ev.tv_usec), (1000, 500));
    assert_eq!(ev.sequence, 1);

    // State is now visible to queries (GETCRTC / OBJ_GETPROPERTIES).
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

    // Page-flip-only commit (no ALLOW_MODESET) works and sequences.
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
    assert_eq!(r.events.len(), 1);
    assert_eq!(r.events[0].sequence, 2);
    assert_eq!(r.events[0].user_data, 0xBEEF);

    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
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

    // -1 ("no fence") must NOT attach an fd.
    let mut sets = modeset_sets(&d, blob, fb);
    sets.push((
        PLANE_PRIMARY_ID,
        find_prop_id(PLANE_PRIMARY_ID, "IN_FENCE_FD").unwrap(),
        u64::from(u32::MAX),
    ));
    let req = atomic_req(&sets, DRM_MODE_ATOMIC_ALLOW_MODESET, 0);
    let r = call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req);
    ok(&r);

    // SAFETY: caller's fd.
    unsafe { libc::close(fd) };
}
