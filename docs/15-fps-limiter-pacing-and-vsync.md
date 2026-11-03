# 15 — the cap, frame pacing, and vsync

Where a frame's rate is decided on the paced path, what each place can express, and
where vsync is real rather than nominal:
`game.exe -> proton-wine (winex11) -> Xwayland -> gamescope -> gndrm-daemon -> SurfaceFlinger`.

Line references are against the trees as they stand, at the A51 patch train plus the
cursor, scan-out and mode patches.

## 1. Three places a rate is decided

| place | what it holds | set by |
|---|---|---|
| the shim's declared present mode | the container's `MESA_VK_WSI_PRESENT_MODE` (else the guest's mode); with no cap, a guest's FIFO request over a non-FIFO env; FIFO under a cap at the display's rate; mailbox under frame generation | the container, the guest and the cap (`docs/10`) |
| gamescope's own limiter | `nRefreshHz / nTargetFPS`, a divisor of the mode's rate; holds FIFO commits only | `debug_set_fps_limit`, sent only when generation is off |
| the shim's acquire gate | the cap, on the display's vblank grid, unless the container's mode is FIFO | `FrameCapNote{fps}` and `VsyncNote` from the daemon |
| the presenter's latch grid | the display's latch points, as measured | `VsyncNote` from the daemon (`docs/17`) |

Which one holds the cap follows the declared mode. Declared FIFO (the container set
fifo, or no env and a FIFO guest), gamescope's limiter passes every
`nRefreshHz / target`-th vblank (`nVblankDivisor = nRefreshHz / min(target, nRefreshHz)`)
and holds the guest's commits to it. Declared mailbox, immediate or FIFO-relaxed (mailbox
is the container default), the limiter does not hold them and the shim's acquire gate
releases the guest's frames on the display's vblank grid. A container in one of those
modes under a cap at the display's rate is declared FIFO with no gate when the guest
asked for mailbox or immediate (`source=cap-rate`), as on master, and keeps its mode with
the gate every vblank when the guest asked for FIFO (`source=cap-rate-vsync`, `docs/10`).
For the former: gamescope latches one commit per vblank at its own
wakeup, which the shim cannot see under mailbox, and its limiter (divisor 1) paces the
guest (`docs/10`). Under frame generation the app
disarms gamescope's limiter and the shim's time gate holds the source rate instead
(`docs/17`).

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
  paced to the panel whether or not it asked to be. On gamescope's override route the
  mode declared per commit is the container's `MESA_VK_WSI_PRESENT_MODE` (the guest's
  own without it), cap or no cap, and mailbox under frame generation (`docs/10`). On
  that route the shim answers `vkWaitForPresentKHR` from
  gamescope's presentation feedback, as Mesa's Wayland WSI does: every commit that
  carries a wait requests a `wp_presentation_feedback`, and the wait is answered on
  `presented` (at the refresh it reports) or `discarded`. The release comes a vblank
  later under FIFO, when the next frame is latched, and a guest that waits on every
  frame (DXVK with VSync on) turned that into two vblanks per frame (35 fps at a 60
  cap), so while a feedback is pending neither the release nor the grace answers. Every
  pending feedback is keyed by its swapchain, so a recreated swapchain, whose ids start
  again at 1, is never answered by the old one's. gamescope answers every commit: it
  sends `presented` each vblank for the last latched commit of *every* window, visible
  or not ("we return presented for any window not visible … for forward progress",
  `steamcompmgr.cpp` before `handle_presented_xwayland`), and `discarded` for a commit
  that is replaced or destroyed (`commit_t::~commit_t`) or whose window is gone. An
  unfocused game, or one behind a dialog, is therefore still answered once per vblank.
  Mesa blocks until the event comes; the shim answers a feedback that never comes
  100 ms after the commit, a backstop for a compositor fault, not a path a working
  session takes (the `fallback=` counter of the periodic `present mode:` line counts it; that line repeats with `GNDRM_LOG_VKSHIM=1`, while the ungated one prints at session start and on a change of the declared mode, its source, the cap or the gate, `docs/10`). Without the global
  (or off the override route) the wait is answered on the release, or two refresh
  periods after the commit when no release report arrives first.
- **Mailbox and immediate.** A guest that asked for either does not wait, and neither
  would one whose frames go out as mailbox or immediate (frame generation, or the
  container's env while the guest believes it has FIFO), so those waits are not held for
  the compositor: they are answered at the commit, after the frame's render fence
  (`present_wait_answer`). One case is answered earlier, when the guest queues the
  present: `source=cap-rate-vsync` (a cap at the display's rate, a container mode other
  than FIFO, a guest that asked for FIFO, declared mailbox with the vblank-grid gate).
  That guest waits on every frame, and answered after the fence a latency-1 guest
  alternated CPU and GPU: Retroid Pocket 6, DTD at 1080p, cap 60, VSync on, 60 Hz, 42 fps
  with 23 ms from acquire to commit, the GPU at 65-69 % busy and 550 MHz because its
  governor saw the bubbles. Answered at queue, as the X11 route's X server answers
  PresentCompleteNotify at the PresentPixmap request, CPU and GPU overlap and the gate
  holds the rate (60.0 fps there, 14.3 ms; `at_queue=` in the periodic census).
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
  posted per vsync while a session runs (and not while it is suspended), and the daemon
  fits the period to the measured vsync spacing over the last 32 vsyncs
  (`gndrm-daemon/src/vblank.rs`, `VsyncRing`). The steps are counted against the mode's
  advertised period and a fit is accepted only within 2 % of it: a "60 Hz" panel that
  latches at 59.973 Hz is stamped at 59.973 Hz, while a callback thread starved into
  delivering every second vsync counts two periods per step instead of locking at twice
  the period. A live mode change reaches the daemon as a new mode
  (`nativeSetPanelRefreshHz`) and starts the fit again. The post functions return
  nothing, so a callback that has not come within a second is posted once more; the
  flip events meanwhile fall back to the latch grid on their own.
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
- **Under FIFO the guest is paced by gamescope's FIFO latch; otherwise by a gate on
  the display's vblank grid.** Declared FIFO with a cap set, gamescope latches one FIFO
  commit per limited vblank, on the display's vblank stream above, and hands the
  previous buffer back with `wl_buffer.release`, which is what frees the guest's next
  acquire; a cap that is not the display's rate over a whole number runs at gamescope's
  next divisor rate above it. Declared FIFO because the cap is the display's rate
  (`source=cap-rate`), no gate runs either: gamescope's FIFO latch alone paces it.
  Declared anything else,
  gamescope takes the newest commit at each composite and the shim's acquire gate is
  the cap (`icd_shim/src/vsync_gate.rs`).
  Its release points are the instants the flip events are stamped at: the daemon's
  latch grid (`VsyncNote` anchor and measured period) minus the stamp lead
  (`stamp_lead_ns`). gamescope wakes to composite just before each (wakeup = stamp +
  interval − draw time − red zone), so a frame released there starts right after
  gamescope took the previous one and has the period, less gamescope's margin, to reach
  the next composite. A cap at the display's rate releases every vblank, a divisor cap
  every `refresh / cap`-th, any other cap below the rate the vblank nearest each instant
  of an ideal `1e9 / cap` timeline; a frame late for its point starts at once and the
  release after it is never more than one grid instant later, so a guest below the cap
  is never quantized to a divisor; a phase
  push never lets two releases into one vblank. The gate this replaces ran on the shim's
  own clock (exactly 1e9 / N ns) and beat against the display's 59.97-59.99 Hz
  (57-59 fps at a 60 cap). A cap above the rate runs on the cap's own clock
  (`gate=clock`, also frame generation's source rate). Without the stamp lead (no
  note yet, or a daemon that predates it) the gate does not run: a capped swapchain is
  declared FIFO and gamescope's limiter holds it (`source=cap-compat`), so a
  payload-only rollout keeps today's pacing until the app ships the daemon.
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

## Behaviour changes in gamescope sessions

- **The container's present mode is declared, except under a cap at the display's
  rate.** `MESA_VK_WSI_PRESENT_MODE` decides the mode on the gamescope route as on the
  X11 Present route; without it the guest's own mode is declared. A container left at
  the mailbox default runs, with no cap, FIFO for a guest that asked for FIFO (its
  own VSync, `source=guest`) and mailbox for any other guest, and mailbox capped by the shim's
  vblank-grid gate under a cap below the display's rate. Under a cap at the display's
  rate it is declared FIFO with no gate (`source=cap-rate`), because the shim
  cannot keep two mailbox commits out of one gamescope composite (`docs/10`). One set
  to fifo runs FIFO under gamescope's limiter.
- **A game below the cap keeps its own rate under mailbox** (caps below the display's
  rate). Under FIFO and gamescope's
  limiter each frame is held to whole limited vblanks, so a game below the cap judders
  between divisors of the display's rate, and one that waits on every present holds at
  the next divisor below (refresh / 2 at a refresh-rate cap); the gate starts a late
  frame at once instead.
- **Caps that do not divide the display's rate** run at gamescope's divisor rate under
  FIFO: `floor(refresh / target)` vblanks, so 40 or 45 on 60 Hz runs at 60 and 50 on
  120 Hz at 60. Under mailbox the gate averages them on vblanks.
- **Queue latency.** Under FIFO a guest faster than the cap fills its free slots, so
  up to `slots − 1` frames can wait in gamescope's FIFO list: at low caps that is up to
  that many limited periods of input latency, as on a PC. A guest that uses present wait
  is bounded by its frame latency.
- **Frame generation turned on mid-session** switches a FIFO guest's swapchain to the
  mailbox discipline with the slot count it was created with (`minImageCount + 1`, one
  fewer than a swapchain created with generation on) until the guest recreates it.
- **Toggling frame generation** sends the cap change and the generation settings
  separately; between the two, for a moment, the swapchain's mode is declared with no
  cap, so a mailbox/immediate swapchain can run uncapped for a few frames.

## 4. What the daemon owns

- The latch grid: `ASurfaceTransactionStats_getLatchTime` on every publish, the phase
  tracked in `vsync.rs`, and a `VsyncNote` (`anchor_ns, period_ns, lead_ns,
  stamp_lead_ns`) pushed whenever its phase, lead or stamp lead moves by more than
  300 µs or its period by more than 1 µs, and once on subscribe. `stamp_lead_ns` is the
  flip events' lead (below), so `anchor_ns - stamp_lead_ns` is gamescope's vblank grid,
  the one the shim's acquire gate releases on. The daemon sends at least 1 ns while
  the flip events follow a grid, so 0 means "not known" (no grid, or a daemon that
  predates the field), and the shim then declares FIFO under a cap instead of gating. Its period is the one measured from Choreographer while that is locked
  (the mode's advertised rate otherwise), so frame generation extrapolates
  the display's real rate. Its anchor is also the
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

Retroid Pocket 6 (59.97-59.98 Hz), DTD title scene, cap 60, against master's FIFO
declaration on the same scene (59.22 fps, 1.3 % doubles):

- **A cap at the display's rate, declared FIFO** (measured with the vblank-grid gate
  kept, `source=cap-rate gate=vsync`, before the gate was dropped from this cell;
  without it the cell is master's FIFO path): in-game VSync off, 59.199 fps on `SurfaceFlinger`
  and 1.3 % doubles over 60 s, shim commits equal to the daemon's publishes within
  0-2 per 10 s (the guest paces itself at ~59.3); VSync on, 59.983 fps and no
  doubles.
- **The same cap declared mailbox with the gate** lost ~1 commit a second inside
  gamescope's mailbox (58.3 fps, 2.8 % doubles), which is why it is declared FIFO
  (`docs/10`).

Retroid Pocket Nova (120 Hz panel, a 60 Hz mode available), Hades II, Vulkan through
zink, measured before the vblank-grid gate, when the acquire gate ran on the cap's
own clock for every capped session (it now does so only for frame generation's
source rate, a cap above the display's rate and the X11 Present route; a capped
gamescope session is held as in §3):

- **The clock gate held the cap.** Cap 40: 40.2 acquires a second over 30 s, 0.1
  skipped batches a second, `gate_avg_us` ≈ 22 ms against a 25 ms period.
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
