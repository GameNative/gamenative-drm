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

use gndrm_uapi::wire::{ScanoutAllocReq, ScanoutAllocResp, SCANOUT_FORMAT_BGRA_8888};
use std::os::fd::RawFd;
use std::sync::Mutex;

/// Default budget for one exchange, matching the LD_PRELOAD shim's: a daemon
/// that has stopped answering must not stall the guest's swapchain creation.
const TIMEOUT_US: i64 = 250_000;

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
        fd
    }
}

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

        // Take any descriptor before validating anything: whatever arrived has
        // to be closed here if the frame turns out not to be a usable reply.
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

        let resp = if n as usize == ScanoutAllocResp::LEN {
            ScanoutAllocResp::decode(&buf)
        } else {
            None
        };
        match resp {
            Some(r) if r.ret == 0 && recv_fd >= 0 => Ok(ScanoutBuf {
                fd: recv_fd,
                ino: r.ino,
                stride: r.stride,
            }),
            Some(_) => {
                // A refusal carries no buffer; the channel stays usable.
                if recv_fd >= 0 {
                    libc::close(recv_fd);
                }
                Err(Failure::Refused)
            }
            None => {
                // Closed peer, timeout, or an unframeable reply.
                if recv_fd >= 0 {
                    libc::close(recv_fd);
                }
                Err(Failure::Dead)
            }
        }
    }
}

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
}
