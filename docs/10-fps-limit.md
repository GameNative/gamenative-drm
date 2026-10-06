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

Panel / gndrm exposes **one** mode only: `1280x960@120` (gndrm-daemon
`objects.rs` `PanelDesc::modeinfo` builds a single `DrmModeModeinfo` with
`vrefresh = refresh_mhz.div_ceil(1000)`; `refresh_mhz` is the Android panel
refresh, 120 Hz here).

| `-r` | gamescope "selecting mode" | raw counter pair (flip first→last) | elapsed | presents/s |
|---|---|---|---|---|
| `0`   (unset→output refresh) | `1280x960@120Hz` | 1303 → 2612 (Δ1309) | 23.152 s | 56.54 |
| `120` | `1280x960@120Hz` | 1355 → 2666 (Δ1311) | 23.169 s | 56.58 |
| `60`  | `1280x960@120Hz` | 1079 → 2000 (Δ921)  | 23.134 s | 39.81 |
| `40`  | `1280x960@120Hz` |  807 → 1501 (Δ694)  | 23.199 s | 29.92 |
| `30`  | `1280x960@120Hz` |  690 → 1244 (Δ554)  | 23.194 s | 23.89 |

The selected DRM mode is `1280x960@120Hz` for **every** `-r` value, including 60,
40 and 30. So `-r N` does **not** switch the DRM mode here: the connector exposes
only the single 120 Hz mode, `find_mode(N)` finds no exact `vrefresh` match and
the one mode is kept. What `-r N` does change is the declared output refresh
`g_nOutputRefresh = N` and the vblank grid / app-refresh cycle = N. That is a
different lever from round 3's failed "advertise a 60 Hz mode" experiment (which
made `drm_set_mode` pick a 60 Hz mode and collapsed the uncapped ceiling to
~39.8/s): `-r` re-times the grid while the mode stays 120 Hz.

For comparison, the runtime **request** path (round 3, advertised mode 120 + a
`set_app_target_refresh_cycle` cap) measured: uncapped 56.54, target 60 → 39.72,
target 40 → 29.42, target 30 → 23.57 presents/s.

### The paced interval and the per-present overhead C

Both paths fit `interval ≈ 1 / (1/N + C)` with a fixed per-present overhead
`C`:

| path | N=60 | N=40 | N=30 |
|---|---|---|---|
| `-r` grid | C = 8.45 ms | C = 8.42 ms | C = 8.53 ms |
| request cap (round 3) | C = 8.51 ms | C = 8.99 ms | C = 9.09 ms |

`C ≈ 8.5 ms` is consistent across both. It is **not** the round-1 FIFO commit
deferral in the shim: temporarily removing the ICD shim's per-commit
`swapchain_set_present_mode(FIFO)` declaration (`icd_shim/src/proxy.rs`, both the
worker and the sync-commit paths) left the mapping unchanged —
`-r 30` 23.89 → 23.96 presents/s, `-r 60` 39.81 → 39.77 presents/s. The shim was
restored afterwards.

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
  the connector has a single mode, keeps the 120 Hz DRM mode). Consequences of a
  low `-r` on the client-visible refresh (`_GAMESCOPE_DISPLAY_REFRESH_RATE_FEEDBACK`,
  `active_display_info` rates) were not measured directly on device — the rates
  `active_display_info` carries are the connector's mode list, not
  `g_nOutputRefresh`, in this configuration.
- **Engagement feedback**: `gamescope_limiter.state` is the honest source for
  "is a limit actually in force".
