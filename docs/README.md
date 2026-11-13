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
| mesa | `gamescope/android/mesa-glamor` | — | The Mesa the payload ships (zink + softpipe, LLVM off, pinned 26.2.6), fetched and built **in-tree** into `gamescope/build-android-mesa-glamor/stage`, which `package-gamescope.sh` itself builds when the stage is missing or older than that source, then reads |

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
15. [15-fps-limiter-pacing-and-vsync.md](15-fps-limiter-pacing-and-vsync.md) — where a frame's rate is decided: the container's present mode, gamescope's limiter under FIFO or the shim's gate on the display's vblank grid otherwise, and the presenter's latch grid; and where vsync is real.
16. [16-hdr-verification-and-gaps.md](16-hdr-verification-and-gaps.md) — what HDR verification measured, and the gaps left open.
17. [17-lsfg-frame-generation.md](17-lsfg-frame-generation.md) — plan for a `gamenative_drm` LSFG backend: the app's own engine inside the ICD shim, reading the container's `Lossless.dll` and building its own shader cache, reusing `LsfgVkManager` and the quick-menu tab, with the artifacts and the measurements that decide it.
18. [18-lsfg-driver-facts.md](18-lsfg-driver-facts.md) — the properties the LSFG presenter rests on, each one silent when broken: queue-family matching, the engine writing only into its own images, the blit capture, destination depth, guest-only release pacing, the `Rgba8` storage binding, and the settings channel that carries the DLL path.
19. [19-fps-limiter-acquire-rework.md](19-fps-limiter-acquire-rework.md) — plan for the limiter rework: cap at acquire instead of at release, park A34 (split from its `-r` mode fix), and count acquisitions for the app's frame ring.
20. [20-swapchain-maintenance1.md](20-swapchain-maintenance1.md) — `VK_KHR_swapchain_maintenance1` served by the ICD shim: present fences, surface-maintenance capabilities, image release; why the leaked KHR name froze DX12 titles; the per-present mode switch left for a follow-up.
21. [21-wsi-extension-policy.md](21-wsi-extension-policy.md) — which surface, swapchain and present extensions the guest sees under the WSI proxy: the registry-derived WSI scope, the served/hidden table in `wsi_policy.rs`, the per-process log line, and how to classify a new extension.
22. [22-unresolvable-extensions.md](22-unresolvable-extensions.md) — the resolve probe: the reported apiVersion is clamped below the first core version whose commands do not resolve by exact name on the guest's instance (its Vulkan1xFeatures read false); the Honor GT 1.4 case, the probe line with its cost, the 1.0-instance skip, `GN_VKSHIM_NO_RESOLVE_PROBE=1`; extension hiding removed.
23. [23-strict-image-count.md](23-strict-image-count.md) — the games that fail unless the swapchain has exactly the image count they asked for: Mesa's strict list carried by the shim, what changes for them, the hidden slots behind their images and how to keep the list in step.
24. [24-release-notes.md](24-release-notes.md) — the scan-out release channel: the three messages and the ticket that pairs them, why the compositor's release is not the display's, the five-step rule that answers a park, what Nova, Odin 3 and RP6 each do with it, and the two counting/truncation defects that hid it.

### Performance

`docs/performance/` — the gamescope-vs-X11 CPU investigation: measurement, the
instrumented run, and one design document per plan item, in the order the plan
was worked. Evidence captures are under `docs/performance/evidence/`.

1. [01-gamescope-vs-x11-cpu.md](performance/01-gamescope-vs-x11-cpu.md) — measured CPU/power delta between the gamescope and in-app X11 paths, as an adjacent pair on the same APK and payload: +0.3 pts of 8 cores and −0.04 W, with the work redistributed (sys +1.0, user −0.9, the guest +5 pp). Stage map, the log gates a container can actually turn on, the causes re-ranked with four withdrawn, the step-1 run that closed three more plan items, and the input route that works.
2. [02-instrumentation.md](performance/02-instrumentation.md) — the zero-rebuild instrumented run, designed and then executed: the byte-anchored container edit that survives `sessionMetadata`, the gates that are and are not reachable from the container, exact grep strings, a parser, the decision table, and both arms' readings in §10 (no `VkPresentIdKHR`, zero present waits, zero slot waits, depth already 4, never FIFO).
3. [03-present-wait.md](performance/03-present-wait.md) — `VK_KHR_present_wait` in the shim: what is advertised, the `Mutex<Waits>`/`Condvar` waitable, the three answer points per present mode, the spec contract for answering early, the edit list, and the container-only 2×2 experiment that falsifies it first.
4. [04-slot-reap.md](performance/04-slot-reap.md) — the slot reap: the lifecycle and its stall sites, why the reap is a no-op on the default route, the epoch that is actually missing, hidden depth behind the guest's images, and the arithmetic that bounds the item at ~46 µs/frame.
5. [05-daemon-present-allocations.md](performance/05-daemon-present-allocations.md) — the daemon present path's per-frame allocations, the JNI lookup that also builds two `CString`s per publish, and reviewable diffs for each with the invariant it preserves.
6. [06-present-mode-declaration.md](performance/06-present-mode-declaration.md) — why `swapchain_set_present_mode` is load-bearing on every commit (gamescope clears it in `PrepareCommit`), what a latch would have to send for the generator path, and the honest size of the saving.
7. [07-compositor-scheduling.md](performance/07-compositor-scheduling.md) — `rt_cap` is a libcap purchase that cannot work for an app uid; what `nice -20` can actually buy, which thread is the late one, and why the verification is frame-time p99 and not cpu%.
8. [08-scanout-pixel-format.md](performance/08-scanout-pixel-format.md) — UBWC on the scan-out path: what the daemon allocates, why an `AHardwareBuffer` cannot express UBWC, the protocol field that would have to change, the prior attempt sitting unmeasured on a backup branch, and `TU_DEBUG=noubwc` as the free measurement that decides it.

## One-paragraph summary

Gamescope's DRM backend talks to the kernel exclusively through a device fd
(ioctls via libdrm + `read()` for events). gamenative-drm emulates that fd: a
Rust daemon hosted inside the GameNative app process listens on a unix socket
named by `GAMENATIVE_DRM_SOCKET`, and a small LD_PRELOAD shim in the gamescope
process redirects the DRM device open and all DRM ioctls to that socket. The
daemon implements the kernel DRM UAPI atomic-KMS subset (resources/connector/
CRTC/plane discovery, properties/blobs, PRIME import/export, AddFB2/GetFB2,
atomic commit, flip events, sync objects). It owns the scan-out
`AHardwareBuffer` pool (BGRA, HAL value 5, or RGBA, HAL value 1) and publishes each committed frame
with `ASurfaceControl`/`ASurfaceTransaction`, so gamescope renders directly into
the buffer SurfaceFlinger scans out. Commits validate and queue and return
immediately; a per-CRTC commit worker presents and emits the flip event with the
publish — matching a real kernel DRM driver. Input is
app-injected into gamescope's compositor over the wayland virtual-input
protocols (no libinput).
