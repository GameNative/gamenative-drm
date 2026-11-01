# 11 — the refresh model: `g_nOutputRefresh` vs `g_nNestedRefresh` (`-r`)

## Two variables, two jobs

**`g_nOutputRefresh`** (mHz, `main.cpp:305`) — the **mode's** rate, assigned by the DRM
backend from the mode it selected (`Backends/DRMBackend.cpp:3727-3728`, the
`GetModeRefresh` recompute at `:229-246`). It is what the whole stack *and the clients*
are told:

- the Xwayland server mode (`steamcompmgr.cpp:7458`, `:10496`),
- the `_GAMESCOPE_DISPLAY_REFRESH_RATE_FEEDBACK` root property (`:9384-9390`),
- `gamescope_control.active_display_info`'s rate list (`wlserver.cpp:1483-1487`),
- mangoapp's snapshot (`mangoapp.cpp:221`), ReShade (`reshade_effect_manager.cpp:293`).

**`g_nNestedRefresh`** (mHz, `main.cpp:299`) — `-r/--nested-refresh`; **0 means fall back
to the output refresh** (`main.cpp:740-746` rejects only negatives). In the nested backends
it *becomes* the output refresh (`SDLBackend.cpp:230`, `WaylandBackend.cpp:1989`,
`HeadlessBackend.cpp:112`). In the **DRM** backend — ours — it is passed to
`init_drm( &g_DRM, w, h, g_nNestedRefresh )` (`Backends/DRMBackend.cpp:3943`) and stored as
`drm->preferred_refresh` (`:1545`).

## In the DRM backend they are separate

`find_mode( connector, w, h, vrefresh )` (`DRMBackend.cpp:1042-1069`) matches `vrefresh`
**exactly** when one is given, so a grid that names no exposed mode falls through —
which is why A47 re-runs the lookup by size with the refresh unconstrained
(`:1417-1426`). `drm_unset_mode` then declares the preferred refresh
(`:3680`), and `drm_set_mode` overwrites it with the selected mode's own rate (`:3728`).

So in the DRM backend **`-r` is the session's grid** — the compositor's paint tick and
the app-refresh cycle — while the **mode's rate is what clients are told**. A cap is a
grid divisor, or the shim's acquire floor (doc 10); it never re-times the mode.

## What the grid and the pacing read

- **Grid:** `CVBlankTimer::GetRefresh()` = `g_nNestedRefresh ?: g_nOutputRefresh`
  (`vblankmanager.cpp:75-77`); the interval comes from `mHzToRefreshCycle(GetRefresh())`
  in `GetNextVBlank()`, and the same expression times the timer's idle sleep (`:405`).
- **App pacing:** `g_SteamCompMgrAppRefreshCycle = mHzToRefreshCycle( g_nNestedRefresh ?:
  g_nOutputRefresh )` (`steamcompmgr.cpp:10606-10607`), consumed per window at `:8355`
  (that is what throttles the guest's frame callbacks).
- **Limiter divisor base:** `nRefreshHz` = the same nested-expression (`:7013`); the
  divisor only applies while `g_nSteamCompMgrTargetFPS != 0`.

Both the grid and the app pacing come from **one** expression, so there is no way to pace
the app at N while keeping the grid at the mode's rate — they move together.

## What gndrm does with it

GameNative launches with `-r 1200` (`GamescopeState.SESSION_GRID_HZ` → `gridArg()`), the
grid the daemon's flip timeline is also built from (`SESSION_REFRESH_MHZ`,
`gndrm-daemon/src/objects.rs:68`; `lib.rs:2091`), so the grid never binds a session.

The **one** mode the connector exposes is the session's scan-out size at the display's
rate (`PanelDesc::modeinfo`, `objects.rs:100-160`, which takes the rate in force;
`clock`/`htotal`/`vtotal`/`vrefresh` all describe it). That rate is the panel's, reported
through `nativeSetPanelRefreshHz`, and the daemon serves it live so a display change is
applied to the running session. Neither the cap nor the frame-generation multiplier
enters it — they move what the session produces, not what the compositor runs at. The
rate clients see is therefore the display's:

- **Uncapped:** the loop reaches its measured ceiling (~259 presents/s); the grid is three
  orders of magnitude above the panel's frame time and never the limit.
- **Mode rate vs panel rate:** equal, always (a 40 cap at x4 on a 120 Hz panel runs a
  120 Hz mode and produces 160 frames a second).
- **Capping at 30/40/60 against the grid is exact arithmetic** — all three divide 1200, and
  a divisor cap lands on its target exactly (`-r 60` reads 60.0 presents/s).

## Open — to measure, not assume

- `active_display_info`'s rate list is the single mode rate above (there are no dynamic
  rates on this connector), so a client sees one entry — the display's rate.
