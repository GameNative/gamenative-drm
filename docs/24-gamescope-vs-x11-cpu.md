# 24 — gamescope vs X11: where the extra CPU goes, and how to take it back

Status: **measured, causes ranked, no fix landed.** This is the reference for
any work that tries to make the gamescope display path cost the same CPU as the
in-app X11 path on an idle, GPU-bound scene.

Scope: Hades II (Steam 1145350), first save slot (Crossroads, night 31), Odin3
(SM8550-class, 8 cores), container 1280x720, panel 120 Hz on charge, gamescope
payload `20261110.1`, both arms sampled for ~210 s after the scene settled.

## 1. What was measured, and how

Two arms, same container except `displayServer`, same save, same idle spot,
same thermal state (~5 min screen-off cool-down before each arm; both arms
saturate `cpuss` at 102 °C within three minutes, so the arms are comparable by
temperature as well as by scene).

| instrument | source | rate |
|---|---|---|
| CPU busy/user/sys, per-core freq | `/proc/stat`, `cpufreq/scaling_cur_freq` | 1 Hz |
| GPU busy/clock | `kgsl-3d0/gpubusy`, `kgsl-3d0/devfreq/cur_freq` | 1 Hz |
| package power | `usb current·voltage − battery current·voltage` | 1 Hz |
| die temps | `thermal_zone` matched by **type** (`cpuss-0-0`, `gpuss-0`, `aoss-0`) | 1 Hz |
| process attribution | `top -n 1 -b -m 14` mean over the window | 10 s |
| frame rate | MangoHud overlay read out of a screencap, 5 reads | 7 s |

Power is the charge-path subtraction because the battery node reads **negative**
while charging on this device; the USB rail alone (~10.2 W) is not the package.

## 2. Result

`20261110.1` narrowed the gap this report set out to explain. The same
comparison on payload `20261105.4` measured +3.8 points of 8-core CPU busy; on
`20261110.1` it is +1.5.

| metric | X11 | gamescope `20261110.1` | delta |
|---|---|---|---|
| fps (5 MangoHud reads) | 344.6 | 339.2 | −1.6 % |
| CPU busy (8 cores) | 45.8 % | 47.3 % | **+1.5 pts** |
| CPU user | 21.5 % | 21.0 % | −0.5 pts |
| CPU sys | 20.3 % | 21.9 % | **+1.6 pts** |
| GPU busy / clock | 83.8 % / 760 MHz | 84.5 % / 759 MHz | +0.7 pts |
| package power | 19.18 W | 19.59 W | **+0.41 W (+2.1 %)** |
| cpuss / gpu / soc | 101.7 / 76.8 / 78.6 °C | 101.9 / 76.3 / 78.0 °C | — |
| RAM used | 6.48 GB | 6.60 GB | +0.12 GB |

Caveat that must be closed before quoting the absolute delta: the X11 row was
taken on the APK **before** the `20261110.1` update (its container and payload
are unchanged by that update, but the app-side presenter is not). Re-measure X11
on the same APK before publishing a number; the direction and the *shape* of the
delta — sys-bound, not user-bound, at equal GPU load — are what the causal
analysis below rests on.

### Per-process attribution (matched pair, both arms on the older build)

| process | X11 | gamescope | Δ (pp of one core) |
|---|---|---|---|
| `Hades2.exe` (guest, incl. the ICD shim) | 293.9 | 314.7 | **+20.8** |
| `app.gamenative` (daemon + SF plumbing) | 17.5 | 24.7 | +7.2 |
| `surfaceflinger` | 9.4 | 13.1 | +3.7 |
| `gamescope` | 0 | 6.1 | +6.1 |
| `wineserver` | 10.6 | 7.7 | −2.9 |
| listed sum | 331.4 | 366.3 | +34.9 |

**These rows are not comparable populations, and that is the single most
important framing fact:** in X11 mode the X server runs *inside*
`app.gamenative` (`XServerComponent` starts `XServer` through `XConnectorEpoll`
in-process, and the frame leaves through `VulkanRenderer`/`ASurfaceRenderer` in
the same process). Under gamescope, `app.gamenative` is only the gndrm daemon;
Xwayland and gamescope are separate processes, and Xwayland lands in the
"unlisted" bucket, which *fell* 4.5 pp between arms. So "the app got 7 pp more
expensive" is really "a compositor moved out of the app process, and the guest
picked up a WSI".

The load-bearing conclusion: **more than half of the extra CPU is inside the
guest process**, because under gamescope the guest's Vulkan WSI *is* our code —
`VK_ICD_FILENAMES` points at the payload's adrenotools ICD
(`BionicProgramLauncherComponent.applyGamescopeEnv`) and the ICD shim
(`icd_shim/src/proxy.rs`) does the acquire/present/release work in
`Hades2.exe`'s address space. In X11 mode Mesa's X11 WSI does that job, and it
is already counted inside Mesa/DXVK, i.e. also in the guest — but with a
shorter path and no cross-process handoff.

## 3. The pipeline, stage by stage (with the log gate that proves each stage)

Two present routes exist in the shim and the **default is not the X11 one**:

| route | selected by | what happens |
|---|---|---|
| **override/Wayland (default)** | `use_guest_surface()` false (`proxy.rs:9655`) | the shim owns a `wl_surface`, binds `gamescope_swapchain` to wine's X window via `override_window_content`, and commits the pool dma-buf per frame |
| DRI3 + Present (opt-in) | `GN_VKSHIM_GUEST_SURFACE=1` | `xcb_present_pixmap` into wine's own window, one X round trip + one idle-fence query per in-flight slot per frame |

Zero-copy holds end to end on the default route: the swapchain image *is* a
daemon-owned scan-out buffer (`gn_vkCreateSwapchainKHR` →
`scanout::allocate_ahb/allocate`), the wayland attach imports its dma-buf
(`slot_make_buffer` → `zwp_linux_dmabuf_v1.create_immed`), and the
SurfaceControl publish re-publishes the same `AHardwareBuffer`. There is no
steady-state full-frame copy and no readback.

| # | stage | site | cost owner |
|---|---|---|---|
| 1 | present dispatch, present-id/mode extraction, slot fence reset | `proxy.rs:3238`–`3357` | guest main |
| 2 | **render-completion submit** (waits the guest's present semaphores) + an extra empty submit when a present fence is chained | `proxy.rs:3415`, `:3418` | guest main |
| 3 | present discipline (`fifo`/`mailbox`/`immediate`), mode census under a global mutex | `proxy.rs:3113`, `:10504` | guest main |
| 4 | present worker: fence wait → commit bookkeeping → **present mark datagram** → wayland attach/damage/commit/flush | `proxy.rs:1961`–`2168` | guest worker |
| 5 | buffer release → `in_flight=false`, slot free | `proxy.rs:8054` | guest worker |
| 6 | acquire gated on a free slot, `x11_reap` per slice | `proxy.rs:2696`, `:2786` | guest main |
| 7 | gamescope composites into a pool buffer, builds the atomic request (~40–60 `drmModeAtomicAddProperty`), libliftoff solve, **`drmModeAtomicCommit`** | gamescope `DRMBackend.cpp:3308`, `:4609` | gamescope |
| 8 | **intercepted ioctl = a synchronous SEQPACKET round trip** under a global lock | `gndrm_shim.c:2225`→`rpc` `:660` | gamescope |
| 9 | daemon serve loop: `poll`+`recvmsg`, parse, validate, apply, **fresh `HashSet` of live inodes**, `dup` of the buffer fd, queue job, reply | `server.rs:380`, `commit.rs:47`–`547` | app |
| 10 | commit worker: pool resolve/retire, `ALooper_pollOnce`, **wait previous release fence** (33 ms cap), 8-op transaction build, apply | `asc.rs:1180`–`3703` | app |
| 11 | SF `on_complete` → vblank stamp, `write_eventfd` → daemon → flip event to gamescope | `asc.rs:385`, `lib.rs:2310` | app/SF |
| 12 | `uPendingFlipCount.wait` — gamescope blocks until the flip event | `DRMBackend.cpp:4715` | gamescope |

The frame is **exactly one deep**: gamescope blocks on the GPU timeline after
its composite and again on the flip event, so every per-frame CPU cost between
the guest's commit and the flip event is serial, not overlapped.

### Log gates (complete list — there is no `GNDRM_LOG_BUFFER`)

| gate | subsystem | where read | usable instrument |
|---|---|---|---|
| `GNDRM_LOG_DAEMON` | `gndrm_daemon::*` + the `asc_log!` macro (HDR decision, layer rect, publish probes) | `log_policy.rs:54`, `asc.rs:135` | `gndrm-daemon: commit #N` (monotonic), `asc hdr decision n=` |
| `GNDRM_LOG_ASC` | `gamenative_drm`, `::asc`, `::ffi` | `log_policy.rs:55` | layer/geometry warnings |
| `GNDRM_LOG_VKSHIM` | the whole ICD shim — **also arms the WSI census itself** | `lib.rs:104` | `proxy commit: … #N`, `proxy hb: acq=/pres=/starved=`, `proxy present: first pNext chain … present_id=` |
| `GNDRM_LOG_SHIM` | the C ioctl interposer | `gndrm_shim.c:226` | `ATOMIC heartbeat #N`, `ADDFB2 ok/fail` |
| `GNDRM_LOG_INPUT` | wayland injector | `log_policy.rs:56` | injector connect/retry |
| `GNDRM_LOG` | fallback for unknown module paths | `log_policy.rs:110` | — |

Gate semantics: on iff the value is present, non-empty and not `"0"` (the C shim
differs — any non-NULL). Container `envVars` reach the **guest only**, so
`GNDRM_LOG_VKSHIM` and `GNDRM_LOG_SHIM` can be set from the container;
`GNDRM_LOG_DAEMON`/`_ASC`/`_INPUT` need an app-side env (APK rebuild).

Frame-rate triad for layer-to-layer accounting, all monotonic on their lines:
`proxy commit: … #N` (guest) → `gndrm-daemon: commit #N` (daemon) →
`asc hdr decision n=` (publish). `ATOMIC heartbeat #N` covers the
gamescope→daemon leg without the vkshim gate.

## 4. Where the CPU actually goes — ranked, each with its falsifier

Ranking is by (plausible size) × (fit to the observed signature: sys-bound,
guest-heavy, −1.6 % fps at unchanged GPU busy).

**A. Guest-side WSI machinery in the shim — the largest term.** Per present the
guest pays an extra `vkQueueSubmit`, a worker-thread wake and `vkWaitForFences`,
an X/server round trip when the DRI3 route is active, a polled per-slot fence
reap (`xcb_sync_query_fence` per in-flight slot), two SEQPACKET datagrams, plus
`libgndrm_shim.so`'s `open/close/ioctl/mmap` hooks over the whole guest. That is
the only per-frame code that exists in one arm and not the other and it is
syscall-shaped, matching the `cpu_sys` delta.
*Falsify:* `simpleperf record` on `Hades2.exe` in the same scene — if
`gn-present`, `xcb_*` and the shim hooks together are under ~2 pp, this is not
the main term.

**B. `VK_KHR_present_wait` turns a display-side wait into guest CPU.** The shim
always advertises present-id/present-wait (`wsi_policy.rs`); Mesa's X11 WSI
implements neither. If DXVK/vkd3d-proton take that capability, the guest waits
per frame on an answer that the shim only produces when *some other thread*
happens to run the reap loop — a wait that can present as busy polling.
*Falsify (cheapest decisive experiment in this report):* set
`dxvk.latencySleep = False` (and optionally `dxvk.maxFrameLatency = 0`) in the
container's `dxvk.conf`. CPU back to ≈X11 at equal fps ⇒ confirmed.

**C. Depth + release-gated slots → acquire starvation.** The guest asks for 3
images and slots only return through the polled reap; at ~2.9 ms/frame with a
4 ms reap slice the acquire loop sits in the reap path on most frames.
*Falsify:* `dxvk.numBackBuffers = 4`, and read `starved=` / the stall lines
(`gn-vkshim stall: acquire found no free slot`, `acquire blocked >=1s …`).

**D. The guest renders into `VK_IMAGE_TILING_LINEAR` so the daemon can scan it
out** (`proxy.rs:12124`), where X11-mode DXVK images are driver-tiled with UBWC.
More bandwidth per frame against an already 84 %-busy GPU, and any spin-based
wait in **B** converts that into CPU.
*Falsify:* GPU-side counters per arm at equal fps; the definitive fix is a
modifier/UBWC import path in the scan-out pool, which the daemon does not have.

**E. Daemon per-frame allocations (app side, +7.2 pp).** Three stand out and all
are trivially removable: a fresh `HashSet<u64>` of every live GEM inode rebuilt
per commit (`commit.rs:540`), 4 `Vec` allocations per atomic parse
(`commit.rs:300`), and a rebuilt `Vec<pollfd>` per scan-out iteration
(`asc.rs:2560`). Also `GetStaticMethodID` on every published frame for the
Kotlin `onFramePublished` upcall (`lib.rs:387`).
*Falsify:* per-thread CPU of `gndrm`/`gndrm-commit` after caching these; or
malloc counters under `simpleperf`.

**F. Redundant per-frame wayland state.** `swapchain_set_present_mode` is issued
on every commit even when the mode did not change (`proxy.rs:2137`).
*Falsify:* drop the call when unchanged; measure guest sys time.

**G. gamescope itself (+6.1 pp) is not the problem it looks like.** It never
composites on the CPU, never reads a frame back, and its composite is a single
compute dispatch sized by output pixels. What it does pay per frame is the
atomic-request build, the libliftoff solve (cached when a layout is known-bad),
and two blocking waits. `-Drt_cap=disabled` means no `nice(-20)`/`SCHED_FIFO`
(`main.cpp:882`), so all of it competes at normal priority.
*Falsify:* gamescope's own per-thread time with the flip path idled by a fake
caps; or build `-Drt_cap=enabled`.

**H. `surfaceflinger` +3.7 pp is transaction traffic**, not composition: one
layer, 8 ops + 1 apply per publish at ~330 Hz plus a completion callback, where
X11 delivers one `queueBuffer` per frame from a persistent buffer.
*Falsify:* SF `TransactionQueued`/`TransactionCompleted` counts per frame per
arm.

Rejected by inspection, so they are not worth another run: per-call shim
dispatch trampolines (sub-µs), `ring_push` in `vkQueueSubmit`, per-frame
daemon→shim datagrams, and gamescope CPU compositing (it does not exist).

## 5. Plan to reduce the gamescope CPU cost

Ordered by expected effect per unit of risk. Every step names the measurement
that decides whether it worked, and none changes visual behaviour.

1. **Instrument before touching anything.** One run per arm with
   `GNDRM_LOG_VKSHIM=1` (+`GNDRM_LOG_SHIM=1`) in the container envVars, 60 s,
   same scene: capture `proxy commit: #N`, `proxy hb: acq=/pres=/starved=`,
   `proxy present: first pNext chain … present_id=`, `ATOMIC heartbeat #N`.
   This alone decides **B** (is present-wait in use?) and **C** (is the guest
   starving?) without a rebuild. Cost: one session, no code.
2. **Kill the per-frame wait in the guest** (fixes **B**). If present-id is in
   use, answer the wait at queue time for every mode the shim owns (it already
   does this for FIFO+cap, `present_wait_answer` `proxy.rs:3116`) rather than at
   release, and make the release path event-driven (present-complete callback)
   instead of a polling `x11_reap`. Expected: most of the guest's extra sys
   time. Verify: `cpu_sys` ratio X11↔gamescope at equal fps, and `starved=`
   returning to 0.
3. **Batch and event-drive the slot reap** (fixes **C**). One
   `xcb_sync_query_fence` per slot per frame becomes one per release event; add
   a 4th depth by default when the guest asks for 3 and the daemon pool has a
   spare buffer. Verify: `slots=[…]`/`starved=`, and acquire→commit time.
4. **Remove the three daemon allocations** (fixes **E**) and cache
   `GetStaticMethodID`. Verify: `gndrm-commit` thread CPU before/after at the
   same publish rate; expect a small but free win.
5. **Suppress unchanged wayland state** (fixes **F**): only call
   `set_present_mode` when it differs from the applied value. Verify: guest sys
   time, and the wayland write count per frame.
6. **Give gamescope real-time priority** (mitigates **G**): build with
   `-Drt_cap=enabled` and pass the capability, or set `SCHED_FIFO` on the
   compositor threads from the app. This does not reduce CPU; it removes the
   18–21 ms idle frames that a late-scheduled commit thread produces (recorded
   in the A52 header). Verify: frame-time p99, not CPU.
7. **Attack the bandwidth term (D) last, and only if 2–4 leave a gap.** A
   UBWC/modifier scan-out import is a pool-format change with a wide blast
   radius (the daemon cannot scan out UBWC today, and the layout must stay
   agreed between shim, interposer and daemon via `GNDRM_SCANOUT_LAYOUT`).
   Verify: GPU busy at equal fps, then CPU with fps pinned by the cap.

What this plan deliberately does not do: switch to `GN_VKSHIM_GUEST_SURFACE=1`
(the DRI3 route adds round trips and is the *worse* CPU path), enable
`-Dpipewire` (adds a full-frame RGB→NV12 dispatch per present), or force
composition (`--force-composition` / `-c` / `ENABLE_VKBASALT=1` all flip every
frame to the composite path).

## 6. Testability note (not a display bug, but it blocks measurement)

Under the current payload the guest receives **no** input in gamescope mode, so
a scene behind a menu cannot be reached by the usual routes. Verified on device:
`zwlr` virtual pointer/keyboard connect (`wlserver: virtual pointer connected` /
`virtual keyboard connected`) but drive nothing; `sendevent` on the pad node
writes the app's gamepad shm correctly (`btn[0]=1`, `connected=1`) and the guest
never reacts; `files/padinject` and a uinput pad are equally unseen (evshim hooks
SDL, and this title loads its own PE `SDL2.dll`); `input keyevent`/`input tap`
reach the gamescope seat and stop there — because `XServer.inject*` returns into
`GamescopeInputSink` (`XServerScreen.kt:2163`) while `XTEST` is compiled out
(`-Dinput_emulation=disabled`, `wlserver: Gamescope built without libei, XTEST
will not be available!`) and Xwayland has no `-Dxwayland_ei=socket`.

What does work, and is the supported way to drive a gamescope session from the
host for now: a small X client inside the guest that warps the pointer and
`XSendEvent`s `ButtonPress`/`ButtonRelease` to the game window (core protocol,
no XTEST, no libei). The X11 arm is unaffected — pointer/keyboard injection
there goes through the in-process server.

The real fix is the documented one: build gamescope with
`-Dinput_emulation=enabled` (libei/eis server) and Xwayland with
`-Dxwayland_ei=socket`, which restores XTEST for every in-guest client
(including `winhandler.exe`'s `keybd_event`/`mouse_event`) and makes the sink's
seat events land where they are supposed to.
