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
    scanout_ahb_format, FrameCapNote, FrameCapSubscribe, LsfgConfig, ScanoutAllocReq,
    BufferReleased, ScanoutAllocResp, ScanoutLayout, VsyncNote, CAP_FRAME_CAP, CAP_PRESENT_ID,
    CAP_RELEASE_NOTE, CAP_VSYNC_NOTE,
    FPS_UNCAPPED, SCANOUT_LAYOUT_ENV, SCANOUT_REQ_AHB_HANDLE,
};
use std::ffi::c_void;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
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

/// Adopt one frame-cap note: the target it carries.
fn apply_note(note: FrameCapNote) {
    NOTES_N.fetch_add(1, Ordering::Relaxed);
    CAP_FPS.store(note.fps, Ordering::Relaxed);
    // 0 while frame generation paces the session: log what is actually in force
    // now that the note is adopted, not what it carried.
    crate::logf!("scanout frame cap: fps={} effective={}",
        note.fps,
        target_fps());
}

/// The cap the daemon's note carries, [`FPS_UNCAPPED`] when none: what the
/// session's limits are computed from, and what the frame-cap line reports.
pub(crate) fn target_fps() -> u32 {
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
                caps: CAP_FRAME_CAP | CAP_VSYNC_NOTE | CAP_PRESENT_ID | CAP_RELEASE_NOTE,
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
                            crate::logf!("scan-out: AHardwareBuffer handle not received ({got})");
                            libc::close(recv_fd);
                            return Err(Failure::Dead);
                        }
                    }
                    note_pool_fd_kind(recv_fd, r.ino);
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
    crate::logf!("scan-out: pool fd ino={ino} size={size} fs={magic:#x} dma-buf={} link={}",
        magic == 0x444d_4142,
        fd_link(fd));
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
/// presents it: the daemon answers when that transaction latches.
///
/// Sent without blocking. This runs on the present worker for every frame, so a
/// send that waited for the daemon to make room would put the daemon's read rate
/// into the frame time — measured as a flat 277 ms on a title running at 60 fps
/// on the route that sends no marks at all. A mark that does not fit is dropped:
/// its frame's wait is then answered by the compositor's feedback or by the
/// two-period grace, both of which are bounded, so the cost of a lost mark is a
/// latch report in the census and nothing the guest can feel.
pub(crate) fn send_present_mark(ino: u64, id: u64) {
    let ch = CHAN.lock().unwrap_or_else(|e| e.into_inner());
    if *ch < 0 {
        return;
    }
    send_frame_nb(*ch, &gndrm_uapi::wire::PresentMark { ino, id }.encode());
}

// ------------------------------------------------------ scan-out releases
//
// A compositor handing a buffer back means it has finished *compositing* with
// it. The display may still be reading it: SurfaceFlinger reports a buffer's
// true release a transaction later, as the previous-release fence of whichever
// buffer replaced it on the layer, and the daemon turns that fence into a
// [`BufferReleased`] note. Until the note arrives a slot stays busy, because a
// producer that reused the buffer on the compositor's release wrote into memory
// the panel was still scanning — and the panel showed both frames at once for
// as long as the two writes overlapped.

/// Scan-out inodes the daemon reported released, as `(ino, CLOCK_MONOTONIC ns
/// when this process took the note)`. Bounded: a note for a buffer whose slot is
/// gone is dropped, and a producer cannot grow this list.
static RELEASED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// How many releases may be held before the oldest is dropped. A session has one
/// per buffer in the pool; a note whose slot never comes back is stale.
const RELEASED_CAP: usize = 32;

/// Release notes seen this session. Zero means the daemon does not send them (an
/// older build), and a slot is freed on the compositor's release as it was
/// before: the note improves on that behaviour, it does not depend on it.
static RELEASE_NOTES: AtomicU64 = AtomicU64::new(0);

/// Record that the daemon reported `ino` released.
///
/// A note is not applied here: the slot that owns the buffer is parked, and it is
/// taken on the next acquire pass (see `acquire_take_free`), so this runs on the
/// thread that read the note and touches nothing else.
pub(crate) fn note_buffer_released(ino: u64) {
    if ino == 0 {
        return;
    }
    RELEASE_NOTES.fetch_add(1, Ordering::Relaxed);
    let mut q = RELEASED.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= RELEASED_CAP {
        q.remove(0);
    }
    q.push((ino, crate::proxy::monotonic_ns()));
}

/// The inos of the notes currently held, up to eight, oldest first.
///
/// For the one case worth a log line: a park that expired while notes were being
/// received. Printed beside the slot's own `ino`, it says whether the daemon keys
/// buffers by an identity this process never holds (a different allocator path on
/// this SoC) or whether the note list was simply empty by then, which is a timing
/// question instead.
pub(crate) fn released_snapshot() -> String {
    let q = RELEASED.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = String::new();
    for (ino, _) in q.iter().take(8) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("{ino}"));
    }
    out
}

/// Whether the daemon sends release notes at all. Until one has arrived, a slot
/// frees on the compositor's release exactly as it did before this note existed.
/// Whether any note has ever arrived.
///
/// The shim no longer decides a park from this: one note followed by silence is
/// a channel that does not work, and `proxy::park_awaits_note` counts the parks
/// since the last note instead. Kept as the test-side predicate.
#[cfg(test)]
pub(crate) fn release_notes_live() -> bool {
    RELEASE_NOTES.load(Ordering::Relaxed) != 0
}

/// Release notes received this session, for the census: the denominator of
/// `notes=` (parks a note resolved), which says whether a park that timed out
/// lost the note or was never sent one.
pub(crate) fn release_notes_received() -> u64 {
    RELEASE_NOTES.load(Ordering::Relaxed)
}

/// Take a release note for `ino` that arrived at or after `since_ns`, and drop
/// every older note for the same buffer.
///
/// `since_ns` is when the slot was parked. A note older than that describes an
/// earlier frame of the same buffer — it says nothing about the frame now on the
/// panel — and must not free the slot early. `true` when a note applied.
pub(crate) fn take_released(ino: u64, since_ns: u64) -> bool {
    if ino == 0 {
        return false;
    }
    let mut q = RELEASED.lock().unwrap_or_else(|e| e.into_inner());
    let mut applied = false;
    q.retain(|&(held, at)| {
        if held != ino {
            return true;
        }
        if at < since_ns {
            return false;
        }
        if applied {
            return true;
        }
        applied = true;
        false
    });
    applied
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
    // One frame of the largest push. SEQPACKET preserves boundaries but truncates
    // on a short buffer, so this must cover every push: `FrameCapNote`,
    // `VsyncNote` and `LsfgConfig`.
    let mut buf = [0u8; PUSH_LEN];
    // SAFETY: `*ch` is the connection this module owns and `buf` is one full
    // frame; MSG_DONTWAIT leaves a frame that has not arrived for the next
    // call, and a read of any other length is a peer that closed.
    let n = unsafe { libc::recv(*ch, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
    let Ok(len) = usize::try_from(n) else {
        return;
    };
    if len == 0 {
        return;
    }
    // Dispatch on the magic: every push type shares the socket, in any order.
    if let Some(note) = FrameCapNote::decode(&buf[..len]) {
        apply_note(note);
    } else if let Some(note) = VsyncNote::decode(&buf[..len]) {
        apply_vsync(note);
    } else if let Some(config) = LsfgConfig::decode(&buf[..len]) {
        if !config.is_request() {
            crate::lsfg::apply(config);
        }
    } else if let Some(done) = gndrm_uapi::wire::PresentDone::decode(&buf[..len]) {
        // The panel latched the frame this id was marked for.
        crate::present::record(done.id, done.latch_ns);
    } else if let Some(released) = BufferReleased::decode(&buf[..len]) {
        // SurfaceFlinger is finished with this scan-out buffer: the producer may
        // write into it again (see `note_buffer_released`).
        note_buffer_released(released.ino);
    } else if let Some(acq) = gndrm_uapi::wire::FrameAcquired::decode(&buf[..len]) {
        // Sent from the acquire path; nothing to apply here. Accepted so the
        // log does not report it as an unknown frame.
        let _ = acq;
    }
}

/// Largest frame the daemon pushes unsolicited.
const PUSH_LEN: usize = LsfgConfig::LEN;

#[cfg(test)]
mod tests {
    use super::*;

    /// A release note frees the frame it describes and no other: the slot is
    /// parked before one can arrive, so a note older than the park describes an
    /// earlier frame of the same buffer and must not free this one.
    #[test]
    fn a_release_note_only_applies_to_the_frame_it_describes() {
        note_buffer_released(9_001);
        assert!(take_released(9_001, 0), "a note after the park frees the slot");
        assert!(!take_released(9_001, 0), "and is consumed by it");

        // A note taken before the slot was parked: dropped, and it must not free
        // the slot on the next pass either.
        note_buffer_released(9_002);
        let parked = crate::proxy::monotonic_ns() + 1;
        assert!(!take_released(9_002, parked));
        assert!(!take_released(9_002, 0));

        // An inode of 0 is never a registered buffer.
        note_buffer_released(0);
        assert!(!take_released(0, 0));
    }

    /// Its own note, not one an earlier test happened to leave: the suite runs
    /// these in parallel, so a test that reads a static another test writes is a
    /// test that passes by scheduler luck.
    #[test]
    fn release_notes_are_bounded_and_observed() {
        note_buffer_released(6_999);
        assert!(release_notes_live(), "the note above is recorded");
        for ino in 0..(RELEASED_CAP as u64 + 8) {
            note_buffer_released(7_000 + ino);
        }
        let held = RELEASED.lock().unwrap_or_else(|e| e.into_inner()).len();
        assert!(held <= RELEASED_CAP, "held {held} of at most {RELEASED_CAP}");
    }


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
            let sub = FrameCapSubscribe::decode(&buf).expect("subscribe frame");
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
        let name = format!("{}", format_args!("gndrm-icdsc-{}", std::process::id()));
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
                let req = ScanoutAllocReq::decode(&buf).expect("request frame");
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
            .expect("exchange");
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
        server.join().expect("server thread");
    }

    /// The frame-cap note stands ahead of the hand-out reply, where the shim
    /// would otherwise expect the reply. The reader finds each frame by its own
    /// magic, so the target is adopted and the reply is still the reply.
    #[test]
    fn exchange_sorts_the_frame_cap_note_from_the_reply() {
        let name = format!("{}", format_args!("gndrm-icdcap-{}", std::process::id()));
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
            .expect("exchange");
        let ino = server.join().expect("server thread");
        assert_eq!(got.ino, ino);
        // The note ahead of the reply set the target.
        assert_eq!(target_fps(), 120);
        // SAFETY: the received descriptor and the connection are live.
        unsafe {
            libc::close(got.fd);
            libc::close(fd);
        }
    }

    /// The cap's period is truncated, never rounded up past `1 / target`, and no
    /// cap has none. The schedule built on it lives in `crate::vsync_gate`.
    #[test]
    fn a_caps_period_is_truncated_and_no_cap_has_none() {
        assert_eq!(period_ns(40), 25_000_000);
        assert_eq!(period_ns(FPS_UNCAPPED), 0);
        assert_eq!(period_ns(3), 333_333_333);
    }
}
