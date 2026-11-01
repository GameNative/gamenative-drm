//! Page-flip events at the latch.
//!
//! gamescope hands the buffers a flip replaced back to their producers on the
//! page-flip event. The event for a commit with a new framebuffer therefore
//! leaves when SurfaceFlinger's completion for that frame's transaction
//! arrives (the frame latched), stamped with the latch time, instead of when
//! `apply` returned, and once the release fence the previous completion
//! delivered has signalled: that is the buffer the previous flip took off
//! screen, which gamescope (A56) releases at this event. Normally it has
//! already signalled; a wait is counted (`prev_release_waits`) and bounded by
//! the valve. Commits with no new framebuffer complete when armed.
//! Events leave in commit order, one per commit; a later completion completes
//! earlier commits; a [`FlipDone`] dropped unfired completes at once; a commit
//! with no completion [`FLIP_VALVE`] after its publish returned is released
//! anyway (counted, logged rate-limited).

use std::collections::{HashSet, VecDeque};
use std::os::fd::RawFd;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gndrm_uapi::drm::DrmEventVblank;

use crate::{close_fd, write_eventfd, LogFn, SharedClock, VblankTimeline};

pub(crate) const FLIP_VALVE: Duration = Duration::from_millis(50);

struct Pending {
    seq: u64,
    fb: Option<u32>,
    event: Option<DrmEventVblank>,
    deferred: bool,
    done: bool,
    published: Option<Instant>,
    /// SurfaceFlinger's latch time, `CLOCK_MONOTONIC` ns; 0 when unknown.
    latch_ns: u64,
    /// The release fence the previous completion delivered (the buffer the
    /// previous flip took off screen, which gamescope's A56 releases at this
    /// event): waited before this event leaves (owned).
    prev_release: Option<RawFd>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(fd) = self.prev_release.take() {
            close_fd(fd);
        }
    }
}

#[derive(Default)]
struct GateState {
    next_seq: u64,
    queue: VecDeque<Pending>,
    shutdown: bool,
    /// The release fence the latest completion delivered, for the next one.
    carry: Option<RawFd>,
}

impl Drop for GateState {
    fn drop(&mut self) {
        if let Some(fd) = self.carry.take() {
            close_fd(fd);
        }
    }
}

#[derive(Default)]
pub(crate) struct FlipGate {
    state: Mutex<GateState>,
    cv: Condvar,
}

impl FlipGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn arm(
        &self,
        fb: Option<u32>,
        event: Option<DrmEventVblank>,
        deferred: bool,
    ) -> u64 {
        let mut st = self.lock();
        st.next_seq += 1;
        let seq = st.next_seq;
        st.queue.push_back(Pending {
            seq,
            fb,
            event,
            deferred,
            done: !deferred,
            published: None,
            latch_ns: 0,
            prev_release: None,
        });
        drop(st);
        self.cv.notify_all();
        seq
    }

    /// The commit's publish returned: the valve starts.
    pub(crate) fn published(&self, seq: u64) {
        if let Some(p) = self.lock().queue.iter_mut().find(|p| p.seq == seq) {
            p.published.get_or_insert_with(Instant::now);
        }
        self.cv.notify_all();
    }

    /// Commit `seq` latched at `latch_ns`; `release` (owned) is the release
    /// fence its completion delivered, carried to the next completed commit.
    /// This commit waits the fence the previous completion delivered.
    fn complete(&self, seq: u64, latch_ns: u64, release: Option<RawFd>) {
        let mut st = self.lock();
        let mut prev = std::mem::replace(&mut st.carry, release);
        for p in st.queue.iter_mut().take_while(|p| p.seq <= seq) {
            if p.seq == seq {
                p.latch_ns = latch_ns;
                if let Some(old) = std::mem::replace(&mut p.prev_release, prev.take()) {
                    close_fd(old);
                }
            }
            p.done = true;
        }
        drop(st);
        // Already emitted (the valve): nothing waits it any more.
        if let Some(fd) = prev {
            close_fd(fd);
        }
        self.cv.notify_all();
    }

    pub(crate) fn shutdown(&self) {
        self.lock().shutdown = true;
        self.cv.notify_all();
    }
}

/// The presenter's handle on one commit's flip event.
pub struct FlipDone {
    gate: Arc<FlipGate>,
    seq: u64,
    fired: bool,
}

impl FlipDone {
    pub(crate) fn new(gate: Arc<FlipGate>, seq: u64) -> Self {
        Self {
            gate,
            seq,
            fired: false,
        }
    }

    /// The frame latched at `latch_ns` (0 when unknown); `release` (owned) is
    /// the previous-release fence its completion delivered.
    pub fn complete(mut self, latch_ns: u64, release: Option<RawFd>) {
        self.fired = true;
        self.gate.complete(self.seq, latch_ns, release);
    }
}

impl Drop for FlipDone {
    fn drop(&mut self) {
        if !self.fired {
            self.gate.complete(self.seq, 0, None);
        }
    }
}

/// Wait up to `ms` for `fd`; `true` when it signalled.
fn poll_fence(fd: RawFd, ms: i32) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms.max(0) as u64);
    loop {
        let left = deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pollfd for the call.
        let rc = unsafe { libc::poll(&mut pfd, 1, left) };
        if rc > 0 {
            return true;
        }
        if rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
    }
}

pub(crate) fn flip_thread(
    gate: Arc<FlipGate>,
    flip_queue: Arc<Mutex<VecDeque<DrmEventVblank>>>,
    flip_event_fd: RawFd,
    in_flight: Arc<Mutex<HashSet<u32>>>,
    clock: SharedClock,
    logger: Arc<Mutex<Option<LogFn>>>,
) {
    let mut vblank = VblankTimeline::new();
    let mut last_stamp = 0u64;
    let mut valves = 0u64;
    let mut prev_release_waits = 0u64;
    loop {
        let mut valved = false;
        let mut p = {
            let mut st = gate.lock();
            loop {
                // Teardown: whatever is still queued has no reader any more.
                if st.shutdown {
                    close_fd(flip_event_fd);
                    return;
                }
                let Some(front) = st.queue.front() else {
                    if st.shutdown {
                        close_fd(flip_event_fd);
                        return;
                    }
                    st = gate.cv.wait(st).unwrap_or_else(PoisonError::into_inner);
                    continue;
                };
                if front.done {
                    break st.queue.pop_front().unwrap();
                }
                let Some(t) = front.published else {
                    st = gate.cv.wait(st).unwrap_or_else(PoisonError::into_inner);
                    continue;
                };
                let now = Instant::now();
                if now >= t + FLIP_VALVE {
                    valved = true;
                    break st.queue.pop_front().unwrap();
                }
                st = gate
                    .cv
                    .wait_timeout(st, t + FLIP_VALVE - now)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        };
        // The buffer the previous flip took off screen is released by
        // gamescope at this event: its release fence must have signalled.
        // Normally it has; a pending one is waited, bounded by the valve.
        if let Some(fd) = p.prev_release.take() {
            if !poll_fence(fd, 0) {
                prev_release_waits += 1;
                poll_fence(fd, FLIP_VALVE.as_millis() as i32);
                if prev_release_waits <= 8 || prev_release_waits.is_multiple_of(100) {
                    if let Some(l) = logger
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_ref()
                    {
                        l(&format!(
                            "{}",
                            gndrm_obf::obf!("gndrm-daemon: flip: waited the previous release fence (prev_release_waits={prev_release_waits})")
                        ));
                    }
                }
            }
            close_fd(fd);
        }
        // Logged outside the gate's lock.
        if valved {
            valves += 1;
            if valves <= 8 || valves.is_multiple_of(100) {
                if let Some(l) = logger
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                {
                    l(&format!(
                        "{}",
                        gndrm_obf::obf!(
                            "gndrm-daemon: flip: no completion {} ms after publish; event sent (valve={valves})",
                            FLIP_VALVE.as_millis()
                        )
                    ));
                }
            }
        }
        let (sec, usec) = (clock.lock().unwrap_or_else(PoisonError::into_inner))();
        let now = u64::from(sec) * 1_000_000_000 + u64::from(usec) * 1_000;
        // A latched frame is stamped with its latch time; anything else keeps
        // the vblank claim it had. Either way never ahead of now and never
        // behind the previous stamp.
        let at = if p.deferred {
            let at = if p.latch_ns > 0 { p.latch_ns } else { now };
            vblank.anchor(at);
            at
        } else {
            vblank.claim(now)
        };
        last_stamp = at.min(now).max(last_stamp);
        let stamp = last_stamp;
        if let Some(fb) = p.fb.take() {
            in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&fb);
        }
        if let Some(mut ev) = p.event.take() {
            ev.tv_sec = (stamp / 1_000_000_000) as u32;
            ev.tv_usec = ((stamp % 1_000_000_000) / 1_000) as u32;
            flip_queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_back(ev);
            write_eventfd(flip_event_fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gndrm_uapi::drm::{DrmEvent, DRM_EVENT_FLIP_COMPLETE};

    fn harness() -> (
        Arc<FlipGate>,
        Arc<Mutex<VecDeque<DrmEventVblank>>>,
        std::thread::JoinHandle<()>,
    ) {
        let gate = Arc::new(FlipGate::default());
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let efd = crate::new_eventfd(0).unwrap();
        let clock: SharedClock = Arc::new(Mutex::new(Box::new(|| (1000, 0))));
        let (g, q) = (Arc::clone(&gate), Arc::clone(&queue));
        let t = std::thread::spawn(move || {
            flip_thread(
                g,
                q,
                efd,
                Arc::new(Mutex::new(HashSet::new())),
                clock,
                Arc::new(Mutex::new(None)),
            );
        });
        (gate, queue, t)
    }

    fn ev(user_data: u64) -> Option<DrmEventVblank> {
        Some(DrmEventVblank {
            base: DrmEvent {
                ty: DRM_EVENT_FLIP_COMPLETE,
                length: core::mem::size_of::<DrmEventVblank>() as u32,
            },
            user_data,
            tv_sec: 0,
            tv_usec: 0,
            sequence: 0,
            crtc_id: 0,
        })
    }

    fn expect(q: &Mutex<VecDeque<DrmEventVblank>>, want: &[(u64, u32)]) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let got = || {
            q.lock()
                .unwrap()
                .iter()
                .map(|e| (e.user_data, e.tv_usec))
                .collect::<Vec<_>>()
        };
        while got() != want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(got(), want);
    }

    #[test]
    fn events_leave_at_the_latch_in_order_exactly_once() {
        let (gate, q, t) = harness();
        let a = gate.arm(Some(1), ev(1), true);
        gate.published(a);
        gate.arm(None, ev(2), false);
        std::thread::sleep(Duration::from_millis(10));
        assert!(q.lock().unwrap().is_empty(), "nothing before the latch");
        // Latched 0.5 ms before the (frozen) clock: stamped with the latch.
        FlipDone::new(Arc::clone(&gate), a).complete(999_999_500_000, None);
        // The immediate one continues the vblank grid from the latch, clamped
        // to now (the frozen clock).
        expect(&q, &[(1, 999_500), (2, 0)]);
        gate.complete(a, 0, None); // a second completion emits nothing
        gate.shutdown();
        t.join().unwrap();
        assert_eq!(q.lock().unwrap().len(), 2);
    }

    #[test]
    fn the_valve_releases_a_commit_never_completed_and_a_dropped_handle_goes_at_once() {
        let (gate, q, t) = harness();
        let a = gate.arm(Some(1), ev(1), true);
        std::thread::sleep(FLIP_VALVE + Duration::from_millis(10));
        assert!(
            q.lock().unwrap().is_empty(),
            "the valve runs from the publish"
        );
        gate.published(a);
        expect(&q, &[(1, 0)]);
        let b = gate.arm(Some(2), ev(2), true);
        drop(FlipDone::new(Arc::clone(&gate), b));
        expect(&q, &[(1, 0), (2, 0)]);
        gate.shutdown();
        t.join().unwrap();
    }

    #[test]
    fn a_completion_before_the_publish_returns_still_counts() {
        let (gate, q, t) = harness();
        let a = gate.arm(Some(1), ev(1), true);
        FlipDone::new(Arc::clone(&gate), a).complete(999_999_000_000, None);
        gate.published(a);
        expect(&q, &[(1, 999_000)]);
        gate.shutdown();
        t.join().unwrap();
    }

    #[test]
    fn a_late_completion_after_the_valve_emits_nothing_more() {
        let (gate, q, t) = harness();
        let a = gate.arm(Some(1), ev(1), true);
        gate.published(a);
        expect(&q, &[(1, 0)]); // the valve
        FlipDone::new(Arc::clone(&gate), a).complete(999_999_000_000, None);
        gate.arm(None, ev(2), false);
        expect(&q, &[(1, 0), (2, 0)]);
        gate.shutdown();
        t.join().unwrap();
    }

    #[test]
    fn a_completion_with_no_latch_time_is_stamped_now() {
        let (gate, q, t) = harness();
        let a = gate.arm(Some(1), ev(1), true);
        gate.published(a);
        FlipDone::new(Arc::clone(&gate), a).complete(0, None); // never presented
        expect(&q, &[(1, 0)]);
        gate.shutdown();
        t.join().unwrap();
    }

    #[test]
    fn teardown_with_queued_commits_returns_at_once() {
        let (gate, _q, t) = harness();
        gate.arm(Some(1), ev(1), true); // never published, never completed
        let started = Instant::now();
        gate.shutdown();
        t.join().unwrap();
        assert!(started.elapsed() < FLIP_VALVE);
    }

    #[test]
    fn stamps_never_go_back_after_an_immediate_claim() {
        let (gate, q, t) = harness();
        // An immediate event first (its claim is the clock: 1000 s).
        gate.arm(None, ev(1), false);
        expect(&q, &[(1, 0)]);
        // A latch earlier than that stamp: clamped up to it, not behind.
        let b = gate.arm(Some(2), ev(2), true);
        gate.published(b);
        FlipDone::new(Arc::clone(&gate), b).complete(999_000_000_000, None);
        expect(&q, &[(1, 0), (2, 0)]);
        assert!(q.lock().unwrap().iter().all(|e| e.tv_sec == 1000));
        gate.shutdown();
        t.join().unwrap();
    }

    #[test]
    fn an_event_waits_the_release_fence_of_the_previous_completion() {
        let (gate, q, t) = harness();
        // Commit 1 completes with the fence of the buffer it replaced: the
        // first commit has no previous fence, so it leaves at once.
        let pending = crate::new_eventfd(0).unwrap();
        let probe = crate::dup_fd(pending, 0x02).unwrap();
        let a = gate.arm(Some(1), ev(1), true);
        gate.published(a);
        FlipDone::new(Arc::clone(&gate), a).complete(0, Some(pending));
        expect(&q, &[(1, 0)]);
        // Commit 2 waits commit 1's fence (the buffer flip 1 took off screen).
        let b = gate.arm(Some(2), ev(2), true);
        gate.published(b);
        FlipDone::new(Arc::clone(&gate), b).complete(0, None);
        std::thread::sleep(Duration::from_millis(15));
        assert_eq!(q.lock().unwrap().len(), 1, "held for the previous release");
        crate::write_eventfd(probe);
        expect(&q, &[(1, 0), (2, 0)]);
        close_fd(probe);
        gate.shutdown();
        t.join().unwrap();
    }

    #[test]
    fn an_already_signalled_previous_fence_costs_nothing() {
        let (gate, q, t) = harness();
        let a = gate.arm(Some(1), ev(1), true);
        gate.published(a);
        FlipDone::new(Arc::clone(&gate), a).complete(0, crate::new_eventfd(1));
        let b = gate.arm(Some(2), ev(2), true);
        gate.published(b);
        let started = Instant::now();
        FlipDone::new(Arc::clone(&gate), b).complete(0, None);
        expect(&q, &[(1, 0), (2, 0)]);
        assert!(started.elapsed() < Duration::from_millis(40));
        gate.shutdown();
        t.join().unwrap();
    }
}
