//! `gndrm-hostd` — host-side test daemon (B9 harness, also used by phase-3
//! bring-up scripts): serves one `Device` over a Unix socket, exactly like
//! the in-app `gndrm-android` loop will, minus anything Android.
//!
//! Usage: `gndrm-hostd <socket-path>`
//!
//! Connection contract: the first accepted connection is the RPC channel,
//! the second is the event channel (matches `gndrm-shim`'s connect order).

#![deny(clippy::all)]

use std::os::fd::RawFd;

use gndrm_daemon::{Device, DispatchResult, NullPresenter, PanelDesc};
use gndrm_uapi::wire::{ReqHdr, RespHdr, MAX_FRAME_LEN, REQ_HDR_LEN, RESP_HDR_LEN};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: gndrm-hostd <socket-path>");
    let _ = std::fs::remove_file(&path);
    let listener = listen_seqpacket(&path).unwrap();
    eprintln!("[gndrm-hostd] listening on {path} (SOCK_SEQPACKET)");

    // First connection = RPC, second = events (wire contract).
    let rpc = accept(listener).unwrap();
    let events = accept(listener).unwrap();
    eprintln!("[gndrm-hostd] client connected (rpc + events)");

    let mut dev = Device::new(PanelDesc::default(), Box::<NullPresenter>::default());
    serve(&mut dev, rpc, events);
}

/// std's `UnixListener` is always SOCK_STREAM; the wire protocol needs
/// SOCK_SEQPACKET (one frame per message), so bind/listen go through libc.
fn listen_seqpacket(path: &str) -> std::io::Result<RawFd> {
    let mut sa: libc::sockaddr_un = unsafe { core::mem::zeroed() };
    sa.sun_family = libc::AF_UNIX as u16;
    let bytes = path.as_bytes();
    if bytes.len() >= sa.sun_path.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path too long",
        ));
    }
    // SAFETY: sa is zeroed; sun_path bounds checked.
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            sa.sun_path.as_mut_ptr().cast(),
            bytes.len(),
        );
    }
    // SAFETY: standard socket/bind/listen sequence on a fresh sockaddr_un.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::bind(
            fd,
            (&raw const sa).cast(),
            core::mem::size_of::<libc::sockaddr_un>() as u32,
        ) < 0
        {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        if libc::listen(fd, 4) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        Ok(fd)
    }
}

fn accept(listener: RawFd) -> std::io::Result<RawFd> {
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
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

/// Blocking dispatch loop: one frame in → one frame out, events pushed on
/// the event channel. Returns when the peer closes the RPC socket.
fn serve(dev: &mut Device, rpc_fd: RawFd, event_fd: RawFd) {
    let mut buf = vec![0u8; MAX_FRAME_LEN];
    loop {
        let (n, fd_in) = match recv_frame(rpc_fd, &mut buf) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[gndrm-hostd] recv: {e}");
                return;
            }
        };
        if n == 0 {
            eprintln!("[gndrm-hostd] peer closed");
            return;
        }
        let Ok(hdr) = ReqHdr::decode(&buf[..n]) else {
            eprintln!("[gndrm-hostd] bad request header, dropping");
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
            eprintln!("[gndrm-hostd] send failed");
            return;
        }

        for ev in result.events {
            let mut bytes = Vec::new();
            gndrm_daemon::pod::pod_write(&mut bytes, &ev);
            if send_frame(event_fd, &bytes, None).is_err() {
                eprintln!("[gndrm-hostd] event send failed");
                return;
            }
        }
    }
}

/// `recvmsg(2)` one SEQPACKET frame (+ optional SCM_RIGHTS fd).
fn recv_frame(fd: RawFd, buf: &mut [u8]) -> std::io::Result<(usize, Option<RawFd>)> {
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
        return Err(std::io::Error::last_os_error());
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
fn send_frame(fd: RawFd, bytes: &[u8], fd_send: Option<RawFd>) -> std::io::Result<()> {
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
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
