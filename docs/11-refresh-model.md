# 11 — the refresh model: `g_nOutputRefresh` vs `g_nNestedRefresh` (`-r`)

## Two variables, two jobs

**`g_nOutputRefresh`** (mHz, `main.cpp:305`) — the **panel mode's** rate, assigned by the
DRM backend from the selected mode (`Backends/DRMBackend.cpp:3695`). It is what the
whole stack *and the clients* are told:

- the Xwayland server mode (`steamcompmgr.cpp:7356`, `:10382`),
- the `_GAMESCOPE_DISPLAY_REFRESH_RATE_FEEDBACK` root property (`:9283-9288`),
- `gamescope_control.active_display_info`'s rate list (`wlserver.cpp:1538-1541`),
- mangoapp's snapshot (`mangoapp.cpp:205`), ReShade (`reshade_effect_manager.cpp:293`).

**`g_nNestedRefresh`** (mHz, `main.cpp:299`) — `-r/--nested-refresh`; **0 means fall back
to the output refresh** (`main.cpp:749` rejects only negatives). In the nested backends it
*becomes* the output refresh (`SDLBackend.cpp:230`, `WaylandBackend.cpp:1989`,
`HeadlessBackend.cpp:112`). In the **DRM** backend — ours — it is passed to
`init_drm( &g_DRM, w, h, g_nNestedRefresh )` (`Backends/DRMBackend.cpp:3910`) and stored as
`drm->preferred_refresh` (`:1503`).

## A preferred refresh selects the mode

`find_mode( connector, w, h, preferred_refresh_hz )` (`DRMBackend.cpp:1385`) matches
`mode->vrefresh` **exactly** (`:1029`), and where a preference exists `g_nOutputRefresh` is
taken from it (`:3628`, `:3647`).

So in the DRM backend **`-r N` is a declaration that the display is N Hz** — it reaches
mode selection, the client-visible refresh and the grid together. It is not a cap layered
on top of a 120 Hz grid.

## What the grid and the pacing read

- **Grid:** `CVBlankTimer::GetRefresh()` = `g_nNestedRefresh ?: g_nOutputRefresh`
  (`vblankmanager.cpp:77`); the interval comes from `mHzToRefreshCycle(GetRefresh())`
  in `GetNextVBlank()`, and the same expression times the timer's idle sleep (`:405`).
- **App pacing:** `g_SteamCompMgrAppRefreshCycle = mHzToRefreshCycle( g_nNestedRefresh ?:
  g_nOutputRefresh )` (`steamcompmgr.cpp:10492`), consumed per window at `:8246-8253`
  (that is what throttles the guest's frame callbacks).
- **Limiter divisor base:** `nRefreshHz` = the same nested-expression (`:6911`); the
  divisor only applies while `g_nSteamCompMgrTargetFPS != 0`.

Both the grid and the app pacing come from **one** expression, so there is no way to pace
the app at N while keeping the grid at the mode rate — they move together.

## Consequences on the session's own rate

gndrm advertises **one** preferred mode — `1280x960@1200`, from its own
`SESSION_REFRESH_MHZ` constant (`gndrm-daemon/src/objects.rs`,
`vrefresh: SESSION_REFRESH_MHZ.div_ceil(1000)`), not the panel's rate — so a
preferred refresh with no matching exposed mode has nothing to select, and the
grid, declared output refresh and client-visible refresh move together with `-r`.

- **Uncapped:** `-r 0` falls back to the output refresh (1200 Hz); the loop
  reaches its measured ceiling (~259 presents/s).
- **`-r N`:** declares N Hz everywhere — the guest is told N, the grid runs at N —
  while the single 1200 Hz DRM mode is kept.
- **Capping at 30/40/60 is exact arithmetic** — all three divide 1200, and a
  divisor cap lands on its target exactly (`-r 60` reads 60.0 presents/s).

## Open — to measure, not assume

- The `active_display_info` refresh list carries the connector's mode list, not
  `g_nOutputRefresh`; the client-visible refresh for a low `-r` was not measured
  directly on device.
