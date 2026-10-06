# GameScope Integration for GameNative

Status: **shipped.** The full chain runs on device: Windows games render through
proton-wine into XWayland, gamescope composites them over its DRM backend, and
gamenative-drm publishes the result to an Android `Surface`. The winlator X11
path remains GameNative's default; GameScope is the opt-in display-server
choice.

## Goal

Run Windows games on Android through:

```
game.exe → proton-wine (winex11) → XWayland → gamescope (DRM backend) → gndrm → Android Surface
```

exposed as GameNative's "Gamescope" display-server option alongside the
winlator X11 path (which remains the default for normal launches).

## Repos & branches

| Repo | Path | Branch | Role |
|---|---|---|---|
| gamescope | `/data/workspace/Projects/gamescope` | `gamenative` | Valve micro-compositor, patched for Android; runs in the bionic/termux prefix |
| gamenative-drm | `/data/workspace/Projects/gamenative-drm` | `master` | Rust project: userspace DRM (KMS) emulation + SurfaceControl presenter + LD_PRELOAD shim |
| GameNative | `/data/workspace/Projects/GameNative` | `feat/gamescope-display` | App: Display Server setting, launch integration, JNI host for gamenative-drm |
| proton-wine | `/data/workspace/Projects/proton-wine` | `proton_11.0-2` | Wine: `winex11.drv` client driver + DXVK; the X11 client that talks to XWayland |
| gamenative-xwayland | `/data/workspace/Projects/gamenative-xwayland` | `gamenative` | XWayland: the X server the payload ships, built for the bionic prefix |
| mesa | `/data/workspace/Projects/mesa` | `build-xwayland` | The staged minimal Mesa the payload ships (zink + softpipe, LLVM off), built into `build-android-glamor/stage` |

## Documents

1. [01-architecture.md](01-architecture.md) — end-to-end architecture, IPC model, sync/input/display data flow, gamescope source facts.
2. [02-gamenative-drm.md](02-gamenative-drm.md) — the Rust userspace DRM daemon: DRM UAPI surface, wire protocol, LD_PRELOAD shim, scan-out allocation, presentation, threading.
3. [03-gamescope-build.md](03-gamescope-build.md) — meson configuration, dependency strategy for the bionic/termux prefix, wlroots config, patch list, packaging.
4. [04-gamenative-app.md](04-gamenative-app.md) — GameNative app changes: Display Server dropdown, launch path, wine driver steering, Surface host, JNI bridge, input.
5. [05-xwayland-reintroduction.md](05-xwayland-reintroduction.md) — client windowing via `winex11.drv` + XWayland (`gamenative-xwayland`), XID-bound proxy surfaces, cursor/input via the upstream X11 compositor path.
6. [06-presenter-latency.md](06-presenter-latency.md) — the SurfaceControl presenter: AHardwareBuffer scan-out pool and `ASurfaceTransaction` publish.
7. [07-kernel-drm-parity.md](07-kernel-drm-parity.md) — the async commit + vblank event-queue model.
8. [08-xwayland-render-node.md](08-xwayland-render-node.md) — the render-node path: Xwayland's second DRM listener, and why the card and render nodes are served separately.
9. [09-runtime-features.md](09-runtime-features.md) — the gamescope runtime controls behind the QuickMenu Gamescope tab, and which of them the control connection can actually carry.
10. [10-fps-limit.md](10-fps-limit.md) — how the app's fps limit reaches gamescope, what the deadline grid and the commit deferral each do, and what the divisor can express.
11. [11-refresh-model.md](11-refresh-model.md) — `g_nOutputRefresh` against `g_nNestedRefresh` (`-r`): what each drives, and what that means for a panel whose real rate is 120 Hz.
12. [12-present-latency-budget.md](12-present-latency-budget.md) — the present-latency budget: what each frame on the paced path pays, and the round-trip that was removed from it.

## One-paragraph summary

Gamescope's DRM backend talks to the kernel exclusively through a device fd
(ioctls via libdrm + `read()` for events). gamenative-drm emulates that fd: a
Rust daemon hosted inside the GameNative app process listens on a unix socket
named by `GAMENATIVE_DRM_SOCKET`, and a small LD_PRELOAD shim in the gamescope
process redirects the DRM device open and all DRM ioctls to that socket. The
daemon implements the kernel DRM UAPI atomic-KMS subset (resources/connector/
CRTC/plane discovery, properties/blobs, PRIME import/export, AddFB2/GetFB2,
atomic commit, flip events, sync objects). It owns the scan-out
`AHardwareBuffer` pool (BGRA, HAL value 5) and publishes each committed frame
with `ASurfaceControl`/`ASurfaceTransaction`, so gamescope renders directly into
the buffer SurfaceFlinger scans out. Commits validate and queue and return
immediately; a per-CRTC commit worker presents, waits the SurfaceFlinger latch,
then emits the flip event — matching a real kernel DRM driver. Input is
app-injected into gamescope's compositor over the wayland virtual-input
protocols (no libinput).
