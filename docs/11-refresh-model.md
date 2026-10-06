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
the app at N while keeping the grid at the panel rate — they move together.

## Consequences on a native 120 Hz panel

- **Uncapped:** `-r 0` is the optimum. Mode, grid and the client-visible refresh all stay
  at the panel's real 120 Hz and the loop reaches its measured ceiling (~56.5 presents/s).
- **`-r N`, N ≠ panel rate:** declares N Hz everywhere — the guest is told N, the grid runs
  at N, and the achievable ceiling follows the grid (measured: declared 60 → ~39.8/s
  uncapped).
- **Capping at 30/40/60 on a 120 Hz panel is exact arithmetic** — all three divide 120. The
  measured shortfall is therefore not the divisor: the paced interval behaves as
  `1 / (1/target + C)` with **C ≈ 8.7 ms** (fits 30→23.6, 40→29.4, 60→39.7). The divisor
  itself was verified correct (presents land exactly every D vblank indices).
- **gndrm exposes a single preferred mode** (`gndrm-daemon/src/objects.rs:110`,
  `vrefresh: self.refresh_mhz.div_ceil(1000)`), so a preferred refresh with no matching
  exposed mode has nothing to select.

## Open — to measure, not assume

- Whether `-r N` actually changes the selected mode (`drm: selecting mode …`) and what
  `g_nOutputRefresh` ends up as when the connector exposes no N-Hz mode.
- Whether the ~8.7 ms pacing cost is the FIFO commit deferral that was introduced to make
  the limiter engage at all.
