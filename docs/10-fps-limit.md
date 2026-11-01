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
   `g_nOutputRefresh`. GameNative launches with a constant 1200 Hz
   (`GamescopeState.SESSION_GRID_HZ` → `gridArg()`), high enough that the grid
   never binds the session. Verified consumers:
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

gndrm exposes **one** mode: the session's scan-out size at the panel's real rate
(`PanelDesc::modeinfo` in `gndrm-daemon/src/objects.rs`; the rate comes from
`nativeSetPanelRefreshHz`, or from the frame-generation output rate when that is
higher). The vblank grid is a separate value — the daemon's own
`SESSION_REFRESH_MHZ`, 1200 Hz — so `-r` matches no `vrefresh` by design: A47
re-runs the lookup by size, and `drm_set_mode` makes the mode's rate the
declared output refresh. `-r` is the grid (paint tick, app-refresh cycle) and
never the client-visible rate.

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
- **The shim enforces it** (`icd_shim/src/scanout.rs:118-146`,
  `next_acquire_after`): the next acquire may start no sooner than one
  `1/target` period after the previous *deadline*, so the guest blocks in
  `vkAcquireNextImageKHR` — the one point where waiting costs neither rendered
  work nor display latency. The deadline, not the wake: the wait returns a hair
  after it, and `max(prev, now) + period` made that hair part of every period
  (a 60 cap read 59.78 on an Odin 3 menu, 60.00 once stepped from `prev`).
  Only a guest a whole period behind is re-based on `now`, so no debt is
  carried out of a stall.
- **`-r` is not part of it**: the grid is the constant 1200 Hz, and no launch
  argument carries the limit. `--framerate-limit` is never built.

Open points:
- **Divisor rounding bites the gamescope route only.** `nVblankDivisor =
  nRefreshHz / nTargetFPS` (`steamcompmgr.cpp:7040`, and `:10621` for the window's
  limited cycle) lands on the nearest divisor of the grid; the shim's floor is
  exact (measured: 60.00 commits/s at a 60 cap).
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
