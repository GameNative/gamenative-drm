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
3. **`-r <N>` / `--nested-refresh`** — the launch knob GameNative uses to carry
   the FPS limit value directly. `main.cpp:748` stores it as
   `g_nNestedRefresh` in **mHz**; `0` is the unset sentinel (only negatives are
   rejected) and every consumer falls back to `g_nOutputRefresh`. Verified
   consumers:
   - `vblankmanager.cpp:77` `GetVBlankInterval()` — the vblank grid itself.
   - `vblankmanager.cpp:405` — a sleep computed from the same value.
   - `steamcompmgr.cpp:10492-10494` — `nRealRefreshmHz` →
     `g_SteamCompMgrAppRefreshCycle`, consumed at `:8252-8253` as the window's
     refresh cycle (this is what paces the app's frame callbacks).
   - `steamcompmgr.cpp:6911` — the limiter's divisor base `nRefreshHz` (only
     used when a target FPS is also set; with `-r` alone `g_nSteamCompMgrTargetFPS`
     stays 0, so the cap comes from the app refresh cycle, not the divisor).
   - `Backends/DRMBackend.cpp:3910` — passed into `init_drm(..., g_nNestedRefresh)`
     → `drm->preferred_refresh` → mode selection (`find_mode`, exact `vrefresh`
     match `:1029`) and the declared output refresh (`drm_unset_mode`, `:3646`
     `if (bPreferredRefresh) g_nOutputRefresh = drm->preferred_refresh`).

   `--framerate-limit` is a dead option in this tree — declared at
   `main.cpp:97`/documented at `:221`, absent from main.cpp's long-option chain —
   and stays dead. It is not used.

## Measured behaviour (Dave the Diver, gamescope mode)

Method: deltas of gamescope's unconditional 1/s `drm: A23 flip commit <N>` over a
≥23 s window after ~95 s settle, same scene each run. Device 97c32e85, container
`xuser-STEAM_1868140`.

gndrm exposes **one** mode only: `1280x960@1200` (gndrm-daemon `objects.rs`
`PanelDesc::modeinfo` builds a single `DrmModeModeinfo`; `vrefresh` comes from
`SESSION_REFRESH_MHZ`, the daemon's own 1200 Hz constant — not the panel's rate).

The selected DRM mode is `1280x960@1200` for **every** `-r` value: the connector
exposes only that one mode, `find_mode(N)` finds no exact `vrefresh` match and
the single mode is kept. What `-r N` changes is the declared output refresh
`g_nOutputRefresh = N` and the vblank grid / app-refresh cycle = N — the grid is
re-timed while the mode stays 1200 Hz.

| `-r` | presents/s |
|---|---|
| `0` (unset→output refresh; uncapped) | ~259 |
| `60` | 60.0 |

The 1200 Hz grid never binds: the uncapped ceiling is the client↔gamescope lap,
and a divisor cap lands on its target exactly (`-r 60` reads 60.0 presents/s).

### Pacing cost

The flip event is emitted with the publish (see 12), so a paced frame's present
does not carry the SurfaceFlinger round trip: `interval ≈ 1 / target`, and a
divisor cap reads its target exactly.

## Recommended wiring

- **Runtime (mid-session) changes** stay on the existing request path: a
  `nativeSetFpsLimit(fps)` JNI call on the `GameNativeDrm` surface issues
  `gamescope_control.set_app_target_refresh_cycle(fps, internal_display)` and
  re-issues it when the setting changes (`fps = 0` = uncapped). This is the only
  route that can change the limit mid-session, because `-r` is a launch-time
  argument.
- **Launch contract**: pass `-r <value>` where value = the QuickMenu fps limit —
  `0` when the limiter is off (gamescope falls back to the output refresh =
  uncapped) and the user's target when it is on. Sourced from the same container
  extra (`fpsLimiterTarget`, off ⇒ 0) the runtime request reads, so the launch
  value and a mid-session change cannot drift. The launch command builds no
  `--framerate-limit`.

Open points:
- **Divisor rounding only bites the request path.** With `-r` alone the cap comes
  from the grid rate, not from `nVblankDivisor`, so the `nearest divisor`
  behaviour applies to runtime changes made while `g_nSteamCompMgrTargetFPS != 0`.
- **`-r` also sets the declared output refresh `g_nOutputRefresh`** (and, since
  the connector has a single mode, keeps the 1200 Hz DRM mode). Consequences of a
  low `-r` on the client-visible refresh (`_GAMESCOPE_DISPLAY_REFRESH_RATE_FEEDBACK`,
  `active_display_info` rates) were not measured directly on device — the rates
  `active_display_info` carries are the connector's mode list, not
  `g_nOutputRefresh`, in this configuration.
- **Engagement feedback**: `gamescope_limiter.state` is the honest source for
  "is a limit actually in force".
