# 24 — gamescope vs X11: where the extra CPU goes, and how to take it back

Status: **measured on the shipped payload, causes ranked, no fix landed.** This
is the reference for any work that tries to make the gamescope display path cost
the same CPU as the in-app X11 path on an idle, GPU-bound scene — and the record
of what that difference actually is today, which is much less than the 2026-11-09
pass first concluded.

Scope: Hades II (Steam 1145350), first save slot (Crossroads, night 31), Odin3
(SM8550-class, 8 cores), container 1280x720, panel 120 Hz on charge, gamescope
payload `20261110.1`, both arms run back to back and sampled for 240 s each.

## 1. What was measured, and how

Two arms, same container except `displayServer`, same save, same idle spot,
same APK, same payload, run as an **adjacent pair** — X11 first, gamescope
immediately after, 240 s each, on the same charge. Both arms saturate `cpuss` at
102 °C within three minutes, so the arms are matched by temperature as well as by
scene; that is the check that the pair is comparable, and it is why the earlier
non-adjacent runs (+3.8, then +1.5) are superseded rather than averaged.

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

Payload `20261110.1` removed most of the gap this report set out to explain.
Measured as an **adjacent pair** — the two arms run back to back in the same
thermal state, both 240 s, on the same APK and the same payload:

| metric | X11 | gamescope `20261110.1` | delta |
|---|---|---|---|
| fps (5 MangoHud reads) | 338.8 | 342.5 | +1.1 % |
| CPU busy (8 cores) | 45.9 % | 46.2 % | **+0.3 pts** |
| CPU user | 20.9 % | 20.0 % | −0.9 pts |
| CPU sys | 20.8 % | 21.8 % | **+1.0 pts** |
| GPU busy / clock | 83.7 % / 760 MHz | 84.4 % / 747 MHz | +0.7 pts |
| package power | 19.44 W | 19.40 W | **−0.04 W** |
| cpuss / gpu / soc | 101.8 / 76.2 / 78.0 °C | 101.9 / 76.6 / 78.3 °C | — |
| RAM used | 6.56 GB | 6.42 GB | −0.14 GB |

The same comparison on payload `20261105.4` measured **+3.8 points** of 8-core
CPU busy, and the APK-only pair (new APK, old payload for the gamescope arm)
measured +1.5. The trend is the payload, not the app.

Read the shape, not just the total: **the total is flat and the split moved.**
Sys is +1.0 and user is −0.9 at identical fps — the work is the same, it is
spent differently. That is exactly the signature of a display path that trades
in-process X calls for cross-process IPC hops, and it says the remaining cost is
syscall-shaped, not compute.

### Per-process attribution (the adjacent pair)

| process | X11 | gamescope | Δ (pp of one core) |
|---|---|---|---|
| `Hades2.exe` (guest, incl. the ICD shim) | 289.8 | 294.9 | **+5.1** |
| `app.gamenative` | 19.3 | 18.0 | −1.3 |
| `gamescope` | 0 | 5.7 | **+5.7** |
| `surfaceflinger` | 10.9 | 10.9 | 0.0 |
| `wineserver` | 10.7 | 10.5 | −0.2 |
| `vendor.qti.hardware.display.composer` | 5.6 | 6.0 | +0.4 |

Two rows carry the whole story.

**The compositor costs about what the X server cost.** In X11 mode the X server
runs *inside* `app.gamenative` (`XServerComponent` starts `XServer` through
`XConnectorEpoll` in-process, and the frame leaves via
`VulkanRenderer`/`ASurfaceRenderer` in the same process), so that 19.3 includes
an X server. Under gamescope, `app.gamenative` is only the gndrm daemon (18.0)
and gamescope is a separate 5.7 — plus Xwayland, which `top` files in the
truncated tail of this window. "The app got cheaper and gamescope appeared" is a
component moving, not a cost appearing.

**The guest pays +5.1 pp, and that is the only real delta.** It was +20.8 pp on
`20261105.4`. The guest is expensive here because under gamescope its Vulkan WSI
*is* our code: `VK_ICD_FILENAMES` points at the payload's adrenotools ICD
(`BionicProgramLauncherComponent.applyGamescopeEnv`) and the shim
(`icd_shim/src/proxy.rs`) does the acquire/present/release work in
`Hades2.exe`'s address space, where Mesa's X11 WSI does it in X11 mode. +5.1 pp
is ~1.8 % of the guest's own CPU for the whole present path — small, and the
right ceiling for the plan in §5.

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

Ranking is by (plausible size) × (fit to the observed signature: sys +1.0 and
user −0.9 at flat total CPU and identical fps, with the only growth inside
`Hades2.exe` at +5.1 pp of one core). Only what can plausibly move *that* term is
worth acting on; the ceiling for everything below is a few tenths of a point of
8-core CPU.

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

**Scope, set by the matched pair in §2:** the target is the guest's +5.1 pp, not
the total. At +0.3 pts of 8-core busy and −0.04 W there is no power win left to
collect, so items 2–5 are justified only as *less work in the guest*; item 6 is a
latency item with no CPU claim; item 7 is closed unless the GPU is shown to be
bandwidth-starved at the tested scene. Do item 1 first regardless — it is one
session, no code, and it decides whether 2 and 3 have a target at all.

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

## 6. Driving a gamescope session from the host (measurement prerequisite)

Needed for any A/B on an in-game scene, because a title screen is input-gated and
a loading screen is not gameplay.

**Touch works, in both display modes, and is the supported route.** The path is
`adb shell input swipe/tap` → the app's `TouchpadView` → `XServer.injectPointerMove` /
`injectPointerButton` → `GamescopeInputSink` → `GameNativeDrm.nativePointerMotion`
→ the injector's `zwlr_virtual_pointer` → gamescope's seat → Xwayland → wine. The
virtual pointer **is** plumbed all the way through; only the keyboard is not (see
below).

Recipe, verified on device at a Hades II title screen in *both* modes:

```sh
adb exec-out screencap -p > a.png          # the game draws its cursor: find it
adb shell 'input swipe <x> <y> <x2> <y2> 300'   # drag = relative motion, ~1.5x amplified
adb exec-out screencap -p > b.png          # confirm the cursor landed on the item
adb shell 'input tap <anywhere>'
```

- **A drag moves the pointer, a tap clicks wherever the pointer already is** — the
  tap's own coordinates are ignored, so `input tap <the thing you want>` looks like
  a broken coordinate mapping. Move, look, then tap.
- Motion is amplified (the touchpad path applies `CURSOR_ACCELERATION = 1.5f`), so
  a ~640 px drag moves the cursor ~960 px. Measure one drag off a screencap before
  chaining more.
- Hover is visible: Hades II turns the menu item green, which is a free confirmation
  that the pointer event reached the guest *before* committing to a click.
- **Failure mode that produced a false negative**: a drag that starts where the
  pointer already is, or a chain of swipes issued without a screencap between them,
  leaves the cursor parked at the screen edge and every later tap clicks nothing.
  The 2026-11-09 pass concluded from exactly that "gamescope gets no input" and it
  was wrong — the virtual pointer worked the whole time.

**Keys do not reach the guest under gamescope**, and this is structural, not a
driving error. `XServer.injectKeyPress` returns into `GamescopeInputSink`
(`XServerScreen.kt:2163` sets the sink for `SessionDisplay.GAMESCOPE` and clears it
for `X11`), the injector creates a `zwp_virtual_keyboard` alongside the pointer, and
gamescope answers with:

```
[gamescope] [Error] wlserver: Gamescope built without libei, XTEST will not be available!
```

`android/build-gamescope.sh` passes `-Dinput_emulation=disabled` and Xwayland is
built without `-Dxwayland_ei=socket`, so the EI hop that carries seat key events
into Xwayland does not exist. Consequence: a guest toplevel that is *keyboard*-gated
cannot be advanced from the host (verified: `input keyevent 111` at the in-game
screen produces no pause menu in gamescope mode). The X11 arm is unaffected — there
the sink is null and keys go through the in-process server.

Testable consequence: `sendevent` on the real pad node, `files/padinject`, a
uinput pad, and `input gamepad keyevent` are all irrelevant here — the guest reads
its pad through `evshim`, which interposes SDL, and this title loads its own PE
`SDL2.dll`, so nothing app-side reaches it. Do not spend time on them.

If a keyboard is genuinely needed, the fix is the documented one: build gamescope
with `-Dinput_emulation=enabled` (libei/eis server) and Xwayland with
`-Dxwayland_ei=socket`. That restores XTEST for every in-guest client, including
`winhandler.exe`'s `keybd_event`/`mouse_event`.
