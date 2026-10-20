# 17 — LSFG frame generation under gamescope

Status: **plan.** No code yet.

## The problem

Frame generation exists only for the X11 display server. The `legacy` backend
installs `liblsfg-vk-layer.so` as an implicit Vulkan layer inside the container
and lets it hook the game's `vkCreateSwapchainKHR`/`vkQueuePresentKHR`; the
`native` backend runs the same algorithm inside the app's `VulkanRenderer`.

Neither reaches a gamescope session. The app's renderer is not in that present
path, and the layer must not be loadable at all: its failed device init on this
driver takes gamescope's own `vkCreateDevice` with it. `LsfgVkManager
.disableLayer()` deletes the manifest for a gamescope session for that reason.

The layer is not the thing to fix. The engine is: it needs a `VkDevice`, the
user's `Lossless.dll`, and somewhere to write a finished frame — and the ICD
shim has all three.

## Why this engine, not the layer

Both paths run LSFG 3.1 and take their shaders from the same `Lossless.dll`; the
difference is the plumbing, and the plumbing decides it.

| | `legacy` — the lsfg-vk layer | `native` — this plan |
|---|---|---|
| Device | a **second** `VkDevice` created inside the layer, beside the game's | the guest's device |
| Sync per source frame | `waitIdle()` on that second device (`src/context.cpp:252-266`) | none; the frame's own submit |
| Its "device-wide wait" | `ovkQueueSubmit(queue, 0, nullptr, VK_NULL_HANDLE)` — zero command buffers and no wait semaphores, so it orders nothing (`src/context.cpp:243-245`; `ovkQueueSubmit` is a passthrough, `src/layer.cpp:513`) | n/a |
| Copies | swapchain → `frame_0/1`, then `out_n` → swapchain ×N | generate → target, blit → swapchain ×N |
| Generations | fixed `multiplier` | adaptive to a target rate, with warm-up and panel-bound detection |
| HDR | `hdr_mode` written false; the image format is chosen as FP16 or RGBA8 rather than from the swapchain (`src/context.cpp:70-73`) | gated on the composite's format |
| API coverage | Vulkan clients only — it is a layer | any API the app composites |
| Failure mode | its device init fails and takes the session with it; that is why the gamescope path bans it | in-process, no loader |
| Upkeep | 2,637 lines plus 94 MB of thirdparty, with per-card version pins | 3,270 lines of our own code, clippy-gated |

The layer's cost is structural, not a tuning problem: a second device cannot be
ordered against the game's queue without stalling, so every source frame pays
one. The engine here runs the whole chain inside the frame's own command buffer,
on the frame's own device.

What the layer has that is worth keeping is its **position** — on the guest's
swapchain, inside the guest process — and that is the position the ICD shim
already occupies.

## Where it runs

A third backend, `gamenative_drm`: the engine runs as its **own shared library**,
`liblsfg_engine.so`, driven by the ICD shim on the guest's own device and queue,
and each generated frame is committed through the shim's existing slot/commit
path.

Two artifacts, and the dependency runs both ways — deliberately, and only through
the two edges below:

```
libadrenotools_gamescope.so (ICD, 3 exports)          liblsfg_engine.so
  dlopen/dlsym ──────────────────────────────────────▶ vkr_lsfg_* (frame gen)
  host table  ◀────────────────────────────────────── lsfg_load_modules/release
                  (installed once, before the first create)
```

Splitting them is not cosmetic. The ICD's dynamic table is pinned to its three
Khronos loader entry points, and the engine needs none of them: as objects inside
the ICD it either leaks its symbols into that table or has to be hidden by the
linker, and libc++ then collides with rustc's stubs over the exception
personality. As a leaf library it links its own libc++ statically, exports a
16-symbol allowlist of its own, and needs nothing from the ICD — the shader cache
arrives through a two-entry host table instead of a symbol.

```
game frame ──► engine ──► generated ──► slot ──► commit (paced)
                  │
                  └──────► real frame ──► slot ──► commit
```

The shim is an ICD, so it already holds the guest's `VkDevice`,
`VkPhysicalDevice`, `VkQueue`, the swapchain images and their acquire/present
timeline. The layer was only ever a way to reach those from outside a device it
did not own.

The daemon gains nothing: it keeps presenting whatever is committed. The layer
stays banned, and `disableLayer()` keeps deleting the manifest.

## Lossless.dll in the shim

The shim reads `Lossless.dll` itself. There is no app-side cache build in this
path: the DLL is the input, the SPIR-V modules are the output, and both steps
live in the code the shim already compiles.

The file is already inside the container — `LsfgVkManager` installs it at
`<container>/.local/share/lsfg-vk/Lossless.dll` — and the shim runs in that same
guest process, so it can open it by path like any other file.

The work splits by language, and the boundary is chosen where it is cheapest:

| Piece | Language | Notes |
|---|---|---|
| PE parse, RCDATA walk (type 10, 512 entries), variant select, SPIR-V adoption and binding renumbering, cache write/read/validate | **Rust** | Replaces `lsfg_dll.c` (740 lines of offset arithmetic and `malloc`) with modules under `icd_shim/src/lsfg/`. Exported `#[no_mangle] extern "C"` so `lsfg_shaders.cpp` is untouched, and the `local: *` version script keeps the three-export invariant. |
| DXBC → SPIR-V | C++ | `lsfg_dxbc.cpp` is 93 lines wrapping **dxvk's `dxbc`** static library — 22,690 lines, `lsfg-vk-android/thirdparty/dxbc`, v2.6.2. No Rust equivalent exists; reimplementing a DXBC compiler is out of scope. Compiled into the shim behind one ABI function, `lsfg_translate_dxbc`. |
| `vkr_lsfg`, `lsfg_{chain,alpha,beta,delta,gamma,generate,mipmaps,shaders,common,pacer}` | C++ | Unchanged. |

The Rust side keeps the file format exactly as it is: cache magic `0x4746534C`,
version 1, FNV-1a over the source DLL, the same `LsfgModuleSet` layout, so an
existing cache built by the app still loads.

Sequence at LSFG create, once per session:

1. Resolve the DLL: the container path above; absent ⇒ frame generation is off.
2. Resolve the variant. **The shim decides this, not the app**: query
   `VkPhysicalDeviceFeatures2` for `shaderFloat16` / `storageBuffer16BitAccess`
   on the device that will run the shaders. The app's `nativeSupportsFp16` is a
   driver-*name* guess and cannot be called here anyway — it takes an Android
   `Context`.
3. `lsfg_cache_matches_source` against the cache in the container's LSFG
   directory; rebuild only when it does not match (first run, a different DLL, a
   changed variant).
4. `lsfg_load_modules`; any non-`LSFG_OK` ⇒ frame generation off for the session.

Cost: ~200 ms on a cold build, per the existing call site's own note. It happens
at create, never on the present path, and the rebuild is skipped whenever the
DLL and variant are unchanged.

Consequence worth stating plainly: the cache can no longer go stale against the
driver or the DLL, because the thing that consumes it also validates it. What it
replaces is `LosslessScaling.resolveOrBuildCache` → `nativeBuildCache`, which
builds into app-private `filesDir/lsfg/` — a path the guest process cannot read.
That app-side path stays for the X11 `native` backend, which runs in the app
process.

## What changes in GameNative

Additive wiring only. The `legacy` layer and the `native` renderer are **not
modified, fixed or refactored** by this work — they keep exactly the behaviour
they have today.

- **`LsfgVkManager`** — a `BACKEND_GAMENATIVE_DRM` constant, and the `backend()`
  coercion at `:181-183` extended so the new value survives instead of being
  read as `native`. Its extras, DLL discovery/install and present mode are reused
  as they are; its cache build is not used by this backend.
- **`LsfgQuickMenuTab`** (`QuickMenu.kt:1806`, selector list at ~`:1914`) — a
  third entry and one string. Nothing else in the tab changes.
- **Launch** — push the config as container env alongside `GNDRM_HDR`, and skip
  the app-side cache build when the backend is this one.
- **`LsfgQuickMenuHelper.applyBackend/PresentMode`** — no change.

Per-container config reaches the shim as container env, like `GNDRM_HDR`:
`lsfgEnabled`, `lsfgMultiplier`, `lsfgFlowScale`, `lsfgPerformanceMode`,
`lsfgPresentMode`, and the DLL path.

## The engine's contract

Its existing interface is already the right one for an embedder, which is why
this is a port and not a rewrite:

```
vkr_lsfg_create(VkDevice, VkPhysicalDevice, const char* cache_path)
vkr_lsfg_configure(multiplier, target_rate, flow_scale, refresh_rate)
vkr_lsfg_prepare(w, h, format)          // rebuild internal targets if changed
vkr_lsfg_plan(capacity, source_frames)  // how many generations this frame yields
vkr_lsfg_process(cmd, source_image, w, h, generations)
vkr_lsfg_generate_into(cmd, generation, target_index, target_image, target_view, w, h)
```

No device extensions, no `features2`, no window system, no swapchain, no
present: compute + graphics on a caller-supplied device, writing into
caller-supplied images. `VKR_LSFG_MAX_GENERATIONS` is 3, matching multipliers
2–4. The app's own call site, for reference: `VulkanRendererContext.cpp` —
process at `:1343`, generate at `:1349`.

## Artifacts

| Item | Source | Notes |
|---|---|---|
| Engine + shaders | **vendored** into `lsfg/engine/` (`vkr_lsfg`, `lsfg_{chain,alpha,beta,delta,gamma,mipmaps,generate,shaders,common,pacer}`, `vk_dispatch`, `lsfg_host`), adopted from the `native` engine in GameNative | Built by `tools/build-lsfg.sh` into `liblsfg_engine.so`: a leaf library with its own 16-symbol allowlist (`lsfg/lsfg_engine.map`) and static libc++, so its `NEEDED` set is libc/libdl/libm only. |
| Engine seam | `lsfg/engine/lsfg_host.{h,cpp}` | The only edit to the vendored engine: `lsfg_shaders.cpp`'s two calls go through a host table the shim installs. Without it the engine would need symbols the ICD must not export. |
| DLL reader and cache | **written in Rust** in this repo (`icd_shim/src/lsfg/`): PE parse, RCDATA walk, variant select, SPIR-V renumbering, cache write/read/validate — replaces `GameNative/.../lsfg_dll.c` | Same on-disk cache format as the app's, so either can read the other's. |
| DXBC → SPIR-V | **vendored** into `lsfg/dxbc/` (wrapper) and `lsfg/thirdparty/dxbc/` (dxvk `dxbc` v2.6.2, 60 files, 1.2 MB, zlib) | C++, compiled into the shim; the only C++ beyond the engine itself. Exceptions stay enabled. |
| `Lossless.dll` | the user's own Lossless Scaling install (Steam 993090), installed by `LsfgVkManager` into `<container>/.local/share/lsfg-vk/` | Read by the shim at LSFG create. Never shipped, never redistributed. |
| Shader cache | `lossless_{fp16,fp32}.lsfgcache`, written by the shim into the container's LSFG directory | Derived from the DLL; rebuilt when `lsfg_cache_matches_source` fails. |
| Vulkan | already in the shim | Generated frames use the guest's device and queue. |
| Pacing | the existing commit path and its deferral; `lsfg_pacer` for the internal grid |

## Changes in gamenative-drm

No new crate and no new dependency beyond a build-time C++ compiler. The engine
is compiled into the existing cdylib, so the shipped artifact and its three
exported symbols do not change.

| File | Change |
|---|---|
| `tools/build-lsfg.sh` **(new)** | Builds `liblsfg_engine.so` for host (load test) and for `aarch64-linux-android28`: engine + `lsfg_host` + `lsfg_dxbc` + dxvk `dxbc`, `-O3`, `-static-libstdc++`, stripped, version-scripted. No `build.rs` C++ and no `cc` dependency: the ICD's Rust build stays a Rust build. |
| `lsfg/lsfg_engine.map` **(new)** | The engine's export allowlist: `lsfg_engine_abi`, `lsfg_engine_set_host`, `vkd_load`, `vkd_unload`, and the twelve `vkr_lsfg_*` entry points. |
| `icd_shim/build.rs` | Compile the C++ half: the engine (`vkr_lsfg`, `lsfg_{alpha,beta,delta,gamma,generate,mipmaps,chain,shaders,common,pacer}.cpp`), `lsfg_dxbc.cpp`, and dxvk's `dxbc` sources (`thirdparty/dxbc/src/{dxbc,spirv,util}`) as C++17, exceptions on. Permanent `-fPIC -O3 -DNDEBUG`. Link `c++_static`, `c++abi`, `unwind` statically so the `NEEDED` set and `tools/check-elf-deps.sh` stay as they are; `--exclude-libs,ALL` already hides their symbols. `lsfg_dll.c` is **not** compiled — the Rust modules replace it. Sources come from this repo's `lsfg/`, so the build has no dependency on a GameNative checkout. |
| `icd_shim/src/lsfg/mod.rs` **(new)** | The loader: `dlopen` (`GNDRM_LSFG_ENGINE`, else `liblsfg_engine.so` on the payload's search path), ABI check against `lsfg_engine_abi`, `dlsym` of all twelve `vkr_lsfg_*` pointers, and installation of the host table. `open()` returns `None` on any failure, so frame generation is off rather than fatal. Keeps `proxy.rs` (8077 lines) from absorbing a second subsystem. |
| `modules/gndrm-lsfg` **(new crate)** | The Rust half of the DLL work: `pe.rs` (PE parse, RCDATA walk), `cache.rs` (variant select, SPIR-V renumbering, cache write/read/validate), and the two host callbacks. A crate rather than a module for one reason: a cdylib exports every `#[no_mangle]` in its own crate, and this code has none — the table removes the link-time edge entirely. |
| `icd_shim/src/lsfg/pe.rs` **(new)** | PE parsing and the resource walk to RCDATA, in Rust: DOS/PE headers, `RVA → file offset`, section table, resource directory (named + ID entries, subdirectory flag), leaf read. Replaces the `pe_*`/`resource_*`/`collect_rcdata` half of `lsfg_dll.c`. |
| `icd_shim/src/lsfg/cache.rs` **(new)** | Variant selection (`fp16` 49 / `fp32` 98), SPIR-V adoption and binding renumbering (`OpDecorate` → `Binding`, `DescriptorSet`), the cache header and FNV-1a source hash, `build`, `matches_source`, `load`, `find`. Replaces the rest of `lsfg_dll.c`; same on-disk format, so the app's existing cache still loads. |
| `icd_shim/src/proxy.rs` — device create | Give the engine its device: `gd.real` and `gd.f.gdp` (the real driver's `vkGetDeviceProcAddr`) so it resolves its own entry points. Never the loader: resolving through the shim's exports would re-enter the interposer. |
| `icd_shim/src/proxy.rs` — swapchain create (`:836`) | When FG is on: add `TRANSFER_DST` to the slot usage, create the `multiplier − 1` destination slots, `vkr_lsfg_prepare` for extent/format, and re-`prepare` on recreate or resize. |
| `icd_shim/src/proxy.rs` — `GnSlot`, `GnSwapchain` | Destination slots are a second table (`gen: Vec<GnSlot>`) with the same pool allocation and fences, **not** appended to `slots`, which `vkGetSwapchainImagesKHR` hands to the guest. Each carries its `GnSlot` image plus the engine view it is blitted from. |
| `icd_shim/src/proxy.rs` — `present_fifo*` / `present_dispatch` (`:1441-1576`) | Queue generated jobs between real ones instead of only the real index. `take_job` alternates so a generated frame is committed after its source and before the next real frame. |
| `icd_shim/src/proxy.rs` — `worker_main` (`:755`) | Build one command buffer per generated frame: `vkr_lsfg_process` on the source slot, `vkr_lsfg_generate_into` for the generation, barrier, `vkCmdBlitImage` into the destination slot, submit on that slot's fence, then commit through the existing publish path. |
| `icd_shim/src/proxy.rs` — destroy paths | `vkr_lsfg_forget_targets` on swapchain destroy, `vkr_lsfg_destroy` on device destroy, destination slots released to the pool. |
| `icd_shim/tests/shim_integration.rs` | The mock driver must answer the engine's `gdp` lookups, or FG stays uninitialised in host tests; plus a unit test for the job plan (N presents in, N×(multiplier−1) generated jobs out, in order). |
| `tools/build-android.sh`, `tools/exports/*.map` | No change expected — static libc++, same three exports. Verify with `check-elf-deps.sh` and `check-exports.sh` rather than assuming. |

New source tree in this repo:

| Path | Contents |
|---|---|
| `lsfg/engine/` | The 11 engine translation units and their headers, copied from GameNative's `native` engine. `vk_dispatch.c`/`.h` gain a device-side init (`vkd.InitFromDeviceProcAddr(gdp, device)`), which is the only edit the copy needs. `lsfg_dll.c`, `lsfg_jni.c` and `lsfg_probe.c` are **not** taken: the first is replaced by the Rust modules, the other two are app-only (JNI entry points, diagnostics). |
| `lsfg/dxbc/` | `lsfg_dxbc.cpp` — the 93-line DXBC → SPIR-V wrapper. |
| `lsfg/thirdparty/dxbc/` | dxvk's `dxbc` v2.6.2, 60 files, 1.2 MB, zlib. Vendored because there is no Rust equivalent. |

Nothing outside this repo changes. The engine is a copy, so a fix to the
algorithm has to be applied in both places — the price of not making this repo's
build depend on a GameNative checkout.

**Language boundary.** `lsfg_dll.c` is not compiled into the shim: PE parsing,
resource walking, variant selection, the cache and the SPIR-V renumbering are
Rust. The DXBC→SPIR-V compiler is not Rust and will not be: it is dxvk's `dxbc`
library at 22,690 lines, pulled in as C++ behind the single `lsfg_translate_dxbc`
entry point. So the shim's build compiles ~22.7k lines of C++ once, and the
code we own at that boundary is Rust.

**Licensing: settled.** The engine is GPL-3.0-or-later — 20 of its files carry
`SPDX-License-Identifier: GPL-3.0-or-later`, `Copyright 2026 Eden Emulator
Project` — and the application it ships inside is already distributed under that
license (`GameNative/THIRD_PARTY_NOTICES`, "LSFG native pipeline
(libvulkan_renderer.so)"). Linking it into `libadrenotools_gamescope.so`
therefore changes nothing about the distribution's terms; the requirement is
attribution, which `THIRD_PARTY_NOTICES` in this repo now carries for both the
engine and dxvk's `dxbc` (zlib). The MIT `LICENSE` here continues to cover this
repo's own code — daemon, UAPI crate, obfuscation crates, tools — none of which
links the engine.

A note for the record: rewriting the engine in another language would not have
changed this. A translation of GPL code is a derivative work. The only
MIT-licensed route was lsfg-vk's `framegen/`, which owns a second device and
therefore carries the synchronous cost the comparison above rejects.

**The risk is the slot table, not the engine.** Present, commit, release and the
daemon's pool are all keyed on an index into `sc.slots`. Adding a second table
means every one of those paths learns a second index space; a mistake there
presents as a frame composited into the wrong buffer rather than as a compile
error. That is the part to review hardest, and the reason step 1 below ships with
one generation and no pacing change.

## Integration points

1. **Dispatch population.** The engine resolves entry points from a process-global
   `VkDispatch vkd`, filled by `vkd_load(instance, device, gipa)` — exported by
   the engine for this purpose. The shim must hand it the **real driver's**
   instance, device and `vkGetInstanceProcAddr`, never a loader lookup, or the
   engine's own resource creation re-enters the shim's entry points and the shim
   ends up tracking handles it did not create.
2. **A generated frame does not land in a guest slot image.** Slot images are
   `VK_IMAGE_TILING_LINEAR` and are created with the guest's own usage flags
   (`proxy.rs:7977`), while the generate pass writes its target as a **storage
   image** in `VK_IMAGE_LAYOUT_GENERAL` (`lsfg_generate.cpp:108-131`). Linear
   plus storage is not a supported combination here, so the engine keeps its own
   optimal-tiling targets and the shim blits the finished frame into the
   destination image, which therefore needs `TRANSFER_DST`. `vkCmdBlitImage` and
   `vkCmdPipelineBarrier` are already resolved in the shim's real function table
   (`cmd_blit_image`, `cmd_pipeline_barrier`), so this adds no driver entry
   point.
3. **Image count.** `multiplier − 1` extra images are in flight; the swapchain's
   `max_swapchain_images` and the slot table must allow them, or a generated frame
   waits for a released slot.
4. **Frame capture has to happen before the commit, not after.** The engine's
   `vkr_lsfg_process` copies its source out of `VK_IMAGE_LAYOUT_GENERAL` — correct
   for the app, whose source is a compute-written composite image, but a guest
   swapchain image arrives in `PRESENT_SRC_KHR`. The shim must transition in and
   back out, and must do it in the submission that already waits the slot's
   fence and *before* handing the buffer to the compositor: once committed, the
   next release can return the slot to the guest, which will render into it.
5. **Commit and pacing.** A generated frame enters the daemon's deferral exactly
   like a real one. Whether consecutive commits land on consecutive deadlines or
   arrive as a burst is unproven; if they burst, the shim spaces the presents and
   the daemon stays unchanged.
6. **Format.** 8-bit BGRA is the shipped case. 10-bit (`A2B10G10R10`) storage in
   the engine is plausible on this driver and untested — a goal, not an assumption.
7. **Where the cache is written.** Inside the container, next to the DLL, so a
   guest-side rebuild and a guest-side read use the same file the app installed.
   The app-private `filesDir` store is not reachable from here.

## Goals to verify

1. **It runs in gamescope at all.** Hades II, multiplier 2: presented frames ≈ 2×
   the guest's presents; `acq == present`, `starved = 0`; daemon commit rate
   matching.
2. **Even pacing, not a burst.** Present-interval distribution at the panel: with
   a 60 fps guest and multiplier 2 the intervals sit on the 120 Hz grid. Report
   mean/median/p95 and the fraction outside ±10 % of the grid; compare with the
   X11 `native` backend.
3. **Generated frames are not copies.** Frame-to-frame difference statistics for
   real and generated frames reported separately; a generated frame that repeats
   its input, tears, or is misplaced is an outlier.
4. **Cost.** GPU busy %, GPU clock, package power, temperature at 2× and 3×
   against the same title with FG off, plus shim and daemon CPU. If the
   multiplier is not worth that cost on this class of device, say so.
5. **Latency.** Guest present → last generated commit must not grow with the
   multiplier: distributions at 1×/2×/3× and the queue depth.
6. **HDR.** PQ session (10-bit scan-out, `buffer10 = true`): generated frames carry
   the same encoding, no colour shift, no metadata churn.
7. **Robustness.** Swapchain recreate, resolution change, game pause/resume,
   teardown: no leaked slot fds, no unbounded pool growth, no commit stall.
8. **The DLL is read and the cache built shim-side.** On a container that has the
   DLL but no cache, and with the app never asked to build one, the session
   produces a cache and frame generation runs. Replacing the DLL causes a rebuild;
   relaunching with it unchanged does not (one build per session at most).
9. **Absent or unusable DLL degrades safely.** No `Lossless.dll`, an unreadable
   one, or a failed translation: FG reports unavailable and the session runs
   unframed — never a crash, never a black frame, never a stall at create.
10. **The layer stays banned.** With FG on, a gamescope session still loads no
    `VkLayer_LS_frame_generation.json`; the shim must not resurrect the
    `vkCreateDevice` failure that forced the ban.

## Order of work

1. ~~Vendor `lsfg/` and build `liblsfg_engine.so`.~~ Done: host and Android
   builds, 16-symbol allowlist, `NEEDED` = libc/libdl/libm, and a load test that
   opens the library, checks the ABI and installs the host table.
2. ~~The Rust `lsfg/` modules (`pe.rs`, `cache.rs`).~~ Done: 11 unit tests,
   including a byte-for-byte cache round-trip and the SPIR-V renumbering order.
3. ~~Wire the shim.~~ Done: the device is created, `vkd_load` is handed the real
   driver's table, the cache is built from `GNDRM_LSFG_DLL` when it does not
   match, the engine context is created and configured, and the session is torn
   down with the device. fp16 is chosen only when the **guest** enabled
   `shaderFloat16` on the device being created — the feature has to be on the
   device the pipelines are built on, and that device is not ours.
4. Swapchain: destination slots, `vkr_lsfg_prepare` on create/recreate, and the
   generation plan — still with nothing presented.
5. One generated frame per real frame, multiplier 2, no pacing change: prove it
   reaches the display intact (goal 1).
6. Pacing in the shim if the commits burst (goal 2).
7. Cost and latency at 2× and 3× (goals 4, 5); decide whether fp16 and
   performance mode change the answer.
8. HDR (goal 6), robustness (goal 7), absent-DLL path (goal 9).
9. Re-check the layer ban (goal 10) and the X11 backends for regressions.

## Non-goals

- Making the Vulkan layer work under gamescope; it stays banned.
- Any change to the daemon's presentation model beyond what pacing requires.
- Distributing Lossless Scaling or its `Lossless.dll`; the app installs the user's
  own copy and nothing else ships.
- Frame generation for X11 sessions: `legacy` and `native` keep that path and are
  not modified by this work, including the app-side cache build they already do.
- Sharing one engine copy between the two display servers: GameNative's copy and
  this repo's vendored copy are independent.
