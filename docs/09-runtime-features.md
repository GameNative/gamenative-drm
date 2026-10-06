# 09 — gamescope runtime features (QuickMenu candidates)

What a client can change in a running gamescope, and what it can only read.
Everything below is already reachable from the app process: the input injector
(`modules/gndrm-android/src/injector.rs`) holds a wayland connection to
`$XDG_RUNTIME_DIR/gamescope-0` and binds `gamescope_control`, which is the same
route `nativeFps()` uses. No patch and no new transport is needed per feature.

## gamescope_control (protocol v7; each request's `since` shown)

Requests:
- `set_app_target_refresh_cycle(fps, flags)` — since 2. Frame-rate limit and/or
  refresh-rate target. `flags`: `internal_display` 0x1 (apply under the internal
  display too), `allow_refresh_switching` 0x2, `only_change_refresh_rate` 0x4
  (change the rate without applying the fps cap). `fps = 0` = native/uncapped.
- `take_screenshot(type, flags)` — since 3, answered by `screenshot_taken`.
  `type`: 1 base plane only, 2 game + overlays, 3 full composition, 4 screen
  buffer 1:1.
- `display_sleep(flags)` — since 4. `sleep` 0x1 / `wake` 0x2.
- `set_look(look, flags)` / `unset_look()` — since 5. gamescope's composite
  "looks" (colour/shaping presets).
- `request_app_performance_stats(app_id)` — since 6, answered by
  `app_performance_stats` with the frame delta. Already used by the FPS counter.
- `set_keyboard_layout(layout, variant)` — since 7.

Events (server → client):
- `feature_support` — one per supported feature, terminated by `done`. Entries:
  `reshade_shaders`, `display_info`, `pixel_filter`,
  `refresh_cycle_only_change_refresh_rate`, `mura_correction`, `look`,
  `perf_query`, `keyboard_layout`, `sgsr_filter`. Read this first: it is the
  authoritative answer to "can this build do X".
- `active_display_info` — connector name, display make/model, display flags
  (`internal_display` 0x1, `supports_hdr` 0x2, `supports_vrr` 0x4) and the list
  of valid refresh rates. This is what a refresh-rate or VRR control must be
  built on; it also tells us whether HDR/VRR are possible at all.

## Other gamescope protocols

- `gamescope_limiter` (v1) — read-only. `state(frame_limit_state)` on bind and on
  every change: 1 = limiter engaged, clients should present FIFO so the
  compositor paces them; 0 = no override. The way a UI can show that a limit is
  actually in force (it supersedes the old GAMESCOPE_LIMITER_FILE).
- `gamescope_private` — `execute(...)` runs a gamescope debug command
  (a concommand, e.g. `debug_set_fps_limit`) and reports back via
  `command_executed` / `log`. This is the escape hatch for anything that has a
  concommand but no protocol request.
- `gamescope_reshade` — ReShade effect management; pairs with the
  `reshade_shaders` feature flag.
- `gamescope_pipewire` — capture/screenshot plumbing.
- `gamescope_swapchain` — WSI for presenting clients; already used by the ICD
  shim (`override_window_content`).
- `gamescope_input_method`, `gamescope_action_binding` — input method and
  controller action bindings.
- `gamescope_xwayland` — Xwayland server control.

## Launch-time only (not settable at runtime)

`--framerate-limit N`, `--backend`, `--expose-wayland`, `-W/-H/-w/-h`,
`--debug-hud`, `--debug-focus`, and the rest of main.cpp's options. These are
fixed when the launcher builds the gamescope command
(`BionicProgramLauncherComponent.getFinalCommand`). Anything that must be
runtime-settable has to go through a protocol request above.

## Suggested QuickMenu mapping

| QuickMenu item | Mechanism | Confidence |
| --- | --- | --- |
| FPS limit | `set_app_target_refresh_cycle(fps, 0)` | high (see doc 10) |
| Refresh rate | `set_app_target_refresh_cycle(…, allow_refresh_switching)` fed by `active_display_info.valid_refresh_rates` | medium — needs the rate list plumbed to the UI |
| Limiter engaged indicator | `gamescope_limiter.state` | high |
| Screenshot | `take_screenshot(type, 0)` | high |
| Display sleep/wake | `display_sleep(sleep/wake)` | high |
| gamescope "look" | `set_look` / `unset_look`, gated by `feature_support`/`look` | medium |
| ReShade | `gamescope_reshade`, gated by `reshade_shaders` | medium |
| HDR / VRR | read `active_display_info` flags first; the flags say whether the display path supports them at all | low — display flags only; the actual enable paths are not in this protocol |
