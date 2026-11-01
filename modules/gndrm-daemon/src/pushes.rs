//! Push delivery to the clients that asked for it.
//!
//! The card listener hands the *allocator* role to one connection at a time
//! ([`crate::server::accept_newest`]), but a push is not the allocator's: the
//! frame cap, the panel's latch grid and the frame-generation settings belong to
//! every client that gates its own behaviour on them. A guest that connects and
//! is later superseded by another client keeps its registration here, so it
//! keeps learning — which is what a live cap change needs.
//!
//! The single slot could not carry that. The note used to be sent to whoever
//! held the allocator at the instant it changed, gated on that connection having
//! subscribed; a session has three clients that open the listener (the
//! compositor, a wine child, the game), they supersede each other for the first
//! ~20 s, and the one that ends up holding the slot is not necessarily the one
//! that gates. A subscriber that lost the slot was closed with it, so its cap
//! froze at whatever it read when it connected.
//!
//! Delivery is fire-and-forget on a non-blocking send. A sink whose peer is gone
//! fails that send, and the failure is the only reaper: the connection is closed
//! here and forgotten — except the one currently served, which its own loop
//! closes when the failed push surfaces there.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, PoisonError};

#[cfg(test)]
use gndrm_uapi::wire::{CAP_FRAME_CAP, CAP_PRESENT_ID, CAP_VSYNC_NOTE};

/// One connection that asked to be pushed to: its descriptor and the cap bits it
/// declared in its subscribe.
#[derive(Debug, Clone, Copy)]
struct Sink {
    fd: RawFd,
    caps: u32,
}

/// Every registered sink. Process-lifetime: a subscription outlives the
/// allocator slot, and a new subscribe on a reused descriptor replaces the old
/// entry rather than shadowing it.
static SINKS: Mutex<Vec<Sink>> = Mutex::new(Vec::new());

/// The descriptor the acceptor is serving right now, `-1` when it is parked on
/// the listener. Only used to keep [`push`] from closing a descriptor another
/// loop still owns.
static SERVED_FD: AtomicI32 = AtomicI32::new(-1);

/// Register `fd` for the pushes its `caps` bits name. A second subscribe on the
/// same descriptor (a shim reconnecting onto a reused number) replaces the caps.
pub fn add(fd: RawFd, caps: u32) {
    let mut sinks = SINKS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(sink) = sinks.iter_mut().find(|s| s.fd == fd) {
        sink.caps = caps;
    } else {
        sinks.push(Sink { fd, caps });
    }
}

/// Forget `fd`; `true` when it was registered, i.e. when this registry — not the
/// caller — is the descriptor's only owner and the caller must therefore close
/// it only if this returned `false`.
#[must_use]
pub fn forget(fd: RawFd) -> bool {
    let mut sinks = SINKS.lock().unwrap_or_else(PoisonError::into_inner);
    let before = sinks.len();
    sinks.retain(|s| s.fd != fd);
    sinks.len() != before
}

/// The registered descriptors, minus `except`: what a loop serving one
/// connection should keep reading for the others (a retired subscriber still
/// reports what it produces — see [`crate::pushes`]).
#[must_use]
pub fn fds_except(except: RawFd) -> Vec<RawFd> {
    SINKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|s| s.fd)
        .filter(|fd| *fd != except)
        .collect()
}

/// Whether any registered sink asked for pushes of `cap`'s kind. This is what
/// keeps a measurement running while the client that wants it is not the one
/// holding the allocator: the grid is only worth tracking if someone reads it.
#[must_use]
pub fn wants(cap: u32) -> bool {
    SINKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|s| s.caps & cap != 0)
}

/// Whether `fd` is registered, without disturbing it.
#[must_use]
pub fn holds(fd: RawFd) -> bool {
    SINKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|s| s.fd == fd)
}

/// Record the descriptor the acceptor is serving, so a failed push to it is left
/// for that loop to close.
pub fn set_served(fd: RawFd) {
    SERVED_FD.store(fd, Ordering::Relaxed);
}

/// Clear the served descriptor if it is still `fd` (a later connection may
/// already have replaced it).
pub fn clear_served(fd: RawFd) {
    let _ = SERVED_FD.compare_exchange(fd, -1, Ordering::Relaxed, Ordering::Relaxed);
}

/// Send one encoded `frame` to every sink that declared `cap` in its subscribe.
///
/// Non-blocking, and it must be: these descriptors are blocking sockets and the
/// caller is the acceptor thread, so a push into a full buffer would stall
/// allocation behind a client that is not reading. `EAGAIN` therefore keeps the
/// sink — the next push carries the current state anyway, and nothing is owed in
/// between. Any other failure means the peer is gone: the sink is dropped, and
/// its descriptor closed unless the acceptor is serving it.
pub fn push(cap: u32, frame: &[u8]) {
    let served = SERVED_FD.load(Ordering::Relaxed);
    let mut sinks = SINKS.lock().unwrap_or_else(PoisonError::into_inner);
    sinks.retain(|sink| {
        if sink.caps & cap == 0 {
            return true;
        }
        // SAFETY: `sink.fd` is a descriptor this registry owns until it is
        // dropped or closed here, and `frame` outlives the call. MSG_NOSIGNAL
        // keeps a peer that has gone away from raising SIGPIPE in the daemon.
        let sent = unsafe {
            libc::send(
                sink.fd,
                frame.as_ptr().cast(),
                frame.len(),
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if sent == frame.len() as isize {
            return true;
        }
        let full = sent < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN);
        if full {
            return true;
        }
        if sink.fd != served {
            // SAFETY: the failed send proved the peer unusable, and this
            // registry holds the only claim on the descriptor.
            unsafe { libc::close(sink.fd) };
        }
        false
    });
}

/// Close and forget every sink (session teardown: nothing is left to push to).
pub fn close_all() {
    let mut sinks = SINKS.lock().unwrap_or_else(PoisonError::into_inner);
    for sink in sinks.drain(..) {
        // SAFETY: this registry is the descriptor's only owner.
        unsafe { libc::close(sink.fd) };
    }
    SERVED_FD.store(-1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connected `SOCK_SEQPACKET` pair, the transport the scan-out channel is.
    fn pair() -> (RawFd, RawFd) {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: two out-params in a live array.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0);
        (fds[0], fds[1])
    }

    fn recv_one(fd: RawFd) -> Option<Vec<u8>> {
        let mut buf = [0u8; 64];
        // SAFETY: `fd` is one end of a pair and `buf` is one frame.
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        (n > 0).then(|| buf[..n as usize].to_vec())
    }

    /// The registry is process-wide, so these tests take turns: each one starts
    /// from an empty registry and holds the lock until it ends.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        close_all();
        guard
    }

    #[test]
    fn every_subscriber_receives_the_push() {
        let _g = reset();
        let (a, peer_a) = pair();
        let (b, peer_b) = pair();
        add(a, CAP_FRAME_CAP);
        add(b, CAP_FRAME_CAP);
        push(CAP_FRAME_CAP, b"cap");
        assert_eq!(recv_one(peer_a).as_deref(), Some(&b"cap"[..]));
        assert_eq!(recv_one(peer_b).as_deref(), Some(&b"cap"[..]));
        assert!(holds(a) && holds(b));
        // SAFETY: the test owns every descriptor it opened.
        unsafe {
            [a, b, peer_a, peer_b].iter().for_each(|fd| {
                libc::close(*fd);
            })
        };
    }

    #[test]
    fn a_sink_only_gets_the_kinds_it_declared() {
        let _g = reset();
        let (cap_only, peer) = pair();
        add(cap_only, CAP_FRAME_CAP);
        push(CAP_VSYNC_NOTE, b"grid");
        assert!(recv_one(peer).is_none(), "a cap-only sink got a grid note");
        push(CAP_FRAME_CAP, b"fps");
        assert_eq!(recv_one(peer).as_deref(), Some(&b"fps"[..]));
        push(CAP_PRESENT_ID, b"done");
        assert!(recv_one(peer).is_none(), "a cap-only sink got a present id");
        // SAFETY: the test owns every descriptor it opened.
        unsafe {
            [cap_only, peer].iter().for_each(|fd| {
                libc::close(*fd);
            })
        };
    }

    #[test]
    fn a_dead_sink_is_dropped_and_its_descriptor_closed() {
        let _g = reset();
        let (live, peer_live) = pair();
        let (dead, peer_dead) = pair();
        add(live, CAP_FRAME_CAP);
        add(dead, CAP_FRAME_CAP);
        // The peer goes away: the pair's other end is closed, so a send fails.
        // SAFETY: the test owns `peer_dead`.
        unsafe { libc::close(peer_dead) };
        push(CAP_FRAME_CAP, b"cap");
        assert!(holds(live));
        assert!(!holds(dead), "a sink whose peer is gone must be dropped");
        assert_eq!(recv_one(peer_live).as_deref(), Some(&b"cap"[..]));
        // SAFETY: closing a descriptor this registry already closed is the bug
        // this test guards against; a second close would show as EBADF.
        let rc = unsafe { libc::fcntl(dead, libc::F_GETFD) };
        assert_eq!(rc, -1, "the dropped sink's descriptor was left open");
        // SAFETY: the test owns the remaining descriptors.
        unsafe {
            [live, peer_live].iter().for_each(|fd| {
                libc::close(*fd);
            })
        };
    }

    #[test]
    fn a_full_buffer_keeps_the_sink() {
        let _g = reset();
        let (busy, peer) = pair();
        // A tiny send buffer, so a few frames fill it and the next send is
        // `EAGAIN` — a client that is merely behind, not gone.
        let small: libc::c_int = 128;
        // SAFETY: setsockopt on a socket this test owns.
        unsafe {
            libc::setsockopt(
                busy,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw const small).cast(),
                core::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        add(busy, CAP_FRAME_CAP);
        for _ in 0..64 {
            push(CAP_FRAME_CAP, &[0u8; 64]);
        }
        assert!(
            holds(busy),
            "a sink that is behind must not be dropped as if its peer had gone"
        );
        // SAFETY: the test owns every descriptor it opened.
        unsafe {
            [busy, peer].iter().for_each(|fd| {
                libc::close(*fd);
            })
        };
    }

    #[test]
    fn the_served_descriptor_is_left_to_its_own_loop() {
        let _g = reset();
        let (served, peer) = pair();
        add(served, CAP_FRAME_CAP);
        set_served(served);
        // SAFETY: the test owns `peer`.
        unsafe { libc::close(peer) };
        push(CAP_FRAME_CAP, b"cap");
        assert!(!holds(served), "a failed push still drops the registration");
        // SAFETY: probe only, no ownership taken.
        let rc = unsafe { libc::fcntl(served, libc::F_GETFD) };
        assert_ne!(rc, -1, "the served descriptor must stay open for its loop");
        clear_served(served);
        // SAFETY: the test owns `served`.
        unsafe { libc::close(served) };
    }

    #[test]
    fn a_second_subscribe_on_one_descriptor_replaces_its_caps() {
        let _g = reset();
        let (fd, peer) = pair();
        add(fd, CAP_FRAME_CAP);
        add(fd, CAP_VSYNC_NOTE);
        push(CAP_FRAME_CAP, b"fps");
        assert!(
            recv_one(peer).is_none(),
            "the old caps survived the resubscribe"
        );
        push(CAP_VSYNC_NOTE, b"grid");
        assert_eq!(recv_one(peer).as_deref(), Some(&b"grid"[..]));
        // SAFETY: the test owns every descriptor it opened.
        unsafe {
            [fd, peer].iter().for_each(|x| {
                libc::close(*x);
            })
        };
    }

    #[test]
    fn teardown_closes_what_is_left() {
        let _g = reset();
        let (a, peer_a) = pair();
        let (b, peer_b) = pair();
        add(a, CAP_FRAME_CAP);
        add(b, CAP_VSYNC_NOTE);
        close_all();
        // SAFETY: probe only.
        assert_eq!(unsafe { libc::fcntl(a, libc::F_GETFD) }, -1);
        // SAFETY: probe only.
        assert_eq!(unsafe { libc::fcntl(b, libc::F_GETFD) }, -1);
        assert!(!holds(a) && !holds(b));
        // SAFETY: the test owns the peers.
        unsafe {
            [peer_a, peer_b].iter().for_each(|fd| {
                libc::close(*fd);
            })
        };
    }
}
