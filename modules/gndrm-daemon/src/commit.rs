//! `DRM_IOCTL_MODE_ATOMIC`: linearized request parsing, validation, and the
//! commit→present→event flow (doc `02` §5/§6, tasks A6/A7).
//!
//! ## Wire layout (extension after the fixed `DrmModeAtomic` struct)
//!
//! ```text
//! u32 objs[count_objs]               object ids, in commit order
//! u32 counts[count_objs]             per-object property counts
//! u32 prop_ids[sum(counts)]          property ids, flattened
//! u64 values  [sum(counts)]          property values, flattened
//! ```
//!
//! `IN_FENCE_FD` semantics: a plane value of `-1` (`u64::MAX`) means "no
//! fence" (kernel convention). Any other value means exactly one fence fd
//! rides this frame via `SCM_RIGHTS` (phase-1 contract: at most one fence per
//! commit — gamescope only ever fences the primary plane).

use std::os::fd::RawFd;

use gndrm_uapi::drm::*;
use gndrm_uapi::mode::*;
use gndrm_uapi::wire::errno;

use crate::objects::{CONNECTOR_ID, CRTC_ID, PLANE_CURSOR_ID, PLANE_PRIMARY_ID};
use crate::pod::*;
use crate::{props, Device, DispatchResult};

/// One parsed `(obj, prop, value)` triple.
#[derive(Debug, Clone, Copy)]
struct PropSet {
    obj_id: u32,
    prop_index: usize,
    value: u64,
}

/// Parsed + validated commit, ready to apply.
#[derive(Debug)]
struct ParsedCommit {
    sets: Vec<PropSet>,
    flags: u32,
    user_data: u64,
    wants_fence: bool,
}

impl Device {
    pub(crate) fn ioctl_atomic(&mut self, payload: &[u8], fd_in: Option<RawFd>) -> DispatchResult {
        let Some(a) = pod_read::<DrmModeAtomic>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        // Unknown flag bits are a hard error, like the kernel.
        const KNOWN: u32 = DRM_MODE_ATOMIC_TEST_ONLY
            | DRM_MODE_ATOMIC_NONBLOCK
            | DRM_MODE_ATOMIC_ALLOW_MODESET
            | DRM_MODE_PAGE_FLIP_EVENT;
        if a.flags & !KNOWN != 0 || a.reserved != 0 {
            return DispatchResult::err(errno::EINVAL);
        }
        // Events without NONBLOCK would deadlock a blocking commit path; we
        // emulate commits synchronously, so both orders are safe — but reject
        // the combination the kernel rejects (event without ALLOW_MODESET is
        // fine; TEST_ONLY with event is not).
        if a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 && a.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            return DispatchResult::err(errno::EINVAL);
        }

        let parsed = match Self::parse_atomic(&a, payload) {
            Ok(p) => p,
            Err(e) => return e,
        };
        if parsed.wants_fence != fd_in.is_some() {
            // Fence promised but not delivered (or an unsolicited fd).
            return DispatchResult::err(errno::EINVAL);
        }

        if let Err(e) = self.validate_commit(&parsed) {
            return DispatchResult::err(e);
        }
        if a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 {
            return DispatchResult::ok(Vec::new());
        }

        // Apply.
        let mut result = DispatchResult::ok(Vec::new());
        self.apply_commit(&parsed, fd_in, &mut result);
        result
    }

    /// Decode the linearized extension into `(obj, prop, value)` triples.
    fn parse_atomic(a: &DrmModeAtomic, payload: &[u8]) -> Result<ParsedCommit, DispatchResult> {
        let mut off = core::mem::size_of::<DrmModeAtomic>();
        let n = a.count_objs as usize;
        // Kernel caps the object count; mirror with a sane bound.
        if n == 0 || n > 64 {
            return Err(DispatchResult::err(errno::EINVAL));
        }
        let mut objs = Vec::with_capacity(n);
        for _ in 0..n {
            let Some(id) = take_u32(payload, &mut off) else {
                return Err(DispatchResult::err(errno::EINVAL));
            };
            objs.push(id);
        }
        let mut counts = Vec::with_capacity(n);
        for _ in 0..n {
            let Some(c) = take_u32(payload, &mut off) else {
                return Err(DispatchResult::err(errno::EINVAL));
            };
            if c > 64 {
                return Err(DispatchResult::err(errno::EINVAL));
            }
            counts.push(c);
        }
        // Two flat arrays: all prop ids, then all values (wire doc above).
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        let mut prop_ids = Vec::with_capacity(total);
        for _ in 0..total {
            let Some(p) = take_u32(payload, &mut off) else {
                return Err(DispatchResult::err(errno::EINVAL));
            };
            prop_ids.push(p);
        }
        let mut values = Vec::with_capacity(total);
        for _ in 0..total {
            let Some(v) = take_u64(payload, &mut off) else {
                return Err(DispatchResult::err(errno::EINVAL));
            };
            values.push(v);
        }
        let mut sets = Vec::with_capacity(total);
        let mut wants_fence = false;
        let mut flat = 0;
        for (i, &obj_id) in objs.iter().enumerate() {
            for _ in 0..counts[i] {
                let prop_id = prop_ids[flat];
                let value = values[flat];
                flat += 1;
                let Some(prop_index) = props::resolve(obj_id, prop_id) else {
                    return Err(DispatchResult::err(errno::ENOENT));
                };
                if prop_index == props::P_PLANE_IN_FENCE_FD && value != u64::from(u32::MAX) {
                    wants_fence = true;
                }
                sets.push(PropSet {
                    obj_id,
                    prop_index,
                    value,
                });
            }
        }
        Ok(ParsedCommit {
            sets,
            flags: a.flags,
            user_data: a.user_data,
            wants_fence,
        })
    }

    /// Everything the kernel would check before touching state.
    fn validate_commit(&mut self, commit: &ParsedCommit) -> Result<(), i32> {
        // Immutable props can never be set.
        for s in &commit.sets {
            let (_, _, immutable) = props::PROPS[s.prop_index];
            if immutable {
                return Err(errno::EPERM);
            }
        }
        // MODESET-required changes need ALLOW_MODESET.
        let modeset = commit.sets.iter().any(|s| {
            matches!(
                s.prop_index,
                props::P_CRTC_MODE_ID | props::P_CRTC_ACTIVE | props::P_CONN_CRTC_ID
            )
        });
        if modeset && commit.flags & DRM_MODE_ATOMIC_ALLOW_MODESET == 0 {
            return Err(errno::EINVAL);
        }
        // Referenced objects must exist.
        for s in &commit.sets {
            match s.prop_index {
                props::P_PLANE_FB_ID => {
                    if s.value != 0 && !self.stores().fbs.contains_key(&(s.value as u32)) {
                        return Err(errno::ENOENT);
                    }
                }
                props::P_CRTC_MODE_ID => {
                    if s.value != 0 && !self.stores().blobs.contains_key(&(s.value as u32)) {
                        return Err(errno::ENOENT);
                    }
                }
                props::P_PLANE_CRTC_ID | props::P_CONN_CRTC_ID => {
                    if s.value != 0 && s.value != u64::from(CRTC_ID) {
                        return Err(errno::ENOENT);
                    }
                }
                props::P_CRTC_ACTIVE if s.value > 1 => return Err(errno::EINVAL),
                _ => {}
            }
        }
        // A plane with an FB needs a CRTC; check post-commit state by
        // simulating onto copies.
        let mut primary = self.primary_state();
        let mut cursor = self.cursor_state();
        for s in &commit.sets {
            let plane = match s.obj_id {
                PLANE_PRIMARY_ID => &mut primary,
                PLANE_CURSOR_ID => &mut cursor,
                _ => continue,
            };
            apply_plane_prop_pub(plane, s.prop_index, s.value);
        }
        for plane in [&primary, &cursor] {
            if plane.fb_id != 0 && plane.crtc_id != CRTC_ID {
                return Err(errno::EINVAL);
            }
        }
        Ok(())
    }

    fn apply_commit(
        &mut self,
        commit: &ParsedCommit,
        fence_fd: Option<RawFd>,
        result: &mut DispatchResult,
    ) {
        let old_primary_fb = self.primary_state().fb_id;

        for s in &commit.sets {
            match s.obj_id {
                CONNECTOR_ID => {
                    if s.prop_index == props::P_CONN_CRTC_ID {
                        self.set_conn_crtc(s.value as u32);
                    }
                }
                CRTC_ID => match s.prop_index {
                    props::P_CRTC_ACTIVE => self.set_crtc_active(s.value != 0),
                    props::P_CRTC_MODE_ID => self.set_crtc_mode(s.value as u32),
                    _ => {}
                },
                PLANE_PRIMARY_ID | PLANE_CURSOR_ID => {
                    let idx = usize::from(s.obj_id == PLANE_CURSOR_ID);
                    self.apply_plane(idx, s.prop_index, s.value);
                }
                _ => {}
            }
        }
        let crtc_on = self.crtc_state().active;

        // Consume the fence: the buffer is ready to scan out. The real
        // presenter will wait/poll it; phase-1 semantics: ownership moves to
        // the daemon and we release it once the frame is presented.
        if let Some(fd) = fence_fd {
            close_fence_after_present(fd);
        }

        // Present when the primary FB changed under an active CRTC.
        let new_fb = self.primary_state().fb_id;
        if crtc_on && new_fb != 0 && new_fb != old_primary_fb {
            self.present_primary(new_fb);
        }

        if commit.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            let (sec, usec) = self.now();
            let seq = self.next_flip_sequence();
            result.events.push(DrmEventVblank {
                base: DrmEvent {
                    ty: DRM_EVENT_FLIP_COMPLETE,
                    length: core::mem::size_of::<DrmEventVblank>() as u32,
                },
                user_data: commit.user_data,
                tv_sec: sec,
                tv_usec: usec,
                sequence: seq,
                crtc_id: CRTC_ID,
            });
        }
    }
}

/// Apply one property to a plane state (shared by validate/apply paths).
pub(crate) fn apply_plane_prop_pub(
    plane: &mut crate::objects::PlaneState,
    index: usize,
    value: u64,
) {
    use props::*;
    match index {
        P_PLANE_FB_ID => plane.fb_id = value as u32,
        P_PLANE_CRTC_ID => plane.crtc_id = value as u32,
        P_PLANE_SRC_X => plane.src_x = value as u32,
        P_PLANE_SRC_Y => plane.src_y = value as u32,
        P_PLANE_SRC_W => plane.src_w = value as u32,
        P_PLANE_SRC_H => plane.src_h = value as u32,
        P_PLANE_CRTC_X => plane.crtc_x = value as u32 as i32,
        P_PLANE_CRTC_Y => plane.crtc_y = value as u32 as i32,
        P_PLANE_CRTC_W => plane.crtc_w = value as u32 as i32,
        P_PLANE_CRTC_H => plane.crtc_h = value as u32 as i32,
        P_PLANE_IN_FENCE_FD => {} // consumed, not state
        _ => {}
    }
}

/// Release the acquire fence. Phase 1 (NullPresenter): close immediately.
/// Phase 3: the EGL presenter waits on it before sampling the buffer.
fn close_fence_after_present(fd: RawFd) {
    // SAFETY: fd ownership moved to the daemon by the wire contract.
    unsafe { libc::close(fd) };
}
