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
  fifo, fifo_relaxed or immediate), or the guest's own mode when the env is absent
  (`icd_shim/src/proxy.rs` `effective_present_mode`, `live_present_mode`).
  gamescope's limiter holds only commits declared FIFO (`wlserver_surface_is_fifo`):
  it latches one per limited vblank (`steamcompmgr.cpp` `handle_done_commits_xwayland`)
  and the client is paced by `wl_buffer.release`. Any other mode is taken at the
  next composite, so there the shim's acquire gate is the cap:

  | container mode | cap | declared | what limits the rate |
  |---|---|---|---|
  | mailbox (default) | none | mailbox | nobody |
  | mailbox | set | mailbox | the shim's acquire gate, on the display's vblank grid |
  | fifo | none | fifo | the display (FIFO) |
  | fifo | set | fifo | gamescope's divisor limiter |
  | any, frame generation on | any | mailbox | the source-rate gate (`docs/17`) |

  fifo_relaxed and immediate behave as mailbox in this table. No env: the guest's
  own mode takes the env's place.

  The gate (`icd_shim/src/vsync_gate.rs`) releases acquires on the vblank grid
  gamescope composites from, not on a clock of its own: the daemon's `VsyncNote`
  latch grid (measured period and phase) moved earlier by the lead the daemon
  stamps flip events with (`stamp_lead_ns`). A cap at the display's rate releases
  every vblank (60 on a 59.97 Hz panel is 59.97, one frame per latch); a cap that
  divides the rate, every `refresh / cap`-th vblank (30 on 60, 60 on 120); any
  other cap below the rate, the vblank nearest each instant of an ideal
  `1e9 / cap` timeline, which averages the cap on vblank boundaries (45 on 60
  releases 3 of 4 vblanks); a cap above the rate, the cap's own clock. A frame
  that arrives after its release point starts at once, and the next release is
  the next point of the schedule, so a game below the cap keeps its own rate.
  The gate limits how many frames start; it does not space when they finish. A
  game that paces itself just below the display's rate (Unity's frame-rate target
  with VSync off: ~59.3 fps on a 59.97 Hz panel) is late for most release points,
  so the gate rarely holds it, and its frames reach the commit with its own
  jitter; two inside one gamescope composite and the older is replaced, the panel
  repeating the frame before. So the present worker also keeps one commit per
  composite window (`CommitWindow`, `proxy.rs` `hold_for_window`): windows run
  from one gamescope wakeup (2.5 ms before a stamp instant) to the next, and a
  frame whose window already has a commit is held to the stamp instant that
  closes it, after that composite, so the next composite shows it. A newer frame
  that is ready while one is held replaces it. This is the one-deep version of
  the queue FIFO has. A
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

  The census reads which path ran, every 10 s while frames are presented:
  `present mode: declared=… requested=… source=env|guest route=gamescope cap=…
  gate=vsync|clock|off … gate_holds=… late=… skipped=… slot_waits=…
  commit_lat_us=…` (`source=cap-compat`: declared FIFO for want of the stamp
  lead; `gate=clock` is frame generation's source rate or a cap above the
  display's rate). The last five are cumulative except the latency:
  `gate_holds`, acquires the gate held; `late`, acquires that started after
  their release point; `skipped`, release points moved past for them;
  `slot_waits`, acquires that passed the gate and then waited on a compositor
  release for a free image; `commit_lat_us`, the smoothed acquire-to-commit
  time. Then `win_holds` (frames the commit window held), `win_replaced` (held
  frames a newer one replaced) and `spacing=[<15:… 15-16.2:… 16.2-17.2:…
  17.2-20:… >20:…]`, the spacing of consecutive commits in ms since the last
  line: the game's own cadence as it reaches gamescope. Off the override route (the opt-in X11
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
