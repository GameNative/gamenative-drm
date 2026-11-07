//! Client for gndrm's scan-out allocator channel.
//!
//! The guest's swapchain images have to be buffers the daemon owns: the
//! presenter can only publish an `AHardwareBuffer` it allocated itself, because
//! one cannot be reconstructed from a bare dma-buf fd on this platform. The
//! LD_PRELOAD shim redirects gamescope's own `DMA_HEAP_IOCTL_ALLOC` calls by
//! intercepting the ioctl; this ICD runs in a guest process, where that
//! interception is not something this code can rely on, so its swapchain
//! images are requested from the daemon explicitly — over the same channel,
//! with the same `ScanoutAllocReq`/`ScanoutAllocResp` exchange, and the same
//! `SCM_RIGHTS` descriptor hand-over the shim uses. A pool buffer is thereby
//! registered in the presenter's map by construction, not by hoping an
//! interposition caught the allocation.
//!
//! On any failure (no `GAMENATIVE_DRM_SOCKET`, unreachable or refusing daemon)
//! the caller falls back to a local system-heap allocation, which is the
//! pre-existing behaviour rather than a broken one.

use gndrm_uapi::wire::{
    scanout_ahb_format, FrameCapNote, FrameCapSubscribe, LsfgConfig, ReleaseFence, ScanoutAllocReq,
    ScanoutAllocResp, ScanoutLayout, VsyncNote, CAP_FRAME_CAP, CAP_PRESENT_ID, CAP_RELEASE_FENCE,
    CAP_VSYNC_NOTE, FPS_UNCAPPED, SCANOUT_LAYOUT_ENV, SCANOUT_REQ_AHB_HANDLE,
};
use std::ffi::c_void;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;

/// Default budget for one exchange, matching the LD_PRELOAD shim's: a daemon
/// that has stopped answering must not stall the guest's swapchain creation.
const TIMEOUT_US: i64 = 250_000;

/// One nanosecond per second, the release floor's numerator.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// A buffer the daemon allocated, already registered under `ino` in the
/// presenter's map. The caller owns `fd` and closes it with the slot.
pub struct ScanoutBuf {
    pub fd: RawFd,
    pub ino: u64,
    /// Row stride in bytes, as the daemon's `AHardwareBuffer` reports it.
    pub stride: u32,
    /// This process's reference to the buffer's `AHardwareBuffer`, when it was
    /// asked for ([`allocate_ahb`]); null otherwise. The caller releases it
    /// with [`ahb_release`].
    pub ahb: *mut c_void,
}

struct AhbRecvApi {
    recv: unsafe extern "C" fn(i32, *mut *mut c_void) -> i32,
    release: unsafe extern "C" fn(*mut c_void),
}

fn ahb_recv_api() -> Option<&'static AhbRecvApi> {
    static API: std::sync::OnceLock<Option<AhbRecvApi>> = std::sync::OnceLock::new();
    API.get_or_init(|| unsafe {
        let h = libc::dlopen(c"libnativewindow.so".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if h.is_null() {
            return None;
        }
        let r = libc::dlsym(h, c"AHardwareBuffer_recvHandleFromUnixSocket".as_ptr());
        let rel = libc::dlsym(h, c"AHardwareBuffer_release".as_ptr());
        if r.is_null() || rel.is_null() {
            return None;
        }
        Some(AhbRecvApi {
            recv: std::mem::transmute(r),
            release: std::mem::transmute(rel),
        })
    })
    .as_ref()
}

/// Drop a reference [`allocate_ahb`] or [`lookup_ahb`] handed out.
///
/// # Safety
/// `ahb` is null or a reference this process received and still owns.
pub unsafe fn ahb_release(ahb: *mut c_void) {
    if ahb.is_null() {
        return;
    }
    if let Some(api) = ahb_recv_api() {
        // SAFETY: `ahb` is a reference this process received and still owns.
        unsafe { (api.release)(ahb) };
    }
}

/// The allocator connection, `-1` while not connected.
///
/// Reconnected on demand rather than once at startup: the daemon serves a
/// single scan-out reader and a newer connection supersedes an older one, so a
/// channel displaced from the daemon's side is only discovered on the next
/// exchange.
static CHAN: Mutex<RawFd> = Mutex::new(-1);

// ---------------------------------------------------------------- frame cap
//
// The daemon decides the fps cap's target and tells this shim on this channel
// (see `gndrm_uapi::wire::FrameCapNote`). The cap is applied in `acquire`, where
// the guest asks for the next image: a frame that may not start yet can be held
// there at no cost, while holding a release would delay a frame that is already
// on its way to the panel.

/// The daemon's declared target, [`FPS_UNCAPPED`] when none has.
///
/// An atomic, not a mutex: written on the thread that reads notes, read by every
/// acquire, which runs on the guest's own thread.
static CAP_FPS: AtomicU32 = AtomicU32::new(FPS_UNCAPPED);

/// Notes adopted on this connection. Read by the census: `notes=0` means no
/// target ever arrived, which is a different fault from a target whose floor
/// never bound.
static NOTES_N: AtomicUsize = AtomicUsize::new(0);

/// One target period in nanoseconds, truncated to whole nanoseconds so a period
/// is never longer than `1 / target`, and zero when no cap is in force.
#[must_use]
pub(crate) fn period_ns(target_fps: u32) -> u64 {
    if target_fps == FPS_UNCAPPED {
        return 0;
    }
    NANOS_PER_SECOND / u64::from(target_fps)
}

/// The instant the next acquire may start, given the previous one and now.
///
/// The cap is applied here rather than to a release: a frame that may not start
/// yet is the only point where waiting costs neither rendered work nor display
/// latency. `prev + period` holds the interval at the target while the guest
/// keeps up: the wait for `prev` returns a hair late, and that hair must not
/// become the cadence (it did, as `max(prev, now) + period`: a 60 cap ran at
/// 59.4). Once the guest is a whole period behind it is its own rate again,
/// `now + period` — never two frames closer than one period, and no debt
/// carried out of a stall.
#[must_use]
pub(crate) fn next_acquire_after(prev_ns: u64, now_ns: u64, period_ns: u64) -> u64 {
    if period_ns == 0 {
        return prev_ns;
    }
    if prev_ns != 0 && now_ns < prev_ns.saturating_add(period_ns) {
        prev_ns + period_ns
    } else {
        now_ns.saturating_add(period_ns)
    }
}

/// Adopt one frame-cap note: the target it carries.
fn apply_note(note: FrameCapNote) {
    NOTES_N.fetch_add(1, Ordering::Relaxed);
    CAP_FPS.store(note.fps, Ordering::Relaxed);
    // 0 while frame generation paces the session: log what is actually in force
    // now that the note is adopted, not what it carried.
    crate::logf!(gndrm_obf::obf!(
        "scanout frame cap: fps={} effective={}",
        note.fps,
        target_fps()
    ));
}

/// The target in force, [`FPS_UNCAPPED`] when none has: read by the shim's
/// census and by the acquire gate.
pub(crate) fn target_fps() -> u32 {
    // Frame generation owns the source rate while it is on: the app disarms
    // gamescope's limiter in that case, so a zero from the daemon's cap note is
    // not "uncapped", it is "paced by the presenter".
    if let Some(cap) = crate::lsfg::source_cap() {
        return cap;
    }
    CAP_FPS.load(Ordering::Relaxed)
}

/// The cap the daemon's note carries, [`FPS_UNCAPPED`] when none.
pub(crate) fn cap_fps_note() -> u32 {
    CAP_FPS.load(Ordering::Relaxed)
}

/// Adopt the daemon's vsync note: frame generation places its commits on the
/// latch grid, and the acquire gate releases on the vblank grid it gives.
fn apply_vsync(note: VsyncNote) {
    crate::lsfg::apply_vsync(note);
    crate::vsync_gate::apply_note(note);
}

/// How many frame-cap notes have been adopted on this connection, for the same
/// census.
pub(crate) fn notes_seen() -> usize {
    NOTES_N.load(Ordering::Relaxed)
}

/// Send one frame that expects no reply; `false` when the channel did not
/// accept it: the frame-cap subscribe, the one fire-and-forget frame on this
/// channel.
fn send_frame_nb(fd: RawFd, wire: &[u8]) -> bool {
    // SAFETY: `fd` is the connection this module owns, `wire` outlives the call.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: wire.as_ptr().cast_mut().cast(),
            iov_len: wire.len(),
        };
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) >= 0
    }
}

fn send_frame(fd: RawFd, wire: &[u8]) -> bool {
    // SAFETY: `fd` is the connection this module owns, `wire` outlives the
    // call. MSG_NOSIGNAL: a displaced or closed channel must fail rather than
    // raise SIGPIPE inside the guest.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: wire.as_ptr().cast_mut().cast(),
            iov_len: wire.len(),
        };
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL) >= 0
    }
}

/// Why an exchange did not produce a buffer.
enum Failure {
    /// The daemon answered and refused (EINVAL/ENOSPC/ENOSYS); the channel is
    /// healthy.
    Refused,
    /// The connection itself is unusable: closed, displaced, or a reply that
    /// could not be framed.
    Dead,
}

/// The session's scan-out pool layout: `GNDRM_SCANOUT_LAYOUT=rgba` selects
/// [`ScanoutLayout::Rgba8888`], anything else [`ScanoutLayout::Bgra8888`].
#[must_use]
pub fn layout() -> ScanoutLayout {
    std::env::var(SCANOUT_LAYOUT_ENV)
        .ok()
        .and_then(|v| ScanoutLayout::from_env_value(&v))
        .unwrap_or(ScanoutLayout::Bgra8888)
}

/// Ask the daemon named by `GAMENATIVE_DRM_SOCKET` for a `width`x`height`
/// buffer in the session's [`layout`]. `None` when the channel is unset,
/// unreachable, or refuses; the caller then allocates locally.
#[must_use]
/// Ask the daemon for the app's frame-generation settings.
///
/// Same channel as [`allocate`], deliberately: the daemon serves a single
/// scan-out reader, so a second connection would displace the allocator's. Call
/// this before the first allocation of a session, when nothing is in flight —
/// then the only frames on the wire are notes the daemon pushes, which are
/// skipped by magic.
pub fn lsfg_config() -> Option<LsfgConfig> {
    let path = std::env::var("GAMENATIVE_DRM_SOCKET").ok()?;
    if path.is_empty() {
        return None;
    }
    let mut ch = CHAN.lock().unwrap_or_else(|e| e.into_inner());
    if *ch < 0 {
        *ch = connect(&path);
        if *ch < 0 {
            return None;
        }
    }
    let wire = LsfgConfig::request().encode();
    // SAFETY: `*ch` is this module's descriptor; both buffers outlive the calls.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: wire.as_ptr().cast_mut().cast(),
            iov_len: wire.len(),
        };
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if libc::sendmsg(*ch, &msg, libc::MSG_NOSIGNAL) < 0 {
            libc::close(*ch);
            *ch = -1;
            return None;
        }
        // Bounded: a channel that answers with neither our reply nor a frame we
        // recognise is not going to, and the caller has env defaults to fall back
        // to. The subscription's answer (cap note, settings, vsync note) can be
        // ahead of the reply, so the budget covers those plus a few empty reads.
        for _ in 0..8 {
            let mut buf = [0u8; PUSH_LEN];
            let n = libc::recv(*ch, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT);
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
                    // Give the daemon a moment; it answers within one poll turn.
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                libc::close(*ch);
                *ch = -1;
                return None;
            }
            if let Some(settings) = LsfgConfig::decode(&buf[..n as usize]) {
                return (!settings.is_request()).then_some(settings);
            }
            // The answer to the subscription `connect` just sent, or a cap the
            // user changed meanwhile: a note read here is still a note.
            if let Some(note) = FrameCapNote::decode(&buf[..n as usize]) {
                apply_note(note);
            } else if let Some(note) = VsyncNote::decode(&buf[..n as usize]) {
                apply_vsync(note);
            } else if let Some(done) = gndrm_uapi::wire::PresentDone::decode(&buf[..n as usize]) {
                crate::present::record(done.id, done.latch_ns);
            } else if gndrm_uapi::wire::FrameAcquired::decode(&buf[..n as usize]).is_some() {
                // The shim's own counter, echoed nowhere: accepted so it is not
                // mistaken for a frame this reader does not know.
            }
        }
    }
    None
}

pub fn allocate(width: u32, height: u32) -> Option<ScanoutBuf> {
    allocate_with(width, height, false)
}

/// [`allocate`], plus the buffer's `AHardwareBuffer`, for a driver that imports
/// only through `VK_ANDROID_external_memory_android_hardware_buffer`. `None`
/// when libnativewindow cannot receive a handle or the daemon does not send one.
pub fn allocate_ahb(width: u32, height: u32) -> Option<ScanoutBuf> {
    ahb_recv_api()?;
    allocate_with(width, height, true)
}

fn allocate_with(width: u32, height: u32, want_ahb: bool) -> Option<ScanoutBuf> {
    request(ScanoutAllocReq {
        width,
        height,
        format: scanout_ahb_format(layout()) | if want_ahb { SCANOUT_REQ_AHB_HANDLE } else { 0 },
        len: u64::from(width) * u64::from(height) * 4,
    })
}

/// The pool buffer the daemon handed out under dma-buf inode `ino` (to this
/// process or another), with a fresh descriptor and its `AHardwareBuffer`.
/// `None` when the daemon has no such buffer, predates the lookup, or the
/// handle cannot be received.
pub fn lookup_ahb(ino: u64) -> Option<ScanoutBuf> {
    ahb_recv_api()?;
    request(ScanoutAllocReq::lookup_ahb(ino, layout()))
}

fn request(req: ScanoutAllocReq) -> Option<ScanoutBuf> {
    let path = std::env::var("GAMENATIVE_DRM_SOCKET").ok()?;
    if path.is_empty() {
        return None;
    }
    // A few exchanges, reconnecting between them: the daemon serves a single
    // scan-out reader, so a channel displaced by another client (the LD_PRELOAD
    // shim in gamescope's process also holds one) only shows it on the next
    // send. Matches the shim's own retry budget.
    let mut ch = CHAN.lock().unwrap_or_else(|e| e.into_inner());
    for _ in 0..3 {
        if *ch < 0 {
            *ch = connect(&path);
            if *ch < 0 {
                return None;
            }
        }
        match exchange_req(*ch, req) {
            Ok(buf) => return Some(buf),
            Err(Failure::Refused) => return None,
            Err(Failure::Dead) => {
                // SAFETY: `*ch` is a descriptor this module owns.
                unsafe { libc::close(*ch) };
                *ch = -1;
            }
        }
    }
    None
}

/// Connect a `SOCK_SEQPACKET` client to `path`, matching the daemon's address
/// convention (`/abs/path` is a filesystem socket, `@name` or a bare `name` is
/// the abstract namespace) and the full-`sockaddr_un` addrlen both sides use.
/// Returns `-1` on any failure, with both directions bounded by [`TIMEOUT_US`].
fn connect(path: &str) -> RawFd {
    // SAFETY: every libc call below is on a descriptor or buffer this function
    // owns; the sockaddr is zeroed and the name length is checked.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return -1;
        }
        let mut sa: libc::sockaddr_un = core::mem::zeroed();
        sa.sun_family = libc::AF_UNIX as u16;
        let bytes = path.strip_prefix('@').unwrap_or(path).as_bytes();
        if bytes.len() >= sa.sun_path.len() {
            libc::close(fd);
            return -1;
        }
        let dst = if path.contains('/') {
            sa.sun_path.as_mut_ptr()
        } else {
            sa.sun_path.as_mut_ptr().add(1)
        };
        core::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), dst.cast::<u8>(), bytes.len());
        if libc::connect(
            fd,
            (&raw const sa).cast(),
            core::mem::size_of::<libc::sockaddr_un>() as u32,
        ) < 0
        {
            libc::close(fd);
            return -1;
        }
        let tv = libc::timeval {
            tv_sec: 0,
            tv_usec: TIMEOUT_US,
        };
        let tvp = (&raw const tv).cast();
        let tvlen = core::mem::size_of::<libc::timeval>() as u32;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, tvp, tvlen);
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDTIMEO, tvp, tvlen);
        // Opt in to the frame cap on this connection. The daemon latches the
        // subscription per connection and only then sends a `FrameCapNote`
        // (with the hand-out of a buffer, or when the target changes), so
        // sending it here — at the one place a connection is made — is what
        // re-subscribes after a reconnect, with no second site to keep in step.
        // Fire-and-forget: the daemon answers it with the current target, which
        // the reply reader accepts in whatever order it arrives.
        if !send_frame(
            fd,
            &FrameCapSubscribe {
                caps: CAP_FRAME_CAP | CAP_VSYNC_NOTE | CAP_PRESENT_ID | CAP_RELEASE_FENCE,
            }
            .encode(),
        ) {
            libc::close(fd);
            return -1;
        }
        fd
    }
}

/// The most frames one reply burst may hold on this channel: the allocator
/// reply, plus the notes and settings the daemon may put ahead of it (the
/// answer to the subscribe, or every cap change the app made while a request
/// was in flight, two frames each). Beyond this the peer is not speaking this
/// protocol.
const MAX_BURST_FRAMES: usize = 32;

/// One request/response exchange: send a geometry request, receive the reply
/// and its descriptor.
#[cfg(test)]
fn exchange(fd: RawFd, width: u32, height: u32) -> Result<ScanoutBuf, Failure> {
    exchange_req(
        fd,
        ScanoutAllocReq {
            width,
            height,
            format: scanout_ahb_format(ScanoutLayout::Bgra8888),
            len: u64::from(width) * u64::from(height) * 4,
        },
    )
}

fn exchange_req(fd: RawFd, req: ScanoutAllocReq) -> Result<ScanoutBuf, Failure> {
    let want_ahb = req.wants_ahb_handle();
    let wire = req.encode();
    // SAFETY: `fd` is the connection this module owns; `wire`/`buf`/`cbuf`
    // outlive the calls, and only a reply the kernel framed is parsed.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: wire.as_ptr().cast_mut().cast(),
            iov_len: wire.len(),
        };
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        // MSG_NOSIGNAL: a displaced or closed channel must fail with EPIPE and
        // be reconnected, not raise SIGPIPE inside the guest.
        if libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL) < 0 {
            return Err(Failure::Dead);
        }

        // Frames are identified by their own magic, never by position: an
        // unsolicited note (the answer to the subscribe, or a target the user
        // changed) can stand where a reply is expected. A note is applied here
        // and the read continues, so either order leaves the channel in step.
        let mut frames = 0usize;
        loop {
            frames += 1;
            // Sized for the largest frame the daemon can put on this socket: a
            // settings push is far longer than the reply, and SEQPACKET truncates
            // a frame to the buffer, which would make it unreadable.
            let mut buf = [0u8; PUSH_LEN];
            let mut riov = libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            };
            let mut cbuf = [0u8; 64];
            let mut rmsg: libc::msghdr = core::mem::zeroed();
            rmsg.msg_iov = &mut riov;
            rmsg.msg_iovlen = 1;
            rmsg.msg_control = cbuf.as_mut_ptr().cast();
            rmsg.msg_controllen = cbuf.len();
            let n = libc::recvmsg(fd, &mut rmsg, 0);

            // Take any descriptor before validating anything: whatever arrived
            // has to be closed here unless the frame turns out to be the reply
            // it belongs to.
            let mut recv_fd: RawFd = -1;
            let mut cmsg = libc::CMSG_FIRSTHDR(&rmsg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    core::ptr::copy_nonoverlapping(
                        libc::CMSG_DATA(cmsg).cast::<RawFd>(),
                        &mut recv_fd,
                        1,
                    );
                }
                cmsg = libc::CMSG_NXTHDR(&rmsg, cmsg);
            }

            if n > 0 {
                let buf = &buf[..n as usize];
                if let Some(config) = LsfgConfig::decode(buf) {
                    // A settings push, not the reply: apply it and keep waiting.
                    if recv_fd >= 0 {
                        libc::close(recv_fd);
                    }
                    if !config.is_request() {
                        crate::lsfg::apply(config);
                    }
                    if frames < MAX_BURST_FRAMES {
                        continue;
                    }
                } else if let Some(note) = FrameCapNote::decode(buf) {
                    // A note carries no buffer.
                    if recv_fd >= 0 {
                        libc::close(recv_fd);
                    }
                    apply_note(note);
                    if frames < MAX_BURST_FRAMES {
                        continue;
                    }
                } else if let Some(note) = VsyncNote::decode(buf) {
                    if recv_fd >= 0 {
                        libc::close(recv_fd);
                    }
                    apply_vsync(note);
                    if frames < MAX_BURST_FRAMES {
                        continue;
                    }
                } else if let Some(done) = gndrm_uapi::wire::PresentDone::decode(buf) {
                    // A completion pushed while this exchange waits for its
                    // reply: the daemon answers in its own time, and losing the
                    // note here would leave a wait hanging.
                    if recv_fd >= 0 {
                        libc::close(recv_fd);
                    }
                    crate::present::record(done.id, done.latch_ns);
                    if frames < MAX_BURST_FRAMES {
                        continue;
                    }
                } else if let Some(note) = ReleaseFence::decode(buf) {
                    // A buffer's release, pushed while this exchange waits:
                    // kept for the next acquire of that buffer.
                    record_release(note, recv_fd);
                    if frames < MAX_BURST_FRAMES {
                        continue;
                    }
                    return Err(Failure::Dead);
                } else if gndrm_uapi::wire::FrameAcquired::decode(buf).is_some() {
                    // The acquire counter shares this channel and can arrive
                    // while a reply is pending; it is not the reply.
                    if recv_fd >= 0 {
                        libc::close(recv_fd);
                    }
                    if frames < MAX_BURST_FRAMES {
                        continue;
                    }
                } else if let Some(r) = ScanoutAllocResp::decode(buf) {
                    if r.ret != 0 || recv_fd < 0 {
                        // A refusal carries no buffer; the channel stays
                        // usable.
                        if recv_fd >= 0 {
                            libc::close(recv_fd);
                        }
                        return Err(Failure::Refused);
                    }
                    let mut ahb: *mut c_void = core::ptr::null_mut();
                    if want_ahb {
                        // The daemon sends the handle right behind the reply.
                        // Missing it leaves the channel out of step, so the
                        // connection is dropped rather than reused.
                        let got = ahb_recv_api()
                            .map_or(-1, |api| (api.recv)(fd, &mut ahb));
                        if got != 0 || ahb.is_null() {
                            crate::logf!(gndrm_obf::obf!(
                                "scan-out: AHardwareBuffer handle not received ({got})"
                            ));
                            libc::close(recv_fd);
                            return Err(Failure::Dead);
                        }
                    }
                    note_pool_fd_kind(recv_fd, r.ino);
                    note_owned(r.ino);
                    return Ok(ScanoutBuf {
                        fd: recv_fd,
                        ino: r.ino,
                        stride: r.stride,
                        ahb,
                    });
                }
            }
            // Closed peer, timeout, or a frame that is neither shape.
            if recv_fd >= 0 {
                libc::close(recv_fd);
            }
            return Err(Failure::Dead);
        }
    }
}

/// Log, once, what the first pool descriptor is: a buffer whose fd is not a
/// dma-buf cannot be imported as one or scanned out by size.
fn note_pool_fd_kind(fd: RawFd, ino: u64) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !crate::vkshim_enabled() || LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // SAFETY: fstat/fstatfs on a descriptor this module just received; both
    // structs are written on success.
    let (magic, size) = unsafe {
        let mut fs: libc::statfs = core::mem::zeroed();
        let mut st: libc::stat = core::mem::zeroed();
        let magic = if libc::fstatfs(fd, &mut fs) == 0 { fs.f_type as u64 } else { 0 };
        let size = if libc::fstat(fd, &mut st) == 0 { st.st_size as u64 } else { 0 };
        (magic, size)
    };
    crate::logf!(gndrm_obf::obf!(
        "scan-out: pool fd ino={ino} size={size} fs={magic:#x} dma-buf={} link={}",
        magic == 0x444d_4142,
        fd_link(fd)
    ));
}

/// The `/proc/self/fd` link of `fd` (`/dmabuf:…` for a dma-buf), empty when it
/// cannot be read.
pub(crate) fn fd_link(fd: RawFd) -> String {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Adopt any frame-cap note the daemon has pushed on this channel, if one is
/// already there.
///
/// The daemon pushes the target when it changes, and a push is tied to no
/// request, so this is its reader: the shim is told otherwise only when it
/// connects (the answer to its subscribe), and a limiter changed mid-session
/// would never reach an already-connected shim.
///
/// Non-blocking and it gives way to an exchange in progress (`try_lock`). That
/// is what makes it safe to call while a reply may be pending: a reply only
/// exists while `exchange` holds this same lock across its send and receive, so
/// a lock that is free means no reply is in flight and any frame on the socket
/// is a push. A push skipped then is read on the next release.
/// Report the running acquire total to the daemon.
///
/// Non-blocking: this runs on the guest's present thread once per frame, and a
/// full socket must cost a dropped message rather than a stall. Losing one is
/// harmless — the count is cumulative and the next message carries the total.
pub(crate) fn send_frame_acquired(count: u64, generated: u64) {
    let ch = CHAN.lock().unwrap_or_else(|e| e.into_inner());
    if *ch < 0 {
        return;
    }
    send_frame_nb(
        *ch,
        &gndrm_uapi::wire::FrameAcquired { count, generated }.encode(),
    );
}

/// Mark a frame's present id for the daemon, immediately before the commit that
/// presents it: the daemon answers when that transaction latches. Best effort by
/// construction — the channel is shared with the allocator, so an exchange in
/// flight is waited for (bounded by the exchange's own timeout) rather than
/// skipped, because a mark that never arrives leaves `vkWaitForPresentKHR`
/// waiting for a frame the daemon cannot name.
pub(crate) fn send_present_mark(ino: u64, id: u64) {
    let ch = CHAN.lock().unwrap_or_else(|e| e.into_inner());
    if *ch < 0 {
        return;
    }
    send_frame(*ch, &gndrm_uapi::wire::PresentMark { ino, id }.encode());
}

pub(crate) fn poll_notes() {
    let ch = match CHAN.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    if *ch < 0 {
        return;
    }
    // Every frame already queued, not just one: release notes arrive twice per
    // frame, and one read per call would fall behind them.
    for _ in 0..MAX_BURST_FRAMES {
        let Some((buf, len, recv_fd)) = recv_push(*ch) else {
            return;
        };
        dispatch_push(&buf[..len], recv_fd);
    }
}

/// One pushed frame and the descriptor riding with it (`-1` for none), or
/// `None` when nothing is queued (or the peer closed). Non-blocking.
fn recv_push(ch: RawFd) -> Option<([u8; PUSH_LEN], usize, RawFd)> {
    // One frame of the largest push. SEQPACKET preserves boundaries but truncates
    // on a short buffer, so this must cover every push.
    let mut buf = [0u8; PUSH_LEN];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cbuf = [0u64; 8];
    let mut recv_fd: RawFd = -1;
    // SAFETY: `ch` is the connection this module owns; the message points at
    // live stack buffers; MSG_DONTWAIT leaves a frame that has not arrived for
    // the next call.
    let n = unsafe {
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = core::mem::size_of_val(&cbuf) as _;
        let n = libc::recvmsg(ch, &mut msg, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC);
        if n > 0 {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let fd = core::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>());
                    if recv_fd >= 0 {
                        libc::close(recv_fd);
                    }
                    recv_fd = fd;
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        n
    };
    let len = usize::try_from(n).ok().filter(|&l| l > 0)?;
    Some((buf, len, recv_fd))
}

/// Apply one pushed frame. `recv_fd` (owned) is kept only by a release note.
fn dispatch_push(buf: &[u8], recv_fd: RawFd) {
    if let Some(note) = ReleaseFence::decode(buf) {
        record_release(note, recv_fd);
        return;
    }
    if recv_fd >= 0 {
        // SAFETY: a descriptor that rode with a frame that carries none.
        unsafe { libc::close(recv_fd) };
    }
    // Dispatch on the magic: every push type shares the socket, in any order.
    if let Some(note) = FrameCapNote::decode(buf) {
        apply_note(note);
    } else if let Some(note) = VsyncNote::decode(buf) {
        apply_vsync(note);
    } else if let Some(config) = LsfgConfig::decode(buf) {
        if !config.is_request() {
            crate::lsfg::apply(config);
        }
    } else if let Some(done) = gndrm_uapi::wire::PresentDone::decode(buf) {
        // The panel latched the frame this id was marked for.
        crate::present::record(done.id, done.latch_ns);
    }
    // Anything else (the shim's own acquire counter, a frame a newer daemon
    // sends) is ignored.
}

// ------------------------------------------------------------ release fences
//
// The daemon sends gamescope's page-flip event when it publishes a frame, and
// gamescope then hands the replaced buffer back to this shim while the panel
// may still be scanning it out; a guest that renders into it then tears. A
// daemon that knows this shim reads them (`CAP_RELEASE_FENCE`) says so before
// the flip event leaves (a pending note) and forwards the buffer's release fence
// when SurfaceFlinger reports it. The acquire that next hands the buffer to the
// guest waits for both, bounded. An older daemon sends neither, and acquire
// behaves as before.

/// One scan-out buffer's release state.
struct Release {
    ino: u64,
    /// Pending notes not yet followed by their fence.
    outstanding: u32,
    /// The newest fence received, `-1` for none (or "no fence: already free").
    fd: RawFd,
}

/// Release state by scan-out inode, for buffers this process was handed.
static RELEASES: Mutex<Vec<Release>> = Mutex::new(Vec::new());

/// The most buffers tracked; the oldest entry is dropped beyond it.
const MAX_RELEASES: usize = 64;

/// Scan-out inodes this process received from the daemon. Notes for any other
/// buffer (another client's, or the compositor's own) are dropped on arrival.
static OWNED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// The most inodes remembered as owned; the oldest is forgotten beyond it.
const MAX_OWNED: usize = 256;

/// Bound of one acquire-side wait, a 30 Hz frame. A release signals within a
/// vsync or two; this only matters when a note was lost.
pub(crate) const RELEASE_WAIT: std::time::Duration = std::time::Duration::from_millis(33);

/// Release fences waited at acquire, and waits that ran out of budget.
pub(crate) static RELEASE_WAITS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static RELEASE_TIMEOUTS: AtomicUsize = AtomicUsize::new(0);

fn note_owned(ino: u64) {
    let mut o = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    if o.contains(&ino) {
        return;
    }
    if o.len() >= MAX_OWNED {
        o.remove(0);
    }
    o.push(ino);
}

fn owned(ino: u64) -> bool {
    OWNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&ino)
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        // SAFETY: a descriptor the caller owns and gives up here.
        unsafe { libc::close(fd) };
    }
}

/// Take in one release note; `fd` (owned) is the fence that rode with it.
fn record_release(note: ReleaseFence, fd: RawFd) {
    if note.ino == 0 || !owned(note.ino) {
        close_fd(fd);
        return;
    }
    let mut r = RELEASES.lock().unwrap_or_else(|e| e.into_inner());
    let at = r.iter().position(|e| e.ino == note.ino);
    if note.is_pending() {
        close_fd(fd);
        match at {
            Some(i) => r[i].outstanding = r[i].outstanding.saturating_add(1),
            None => {
                if r.len() >= MAX_RELEASES {
                    close_fd(r.remove(0).fd);
                }
                r.push(Release {
                    ino: note.ino,
                    outstanding: 1,
                    fd: -1,
                });
            }
        }
        return;
    }
    // A fence nobody is waiting for: the buffer's wait already ran out (it was
    // handed to the guest), so the fence is dropped rather than kept for a
    // later acquire it does not describe.
    let Some(i) = at.filter(|&i| r[i].outstanding > 0) else {
        close_fd(fd);
        return;
    };
    r[i].outstanding -= 1;
    close_fd(core::mem::replace(&mut r[i].fd, fd));
}

/// What the acquire of one buffer has to wait for.
enum ReleaseState {
    /// Nothing: no note, or the release arrived with no fence.
    Free,
    /// A pending note whose fence has not arrived.
    Pending,
    /// The fence, taken out of the table (the caller owns and closes it).
    Fence(RawFd),
}

fn take_release(ino: u64) -> ReleaseState {
    let mut r = RELEASES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(i) = r.iter().position(|e| e.ino == ino) else {
        return ReleaseState::Free;
    };
    if r[i].outstanding > 0 {
        return ReleaseState::Pending;
    }
    let fd = r.remove(i).fd;
    if fd < 0 {
        ReleaseState::Free
    } else {
        ReleaseState::Fence(fd)
    }
}

/// Forget `ino`'s release state after a wait ran out: a late fence for it is
/// then dropped on arrival.
fn abandon_release(ino: u64) {
    let mut r = RELEASES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = r.iter().position(|e| e.ino == ino) {
        close_fd(r.remove(i).fd);
    }
}

/// Block for at most `left` until the channel has a frame to read (or, with no
/// channel, just for a short slice): the wake for a fence still on its way.
fn wait_channel(left: std::time::Duration) {
    let slice = left.min(std::time::Duration::from_millis(4));
    let ch = match CHAN.try_lock() {
        Ok(g) => *g,
        Err(std::sync::TryLockError::Poisoned(p)) => *p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => -1,
    };
    if ch >= 0 {
        let mut pfd = libc::pollfd {
            fd: ch,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pollfd for the call. The descriptor is only polled,
        // so a reconnect replacing it in the meantime costs at most one slice.
        let rc = unsafe { libc::poll(&mut pfd, 1, slice.as_millis().max(1) as i32) };
        if rc > 0 && pfd.revents & libc::POLLIN != 0 {
            return;
        }
        if rc == 0 {
            return;
        }
    }
    std::thread::sleep(slice.min(std::time::Duration::from_millis(1)));
}

/// Wait for a sync file to signal, for at most `left`. `true` when it did.
fn wait_sync_file(fd: RawFd, left: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + left;
    loop {
        let rest = deadline.saturating_duration_since(std::time::Instant::now());
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // Rounded up, so a sub-millisecond remainder still waits.
        let ms = rest.as_micros().div_ceil(1000) as i32;
        // SAFETY: one live pollfd for the call.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc > 0 {
            return true;
        }
        let eintr = rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR);
        if !eintr || rest.is_zero() {
            return false;
        }
    }
}

/// Before the guest is given buffer `ino` again: wait, for at most `budget`,
/// until the panel is done with it. Returns at once when the daemon announced
/// no release for it (an older daemon, or a buffer the compositor did not scan
/// out). Each fence is waited once and closed here.
pub(crate) fn wait_release(ino: u64, budget: std::time::Duration) {
    if ino == 0 {
        return;
    }
    let start = std::time::Instant::now();
    let deadline = start + budget;
    poll_notes();
    let done = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match take_release(ino) {
            ReleaseState::Free => break true,
            ReleaseState::Fence(fd) => {
                RELEASE_WAITS.fetch_add(1, Ordering::Relaxed);
                let signalled = wait_sync_file(fd, left);
                close_fd(fd);
                break signalled;
            }
            ReleaseState::Pending => {
                if left.is_zero() {
                    abandon_release(ino);
                    break false;
                }
                wait_channel(left);
                poll_notes();
            }
        }
    };
    if !done {
        let n = RELEASE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            crate::logi!(gndrm_obf::obf!(
                "scan-out: a release wait ran out after {}ms (ino={ino} waits={}); counted from here on",
                start.elapsed().as_millis(),
                RELEASE_WAITS.load(Ordering::Relaxed)
            ));
        }
    }
}

/// Largest frame the daemon pushes unsolicited.
const PUSH_LEN: usize = LsfgConfig::LEN;

#[cfg(test)]
mod tests {
    use super::*;

    /// Bind a `SOCK_SEQPACKET` listener on the abstract namespace, the way the
    /// daemon's scan-out acceptor does.
    fn listen(name: &str) -> RawFd {
        // SAFETY: fresh socket; sockaddr_un is zeroed and the name fits.
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
            assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
            let mut sa: libc::sockaddr_un = core::mem::zeroed();
            sa.sun_family = libc::AF_UNIX as u16;
            let b = name.as_bytes();
            assert!(b.len() < sa.sun_path.len() - 1);
            core::ptr::copy_nonoverlapping(
                b.as_ptr().cast(),
                sa.sun_path.as_mut_ptr().add(1).cast::<u8>(),
                b.len(),
            );
            assert_eq!(
                libc::bind(
                    fd,
                    (&raw const sa).cast(),
                    core::mem::size_of::<libc::sockaddr_un>() as u32
                ),
                0,
                "bind: {}",
                std::io::Error::last_os_error()
            );
            assert_eq!(libc::listen(fd, 1), 0);
            fd
        }
    }

    /// `sendmsg` one frame with `payload` attached via `SCM_RIGHTS`.
    fn send_with_fd(fd: RawFd, bytes: &[u8], payload: RawFd) {
        // SAFETY: valid fd, iovec and control buffer sized for one descriptor.
        unsafe {
            let mut iov = libc::iovec {
                iov_base: bytes.as_ptr().cast_mut().cast(),
                iov_len: bytes.len(),
            };
            let mut cbuf = [0u8; 64];
            let mut msg: libc::msghdr = core::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(4) as usize;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(4) as usize;
            core::ptr::write(libc::CMSG_DATA(cmsg).cast::<RawFd>(), payload);
            assert_eq!(libc::sendmsg(fd, &msg, 0), bytes.len() as isize);
        }
    }

    /// The frame-cap subscribe a client sends on connect, so a stand-in daemon
    /// reads it before the client's first request.
    fn recv_subscribe(conn: RawFd) {
        // SAFETY: `conn` is the accepted connection and `buf` is one full frame.
        unsafe {
            let mut buf = [0u8; FrameCapSubscribe::LEN];
            let n = libc::recv(conn, buf.as_mut_ptr().cast(), buf.len(), 0);
            assert_eq!(n as usize, FrameCapSubscribe::LEN);
            let sub = FrameCapSubscribe::decode(&buf).expect(&gndrm_obf::obf!("subscribe frame"));
            assert_eq!(sub.caps & CAP_FRAME_CAP, CAP_FRAME_CAP);
        }
    }

    /// `send` one frame with no descriptor attached.
    fn send_bare(conn: RawFd, bytes: &[u8]) {
        // SAFETY: a live fd and a byte slice that outlives the call.
        unsafe {
            assert_eq!(
                libc::send(conn, bytes.as_ptr().cast(), bytes.len(), 0),
                bytes.len() as isize
            );
        }
    }

    /// The client speaks the shim's protocol: a geometry request goes out, the
    /// reply's descriptor and identity come back. A daemon stand-in answers
    /// with a `/dev/null` descriptor, which is all the transport needs.
    #[test]
    fn connect_and_exchange_receive_a_pool_buffer() {
        let name = format!("{}", gndrm_obf::obf!("gndrm-icdsc-{}", std::process::id()));
        let listener = listen(&name);
        let server = std::thread::spawn(move || {
            // SAFETY: accept on the listener created above, then recv/send on
            // the accepted connection; every descriptor is closed once.
            unsafe {
                let conn = libc::accept4(
                    listener,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                );
                assert!(conn >= 0, "accept: {}", std::io::Error::last_os_error());
                recv_subscribe(conn);
                let mut buf = [0u8; ScanoutAllocReq::LEN];
                let n = libc::recv(conn, buf.as_mut_ptr().cast(), buf.len(), 0);
                assert_eq!(n as usize, ScanoutAllocReq::LEN);
                let req = ScanoutAllocReq::decode(&buf).expect(&gndrm_obf::obf!("request frame"));
                assert_eq!((req.width, req.height), (1280, 960));
                assert_eq!(req.format, scanout_ahb_format(ScanoutLayout::Bgra8888));
                assert_eq!(req.len, 1280 * 960 * 4);

                let payload = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
                assert!(payload >= 0);
                let mut st: libc::stat = core::mem::zeroed();
                assert_eq!(libc::fstat(payload, &mut st), 0);
                let resp = ScanoutAllocResp {
                    ret: 0,
                    ino: st.st_ino as u64,
                    stride: 5120,
                }
                .encode();
                send_with_fd(conn, &resp, payload);
                libc::close(payload);
                libc::close(conn);
                libc::close(listener);
            }
        });

        let fd = connect(&name);
        assert!(fd >= 0, "connect: {}", std::io::Error::last_os_error());
        let got = exchange(fd, 1280, 960)
            .ok()
            .expect(&gndrm_obf::obf!("exchange"));
        assert_eq!(got.stride, 5120);
        // The received descriptor is the one the daemon sent, and the reply's
        // identity is that descriptor's inode — the registration key.
        // SAFETY: both descriptors are live; `st` is fully written on success.
        unsafe {
            let mut st: libc::stat = core::mem::zeroed();
            assert_eq!(libc::fstat(got.fd, &mut st), 0);
            assert_eq!(got.ino, st.st_ino as u64);
            libc::close(got.fd);
            libc::close(fd);
        }
        server.join().expect(&gndrm_obf::obf!("server thread"));
    }

    /// The frame-cap note stands ahead of the hand-out reply, where the shim
    /// would otherwise expect the reply. The reader finds each frame by its own
    /// magic, so the target is adopted and the reply is still the reply.
    #[test]
    fn exchange_sorts_the_frame_cap_note_from_the_reply() {
        let name = format!("{}", gndrm_obf::obf!("gndrm-icdcap-{}", std::process::id()));
        let listener = listen(&name);
        let server = std::thread::spawn(move || {
            // SAFETY: accept on the listener created above; every frame is a
            // full 24 bytes and every descriptor is closed once. Returns the
            // buffer's ino.
            unsafe {
                let conn = libc::accept4(
                    listener,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                );
                assert!(conn >= 0, "accept: {}", std::io::Error::last_os_error());
                recv_subscribe(conn);
                // The daemon answers a subscribe with the current target, ahead
                // of any reply: a note where a reply would be expected.
                send_bare(conn, &FrameCapNote { fps: 120 }.encode());
                let mut buf = [0u8; ScanoutAllocReq::LEN];
                let n = libc::recv(conn, buf.as_mut_ptr().cast(), buf.len(), 0);
                assert_eq!(n as usize, ScanoutAllocReq::LEN);

                let payload = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
                assert!(payload >= 0);
                let mut st: libc::stat = core::mem::zeroed();
                assert_eq!(libc::fstat(payload, &mut st), 0);
                let ino = st.st_ino as u64;
                let resp = ScanoutAllocResp {
                    ret: 0,
                    ino,
                    stride: 5120,
                }
                .encode();
                send_with_fd(conn, &resp, payload);
                libc::close(payload);
                libc::close(conn);
                libc::close(listener);
                ino
            }
        });

        let fd = connect(&name);
        assert!(fd >= 0, "connect: {}", std::io::Error::last_os_error());
        let got = exchange(fd, 1280, 960)
            .ok()
            .expect(&gndrm_obf::obf!("exchange"));
        let ino = server.join().expect(&gndrm_obf::obf!("server thread"));
        assert_eq!(got.ino, ino);
        // The note ahead of the reply set the target.
        assert_eq!(target_fps(), 120);
        // SAFETY: the received descriptor and the connection are live.
        unsafe {
            libc::close(got.fd);
            libc::close(fd);
        }
    }

    /// The acquire schedule: one period after the previous start, never closer,
    /// and never later than the guest's own next frame when it is slower.
    #[test]
    fn the_cadence_does_not_carry_the_wakeup_overshoot() {
        let period = 16_666_666;
        let first = next_acquire_after(0, 1_000_000_000, period);
        assert_eq!(first, 1_016_666_666);
        // The wait returned 150 us late: the next deadline is still one period on.
        let second = next_acquire_after(first, first + 150_000, period);
        assert_eq!(second, first + period);
        // A whole period late: no debt.
        let third = next_acquire_after(second, second + period + 5_000_000, period);
        assert_eq!(third, second + period + 5_000_000 + period);
    }

    #[test]
    fn the_acquire_schedule_spaces_frame_starts() {
        let period = period_ns(40);
        assert_eq!(period, 25_000_000);
        // The first acquire schedules the next one a period later.
        let first = next_acquire_after(0, 1_000_000_000, period);
        assert_eq!(first, 1_025_000_000);
        // A guest faster than the target: every start is exactly one period on.
        assert_eq!(next_acquire_after(first, 1_001_000_000, period), 1_050_000_000);
        assert_eq!(next_acquire_after(1_050_000_000, 1_002_000_000, period), 1_075_000_000);
        // A guest slower than the target keeps its own rate: the gate is a lower
        // bound, not a delay.
        assert_eq!(next_acquire_after(1_075_000_000, 1_200_000_000, period), 1_225_000_000);
        // No cap: the schedule is left alone, so enabling one later does not
        // inherit a stale deadline.
        assert_eq!(next_acquire_after(1_225_000_000, 5, 0), 1_225_000_000);
        // A period is truncated, never rounded up past `1 / target`.
        assert_eq!(period_ns(FPS_UNCAPPED), 0);
        assert_eq!(period_ns(3), 333_333_333);
    }

    /// The release tests share the process-wide tables and counters.
    static RELEASE_LOCK: Mutex<()> = Mutex::new(());

    /// A pipe standing in for a sync file: the read end polls readable once a
    /// byte is written, the way a fence polls once it signals.
    fn fence_pair() -> (RawFd, RawFd) {
        let mut p = [0 as RawFd; 2];
        // SAFETY: two out-params in a live array.
        assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
        (p[0], p[1])
    }

    fn signal(write_end: RawFd) {
        // SAFETY: the write end of a live pipe.
        assert_eq!(
            unsafe { libc::write(write_end, b"x".as_ptr().cast(), 1) },
            1
        );
    }

    fn is_open(fd: RawFd) -> bool {
        // SAFETY: probe only.
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    fn pending(ino: u64) -> ReleaseFence {
        ReleaseFence {
            ino,
            flags: ReleaseFence::PENDING,
        }
    }

    fn fence(ino: u64) -> ReleaseFence {
        ReleaseFence { ino, flags: 0 }
    }

    #[test]
    fn a_pushed_release_fence_is_waited_once_at_acquire() {
        let _g = RELEASE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ino = 0x5eed_0001;
        note_owned(ino);
        let (rd, wr) = fence_pair();
        record_release(pending(ino), -1);
        record_release(fence(ino), rd);
        signal(wr);
        let waits = RELEASE_WAITS.load(Ordering::Relaxed);
        let timeouts = RELEASE_TIMEOUTS.load(Ordering::Relaxed);
        wait_release(ino, RELEASE_WAIT);
        assert_eq!(RELEASE_WAITS.load(Ordering::Relaxed), waits + 1);
        assert!(!is_open(rd), "a waited fence must be closed");
        // The next acquire of the buffer has nothing left to wait for.
        wait_release(ino, RELEASE_WAIT);
        assert_eq!(RELEASE_WAITS.load(Ordering::Relaxed), waits + 1);
        assert_eq!(RELEASE_TIMEOUTS.load(Ordering::Relaxed), timeouts);
        // SAFETY: the test owns the write end.
        unsafe { libc::close(wr) };
    }

    #[test]
    fn an_acquire_waits_for_a_fence_still_on_its_way() {
        let _g = RELEASE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ino = 0x5eed_0002;
        note_owned(ino);
        record_release(pending(ino), -1);
        let (rd, wr) = fence_pair();
        let late = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            signal(wr);
            record_release(fence(ino), rd);
            wr
        });
        let timeouts = RELEASE_TIMEOUTS.load(Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        wait_release(ino, std::time::Duration::from_millis(500));
        let took = t0.elapsed();
        let wr = late.join().unwrap();
        assert!(
            took >= std::time::Duration::from_millis(9),
            "returned before the fence: {took:?}"
        );
        assert!(
            took < std::time::Duration::from_millis(400),
            "waited past the fence: {took:?}"
        );
        assert_eq!(RELEASE_TIMEOUTS.load(Ordering::Relaxed), timeouts);
        assert!(!is_open(rd));
        // SAFETY: the test owns the write end.
        unsafe { libc::close(wr) };
    }

    #[test]
    fn an_unsignalled_fence_is_waited_for_and_bounded() {
        let _g = RELEASE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ino = 0x5eed_0003;
        note_owned(ino);
        let (rd, wr) = fence_pair();
        record_release(pending(ino), -1);
        record_release(fence(ino), rd);
        let timeouts = RELEASE_TIMEOUTS.load(Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        wait_release(ino, std::time::Duration::from_millis(20));
        assert!(t0.elapsed() >= std::time::Duration::from_millis(19));
        assert_eq!(RELEASE_TIMEOUTS.load(Ordering::Relaxed), timeouts + 1);
        assert!(!is_open(rd));
        // SAFETY: the test owns the write end.
        unsafe { libc::close(wr) };
    }

    #[test]
    fn a_lost_fence_costs_one_bounded_wait_and_a_late_one_is_dropped() {
        let _g = RELEASE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ino = 0x5eed_0004;
        note_owned(ino);
        record_release(pending(ino), -1);
        let timeouts = RELEASE_TIMEOUTS.load(Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        wait_release(ino, std::time::Duration::from_millis(20));
        assert!(t0.elapsed() >= std::time::Duration::from_millis(19));
        assert_eq!(RELEASE_TIMEOUTS.load(Ordering::Relaxed), timeouts + 1);
        // The fence arrives after the buffer went to the guest: dropped.
        let (rd, wr) = fence_pair();
        record_release(fence(ino), rd);
        assert!(
            !is_open(rd),
            "a fence for a re-acquired buffer must be closed"
        );
        let waits = RELEASE_WAITS.load(Ordering::Relaxed);
        let t1 = std::time::Instant::now();
        wait_release(ino, RELEASE_WAIT);
        assert!(t1.elapsed() < std::time::Duration::from_millis(15));
        assert_eq!(RELEASE_WAITS.load(Ordering::Relaxed), waits);
        // SAFETY: the test owns the write end.
        unsafe { libc::close(wr) };
    }

    #[test]
    fn a_release_with_no_fence_frees_the_buffer_and_others_buffers_are_ignored() {
        let _g = RELEASE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ino = 0x5eed_0005;
        note_owned(ino);
        record_release(pending(ino), -1);
        record_release(fence(ino), -1);
        let t0 = std::time::Instant::now();
        wait_release(ino, RELEASE_WAIT);
        assert!(t0.elapsed() < std::time::Duration::from_millis(15));
        // A buffer this process was never handed: nothing is kept for it.
        let stranger = 0x5eed_0f00;
        let (rd, wr) = fence_pair();
        record_release(pending(stranger), -1);
        record_release(fence(stranger), rd);
        assert!(!is_open(rd));
        let t1 = std::time::Instant::now();
        wait_release(stranger, RELEASE_WAIT);
        assert!(t1.elapsed() < std::time::Duration::from_millis(15));
        // SAFETY: the test owns the write end.
        unsafe { libc::close(wr) };
    }

    #[test]
    fn a_pushed_fence_is_received_with_its_descriptor() {
        let mut sv = [0 as RawFd; 2];
        // SAFETY: two out-params in a live array.
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    sv.as_mut_ptr(),
                )
            },
            0
        );
        let (rd, wr) = fence_pair();
        send_with_fd(sv[0], &fence(42).encode(), rd);
        let (buf, len, got) = recv_push(sv[1]).expect("a pushed frame");
        assert_eq!(ReleaseFence::decode(&buf[..len]), Some(fence(42)));
        assert!(got >= 0 && got != rd);
        assert!(recv_push(sv[1]).is_none(), "nothing else was queued");
        // SAFETY: the test owns every descriptor it opened or received.
        unsafe {
            [sv[0], sv[1], rd, wr, got].iter().for_each(|fd| {
                libc::close(*fd);
            })
        };
    }
}
