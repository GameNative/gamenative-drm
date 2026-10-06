# 05 — XWayland windowing

GameScope's client windowing runs Wine's `winex11.drv` against an XWayland
server. The gamenative-drm backend (atomic KMS, virtual input, proxy WSI shim)
is unchanged; this doc covers how Wine clients connect and how their frames
reach the compositor.

## 1. Target architecture

```
game.exe
  └─ proton-wine (winex11.drv)
       ├─ X11 windowing/cursor/input ──► XWayland (rootless, SHM, no glamor)
       │                                    │ wl_surface (per X window)
       │                                    ▼
       │                                 gamescope (steamcompmgr, X11 path)
       ├─ vulkan: vkCreateXlibSurfaceKHR ──► gn ICD shim (intercepts)
       │      creates proxy wl_surface + binds it to the X window ID
       │      via gamescope private protocol ──────────► gamescope
       │      presents dmabuf frames on the proxy surface
       │                                    │
       ▼                                    ▼
                            gamenative-drm (atomic KMS) ──► Android Surface
```

- 2D content (GDI, launchers, menus): wine → X11 → XWayland → wl_shm →
  gamescope. Software-rendered; negligible bandwidth; off the frame hot path.
- Game frames: wine vulkan → shim proxy swapchain → dma-buf → gamescope →
  atomic flip. Zero copies, zero X round trips, no GPU work inside XWayland.
- Cursor: X11 cursor → XWayland → wl_cursor → gamescope MouseCursor (the
  upstream-tested path).
- Input: gamescope virtual input (I1) → wl seat → XWayland → XInput2 → wine.
  Relative mouse via pointer constraints (`--force-grab-cursor`).

## 2. XWayland binary: `gamenative-xwayland`

XWayland ships as its own payload, built from source with the NDK in the
`gamenative-xwayland` repo (same conventions as `gamenative-drm`:
`tools/build-android.sh`, cargo-style payload staging, ELF dep audit). The
Termux project's `xorg-xwayland` recipe is the reference for the bionic quirks;
everything lives under GameNative's app-private dir and is located via env
vars, not hardcoded paths.

Payload (extracted to `filesDir/xwayland/`):

```
filesDir/xwayland/
  bin/Xwayland
  bin/xkbcomp
  share/X11/xkb/        (xkeyboard-config data)
  lib/                  (bundled deps: pixman, xkbfile, Xfont2, ...)
```

Build shape (xserver `xwayland-24.1.13` tag, meson):

- `-Dxwayland=true -Dxorg=false -Dxnest=false -Dxvfb=false`
- `-Dglamor=false` — glamor needs GBM, which does not exist on Android. SHM
  path only: game frames never touch XWayland.
- `-Ddri3=false` — DRI3 needs gbm/drm rendering nodes.
- `-Dmitshm=true` + **libandroid-shmem** — bionic has no SysV IPC; MIT-SHM is
  required by winex11 for image transfers. libandroid-shmem (ashmem-backed) is
  the established emulation.
- `-Dxdmcp=false -Dsecure-rpc=false -Dudev=false -Dsystemd_logind=false
  -Dlibunwind=false`
- Deps to build/bundle: pixman, xtrans (headers), libxkbfile, libXfont2
  (+ freetype, libfontenc), wayland-client (already built for gamescope),
  xkbcomp (runtime keymap compiler), xkeyboard-config (runtime data).
- `-Dxkb_dir=`, `-Dxkb_bin_dir=`, `-Dxkb_output_dir=` pointed at the payload
  layout.

Runtime integration needs no gamescope patches: wlroots spawns XWayland with
`execvp(getenv("WLR_XWAYLAND") ?: XWAYLAND_PATH)` — the `WLR_XWAYLAND` env
override is upstream. GameNative sets
`WLR_XWAYLAND=$filesDir/xwayland/bin/Xwayland` (and `XKB_CONFIG_ROOT` for
xkbcommon) when launching gamescope.

## 3. Gamescope windowing surface

- XWayland runs at its default count of one server; gamescope's upstream X11
  compositor paths (cursor, focus, stacking) are active.
- wlroots creates one `wl_output` per XWayland server, carrying the real output
  mode.
- `WLR_X11_SOCKET_DIR` (patch W2) redirects the X11 lock files and fs socket off
  the nonexistent `/tmp`; the launcher pre-creates the socket dir 0700.
- The upstream `gamescope_swapchain` protocol provides XID-bound surface
  routing: `gamescope_swapchain_factory_v2.create_swapchain(wl_surface)` +
  `gamescope_swapchain.override_window_content(server_id, x11_window)` re-points
  the window's `x11_surface_info` at the client's own surface and drains
  `g_PendingCommits` for it.
- The upstream wayland-native path remains for any wayland-native windows.

## 4. Shim changes

In `winex11.drv`, Wine vulkan surfaces are `VK_KHR_xlib_surface`
(`dlls/winex11.drv/vulkan.c`). The shim already sits on every vulkan entry
point, so:

- Advertise `VK_KHR_xlib_surface` alongside `VK_KHR_wayland_surface`; strip
  both from the real instance create (the driver never sees either).
- `vkCreateXlibSurfaceKHR` → open the shim's own wayland connection
  (`wl_display_connect(NULL)`; wine has no wayland display in X11 mode) →
  `wl_compositor_create_surface` → `create_swapchain` →
  `override_window_content(server_id=0, xid)`. The proxy surface becomes the
  commit target, so the swapchain/acquire/present/release path is unchanged.
- `vkGetPhysicalDeviceXlibPresentationSupportKHR` → true.
- Surface destroy tears down the swapchain object and the owned wl_surface.

## 5. GameNative app changes

- `BionicProgramLauncherComponent`: in gamescope mode, set `WLR_XWAYLAND`,
  `XKB_CONFIG_ROOT`, and `DISPLAY=:0`; use the default `--xwayland-count 1`;
  keep `--force-grab-cursor` available per-game.
- Container driver selection resolves to `winex11.drv` in gamescope mode
  (DISPLAY present, wayland driver not forced). The shim still reads
  `WAYLAND_DISPLAY` for its own proxy connection — only Wine's windowing driver
  changes.
- `XServerScreen.kt` ships the `xwayland-gamenative-<tag>.tzst` payload
  component next to the gamescope one.
- The Display Server dropdown shows "GameScope" unconditionally: X11 mode
  loads `winex11.drv`, and the shim's wayland connection is its own raw
  wayland-client code — no part of the gamescope path depends on how
  proton-wine is compiled.

## 6. Performance budget

- **Frame path:** game vulkan never touches X11; XWayland performs no GPU work
  (glamor off), so zero GPU contention.
- **2D path:** wine → ashmem (MIT-SHM, zero copy) → XWayland → wl_shm (memfd)
  → one texture upload per damaged region in gamescope. Only on actual 2D
  updates.
- **Input:** one extra IPC hop versus pure wayland (gamescope → XWayland →
  wine), sub-millisecond, same as Steam Deck.
- **Memory:** one XWayland instance ≈ 20–40 MB RSS + xkb data. Bounded and
  idle-quiet — relevant for 8 GB devices; `--xwayland-count 1` is the control
  knob.
- **Compositor:** unchanged atomic-only prepare/commit. Fullscreen game remains
  a single direct-scanout candidate; cursor composites only on repaint, which
  is vsync-driven anyway.
