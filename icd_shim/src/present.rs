//! `VK_KHR_present_id` / `VK_KHR_present_wait` served by the shim.
//!
//! The guest's swapchain is the shim's own, so the present id never reaches the
//! driver: the shim records the id the app chains in `VkPresentIdKHR`, tells the
//! daemon which buffer carries it, and completes the wait when the daemon reports
//! that the transaction presenting that buffer latched. The daemon samples the
//! latch time for its vsync grid already, so the answer is the panel's, not a
//! guess made from the release fence.
//!
//! Only ids that were armed — seen in a present — are tracked, and both lists are
//! bounded: a guest that marks frames and never waits cannot grow them.

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PRESENT_ID_FEATURES_KHR`
/// (`VK_KHR_present_id`, extension 295, offset 0).
pub const STYPE_PRESENT_ID_FEATURES: i32 = 1_000_294_000;
/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PRESENT_WAIT_FEATURES_KHR`
/// (`VK_KHR_present_wait`, extension 249, offset 0).
pub const STYPE_PRESENT_WAIT_FEATURES: i32 = 1_000_248_000;
/// `VK_STRUCTURE_TYPE_PRESENT_ID_KHR`: chained in `VkPresentInfoKHR`.
pub const STYPE_PRESENT_ID: i32 = 1_000_294_001;

/// The name the guest sees; served here, not by the driver.
pub const EXT_PRESENT_ID: &core::ffi::CStr = c"VK_KHR_present_id";
/// The name the guest sees; served here, not by the driver.
pub const EXT_PRESENT_WAIT: &core::ffi::CStr = c"VK_KHR_present_wait";

/// Every present id armed by a present and not yet completed, oldest first, and
/// the completions the daemon has reported and no wait has consumed.
struct Waits {
    armed: Vec<(u64, usize)>,
    done: Vec<(u64, u64)>,
}

/// Bound on both lists: the ids in flight are a handful of frames, so anything
/// older is a guest that marked frames and never waited.
const CAP: usize = 64;

static WAITS: Mutex<Waits> = Mutex::new(Waits {
    armed: Vec::new(),
    done: Vec::new(),
});

static WAIT_CV: Condvar = Condvar::new();

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

/// A completed present, reported by the daemon with the panel's latch time.
pub(crate) fn record(id: u64, latch_ns: u64) {
    if id == 0 {
        return;
    }
    let mut w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = w.armed.iter().position(|&(armed, _)| armed == id) {
        w.armed.remove(i);
    }
    if w.done.len() >= CAP {
        w.done.remove(0);
    }
    w.done.push((id, latch_ns));
    drop(w);
    WAIT_CV.notify_all();
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
    let mut w = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(i) = w.done.iter().position(|&(done, _)| done == id) {
            w.done.remove(i);
            return true;
        }
        if !w.armed.iter().any(|&(armed, owner)| armed == id && owner == sc) {
            return true;
        }
        w = match deadline {
            Some(d) => {
                let now = std::time::Instant::now();
                if now >= d {
                    return false;
                }
                WAIT_CV
                    .wait_timeout(w, d - now)
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
}
