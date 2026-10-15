# 14 — the guest-surface route's commit stall (queued defect)

Status: **queued defect, not a fix.** Records a defect of the guest-surface
(DRI3 + Present) route — the one `GN_VKSHIM_GUEST_SURFACE=1` selects — so the
next reader starts from the observed behaviour and the two candidate causes
rather than from a guess. The override/Wayland route is the default (doc `13`
§3.3); the guest-surface route remains the **opt-in for GDI/2D content** (the
only route that composites wine's own GDI/2D drawing alongside the guest's
Vulkan frame). No code accompanies this document.

---

## What is known

On the guest-surface route the guest keeps running and keeps presenting, but the
daemon's commit stream **stops**:

- Commits arrive **non-blank** before the stall, and all three presenter early
  exits pass — the frames are real and the presenter is entered.
- The shim's **ATOMIC heartbeat ends at `#45`**, with **no commit after `#24`**.
- The guest itself keeps presenting at **~325 presents/s** with **no errors**.
- gamescope's flip loop goes **dead** at that point: with no flip there is no
  publish, so the splash-clear latch (which requires a published framebuffer,
  `fb.id != 0`) can never fire even though the note is decoded and the
  game-window predicate reports `present=true`.

The onset is the guest's **Vulkan swapchain coming up**. On the same guest and
the same build, the override/Wayland route (the default) renders at a healthy
frame rate with the splash cleared and the QuickMenu overlaying live content —
so the failure is **route-specific**, not a guest or gamescope-wide fault.

All three splash-clear links have **ungated, permanent probes**, so a session log
on this route names which link is live: the shim's `x11 game-window predicate:`
line, the daemon's `scan-out channel non-request frame … note_decoded=…`, and the
daemon's `splash-clear latch fired …` / `splash-clear upcall clearBootingSplash
outcome=…` lines.

## The two candidate causes (with code locations)

### 1. The Present target offset

`icd_shim/src/x11.rs`, `X11Present::present` — the `self.x_off` / `self.y_off`
handed as the x/y placement of `xcb_present_pixmap_checked`. Those offsets come
from the root-child walk in `resolve_target`, which reports them on the ungated
line `x11 present target: 0x… offset=(x,y) (root child of named window 0x…)`.

If the **target XID** or its **offset** does not name the region the compositor
has latched for the game's layer, each present updates content the compositor
never samples: the first frame lands, and no flip follows. This is the one place
this route computes a placement the override route never computes (the override
sizes its layer from the committed buffer, not X geometry — A30), so it is the
route's own arithmetic to suspect first.

### 2. The absence of a swapchain damage source

On this route the shim holds **no `gamescope_swapchain` and no `wl_surface`**:
`worker_main` (`icd_shim/src/proxy.rs`, the `(*(*sc).surf).parent.is_null()`
arm) presents with DRI3 + Present and skips the
`surface_attach` / `surface_damage_buffer` / `surface_commit` sequence the
override arm runs (`icd_shim/src/proxy.rs`, the non-null-`parent` arm).

So the only possible damage source feeding gamescope on this route is Xwayland's
own commit for the window's composited surface, generated from Present. If that
per-present damage does not reach gamescope — or reaches it as a region the layer
never samples — then nothing else on the route drives a flip, and the flip loop
stops exactly when the swapchain starts driving presents into the window.

## The single instrumentation that would decide between them

One per-present line that pairs the shim's existing ungated
`x11 present target: 0x… offset=(x,y)` with **whether gamescope registered a
commit on that window's composited surface**. Patch
`diagnostics/0025-A25-startup-present-diagnostics` already emits a per-commit
"wl commit routing" line that names each commit's target, so this is that patch
turned on for the guest-surface route alone, read next to the shim's present
line — no new counter is required. The reading:

- presents continue at ~325/s and the window's surface logs **zero** commits →
  the **damage source is absent** (cause 2);
- the surface logs commits but the committed geometry never matches the
  target XID + offset → the **offset is wrong** (cause 1).

That one line, run once on this route, separates the two causes without a code
change to either candidate.

## The route's standing

The guest-surface route is the **opt-in for GDI/2D content** and stays — it is
the only route that composites wine's own GDI/2D drawing with the guest's frames
in one window. It is not the default (doc `13` §3.3), and this defect is why its
commit stream is not relied on until the instrumentation above names a cause.
