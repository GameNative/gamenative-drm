# 15 — the FPS limiter, frame pacing, and vsync

Scope: an audit of where frame pacing and the fps cap are decided in
`game.exe -> proton-wine (winex11) -> Xwayland -> gamescope -> gndrm-daemon ->
SurfaceFlinger`, and a design for a correct cap with correct pacing and an
honest refresh model.

Nothing in this document is implemented. The pacing-arithmetic patch remains in
`gamescope/android/patches/disabled/0032-A32-fps-cap-lower-bound-delay.patch`
and is **not** applied to `gamescope/src/`. Line references are against
`gamescope` HEAD `f3c3b96`, `gamenative-drm` `master`, and GameNative
`feat/gamescope-display`.

## 1. One variable does two jobs

gamescope derives **both** its vblank grid (the paint tick) and the
client-visible refresh from a single expression:

```
g_nNestedRefresh ? g_nNestedRefresh : g_nOutputRefresh
```

- the grid — `CVBlankTimer::GetRefresh()`, `gamescope/src/vblankmanager.cpp:75-78`,
  consumed at `:87` (`GetNextVBlank`) and `:127` (`CalcNextWakeupTime`);
- app pacing — `g_SteamCompMgrAppRefreshCycle = mHzToRefreshCycle(nRealRefreshmHz)`,
  `gamescope/src/steamcompmgr.cpp:10528-10529`;
- the limiter's divisor base — `nRefreshHz`, `gamescope/src/steamcompmgr.cpp:6938`.

The launcher passes no `-r`
(`GameNative/app/src/main/java/com/winlator/xenvironment/components/BionicProgramLauncherComponent.java:591-594`),
so `g_nNestedRefresh` is 0 and all three read `g_nOutputRefresh`.

The DRM backend sets `g_nOutputRefresh` from the mode it selected, recomputed
rather than read from the label: `drm_set_mode` →
`g_nOutputRefresh = gamescope::GetModeRefresh( mode )`
(`gamescope/src/Backends/DRMBackend.cpp:3674-3685`), and `GetModeRefresh` is
`(clock*1e6/htotal + vtotal/2)/vtotal` (`:229-243`). The daemon exposes one
synthetic mode whose `clock`/`htotal`/`vtotal` encode
`SESSION_REFRESH_MHZ = 1_200_000`
(`gamenative-drm/modules/gndrm-daemon/src/objects.rs:63`, `:91-125`), so the
recomputed rate is 1200 Hz. The mode's `vrefresh` field (`objects.rs:120`) only
has to agree with those three; it is a label, not the rate.

Consequences, all structural:

- the grid interval is `mHzToRefreshCycle(1200 Hz)` ≈ 833 µs — three orders of
  magnitude below the panel's frame time, so the composite is never the ceiling
  and a guest can present faster than the panel at all;
- the refresh pushed to clients is a fabricated 1200 Hz;
- every reachable capped rate is `1200/N`, which is what the limiter's parity
  between a requested target and a landed rate is measured against.

The daemon's flip-event timebase is built from the same constant —
`VblankTimeline::interval_ns = 1e12 / SESSION_REFRESH_MHZ`
(`gamenative-drm/modules/gndrm-daemon/src/lib.rs:1834`) — so the timeline and
the grid agree by construction. `objects.rs:55-62` states the intent: the
advertised rate is a deliberate decoupling from the panel, so that the only cap
on a guest is the user's own fps limit, and the panel downstream coalesces.

## 2. The current limiter path

App to gamescope:

- QuickMenu fps control →
  `XServerScreen.applyFpsLimiterToEngines(effectiveFpsLimit())`
  (`GameNative/app/src/main/java/app/gamenative/ui/screen/xserver/XServerScreen.kt:703-727`;
  `effectiveFpsLimit()` `:733-734`), invoked at session start (`:796`) and on
  every control change (`:745`, `:756`, `:767`). `0` means uncapped.
- gamescope arm: `GamescopeCommandQueue.setFpsLimit(limit)`
  (`XServerScreen.kt:724`) stores the value as one domain of the session's
  desired state and re-applies the whole state
  (`GameNative/app/src/main/java/app/gamenative/display/GamescopeCommandQueue.kt:127`,
  `:143-181`), calling `GameNativeDrm.nativeSetFpsLimit(fps)` (`:177`). The
  re-apply is why a value stored while the session is frozen still lands.
- JNI: `Java_app_gamenative_display_GameNativeDrm_nativeSetFpsLimit`
  (`gamenative-drm/modules/gndrm-android/src/lib.rs:1069-1077`) →
  `injector::set_fps_limit` (`gndrm-android/src/injector.rs:258`, `:262`).
- the injector emits `gamescope_control.set_app_target_refresh_cycle(fps, flags)`
  when the value or the internal-panel decision changes (`injector.rs:635-646`;
  `flags` carries `TargetRefreshCycleFlag::InternalDisplay` for an internal
  panel, `:639-643`).
- gamescope handler `gamescope_control_set_app_target_refresh_cycle`
  (`gamescope/src/wlserver.cpp:1237-1246`, dispatch `:1436`) →
  `steamcompmgr_set_app_refresh_cycle_override`
  (`gamescope/src/steamcompmgr.cpp:1273-1279`) →
  `update_app_target_refresh_cycle` (`:1237-1246`) sets
  `g_nSteamCompMgrTargetFPS` (`:1217`).

Inside gamescope:

- **the limiter cycle** (`gamescope/src/steamcompmgr.cpp:10531-10547`):
  `nRealRefreshmHz = g_nNestedRefresh ?: g_nOutputRefresh` (`:10528`);
  `nTargetFPS = min(target, ConvertmHzToHz(nRealRefreshmHz))` (`:10534-10535`);
  VRR arm `mHzToRefreshCycle(ConvertHztomHz(nTargetFPS))` — exactly `1/target`
  (`:10537-10539`); static arm
  `nVblankDivisor = nRealRefreshHz / nTargetFPS; cycle = AppRefreshCycle * nVblankDivisor`
  (`:10541-10546`) — an integer floor.
- **the gate** (`gamescope/src/steamcompmgr.cpp:6934-6973`): `IsVRRActive()` is
  false in this port (`:6941` — the daemon advertises no `vrr_capable` and no
  mutable `VRR_ENABLED`), so the static arm runs:
  `divisor = nRefreshHz / nTargetFPS; if (vblank_idx % divisor != 0) bSendCallback = false`
  (`:6963-6969`). `bShouldLimitFPS` is the per-window exemption only
  (`steamcompmgr_window_should_limit_fps`, `:1506-1508`; `window_is_limiter_exempt`,
  `:1334`), not the cap.
- the effect of `bSendCallback = false` is a **withheld frame-done**:
  `steamcompmgr_flush_frame_done` runs only under the vblank guard
  (`:10506-10512`), and the Xwayland and XDG done-commit entries gate on the same
  predicate (`:8161`, `:8223`). The cap therefore acts by *not answering the
  client*, not by sleeping.

Guest side:

- the paint gate is `bShouldPaint = vblank && (hasRepaint || hasRepaintNonBasePlane || bForceSyncFlip)`
  (`gamescope/src/steamcompmgr.cpp:10762`), and `vblank` comes only from
  `GetVBlankTimer().ProcessVBlank()` (`:10159-10164`) — i.e. from a grid tick.
- gamescope re-anchors that grid off our flip event:
  `page_flip_handler` → `GetVBlankTimer().MarkVBlank(vblanktime, true)`
  (`gamescope/src/Backends/DRMBackend.cpp:808`;
  `gamescope/src/vblankmanager.cpp:221-229`). The daemon builds the event at
  commit time (`gndrm-daemon/src/commit.rs:499-515`), stamps it with the claimed
  vblank in the commit worker (`gndrm-daemon/src/lib.rs:1944-1979`), and emits it
  **with the publish** so gamescope's present loop never waits out a
  SurfaceFlinger round trip (`lib.rs:1944-1958`).
- the shim never requests `wl_surface.frame` — no `wl_surface_frame` call exists
  anywhere under `gamenative-drm/icd_shim/`. It frees a slot on
  `wl_buffer.release` (`icd_shim/src/proxy.rs:4835`, `:7043`) and
  `vkAcquireNextImageKHR` blocks until a slot is free (`:1047-1069`,
  `:1071-1154`).
- the shim declares its effective present mode to gamescope on every commit
  (`icd_shim/src/proxy.rs:721`, `:1349`), computed by `effective_present_mode`
  (`:6336-6349`), which is what decides whether gamescope's FIFO commit deferral
  applies (`gamescope/src/steamcompmgr.cpp:8163-8184` against
  `gamescope/src/wlserver.cpp:3408-3418`).

## 3. Where pacing is decided, and what an early frame does

The stages, in order, with what each can bound:

1. **Guest production** — the title's own rate.
2. **Shim slot gate** — `acquire_take_free` hands out the first slot that is
   neither held by the app nor awaiting release (`icd_shim/src/proxy.rs:1035-1069`).
   A committed slot returns to circulation only on `wl_buffer.release`. This is
   the only back-pressure the guest reliably feels.
3. **gamescope release and paint** — a buffer is released, and the app's
   frame-done sent, on a grid tick subject to the paint gate and to the limiter
   gate (`steamcompmgr.cpp:10762`, `:6934-6973`). `paint_all` (`:10817`) is what
   drives the compositor's commit to the DRM device.
4. **Daemon publish** — one `setBuffer` transaction per commit; the flip event
   is emitted with the publish, and the publish never waits the display
   (`gndrm-daemon/src/lib.rs:1933-1979`).
5. **SurfaceFlinger to panel** — downstream of everything we own, and
   coalescing.

Which stage binds is per title, not per build. A title whose lap through the
shim and the compositor exceeds the panel's frame time is bound by its own lap
(stage 1+2). A fast title is bound by how often gamescope releases its buffers,
which is per grid tick (stage 3). Nothing bounds it at the panel rate: that is
the intent of the fabricated 1200 Hz grid (`objects.rs:55-62`).

**A frame presented early** — the guest obtained a slot and committed before the
panel's next edge. The daemon publishes it, and `VblankTimeline::claim` stamps it
on the next boundary of the session grid at or after `now`, never moving
backwards (`gndrm-daemon/src/lib.rs:1843-1855`), so an early frame is stamped at
the current boundary and a late frame skips whole intervals. Delivery stays
immediate, so an early frame neither queues nor is dropped here; the commit
replaces the layer buffer and SurfaceFlinger composites it at its own next
vblank. The guest cannot know whether it was displayed, because it holds no
frame callback; it learns only that its slot came back. That is why the next
acquire — not the present — is where the guest is paced.

## 4. Where vsync exists, and where it is only nominal

**There is no real vsync in this stack.** Nothing on the path observes the
panel's vertical blank:

- the daemon has no vblank ioctl and no vblank source; it emits a *claimed*
  timeline (`gndrm-daemon/src/lib.rs:1810-1856`);
- gamescope's "vblank" is a userspace timer (`vblankmanager.cpp:122-128`), armed
  early by `ulOffset = rollingDrawTime + redZone` (`:178`, `:90-94`, and the
  comment at `:106`: queueing a flip early is free, the kernel holds it to the
  panel cycle), and only *re-anchored* by the flip event's timestamp
  (`:221-229`);
- the panel's true blank is SurfaceFlinger's and never reaches us.

What is nominal but load-bearing:

- the grid tick is the compositor's only pacing element: the paint gate
  (`steamcompmgr.cpp:10762`) and the frame-done (`:10510`) both hang off it.
- the "vsync" a title sees is the shim's release cadence
  (`icd_shim/src/proxy.rs:1047-1069`), not a frame callback: a title that
  elects to vsync internally is paced by how often gamescope releases its
  buffers, which is per grid tick.
- upstream's real client-pacing signal, `wl_surface.frame`, is never requested
  here, so the frame-done gamescope sends has no consumer on our ICD.

**What advertising the true refresh rate changes** for a title above it: the
mode's `clock`/`htotal`/`vtotal` feed `GetModeRefresh` →
`g_nOutputRefresh`, which is what the stack *and the clients* are told — the
Xwayland server mode (`steamcompmgr.cpp:10418`, `:7383`), the
`_GAMESCOPE_DISPLAY_REFRESH_RATE_FEEDBACK` root property (`:9307-9312`), the
`active_display_info` rate list (`wlserver.cpp:1488`) and mangoapp's snapshot
(`steamcompmgr.cpp:9856`). A self-vsyncing title asked to present at the
display's rate would then cap itself correctly. But as long as
`g_nOutputRefresh` also feeds the grid (§1), telling the truth also lowers the
grid to the real rate and clamps *every* present to it — so this is not a
relabel, it is a change of which element bounds the frame rate. That is the
whole reason the two jobs must be separated before the real rate can be
advertised.

## 5. Defects that make the limiter incorrect

**D1 — the cap is a vblank-grid modulo, and A32 is disabled.**
`gamescope/src/` carries upstream's static arm, which drops the callback
whenever `vblank_idx % (refresh/target) != 0` (`steamcompmgr.cpp:6965-6967`).
The grid index is unrelated to when the guest committed, so a title already at
its own ceiling loses a whole cycle whenever its commit lands late or off the
grid, and reads *below* its target. The approach of the disabled patch is sound
and salvageable: it gives the static arm the VRR arm's lower-bound delay
(`:6947-6950`, a pure deferral against `last_commit_first_latch_time +
limited_cycle`), which structurally cannot pull the rate below what the guest
produced. The patch is not applied, so the defect is live.

**D2 — the limiter's cycle integer-divides on the static arm.**
`nVblankDivisor = nRealRefreshHz / nTargetFPS` floors
(`steamcompmgr.cpp:10543`), so on any panel whose refresh is not a multiple of
the target the divisor collapses to 1 and the cap disappears entirely — not a
loose cap, no cap. A32's second hunk removes the branch and uses
`mHzToRefreshCycle(ConvertHztomHz(nTargetFPS))` = `1/target` on both arms
(`:10541-10546`), which is the form the VRR arm already had (`:10539`).

**D3 — the advertised 1200 Hz is not the panel's rate, and one value does both
jobs.** `SESSION_REFRESH_MHZ` (`objects.rs:63`) feeds the mode's
`clock`/`htotal`/`vtotal` (`:103-104`), the mode label (`:120`), the flip-event
timeline (`lib.rs:1834`) and — through `g_nOutputRefresh` — the grid, the app
pacing and the limiter base. Nothing in the stack knows the panel's real rate:
`PanelDesc` carries only width/height/mm (`objects.rs:69-75`), built from the
JNI arguments (`gndrm-android/src/lib.rs:717-722`), and `nativeStart` takes only
width and height (`lib.rs:996-1009`). The app *does* compute the panel's maximum
(`XServerScreen.kt:306-324`, `detectMaxRefreshRateHz`) but uses it only to clamp
the slider (`:792-794`); it never crosses JNI, and `displayRefreshRateHz()` is
dead on this arm.

**D4 — VRR is dead, so the limiter always takes the arm D1 and D2 are about.**
The daemon advertises neither the connector's `vrr_capable` nor a mutable CRTC
`VRR_ENABLED`, so `IsVRRActive()` is false at every decision site
(`steamcompmgr.cpp:6941`, `:10537`). A corollary that is not itself a defect but
is the symptom users see: because reachable rates are `1200/N`, a
five-step control whose values do not divide 1200 lands on a different rate than
it displays.

**D5 — the guest never receives a frame callback, so the cap's grip is
indirect.** The shim's only pacing is the slot gate
(`icd_shim/src/proxy.rs:1039-1046`). The cap reaches the title by slowing the
grid, which slows releases, which throttles acquires — an effective but
second-order mechanism: a title that buffers internally past its slot count, or
that stops presenting, is bounded only by slot count and by the compositor's
own release cadence.

## 6. Cheapest decisive measurement per proposal

Each of these is one device arm. None has been run.

**P1 — is the client-visible refresh truthful?** Read what the client is told:
on gamescope's own Xwayland root (`GAMESCOPE_X11_DISPLAY`, socket at
`<imagefs>/usr/tmp/.X11-unix/X0`), `xprop -root GAMESCOPE_DISPLAY_REFRESH_RATE_FEEDBACK`
(`steamcompmgr.cpp:9307-9312`), or `gamescope_control.active_display_info`
(`wlserver.cpp:1488`). Expect the panel's real rate. **Falsified** if it still
reports the fabricated rate, or if the delivered present ceiling collapses onto
the panel rate — the latter means the grid was clamped rather than decoupled,
which is the whole risk of P1.

**P2 — did the grid stay separate?** The delivered cadence on a title capable of
over-refresh, limiter off, from `dumpsys SurfaceFlinger --latency` on the
scan-out **buffer** layer. The layer name is `gndrm-scanout#<id>` and must be
re-discovered per capture, because the `BackgroundColorLayer` sibling is listed
first and has no buffer queue:

```sh
LAYER=$(adb shell dumpsys SurfaceFlinger --list | tr -d '\r' \
        | grep -oE "gndrm-scanout#[0-9]+" | sort -u | head -1)
adb shell dumpsys SurfaceFlinger --latency "$LAYER"
```

**Falsified** if the p50 spacing becomes the real frame time (one panel frame)
instead of the pre-change ceiling.

**P3 — are extra frames actually generated?** The same layer, same title, with
the limiter off, above the panel rate; plus the shim's own `slots=[...]` and
`in_flight=` fields on the release and stall lines
(`icd_shim/src/proxy.rs:245-266`, `:1143-1148`). **Falsified** if the delivered
rate tracks the advertised rate, or if `in_flight` is pinned high with stalls —
the latter localises the ceiling to the slot gate rather than to the compositor.

**P4 — is the cap a lower bound?** One title, three arms: uncapped, a target just
below the title's own ceiling, and a low target of 40. Take the rate from the
daemon's published-frame cadence (the `onFramePublished` upcall feeds
`FrameTimeRing`: `gndrm-android/src/asc.rs:1173`, `lib.rs:147-148`) and from
`dumpsys SurfaceFlinger --latency`; read per-second deltas, never a session mean.
**Falsified** if the near-ceiling title at its target still reads below its own
uncapped rate, or if the 40 target is not exact. The low arm is what proves
non-inertness: a target the guest already cannot exceed separates nothing.

**P5 — does a frame callback arrive, and does it change the rate?** Log the first
N callbacks and their spacing at the shim (the existing `logf!` gate,
`GNDRM_LOG_VKSHIM`). **Falsified** if the callbacks do not arrive at the grid
rate, or if introducing them materially changes the delivered rate — which would
mean the slot gate and the callback are double-pacing the guest.

## 7. What could not be established from the code

- **Which rate "the real refresh rate" is.** `detectMaxRefreshRateHz` takes
  `display.supportedModes.maxOfOrNull { it.refreshRate }`
  (`XServerScreen.kt:314`) — the *maximum* the panel advertises, not the rate of
  the currently active mode. A panel whose active mode is 60 Hz while its
  maximum is 120 Hz would be misreported, and no code here tracks a mid-session
  mode change.
- **Every timing figure this audit would need is unmeasured here.** The rates
  and ceilings carried in docs `10`–`12` and in the project's skill references
  are from earlier sessions; by instruction no device pass was run, so this
  document states mechanisms and predictions, not results. In particular the
  claim that the uncapped ceiling is the client-vs-compositor lap rather than
  the grid is an inference from the slot gate's structure, not a measurement
  made now.
- **Whether `-r <high>` leaves the grid's relationship to the flip-event
  timeline intact.** The daemon's `VblankTimeline` claims boundaries at the
  session grid rate and gamescope's grid interval is `mHzToRefreshCycle(GetRefresh())`
  with `GetRefresh()` preferring `g_nNestedRefresh`; that they agree is a claim
  read from the code, and that raising the value does not perturb the anchor in
  a way that walks the grid is unverified.
- **Whether the shim's per-commit present-mode declaration is the only route by
  which gamescope learns the mode.** `GAMESCOPE_FPS_LIMIT` is also read as an X
  root property on gamescope's own Xwayland (`steamcompmgr.cpp:7398`), writable
  by the shim or by wine rather than by the app; the two routes are not compared
  anywhere.
- **Every reader of `g_nOutputRefresh`.** The consumers listed in §4 are the
  ones found by grep; a change to the mode's rate reaches them all at once, and
  no exhaustive audit of the variable's uses was made.
- **Whether the daemon's presenter depends on the mode rate at all.** The
  presenter publishes on every new-FB commit; the mode rate appears only in
  `objects.rs` and `lib.rs`. That the mode rate does not reach the presenter is
  an absence of evidence, not a proven independence.

## 8. The CPU cost of the daemon and the ICD shim

Scope: what one presented frame costs in the ICD shim and in the userspace DRM
daemon — its commit worker, its presenter, and the LD_PRELOAD ioctl interceptor
— which of those costs is per-frame and which is per-session, what the loop is
bounded by, and what is unmeasurable from the code. No timing figure below is a
measurement: every number is a constant read from the source, a claim already in
the tree, or explicitly absent.

### 8.1 Per frame in the ICD shim

The default present route is the override/Wayland surface
(`icd_shim/src/proxy.rs:5881-5892`); the DRI3+Present route is the opt-in. The
cost below is the default route's, with the opt-in route's difference named at
the end.

**The guest's own threads.**

- `gn_vkQueuePresentKHR` (`proxy.rs:1382-1494`): one relaxed increment, one
  `pres_iv_note` — which takes `PRES_IV_LAST` once, reads `Instant::now()`, then
  bumps five relaxed atomics (`proxy.rs:187-205`) — a `find_queue`, one
  `vkQueueSubmit` carrying the guest's own `pWaitSemaphores` (`:1432-1472`), and
  `present_dispatch` (`:1476`). The dispatch (`:1244-1322`) takes `shared`,
  writes slot flags and notifies a condvar. No I/O, no syscall on this path.
- `ring_push("vkQueuePresentKHR", …)` (`proxy.rs:1504-1513`) takes the global
  `RING` mutex, and at the 48-entry cap does an `r.remove(0)` (`:80-82`). A
  second `ring_push` sits on every acquire (`:1081`).
- `gn_acquire_common` (`proxy.rs:1071-1208`): the `ACQ_US_*` census is four to
  six relaxed atomics (`:1183-1194`) plus, when no slot is free, a block in
  `cv_free.wait_timeout` on a 4 ms slice (`REAP_SLICE`, `:1033`, `:1117-1128`)
  with an `x11_reap` probe between slices (`:1110-1113`). The slot scan
  `acquire_take_free` (`:1047-1069`) is an in-memory walk of the slot vector.

**The shim's present worker** (`worker_main`, `proxy.rs:654-756`):

- `wait_fences` on the slot's own present fence with `u64::MAX` (`:663-665`) —
  a CPU block whose duration is the guest's own render time, not the shim's.
- `slot_make_buffer` is lazy and once per slot (`:695-697`, `:7589`), not per
  frame.
- Under `SEND_LOCK` (`:323`, `:710-735`): `swapchain_set_present_mode` (the
  per-commit present-mode declaration), `surface_attach`,
  `surface_damage_buffer`, `surface_commit`, and `wl_display_flush` — the flush
  is the only syscall in the commit, one `write` on the Wayland socket.

So one presented frame here is one Vulkan submit, one CPU fence wait, one
marshal+flush under a process-wide mutex, two global-mutex `ring_push`es, and a
fixed set of relaxed-atomic bumps.

**Do the census counters format?** They do not. `ACQ_US_*` and `PRES_IV_*`
(`proxy.rs:159-182`) only `fetch_add`/`fetch_max`. The lines that would render
them (`counters()`, `slot_flags()`, `proxy.rs:246-314`) are arguments to
`logf!`, whose `$e` is evaluated inside the gate's body only
(`icd_shim/src/lib.rs:46-52`), and are additionally sampled 1-in-120
(`proxy.rs:744`). A session with `GNDRM_LOG_VKSHIM` off formats nothing here;
what remains per frame is the cached-gate atomic load and its branch.

**Per session, not per frame.** The scan-out allocator exchange — the socket
`sendmsg`/`recvmsg` pair that carries the pool dma-buf over `SCM_RIGHTS`, with
its three-attempt retry (`proxy.rs:7469`, `scanout.rs:124-152`) — runs once per
swapchain slot at swapchain creation (`proxy.rs:865`).
`effective_present_mode` is resolved once at swapchain creation
(`proxy.rs:6336-6349`), and `resolve_override_target` (`proxy.rs:5919`,
`x11.rs:609-620`) and `window_extent` once at surface creation.

**The tree walk.** `maybe_report_game_window` (`proxy.rs:558-579`) is called on
every present but returns on the `GAME_WINDOW_REPORTED` latch and, before that,
only every 30th present (`GAME_WINDOW_CHECK_EVERY`, `proxy.rs:125`). Its
predicate opens a fresh X connection and does X round trips (`x11.rs:1026-1046`),
so this is at most one X round trip per 30 presents until the game is first seen,
and none afterwards — not a per-frame cost.

**The opt-in X11 route's per-present delta.** `x11_present_slot`
(`proxy.rs:446-529`) creates the pixmap and idle fence once per slot
(`:493-517`) and, per present, does `xcb_sync_reset_fence` +
`xcb_present_pixmap_checked` + `check_err`; `check_err` is `xcb_request_check`
(`x11.rs:828-836`, `:1385-1426`), a blocking X round trip. `x11_reap`
(`proxy.rs:584-613`) then walks every slot and calls `fence_idle`, itself a
blocking `xcb_sync_query_fence` round trip (`x11.rs:1431-1449`) — so this route
pays one X round trip per in-flight slot per present. On the default route
`x11_reap` returns on its first line (`proxy.rs:585-587`).

### 8.2 Per frame in the daemon

**The interceptor.** Per flip gamescope issues exactly one atomic commit
(`gamescope/src/Backends/DRMBackend.cpp:4446` → one `DRM_IOCTL_MODE_ATOMIC`).
The LD_PRELOAD `ioctl` (`modules/gndrm-shim/gndrm_shim.c:1940-1976`) routes it to
`rpc()` (`:643-666`), which holds one global mutex across a `send_frame`
(`sendmsg`) and a `recv_frame` (`recvmsg`) that blocks until the daemon answers.
So one flip is one synchronous request/response on the RPC socket, serialized
process-wide, with a 5 s bound that exists to turn an unanswered request into a
failure rather than a hang (`:110-120`, `:489-495`). The dma-heap redirect on a
scan-out allocation is a separate intercepted path (`:1954`, `:1464`) and is per
buffer, not per frame. A first-time buffer also costs one `AddFB2`
(`DRMBackend.cpp:1939-1949`) and a retired one a `RmFB` (`:2905`); every lesser
ioctl was not audited, so "one per flip" is the steady-state count and not a
proven total.

**The serve loop.** Event-driven, not polling: `server.rs:377-399` `poll`s the
RPC socket and the flip-event fd with a `-1` timeout. Per ioctl it receives one
frame (`:423`), dispatches (`:440`), builds a response header and frame
allocating a `Vec` (`:442-449`), and sends it (`:450`). When the flip-event fd
is ready it drains the queue and sends each event (`:409-421`), allocating a
`Vec` per event.

**The commit path.** `ioctl_atomic` (`commit.rs:47`) parses into `Vec`s
(`:284`), validates, and runs `apply_commit` (`:444-525`): apply the property
sets, compute the publish predicate
`publish = crtc_on && new_fb != 0 && new_fb != old_primary_fb` (`:455`), and
build the presenter's liveness set — a fresh `HashSet<u64>` over every live and
closed GEM object **on every commit**, whether or not it publishes (`:481-487`).
A `CommitJob` is queued unconditionally (`:517`), so a re-commit of the unchanged
primary FB costs a job and a worker wakeup but no publish.

**The commit worker** (`lib.rs:1907-1982`). Event-driven: it blocks in
`commit_rx.recv()` (`:1923`) and does nothing when idle. Per job it publishes
(`:1936`) and emits the flip event (`:1966-1979`), re-stamping the claimed vblank
and writing an eventfd; the publish does **not** wait for SurfaceFlinger's
completion, so gamescope's blocked present loop waits only for this worker
(`DRMBackend.cpp:4522-4523`, `:808`, `:824-825`).

**The presenter** (`asc.rs:815-1175`), on that worker:

- a latched `AtomicBool` swap (`:831-840`) and a rect-key comparison that formats
  only on change (`:886-918`);
- the pool lock (`:926-938`): `retire(live_inos, keep)` plus a `resolve(ino)` hash
  lookup. The **AHardwareBuffer handle does not cross any IPC boundary per frame**
  — the commit carries the dma-buf *inode* and the presenter resolves it to its
  own in-process pool buffer (`:932`, `:971-973`); the only handle that crosses a
  socket is the pool fd, once per allocation (`scanout.rs:237-264`);
- `release_retired` → `AHardwareBuffer_release` for buffers whose flip event has
  been reported (`:943-944`, `:2212-2222`), outside the pool lock;
- a zero-timeout `ALooper_pollOnce` (`:1001-1007`);
- **the previous-release-fence wait**: the completion callback's stored release fd
  is swapped out and polled with a 33 ms budget (`:1009-1019`,
  `RELEASE_FENCE_TIMEOUT_MS` `:79`, `wait_fence` `:2468-2476`). This is the one
  unbounded-in-principle wait on the worker — bounded at 33 ms, after which the
  publish proceeds anyway and warns once;
- `build_show` + `apply_transaction` (`:2261-2305`, `:2329-2335`): the layer
  transaction and the `ASurfaceTransaction_apply` binder call, which the code's
  own comments call "the blocking SurfaceFlinger round-trip" (`:2257`) and the
  one "that must not serialise the allocator" (`:2328`);
- `notify_frame_published` (`:1173` → `lib.rs:147-148`), a JNI upcall on every
  new-FB publish.

**Threads that do not spin.** The commit worker blocks in `recv` (`lib.rs:1923`),
the serve loop `poll`s with a `-1` timeout (`server.rs:399`), the scan-out
acceptor `poll`s three fds with `-1` (`asc.rs:1489-1516`), and the warm-spare
worker blocks on its doorbell condvar (`asc.rs:2098-2133`). Nothing in the daemon
polls. The only timed waits are the 33 ms release-fence poll above and the
`WARM_RETRY_MS` backoff (`asc.rs:2129-2130`), and only the former is on the
publish path.

### 8.3 What the cost scales with

Three candidates, and which one bounds:

- **The synthetic 1200 Hz grid.** The compositor's loop runs once per grid tick:
  the vblank timer is re-armed unconditionally after every timer-driven tick,
  explicitly "in case pageflip handler doesn't happen so we don't stop vblanking
  forever" (`steamcompmgr.cpp:10850-10858`); the loop `poll`s the timer as a
  waitable (`:10086`, `:10157-10164`); the interval is
  `mHzToRefreshCycle(GetRefresh())` = 1/1200 s (`vblankmanager.cpp:75-78`,
  `:262-280`; `objects.rs:63`). The loop body — including the change-gated atom
  updates (`steamcompmgr.cpp:10866-10870`, `:9272-9356`) and the per-iteration
  mangoapp snapshot calls (`:10846-10848`, the connector snapshots early-returning
  unless the backend uses virtual connectors, `:9831-9834`) — is therefore driven
  at up to ~1200 iterations/s **whatever the panel does**, and is re-anchored by
  every flip event (`DRMBackend.cpp:808`).
- **The guest's own present rate.** The shim's per-frame cost is strictly one
  present; the daemon's per-flip cost is strictly one gamescope commit, and
  gamescope commits only inside `paint_all`, which runs only when there is a
  repaint (`steamcompmgr.cpp:10762`, `:10817`) — i.e. when a client committed.
  The project's own figure for that pipeline is ~259 presents/s
  (`objects.rs:60-61`, from an earlier session and not re-measured here).
- **The panel's real rate.** Nothing. `g_nOutputRefresh` is the fabricated 1200
  and `PanelDesc` carries no rate (`objects.rs:69-75`); the panel reaches the
  stack only through SurfaceFlinger, downstream of everything we own.

So the loop is bounded by the **guest's present rate** for the per-frame path
(shim present, daemon commit, presenter publish), and by the **1200 Hz grid** for
a fixed-rate floor of compositor loop iterations that runs whether or not a frame
is painted. The limiter does not change that floor: it withholds the client's
frame-done, so it lowers the guest's present rate and with it the per-frame work,
while the grid keeps ticking.

**Cheapest change that reduces the cost.** Lower the grid to the panel rate —
stage 1's separation, whose minimum form is the single constant everything else
derives from (`objects.rs:63`, consumed at `lib.rs:1834` and through
`g_nOutputRefresh` at `vblankmanager.cpp:77`, `steamcompmgr.cpp:10528`, `:6938`).
That cuts the fixed floor by the ratio of 1200 Hz to the panel rate (10×–20× for
a 60–120 Hz panel) with no change to the per-frame path. Stage 3b (a real frame
callback in the shim, `proxy.rs:1035-1069`) is the alternative if the grid must
stay high: it supersedes the per-frame pacing rather than the floor. The
per-frame path has no cheap win of its own — its dominant items are the guest's
fence wait and one blocking round trip to the daemon per flip, both structural.

### 8.4 What is gone, and what was never measured

**The whole-device GPU drain is gone.** There is no `vkQueueWaitIdle` call site
in the shim: the pointer is resolved (`proxy.rs:6830`) and never invoked, and the
only device-idle in the tree is in `gn_vkDestroyDevice` (`:6889-6890`). The
comment at `:451-456` states the replacement contract — the slot's own fence is
signalled by the shim's present submit, which waits the guest's present
semaphores, so no drain is needed and taking one "would destroy all CPU/GPU
overlap".

**The residual risk is a guest that presents with `waitSemaphoreCount == 0`.**
The present submit's waits come straight from `pi.pWaitSemaphores`
(`proxy.rs:1438-1458`), and the slot fence is that submit's completion fence
(`:1468`). With no wait semaphores and a single swapchain, the submit carries no
waits and no signal chain (`signal_chain = i + 1 < nsc`, `:1445`), so the fence
signals as soon as the empty submit completes — the worker's `wait_fences`
(`:663-665`) then returns at once and the buffer is committed while the GPU may
still be writing it. Under the removed drain that could not happen. Nothing in
the code guards the zero-wait case, and no session is known to have exercised it.

**Never measured.** The `slot_signature` byte-walk — a full CPU `mmap` of the
slot with a per-pixel loop over `w*h` and two `DMA_BUF_IOCTL_SYNC` ioctls
(`proxy.rs:345-389`) — is bounded to the first 24 presents of each swapchain
(`X11_TRACE_N`, `:217`), and `slot_mem_signature` (`:395-439`) to the first
three; neither has a timing figure, and both are off after their budget. The
blocking X round trips on the guest-surface route (the per-present
`xcb_request_check` and the per-slot `xcb_sync_query_fence` of §8.1) have never
been timed. The acquire-dwell and present-interval censuses were added to be read
from a real session (`proxy.rs:153-175`), and no session's output is in this
document.

### 8.5 Thermal and power

The device is a handheld, so a fixed-rate floor that runs whether or not a frame
is painted is a sustained cost, and the 1200 Hz grid is exactly that: the
compositor's timer wakes ~1200 times a second, re-arms itself
(`steamcompmgr.cpp:10850-10858`), runs its loop body, and is re-anchored by every
flip event (`DRMBackend.cpp:808`) — independent of the panel, which is 60–120 Hz.
On top of that sits the per-frame path, whose per-flip cost includes a blocking
IPC round trip to the daemon inside gamescope's own present call
(`DRMBackend.cpp:4522-4523`; `lib.rs:1966-1979`) and a SurfaceFlinger binder round
trip on the daemon's worker (`asc.rs:2329-2335`). Neither is a busy-wait, but
both are on the critical path of every frame, so the CPU time and the achievable
rate scale with how often a frame is produced, not with how little an idle
display needs.

What bounds it is the separation of §8.3: a grid at the panel's rate removes the
10×–20× oversampling of the compositor loop, while the per-frame path stays
bounded by the guest's own present rate, which the limiter already lowers. The
unmeasured question for a power budget is the split between the two — whether the
sustained load is the grid's wakeups or the per-frame path — and that is what
§8.6 separates.

### 8.6 The cheapest decisive measurement

One device arm, not run here. On a fixed scene, limiter off, take per-thread CPU
time (`/proc/<pid>/task/<tid>/stat` utime+stime, or `top -H`) for the gamescope
process and for the app process, in two sessions: one with the synthetic mode at
1200 Hz (the current build) and one with the mode's rate describing the panel.
Compare the gamescope vblank/compositor thread against the daemon's commit-worker
thread and the shim's present worker. **If the compositor thread's CPU scales with
the grid while the commit worker's and the present worker's do not, the cost is
the grid's wakeups and §8.3's floor is confirmed. If the daemon and shim threads
track the grid too, the per-frame path is being driven by the grid rather than by
the guest, and stage 1 alone would not fix it.** This needs no instrumentation the
tree does not already have, and it separates the two candidates in one run.

## 9. Proposal

The design is one change of kind, staged: **separate the two jobs the single
refresh variable does**, so the grid becomes a property of the session and the
client-visible refresh a property of the panel; then make the limiter a lower
bound on the target frame time; then make the two guest-facing back-pressures
honest. Nothing here is implemented (scope note above); each stage names its own
falsifier in §6.

**Target behaviour.**

- *A correct limiter.* With a target enabled the delivered rate is exactly
  `1/target` for any panel rate — no integer-division collapse of the divisor,
  and no reading *below* the guest's own rate when the guest is already near the
  target. With the target off the guest is free above the target up to its own
  ceiling.
- *Correct pacing.* The compositor's paint and its frame-done hang off the
  session grid, not the panel (§3), so a guest below the panel rate is never
  gated by a panel tick and one that runs faster composites per frame.
- *Vsync where it can be had.* Nothing on this stack observes the panel's blank
  (§4), so the honest claim is not "vsync" but **a truthful client-visible
  refresh**: a self-vsyncing title then caps itself correctly, while the stack's
  own pacing stays on the grid.
- *Over-refresh preserved.* A title that can exceed the panel keeps compositing
  per frame; the grid stays above the present pipeline's ceiling.

**The first stage is a cost change, not only an arithmetic one.** The
compositor's loop iterates at up to ~1200 Hz *whatever the panel does*: the
vblank timer is re-armed unconditionally after every tick
(`steamcompmgr.cpp:10850-10858`, `ArmNextVBlank(true)`, so the session never
stops vblanking) and gamescope's present blocks on the daemon's flip event
(`DRMBackend.cpp:4522`), so the grid — not the panel — is the sustained cost
floor (§8.3). The limiter does not lower that floor: it caps by *withholding the
frame-done* (§2), which lowers the guest's rate while the grid keeps ticking
(§8.3). Passing the panel's real rate in is therefore not merely the fix for the
three-jobs-one-variable defect in §1; it collapses the fixed-rate floor of
compositor iterations, a 10×–20× oversampling of a 60–120 Hz panel, because the
single constant everything derives from — `SESSION_REFRESH_MHZ`
(`objects.rs:63`, consumed at `lib.rs:1834` and through `g_nOutputRefresh` at
`vblankmanager.cpp:77`, `steamcompmgr.cpp:10528`, `:6938`) — falls to the panel's
rate. That is the case for stage 1, and it is why stage 1 is stated first even
though stage 2 (the limiter patch) is independent of it: the limiter's
correctness does not depend on the grid's value, but the cost floor does, and
only stage 1 moves it.

**Stage 1 — separate the grid from the client-visible rate.**

*1a. Carry the panel's real rate to the daemon and use it for the mode — was P1.*
Add `refresh_mhz` to `PanelDesc` (`objects.rs:69-75`) and use it, not
`SESSION_REFRESH_MHZ`, for the mode's `clock`/`htotal`/`vtotal` and its `vrefresh`
(`modeinfo()` `:91-125`), keeping `SESSION_REFRESH_MHZ` (`:63`) as the grid
constant. `GetModeRefresh` then returns the real rate, so `g_nOutputRefresh`
becomes truthful for every client-visible consumer listed in §4 while the grid
stays high. `VblankTimeline::new` (`gndrm-daemon/src/lib.rs:1834`) keeps its
interval on the session grid, not the mode rate, so the timeline and the grid
still agree by construction. The panel's rate is already computed app-side
(`XServerScreen.kt:306-324`) but never crosses JNI; that is the cheapest thread.
*Risk:* this is exactly the coupling §4 warns about — `g_nOutputRefresh` also
feeds the grid, so 1a *alone* would drop the grid onto the panel rate and clamp
every present to it. 1a is therefore not shippable alone; it lands with 1b, and
the failure it risks is the grid being clamped rather than decoupled.
*Cost payoff:* §8.3 and §8.5 — the fixed compositor floor falls by the
grid-to-panel ratio, the only cheap win in the whole cost audit (§8).

*1b. Hold the grid on the grid value, explicitly — was P2.* Pass `-r <session
grid>` from the launcher so `g_nNestedRefresh` is the grid and wins at
`vblankmanager.cpp:77`, `steamcompmgr.cpp:10528` and `:6938`. `-r` also feeds
`init_drm(..., g_nNestedRefresh)` (`DRMBackend.cpp:3899`) → `preferred_refresh`
(`:1499-1503`) → `find_mode` (`:1383-1385`); on a connector exposing a single
mode whose `vrefresh` is now the real rate that exact match fails and the fallback
`find_mode(0, 0, 0)` (`:1397-1399`) keeps the mode, so `drm_set_mode` (`:1406`,
`:3684`) recomputes `g_nOutputRefresh` from the mode — the real rate. The
`preferred_refresh` assignment in `drm_unset_mode` (`:3635-3636`) is reached only
on the headless connector-loss path (`:1348-1351`), not the connected one.
Result: grid = the grid value, client-visible refresh = the real panel rate, and
the mode label agrees with the recomputed rate.
*Risk:* the single-mode fallback is read from the code, not measured — if
`find_mode` instead rejects the mode, the session starts headless.
*Falsifier:* measurements P1 and P2 (§6).

**Stage 2 — make the limiter a lower bound on the target frame time — was P4.**
Re-cut A32 against current `src/`, two hunks: the gate's static arm becomes the
VRR arm's deferral (`steamcompmgr.cpp:6961-6970`), and the limiter cycle becomes
`mHzToRefreshCycle(ConvertHztomHz(nTargetFPS))` on both arms (`:10541-10546`).
With those, the cap is `1/target` whatever the advertised grid is, the integer
divisor is gone, and "capped exactly when enabled, free above the target when
not" is true for any panel rate. The deferral is sound for the static arm because
it is a pure lower bound against
`last_commit_first_latch_time + g_SteamCompMgrLimitedAppRefreshCycle`
(`:6947-6950`, fudge `k_ulVRRScheduleFudge` `:6949`), which structurally cannot
pull the rate below what the guest produced; the cycle unification is the second
hunk and the one that generalises, since it removes D2's collapse entirely.
*Risk:* the patch is parked at
`gamescope/android/patches/disabled/0032-A32-fps-cap-lower-bound-delay.patch` and
is not applied; the re-cut is against a moving `src/` (HEAD `f3c3b96` here), so a
hunk that no longer reverse-checks stops the build at `PATCH DOES NOT APPLY`, and
the regenerated patch must be re-applied and the train re-verified as a whole.
*Falsifier:* measurement P4 (§6) — the near-ceiling title must not read below its
own uncapped rate, and the low arm (40) must be exact; a fix that improves near
the ceiling but degrades where the cap is exact is a failure, not a fix.

**Stage 3 — make the guest-facing back-pressures honest.**

*3a. Keep over-refresh possible, and do not silently re-cap — was P3.* Once the
grid is separate, nothing in the paint path needs loosening: over-refresh is
already possible because the grid is high. What must be *right* rather than
patched are the two guest-facing back-pressures — the shim's slot gate
(`icd_shim/src/proxy.rs:1047-1069`) and gamescope's FIFO commit deferral
(`steamcompmgr.cpp:8163-8184`). The deferral applies only when the shim declares
FIFO (`icd_shim/src/proxy.rs:721`, `:1349`, resolved by `effective_present_mode`
`:6336-6349` against `wlserver.cpp:3408-3418`); the container default is mailbox,
and the declaration must stay the *effective* mode so a title is not silently
re-capped by a deferral it did not ask for. Relaxing the slot gate itself is not
an option: the scan hands out the first free slot, so a free-running variant
starves the compositor with one repeated buffer (`icd_shim/src/proxy.rs:1041-1046`).
*Falsifier:* measurement P3 (§6).

*3b. Optionally, request `wl_surface.frame` in the shim — was P5.* On its own
merits only: it makes gamescope's frame-done reach the client — upstream's own
pacing signal — so the cap would grip the guest directly rather than through the
slot gate. It is a new mechanism, not a condition to relax: it supersedes the
slot gate as the pacer, so it must be measured before it is trusted, and it is
the answer if the grid's wakeup cost ever shows up in power.
*Risk:* double-pacing — the slot gate and the callback both throttling the guest.
*Falsifier:* measurement P5 (§6) — callbacks must arrive at the grid rate and the
delivered rate must not change when they are introduced.

Exact touch points:

| stage | file | location | change |
|---|---|---|---|
| 1a | `GameNative/app/.../xserver/XServerScreen.kt` | `:306-324` | source the panel rate (already computed) and pass it to the session start/retarget |
| 1a | `GameNative/app/.../display/GameNativeDrm.kt` | `nativeStart` / `nativeRetargetSurface` declarations (`:93` region) | add a `refreshHz` parameter |
| 1a | `gamenative-drm/modules/gndrm-android/src/lib.rs` | `nativeStart` `:996-1009`, `nativeRetargetSurface` `:1024-1036` | accept and forward the rate; build `PanelDesc` at `:717-722` |
| 1a | `gamenative-drm/modules/gndrm-daemon/src/objects.rs` | `PanelDesc` `:69-75`, `modeinfo()` `:91-125` | add `refresh_mhz`; use it for `clock` and `vrefresh`; keep `SESSION_REFRESH_MHZ` (`:63`) as the grid |
| 1a | `gamenative-drm/modules/gndrm-daemon/src/lib.rs` | `VblankTimeline::new` `:1834` | keep the interval on the session grid, not the mode rate |
| 1b | `GameNative/app/.../components/BionicProgramLauncherComponent.java` | `:591-594` | add `-r <session grid>` to the gamescope launch line |
| 1b | `gamescope/src/vblankmanager.cpp` | `:75-78` | no change; verified `g_nNestedRefresh` wins |
| 1b | `gamescope/src/Backends/DRMBackend.cpp` | `:1383-1385`, `:1397-1399`, `:3674-3685` | no change; verify the single-mode fallback is taken |
| 2 | `gamescope/android/patches/gamescope/0032-*.patch` | re-cut from `android/patches/disabled/0032-...` | gate `steamcompmgr.cpp:6961-6970`; cycle `:10541-10546` |
| 3a | (no code) | — | hold the shim's effective present-mode declaration; do not relax the slot gate |
| 3b | `gamenative-drm/icd_shim/src/proxy.rs` | slot acquire/release `:1035-1069`, `:7043` | request `wl_surface.frame` and consume the callback before freeing the slot |
