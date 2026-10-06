# gamenative-drm

Rust userspace display stack that lets [gamescope](https://github.com/GameNative/gamescope) run on
Android inside GameNative. Two cooperating halves:

1. **KMS emulator** — emulates a Linux DRM device so gamescope's DRM/KMS
   backend (and anything speaking the kernel DRM UAPI through libdrm) runs
   without a real KMS device.
2. **Vulkan WSI proxy ICD** — a client-side dma-buf swapchain for the
   gamescope session, so the game renders directly into scan-out buffers
   with no extra compositing hop.

Final pipeline:

```
game.exe ──▶ proton-wine (winex11 driver)
                  │ X11 (XWayland)
                  ▼
             gamescope (DRM backend, one XWayland for wine's X11)
                  │ libdrm ioctls on /dev/dri/card0
                  ▼  (LD_PRELOAD gndrm-shim intercepts → RPC over unix socket)
             gndrm daemon (KMS emulator, hosted in-app via JNI)
                  │ committed dma-bufs
                  ▼
             Android Surface (SurfaceControl presenter: AHB scan-out pool,
                              published with ASurfaceTransaction)
```

```
game (vulkan) ──▶ libadrenotools_gamescope.so (ICD shim)
                    ├─ ordinary processes: passthrough to the real turnip
                    │  driver via adrenotools (invisible when unused)
                    └─ gamescope session: WSI proxy engages
                         swapchain = shim-owned dma-heap ring imported into
                         vulkan; the app renders straight into scan-out
                         buffers. A present goes out over the guest's X11: DRI3 +
                         Present of the dma-buf into the app's root-child X
                         window (the window the compositor enumerates; winex11
                         names a nested client window, so the shim walks up to
                         its root child), paced by Present's idle fence, so one
                         window carries the guest's frame and wine's GDI 2D
                         together
```

Design + wire protocol: see [`docs/02-gamenative-drm.md`](docs/02-gamenative-drm.md),
beside the other design notes in [`docs/`](docs/README.md).

## Status on device

Working end to end: KMS emulation for gamescope's DRM backend, the sync-object
handshake, scan-out, the ICD shim's client dma-buf swapchain, and the
acquire/present/release handshake — game video and loading screens reach the
panel under the GameScope display-server option.

Open items are in the display path, not in the emulation: the present chain tops
out below the panel's rate, so a frame-rate target above that ceiling cannot hold
and the paced rate wobbles; compressed (UBWC) scan-out is refused rather than
translated; and the ICD embeds a build timestamp, so a packaged payload is not
bit-reproducible. See `docs/10-fps-limit.md`, `docs/11-refresh-model.md` and
`docs/12-present-latency-budget.md`.

Diagnostic switches (all default off): `GNDRM_LOG_*` selects a module to log (see
`modules/gndrm-daemon/src/log_policy.rs`), `GN_VKSHIM_*` affect the ICD's own
paths, `GN_VKSHIM_GUEST_SURFACE=1` selects the guest-surface present route, and
`GNDRM_DUMP_DIR` / `GAMENATIVE_DUMP_DIR` enable dumps (game process and app
process respectively). Winding compensation is unconditional (no switch): the
driver leaves effective winding alone under a flipped viewport, so the shim
always flips `frontFace` on culling pipelines.

## Why this shape

- **One XWayland, not a nested server**: wine talks X11 to the single XWayland
  gamescope spawns (`--xwayland-count` stays at its default of 1), so no second
  server sits between the game and the compositor.
- **Client-side swapchain in the ICD**: scan-out buffers are a shim-owned
  ring imported into vulkan, so a present is a DRI3 + Present of the same
  dma-buf into the app's X window — no intermediate copy, and Present's
  idle fence gives the exact backpressure.
- **LINEAR modifiers only**: the SurfaceControl path cannot consume UBWC; the
  daemon advertises `ADDFB2_MODIFIERS` + `IN_FORMATS` (LINEAR) on both planes
  so gamescope never picks a tiled layout.

## Layout

| path | what |
|---|---|
| `modules/gndrm-uapi` | `#[repr(C)]` mirror of the drm.h/drm_mode.h subset, ioctl numbers, wire framing codec. `forbid(unsafe_code)`; ABI cross-checked against the real C headers at test time |
| `modules/gndrm-daemon` | the KMS emulator core (host-testable, no Android deps): object store, ioctl dispatch, atomic commits, flip events, `Presenter` trait. Also `gndrm-hostd`, a socket-serving test/binary harness |
| `modules/gndrm-shim` | C (not Rust): the LD_PRELOAD `open`/`ioctl`/`close` interceptor injected into the gamescope process; raw-syscall passthrough, `gamenative_drm_connect()` export; plus `test_client.c` (KMS card) and `render_node_client.c` (render node) |
| `modules/gndrm-android` | `libgamenative_drm.so` JNI cdylib: daemon hosting (card + render listeners), the SurfaceControl presenter (`asc.rs`: AHardwareBuffer scan-out pool, `ASurfaceTransaction` publish, release-fence accounting), wayland virtual-input injector, `PRESENTED_FRAMES` counter |
| `icd_shim` | `libadrenotools_gamescope.so` cdylib: the vulkan ICD shim — adrenotools passthrough plus the dma-buf WSI proxy (wrapper handles, aarch64 wrapper-swap thunks, slot ring). NEEDEDs stay libc/libdl/libwayland-client plus the xcb DRI3/Present set |
| `utils` | `gn-dmabuf-test` + `gn-wsi-test` smoke clients (dma-buf feedback and `VK_KHR_wayland_surface` paths) |
| `tools/build-shim.sh` | builds the LD_PRELOAD shim for the host and (with `android` arg) for aarch64-android28 via the NDK |
| `tools/build-android.sh` | builds `libgamenative_drm.so`, `libadrenotools_gamescope.so`, `gn-dmabuf-test`, `gn-wsi-test` for aarch64-android28 (16 KB pages); `GAMESCOPE_TERMUX_PREFIX` supplies the link-time wayland/vulkan |
| `tools/b9-test.sh` | end-to-end host integration test: real libdrm card client + render-node client → shim → socket → `gndrm-hostd`, run separately and concurrently |
| `tools/check-x-present.sh` | host end-to-end check of the ICD's X11 present route: import a linear dma-buf with DRI3 `PixmapFromBuffer`, Present it into an X window, read it back, assert the idle fence released the slot (SKIPs without X / DRI3+Present) |
| `tools/check-x-route.sh` | asserts the shim hooks the surface entry point the guest reaches (`vkCreateXlibSurfaceKHR`; `VK_KHR_win32_surface` is winevulkan's client-side name and never reaches an ICD) and reports its route decision ungated — drives the entry point through `icd_probe` |
| `tools/check-x-probe.sh` | guards against a re-added init-time X-server dial in the ICD (a blocking setup read would deadlock Xwayland on itself) |
| `tools/xtree-probe.c` + `tools/build-xtree-probe.sh` | device evidence tool: dumps the live X window tree and the presenting window's `map_state`/`override_redirect`/class/geometry plus its parent chain over the session's own X socket. gamescope only tracks X windows that are direct children of the root, so the tree — not a guess — says whether it can compose the app's window |
| `tools/check-x-tree.sh` | runs that probe on device (`run-as`) against the gamescope session socket (diagnostic, not a gate) |

## Build & test

```sh
cargo test --workspace          # unit + golden + ABI-ground-truth tests
cargo clippy --workspace --all-targets   # gate: zero warnings
./tools/build-shim.sh           # host shim + b9 test client
./tools/build-shim.sh android   # Android aarch64 shim (NDK, API 28)
./tools/b9-test.sh              # full integration run (should print PASS)
./tools/check-x-present.sh      # host DRI3+Present end-to-end (needs an X server)
./tools/check-x-route.sh        # guest's real surface entry point + ungated route line
./tools/check-x-probe.sh        # no init-time X-server dial in the ICD
./tools/build-android.sh        # all four Android artifacts (append `debug`
                                # for a debug build)
```

Socket naming: `"/abs/path"` = filesystem socket; `"@name"` or bare `"name"`
= abstract namespace (both sides agree; Android uses a bare name, e.g.
`GAMENATIVE_DRM_SOCKET=gndrm`). The **card** node is that socket; the **render**
node (Xwayland's glamor/dri3 open) is a second listener on the same daemon at
`<name>-render`. They are separate because the card listener's post-primary
connections are all the single scan-out allocator channel, so a second DRM
client process on it would be adopted and closed as a bad scan-out channel —
which is why a render-node `open` used to fail with `EPIPE` (`Broken pipe`) on
device. `server::serve_render_listener` serves one `Device` per render client,
and both `gndrm-hostd` and `libgamenative_drm.so` run that same loop.

## KMS surface (what the emulator serves)

Kernel-ABI-accurate emulation of the mandatory KMS subset gamescope probes:
VERSION/GET_CAP/SET_CLIENT_CAP, resources/connector/CRTC/plane discovery,
properties + blobs, PRIME fd↔handle with `st_ino` dedup, AddFB2/GETFB2/RmFB,
GEM_CLOSE with kernel refcount semantics (the FB holds the ref), atomic
commits (TEST_ONLY/NONBLOCK/ALLOW_MODESET/PAGE_FLIP_EVENT/PAGE_FLIP_ASYNC),
IN_FENCE_FD via SCM_RIGHTS (signed range, `u64::MAX` sentinel on the wire),
sync objects (create/destroy/handle-to-fd, always-signalled form),
XBGR8888/ABGR8888 upload formats, flip events over a dedicated event
channel.

Sync objects are advertised because gamescope's DRM backend depends on them:
it creates one always-signalled object at init and rides the fd it exports as
the primary plane's in-fence on every commit. Exports are eventfds holding a
count, i.e. poll-ready, which is what "always signalled" has to mean for a
poll-based fence wait.

Deliberately not advertised: writeback, VRR, rotation (gamescope composites
rotation itself; see planning doc 02 open Q5).

### Render node (what Xwayland's glamor/dri3 opens)

Served on `<socket>-render`. A render node exposes only the render-capable
ioctls, gated exactly like the kernel's `DRM_RENDER_ALLOW` table: VERSION /
GET_CAP / SET_CLIENT_CAP, the GEM surface (`GEM_CREATE` allocating a real,
CPU-mappable, dma-buf-exportable buffer; `GEM_MMAP` minting the handle-carrying
offset the shim maps; `GEM_CLOSE`), PRIME both ways, and syncobj. Modesetting
and master acquisition are refused `EACCES`. `GET_CAP` reports
`DUMB_BUFFER=1` and `PRIME=IMPORT|EXPORT`, the caps Mesa's `gbm_create_device`
and glamor's device probe read before allocating.
