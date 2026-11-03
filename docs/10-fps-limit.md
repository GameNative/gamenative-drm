# 10 — applying GameNative's in-game FPS limit in Gamescope mode

## What the app does today (X11 arms)

The QuickMenu performance settings carry `fpsLimiterEnabled` / `fpsLimiterTarget`
/ `fpsLimiterMax`; `XServerScreen` stores them as container extras
(`fpsLimiterEnabled`, `fpsLimiterTarget`) and the limit is pushed through
`XServerView.setFrameRateLimit`, which lands on one of:

- `VulkanRenderer.setFpsLimit` — the Vulkan renderer's own pacing,
- `PresentExtension.frameRateLimit` — the X11 present pacer: an idle-notify
  scheduled at `1e9 / targetFps` (PresentExtension.java:173-179),
- `ShmFramePacer` — the SHM presenting path.

All three pace **our present path**. In Gamescope mode none of them sees the
frames: the guest renders into the ICD shim's slots and the shim hands them to
gamescope, so our pacer would be pacing a path the frames no longer take.

## What gamescope offers

1. **`gamescope_control.set_app_target_refresh_cycle(fps, flags)`** (since v2,
   `wlserver.cpp:1292`). Runtime, per-app, and the only route that needs no
   patch: the injector already holds this connection.
   - `flags`: `internal_display` 0x1, `allow_refresh_switching` 0x2,
     `only_change_refresh_rate` 0x4 (change the rate, skip the fps cap).
   - `fps = 0` disables the limit (native).
   - Applied to the window only when `steamcompmgr_window_should_limit_fps(w)`
     holds, and the cap is applied as a **refresh divisor** —
     `nVblankDivisor = nRefreshHz / nTargetFPS` (`steamcompmgr.cpp:6938`) — so a
     target that is not a divisor of the limiter's base lands on the nearest
     divisor, not on the exact value.
   - Engagement is observable through `gamescope_limiter.state` (1 = engaged),
     which is what a UI should show as the live indicator.
2. **X11 root property `GAMESCOPE_FPS_LIMIT`** on gamescope's Xwayland root
   (interned `steamcompmgr.cpp:9003`, read at `:7371`). Equivalent effect, but it
   needs a client of *gamescope's* Xwayland — writable by the ICD shim (which
   already has that connection) or by wine, not by the app process.
3. **`-r <N>` / `--nested-refresh`** — the session's vblank grid.
   `main.cpp:740-741` stores it as `g_nNestedRefresh` in **mHz**; `0` is the
   unset sentinel (only negatives are rejected) and every consumer falls back to
   `g_nOutputRefresh`. GameNative passes no `-r`, so `g_nNestedRefresh` is 0 and
   every consumer below falls back to the mode's rate, which is the display's.
   Verified consumers:
   - `vblankmanager.cpp:75-77` `GetRefresh()` — the vblank grid itself, and
     `:405` a sleep computed from the same value.
   - `steamcompmgr.cpp:10606-10607` — `nRealRefreshmHz` →
     `g_SteamCompMgrAppRefreshCycle`, the window's refresh cycle (what paces the
     app's frame callbacks).
   - `steamcompmgr.cpp:7013` — the limiter's divisor base `nRefreshHz`, used only
     when a target FPS reached gamescope through
     `set_app_target_refresh_cycle`.
   - `Backends/DRMBackend.cpp:3943` — passed into
     `init_drm(..., g_nNestedRefresh)` → `drm->preferred_refresh`, which A47
     makes a size-only mode lookup because the grid matches no `vrefresh`;
     `drm_set_mode` (`:3728`) then makes the mode's own rate the declared
     output refresh.

   `--framerate-limit` is a dead option in this tree — declared at
   `main.cpp:97`/documented at `:221`, absent from main.cpp's long-option chain —
   and stays dead. It is not used.

## Measured behaviour (Dave the Diver, gamescope mode)

Method: deltas of gamescope's unconditional 1/s `drm: A23 flip commit <N>` over a
≥23 s window after ~95 s settle, same scene each run. Device 97c32e85, container
`xuser-STEAM_1868140`.

gndrm exposes **one** mode: the session's scan-out size at the display's own rate
(`PanelDesc::modeinfo` in `gndrm-daemon/src/objects.rs`; the rate comes from
`nativeSetPanelRefreshHz`). The cap and the frame-generation multiplier never
enter it, so the mode is the rate the compositor paints at and the rate clients
are told. With no `-r` on the launch line, the paint tick, the app-refresh cycle
and the divisor base all derive from that one rate.

A divisor cap on the grid lands on its target exactly, and the uncapped ceiling
is the client↔gamescope lap — the grid itself never binds.

| `-r` | presents/s |
|---|---|
| `0` (unset→output refresh; uncapped) | ~259 |
| `60` | 60.0 |

### Pacing cost

The flip event is emitted with the publish (see 12), so a paced frame's present
does not carry the SurfaceFlinger round trip: `interval ≈ 1 / target`, and a
divisor cap reads its target exactly.

## How the cap is applied

- **`nativeSetFpsLimit(fps)`** (`gndrm-android/src/lib.rs:1454`, `0` = uncapped)
  fans out to both routes. `injector::set_fps_limit` issues
  `gamescope_control.set_app_target_refresh_cycle(fps, internal_display)` and
  re-issues it when the value or the panel's screen type changes
  (`injector.rs:717-722`); `set_frame_cap_target` (`lib.rs:322`) holds the target
  so the scan-out channel pushes a `FrameCapNote` to a subscribed shim.
- **Every subscriber is pushed to, not the allocator.** The card listener hands
  one client the allocator role at a time (`server::accept_newest`), and a session
  has three that open it — the compositor, a wine child and the game — swapping it
  for the first ~20 s. A `FrameCapSubscribe` therefore outlives that role:
  `gndrm_daemon::pushes` keeps the registration and sends the cap, the panel's
  grid and the frame-generation settings to every client that asked for them. A
  live cap change reaches the guest that gates on it even after another client
  took the slot; a subscriber whose peer is gone is dropped there, and the
  connection currently being served stays its own loop's to close. This is what
  makes a cap change made mid-session reach an already-connected shim at all.
- **Who enforces it follows the present mode, and the mode follows the container.**
  On gamescope's override route the shim declares to gamescope, per commit
  (`gamescope_swapchain.set_present_mode`), the container's
  `MESA_VK_WSI_PRESENT_MODE` (mailbox is the container default; the user may set
  fifo, relaxed or immediate), or the guest's own mode when the env is absent
  (`icd_shim/src/proxy.rs` `effective_present_mode`, `live_present_mode`).
  gamescope's limiter holds only commits declared FIFO (`wlserver_surface_is_fifo`):
  it latches one per limited vblank (`steamcompmgr.cpp` `handle_done_commits_xwayland`)
  and the client is paced by `wl_buffer.release`. Any other mode is taken at the
  next composite, so there the shim's acquire gate is the cap:

  | container mode | cap | declared | what limits the rate |
  |---|---|---|---|
  | mailbox (default) | none, guest asked for FIFO (in-game VSync on) | fifo (`source=guest`) | the display (FIFO) |
  | mailbox (default) | none, guest asked for mailbox or immediate | mailbox | nobody |
  | mailbox | at the display's rate, guest asked for FIFO (in-game VSync on) | mailbox (`source=cap-rate-vsync`) | the shim's acquire gate, every vblank; the present wait is answered when the guest queues the present |
  | mailbox | at the display's rate, guest asked for mailbox or immediate | fifo (`source=cap-rate`) | gamescope's limiter (divisor 1), one FIFO commit per vblank; no gate, as on master |
  | mailbox | below the display's rate | mailbox | the shim's acquire gate, on the display's vblank grid |
  | fifo | none | fifo | the display (FIFO) |
  | fifo | set | fifo | gamescope's divisor limiter |
  | any, frame generation on | any | mailbox | the source-rate gate (`docs/17`) |

  FIFO-relaxed (`relaxed`) and immediate behave as mailbox in this table. No env: the guest's
  own mode takes the env's place. "At the display's rate" is a cap within 0.5 %
  of the measured rate (60 on a 59.97 Hz panel); a cap above it runs on the
  gate's own clock under the container's mode.

  **With no cap a guest's FIFO request wins over the container's default.** Every
  container carries `MESA_VK_WSI_PRESENT_MODE=mailbox` as a stamped default the user
  did not choose. With no cap, honouring it turned a game's own VSync (a FIFO
  request) into an uncapped mailbox session, which is neither master's behaviour nor
  what the user asked for. A FIFO request (FIFO-relaxed too) is a specific wish and
  is declared as asked (`source=guest`); a mailbox or immediate request means "do not
  make me wait", and the container's mailbox is the substitute, as on the X11 route
  (an uncapped immediate guest gets mailbox, not tearing). An env of `fifo` is an
  explicit wish and wins in every case. Under a cap the table above applies whatever
  the guest asked for.

  **The user's control is `MESA_VK_WSI_PRESENT_MODE`.** It sits in the container's
  environment, written from the driver's present-mode setting, and takes Mesa's
  names: `fifo`, `relaxed` (FIFO-relaxed), `mailbox`, `immediate`. Anything else,
  `fifo_relaxed` included, is rejected as Mesa rejects it, the guest's own mode is
  used, and the shim logs `Invalid MESA_VK_WSI_PRESENT_MODE value!` whether or not
  `GNDRM_LOG_VKSHIM` is set, so a typo shows in a shipped session. It is
  overridden in one case: a non-FIFO value under a cap at the display's rate is
  declared FIFO (`source=cap-rate`, below). Mesa reads the same variable, so it
  also sets the mode a zink guest's own swapchain asks for below the shim.

  **Why mailbox is not declared at the display's rate.** At that cap the panel
  shows one frame per vblank under either mode, and under mailbox the shim has to
  keep two commits out of one gamescope composite. It cannot see when gamescope
  latches: gamescope takes commits at its wakeup, the target vblank minus its
  rolling draw time and red zone (`vblankmanager.cpp` 142-199), and that draw time
  is a peak hold that jumps on any slow draw (164-165) and decays per wakeup
  (167); draw time runs to the page flip queued (`DRMBackend.cpp` 4465), on this
  stack through the daemon's commit. The client sees only `presented` (stamped
  with the wakeup's target vblank, `steamcompmgr.cpp` 8140, 8168) and `discarded`,
  one inequality per frame. Measured on a Retroid Pocket 6 (DTD, cap 60, in-game
  VSync off, a guest pacing itself at ~59.3 fps on a 59.98 Hz panel): mailbox with
  the gate gave 58.3 fps on `SurfaceFlinger` with ~1 commit a second replaced in
  gamescope's mailbox; a one-commit-per-window queue in the shim at a guessed
  2.5 ms boundary gave 59.0 with ~0.8 a second still replaced; FIFO on master
  gives 59.22 on the same scene. Under FIFO gamescope latches one commit per
  vblank at its own wakeup (`steamcompmgr.cpp` 8028-8049) and its limiter's
  divisor at this cap is 1, which is master's behaviour in this cell. That holds
  for a guest that asked for mailbox or immediate, which paces itself. A guest
  that asked for FIFO (in-game VSync on) waits on each frame's presentation
  before starting the next; declared FIFO, that wait is gamescope's `presented`
  at its latch, so the whole frame has to fit between two latches, and with
  ~16 ms of work and three images it fell to 40 fps at 60 Hz (a 1280x960
  device, DTD in game: 400 commits per 10 s, 400/600 publishes). That guest is
  declared the container's mode with the gate every vblank instead
  (`source=cap-rate-vsync`): its wait is answered when it queues the present
  (`docs/15`), the gate paces it,
  and the swapchain keeps the mailbox image count (Retroid Pocket 6, DTD,
  VSync on, cap 60: 59.983 fps, no doubles; with the wait answered at queue,
  42 fps before and 60.0 after in a GPU-heavy 1080p scene). This is the only
  case answered at queue; every other mailbox or immediate presentation is
  answered at the commit, as on master. A container set to
  `MESA_VK_WSI_PRESENT_MODE=fifo` never reaches it, so setting a game's
  container to fifo restores master's behaviour for that game. The
  vblank-grid gate is not kept on top of it: FIFO's back-pressure already holds
  the guest, and the two stacked turned a guest whose acquire-to-commit grew
  past a period into a 40 fps session on a 60 Hz panel (DTD on a 1280x960
  device after a live 120 to 60 Hz switch: `gate_holds` 0, `gate_late` 400 and
  `skipped` 200 per 10 s, commit latency 24 ms, 400/600 publishes). Below the
  display's rate the gate's releases
  are at least two vblanks apart and two frames in one composite are rare, so
  mailbox stays (FIFO with gamescope's limiter quantized a 32 fps game to 30 at a
  60 cap on 120 Hz; mailbox with the gate did not).

  The gate (`icd_shim/src/vsync_gate.rs`) releases acquires on the vblank grid
  gamescope composites from, not on a clock of its own: the daemon's `VsyncNote`
  latch grid (measured period and phase) moved earlier by the lead the daemon
  stamps flip events with (`stamp_lead_ns`). A cap at the display's rate releases
  every vblank (60 on a 59.97 Hz panel is 59.97, one frame per latch); a cap that
  divides the rate, every `refresh / cap`-th vblank (30 on 60, 60 on 120); any
  other cap below the rate, the vblank nearest each instant of an ideal
  `1e9 / cap` timeline, which averages the cap on vblank boundaries (45 on 60
  releases 3 of 4 vblanks); a cap above the rate, the cap's own clock. A frame
  that arrives after its release point starts at once, and the release after it
  is never more than one grid instant after it, so a game below the cap keeps its
  own rate. A
  gate on the shim's own `1e9 / N` clock, as before 7bdcbae, beat against the
  panel: 57-59 fps on `SurfaceFlinger` at a 60 cap on a 59.97 Hz panel.

  The gate needs the stamp lead. Until a note carrying one has arrived (no note
  yet, or a daemon that predates the field and sends 0), a capped swapchain whose
  mode is not FIFO is declared FIFO and gamescope's limiter is the cap, as before
  the container's mode was honoured here; the census reads `source=cap-compat`.
  Released on the latch points themselves, frames start ~2 ms after gamescope's
  wakeup and straddle its composite (Retroid Pocket 6, cap 60: 58.3 fps and 102
  frames replaced in gamescope's mailbox per minute at 60 Hz, 13 % of publishes
  late at 120 Hz). A rollout that ships only the payload (the shim) therefore keeps
  today's pacing until the app ships the daemon that sends the lead. A swapchain
  created before the lead is known keeps the slot count of the guest's own request
  until it is recreated.

  The census says which path ran. Ungated, once when frames first flow and again
  whenever the declared mode, its source, the cap or the gate changes:
  `present mode: declared=… requested=… source=env|guest|cap-rate|cap-compat
  route=gamescope cap=… gate=vsync|clock|off lsfg=…` (`source=guest`: the guest's
  own mode, no env or a FIFO request with no cap; `source=cap-rate`: declared FIFO
  because the cap is the display's rate and the guest paces itself, with
  `gate=off`; `source=cap-rate-vsync`: the container's mode with `gate=vsync`
  because the cap is the display's rate and the guest asked for FIFO; `source=cap-compat`:
  declared FIFO for want of the stamp lead, with `gate=off`; `gate=clock` is frame
  generation's source rate or a cap above the display's rate). With
  `GNDRM_LOG_VKSHIM=1` the same line repeats every 10 s with the counters a device
  run checks the pacing against: `acquires= commits= declared_fifo=
  declared_other= releases= feedback= presented= discarded= fallback=
  gate_holds= gate_late= skipped= slot_waits= commit_lat_us= spacing=[…]`. They
  are cumulative except the latency and the spacing: `gate_holds`, acquires the
  gate held; `gate_late`, acquires that started after their release point (not
  the daemon's `late=` on its `vsync: check` line, which counts publishes that
  missed their latch); `skipped`, release points moved past for them;
  `slot_waits`, acquires that passed the gate (if any) and then waited on a
  compositor release for a free image; `commit_lat_us`, the smoothed
  acquire-to-commit time (both counted whether or not the gate runs);
  `spacing=[<15:… 15-16.2:… 16.2-17.2:… 17.2-20:… >20:…]`, the spacing of
  consecutive commits in ms since the last line, the game's own cadence as it
  reaches gamescope. Off the override route (the opt-in X11
  Present route) the env decides the mode as in Mesa and the gate runs on the
  cap's own clock, as before.
- **`-r` is not part of it, and no launch argument carries the limit.** The app
  sends the cap to the shim's gate; `--framerate-limit` is never built.

Open points:
- **Divisor rounding bites the FIFO route only.** `nVblankDivisor =
  nRefreshHz / nTargetFPS` (`steamcompmgr.cpp:7040`, and `:10621` for the window's
  limited cycle) lands on the nearest divisor of the grid, so with the container
  set to fifo a 40 or 45 cap on 60 Hz runs at 60. Under mailbox the shim's gate
  averages a non-divisor cap on vblanks instead.
- **The client-visible refresh is the mode's, and the mode is the display's.**
  `-r` reaches `g_nOutputRefresh` only transiently (`drm_unset_mode`, `:3680`),
  before `drm_set_mode` (`:3728`) overwrites it with the mode's rate —
  `_GAMESCOPE_DISPLAY_REFRESH_RATE_FEEDBACK` and `active_display_info` therefore
  carry the mode's rate, never the grid. The cap and the frame-generation
  multiplier never enter it: the connector advertises the display's own rate, so
  the compositor paints at the rate the display latches whatever the session does
  (a 40 cap at x4 runs a 120 Hz mode and produces 160 frames a second). The
  daemon serves the rate live — the mode is built on every connector read — so a
  display change is applied by dirtying gamescope's backend
  (`backend_set_dirty`), which re-reads the connector and re-selects the mode.
  The QuickMenu never moves it.
- **The frame ring counts produced frames**, not published ones: the shim reports
  its acquire and generation totals (`FrameAcquired`), and the daemon turns the
  deltas into `onFramePublished`. A cap above the panel therefore reads as more
  frames than the panel shows — which is the point, and why the publish count is
  kept only as the session's own statistic.
- **Engagement feedback**: `gamescope_limiter.state` covers the gamescope route;
  the shim logs `scanout frame cap: fps=… effective=…` (`scanout.rs:148-158`),
  `effective` being the target in force once the note is adopted, and reports what
  the cap delivered in its own census (`cap=[fps= notes= gate_holds= acq_fps=]`,
  `GNDRM_LOG_VKSHIM=1`): `acq_fps` is the rate at the instant the cap acts, so it
  — not the app's display-side frame count — is what a target is checked against.
