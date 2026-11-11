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
- **The request is never rewritten; a cap is a rate.** On gamescope's override
  route the shim declares, per commit (`gamescope_swapchain.set_present_mode`),
  the mode the game requested (`icd_shim/src/proxy.rs` `guest_present_mode`: its
  `VkSwapchainCreateInfoKHR.presentMode`, FIFO when it names an unknown one).
  Nothing above it changes that: no environment variable, and neither the cap nor
  the frame-generation multiplier. What those decide is a rate —
  `icd_shim/src/limits.rs` turns the guest's present mode, the cap, the
  multiplier and the panel's refresh into the frames per second the guest may
  acquire and the frames per second the generator may add:

  | present | cap | LSFG | acquire | generated |
  |---|---|---|---|---|
  | synced | on | off | cap | — |
  | synced | off | off | refresh | — |
  | synced | on | on | cap | `cap × (m−1)`, at most `refresh` |
  | synced | off | on | `refresh / m` | `acquire × (m−1)` |
  | unsynced | on | off | cap | — |
  | unsynced | off | off | none | — |
  | unsynced | on | on | cap | `cap × (m−1)` |
  | unsynced | off | on | `refresh / m` | `acquire × (m−1)` |

  "Synced" is FIFO and FIFO-relaxed, where the panel is the ceiling in both
  directions; "unsynced" is immediate and mailbox, where the guest asked to run
  past the panel and the compositor drops what it cannot show. The generated
  ceiling is a rate and not a mode: 40 acquired ×2 on a 60 Hz panel is 80
  presented frames, of which the compositor shows 60. An unreported panel imposes
  no limit rather than a wrong one, and a multiplier of 1 is not generation.

  Who holds the acquire rate follows the declaration, not the cap. A FIFO
  declaration is held by gamescope's limiter (`wlserver_surface_is_fifo`), which
  latches one commit per limited vblank (`steamcompmgr.cpp`
  `handle_done_commits_xwayland`); any other mode is taken at the next composite,
  so the shim's acquire gate holds it, on the display's vblank grid. With
  generation on, the gate holds the source rate on its own clock and the generator
  places the batches on the latch grid (`docs/17`).

  The gate (`icd_shim/src/vsync_gate.rs`) releases acquires on the vblank grid
  gamescope composites from, not on a clock of its own: the daemon's `VsyncNote`
  latch grid (measured period and phase) moved earlier by the lead the daemon
  stamps flip events with (`stamp_lead_ns`). A limit that is the display's rate
  releases every vblank; one that divides the rate, every `refresh / limit`-th
  vblank; any other limit below the rate, the vblank nearest each instant of an
  ideal timeline, which averages the limit on vblank boundaries (45 on 60 releases
  3 of 4 vblanks); one above the rate, its own clock. A frame that arrives after
  its release point starts at once, and the release after it is never more than
  one grid instant after it, so a game below the limit keeps its own rate. A gate
  on the shim's own `1e9 / N` clock, as before 7bdcbae, beat against the panel:
  57-59 fps on `SurfaceFlinger` at a 60 cap on a 59.97 Hz panel.

  Until a note carrying the lead has arrived (no note yet, or a daemon that
  predates the field and sends 0) the gate cannot run on the grid and paces on its
  own clock, `gate=clock`. A swapchain created before the lead is known keeps the
  slot count of the guest's own request until it is recreated.

  The census says which path ran. Ungated, once when frames first flow and again
  whenever the declaration, its source, the cap, the limits or the gate changes:
  `present mode: declared=… source=guest|generating route=gamescope cap=… gate=…
  lsfg=… acquire=… generate=…` (`source=generating`: a generated frame, declared
  mailbox; `acquire` and `generate` are the limits in frames per second,
  `acquire=0` for none and `generate=0` for generation off; `gate=clock` is
  generation's source rate or a limit above the display's rate). With
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
