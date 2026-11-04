# 17 — LSFG frame generation under gamescope

Status: **implemented.** Backend `gamenative_drm`; frame generation runs on the
guest's own `VkDevice` and queue, driven by the ICD shim.

## Shape

The engine is a leaf shared library, `liblsfg_engine.so`, loaded by the ICD shim
in the guest process. The shim already holds the guest's device, physical device,
queues, swapchain images and their acquire/present timeline; the engine needs
nothing else.

```
libadrenotools_gamescope.so (ICD, 3 exports)          liblsfg_engine.so
  dlopen/dlsym ──────────────────────────────────────▶ vkr_lsfg_* (frame gen)
  host table  ◀────────────────────────────────────── lsfg_load_modules/release
```

The split is forced by the ICD's export gate: `check-exports.sh` pins
`libadrenotools_gamescope.so` to the three Khronos loader entry points, and a
cdylib exports every `#[no_mangle]` in its own crate. As a leaf library the
engine links its own libc++ statically, exports a 16-symbol allowlist
(`lsfg/lsfg_engine.map`) and reaches the shim only through a two-entry host
table.

The X11 `legacy` layer stays banned: its failed device init takes gamescope's
`vkCreateDevice` with it. `LsfgVkManager.disableLayer()` keeps deleting the
manifest for a gamescope session.

## Engine contract

```
vkr_lsfg_create(VkDevice, VkPhysicalDevice, const char* cache_path)
vkr_lsfg_configure(multiplier, target_rate, flow_scale, refresh_rate)
vkr_lsfg_prepare(w, h, format)          // rebuild internal targets if changed
vkr_lsfg_plan(capacity, source_frames)  // how many generations this frame yields
vkr_lsfg_process(cmd, source_image, w, h, generations)
vkr_lsfg_generate_into(cmd, generation, target_index, target_image, target_view, w, h)
vkr_lsfg_forget_targets() / vkr_lsfg_destroy()
```

No window system, no swapchain, no present. `VKR_LSFG_MAX_GENERATIONS` is 3
(multipliers 2–4). The device is handed over as `gd.real` plus the real driver's
`vkGetDeviceProcAddr`, never a loader lookup: resolving through the shim's
exports would re-enter the interposer.

## Present path

Runs on the present worker after the guest's frame is committed, so the capture
happens before the source buffer can return to the guest.

1. One batch at a time. If the previous batch has not retired, this frame goes
   out unframed and `GEN_SKIPS` increments. There is no wait: a global "batch in
   flight" gate turns a chain slightly over budget into a stalled session.
2. Capture: `vkr_lsfg_process` copies the source out of `PRESENT_SRC_KHR`, and
   the shim transitions in and back out around it.
3. Generate: `vkr_lsfg_generate_into` per generation into the caller's target
   slot image, which needs `TRANSFER_SRC|TRANSFER_DST`; the guest slot needs
   `TRANSFER_SRC`.
4. One `vkQueueSubmit` on a family-matched queue, signalled fence on
   `gd.gen_fence`, command buffer freed only after that fence retires.
5. Commit: each generated frame is queued for a latch point of the panel (see
   below) and committed by `flush_due`, which waits the batch fence first — the
   destination is a plain dma-buf, so neither gamescope nor the display can wait
   on it, and attaching early shows an unwritten image.

`GEN_COMMITS` counts committed generations, `GEN_SKIPS` counts batches dropped
for this reason; one rate-limited line (`lsfg: generated N (skips M)`, every 600
batches) reports both.

## Pacing to the panel

SurfaceFlinger takes the newest buffer a layer holds at each of its latch
points, one panel period apart, and shows it at the vsync after. Nothing between
the shim and the panel paces to that grid — gamescope's tick is its mode's rate,
the display's, and the daemon's `ASurfaceTransaction_apply` carries no present time —
so a presenter that commits on its own timer beats against it: every so often
two commits fall inside one period and the first is replaced before it is shown,
and a rate above the panel's (60 × 3 on a 120 Hz panel) drops frames in a
drifting pattern. Three rules keep one frame per period, in order:

- **Multiplier.** A batch generates `multiplier - 1` frames and the source frame
  after them, one batch per acquired source frame, so the session produces
  `cap × multiplier` frames whatever the display can latch: a 40 cap at x4 on a
  120 Hz panel produces 160 and the panel shows 120 of them. Generation is not
  capped by the panel: the mode is the display's rate and never moves for a cap or
  a multiplier, so capping the output by it would only make x4 at 40 read 120 and
  say nothing about what the session was doing. The rate is the user's cap, or
  the measured source interval without one (a source hovering at the threshold,
  64 fps on 120 Hz, flips between x2 and none from batch to batch), and the
  display never reshapes it: a 40 cap runs 40 acquires a second on a 60 Hz panel
  as much as on a 120 Hz one, and the multiplier decides what is generated on
  top. The source rate is held by the acquire gate on the cap's own clock
  (`scanout::next_acquire_after`, `gate=clock` in the census), not on the
  display's vblank grid: the vblank-grid gate of `docs/10` covers a cap without
  frame generation only, and generated frames are placed on the latch grid
  below. The panel's rate is the one the app reported (`LsfgConfig.refresh_mhz`),
  not a measurement: while the daemon is the only thing publishing, the latch
  samples are spaced by its own rate, so a panel SurfaceFlinger runs below that
  rate (an LTPO panel that dropped to 60 Hz) is still paced at the reported rate
  and shows every other frame, as before.
- **Grid.** The daemon reads `ASurfaceTransactionStats_getLatchTime` on every
  completion, tracks the grid's phase (`vsync.rs`), and pushes a `VsyncNote`
  (`anchor_ns, period_ns, lead_ns`) on the scan-out channel whenever it moves
  by more than 300 µs, and once on subscribe — only to a shim whose
  `FrameCapSubscribe` set `CAP_VSYNC_NOTE`, since an older shim's reply reader
  takes an unknown frame for a dead channel. The shim gives the frames of a
  batch consecutive latch points from the one after the previous queued
  frame's, or the first a commit can still reach when that has passed
  (`pacing::Grid::first_latch`), and commits `lead_ns` ahead of each. A batch
  with more frames than the panel has points in a source period goes out on the
  shim's clock spread evenly over that period instead: on the grid the source
  frame would follow the batch's last point, a whole period late, and the queue
  would carry that lateness forever. Without a note the frames are spread the
  same way; the order rules below apply either way.
- **Lead.** The daemon stamps when its apply returned and measures how far ahead
  of the latch it was; a loop moves the lead so that margin sits at half a
  period, the middle of the window between two latch points, with the error
  taken modulo the period (a frame that just missed its
  point reads as late, not as a period early). A pipeline slower than one period
  settles one latch point later, which is still one frame per period. The loop
  runs only while generation is on and the connection in service subscribed to
  the note: any other frame's margin does not follow the lead.

Order is kept on the skip path too: a source frame whose batch could not be
built (`GEN_SKIPS`) is queued behind what is already waiting instead of being
committed inline ahead of it — on the next latch point, or one output step
later without a grid — and the present worker waits a guest frame's fence in
1 ms slices while frames are queued, flushing the due ones between slices, so a
slow guest frame no longer holds the queued frames of the previous one. A guest
running ahead of the output (uncapped and faster than the panel) would stack a
frame of latency per present this way, so a queued source frame that is not due
for a source period is replaced by the newer one, and its slot goes back.

## Driver constraints

The properties this path rests on — queue-family matching, the engine's own output
image, the blit capture, destination depth, guest-only release pacing, the `Rgba8`
storage binding and the settings channel — are in
[18-lsfg-driver-facts.md](18-lsfg-driver-facts.md). Each is a property of the
driver or of the presentation model, and getting one wrong disables generation
without reporting an error.

## Settings and artifacts

Per-container config arrives as container env, like `GNDRM_HDR`: `GNDRM_LSFG`
(multiplier), `GNDRM_LSFG_DLL`, `GNDRM_LSFG_CACHE`, `GNDRM_LSFG_FLOW`,
`GNDRM_LSFG_FPS_LIMIT`, `GNDRM_LSFG_ENGINE` (library path override). The daemon's
per-container config wins over the environment when present.

| Item | Source | Notes |
|---|---|---|
| Engine + shaders | vendored in `lsfg/engine/`, built by `tools/build-lsfg.sh` | Leaf library, 16-symbol allowlist, static libc++, `NEEDED` = libc/libdl/libm. |
| Engine seam | `lsfg/engine/lsfg_host.{h,cpp}` | The only edit to the vendored engine: `lsfg_shaders.cpp`'s calls go through the host table. |
| DLL reader, cache | Rust, `icd_shim/src/lsfg/{pe,cache}.rs` | PE parse, RCDATA walk, variant select (fp16 49 / fp32 98), SPIR-V renumbering, cache write/read/validate. Same on-disk format as the app's. |
| DXBC → SPIR-V | `lsfg/dxbc/` + `lsfg/thirdparty/dxbc` (dxvk `dxbc` v2.6.2) | C++; no Rust equivalent. |
| `Lossless.dll` | the user's own Lossless Scaling install, installed by `LsfgVkManager` into `<container>/.local/share/lsfg-vk/` | Read by the shim at create. Never shipped or redistributed. |
| Cache | `lossless_{fp16,fp32}.lsfgcache` beside the DLL | Rebuilt only when the source hash or variant changes. |
| Licensing | engine GPL-3.0-or-later, dxvk `dxbc` zlib | Attribution in `THIRD_PARTY_NOTICES`; this repo's own code is GPL-3.0-or-later too. |

fp16 is chosen only when the **guest** enabled `shaderFloat16` on the device
being created — the feature has to be on the device the pipelines are built on.

## Non-goals

- Making the Vulkan layer work under gamescope; it stays banned.
- Changing the daemon's presentation model beyond pacing: the latch grid is
  measured and reported, not driven (no `setDesiredPresentTime`, no frame-rate
  vote).
- Distributing Lossless Scaling or its DLL.
- Frame generation for X11 sessions: `legacy` and `native` keep that path.
- Sharing one engine copy between GameNative and this repo: the copies are
  independent, so an algorithm fix has to be applied in both.
