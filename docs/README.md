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
6. [06-presenter-latency.md](06-presenter-latency.md) — the SurfaceControl presenter: AHardwareBuffer scan-out pool, the scan-out alpha invariant, and `ASurfaceTransaction` publish.
7. [07-kernel-drm-parity.md](07-kernel-drm-parity.md) — the async commit + vblank event-queue model.
8. [08-xwayland-render-node.md](08-xwayland-render-node.md) — the render-node path: Xwayland's second DRM listener, and why the card and render nodes are served separately.
9. [09-runtime-features.md](09-runtime-features.md) — the gamescope runtime controls behind the QuickMenu Gamescope tab, which of them the control connection can actually carry, and Scaling Mode's guest-X-client transport.
10. [10-fps-limit.md](10-fps-limit.md) — how the app's fps limit reaches gamescope, what the deadline grid and the commit deferral each do, and what the divisor can express.
11. [11-refresh-model.md](11-refresh-model.md) — `g_nOutputRefresh` against `g_nNestedRefresh` (`-r`): what each drives, and what that means when gndrm advertises its own session rate rather than the panel's.
12. [12-present-latency-budget.md](12-present-latency-budget.md) — the present-latency budget: what each frame on the paced path pays, and why the flip event is emitted with the publish.
13. [13-single-layer-windowing.md](13-single-layer-windowing.md) — the window/layer model on the single-layer path: what plays layer 0/1/N here, why the content-override mechanism exists, the per-patch layer audit, and the target architecture.
14. [14-guest-surface-commit-stall.md](14-guest-surface-commit-stall.md) — the guest-surface route's queued defect: the commit stream stalls when the guest's Vulkan swapchain comes up, the two candidate causes, and the instrumentation that would decide between them.
15. [15-fps-limiter-pacing-and-vsync.md](15-fps-limiter-pacing-and-vsync.md) — the fps limiter's deadline grid and how it meets the panel's vsync.
16. [16-hdr-verification-and-gaps.md](16-hdr-verification-and-gaps.md) — what HDR verification measured, and the gaps left open.
17. [17-lsfg-frame-generation.md](17-lsfg-frame-generation.md) — plan for a `gamenative_drm` LSFG backend: the app's own engine inside the ICD shim, reading the container's `Lossless.dll` and building its own shader cache, reusing `LsfgVkManager` and the quick-menu tab, with the artifacts and the measurements that decide it.
18. [18-lsfg-driver-facts.md](18-lsfg-driver-facts.md) — the properties the LSFG presenter rests on, each one silent when broken: queue-family matching, the engine writing only into its own images, the blit capture, destination depth, guest-only release pacing, the `Rgba8` storage binding, and the settings channel that carries the DLL path.
19. [19-fps-limiter-acquire-rework.md](19-fps-limiter-acquire-rework.md) — plan for the limiter rework: cap at acquire instead of at release, park A34 (split from its `-r` mode fix), and count acquisitions for the app's frame ring.

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
immediately; a per-CRTC commit worker presents and emits the flip event with the
publish — matching a real kernel DRM driver. Input is
app-injected into gamescope's compositor over the wayland virtual-input
protocols (no libinput).
