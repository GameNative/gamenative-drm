//! `VK_KHR_present_id` / `VK_KHR_present_wait` served by the shim.
//!
//! The guest's swapchain is the shim's own, so the present id never reaches the
//! driver: the shim records the id the app chains in `VkPresentIdKHR`, tells the
//! daemon which buffer carries it, and completes the wait when the daemon reports
//! that the transaction presenting that buffer latched. The daemon samples the
//! latch time for its vsync grid already, so the answer is the panel's, not a
//! guess made from the release fence.
//!
//! On gamescope's override route the answer is gamescope's own presentation
//! feedback, as in Mesa's Wayland WSI: the shim requests a
//! `wp_presentation_feedback` with every commit that carries a wait, and the
//! wait is answered on `presented` (at the presentation time it reports) or on
//! `discarded` (the frame will never be shown under that id). While a feedback
//! is pending for an id, the compositor's release and the two-period grace do
//! not answer it: they are the fallback for a compositor without the global,
//! and answering from them is a vblank late under FIFO, which a guest waiting
//! on every frame turns into two vblanks per frame.
//!
//! Only ids that were armed — seen in a present — are tracked, and the lists are
//! bounded: a guest that marks frames and never waits cannot grow them.

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PRESENT_ID_FEATURES_KHR`
/// (`VK_KHR_present_id`, extension 295, offset 0).
pub const STYPE_PRESENT_ID_FEATURES: i32 = 1_000_294_001;
/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PRESENT_WAIT_FEATURES_KHR`
/// (`VK_KHR_present_wait`, extension 249, offset 0).
pub const STYPE_PRESENT_WAIT_FEATURES: i32 = 1_000_248_000;
/// `VK_STRUCTURE_TYPE_PRESENT_ID_KHR`: chained in `VkPresentInfoKHR`.
pub const STYPE_PRESENT_ID: i32 = 1_000_294_000;

/// The name the guest sees; served here, not by the driver.
pub const EXT_PRESENT_ID: &core::ffi::CStr = c"VK_KHR_present_id";
/// The name the guest sees; served here, not by the driver.
pub const EXT_PRESENT_WAIT: &core::ffi::CStr = c"VK_KHR_present_wait";

/// Every present id armed by a present and not yet completed, oldest first, and
/// the completions the daemon has reported and no wait has consumed.
struct Waits {
    armed: Vec<(u64, usize)>,
    done: Vec<(u64, u64)>,
    /// Ids whose frame has been committed, with the commit time: the latest a
    /// wait on them is answered is two refresh periods after that.
    committed: Vec<(u64, u64)>,
    /// Ids whose commit asked gamescope for presentation feedback that has not
    /// arrived: only `presented` / `discarded` answers them (or, should it
    /// never come, [`FEEDBACK_SAFETY_NS`] after the commit).
    feedback: Vec<u64>,
}

/// Bound on both lists: the ids in flight are a handful of frames, so anything
/// older is a guest that marked frames and never waited.
const CAP: usize = 64;

static WAITS: Mutex<Waits> = Mutex::new(Waits {
    armed: Vec::new(),
    done: Vec::new(),
    committed: Vec::new(),
    feedback: Vec::new(),
});

/// How long after its commit an id whose feedback never came is answered
/// anyway: far beyond any presentation gamescope makes, so it never answers
/// early, and well inside the guest's one-second bound.
const FEEDBACK_SAFETY_NS: u64 = 100_000_000;

/// Presentation feedback outcomes, and waits answered by a fallback (release,
/// grace or the feedback safety), for the shim's ungated census.
static PRESENTED_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static DISCARDED_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static FALLBACK_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `(presented, discarded, answered by a fallback)` so far.
pub(crate) fn feedback_counts() -> (usize, usize, usize) {
    (
        PRESENTED_N.load(Ordering::Relaxed),
        DISCARDED_N.load(Ordering::Relaxed),
        FALLBACK_N.load(Ordering::Relaxed),
    )
}

/// The commit carrying `id` asked gamescope for presentation feedback. Called
/// before that commit is sent, so no release or grace can answer it first.
pub(crate) fn expect_feedback(id: u64) {
    if id == 0 {
        return;
    }
    let mut w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    if w.feedback.len() >= CAP {
        w.feedback.remove(0);
    }
    w.feedback.push(id);
}

/// gamescope's `wp_presentation_feedback.presented` for the commit carrying
/// `id`, at `present_ns`.
pub(crate) fn feedback_presented(id: u64, present_ns: u64) {
    PRESENTED_N.fetch_add(1, Ordering::Relaxed);
    feedback_done(id, present_ns);
}

/// gamescope's `wp_presentation_feedback.discarded` for the commit carrying
/// `id`: it will never be shown, and its wait is answered now.
pub(crate) fn feedback_discarded(id: u64, now_ns: u64) {
    DISCARDED_N.fetch_add(1, Ordering::Relaxed);
    feedback_done(id, now_ns);
}

/// The feedback for `id` could not be requested after all: the release and the
/// grace answer it again.
pub(crate) fn cancel_feedback(id: u64) {
    if id == 0 {
        return;
    }
    let mut w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    w.feedback.retain(|&f| f != id);
}

fn feedback_done(id: u64, ns: u64) {
    {
        let mut w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
        w.feedback.retain(|&f| f != id);
    }
    record(id, ns);
}

fn feedback_pending(w: &Waits, id: u64) -> bool {
    w.feedback.contains(&id)
}

/// Two refresh periods after a commit, the frame has been latched whatever the
/// compositor does with the buffer afterwards.
fn commit_grace_ns() -> u64 {
    let hz = crate::lsfg::refresh_hz().max(30) as u64;
    2 * (1_000_000_000 / hz)
}

/// The frame carrying `id` has been committed to the compositor.
pub(crate) fn note_commit(id: u64, commit_ns: u64) {
    if id == 0 {
        return;
    }
    let mut w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    if w.committed.len() >= CAP {
        w.committed.remove(0);
    }
    w.committed.push((id, commit_ns));
    drop(w);
    WAIT_CV.notify_all();
}

static WAIT_CV: Condvar = Condvar::new();

/// The frame carrying `id` has been committed to the compositor. With
/// `answer_now`, a wait on it is answered at this commit; otherwise it is
/// answered by the compositor's release or, at the latest, two refresh periods
/// after the commit.
///
/// `answer_now` is for swapchains whose effective present mode is mailbox or
/// immediate. The guest may believe it has FIFO (the shim applies
/// `MESA_VK_WSI_PRESENT_MODE` behind its back) and wait on every frame, while a
/// guest that knew its real mode would not wait at all: DXVK skips the present
/// wait for mailbox and immediate. Holding the wait for the grace there throttles
/// the guest's frame-latency loop to `(start-to-commit + 2 periods) / (L + 1)`.
pub(crate) fn committed(id: u64, commit_ns: u64, answer_now: bool) {
    if answer_now {
        record(id, commit_ns);
    } else {
        note_commit(id, commit_ns);
    }
}

/// Whether the guest enabled each feature. The shim serves both, so the answer
/// is whichever the guest asked for at device creation.
static ID_ENABLED: AtomicBool = AtomicBool::new(false);
static WAIT_ENABLED: AtomicBool = AtomicBool::new(false);

/// Note the features the guest chained at `vkCreateDevice`, before the structs
/// are stripped from the driver's chain.
pub(crate) fn note_enabled(id: bool, wait: bool) {
    ID_ENABLED.store(id, Ordering::Relaxed);
    WAIT_ENABLED.store(wait, Ordering::Relaxed);
}

/// Whether the shim should advertise the extensions and serve the entry points:
/// true once the guest enabled either feature. A guest that never asks sees
/// exactly the device it saw before.
#[must_use]
pub(crate) fn enabled() -> bool {
    ID_ENABLED.load(Ordering::Relaxed) || WAIT_ENABLED.load(Ordering::Relaxed)
}

/// Arm a present id: it is in flight for `sc` and a wait on it must block until
/// the daemon reports its latch.
pub(crate) fn arm(id: u64, sc: usize) {
    if id == 0 {
        return;
    }
    let mut w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    if w.armed.len() >= CAP {
        w.armed.remove(0);
    }
    w.armed.push((id, sc));
}

/// A completed present: presentation feedback, the daemon's latch report, a
/// commit under mailbox, or a fallback. An id no longer armed was answered
/// already and is left alone, so a second answer never queues a stale one.
pub(crate) fn record(id: u64, latch_ns: u64) {
    if id == 0 {
        return;
    }
    let mut w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(i) = w.armed.iter().position(|&(armed, _)| armed == id) else {
        return;
    };
    w.armed.remove(i);
    w.committed.retain(|&(c, _)| c != id);
    w.feedback.retain(|&f| f != id);
    if w.done.len() >= CAP {
        w.done.remove(0);
    }
    w.done.push((id, latch_ns));
    drop(w);
    WAIT_CV.notify_all();
}

/// The first few wait outcomes, so a log shows whether present waits are being
/// answered by the daemon, falling through as unarmed, or hitting the bound.
fn note_outcome(id: u64, what: &str, t0: std::time::Instant) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    if n < 8 || n.is_multiple_of(600) {
        crate::logf!(gndrm_obf::obf!(
            "present wait #{}: id={} {} after {} us",
            n,
            id,
            what,
            t0.elapsed().as_micros()
        ));
    }
}

/// The compositor released the frame's buffer: it has been taken into the
/// output, and the next refresh shows it. Answers the wait when no latch report
/// came first; a daemon that did report is left as the authority.
///
/// Not while presentation feedback is pending for it: under FIFO the release of
/// a frame comes when the next one is latched, a vblank after it was shown.
pub(crate) fn complete_on_release(id: u64, release_ns: u64) {
    let answers = {
        let w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
        !feedback_pending(&w, id) && w.armed.iter().any(|&(armed, _)| armed == id)
    };
    if answers {
        FALLBACK_N.fetch_add(1, Ordering::Relaxed);
        record(id, release_ns);
    }
}

/// The frame carrying `id` will never reach the panel under that id: a newer
/// frame replaced it in the mailbox, or its slot was presented again before it
/// was. A wait on it is answered now (`wp_presentation`'s "discarded"), so the
/// guest does not sit out the bound in `wait`. An id no longer armed was
/// answered already and is left alone.
pub(crate) fn discard(id: u64, ns: u64) {
    if id == 0 {
        return;
    }
    let armed = {
        let w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
        w.armed.iter().any(|&(armed, _)| armed == id)
    };
    if armed {
        record(id, ns);
    }
}

/// The swapchain is going away: nothing armed on it will be released or
/// latched, so its ids are dropped and every waiter is woken. An id no longer
/// armed is answered as presented, which is the only answer left to give.
pub(crate) fn forget_swapchain(sc: usize) {
    let mut w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    let before = w.armed.len();
    w.armed.retain(|&(_, owner)| owner != sc);
    let dropped = before - w.armed.len();
    drop(w);
    if dropped != 0 {
        WAIT_CV.notify_all();
    }
}

/// `vkWaitForPresentKHR`: block until `id` has been presented, or `timeout_ns`
/// elapses (`u64::MAX` waits forever). An id that was never armed cannot arrive
/// and would hang the caller, so it is answered as presented — the guest cannot
/// wait on a frame it never submitted.
pub(crate) fn wait(id: u64, sc: usize, timeout_ns: u64) -> bool {
    // An infinite wait is the guest's default (`UINT64_MAX`), but the answer
    // comes from the daemon: a daemon too old to know the mark would never send
    // one, and the guest would hang for the session. A one-second bound keeps a
    // mismatched pair degrading to "no wait" instead. `VK_TIMEOUT` is positive,
    // so a caller that treats negative results as errors carries on.
    const MAX_WAIT_NS: u64 = 1_000_000_000;
    let bound = if timeout_ns == u64::MAX { MAX_WAIT_NS } else { timeout_ns };
    let deadline = Some(std::time::Instant::now() + Duration::from_nanos(bound));
    let t0 = std::time::Instant::now();
    let mut w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(i) = w.done.iter().position(|&(done, _)| done == id) {
            w.done.remove(i);
            note_outcome(id, "done", t0);
            return true;
        }
        if !w.armed.iter().any(|&(armed, owner)| armed == id && owner == sc) {
            note_outcome(id, "unarmed", t0);
            return true;
        }
        // A committed frame counts as latched two refresh periods after its
        // commit, whether or not its buffer ever comes back; one whose
        // presentation feedback is pending only at the feedback safety.
        let grace = if feedback_pending(&w, id) {
            FEEDBACK_SAFETY_NS
        } else {
            commit_grace_ns()
        };
        let latch_by = w
            .committed
            .iter()
            .find(|&&(c, _)| c == id)
            .map(|&(_, ns)| ns.saturating_add(grace));
        if let Some(latch_ns) = latch_by {
            let now_ns = crate::proxy::monotonic_ns();
            if now_ns >= latch_ns {
                drop(w);
                FALLBACK_N.fetch_add(1, Ordering::Relaxed);
                record(id, latch_ns);
                let mut w2 = WAITS.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(i) = w2.done.iter().position(|&(done, _)| done == id) {
                    w2.done.remove(i);
                }
                note_outcome(id, "latched", t0);
                return true;
            }
        }
        w = match deadline {
            Some(d) => {
                let now = std::time::Instant::now();
                if now >= d {
                    note_outcome(id, "timeout", t0);
                    return false;
                }
                let mut slice = d - now;
                if let Some(latch_ns) = latch_by {
                    let left = latch_ns.saturating_sub(crate::proxy::monotonic_ns());
                    slice = slice.min(Duration::from_nanos(left.max(1)));
                }
                WAIT_CV
                    .wait_timeout(w, slice)
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
            None => WAIT_CV.wait(w).unwrap_or_else(|e| e.into_inner()),
        };
    }
}

/// `VkPresentInfoKHR`'s chain walk: the present id for swapchain `index`, or 0
/// when the guest chained none.
///
/// # Safety
/// `p_next` must be the `pNext` of a live `VkPresentInfoKHR`, and `index` below
/// its `swapchainCount`.
pub(crate) unsafe fn present_id(p_next: *const c_void, index: usize) -> u64 {
    if p_next.is_null() {
        return 0;
    }
    /// The two leading members every Vulkan structure shares.
    #[repr(C)]
    struct Head {
        s_type: i32,
        p_next: *const c_void,
    }
    /// `VkPresentIdKHR`.
    #[repr(C)]
    struct PresentId {
        s_type: i32,
        p_next: *const c_void,
        swapchain_count: u32,
        present_ids: *const u64,
    }

    let mut head = p_next.cast::<Head>();
    // Bounded: a malformed chain must not walk forever.
    for _ in 0..64 {
        if head.is_null() {
            return 0;
        }
        // SAFETY: `head` is a structure in the guest's chain; only its head is
        // read before its type is known.
        let node = unsafe { &*head };
        if node.s_type == STYPE_PRESENT_ID {
            let p = head.cast::<PresentId>();
            // SAFETY: the structure declares this type, so it has this layout.
            let id = unsafe { &*p };
            if (index as u32) >= id.swapchain_count || id.present_ids.is_null() {
                return 0;
            }
            // SAFETY: the count bounds the array the guest supplied.
            return unsafe { *id.present_ids.add(index) };
        }
        head = node.p_next.cast::<Head>();
    }
    0
}

/// Patch the two feature structs a guest chains into `vkGetPhysicalDeviceFeatures2`.
///
/// The driver's answer is meaningless for a swapchain it does not own, so the
/// shim reports what it serves.
///
/// # Safety
/// `p_next` must be the chain head of a `VkPhysicalDeviceFeatures2` the shim is
/// about to return to the guest.
pub(crate) unsafe fn features2(p_next: *mut c_void) {
    /// The two leading members every Vulkan structure shares.
    #[repr(C)]
    struct Head {
        s_type: i32,
        p_next: *mut c_void,
    }
    /// `VkPhysicalDevicePresentIdFeaturesKHR` / `...PresentWaitFeaturesKHR`.
    #[repr(C)]
    struct Feature {
        s_type: i32,
        p_next: *mut c_void,
        present: u32,
    }

    let mut head = p_next.cast::<Head>();
    for _ in 0..64 {
        if head.is_null() {
            return;
        }
        // SAFETY: `head` is a structure in the guest's chain.
        let node = unsafe { &*head };
        if node.s_type == STYPE_PRESENT_ID_FEATURES || node.s_type == STYPE_PRESENT_WAIT_FEATURES {
            // SAFETY: the structure declares this type, so it has this layout.
            unsafe { (*head.cast::<Feature>()).present = 1 };
        }
        head = node.p_next.cast::<Head>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_without_ids_reads_zero() {
        // SAFETY: a null chain is the documented "no ids" case.
        assert_eq!(unsafe { present_id(core::ptr::null(), 0) }, 0);
    }

    #[test]
    fn ids_are_read_per_swapchain_and_bounded_by_the_count() {
        let ids = [41u64, 42];
        let mut node = PresentIdExt {
            s_type: STYPE_PRESENT_ID,
            p_next: core::ptr::null(),
            swapchain_count: 2,
            present_ids: ids.as_ptr(),
        };
        // SAFETY: `node` is a live structure of the declared type.
        let p = (&mut node as *mut PresentIdExt).cast::<c_void>();
        assert_eq!(unsafe { present_id(p, 0) }, 41);
        assert_eq!(unsafe { present_id(p, 1) }, 42);
        // An index past the count reads nothing.
        assert_eq!(unsafe { present_id(p, 2) }, 0);
    }

    #[repr(C)]
    struct PresentIdExt {
        s_type: i32,
        p_next: *const c_void,
        swapchain_count: u32,
        present_ids: *const u64,
    }

    #[test]
    fn a_wait_for_an_unarmed_id_returns_at_once() {
        assert!(wait(9_999_999, 1, 0));
    }

    #[test]
    fn an_armed_id_completes_only_on_the_daemons_report() {
        arm(7, 3);
        // Nothing reported yet, so a zero timeout expires.
        assert!(!wait(7, 3, 0));
        record(7, 1234);
        assert!(wait(7, 3, 0));
        // Consumed once: the second wait finds no armed id and returns.
        assert!(wait(7, 3, 0));
    }

    #[test]
    fn a_fifo_commit_is_answered_only_after_the_grace() {
        arm(101, 5);
        committed(101, crate::proxy::monotonic_ns(), false);
        // The grace (two refresh periods, at least 2 ms at any rate the shim
        // accepts) has not passed, so a zero timeout expires.
        assert!(!wait(101, 5, 0));
        // A commit far enough in the past is answered as latched.
        arm(102, 5);
        committed(102, crate::proxy::monotonic_ns().saturating_sub(1_000_000_000), false);
        assert!(wait(102, 5, 0));
        record(101, 0);
        assert!(wait(101, 5, 0));
    }

    #[test]
    fn a_mailbox_commit_is_answered_at_the_commit() {
        arm(201, 6);
        committed(201, crate::proxy::monotonic_ns(), true);
        assert!(wait(201, 6, 0));
        // Answered once; nothing is left armed for the release to answer again.
        assert!(!WAITS.lock().unwrap().armed.iter().any(|&(id, _)| id == 201));
        assert!(wait(201, 6, 0));
    }

    #[test]
    fn presentation_feedback_answers_and_the_release_does_not_preempt_it() {
        arm(401, 8);
        expect_feedback(401);
        committed(401, crate::proxy::monotonic_ns().saturating_sub(50_000_000), false);
        // The release comes first (the next frame was latched): no answer.
        complete_on_release(401, crate::proxy::monotonic_ns());
        // Nor does the two-period grace, 50 ms after the commit.
        assert!(!wait(401, 8, 0));
        feedback_presented(401, 777);
        assert!(wait(401, 8, 0));
        // A late duplicate from another path leaves nothing behind.
        record(401, 1);
        assert!(!WAITS.lock().unwrap().done.iter().any(|&(id, _)| id == 401));
    }

    #[test]
    fn a_discarded_frame_is_answered() {
        arm(501, 9);
        expect_feedback(501);
        committed(501, crate::proxy::monotonic_ns(), false);
        feedback_discarded(501, crate::proxy::monotonic_ns());
        assert!(wait(501, 9, 0));
    }

    #[test]
    fn a_feedback_that_never_comes_is_answered_by_the_safety() {
        arm(601, 10);
        expect_feedback(601);
        committed(601, crate::proxy::monotonic_ns().saturating_sub(FEEDBACK_SAFETY_NS + 1), false);
        assert!(wait(601, 10, 0));
    }

    #[test]
    fn without_feedback_the_release_still_answers() {
        arm(701, 11);
        committed(701, crate::proxy::monotonic_ns(), false);
        complete_on_release(701, crate::proxy::monotonic_ns());
        assert!(wait(701, 11, 0));
    }

    #[test]
    fn a_replaced_frame_is_answered_without_a_commit() {
        // The mailbox drops a frame before it is ever committed: recording it
        // is what releases a guest waiting on it before the one-second bound.
        arm(301, 7);
        discard(301, crate::proxy::monotonic_ns());
        let t0 = std::time::Instant::now();
        assert!(wait(301, 7, u64::MAX));
        assert!(t0.elapsed() < Duration::from_millis(100));
        // A second discard of an answered id leaves nothing for a later wait.
        discard(301, 0);
        assert!(!WAITS.lock().unwrap().done.iter().any(|&(id, _)| id == 301));
    }
}
