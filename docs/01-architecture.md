# 01 — Architecture

## 1. End-to-end picture

```
┌─────────────────────────── GameNative app process ───────────────────────────┐
│                                                                              │
│   XServerDisplayActivity (extended)                                          │
│     └─ SurfaceView ──► ANativeWindow ──┐                                     │
│                                        │                                     │
│   gamenative-drm (Rust, JNI cdylib)    │                                     │
│     • DRM UAPI server on unix socket   │                                     │
│     • scan-out AHardwareBuffer pool ───┘ (daemon-owned, RGBA)                │
│     • SurfaceControl/Transaction publish ──► Android Surface                 │
│     • async commit worker + flip-event queue                                 │
│     • input injector (wayland virtual input)                                 │
└──────────────▲───────────────────────────────────────────────────────────────┘
               │ abstract unix socket $GAMENATIVE_DRM_SOCKET
               │ (ioctl RPC + SCM_RIGHTS dma-buf fds + raw drm_event stream)
┌──────────────┴───────────────────────────────────────────────────────────────┐
│ gamescope process (bionic, termux prefix)                                    │
│   libgndrm_shim.so (LD_PRELOAD, C):                                          │
│     intercepts ioctl() on the fake DRM fd → RPC to daemon                    │
│     requests scan-out buffers by geometry; exports gamenative_drm_connect()  │
│   gamescope (DRM backend, atomic-only) ── wlroots-0.20 wayland server        │
└──────────────▲───────────────────────────────────────────────────────────────┘
               │ XWayland (X11 protocol) + wayland protocol
┌──────────────┴───────────────────────────────────────────────────────────────┐
│ proton-wine                                                                  │
│   winex11.drv → XWayland → gamescope;  game.exe (DXVK→Vulkan)                │
└──────────────────────────────────────────────────────────────────────────────┘
```

## 2. gamescope source facts (the constraints gndrm is built against)

1. **wlroots is a hard dependency.** `src/meson.build:26` requires `wlroots-0.20`
   (vendored subproject fallback with `backends=libinput, renderers=[],
   allocators=[], session=enabled`). The wayland server (`wlserver.cpp`), seats,
   xwayland, and dmabuf/format helpers are wlroots.
2. **The DRM backend is atomic-only.** No `drmModePageFlip`, no legacy modeset
   path: `drmModeAtomicCommit` (`src/Backends/DRMBackend.cpp:1291`-area), plus
   `DRM_CLIENT_CAP_ATOMIC` required.
3. **No GBM.** The scan-out buffer is a dma-buf handed to gamescope by the shim
   (from the daemon's AHardwareBuffer pool). Gamescope renders into it, then
   registers it with `drmPrimeFDToHandle` → `drmModeAddFB2` → atomic commit sets
   `FB_ID` on the primary plane → on release `DRM_IOCTL_GEM_CLOSE` +
   `drmModeRmFB`.
4. **Device open is the single patch point for display.** `init_drm`
   (`DRMBackend.cpp:1250`): `vulkan_primary_dev_id()` →
   `drmGetDeviceFromDevId()` → `drm->fd = wlsession_open_kms(drm->device_name)`.
   `wlsession_open_kms` is gamescope's own wrapper (`src/wlserver.cpp:1833`) over
   `wlr_session_open_file` / `wlr_session_find_gpus`. With `$GAMENATIVE_DRM_SOCKET`
   set, the whole dev-id/session path is skipped and the connected socket fd is
   returned.
5. **Capability negotiation is the compatibility lever** (`DRMBackend.cpp:1291-1330`):
   `drmSetClientCap(ATOMIC)`, `drmGetCap(CURSOR_WIDTH/HEIGHT)`, `DRM_CAP_SYNCOBJ`,
   `DRM_CAP_ADDFB2_MODIFIERS`, `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP`. Whatever the
   daemon reports as unsupported takes gamescope down its own fallback paths.
   **Design rule: implement the mandatory subset; let caps gate everything
   optional.**
6. **Sync objects are part of the UAPI subset.** The daemon advertises
   `DRM_CAP_SYNCOBJ=1` and implements the syncobj ioctls (create/destroy,
   fd handoff, timeline), so gamescope's explicit-sync path runs. `IN_FENCE_FD`
   is the implicit-sync acquire fence on the primary plane; `OUT_FENCE_PTR` is
   the release fence.
7. **Events are plain `read()`s.** The backend polls `g_DRM.fd` and calls
   `drmHandleEvent` (`DRMBackend.cpp:810/838`), which `read()`s
   `struct drm_event*` records. The daemon writes raw `drm_event`
   (`DRM_EVENT_FLIP_COMPLETE`) bytes to the socket — no ioctl interception is
   needed on the event path.
8. **vblank has a timerfd fallback.** `vblankmanager.cpp` supports a timerfd
   mode, so the daemon's event cadence is a refinement, not a hard requirement.
9. **Session/VT handling is wlroots-session-based and is bypassed.** wlserver
   session init (`wlserver.cpp:1800`+, `wlr_session_create`) needs
   libseat/logind/direct-tty — none exist on Android. The `$GAMENATIVE_DRM_SOCKET`
   patch makes the backend non-session-based (no VT switching, no DRM master —
   the daemon always owns its fake device).
10. **Input on the DRM backend is libinput/udev, which is dead on Android and
    cut entirely.** Upstream, KMS input comes from wlroots' libinput backend
    (`wlserver.cpp:2183`, gated on `IsSessionBased()`), a touch→monitor heuristic
    using libinput/udev (`wlserver.cpp:427-448`), and `src/LibInputHandler.cpp`.
    None of these can function on Android (no `/dev/input/event*` access without
    root, no logind/seatd session). Gamescope is therefore built **without
    libinput** (patch M3), and input is injected the way nested compositors do:
    GameNative captures touch/keys/gamepads at the Android layer and feeds them
    into gamescope's seat via the standard wlroots virtual-input protocols
    (`zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1`). Wine then receives
    input from gamescope like any focused X11 client. Build-side details in
    03-gamescope-build.md §4 (M3/I1), app-side injector in
    04-gamenative-app.md §3.7.

## 3. The two-fd channel design

Gamescope must believe it holds a real DRM fd. One socket can't cleanly serve
both synchronous ioctl RPC (framed) and async events (raw `drm_event` byte
stream that `drmHandleEvent` `read()`s unparsed), so the connection is:

| fd | Who holds it | Direction | Format |
|---|---|---|---|
| **E** (event) | gamescope `g_DRM.fd` | daemon → gamescope | raw serialized `struct drm_event` records; `poll()`/`read()` work natively, zero interception |
| **C** (command) | shim-private, dup'd at connect | gamescope → daemon (request), daemon → gamescope (reply) | length-prefixed RPC; dma-buf fds pass via SCM_RIGHTS |

`ioctl(E, …)` is intercepted by the shim (LD_PRELOAD), which recognizes `E`,
performs the RPC over `C`, and returns. Every `drmMode*` helper in real libdrm
(built for Android, linked normally) bottoms out in `drmIoctl`→`ioctl`, so
**all** KMS calls funnel through the shim with no per-API work.

`open()`/`stat()` of `/dev/dri/*` never happens on the patched path (the env-var
patch connects the socket directly). `drmGetDevices2`/`drmGetDeviceFromDevId`
are skipped by the same patch.

## 4. Wire protocol sketch (versioned, little-endian)

```
req  := { u32 len; u32 proto_version; u32 ioctl_nr; u32 flags; u8 payload[len] } + SCM_RIGHTS fds?
resp := { u32 len; i32 ret; i32 errno_; u8 payload[len] } + SCM_RIGHTS fds?
```

- Payloads are the kernel UAPI structs from `include/uapi/drm/drm.h` /
  `drm_mode.h` (C layout; the Rust daemon defines them with `#[repr(C)]` via a
  shared `gndrm-uapi` crate; the C shim uses the same system headers as libdrm —
  single source of truth is the kernel header the user linked).
- Variable-length queries (resources, properties, modes) follow the kernel's
  two-call pattern: call 1 fills counts, call 2 fills arrays — the daemon
  implements identical semantics, so no protocol changes are needed.
- FD passing: `DRM_IOCTL_PRIME_FD_TO_HANDLE` sends the dma-buf fd with the
  request; `PRIME_HANDLE_TO_FD` returns one with the reply. The scan-out buffer
  request is the same shape: the shim asks for a buffer by geometry and receives
  a dup'd dma-buf fd via `SCM_RIGHTS`.
- Events: the commit worker emits `DRM_EVENT_FLIP_COMPLETE` (+ vblank
  sequence/timestamps) on E with each presented frame's publish, honoring
  `DRM_MODE_ATOMIC_EVENT` / `DRM_EVENT_*` semantics.

## 5. Presentation (daemon)

The daemon owns the scan-out AHardwareBuffer pool and publishes committed frames
with `ASurfaceControl`/`ASurfaceTransaction` (dlopen'd from `libandroid.so`).
Gamescope renders directly into the pool buffer (RGBA, byte-for-byte
`XBGR8888`/`ABGR8888`), and the same buffer is what SurfaceFlinger scans out —
no copy, no conversion. Back-pressure comes from
`ASurfaceTransactionStats_getPreviousReleaseFenceFd`; a warm spare keeps one
pre-allocated buffer ready. Full details in 06-presenter-latency.md.

## 6. Input path (inverse direction)

GameNative turns touch/overlay/gamepad input into wayland virtual-input events.
An input injector thread inside `libgamenative_drm.so` connects to gamescope's
wayland socket and creates one `zwlr_virtual_pointer_v1` + one
`zwp_virtual_keyboard_v1` (gamescope's I1 globals), raising pointer/keyboard
events on the wlroots seat. Wine then receives input from gamescope through
XWayland. Wire format is the wayland protocol, not the DRM channel — the DRM
socket stays display-only. App-side details in 04-gamenative-app.md §3.7.

## 7. Process & lifecycle

1. GameNative launches a container with Display Server = GameScope → the display
   activity starts, creates the SurfaceView, loads `libgamenative_drm.so` (JNI),
   and the daemon begins listening on an abstract socket, passing
   `GAMENATIVE_DRM_SOCKET` + `LD_PRELOAD=libgndrm_shim.so` (+ `WAYLAND_DISPLAY`,
   `XDG_RUNTIME_DIR`, `WLR_XWAYLAND`) into the guest env (same EnvVars mechanism
   used for `DISPLAY`).
2. The guest launcher starts `gamescope -- wine game.exe` inside the bionic
   prefix.
3. Gamescope connects → daemon handshakes → KMS discovery → atomic commits →
   frames.
4. Surface destroy / activity pause → the daemon drops presents, signals
   gamescope (hotplug/disconnect event), and tears down on process death (socket
   close = clean exit).
