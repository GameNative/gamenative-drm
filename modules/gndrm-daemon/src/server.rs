//! Blocking SEQPACKET socket server shared by `gndrm-hostd` (host tests,
//! B9 harness) and `gndrm-android` (in-app daemon).
//!
//! Connection contract: the first accepted connection is the RPC channel,
//! the second is the event channel (matches `gndrm-shim`'s connect order).
//! One client at a time.
//!
//! Address convention (must match `gndrm-shim`'s `connect_one`):
//! - `"/abs/path"` → filesystem socket (unlinked first)
//! - `"@name"` or a bare `"name"` → Linux abstract namespace
//!
//! Both sides always pass a full-`sizeof(sockaddr_un)` addrlen, so abstract
//! names compare identically padded.

use std::io;
use std::os::fd::RawFd;

use gndrm_uapi::wire::{ReqHdr, RespHdr, MAX_FRAME_LEN, REQ_HDR_LEN, RESP_HDR_LEN};

use crate::pod::pod_write;
use crate::{Device, DispatchResult};

/// Log sink: `eprintln` adapter on host, `__android_log_write` on Android.
/// Cold paths only — the per-frame hot path never logs.
pub type Log<'a> = &'a dyn Fn(&str);

/// Resolve `name` into a bound + listening `SOCK_SEQPACKET` fd.
///
/// std's `UnixListener` is always `SOCK_STREAM`; the wire protocol needs
/// `SOCK_SEQPACKET` (one frame per message), so bind/listen go through libc.
pub fn listen_seqpacket(name: &str) -> io::Result<RawFd> {
    let mut sa: libc::sockaddr_un = unsafe { core::mem::zeroed() };
    sa.sun_family = libc::AF_UNIX as u16;

    let bytes = name.strip_prefix('@').unwrap_or(name).as_bytes();
    let abstract_sock = !name.contains('/');
    if bytes.len() >= sa.sun_path.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "name too long"));
    }
    // SAFETY: sa is zeroed; sun_path bounds checked above. Abstract names
    // sit after a leading NUL (already zero).
    unsafe {
        let dst = if abstract_sock {
            sa.sun_path.as_mut_ptr().add(1)
        } else {
            sa.sun_path.as_mut_ptr()
        };
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.cast(), bytes.len());
    }
    bind_listen(&mut sa)
}

fn bind_listen(sa: &mut libc::sockaddr_un) -> io::Result<RawFd> {
    // SAFETY: standard socket/bind/listen sequence on a fresh sockaddr_un;
    // full-size addrlen is the wire convention (see module docs).
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::bind(
            fd,
            (&raw const *sa).cast(),
            core::mem::size_of::<libc::sockaddr_un>() as u32,
        ) < 0
        {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        if libc::listen(fd, 4) < 0 {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        Ok(fd)
    }
}

/// Accept one connection (`SOCK_CLOEXEC`).
pub fn accept(listener: RawFd) -> io::Result<RawFd> {
    // SAFETY: accept on a listening socket.
    let fd = unsafe {
        libc::accept4(
            listener,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Blocking dispatch loop: one frame in → one frame out, events pushed on
/// the event channel. Returns when the peer closes the RPC socket, the RPC
/// fd is shut down externally (`nativeStop`), or a send fails.
pub fn serve(dev: &mut Device, rpc_fd: RawFd, event_fd: RawFd, log: Log) {
    let mut buf = vec![0u8; MAX_FRAME_LEN];
    loop {
        let (n, fd_in) = match recv_frame(rpc_fd, &mut buf) {
            Ok(v) => v,
            Err(e) => {
                log(&format!("recv: {e}"));
                return;
            }
        };
        if n == 0 {
            log("peer closed");
            return;
        }
        let Ok(hdr) = ReqHdr::decode(&buf[..n]) else {
            log("bad request header, dropping");
            return;
        };
        let payload = &buf[REQ_HDR_LEN..hdr.total_len as usize];
        let result: DispatchResult = dev.dispatch(hdr.ioctl_nr, payload, fd_in);

        let resp = RespHdr {
            total_len: (RESP_HDR_LEN + result.body.len()) as u32,
            seq: hdr.seq,
            ret: result.ret,
            out_len: result.body.len() as u32,
        };
        let mut frame = resp.encode().to_vec();
        frame.extend_from_slice(&result.body);
        if send_frame(rpc_fd, &frame, result.fd_out).is_err() {
            log("send failed");
            return;
        }

        for ev in result.events {
            let mut bytes = Vec::new();
            pod_write(&mut bytes, &ev);
            if send_frame(event_fd, &bytes, None).is_err() {
                log("event send failed");
                return;
            }
        }
    }
}

/// `recvmsg(2)` one SEQPACKET frame (+ optional SCM_RIGHTS fd).
fn recv_frame(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, Option<RawFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cbuf = [0u8; 64];
    // SAFETY: valid fd, valid iovec pointing at `buf`, control buffer is
    // large enough for one SCM_RIGHTS fd and zeroed.
    let (n, msg) = unsafe {
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = cbuf.len();
        let n = libc::recvmsg(fd, &mut msg, 0);
        (n, msg)
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Ok((0, None));
    }
    // SAFETY: msg was initialized by the successful recvmsg above.
    let mut out_fd = None;
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg.is_null() {
        // SAFETY: cmsg comes from the kernel-parsed control buffer.
        unsafe {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let mut fd_out: RawFd = -1;
                core::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(cmsg).cast::<RawFd>(),
                    &mut fd_out,
                    1,
                );
                out_fd = Some(fd_out);
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((n as usize, out_fd))
}

/// `sendmsg(2)` one frame (+ optional fd).
fn send_frame(fd: RawFd, bytes: &[u8], fd_send: Option<RawFd>) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut cbuf = [0u8; 64];
    // SAFETY: valid fd, valid iovec; control buffer only touched when an fd
    // is attached, and CMSG_SPACE-sized for exactly one fd.
    let n = unsafe {
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if let Some(f) = fd_send {
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(4) as usize;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(4) as usize;
            core::ptr::write(libc::CMSG_DATA(cmsg).cast::<RawFd>(), f);
        }
        libc::sendmsg(fd, &msg, 0)
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
