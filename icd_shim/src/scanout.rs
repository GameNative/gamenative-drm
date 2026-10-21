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
    FrameCapNote, FrameCapSubscribe, GameWindowNote, LsfgConfig, ScanoutAllocReq, ScanoutAllocResp,
    CAP_FRAME_CAP, FPS_UNCAPPED, SCANOUT_FORMAT_BGRA_8888,
};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
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
// (see `gndrm_uapi::wire::FrameCapNote`). The floor the target implies is
// derived here, because a release is what it is measured against and this shim
// is the only side that sees one — and it sees every one. A per-buffer deadline
// sent with a hand-out cannot pace a title: a hand-out happens once per pool
// buffer per session, while a release happens every frame.

/// The frame cap the daemon has declared on this connection, and this shim's
/// own release schedule.
struct FrameCapState {
    /// The target in fps, [`FPS_UNCAPPED`] when the limiter is off.
    target_fps: u32,
    /// The instant the previous release freed its slot, in `CLOCK_MONOTONIC`
    /// ns, or 0 before a release has been seen.
    last_release_ns: u64,
}

static CAP: Mutex<FrameCapState> = Mutex::new(FrameCapState {
    target_fps: FPS_UNCAPPED,
    last_release_ns: 0,
});

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

/// The instant a release seen at `now_ns` may free its slot.
///
/// The floor is one target period after the previous release's own free
/// instant, so it applies the same rule to every release rather than to the
/// first release of each buffer. `now_ns` comes back unchanged whenever it is at
/// or after that floor — and always when no cap is in force or no release has
/// been seen — so a release the guest was already slow enough to produce is
/// never delayed. The delivered interval is therefore `max(guest interval,
/// period)`: exact at the target when the guest is faster, the guest's own rate
/// when it is slower, and never below it.
#[must_use]
fn release_floor(target_fps: u32, last_release_ns: u64, now_ns: u64) -> u64 {
    let period = period_ns(target_fps);
    if period == 0 || last_release_ns == 0 {
        return now_ns;
    }
    last_release_ns.saturating_add(period).max(now_ns)
}

/// Record a release seen at `now_ns` (`CLOCK_MONOTONIC` ns) and return the
/// instant from which its slot may be freed.
///
/// The recorded instant is that free instant, not the release's arrival: a held
/// slot frees when its floor passes, so the schedule and what the guest
/// observes are the same sequence. That is what makes consecutive frees at
/// least one period apart, which is the cap.
#[must_use]
pub(crate) fn note_release(now_ns: u64, guest_slot: bool) -> u64 {
    // Only a guest slot's release is paced. A generated frame is the thing the
    // floor exists to make room for: holding its release to the source period
    // throttles the output back to the source rate, which is exactly the
    // behaviour that makes frame generation look like it does nothing.
    if !guest_slot {
        return now_ns;
    }
    let mut c = CAP.lock().unwrap_or_else(|e| e.into_inner());
    // The effective rate, not the note's: while frame generation runs the source
    // cap comes from the app through `lsfg::live()`, and the daemon's own note is
    // 0 (the app disarms gamescope's limiter in that case).
    let target = crate::lsfg::source_cap().unwrap_or(c.target_fps);
    let free_at = release_floor(target, c.last_release_ns, now_ns);
    c.last_release_ns = free_at;
    free_at
}

/// Adopt one frame-cap note: the target it carries.
fn apply_note(note: FrameCapNote) {
    // 0 while frame generation paces the session: log what is actually in force,
    // not what the note carried.
    crate::logf!(gndrm_obf::obf!(
        "scanout frame cap: fps={} effective={}",
        note.fps,
        target_fps()
    ));
    NOTES_N.fetch_add(1, Ordering::Relaxed);
    let mut c = CAP.lock().unwrap_or_else(|e| e.into_inner());
    c.target_fps = note.fps;
}

/// The target the daemon last declared, [`FPS_UNCAPPED`] when none has: read by
/// the shim's census, so a round can tell "no note arrived" from "a note
/// arrived whose floor never bound".
pub(crate) fn target_fps() -> u32 {
    // Frame generation owns the source rate while it is on: the app disarms
    // gamescope's limiter in that case, so a zero from the daemon's cap note is
    // not "uncapped", it is "paced by the presenter".
    if let Some(cap) = crate::lsfg::source_cap() {
        return cap;
    }
    CAP.lock().unwrap_or_else(|e| e.into_inner()).target_fps
}

/// How many frame-cap notes have been adopted on this connection, for the same
/// census.
pub(crate) fn notes_seen() -> usize {
    NOTES_N.load(Ordering::Relaxed)
}

/// Tell the daemon the game's own window has appeared, so it can dismiss the
/// booting splash on that event instead of on the desktop's first frame.
///
/// Fire-and-forget on the scan-out channel (the connection this module already
/// holds): unlike the allocator exchange there is no reply, and a failure costs
/// nothing — the daemon's compile-time grace clears the splash anyway. Sent
/// once per session (the caller latches); idempotent on the daemon.
///
/// A few attempts, reconnecting between them, exactly like [`allocate`]: the
/// daemon serves a **single** scan-out reader and closes whatever connection a
/// newer one supersedes, and the other client on this same listener (the
/// LD_PRELOAD shim in gamescope's process) reconnects on its own cadence — so
/// the descriptor this module cached is frequently already dead when the note
/// is sent. A single `sendmsg` on it fails with `EPIPE`, and because the caller
/// latches `GAME_WINDOW_REPORTED` before calling this, the note would be lost
/// for the session. Reconnecting makes this connection the newest, which is
/// what gets the note read.
pub fn report_game_window_present() {
    let Ok(path) = std::env::var("GAMENATIVE_DRM_SOCKET") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let note = GameWindowNote {
        present: GameWindowNote::PRESENT,
    };
    let mut ch = CHAN.lock().unwrap_or_else(|e| e.into_inner());
    for _ in 0..3 {
        if *ch < 0 {
            *ch = connect(&path);
            if *ch < 0 {
                return;
            }
        }
        if send_note(*ch, note) {
            return;
        }
        // SAFETY: `*ch` is a descriptor this module owns.
        unsafe { libc::close(*ch) };
        *ch = -1;
    }
}

/// Send one notification frame; `false` when the channel did not accept it. No
/// reply is read: the daemon does not answer this message, so nothing is
/// expected back on the connection.
fn send_note(fd: RawFd, note: GameWindowNote) -> bool {
    send_frame(fd, &note.encode())
}

/// Send one frame that expects no reply; `false` when the channel did not
/// accept it. Shared by the two fire-and-forget frames on this channel: the
/// game-window notification and the frame-cap subscribe.
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

/// Ask the daemon named by `GAMENATIVE_DRM_SOCKET` for a `width`x`height`
/// buffer in the pool's BGRA layout. `None` when the channel is unset,
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
        // to.
        for _ in 0..4 {
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
            }
        }
    }
    None
}

pub fn allocate(width: u32, height: u32) -> Option<ScanoutBuf> {
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
        match exchange(*ch, width, height) {
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
                caps: CAP_FRAME_CAP,
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
/// reply, plus a frame-cap note the daemon may put ahead of it (the answer to
/// the subscribe, or a target the user changed while a request was in flight).
/// Beyond this the peer is not speaking this protocol.
const MAX_BURST_FRAMES: usize = 4;

/// One request/response exchange: send a geometry request, receive the reply
/// and its descriptor.
fn exchange(fd: RawFd, width: u32, height: u32) -> Result<ScanoutBuf, Failure> {
    let req = ScanoutAllocReq {
        width,
        height,
        format: SCANOUT_FORMAT_BGRA_8888,
        len: u64::from(width) * u64::from(height) * 4,
    };
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
            let mut buf = [0u8; ScanoutAllocResp::LEN];
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

            if n as usize == ScanoutAllocResp::LEN {
                if let Some(config) = LsfgConfig::decode(&buf) {
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
                } else if let Some(note) = FrameCapNote::decode(&buf) {
                    // A note carries no buffer.
                    if recv_fd >= 0 {
                        libc::close(recv_fd);
                    }
                    apply_note(note);
                    if frames < MAX_BURST_FRAMES {
                        continue;
                    }
                } else if let Some(r) = ScanoutAllocResp::decode(&buf) {
                    if r.ret != 0 || recv_fd < 0 {
                        // A refusal carries no buffer; the channel stays
                        // usable.
                        if recv_fd >= 0 {
                            libc::close(recv_fd);
                        }
                        return Err(Failure::Refused);
                    }
                    return Ok(ScanoutBuf {
                        fd: recv_fd,
                        ino: r.ino,
                        stride: r.stride,
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
    // on a short buffer, so this must cover both `FrameCapNote` and `LsfgConfig`.
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
    // Dispatch on the magic: both push types are the same socket, in either order.
    if let Some(note) = FrameCapNote::decode(&buf[..len]) {
        apply_note(note);
    } else if let Some(config) = LsfgConfig::decode(&buf[..len]) {
        if !config.is_request() {
            crate::lsfg::apply(config);
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
            assert_eq!(sub.caps, CAP_FRAME_CAP);
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
                assert_eq!(req.format, SCANOUT_FORMAT_BGRA_8888);
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

    /// The floor is one period after the previous release's own free instant,
    /// so it is a lower bound that holds only a release that would have come
    /// sooner than `1 / target`.
    #[test]
    fn the_release_floor_is_one_period_after_the_previous_release() {
        let period = period_ns(40);
        assert_eq!(period, 25_000_000);
        // The first release has nothing to measure from.
        assert_eq!(release_floor(40, 0, 1_000), 1_000);
        // A second release inside the period is held to it.
        assert_eq!(
            release_floor(40, 1_000_000_000, 1_004_000_000),
            1_025_000_000
        );
        // One at or after the floor is not delayed at all: a guest already at
        // or below the target keeps its own rate.
        assert_eq!(
            release_floor(40, 1_000_000_000, 1_025_000_000),
            1_025_000_000
        );
        assert_eq!(
            release_floor(40, 1_000_000_000, 1_100_000_000),
            1_100_000_000
        );
        // No cap: no floor, whenever the release arrives.
        assert_eq!(release_floor(FPS_UNCAPPED, 1_000_000_000, 1_000), 1_000);
        assert_eq!(period_ns(FPS_UNCAPPED), 0);
        // A period is truncated, never rounded up past `1 / target`.
        assert_eq!(period_ns(3), 333_333_333);
    }

    /// The recorded instant is the free instant, so a run of releases inside
    /// the period frees one per period rather than in a burst, and never later
    /// than the guest's own next release would have been.
    #[test]
    fn consecutive_releases_frees_are_spaced_by_the_period() {
        let period = period_ns(40);
        let mut last = 0u64;
        let mut frees = Vec::new();
        for i in 0..5u64 {
            // The guest presents every 4 ms, far faster than the target.
            let now = 1_000_000_000 + i * 4_000_000;
            let free_at = release_floor(40, last, now);
            last = free_at;
            frees.push(free_at);
        }
        assert_eq!(
            frees,
            vec![
                1_000_000_000,
                1_025_000_000,
                1_050_000_000,
                1_075_000_000,
                1_100_000_000,
            ]
        );
        for pair in frees.windows(2) {
            assert_eq!(pair[1] - pair[0], period);
        }
    }
}
