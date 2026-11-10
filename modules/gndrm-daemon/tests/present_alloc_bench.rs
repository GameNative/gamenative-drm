//! Cost of the steady-state flip path, as counts rather than opinions.
//!
//! Runs the daemon exactly as `golden.rs` does — real `dispatch` calls with a
//! linearized ATOMIC request — and reports two things over N identical flips:
//!
//!   * allocations and bytes allocated inside the loop, counted by a global
//!     allocator wrapper (a hard count, not a wall-clock inference);
//!   * nanoseconds per commit, for the second column.
//!
//! Both numbers are comparable across revisions because the file is identical
//! in each. It is a bench, not an assertion: nothing here fails on a budget,
//! it prints `BENCH ...` for a human to read.
//!
//! Run with:
//!
//! ```sh
//! cargo test --release -p gndrm-daemon --test present_alloc_bench -- --nocapture
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use gndrm_daemon::pod::{pod_read, pod_write};
use gndrm_daemon::{
    find_prop_id, Device, DispatchResult, NullPresenter, PanelDesc, CONNECTOR_ID, CRTC_ID,
    PLANE_PRIMARY_ID, UNKNOWN_PANEL_HZ,
};
use gndrm_uapi::drm::*;
use gndrm_uapi::mode::*;

// ---------------------------------------------------------------------------
// counting allocator
// ---------------------------------------------------------------------------

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

struct Counting;

// SAFETY: every method forwards to `System` unchanged; the counters are the
// only addition, and an off-by-one in them cannot affect soundness.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// ---------------------------------------------------------------------------
// harness (copied from golden.rs, which is where the sequences are proven)
// ---------------------------------------------------------------------------

fn dev() -> Device {
    let mut d = Device::new(PanelDesc::default(), Box::<NullPresenter>::default());
    d.set_clock(Box::new(|| (1000, 500)));
    d
}

fn call(d: &mut Device, nr: u32, payload: &[u8]) -> DispatchResult {
    d.dispatch(nr, payload, None)
}

fn call_fd(d: &mut Device, nr: u32, payload: &[u8], fd: RawFd) -> DispatchResult {
    // The wire's SCM_RIGHTS duplicates the descriptor into the daemon; the
    // client keeps its own reference. Simulate that so neither side
    // double-closes one fd. SAFETY: dup of a live fd.
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    assert!(dup >= 0, "dup fd for SCM_RIGHTS");
    d.dispatch(nr, payload, Some(dup))
}

fn ok(r: &DispatchResult) {
    assert_eq!(r.ret, 0, "expected success, got {}", r.ret);
}

fn pod<T: Copy>(v: &T) -> Vec<u8> {
    let mut b = Vec::new();
    pod_write(&mut b, v);
    b
}

/// `memfd_create` — stands in for a dma-buf (fstat-able, dup-able, uniquely
/// `st_ino`-identifiable, same as the real thing).
fn memfd(size: i64) -> RawFd {
    // SAFETY: valid name, standard flags.
    let fd = unsafe { libc::memfd_create(c"gndrm-bench".as_ptr(), 0) };
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

fn create_mode_blob(d: &mut Device) -> u32 {
    let mode = PanelDesc::default().modeinfo(UNKNOWN_PANEL_HZ);
    let mut req = pod(&DrmModeCreateBlob {
        length: core::mem::size_of::<DrmModeModeinfo>() as u32,
        ..Default::default()
    });
    req.extend_from_slice(&pod(&mode));
    let r = call(d, DRM_IOCTL_MODE_CREATEPROPBLOB, &req);
    ok(&r);
    pod_read::<DrmModeCreateBlob>(&r.body).unwrap().blob_id
}

fn import_fb(d: &mut Device) -> (u32, u32, RawFd) {
    let fd = memfd(1280 * 720 * 4);
    let r = call_fd(
        d,
        DRM_IOCTL_PRIME_FD_TO_HANDLE,
        &pod(&DrmPrimeHandle::default()),
        fd,
    );
    ok(&r);
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
    ok(&r);
    let fb_id = pod_read::<DrmModeFbCmd2>(&r.body).unwrap().fb_id;
    (handle, fb_id, fd)
}

/// The properties gamescope re-sends on every page flip. Deliberately *not* a
/// modeset: no ALLOW_MODESET, no MODE_ID, no ACTIVE — the steady-state shape.
fn flip_sets(fb: u32) -> Vec<(u32, u32, u64)> {
    vec![
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

fn modeset_sets(blob: u32, fb: u32) -> Vec<(u32, u32, u64)> {
    let mut v = vec![
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
    ];
    v.extend(flip_sets(fb));
    v
}

const ITERS: usize = 4000;

#[test]
fn present_path_cost() {
    let mut d = dev();
    let blob = create_mode_blob(&mut d);
    let (h0, fb0, fd0) = import_fb(&mut d);
    let (h1, fb1, fd1) = import_fb(&mut d);

    // Modeset once so the flip loop is steady state.
    let req = atomic_req(
        &modeset_sets(blob, fb0),
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
        0xA000,
    );
    let r = call(&mut d, DRM_IOCTL_MODE_ATOMIC, &req);
    ok(&r);

    // Pre-build both requests: the loop must measure the daemon, not the
    // harness's own `Vec` growth.
    let reqs: Vec<Vec<u8>> = [fb0, fb1]
        .iter()
        .cycle()
        .take(2)
        .map(|fb| {
            atomic_req(
                &flip_sets(*fb),
                DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
                0xB000,
            )
        })
        .collect();

    // Warm up: first-touch pages, log caches, lazily built tables.
    for _ in 0..64 {
        for q in &reqs {
            let r = call(&mut d, DRM_IOCTL_MODE_ATOMIC, q);
            ok(&r);
        }
        let _ = d.drain_flip_events();
    }

    let a0 = ALLOCS.load(Ordering::Relaxed);
    let b0 = BYTES.load(Ordering::Relaxed);
    let t0 = Instant::now();

    for i in 0..ITERS {
        let q = &reqs[i & 1];
        let r = call(&mut d, DRM_IOCTL_MODE_ATOMIC, q);
        ok(&r);
        if i % 256 == 255 {
            let _ = d.drain_flip_events();
        }
    }

    let dt = t0.elapsed();
    let allocs = ALLOCS.load(Ordering::Relaxed) - a0;
    let bytes = BYTES.load(Ordering::Relaxed) - b0;
    let _ = d.drain_flip_events();

    println!(
        "BENCH flips={ITERS} allocs={allocs} bytes={bytes} per_flip={:.2} allocs, {:.1} bytes | ns/flip={:.0}",
        allocs as f64 / ITERS as f64,
        bytes as f64 / ITERS as f64,
        dt.as_nanos() as f64 / ITERS as f64,
    );

    // Keep the import handles alive to the end so the daemon's close paths run
    // after the measurement, not inside it.
    for (h, fd) in [(h0, fd0), (h1, fd1)] {
        let _ = call(
            &mut d,
            DRM_IOCTL_GEM_CLOSE,
            &pod(&DrmGemClose {
                handle: h,
                ..Default::default()
            }),
        );
        // SAFETY: fd is ours and still open.
        unsafe { libc::close(fd) };
    }
}
