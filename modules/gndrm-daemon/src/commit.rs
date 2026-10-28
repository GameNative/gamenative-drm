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

use std::collections::HashSet;
use std::os::fd::RawFd;

use gndrm_uapi::drm::*;
use gndrm_uapi::mode::*;
use gndrm_uapi::wire::errno;

use crate::objects::{PlaneState, CONNECTOR_ID, CRTC_ID, PLANE_CURSOR_ID, PLANE_PRIMARY_ID};
use crate::pod::*;
use crate::{props, CommitJob, Device, DispatchResult};

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
    pub(crate) fn ioctl_atomic(
        &mut self,
        payload: &[u8],
        mut fd_in: Option<RawFd>,
    ) -> DispatchResult {
        let Some(a) = pod_read::<DrmModeAtomic>(payload) else {
            return DispatchResult::err(errno::EINVAL);
        };
        // Unknown flag bits are a hard error, like the kernel. ASYNC is
        // accepted: our present path is asynchronous anyway (commit queues and
        // returns; the worker presents on its own thread), so "present
        // immediately" is the only behaviour we have.
        const KNOWN: u32 = DRM_MODE_ATOMIC_TEST_ONLY
            | DRM_MODE_ATOMIC_NONBLOCK
            | DRM_MODE_ATOMIC_ALLOW_MODESET
            | DRM_MODE_PAGE_FLIP_EVENT
            | DRM_MODE_PAGE_FLIP_ASYNC;
        if a.flags & !KNOWN != 0 || a.reserved != 0 {
            self.diag(|| {
                format!(
                    "{}",
                    gndrm_obf::obf!(
                        "atomic: reject EINVAL unknown flags=0x{:x} reserved=0x{:x}",
                        a.flags,
                        a.reserved
                    )
                )
            });
            return DispatchResult::err(errno::EINVAL);
        }
        // TEST_ONLY with an event is what the kernel rejects (a test commit
        // presents nothing).
        if a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 && a.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            self.diag(|| "atomic: reject EINVAL TEST_ONLY+EVENT".to_string());
            return DispatchResult::err(errno::EINVAL);
        }

        let mut parsed = match self.parse_atomic(&a, payload) {
            Ok(p) => p,
            Err(e) => return e,
        };
        // An always-signalled emulated fence — our own exported sync object,
        // which is a bare eventfd — is already signalled, so as far as the
        // emulated hardware is concerned there is no fence to carry. It must
        // not be forwarded either: it is not a `sync_file`, and SurfaceFlinger
        // logs `sync_file_info returned NULL` when asked to inspect one. Treat
        // it exactly like the `-1` "no fence" sentinel.
        if fd_in.is_some_and(|fd| self.is_emulated_fence(fd)) {
            if let Some(fd) = fd_in {
                crate::close_fd(fd);
            }
            fd_in = None;
            parsed.wants_fence = false;
        }
        if parsed.wants_fence != fd_in.is_some() {
            // Fence promised but not delivered (or an unsolicited fd). Close the
            // unsolicited reference we were given either way.
            if let Some(fd) = fd_in {
                crate::close_fd(fd);
            }
            self.diag(|| {
                format!(
                    "{}",
                    gndrm_obf::obf!(
                        "atomic: reject EINVAL fence mismatch wants={} got={}",
                        parsed.wants_fence,
                        fd_in.is_some()
                    )
                )
            });
            return DispatchResult::err(errno::EINVAL);
        }

        if let Err(e) = self.validate_commit(&parsed) {
            if let Some(fd) = fd_in {
                crate::close_fd(fd);
            }
            return DispatchResult::err(e);
        }
        if a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 {
            // TEST_ONLY validates and stops here: no state change, no queue,
            // no event. The fence was never going to be consumed.
            if let Some(fd) = fd_in {
                crate::close_fd(fd);
            }
            return DispatchResult::ok(Vec::new());
        }
        // A non-blocking commit whose acquire fence has not signalled cannot
        // proceed without stalling: the kernel returns EBUSY rather than block
        // the caller (which is what a blocking commit would be allowed to do).
        if a.flags & DRM_MODE_ATOMIC_NONBLOCK != 0 {
            if let Some(fd) = fd_in {
                if !fence_ready(fd) {
                    crate::close_fd(fd);
                    self.diag(|| {
                        "atomic: reject EBUSY non-blocking commit with unsignalled in-fence"
                            .to_string()
                    });
                    return DispatchResult::err(errno::EBUSY);
                }
            }
        }

        // Apply software state synchronously (queries see it immediately), then
        // queue the present + flip event for the worker.
        let prev_primary_fb = self.primary_state().fb_id;
        self.apply_commit(&parsed, fd_in);
        if !self.logged_first_commit {
            self.logged_first_commit = true;
            self.diag(|| {
                format!(
                    "{}",
                    gndrm_obf::obf!(
                        "atomic: first commit queued (primary fb={}, {} props, flags=0x{:x})",
                        self.primary_state().fb_id,
                        parsed.sets.len(),
                        parsed.flags
                    )
                )
            });
        }
        // Periodic commit geometry: what the client actually puts on the
        // primary plane. First 30 commits in full, then every 120th — this
        // is the ground truth for "stretched/misplaced frame" bugs (run 12:
        // taskbar band at top of a white screen).
        self.commit_count = self.commit_count.wrapping_add(1);
        // Opt-in (`GNDRM_LOG_DAEMON`): a shipped session must not narrate how the
        // presenter works, and the daemon shares the app's address space so no
        // container `GNDRM_LOG_*` can gate this. When it is on, a commit is
        // logged when the plane's state changes rather than once per second of
        // identical state; the first commits and a slow heartbeat keep the
        // startup sequence visible.
        let want_commit_log = crate::verbose_logging();
        let commit_state = {
            let p = self.primary_state();
            (p.fb_id, prev_primary_fb, self.crtc_state().active)
        };
        if want_commit_log
            && (self.commit_count <= 10
                || self.logged_commit_state != Some(commit_state)
                || self.commit_count.is_multiple_of(1200))
        {
            self.logged_commit_state = Some(commit_state);
            let p = self.primary_state();
            let (w, h, fmt, pitch) = self
                .stores
                .fbs
                .get(&p.fb_id)
                .map(|f| (f.width, f.height, f.pixel_format, f.pitches[0]))
                .unwrap_or((0, 0, 0, 0));
            // UNGATED sibling of the line below, on the same rate policy: the
            // commit's primary-fb id and a running commit count in one line, so
            // "commits arrive but the plane is blank (fb=0)" is separable from
            // "no commit arrives at all" (no probe line). The gated line cannot
            // serve this — the daemon runs in the app process, where no
            // `GNDRM_LOG_*` from the container can be set. `prev_fb` is the
            // publish verdict's other half: the daemon publishes only when the
            // primary fb both is non-zero and changed.
            self.always_log(|| {
                format!(
                    "{}",
                    gndrm_obf::obf!(
                        "gndrm-daemon: commit #{} fb={} prev_fb={} crtc_on={} fbs={}",
                        self.commit_count,
                        p.fb_id,
                        prev_primary_fb,
                        self.crtc_state().active,
                        self.stores.fbs.len()
                    )
                )
            });
            self.diag(|| format!("{}", gndrm_obf::obf!(
                "atomic #{}: fb={} {}x{} fmt={:#010x} pitch={} src=({},{}) {}x{} crtc=({},{}) {}x{}",
                self.commit_count,
                p.fb_id,
                w,
                h,
                fmt,
                pitch,
                p.src_x >> 16,
                p.src_y >> 16,
                p.src_w >> 16,
                p.src_h >> 16,
                p.crtc_x,
                p.crtc_y,
                p.crtc_w,
                p.crtc_h,
            )));
        }
        // Plane census: the full plane set, on the same rate policy as the
        // `atomic #N:` line, plus every change at once — a plane that only
        // appears (or turns on/changes fb) late in a session must not be
        // missed by sampling only the first seconds. Diagnostics only, and the
        // one per-commit message built outside `diag`: the gate is tested here
        // so a diagnostics-off daemon builds (and decodes) nothing.
        if self.diag_on() {
            let census = self.plane_census_text();
            let changed = census != self.plane_census;
            if changed {
                self.plane_census = census.clone();
            }
            if changed || self.commit_count <= 30 || self.commit_count.is_multiple_of(120) {
                self.diag(|| {
                    format!(
                        "{}",
                        gndrm_obf::obf!("planes #{}: {}", self.commit_count, census)
                    )
                });
            }
        }
        DispatchResult::ok(Vec::new())
    }

    /// One line describing every KMS plane this device exposes: object id,
    /// type, framebuffer, CRTC, z-order, alpha and the source/CRTC rects.
    /// Inactive planes (`fb_id == 0`) are included so "no plane" is
    /// distinguishable from "a plane the daemon dropped". `zpos`/`alpha` are
    /// not exposed by this device (props.rs), so they print as `-`.
    #[must_use]
    fn plane_census_text(&self) -> String {
        let planes: [(u32, &str, PlaneState); 2] = [
            (PLANE_PRIMARY_ID, "primary", self.primary_state()),
            (PLANE_CURSOR_ID, "cursor", self.cursor_state()),
        ];
        let parts: Vec<String> = planes
            .iter()
            .map(|(id, ty, p)| {
                if p.fb_id == 0 {
                    format!("{}", gndrm_obf::obf!("[{ty} id={id} fb=0 crtc={} zpos=- alpha=- **]", p.crtc_id))
                } else {
                    format!("{}", gndrm_obf::obf!(
                        "[{ty} id={id} fb={} crtc={} zpos=- alpha=- src={}x{}+({},{}) crtc={}x{}+({},{})]",
                        p.fb_id,
                        p.crtc_id,
                        p.src_w >> 16,
                        p.src_h >> 16,
                        p.src_x >> 16,
                        p.src_y >> 16,
                        p.crtc_w,
                        p.crtc_h,
                        p.crtc_x,
                        p.crtc_y,
                    ))
                }
            })
            .collect();
        format!(
            "{}",
            gndrm_obf::obf!("count={} {}", planes.len(), parts.join(" "))
        )
    }

    /// Decode the linearized extension into `(obj, prop, value)` triples.
    fn parse_atomic(
        &self,
        a: &DrmModeAtomic,
        payload: &[u8],
    ) -> Result<ParsedCommit, DispatchResult> {
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
                    self.diag(|| {
                        format!(
                            "{}",
                            gndrm_obf::obf!(
                            "atomic: reject ENOENT unresolved prop obj={obj_id} prop=0x{prop_id:x}"
                        )
                        )
                    });
                    return Err(DispatchResult::err(errno::ENOENT));
                };
                if prop_index == props::P_PLANE_IN_FENCE_FD && value != u64::MAX {
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
                self.diag(|| {
                    format!(
                        "{}",
                        gndrm_obf::obf!(
                            "atomic: reject EPERM immutable prop {} (idx {})",
                            props::PROPS[s.prop_index].0,
                            s.prop_index
                        )
                    )
                });
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
            self.diag(|| "atomic: reject EINVAL modeset props without ALLOW_MODESET".to_string());
            return Err(errno::EINVAL);
        }
        // Referenced objects must exist, and enum/numeric values must be in
        // range — the same checks the legacy `OBJ_SETPROPERTY` path makes.
        for s in &commit.sets {
            if let Err(e) = self.validate_property(s.prop_index, s.value) {
                self.diag(|| {
                    format!(
                        "{}",
                        gndrm_obf::obf!(
                            "atomic: reject errno={} invalid prop {} (idx {}) value={}",
                            e,
                            props::PROPS[s.prop_index].0,
                            s.prop_index,
                            s.value
                        )
                    )
                });
                return Err(e);
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
                self.diag(|| {
                    format!(
                        "{}",
                        gndrm_obf::obf!(
                            "atomic: reject EINVAL fb={} without crtc (crtc_id={})",
                            plane.fb_id,
                            plane.crtc_id
                        )
                    )
                });
                return Err(errno::EINVAL);
            }
        }
        Ok(())
    }

    fn apply_commit(&mut self, commit: &ParsedCommit, fence_fd: Option<RawFd>) {
        let old_primary_fb = self.primary_state().fb_id;

        for s in &commit.sets {
            self.apply_property(s.obj_id, s.prop_index, s.value);
        }
        let crtc_on = self.crtc_state().active;
        let new_fb = self.primary_state().fb_id;

        // Publish only when the primary FB changed under an active CRTC — the
        // kernel does not re-present an unchanged plane on a re-commit.
        let publish = crtc_on && new_fb != 0 && new_fb != old_primary_fb;

        let fb = if publish {
            self.stores.fbs.get(&new_fb).cloned()
        } else {
            None
        };
        // Resolve the buffer identity the presenter will look up (the GEM may
        // already be orphaned-but-FB-referenced, so search both tables).
        let gem = fb.as_ref().and_then(|fb| {
            fb.handles
                .first()
                .copied()
                .filter(|h| *h != 0)
                .and_then(|h| {
                    self.stores
                        .gems
                        .get(&h)
                        .or_else(|| self.stores.gems_closed.get(&h))
                })
        });
        let ino = gem.map(|gem| gem.ino);
        let src_fd = gem.and_then(|gem| {
            // SAFETY: `gem.fd` is the store's live duplicate; the new
            // descriptor is owned by the job.
            let fd = unsafe { libc::fcntl(gem.fd, libc::F_DUPFD_CLOEXEC, 0) };
            (fd >= 0).then_some(fd)
        });
        // Inodes still backed by a GEM object — the presenter's retire signal
        // ("client stopped importing"). The pool reuses a retired buffer only
        // once the presenter has reported the frame's completion (see
        // `scanout`).
        let live_inos: HashSet<u64> = self
            .stores
            .gems
            .values()
            .map(|g| g.ino)
            .chain(self.stores.gems_closed.values().map(|g| g.ino))
            .collect();
        let plane = self.primary_state();

        // Real GEM semantics: the buffer is in-flight from commit until its
        // flip event fires; the worker clears it when it emits the event.
        if let Some(fb) = &fb {
            self.mark_in_flight(fb.id);
        }

        // The flip event is built at commit time with its sequence and
        // user_data; the worker stamps the claimed vblank and emits it with the
        // publish.
        let event = if commit.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            let (sec, usec) = self.now();
            let seq = self.next_flip_sequence();
            Some(DrmEventVblank {
                base: DrmEvent {
                    ty: DRM_EVENT_FLIP_COMPLETE,
                    length: core::mem::size_of::<DrmEventVblank>() as u32,
                },
                user_data: commit.user_data,
                tv_sec: sec,
                tv_usec: usec,
                sequence: seq,
                crtc_id: CRTC_ID,
            })
        } else {
            None
        };

        self.queue_commit(CommitJob {
            fb,
            plane,
            ino,
            src_fd,
            live_inos,
            fence: fence_fd,
            event,
        });
    }

    /// Validate a writable property's value the way the kernel would — a
    /// referenced id must exist, an enum value must be in the list. Shared by
    /// the atomic path ([`Self::validate_commit`]) and the legacy
    /// `OBJ_SETPROPERTY` path so both reject the same input.
    ///
    /// # Errors
    /// The errno the client sees: `EINVAL` for an out-of-range enum/range
    /// value, `ENOENT` for a reference to a non-existent object.
    pub(crate) fn validate_property(&self, index: usize, value: u64) -> Result<(), i32> {
        use props::*;
        match index {
            P_CONN_COLORSPACE => {
                if enum_contains(index, value) {
                    Ok(())
                } else {
                    Err(errno::EINVAL)
                }
            }
            P_CONN_HDR_METADATA => {
                // 0 clears it; otherwise it must name a live property blob
                // (gamescope's `hdr_output_metadata`).
                if value == 0 || self.stores().blobs.contains_key(&(value as u32)) {
                    Ok(())
                } else {
                    Err(errno::ENOENT)
                }
            }
            P_CONN_CRTC_ID | P_PLANE_CRTC_ID => {
                if value == 0 || value == u64::from(CRTC_ID) {
                    Ok(())
                } else {
                    Err(errno::ENOENT)
                }
            }
            P_CRTC_MODE_ID => {
                if value == 0 || self.stores().blobs.contains_key(&(value as u32)) {
                    Ok(())
                } else {
                    Err(errno::ENOENT)
                }
            }
            P_PLANE_FB_ID => {
                if value == 0 || self.stores().fbs.contains_key(&(value as u32)) {
                    Ok(())
                } else {
                    Err(errno::ENOENT)
                }
            }
            P_CRTC_ACTIVE => {
                if value > 1 {
                    Err(errno::EINVAL)
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }

    /// Apply one already-validated property to software state.
    pub(crate) fn apply_property(&mut self, obj_id: u32, index: usize, value: u64) {
        use props::*;
        match obj_id {
            CONNECTOR_ID => match index {
                P_CONN_CRTC_ID => self.set_conn_crtc(value as u32),
                P_CONN_COLORSPACE => self.set_conn_colorspace(value),
                P_CONN_HDR_METADATA => self.set_conn_hdr_metadata(value),
                _ => {}
            },
            CRTC_ID => match index {
                P_CRTC_ACTIVE => self.set_crtc_active(value != 0),
                P_CRTC_MODE_ID => self.set_crtc_mode(value as u32),
                _ => {}
            },
            PLANE_PRIMARY_ID | PLANE_CURSOR_ID => {
                let idx = usize::from(obj_id == PLANE_CURSOR_ID);
                self.apply_plane(idx, index, value);
            }
            _ => {}
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

/// Is `fd` poll-ready (readable)? Used to honour the NONBLOCK contract without
/// stalling the serve thread: a signalled fence is an eventfd holding a count,
/// and any regular fd (a sync_file) is readable once its fence fires.
fn fence_ready(fd: RawFd) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd, zero timeout.
    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
    n > 0 && pfd.revents & libc::POLLIN != 0
}
