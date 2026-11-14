# 01 — gamescope vs X11: where the extra CPU goes, and how to take it back

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
| 3 | present discipline (`fifo`/`mailbox`/`immediate`), present-id answer, and `mode_census()` under a **process-global mutex** every present | `proxy.rs:3114`, `:3095`, `:10547` | guest main |
| 4 | present worker: fence wait → commit bookkeeping → **present mark datagram** → wayland attach/damage/commit/flush | `proxy.rs:1961`–`2168` | guest worker |
| 5 | buffer release → `in_flight=false`, slot free | `proxy.rs:8054` | guest worker |
| 6 | acquire gated on a free slot; `x11_reap` is a **no-op on this route** (`present.is_null()` → return, `proxy.rs:675`), so only the `cv_free` slice and its timeout remain | `proxy.rs:2696`, `:2657` | guest main |
| 7 | gamescope composites into a pool buffer, builds the atomic request (~40–60 `drmModeAtomicAddProperty`), libliftoff solve, **`drmModeAtomicCommit`** | gamescope `DRMBackend.cpp:3308`, `:4609` | gamescope |
| 8 | **intercepted ioctl = a synchronous SEQPACKET round trip** under a global lock | `gndrm_shim.c:2225`→`rpc` `:660` | gamescope |
| 9 | daemon serve loop: `poll`+`recvmsg`, parse, validate, apply, **fresh `HashSet` of live inodes**, `dup` of the buffer fd, queue job, reply | `server.rs:380`, `commit.rs:47`–`547` | app |
| 10 | commit worker: pool resolve/retire, `ALooper_pollOnce`, **wait previous release fence** (33 ms cap), 8-op transaction build, apply | `asc.rs:1180`–`3703` | app |
| 11 | SF `on_complete` → vblank stamp, `write_eventfd` → daemon → flip event to gamescope | `asc.rs:385`, `lib.rs:2310` | app/SF |
| 12 | `uPendingFlipCount.wait` — gamescope blocks until the flip event | `DRMBackend.cpp:4715` | gamescope |

The frame is **exactly one deep**: gamescope blocks on the GPU timeline after
its composite and again on the flip event, so every per-frame CPU cost between
the guest's commit and the flip event is serial, not overlapped.

### Log gates, and which ones a container can actually turn on

| gate | subsystem | read at | container-settable? |
|---|---|---|---|
| `GNDRM_LOG_VKSHIM` | the whole ICD shim — **also arms the WSI census itself** | `lib.rs:104` | **yes** (guest process) |
| `GNDRM_LOG_SHIM` | the C ioctl interposer | `gndrm_shim.c:226` | **yes** (gamescope process) |
| `GNDRM_LOG_DAEMON` | `gndrm_daemon::*` + `asc_log!` | `log_policy.rs:54`, `asc.rs:135` | **no** — app process |
| `GNDRM_LOG_ASC` | `gamenative_drm`, `::asc`, `::ffi` | `log_policy.rs:55` | **no** — app process |
| `GNDRM_LOG_INPUT` | wayland injector | `log_policy.rs:56` | **no** — app process |
| `GNDRM_LOG` | fallback for unknown module paths | `log_policy.rs:110` | **no** — app process |

Gate semantics: on iff the value is present, non-empty and not `"0"` (the C shim
differs — any non-NULL). Container `envVars` reach the **guest process tree
only**, and the daemon runs inside `app.gamenative`, so the two gates that would
measure the daemon and the publish need an app-side env, i.e. an APK rebuild.

That matters because **the daemon-side counters are the ones that look free and
are not.** `gndrm-daemon: commit #{}` is an `always_log` line, but it sits inside
`if want_commit_log && (…)` where `want_commit_log = verbose_logging()`
(`commit.rs:179`), and `verbose_logging()` reads `GNDRM_LOG_DAEMON` from the
environment (`lib.rs:262`). Same for `asc hdr decision n=` (`asc.rs:1538`). So a
container-only run cannot read the daemon→publish leg; it reads the guest leg and
the gamescope→daemon leg and infers the rest. Step 1 of the plan is written
around that (`02-instrumentation.md`).

Instruments that a container-only run **does** get, all monotonic on their line:

- `proxy commit: … #N` — guest commits (`proxy.rs:2175`, `N_COMMIT_OK`).
- `proxy hb: fmt=… create= destroy= images= acq= present=` — the WSI census. It
  carries `acq=` and `present=` but **not** `starved=` or `slots=`
  (`proxy.rs:307`); doc 24 originally claimed otherwise.
- `proxy present: first pNext chain … present_id=` — whether the guest opted into
  present-id.
- `present mode: declared=… commits= declared_fifo= declared_other= slot_waits=
  commit_lat_us=` — the ready-made per-mode census, change-triggered and ungated,
  gated to 10 s otherwise (`proxy.rs:10547`).
- `ATOMIC heartbeat #N` — the gamescope→daemon atomic rate, no vkshim gate
  (`gndrm_shim.c`).

## 4. Where the CPU actually goes — re-ranked after seven deep passes

The ranking below supersedes the first pass. Four of its eight candidates did not
survive being read against the source; each withdrawal is recorded with the
evidence, because the wrong model is what would otherwise get built.

**Read the per-process table with its error bar.** `top -n 1 -b -m 14` truncates
to the top 14 and quantises to 3.7 pp per row per sample, with ~20 samples per
arm, so a per-process mean is trustworthy only to roughly ±5 pp. A +20.8 pp move
is real; a +5.1 pp move is at the edge of resolution. The reconciliation of the
per-process sum against `/proc/stat` does not close (+95 ms/s from the listed
rows vs +24 ms/s measured overall), and that is a sampling artefact, not a
finding — do not quote the per-process table as a partition of the total.

**A — guest-side WSI machinery: still the only term worth attacking, but
narrower than first stated.** Per present on the default route the guest pays the
render-completion submit (`proxy.rs:3415`, plus an extra empty submit at `:3418`
when a present fence is chained), a present-worker wake and fence wait, two
SEQPACKET datagrams, the wayland attach/commit/flush, and `mode_census()` under a
process-global mutex (`proxy.rs:3114`). **Withdrawn**: the "polled per-slot fence
reap, one `xcb_sync_query_fence` per in-flight slot per frame" is *not* on this
route — `x11_reap` returns immediately when `present.is_null()` (`proxy.rs:675`),
and that field is only set on the `GN_VKSHIM_GUEST_SURFACE=1` DRI3 route. On the
default route the acquire side costs a `cv_free` wait with a 4 ms slice
(`proxy.rs:2657`), not an X round trip.

**B — per-frame `VK_KHR_present_wait`: largely refuted as written for this
container.** The shim does advertise `present_id`/`present_wait`
(`wsi_policy.rs:65`), and the waitable is a process-global `Mutex<Waits>` +
`Condvar` (`present.rs:72`, `:181`), so any spin would be guest-side. But
`present_wait_answer` already answers **at commit** for mailbox and immediate
(`proxy.rs:2057`), and at queue time for a rate-vsync declaration
(`proxy.rs:3097`); only the FIFO path defers to release. The container ships
`MESA_VK_WSI_PRESENT_MODE=mailbox`, so the mechanism this cause needs — "the wait
is only satisfied by another thread's poll" — cannot fire unless the guest
declares FIFO. Keep it as a conditional, not a cause.

**C — acquire starvation: arithmetically bounded at ~46 µs/frame.** 4 ms slice
against a 2.9 ms frame, and 340 vs 344.6 fps, is one lost wakeup per ~87 frames.
Also **withdrawn**: `starved=` is not on the `proxy hb:` line at all
(`proxy.rs:307` carries `acq=`/`present=` only); the readable census is
`present mode: … slot_waits= commit_lat_us=` (`proxy.rs:10547`). `N_STARVE`
increments when no free slot is found (`proxy.rs:2823`), which an unbounded wait
may never reach — the run settles it.

**D — linear tiling: real but small, and not the CPU lever.** ~1.25 GB/s per pass,
~5 GB/s over four passes ≈ 7 % of the 68.3 GB/s peak; recovering ~0.5 GB/s is
0.7 %. At 1280 px the gralloc stride is already tight (5120 = 1280×4), so there
is no padding to reclaim, only compression. And bandwidth can only turn into CPU
through B's spin, so D's CPU payoff collapses once B is settled
(`08-scanout-pixel-format.md`).

**E — daemon per-frame allocations: confirmed, and smaller than claimed.** Five
`Vec`s per atomic parse (not four), two per poll turn (not one), a per-commit
`HashSet` of live inodes that needs refcounts rather than a dirty flag, and a JNI
lookup that also builds two CESU-8 `CString`s per published frame. Total
0.9–2.3 ms of CPU per second against the daemon's ~72 ms/s — 1.5–3 % of the
daemon's own cost, i.e. free to remove but not decisive
(`05-daemon-present-allocations.md`).

**F — redundant wayland state: withdrawn, and the opposite is true.**
`swapchain_set_present_mode` is *load-bearing on every commit*: gamescope clears
`oCurrentPresentMode` in `PrepareCommit` (`wlserver.cpp:180`) after consuming it
into the frame's pacing decision, and the shim turns its own acquire gate off for
a FIFO declaration (`proxy.rs:10454`). Suppressing the re-send shim-side would
un-pace every FIFO commit after the first. See `06-present-mode-declaration.md`.

**G — the compositor: not a defect.** `gamescope` at 5.7 pp of one core replaces
the X server that used to live inside `app.gamenative` (19.3 → 18.0 in the same
pair). It never composites on the CPU and never reads a frame back. Its per-frame
cost is the atomic build, the libliftoff solve and two blocking waits
(`DRMBackend.cpp:4609`, `:4718`), and with `-Drt_cap=disabled` it competes at
normal priority.

**H — SurfaceFlinger: withdrawn.** The matched pair shows 10.9 vs 10.9, not
+3.7. The first pass's +3.7 came from comparing across builds.

Bottom line: the residual is **+0.3 pts of 8 cores**, of which the guest term is
~5 pp of one core, and the only candidate that can plausibly move it is A. There
is no power to win (−0.04 W).

## 5. The plan, item by item — verdicts after the design pass

Each item now has its own document with the file:line evidence, the change, the
invariants, the risks and the measurement that decides it. **The plan is
re-ordered and three items are closed**, because the premise they rested on did
not survive the source read.

| # | item | verdict | doc |
|---|---|---|---|
| 1 | one instrumented run, no rebuild | **done — see §5.1** | `02-instrumentation.md` |
| 2 | answer `present_wait` at queue time | **closed by measurement** — nothing chains `VkPresentIdKHR`, zero `vkWaitForPresentKHR` | `03-present-wait.md` |
| 3 | batch/event-drive the slot reap, add depth | **closed by measurement** — 0 slot waits, 0.013 ms dwell, and the depth is already 4 | `04-slot-reap.md` |
| 4 | remove per-frame allocations + JNI lookup | **the only item left with a CPU target** — free, 1.5–3 % of the daemon | `05-daemon-present-allocations.md` |
| 5 | stop re-sending unchanged wayland state | **closed twice** — load-bearing per commit, and `declared_fifo=0` so the path is unreachable | `06-present-mode-declaration.md` |
| 6 | give gamescope real-time priority | **reframed** — `rt_cap` cannot work on Android; `nice -20` can; this is latency, not CPU | `07-compositor-scheduling.md` |
| 7 | UBWC/tiled scan-out import | **closed** — an `AHardwareBuffer` cannot express UBWC, and the attempt already exists unmeasured on a backup branch | `08-scanout-pixel-format.md` |

### 5.1 Step 1 has now been run, and it closed two more items

Both arms, 35 s settle + 95 s window, container restored byte-identical each
time (`tools/gamescope-cpu-arm.sh`, full numbers in
`02-instrumentation.md` §10):

| question the run answers | reading | consequence |
|---|---|---|
| does the guest chain `VkPresentIdKHR`? | **no** (only sType 1000275001) | cause B dead, item 2 closed |
| is `vkWaitForPresentKHR` called? | **0 lines** | same |
| do acquires ever wait for a slot? | **0.0**, dwell **0.013 ms**, `starved=0` | cause C dead, item 3 closed |
| what depth does the guest actually get? | **4** (mean in flight 1.66, free 2.08) | item 3's "add a 4th slot" was already true |
| is a FIFO declaration ever made? | **no** — `declared=mailbox requested=immediate source=env`, `declared_fifo=0` over 25 009 presents | item 5 unreachable as well as semantic (the `source=env` form of the census no longer exists; the shim declares the guest's mode now) |
| does the X11 arm exercise the shim? | **no** — `gn-vkshim: ICD loaded` = 0 | the +5.1 pp guest term *is* the shim's existence, not its behaviour |

At the time of this run the container's `MESA_VK_WSI_PRESENT_MODE=mailbox`
overrode the title's own `immediate` request, which is why no FIFO path was ever
taken, and it is also why three of the plan items could not fire on this title.
The shim reads no such override any more (`refactor/vsync`): the declared mode is
the guest's request, so a re-run of this arm would declare `immediate`.

~~One asymmetry is left unexplained: 241.9 atomic commits/s against 357.3
presents/s.~~ **Withdrawn — the two numbers count different events and were never
meant to agree.** `ATOMIC heartbeat` is `g_n_atomic`, every
`DRM_IOCTL_MODE_ATOMIC` the interposer sees (`gndrm_shim.c:2269`), and
`notify_frame_published()` is the *producer* ring, documented at
`modules/gndrm-android/src/lib.rs:303` as "one per frame the producer reports —
not from the publish path". The pipeline carries four distinct rates (guest
present 341/s, gamescope commit 242/s, panel 120/s, daemon vsync grid 121/s) and
they are not supposed to match. See `02-instrumentation.md` §12.

### 5.2 The app now stops overriding the guest's present mode

`XServerScreen.kt` strips `MESA_VK_WSI_PRESENT_MODE` and `WRAPPER_MAX_IMAGE_COUNT`
on the gamescope route, so the shim declares what the guest asked for. Re-measured
(`02-instrumentation.md` §11):

- **It works:** `declared/requested/source` went from `mailbox/immediate/env` to
  `immediate/immediate/guest`.
- **It costs one side effect, from the mode and not the count:** the depth is now
  **3** with `slot_waits` at **9.8 %** of acquires (dwell 28 µs, no `starved`,
  nothing over 8.33 ms). The 4th slot was the mailbox floor
  (`SC_MAILBOX_IMAGES = 4`, `proxy.rs:7364`), so dropping mailbox dropped it.
  Removing `WRAPPER_MAX_IMAGE_COUNT` did **nothing** here: the container set it
  to `0`, and `max_swapchain_images()` treats anything below 2 as absent
  (`proxy.rs:7387`) — it only lowers the ceiling for values ≥ 2.
- **It does not move the atomic rate:** still 242/s against 357 presents/s, so the
  1.48 ratio is not a mailbox artefact.
- **On the one truly controlled scene (the title screen) it is a small win:**
  CPU busy 27.3 % → 25.7 %, sys 14.9 % → 13.8 %, power 8.06 → 7.84 W, GPU identical.
- **The in-game pair cannot attribute it:** the arm ran in a heavier area than
  the §2 pair and has no old-APK counterpart. One in-game A/B on the same area is
  still owed, and it needs no rebuild: `dxvk.numBackBuffers = 4` makes the guest
  ask for 4, restoring the depth under immediate so mode and depth can be
  separated. (The spawn point is stable enough to plan on — `tools/scene-fingerprint.py`
  puts every in-game capture after the first within 2.2 of each other and only
  that first load ~34 away, so the "different room on every load" impression was
  wrong. Compare fingerprints before laying two arms against each other.)

### 5.3 The lever that is bigger than the whole plan

The container runs **uncapped** — `fpsLimiterEnabled: false`, `fpsLimiterTarget`
already **120**, MangoHud reading 341–358 fps — on a **120 Hz** panel at
1280x720. The guest therefore renders ~2.8× the frames the display can show, and
every per-frame cost this plan is about (shim submit, worker wake, datagram,
wayland commit, SF transaction) is paid at 341/s instead of 120/s.

Capping to the panel rate cuts the per-frame path's total cost by ~2.8× before
any code changes — orders of magnitude more than any item below, which is why the
plan is now ranked against *that* baseline rather than against an uncapped one.
It also accounts for the scene dependence: in the heavier room `slot_waits` goes
9.8 % → 48.9 % at depth 3, so the acquire path's sys time rises with the overshoot
rate (`02-instrumentation.md` §12).

### 5.4 Capped to 120 Hz: the two paths converge on CPU and invert on power

`fpsLimiterEnabled` was set true in the container (`fpsLimiterTarget` was already
120, `dxvk.numBackBuffers` untouched, `displayServer` varied only), and the same
room and save were re-measured as an adjacent pair, 240 s each:

| metric | X11 capped | gamescope capped | delta |
|---|---|---|---|
| fps (MangoHud) | 120.0 | 119.8 | — |
| CPU busy (8 cores) | 25.6 % | **22.9 %** | **−2.7 pts** |
| CPU user | 14.5 % | 13.1 % | −1.4 |
| CPU sys | 9.3 % | 7.7 % | −1.6 |
| GPU busy / clock | 55.6 % / 607 MHz | 55.0 % / 607 MHz | −0.6 |
| package power | 6.50 W | **7.49 W** | **+0.99 W (+15 %)** |
| cpuss / gpu | 61.1 / 46.8 °C | 62.9 / 48.1 °C | +1.8 / +1.3 |
| RAM used | 6.32 GB | 6.46 GB | +0.14 |

Per process, over the same windows:

| process | X11 | gamescope | Δ |
|---|---|---|---|
| `Hades2.exe` | 106.2 | 96.0 | **−10.2** |
| `app.gamenative` | 35.9 | 14.7 | **−21.2** |
| `surfaceflinger` | 12.9 | 18.1 | +5.2 |
| `gamescope` | 0 | 4.2 | +4.2 |

Three things fall out of this, and the first two contradict the uncapped picture.

**1. The cap is the lever, and it is worth far more than everything else.**
Uncapped → capped, same room, same path: CPU busy 46.1 % → 25.6 % (X11) and
50.0 % → 22.9 % (gamescope); package power 19.6 W → 6.5–7.5 W; GPU clock
774 → 607 MHz; cpuss 101.7 → 61.1 °C. That is ~2.7× the power for the same 120
frames the panel can show, bought by rendering 341.

**2. Capping flips which path is CPU-cheaper.** Uncapped, gamescope cost more
(+5.1 pp in the guest, +3.9 pts overall). Capped, gamescope costs **less** CPU
(−2.7 pts), and the guest is 10 pp *cheaper* under gamescope. The mechanism is
visible in the per-process rows: at 120 fps the in-process X server inside
`app.gamenative` costs 35.9 pp of a core — more than the gndrm daemon (14.7) plus
gamescope (4.2) plus SF's increase (5.2) together. Under X11 the cap is enforced
inside the app, and that limiter is not free. So the earlier "gamescope costs more
CPU" finding is a property of the **uncapped** configuration, not of the display
path.

**3. Power disagrees with CPU, in the opposite direction.** gamescope is 2.7 pts
cheaper on CPU and 1 W dearer on power, 1.8 °C hotter, with identical GPU busy and
clock. The same inversion appears at the title screen (3.5 W X11 vs 4.0 W
gamescope). So CPU is not a usable proxy for the display path's energy cost: the
extra watt is being spent outside the CPU — most plausibly the per-frame composite
dispatch and the higher SurfaceFlinger activity (12.9 → 18.1) keeping the display
pipeline and more cores resident. **Any further work on this should be judged on
package power, not cpu%.** §5.5 then shows that on battery the watt difference
itself vanishes, so the surviving claim is CPU: gamescope is cheaper at the cap.

The capped configuration was then censused (`02-instrumentation.md` §13), which
answers the one open item: **the 3-deep pool costs nothing at the cap.** At 120 fps
the guest switches to FIFO, `slot_waits` reads 100 % but the dwell is 6.25 ms —
one frame period — with `starved = 0` and a free slot available, so the acquire is
being parked by the limiter, not contended. `dxvk.numBackBuffers = 4` has nothing
to win. The same census shows the atomic rate is exactly **2.0 per present** at
the cap and 0.71 uncapped, so "242/s" was two different constraints coinciding,
not a fixed cadence.

### 5.5 On battery: the 1 W gap does not reproduce

Everything above measured power as `usb_in − battery_net`, an inference that
folds in charger taper and phantom readings on the USB node — that node reports
**3000 µA at 86000 µV (0.26 W) while offline**, which on battery inflates every
reading unless it is zeroed. On battery `battery current × voltage` *is* system
power, so the arms were repeated there, adb over wireless, same title screen
(the only scene that is byte-identical every load), 120 s each, adjacent.

| arm | fps | CPU busy | CPU sys | GPU | **package power** | cpuss |
|---|---|---|---|---|---|---|
| X11 uncapped | 358 | 25.5 % | 14.1 % | 37.9 % | **7.38 W** ±0.16 | 64.2 °C |
| X11 capped | 120 | 17.8 % | 7.8 % | 26.1 % | **4.38 W** ±0.11 | 52.0 °C |
| gamescope capped | 121 | **16.1 %** | 6.5 % | 26.6 % | **4.53 W** ±0.18 | 51.5 °C |
| gamescope, cap off | 121 | 25.1 % | 12.5 % | 26.6 % | 4.32 W ±0.14 | 52.4 °C |

**The 1 W was an artefact of the charger measurement.** At the cap, gamescope is
**+0.15 W ± 0.21 (0.7 σ)** — no measurable difference — while still being 1.7
points cheaper on CPU. The +0.99 W seen on the charger is not reproducible on
battery, and the phantom 0.26 W on the offline USB node is the right order of
magnitude to have produced it.

**The cap is worth 3.0 W on the same path** (X11 7.38 → 4.38 W), which remains the
dominant lever by a wide margin.

### 5.6 The guest, not the container, decides the title screen's rate

The fourth arm was meant to be gamescope uncapped, and it would not run: with
`fpsLimiterEnabled` verified **false** in the container, the guest still asked for
`fifo` (`present mode: declared=fifo requested=fifo source=guest`) and DXVK's own
vsync locked the title to 121 fps. Two consequences:

- **A title-screen uncapped arm is impossible under gamescope.** At the title the
  guest picks FIFO itself, so the container's cap setting cannot express
  "uncapped" there. Uncapped numbers have to come from an in-game scene, which
  needs the scene fingerprint to keep the comparison honest.
- **At an identical 121 fps, letting the guest do the vsync costs 9 more points of
  CPU than our limiter does** (25.1 % vs 16.1 %) — and costs nothing in watts
  (4.32 vs 4.53 W, inside noise). So in this regime the extra CPU is free, and
  the app's limiter is the cheaper way to hold the rate, not the dearer one.

### Where that leaves the work

Step 1 answered all three questions in one session (§5.1) and **closed items 2,
3 and 5**. Nothing the plan set out to remove is actually running on this title.

What is left, in order:

1. **Item 4** — the daemon's per-frame allocations and the per-frame JNI lookup.
   The only remaining item with a defensible CPU target, worth 1.5–3 % of the
   daemon's own cost, and free to land.
2. **Items 6 and 7 are not CPU work** — one is frame-time p99 under a
   late-scheduled compositor thread, the other is closed.
3. **Cause A's remainder in the shim** (the extra submit, the worker wake, two
   datagrams, the `mode_census()` mutex) has no design doc yet, because step 1
   refuted the two witnesses that would have justified it. If someone wants to
   chase the last +5.1 pp of one core in the guest, `simpleperf record` on
   `Hades2.exe` in the same scene is the next instrument, not another plan item.

The honest conclusion of this report stands and is now measured rather than
argued: **payload `20261110.1` has already collected the win.** +0.3 pts of eight
cores and −0.04 W at identical fps, with the daemon serving 242 of the guest's
357 presents per second and no stall anywhere in the chain. The useful output of
this investigation is the four refutations and the tooling, not a set of changes.

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
