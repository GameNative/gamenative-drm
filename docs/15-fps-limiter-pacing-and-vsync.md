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
- **The flip event** carries the display's vsync, as a KMS driver's carries the
  hardware vblank. gamescope re-arms its composite timer from every event (wakeup =
  stamp + interval − draw time − red zone, `vblankmanager.cpp`), so the stamp decides
  where in the display's cycle gamescope finishes; a timeline of its own, however
  regular, slides against the display. The source is `AChoreographer`
  (`gndrm-android/src/choreographer.rs`): one thread with an `ALooper` keeps a callback
  posted per vsync while a session runs, and the daemon fits the period to the measured
  vsync spacing over the last 32 vsyncs (`gndrm-daemon/src/vblank.rs`, `VsyncRing`). The
  mode's integer rate never enters it: a "60 Hz" panel that latches at 59.973 Hz is
  stamped at 59.973 Hz, and a live mode change is followed when the spacing changes.
- **Which instant is stamped.** KMS has one instant per cycle: the vblank is both the
  point a flip must be queued by and the point scan-out starts. Android splits it into
  `SurfaceFlinger`'s latch and the present one or more periods later. gamescope uses
  the stamp as the point its next commit must beat by its red zone, so the stamps sit
  on the **latch** grid, not the present: the preferred frame timeline's deadline on
  API 33+ (`AChoreographer_postVsyncCallback`), moved by where `SurfaceFlinger`
  actually latched the daemon's publishes against it (`latch-after-deadline`, smoothed
  from `ASurfaceTransactionStats_getLatchTime`). A present-time stamp would aim
  gamescope's margin at an instant whose phase to the latch is the compositor's work
  duration. A publish is claimed for the first latch at or after its apply returned —
  the latch that takes it, even when that is after Choreographer's deadline; claiming
  the next one would move gamescope's next wakeup a whole period out and leave a latch
  with no new frame.
- **The lead.** The stamp is that latch minus a lead. gamescope's 1.65 ms red zone was
  sized for a kernel commit; here a commit still crosses the daemon's commit worker and
  a binder apply, with about 1 ms of spread. Stamped on the deadline itself (Retroid
  Pocket 6, SWD2, cap 60, 2026-11-02) the applies landed 1.0 / 2.1 / 4.5 ms (p10 / p50 /
  p90) ahead of the latch, the tail crossed it, and SurfaceFlinger showed 58.65 fps on a
  59.967 Hz panel. The lead moves gamescope's whole schedule earlier by the same amount.
  It is measured, not fixed: each completed publish's margin (latch − apply) feeds a
  quantile loop that holds the 10th percentile at 3 ms (30 % of a period on faster
  panels), starting from 2 ms and capped at a third of a period. `GNDRM_GS_LATCH_LEAD_US`
  in the app process's environment fixes it instead. The cost is latency: every frame
  waits the lead longer between gamescope's commit and the latch (about 2 ms more than a
  deadline stamp on that device). The stamps are earlier than the light by the lead plus
  the compositor's pipeline; consumers read differences between stamps (frame time),
  which a constant offset does not touch.
- **Fallbacks.** On API 29-32 Choreographer reports the vsync time only
  (`AChoreographer_postFrameCallback64`): its measured period is used with the latch
  grid's phase (`vsync.rs`, from `ASurfaceTransactionStats_getLatchTime`), or the vsync
  time's own phase until a latch was seen. With Choreographer silent for a second the
  latch grid alone is followed, at the mode's period; with neither reporting yet, the
  events walk the mode's interval from the clock.
- **Ordering.** A claim is never earlier than the previous one and never repeats an
  instant: a second frame inside one period claims the next instant, and a late frame
  skips whole periods. Switching sources keeps the rule (the next claim is at least half
  a period after the last).

## 4. What the daemon owns

- The latch grid: `ASurfaceTransactionStats_getLatchTime` on every publish, the phase
  tracked in `vsync.rs`, and a `VsyncNote` (`anchor_ns, period_ns, lead_ns`) pushed
  whenever its phase moves by more than 300 µs or its period by more than 1 µs, and once
  on subscribe. Its period is the one measured from Choreographer while that is locked
  (the mode's advertised rate otherwise), so the shim's vblank gate and frame
  generation extrapolate the display's real rate. Its anchor is also the
  flip timeline's phase where Choreographer gives no deadline, and its fallback.
- The lead: how far ahead of a latch point a commit has to be made to be the frame that
  point takes, driven so the margin sits at half a period.
- The display's vsync timeline: Choreographer's vsyncs on a thread of its own while a
  session runs, the period fitted to their spacing, the phase from the frame deadline
  (or the latch grid), and the flip event's timestamp on it, as above. One ungated line
  marks each lock — `vsync: flip events on the display's vsync: source=… period=…us
  (… Hz, mode … Hz) latch-vs-grid=…` — the first, and the first after a rate change.
  Another, every 10 s while publishes complete, is the cross-check: `vsync: check
  source=… period=…us (… Hz) publishes=… vsyncs=… late=… pushed=… margin
  p10/p50/p90=…/…/… lead=… latch-after-deadline=…`. `late` counts publishes that missed
  the latch they were aimed at; `pushed`, flip events moved past their latch by the
  one-claim-per-instant rule; publishes below vsyncs at a cap at or above the panel's
  rate is gamescope skipping a wakeup.
- The stamp lead, as above.

The daemon derives no rate for the mode. The launcher hands it the display's rate and the
cap; the connector advertises that rate, and the daemon reports back only what it
measures — the latch grid to the presenter, the vsync timeline in the flip events.

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
