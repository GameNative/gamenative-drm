//! Blocking SEQPACKET socket server shared by `gndrm-hostd` (host tests,
//! B9 harness) and `gndrm-android` (in-app daemon).
//!
//! Two listeners, matching the two node types the shim impersonates:
//!
//! - **card node** — the main socket ([`listen_seqpacket`]'s name). Connection
//!   contract: the first accepted connection is the RPC channel, the second is
//!   the event channel (matches `gndrm-shim`'s connect order); every later
//!   connection is the single scan-out allocator channel (newest wins). One
//!   card client at a time — gamescope.
//! - **render node** — [`render_socket_name`] (the main name + `-render`).
//!   Served by [`serve_render_listener`], one [`Device`] per RPC/event pair,
//!   for as many concurrent clients as connect. Xwayland's glamor/dri3/drm
//!   opens `/dev/dri/renderDN` here; a render node never scans out, so it has
//!   no allocator channel.
//!
//! Keeping the render node on its own listener is what lets a second DRM client
//! process (Xwayland) coexist with gamescope: on the card listener any
//! post-primary connection is the scan-out channel, so a render-node client
//! there would be adopted as a (bad) scan-out channel and closed — which is
//! exactly the `EPIPE`/`Broken pipe` a render-node `open` used to see on device.
//!
//! Address convention (must match `gndrm-shim`'s `connect_one`):
//! - `"/abs/path"` → filesystem socket (unlinked first)
//! - `"@name"` or a bare `"name"` → Linux abstract namespace
//!
//! Both sides always pass a full-`sizeof(sockaddr_un)` addrlen, so abstract
//! names compare identically padded.

use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex};
use std::thread;

use gndrm_uapi::wire::{ReqHdr, RespHdr, MAX_FRAME_LEN, REQ_HDR_LEN, RESP_HDR_LEN};

use crate::diag::{diag, Role, State};
use crate::pod::pod_write;
use crate::{close_fd, Device, DispatchResult};

/// Log sink: `eprintln` adapter on host, `__android_log_write` on Android.
/// Cold paths only — the per-frame hot path never logs.
pub type Log<'a> = &'a dyn Fn(&str);

/// A log sink that can be shared with the per-client threads
/// [`serve_render_listener`] spawns (`Log` borrows, so it cannot cross a
/// thread boundary on its own).
pub type SharedLog = Arc<dyn Fn(&str) + Send + Sync>;

/// The dedicated render-node listener name for a card socket `main`.
///
/// Both the daemon and `gndrm-shim` derive it the same way, so a render-node
/// client reaches a listener that serves it as a render node instead of the
/// card listener's scan-out allocator.
#[must_use]
pub fn render_socket_name(main: &str) -> String {
    format!("{}", gndrm_obf::obf!("{main}-render"))
}

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

/// Accept the *newest* connection already queued on `listener`, closing any
/// connection that was queued before it.
///
/// The scan-out channel is re-established by the shim, and a reconnect can be
/// queued on the listener while the daemon is still serving the previous
/// connection. Accepting in accept order would leave the daemon reading the
/// older, already-abandoned connection while the shim writes every request on
/// the newest one, so nothing the shim sends after the reconnect is ever read.
/// This drains the backlog and keeps only the latest, which is what "one
/// reader, on the current fd" requires; each superseded descriptor is closed
/// here so none is leaked and no reader is left on a replaced fd.
///
/// The first accept blocks (the caller polls the listener first); the rest of
/// the drain is non-blocking, so at most one connection is consumed when only
/// one is queued.
pub fn accept_newest(listener: RawFd) -> io::Result<RawFd> {
    let mut newest = accept(listener)?;
    // F_GETFL/F_SETFL on the listener is the portable way to make accept(2)
    // report "nothing queued" (EAGAIN) instead of blocking; the accepted
    // descriptors do not inherit O_NONBLOCK, so the served fd stays blocking,
    // which `recvmsg` on the request path relies on. The original flags are
    // restored before returning.
    // SAFETY: fcntl on a live listening socket.
    let flags = unsafe { libc::fcntl(listener, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(listener, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Ok(newest);
    }
    while let Ok(fd) = accept(listener) {
        // A connection newer than the one held: the peer moved on, so the
        // previous one is dead to it. Close it and keep the newer descriptor as
        // the current one.
        // SAFETY: `newest` came from accept and is not used again.
        unsafe { libc::close(newest) };
        newest = fd;
    }
    // SAFETY: restore the listener's original (blocking) flags; the listening
    // socket must keep them for the acceptor's own blocking poll/accept.
    unsafe { libc::fcntl(listener, libc::F_SETFL, flags) };
    Ok(newest)
}

/// Create an `EFD_CLOEXEC` eventfd used to stop a listener loop (see
/// [`serve_render_listener`]).
pub fn new_stop_eventfd() -> io::Result<RawFd> {
    // SAFETY: eventfd takes a count and flags word; it returns a fresh
    // descriptor this caller owns, or -1 with errno set.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Wake a loop parked on a stop eventfd created by [`new_stop_eventfd`].
pub fn signal_stop(stop_fd: RawFd) {
    let one: u64 = 1;
    // SAFETY: an 8-byte counter write to an eventfd the caller owns.
    unsafe { libc::write(stop_fd, (&raw const one).cast(), 8) };
}

/// The peer process id of a connected Unix socket (`SO_PEERCRED`), or `-1`.
///
/// [`serve_render_listener`] uses it to pair each client's RPC and event
/// connections: they are opened back to back by one process, so they share a
/// pid and can be paired even if two clients' connects interleave.
fn peer_pid(fd: RawFd) -> i32 {
    // SAFETY: getsockopt writes at most sizeof(ucred) into `cred`, whose length
    // is passed explicitly; a failure leaves the zeroed value (pid 0).
    unsafe {
        let mut cred: libc::ucred = core::mem::zeroed();
        let mut len = core::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        ) != 0
        {
            return -1;
        }
        cred.pid
    }
}

/// Serve render-node clients on the dedicated render listener until `stop_fd`
/// is signalled (or the listener is closed).
///
/// Every accepted connection is one half of a client: the shim opens the RPC
/// channel and then the event channel back to back, so two connections from the
/// same peer pid form one client. Each pair gets its own [`Device`], built by
/// `make_device` (a factory so the Android half can inject its presenter and
/// geometry sink while the host harness and `gndrm-hostd` run the identical
/// serve loop), and is served on its own thread — a render node has no scan-out
/// and never touches the presenter.
///
/// This is the device-side path `libgamenative_drm.so` runs for Xwayland's
/// glamor/dri3 open, factored out of the Android glue so the host harness
/// exercises exactly the same code.
pub fn serve_render_listener(
    listener: RawFd,
    stop_fd: RawFd,
    make_device: Arc<dyn Fn() -> Device + Send + Sync>,
    log: SharedLog,
) {
    // RPC connections awaiting their event partner, keyed by peer pid.
    let mut pending: HashMap<i32, RawFd> = HashMap::new();
    // Live clients (`rpc`, `event`), so teardown can unblock and reap them.
    let clients: Arc<Mutex<Vec<(RawFd, RawFd)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut handles: Vec<thread::JoinHandle<()>> = Vec::new();
    let mut next_id = 0usize;
    loop {
        let mut pfds = [
            libc::pollfd {
                fd: listener,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfds waited on indefinitely.
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            log(&format!("{}", gndrm_obf::obf!("render listener poll: {e}")));
            break;
        }
        if pfds[1].revents != 0 {
            log(&gndrm_obf::obf!("render listener stopping"));
            break;
        }
        if pfds[0].revents == 0 {
            continue;
        }
        let fd = match accept(listener) {
            Ok(fd) => fd,
            Err(e) => {
                log(&format!(
                    "{}",
                    gndrm_obf::obf!("render listener accept: {e}")
                ));
                break;
            }
        };
        let pid = peer_pid(fd);
        match pending.remove(&pid) {
            Some(rpc) => {
                // Second half of this client: its event channel. Serve the pair.
                log(&format!(
                    "{}",
                    gndrm_obf::obf!("render client paired (pid {pid})")
                ));
                clients
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((rpc, fd));
                let make_device = Arc::clone(&make_device);
                let thread_log = Arc::clone(&log);
                let thread_clients = Arc::clone(&clients);
                let name = format!("{}", gndrm_obf::obf!("gndrm-rn{next_id}"));
                next_id += 1;
                let spawned = thread::Builder::new().name(name).spawn(move || {
                    let mut dev = make_device();
                    let sink = Arc::clone(&thread_log);
                    dev.set_logger(Box::new(move |m: &str| sink(m)));
                    let lend: &dyn Fn(&str) = &*thread_log;
                    serve(&mut dev, rpc, fd, lend);
                    // SAFETY: this thread accepted neither fd but is its only
                    // reader/writer; the acceptor's registry entry is removed
                    // first so teardown cannot double-shutdown a closed fd.
                    if let Ok(mut c) = thread_clients.lock() {
                        c.retain(|(r, e)| !(*r == rpc && *e == fd));
                    }
                    unsafe {
                        libc::close(rpc);
                        libc::close(fd);
                    }
                });
                match spawned {
                    Ok(h) => handles.push(h),
                    Err(e) => {
                        log(&format!("{}", gndrm_obf::obf!("render client spawn: {e}")));
                        if let Ok(mut c) = clients.lock() {
                            c.retain(|(r, ev)| !(*r == rpc && *ev == fd));
                        }
                        // SAFETY: nothing else owns these.
                        unsafe {
                            libc::shutdown(rpc, libc::SHUT_RDWR);
                            libc::shutdown(fd, libc::SHUT_RDWR);
                            libc::close(rpc);
                            libc::close(fd);
                        }
                    }
                }
            }
            None => {
                // First half of this client: its RPC channel. Until its event
                // half arrives this descriptor is read by nobody, so a client
                // whose event connection never follows would block on its first
                // request with nothing on either side to say why. The line is
                // the daemon half of that handshake.
                log(&format!(
                    "{}",
                    gndrm_obf::obf!(
                        "render client rpc accepted (pid {pid}), awaiting event channel"
                    )
                ));
                pending.insert(pid, fd);
            }
        }
    }
    // Teardown: unblock every live serve loop, then close the RPC halves whose
    // event partner never arrived.
    for (rpc, event) in clients.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        // SAFETY: shutdown only unblocks a recvmsg/poll; the client thread
        // remains the owner that closes the descriptors.
        unsafe {
            libc::shutdown(*rpc, libc::SHUT_RDWR);
            libc::shutdown(*event, libc::SHUT_RDWR);
        }
    }
    for (_, rpc) in pending.drain() {
        // SAFETY: the RPC half was accepted here and has no client thread.
        unsafe { libc::close(rpc) };
    }
    for h in handles {
        let _ = h.join();
    }
}

/// Blocking dispatch loop: one frame in → one frame out, plus flip events the
/// commit worker produces. Returns when the peer closes the RPC socket, the RPC
/// fd is shut down externally (`nativeStop`), or a send fails.
///
/// The loop polls the RPC socket together with the device's flip-event fd so a
/// flip event is delivered promptly even while no request is in flight — a
/// client (gamescope) parks in `poll()` on its event fd right after a commit,
/// and the serve thread must not wait for the *next* request to push the event
/// through.
pub fn serve(dev: &mut Device, rpc_fd: RawFd, event_fd: RawFd, log: Log) {
    let mut buf = vec![0u8; MAX_FRAME_LEN];
    let flip_fd = dev.flip_event_fd();
    // Publish this loop's state so a stalled scan-out request can rule the
    // serve thread in or out as the blocker.
    let d = diag();
    loop {
        // Wait for either an RPC frame or a flip event from the worker.
        d.set_role(Role::Serve, State::ServeWaitingRpc);
        let mut pfds = [
            libc::pollfd {
                fd: rpc_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: flip_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfds waited on indefinitely.
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            log(&format!("{}", gndrm_obf::obf!("poll: {e}")));
            return;
        }
        // Flip events are ready: write them to the client's event channel in
        // commit order.
        if pfds[1].revents != 0 {
            for ev in dev.drain_flip_events() {
                let mut bytes = Vec::new();
                pod_write(&mut bytes, &ev);
                if let Err(e) = send_frame(event_fd, &bytes, None) {
                    log(&format!("{}", gndrm_obf::obf!("event send failed: {e}")));
                    return;
                }
            }
        }
        if pfds[0].revents == 0 {
            continue;
        }
        let (n, fd_in) = match recv_frame(rpc_fd, &mut buf) {
            Ok(v) => v,
            Err(e) => {
                log(&format!("{}", gndrm_obf::obf!("recv: {e}")));
                return;
            }
        };
        if n == 0 {
            log(&gndrm_obf::obf!("peer closed"));
            return;
        }
        let Ok(hdr) = ReqHdr::decode(&buf[..n]) else {
            log(&gndrm_obf::obf!("bad request header, dropping"));
            return;
        };
        let payload = &buf[REQ_HDR_LEN..hdr.total_len as usize];
        d.set_role(Role::Serve, State::ServeDispatch);
        let result: DispatchResult = dev.dispatch(hdr.ioctl_nr, payload, fd_in);

        let resp = RespHdr {
            total_len: (RESP_HDR_LEN + result.body.len()) as u32,
            seq: hdr.seq,
            ret: result.ret,
            out_len: result.body.len() as u32,
        };
        let mut frame = resp.encode().to_vec();
        frame.extend_from_slice(&result.body);
        let sent = send_frame(rpc_fd, &frame, result.fd_out);
        // The daemon's copy of the attached descriptor is closed either way:
        // `SCM_RIGHTS` duplicates it into the peer on success, and a failed
        // `sendmsg` transfers nothing. Not closing it leaks one descriptor per
        // `PRIME_HANDLE_TO_FD`, which pins the exported dma-buf forever.
        if let Some(fd) = result.fd_out {
            close_fd(fd);
        }
        if let Err(e) = sent {
            log(&format!("{}", gndrm_obf::obf!("send failed: {e}")));
            return;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Connect one `SOCK_SEQPACKET` client to the abstract-name listener the
    /// way the shim does (full `sizeof(sockaddr_un)` addrlen).
    fn client_connect(name: &str) -> RawFd {
        // SAFETY: fresh socket; the sockaddr_un is zeroed and the abstract name
        // is copied into sun_path+1 with the length bounds checked by the test.
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
            assert!(fd >= 0, "socket: {}", io::Error::last_os_error());
            let mut sa: libc::sockaddr_un = core::mem::zeroed();
            sa.sun_family = libc::AF_UNIX as u16;
            assert!(name.len() < sa.sun_path.len() - 1);
            core::ptr::copy_nonoverlapping(
                name.as_ptr().cast::<u8>(),
                sa.sun_path.as_mut_ptr().add(1).cast::<u8>(),
                name.len(),
            );
            let rc = libc::connect(
                fd,
                (&raw const sa).cast(),
                core::mem::size_of::<libc::sockaddr_un>() as u32,
            );
            assert_eq!(rc, 0, "connect: {}", io::Error::last_os_error());
            fd
        }
    }

    /// A unique abstract socket name per test invocation (tests are threads in
    /// one process, so a fixed name would collide across parallel runs).
    fn unique_name() -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        format!(
            "{}",
            gndrm_obf::obf!(
                "gndrm-srv-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            )
        )
    }

    /// The accept-newest contract the scan-out acceptor depends on: when a
    /// shim reconnect is queued behind the connection it is abandoning, the
    /// daemon must end up reading the *newest* connection and the superseded
    /// one must be closed (no reader left on a replaced fd, no leak).
    #[test]
    fn accept_newest_adopts_the_latest_and_drops_the_superseded() {
        let name = unique_name();
        let listener = listen_seqpacket(&name).expect(&gndrm_obf::obf!("listen"));
        let old = client_connect(&name);
        let new = client_connect(&name);

        let adopted = accept_newest(listener).expect(&gndrm_obf::obf!("accept_newest"));

        // The adopted fd is the newest client: a byte it sends is readable here.
        let marker = [0xA5u8];
        // SAFETY: valid sockets and a one-byte buffer.
        let sent = unsafe { libc::send(new, marker.as_ptr().cast(), 1, 0) };
        assert_eq!(sent, 1, "send(new): {}", io::Error::last_os_error());
        let mut got = [0u8; 1];
        let n = unsafe { libc::recv(adopted, got.as_mut_ptr().cast(), 1, 0) };
        assert_eq!(n, 1, "recv(adopted): {}", io::Error::last_os_error());
        assert_eq!(got[0], 0xA5);

        // The superseded connection was closed by `accept_newest`: a write from
        // it now fails instead of reaching a reader nobody owns.
        // SAFETY: valid socket; MSG_NOSIGNAL avoids SIGPIPE on the closed peer.
        let r = unsafe { libc::send(old, marker.as_ptr().cast(), 1, libc::MSG_NOSIGNAL) };
        assert!(
            r < 0,
            "the superseded connection must be closed, send returned {r}"
        );

        // SAFETY: every descriptor created here is closed once.
        unsafe {
            libc::close(old);
            libc::close(new);
            libc::close(adopted);
            libc::close(listener);
        }
    }

    /// A single queued connection is adopted unchanged (the common case: no
    /// reconnect, the drain touches nothing).
    #[test]
    fn accept_newest_adopts_a_lone_connection() {
        let name = unique_name();
        let listener = listen_seqpacket(&name).expect(&gndrm_obf::obf!("listen"));
        let client = client_connect(&name);

        let adopted = accept_newest(listener).expect(&gndrm_obf::obf!("accept_newest"));

        let marker = [0x11u8];
        // SAFETY: valid sockets and a one-byte buffer.
        let sent = unsafe { libc::send(client, marker.as_ptr().cast(), 1, 0) };
        assert_eq!(sent, 1);
        let mut got = [0u8; 1];
        let n = unsafe { libc::recv(adopted, got.as_mut_ptr().cast(), 1, 0) };
        assert_eq!(n, 1);
        assert_eq!(got[0], 0x11);

        // SAFETY: every descriptor created here is closed once.
        unsafe {
            libc::close(client);
            libc::close(adopted);
            libc::close(listener);
        }
    }

    /// Build one request frame (`ReqHdr` + payload) the way the shim does.
    fn req(ioctl_nr: u32, payload: &[u8], seq: u64) -> Vec<u8> {
        let hdr = ReqHdr {
            total_len: (REQ_HDR_LEN + payload.len()) as u32,
            ioctl_nr,
            flags: 0,
            seq,
        };
        let mut f = hdr.encode().to_vec();
        f.extend_from_slice(payload);
        f
    }

    /// Send one frame and read its response: `(ret, body)`.
    fn exchange(fd: RawFd, frame: &[u8]) -> (i32, Vec<u8>) {
        // SAFETY: valid socket and buffer; SOCK_SEQPACKET preserves the frame.
        let sent = unsafe { libc::send(fd, frame.as_ptr().cast(), frame.len(), 0) };
        assert_eq!(
            sent as usize,
            frame.len(),
            "send: {}",
            io::Error::last_os_error()
        );
        let mut buf = vec![0u8; MAX_FRAME_LEN];
        // SAFETY: valid socket and a MAX_FRAME_LEN buffer.
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
        assert!(n > 0, "recv: {}", io::Error::last_os_error());
        let n = n as usize;
        let hdr = RespHdr::decode(&buf[..n]).expect(&gndrm_obf::obf!("response header"));
        (hdr.ret, buf[RESP_HDR_LEN..n].to_vec())
    }

    /// `serve_render_listener` serves a render-node client the way the device
    /// daemon serves Xwayland: the RPC/event pair is paired by peer pid, the
    /// node kind is declared over the wire, GEM_CREATE works and modesetting is
    /// gated — all through the shared device-side loop.
    #[test]
    fn render_listener_serves_a_render_client_end_to_end() {
        use crate::pod::pod_read;
        use gndrm_uapi::drm::{
            DrmGemCreate, DRM_IOCTL_GEM_CREATE, DRM_IOCTL_SET_MASTER, GNDRM_IOCTL_NODE_KIND,
            NODE_KIND_RENDER,
        };
        use gndrm_uapi::wire::errno;

        let name = unique_name();
        let listener = listen_seqpacket(&name).expect(&gndrm_obf::obf!("listen"));
        let stop_fd = new_stop_eventfd().expect(&gndrm_obf::obf!("stop eventfd"));
        let log: SharedLog = Arc::new(|_| {});
        let acceptor = thread::spawn(move || {
            serve_render_listener(
                listener,
                stop_fd,
                Arc::new(|| {
                    crate::Device::new(
                        crate::PanelDesc::default(),
                        Box::<crate::NullPresenter>::default(),
                    )
                }),
                log,
            );
        });

        // One client = an RPC connection then the event connection, from this
        // process (same peer pid — the pairing key).
        let rpc = client_connect(&name);
        let event = client_connect(&name);

        // Declare a render node over the wire.
        let (ret, _) = exchange(
            rpc,
            &req(GNDRM_IOCTL_NODE_KIND, &NODE_KIND_RENDER.to_le_bytes(), 1),
        );
        assert_eq!(ret, 0, "NODE_KIND_RENDER must be accepted");

        // GEM_CREATE is render-capable: it succeeds and returns a handle.
        let mut body = Vec::new();
        pod_write(
            &mut body,
            &DrmGemCreate {
                size: 4096,
                handle: 0,
                flags: 0,
            },
        );
        let (ret, out) = exchange(rpc, &req(DRM_IOCTL_GEM_CREATE, &body, 2));
        assert_eq!(ret, 0, "GEM_CREATE on a render node");
        let created: DrmGemCreate = pod_read(&out).expect(&gndrm_obf::obf!("GEM_CREATE body"));
        assert!(created.handle > 0, "GEM_CREATE returned a handle");

        // Modesetting is refused on a render node (DRM_RENDER_ALLOW gate).
        let (ret, _) = exchange(rpc, &req(DRM_IOCTL_SET_MASTER, &[], 3));
        assert_eq!(
            ret,
            -errno::EACCES,
            "SET_MASTER must be gated on a render node"
        );

        // SAFETY: both client descriptors were created here and are closed once.
        unsafe {
            libc::close(rpc);
            libc::close(event);
        }

        // Stop and reap: proves the loop exits cleanly on its stop eventfd.
        signal_stop(stop_fd);
        acceptor
            .join()
            .expect(&gndrm_obf::obf!("render acceptor thread"));
    }

    /// The render listener name the shim and the daemon must agree on.
    #[test]
    fn render_socket_name_is_the_main_name_plus_render() {
        assert_eq!(render_socket_name("/tmp/x.sock"), "/tmp/x.sock-render");
        assert_eq!(render_socket_name("gndrm"), "gndrm-render");
        assert_eq!(render_socket_name("@gndrm"), "@gndrm-render");
    }
}
