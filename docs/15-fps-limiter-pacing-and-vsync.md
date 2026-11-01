# 15 — the cap, frame pacing, and vsync

Where a frame's rate is decided on the paced path, what each place can express, and
where vsync is real rather than nominal:
`game.exe -> proton-wine (winex11) -> Xwayland -> gamescope -> gndrm-daemon -> SurfaceFlinger`.

Line references are against the trees as they stand, at the A51 patch train plus the
cursor, scan-out and mode patches.

## 1. Three places a rate is decided

| place | what it holds | set by |
|---|---|---|
| the shim's acquire gate | the user's cap, exactly | `FrameCapNote{fps}` from the daemon (`docs/19`) |
| gamescope's own limiter | `nRefreshHz / nTargetFPS`, a divisor of the mode's rate | `debug_set_fps_limit`, sent only when generation is off |
| the presenter's latch grid | the display's latch points, as measured | `VsyncNote` from the daemon (`docs/17`) |

They do different jobs, and only the first is a cap in the strict sense: the gate parks
an acquire before a frame is drawn, so the guest converges on the requested rate by
arithmetic rather than by a deadline it can miss. The second is a divisor, so with
generation off the app hands gamescope the limit and lets it express what it can:
`nVblankDivisor = nRefreshHz / min(target, nRefreshHz)`.

## 2. The mode is the display's rate

The connector exposes one mode: the session's scan-out size at the display's own rate
(`PanelDesc::modeinfo`, `objects.rs`, whose rate argument is the launcher's report
through `nativeSetPanelRefreshHz`). Nothing else enters it — not the cap, not the
frame-generation multiplier — so `g_nOutputRefresh` is the refresh the compositor runs
at and the refresh clients are told, and it changes only when the display does. The
daemon builds the mode on every connector read, so a display change reaches a running
session when gamescope re-polls (`backend_set_dirty`, see `docs/11`).

The launcher passes no `-r`: `g_nNestedRefresh` is 0, so every consumer of it falls back
to `g_nOutputRefresh` (`vblankmanager.cpp:75-78`, `steamcompmgr.cpp:10606-10607`,
`:7013`) — one rate for the paint tick, the app-refresh cycle and the divisor base, and
that rate is the display's.

## 3. Vsync, where it is real

- **FIFO.** A present is requeued until the display releases a buffer, so the guest is
  paced to the panel whether or not it asked to be. The shim answers
  `vkWaitForPresentKHR` on that release, or two refresh periods after the commit when no
  release report arrives first.
- **Mailbox and immediate** are applied behind the guest's back by
  `MESA_VK_WSI_PRESENT_MODE`, while the guest still believes it has FIFO and waits on
  every frame. A guest that knew its real mode would not wait at all, so the wait is
  answered at the commit rather than holding the guest's frame-latency loop for two
  periods (`answers_present_wait_at_commit`).
- **A replaced frame.** Under mailbox a frame can be replaced before it is shown. It is
  never committed, released or latched under that id, so its wait is answered when it is
  discarded instead of timing out.
- **Generated frames** are committed by the shim itself on the display's latch points
  (`docs/17`); they carry no present id and no wait.
- **The flip event** the daemon emits carries the vblank the frame lands on
  (`VblankTimeline`, one mode period), not the instant it is built, so gamescope reads a
  completion time rather than a delivery time.

## 4. What the daemon owns

- The latch grid: `ASurfaceTransactionStats_getLatchTime` on every publish, the phase
  tracked in `vsync.rs`, and a `VsyncNote` (`anchor_ns, period_ns, lead_ns`) pushed
  whenever it moves by more than 300 µs, and once on subscribe.
- The lead: how far ahead of a latch point a commit has to be made to be the frame that
  point takes, driven so the margin sits at half a period.
- The flip event's timestamp, as above.

The daemon derives no rate. The launcher hands it the display's rate and the cap, and it
reports back only what it measures.

## 5. Measured on device

Retroid Pocket Nova (120 Hz panel, a 60 Hz mode available), Hades II, Vulkan through
zink:

- **The gate holds the cap.** Cap 40: 40.2 acquires a second over 30 s, 0.1 skipped
  batches a second, `gate_avg_us` ≈ 22 ms against a 25 ms period.
- **Generation follows the multiplier, the display does not.** Cap 40 with frame
  generation at x2 / x3 / x4 produces and counts 80 / 120 / 160 frames a second, while
  the mode stays at the display's rate (120 Hz, or 60 Hz on a 60 Hz panel).
- **A cap above the display's rate** reads as produced: 190 at a 190 cap and ~300
  uncapped on a 120 Hz mode, with publishes still bounded by the display.
- **Swapchain recreations survive.** Six toggles of Vsync (`mode=0` ↔ `mode=2`), no
  `slot init failed`, the session continuing at its rate.
- **A display change reaches the session.** Forcing the panel to 60 Hz reported 60 and
  re-selected the mode once (`selecting mode 1280x960@60Hz`).

## 6. What this document does not own

- The gate's arithmetic and the cap's plumbing: `docs/10`, `docs/19`.
- `g_nOutputRefresh` against `-r`, and the mode: `docs/11`.
- Frame generation, the latch grid and the lead loop: `docs/17`, `docs/18`.
- The present-latency budget: `docs/12`.
