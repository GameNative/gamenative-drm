# 31 — Scan-out pixel format: UBWC cannot be expressed by an `AHardwareBuffer`

Scope: read-only. `gamenative-drm` @ `performance-report` (3588fa9), `gamescope` @
`audit/check-patches` (ddf9ffa8), `mesa` working tree, plus the unmerged branch
`backup-image-count-20261107-0904` which contains a prior attempt at exactly this item
(§6). Everything below is quoted with file:line.

---

## 0. Verdict

**Not worth doing as scoped. Do one env-var measurement first, and if it clears, resurrect
the 219-line attempt that already exists on an unmerged branch — do not build a KMS
modifier path.**

Three findings drive that, in order of weight:

1. **The prize is bounded by +1.5 points of CPU, and item 7 cannot plausibly collect more
   than a fraction of one.** The measured gap is *sys-bound at equal GPU load*
   (`docs/24-gamescope-vs-x11-cpu.md:39-44`: cpu_sys +1.6 pts, cpu_user −0.5 pts,
   gpubusy +0.7 pts at −1.6 % fps). Bandwidth does not produce sys time; it produces GPU
   idle time. The only mechanism that turns a bandwidth stall into CPU in this stack is
   cause **B**'s spin-based `vkWaitForPresentKHR` (`docs/24:156-163`). Fix B and item 7's
   CPU contribution goes to approximately zero by construction. Item 7 is correctly
   ranked last and should stay gated behind items 2–4.
2. **The daemon cannot scan out UBWC today and the reason is one usage bit that is there
   on purpose.** The pool is LINEAR *because* `CPU_READ_OFTEN` is in the pool usage, and
   two separate places in the tree already say so in comments
   (`modules/gndrm-android/src/ffi.rs:96-98`, `icd_shim/src/proxy.rs:12399-12401`). If
   that bit is what pins the layout, the whole KMS-modifier edifice in §3 is unnecessary —
   drop the bit and gralloc gives UBWC or it does not, which is a *measurement*, not a
   design (§4).
3. **This item was already implemented and is not on the branch.** Commit `e14c2a7`
   (2026-11-07, branch `backup-image-count-20261107-0904`) does compression negotiation,
   pooling, import and labelling in 6 files / +219/−25, with no daemon change and no
   gamescope change, and its own verification line (`proxy slot: pool layout modifier
   0x… (linear|compressed/tiled)`) was never recorded as run. It is not an ancestor of
   `performance-report` HEAD. That is the honest cost estimate for the *useful* form of
   this item: two days of one person's work, already done once, unverified.

The thing that would change this verdict is measurement, not analysis: on this GPU at
720p, uncompressed scan-out costs ~1.25 GB/s per full-frame pass against a 68.3 GB/s peak,
i.e. a frame's whole traffic (~5 GB/s over four passes) is ~7 % of peak DRAM — see §4.

---

## 1. What the daemon can scan out today

### 1.1 The allocation

One site, `modules/gndrm-android/src/asc.rs:3455-3501`:

```rust
// asc.rs:3461-3468
let desc = ffi::AHardwareBufferDesc {
    width, height, layers: 1,
    format: served_ahb_format(format),      // BGRA_8888(5) | RGBA_8888(1) | R10G10B10A2(0x2b)
    usage: ffi::SCANOUT_POOL_USAGE,
    ..Default::default()
};
let rc = unsafe { ffi::AHardwareBuffer_allocate(&desc, &mut ahb) };  // asc.rs:3472
```

`SCANOUT_POOL_USAGE` (`ffi.rs:99-102`) is
`CPU_READ_OFTEN | GPU_SAMPLED_IMAGE | GPU_COLOR_OUTPUT | COMPOSER_OVERLAY`. The comment
above it is the load-bearing sentence for this whole analysis:

> `CPU_READ_OFTEN` was added afterwards for the panel dump, so the linear-and-directly-
> composable result does not attest this set — **gralloc may pick a different layout once a
> CPU-visible usage is present.** (`ffi.rs:96-98`)

The AHB descriptor has **no modifier and no plane count field** — four `u32`, a `u64` and
two reserved words (`ffi.rs:67-76`) — and `AHardwareBuffer_describe` on the way back yields
only `stride`, which the daemon multiplies by 4 to get a byte pitch (`asc.rs:3488-3491`).
That pair of facts is the platform limit: a linear-pitch model is the only model the public
Android API can express.

### 1.2 It never imports a guest buffer for scan-out

The direction is fixed and documented: the presenter can only publish an
`AHardwareBuffer` it allocated itself, because one cannot be reconstructed from a bare
dma-buf fd (`ffi.rs:46-55`, `icd_shim/src/scanout.rs:3-13`). The shim therefore *asks* for
a pool buffer per swapchain image (`proxy.rs:12093`, `scanout::allocate` →
`scanout.rs:330-337`, a `ScanoutAllocReq` of `{width, height, format, len = w*h*4}`) and
renders into it; the same AHB is published.

When a commit names an ino the pool does not own, the presenter tries a row copy into a
pool buffer, else re-publishes the last frame (`asc.rs:1308-1351`):

```rust
// asc.rs:1331-1334
// A buffer the pool does not own is copied into one the presenter
// does, rather than dropped. Only a pool miss reaches this, so the
// zero-copy path is unchanged.
```

and the copy planner refuses anything tiled or compressed before it ever touches memory
(`modules/gndrm-daemon/src/foreign.rs:49-60`, `:91-96`):

```rust
if fb.flags & DRM_MODE_FB_MODIFIERS != 0 {
    let m = fb.modifier[0];
    if m != DRM_FORMAT_MOD_LINEAR && m != DRM_FORMAT_MOD_INVALID {
        return Err(CopyRefusal::Modifier(m));
    }
}
```

So a compressed *client* framebuffer today ends as a dropped or stale frame
(`asc.rs:1437-1444`, `:1455-1465`). That is the failure mode a UBWC pool would have to
avoid, not create.

### 1.3 What it advertises to gamescope

| property | value | site |
|---|---|---|
| primary plane formats | `XR24, AR24, XB24, AB24, RG16` (BGRA) / `XB24, AB24, RG16` (RGBA) | `docs/02:87`, `primary_formats_for` `lib.rs:146-153` |
| primary plane modifiers | `DRM_FORMAT_MOD_LINEAR` only, `count_modifiers = 1` | `lib.rs:2352-2371` |
| `DRM_CAP_ADDFB2_MODIFIERS` | 1 ("Explicit modifiers accepted (LINEAR only; see IN_FORMATS)") | `lib.rs:1187-1188` |
| plane count | 1 (formats are single-plane; a second handle is a copy refusal) | `foreign.rs:105` |
| pitch | the AHB's own `stride * 4`, carried to the guest by `gamenative_drm_scanout_pitch` and to gamescope by the A51 patch | `asc.rs:3491`, `gndrm_shim.c:1396-1412`, `gamescope src/rendervulkan.cpp:2709-2713` |

`in_formats_blob_data` (`lib.rs:2354-2371`) writes exactly one `drm_format_modifier` entry,
masked over every format, with modifier `DRM_FORMAT_MOD_LINEAR`. `ADDFB2` *does* accept an
arbitrary modifier and records it (`lib.rs:1937-1944`) — deliberately, because gamescope can
direct-scan-out a client dma-buf:

> Modifiers are recorded, not gated on: gamescope scans out the client dma-bufs directly,
> and a compressed (UBWC) slot is one of them.

That is a real, measured state of the world: the daemon has already seen a compressed
framebuffer come through, and `modules/gndrm-daemon/src/scanout.rs:961-989` encodes the
number — **the Adreno UBWC surface for 1280x960 is 4 947 968 B, not the 4 915 200 B a LINEAR
image needs** (+0.667 %, and the test says the earlier refusal "was this exact lookup
returning `None`"). The length index was extended to cope with compression leaking in; the
layout was never made to work.

### 1.4 What the shim creates

`slot_init_with_usage` (`proxy.rs:12060-...)`, one image per slot:

```rust
// proxy.rs:12124-12147
let ici = VkImageCreateInfo {
    pNext: &ext_img,                       // DMA_BUF only
    format: (*sc).format,
    extent: { width, height, 1 },
    tiling: VK_IMAGE_TILING_LINEAR,        // <-- the contract
    usage, sharingMode: EXCLUSIVE, ...
};
```

with the comment at `proxy.rs:12138-12140` stating it outright: *"TILING_LINEAR forbids UBWC
outright, so the fd is always plain scan-out pixels."* The declaration above it
(`proxy.rs:12046-12053`) adds *"Either way the driver produces plain scan-out pixels (LINEAR
forbids UBWC outright) and DRI3 wayland present see a modifier of 0."*

Two fallbacks exist and both are LINEAR (`proxy.rs:12156-12182` explicit-LINEAR modifier at
the pool stride; `proxy.rs:12192-12240` the same when the driver's pitch ≠ the pool stride).
The `AHardwareBuffer` route (`slot_init_ahb`, `proxy.rs:12395-12545`) uses
`VK_IMAGE_TILING_OPTIMAL` and states why that is still linear:

> Images backed by an `AHardwareBuffer` must use OPTIMAL tiling; **the layout is the one
> gralloc chose for the pool's usage (CPU-readable, so linear)**, which is what the
> compositor reads through the fd. (`proxy.rs:12399-12401`)

And the `wl_buffer` carries modifier 0 into the compositor at `proxy.rs:12558`:

```rust
wlproto::params_add(params, (*s).fd, 0, (*s).offset, (*s).stride, (*s).modifier); // == GN_DRM_MOD_LINEAR
```

The protocol field exists (`wlproto.rs:610-629`, `zwp_linux_buffer_params_v1.add` opcode 1
with the hi/lo modifier words) and is always fed zero. The shim does not even listen for the
compositor's `modifier` event today — the interface declares it (`wlproto.rs:523-533`) but
nothing consumes it on the default route.

---

## 2. What Adreno UBWC actually requires

### 2.1 Through AHardwareBuffer: not expressible, only implicit

- `AHardwareBuffer_Desc` has no modifier field (`ffi.rs:67-76`); the format is a HAL pixel
  format from a fixed enum of ~30 values with no UBWC member; `stride` is an output on
  `describe` and ignored on `allocate`; `rfu0`/`rfu1` are reserved. There is no
  `AHardwareBuffer_createFromHandle` and no modifier round-trip
  (`ffi.rs:46-55`).
- Requesting UBWC explicitly means vendor gralloc metadata (gralloc4 `IMapper`'s
  vendor-metadata typed buffer, or gralloc1 producer-usage bits) — neither is in the NDK,
  and `AHardwareBuffer_allocate` gives no way to pass either. **A non-privileged app cannot
  ask for UBWC.**
- What a non-privileged app *can* do is stop asking for CPU access and let gralloc choose.
  On QCOM that is the actual mechanism by which Android apps get UBWC, and it is precisely
  what `e14c2a7` did (`CPU_READ_OFTEN` moved behind a `pool_usage(cpu_readable)` gate that
  is false when a dump is not configured), so the flag-off state is already written.
- Privilege is *not* the gate on ability; it is the gate on expressiveness. UBWC allocation
  is available to any process. The layout is unnameable, not forbidden.

### 2.2 Through the DRM side: one constant, and gamescope is not the wall

`DRM_FORMAT_MOD_QCOM_COMPRESSED = fourcc_mod_code(QCOM = 0x05, 1) =
0x0500_0000_0000_0001` (`/usr/include/drm/drm_fourcc.h:482`, `:831`). The daemon's own
foreign-copy test already uses that exact literal as its "tiled" case
(`modules/gndrm-daemon/src/foreign.rs:267`).

Verified in gamescope source: **the string `QCOM`, `UBWC` and `COMPRESSED` do not appear
anywhere in `gamescope/src/` outside the vendored Vulkan registry JSON.** There is therefore
no fourcc↔modifier table to extend, because gamescope has no table at all — it is
data-driven end to end:

| step | site | what it needs |
|---|---|---|
| harvest plane formats+modifiers | `DRMBackend.cpp:684-719` — walks the plane's `IN_FORMATS` `drm_format_modifier_blob` and `wlr_drm_format_set_add(format, modifier)` for every bit | the daemon must put `QCOM_COMPRESSED` in the blob |
| gate a client dma-buf for scan-out | `DRMBackend.cpp:1972-1977` — `if (!wlr_drm_format_set_has(&drm->formats, dma_buf->format, dma_buf->modifier)) { "not supported for scan-out"; return nullptr; }` | same blob |
| carry the modifier to KMS | `DRMBackend.cpp:1999-2005` — `drmModeAddFB2WithModifiers(..., DRM_MODE_FB_MODIFIERS)`, gated on `drm->allow_modifiers` (`:1652`, `:4380`) | nothing; the daemon already sets `ADDFB2_MODIFIERS = 1` |
| import a dmabuf with any modifier into Vulkan | `rendervulkan.cpp:2820-2912` — modifier list create-info, `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`, then `drmFormatModifier`/plane count read back | Turnip must report the modifier for the format+usage |
| composite / scan-out target | `rendervulkan.cpp:2915-2955` (gamenative A51) — currently **forces** an explicit `DRM_FORMAT_MOD_LINEAR` at the pool pitch | this block would have to become "follow the AHB's real modifier and plane count" |

So: gamescope's engine already has the machinery (`VK_EXT_image_drm_format_modifier`,
multi-plane `wlr_dmabuf_attributes`, `AddFB2WithModifiers`). What it does not have is any
knowledge of *this* modifier's alignment rules (UBWC needs width/height aligned to the
tile, the pitch is not a byte stride, the metadata plane is a separate allocation). Nothing
in gamescope validates that; it would simply trust the numbers.

Two hard gates remain, and neither is in gamescope:

- **`libliftoff`** reads the same blob and refuses to place a plane whose
  (format, modifier) is not advertised: `subprojects/libliftoff/plane.c:309-353`
  (`plane->in_formats_blob` → match format → match modifier). Without
  `QCOM_COMPRESSED` in the blob, a compressed plane is silently not composited by KMS.
- **The `AHardwareBuffer`'s metadata fd.** `dup_scanout_fd` takes exactly **one** dma-buf
  descriptor out of the native handle (`asc.rs:3714-3731`, via `scanout_dmabuf_fd`), the
  wire reply carries exactly one fd with `{ret, ino, stride}` and no room for anything else
  (`modules/gndrm-uapi/src/wire.rs:399-416`), and `params_add` is called for plane 0 only
  (`proxy.rs:12558`). QCOM gralloc's compressed handles carry the UBWC metadata as a second
  buffer in the same native handle — the tree already knows handle layouts vary
  (`asc.rs:3755-3760`: MediaTek's lead fd is not even a dma-buf). **Rendering into a
  compressed pool buffer from the guest or from gamescope needs that metadata (a second fd,
  or one dma-buf with a plane layout) to travel**, on both the wire and the wayland buffer.
  That is a protocol change, not a flag — and it is the first thing to verify on device,
  because if the metadata fd cannot be reached the item is over.

That last point is the one that turns item 7 from "pool format change" into "protocol
change", and it is why the attempt in §6 stayed on the *AHB-handle* route (where the driver
receives the whole native handle and needs no second fd on the wire) instead of the fd
route.

---

## 3. Blast radius, component by component

Assume the goal is "the guest's swapchain image and/or gamescope's scan-out image is UBWC".
Seven components must agree, and two of them are wire formats.

| # | component | what must change | why it can't be local |
|---|---|---|---|
| 1 | **pool allocation** `asc.rs:3455-3501`, `ffi.rs:99-102` | drop `CPU_READ_OFTEN` for non-dump sessions | breaks `dump_published` (`asc.rs:1814-1846` locks the AHB with `CPU_READ_OFTEN`) and `content_sample` (`asc.rs:1780-1790`); also the plain-`lock` path in `foreign.rs:170-179`. Diagnostic-only, so gated, but it is the one-line that decides everything |
| 2 | **wire protocol** `gndrm-uapi/src/wire.rs` | `ScanoutAllocResp` needs modifier + plane count (+ a second fd or a plane offset); `ScanoutAllocReq` needs a "compressible" request bit | `ScanoutAllocResp` is a fixed 24-byte struct with `{ret, ino, stride}` (`wire.rs:399-416`). Both ends ship together: the shim is inside the guest, the daemon is inside the APK — a mismatch is a black screen, not a fallback. `ScanoutLayout` (`wire.rs:222-228`) names only `bgra`/`rgba`; folding compression into it would also change `nativeScanoutLayout()` (`modules/gndrm-android/src/lib.rs:1309`) and its Kotlin caller |
| 3 | **shim image creation** `proxy.rs:12060-12240` | create at the buffer's real modifier instead of `VK_IMAGE_TILING_LINEAR` | the LINEAR create is the *layout contract* for the guest's swapchain; a compressed slot must be created as `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` with an exact plane layout, which Turnip may only accept at its own alignment — and DXVK's swapchain usage (colour attachment + transfer src) has to be supported for that modifier |
| 4 | **wayland buffer** `proxy.rs:12547-12572`, `wlproto.rs:610-629` | pass the real modifier, and listen for the compositor's `modifier` event to know whether it will take it | the field is already there and always zero; but a modifier the compositor did not advertise gets the whole commit rejected at `DRMBackend.cpp:1972` (composited elsewhere, or a black window) — hence the negotiation in `e14c2a7` |
| 5 | **interposer AddFB2** `gndrm_shim.c` → `lib.rs:1926-2033` | nothing to *accept* (already permissive), but the pitch check at `lib.rs:1951-1958` is a byte-pitch model | `min_pitch = width*bpp/8` and `need = offset + pitch*(h-1) + width*4` are LINEAR arithmetic; a UBWC pitch is not a row stride and the check would pass nonsense. Also `addfb_geometry` (`lib.rs:2003-2013`) feeds the pool's length index from `st_size`, which is what today lets a compressed length resolve — that coupling gets more fragile, not less |
| 6 | **daemon pool** `modules/gndrm-daemon/src/scanout.rs` | `ROW_ALIGN_MIN..ROW_ALIGN_MAX` / `PAGE_LEN` (`:36-40`) and `row_padding_for` (`:191-198`) are a linear-layout model of the length index | this is exactly what the `compressed_allocation_length_resolves_to_its_geometry` test (`:961-989`) patches around; a pool whose *own* buffers are compressed needs the length index keyed by modifier too |
| 7 | **presenter** `asc.rs` `build_show`/`setBuffer` (`asc.rs:1308-1469`) | nothing structural — SurfaceControl is layout-agnostic and SF/HWC natively accepts UBWC | but the *reverse* path gets worse: every foreign commit must still be copyable, and `plan_copy` still refuses modifiers (`foreign.rs:91-96`). A UBWC pool plus a UBWC guest buffer plus a direct-scanout commit means the presenter publishes an ino it owns; fine. Anything else drops a frame |

Plus **`GNDRM_SCANOUT_LAYOUT`** and its app-side twin: it is a *format* switch (`bgra|rgba`)
read independently by the app (`layout.rs:89`, `lib.rs:1309`), the shim (`scanout.rs:236-241`)
and the daemon (`lib.rs:179-205`), and the request format is a hard contract
(`asc.rs:3415-3430` refuses a mismatched format with `EINVAL`). Adding compression to that
variable adds a third agreement point to a mechanism whose failure mode is a channel-swapped
or blank frame.

### Tests and goldens in `gamenative-drm` that break

- `modules/gndrm-daemon/tests/golden.rs:834-915` `in_formats_blob_on_both_planes` — asserts
  `u32at(4) == 1` (`count_modifiers`), `modifiers_offset == 24 + 4*n`, and the modifier word
  equals `DRM_FORMAT_MOD_LINEAR` at a fixed offset. Adding `QCOM_COMPRESSED` to `IN_FORMATS`
  breaks it on structure, not on values.
- `modules/gndrm-daemon/src/foreign.rs:263-290` `refusals` — asserts
  `Err(CopyRefusal::Modifier(0x0500_0000_0000_0001))`. That is *the* UBWC modifier; the test
  encodes "we can never copy a compressed buffer", which stays true, so it survives a
  compressed *pool* but not a compressed *foreign* path.
- `modules/gndrm-daemon/src/scanout.rs` — `compressed_allocation_length_resolves…`
  (`:961-989`), `padded_allocation_length_resolves…` (`:943-959`),
  `row_padding_never_stands_in_for_a_different_size` (`:1016-1029`),
  `least_padded_geometry_serves_an_ambiguous_length` (`:1045-1056`) all assert the linear
  pitch model. They survive only if the pool keeps serving `(alloc_len, format)` for both
  layouts, which is what the first one already does.
- `modules/gndrm-uapi` ABI ground-truth test (`tests/abi_ground_truth.rs`) — if
  `ScanoutAllocResp` changes size, the shim/daemon cross-check is the thing that catches a
  half-shipped pair.
- `icd_shim/src/scanout.rs:817-937` — the two exchange tests assert a 24-byte reply
  (`ScanoutAllocResp::LEN`) and `stride: 5120`; a modifier word in the reply changes both.
- `modules/gndrm-android` has no test for `allocate_buffer` (it needs a device), so the
  usage-bit change is *untested* on the host by construction — which is the real risk in
  component 1.

### What does *not* break

`docs/02:73-76`'s capability table, the `ADDFB2` acceptance path, the flip-event contract,
`libliftoff` itself, and gamescope's Vulkan dmabuf import — all already modifier-neutral or
already permissive.

---

## 4. The cheapest experiment, and the decision rule

### 4.1 The measurement that decides it, in one env var

Turnip has a debug switch that disables UBWC for **every image it creates**:

- `mesa src/freedreno/vulkan/tu_util.cc:27` — `{ "noubwc", TU_DEBUG_NOUBWC }`
- `mesa src/freedreno/vulkan/tu_image.cc:738-740` — `if (TU_DEBUG(NOUBWC)) image->ubwc_enabled = false;`

So: run **Hades II in the in-app X11 arm** (where DXVK really does get UBWC render targets —
`docs/24:171-176`) with and without `TU_DEBUG=noubwc` in the container's env vars, same save,
same 210 s window, uncapped at 1280x720, and compare fps / gpubusy / cpu_sys. Container env
reaches the guest (`docs/24:130-133`), so this is **zero code, no rebuild, two sessions**.

That number is the *entire* value of UBWC on this workload on this GPU. It is an upper bound
on item 7, because item 7 also has to pay for the extra passes the gamescope path adds.

Confound to state up front: `noubwc` also affects textures and the depth buffer, so it
measures "UBWC everywhere", not "UBWC on the swapchain only". That is fine — it is an upper
bound, and an upper bound that fails to move fps closes the item.

### 4.2 The supporting measurement, in the existing harness

`docs/24`'s own falsifier for cause D is "GPU-side counters per arm at equal fps". Be honest
about what that can be on this device without root:

| counter | available to a non-privileged app | site |
|---|---|---|
| `gpubusy`, `devfreq/cur_freq` | yes | already sampled at 1 Hz (`docs/24:21`) |
| package power (USB − battery) | yes | already sampled (`docs/24:22-28`) |
| Adreno performance counters (SP/TP/DRAM busy) | **no** — `/sys/kernel/debug/kgsl` needs root; there is no non-root DRAM-throughput node on this platform | — |
| HWC/SF-side frame stats | yes, but they describe composition, not guest bandwidth | `dumpsys SurfaceFlinger` |

So "a GPU-counter read per arm" reduces to gpubusy + clock + power, which is what §2 of
`docs/24` already reports: **gpubusy 83.8 % → 84.5 %, clock 760 → 759 MHz, power +2.1 %.**
If linear scan-out cost real bandwidth on a bandwidth-limited GPU, the gamescope arm would
show a *higher* busy fraction and a *lower* clock at 1.6 % fewer fps. It shows +0.7 pts and
−1 MHz. That is the measurement, already taken.

### 4.3 The second cheap experiment: pin fps and see whether the delta survives

Set a frame cap at 60 (GPU ~30 % busy) and repeat both arms in the same scene, then at 120,
then uncapped. No code; gamescope's limiter and the container's fps setting.

- **Delta roughly constant across the three points** → the term is per-frame fixed cost (WSI
  machinery, causes A/B/C). Bandwidth contributes ~nothing. **Close item 7.**
- **Delta falls with GPU utilisation** → the term scales with GPU load, i.e. it *is* the
  bandwidth/stall coupling. Then, and only then, §4.1's number sizes the prize.

### 4.4 What result would justify the work, and what closes it

Justify:
- `TU_DEBUG=noubwc` costs **≥ 4 % fps** at 1280x720 uncapped in the X11 arm (i.e. UBWC is
  worth ≥ 4 % of frame time on this title/GPU), **and**
- the delta in §4.3 grows by **≥ 0.7 points** between the 60-fps and uncapped points, **and**
- items 2–4 have landed and the residual gamescope↔X11 gap is still **≥ 1.0 point** of cpu_sys.

Close:
- `TU_DEBUG=noubwc` costs **< 1.5 % fps** at 720p (then compression cannot buy even 0.5 points
  of CPU, and the item is dead regardless of how clean the implementation is), **or**
- the §4.3 delta is flat with GPU utilisation, **or**
- the post-item-2/3/4 residual is under ~0.5 points (nothing left for this item to win).

### 4.5 Why the arithmetic points at "close", not "justify"

1280×720×4 = 3 686 400 B. At the measured 339 fps that is **1.25 GB/s per full-frame pass**;
a plausible frame is four passes (guest write, gamescope read, gamescope write, SF/HWC read)
= **~5.0 GB/s** against a **68.3 GB/s** LPDDR5X peak (8533 MT/s × 4 × 16 bit / 8). A 1.6:1
compression of the guest's render target saves ~9 % of *frame* traffic — about **0.5 GB/s, or
0.7 % of peak DRAM**, on a GPU whose busy fraction moves 0.7 points between arms. At 1280 px
the pool's gralloc stride is already tight (5120 B = 1280×4), so there is not even row
padding to reclaim: the only variable is pixel compression, which is exactly what §4.1
measures.

---

## 5. Verdict

**Do this smaller thing instead, in this order.**

1. **Spend one session on §4.1** (`TU_DEBUG=noubwc`, X11 arm). It costs nothing, it is the
   only measurement that can justify item 7 at all, and it answers "does Adreno's lossless
   compression matter for Hades II at 720p" for every future discussion, not just this item.
2. **If it clears (>~1.5 % fps), resurrect `e14c2a7` rather than build §3.** That commit is
   the cheap form of this item and deliberately avoids the two structural walls: it keeps
   the fd path (no second fd on the wire), it negotiates with the compositor
   (`zwp_linux_dmabuf_v1.modifier` listener + `compositor_accepts_compression`), and it
   prefers the AHB route because that is the only one that can *report* a layout
   (`vkGetAndroidHardwareBufferPropertiesANDROID`'s `drmFormatModifier`). 6 files, +219/−25,
   no daemon change, no gamescope change. It needs the device verification its own message
   asks for — one log line, `proxy slot: pool layout modifier 0x… (linear|compressed/tiled)`
   — and if that line says `linear`, the item is over on this gralloc and nobody writes any
   of §3.
3. **Do not build the KMS modifier path** (`IN_FORMATS` + `libliftoff` + A51 + `ScanoutAllocResp`
   + a second fd) unless (a) step 2 shows a compressed pool buffer is achievable on this
   gralloc *and* (b) items 2–4 leave ≥ 1.0 point on the table. On the measured numbers
   (+1.5 points total, sys-bound, gpubusy flat to 0.7 pts) the whole item's ceiling is well
   under one point of CPU, and the KMS path costs the golden blob test, a wire-struct change
   that both halves of a shipped pair must agree on, the presenter's foreign-copy fallback,
   gamescope's A51 block, and libliftoff's willingness to place the plane.

The ranking in `docs/24:243-247` is right, and it is right for a reason the document states
but does not lean on: **the delta is sys time, and sys time is causes A/B/C.** Until B is
fixed there is no mechanism by which item 7's bandwidth term can reach the CPU counter at
all; after B is fixed the term has nowhere to go but fps, where it is bounded by the number
§4.1 measures.

---

## 6. Prior art in the tree (read this before writing code)

`git log --all --grep=compressed` on `gamenative-drm`:

- **`e14c2a7` "scan-out: the pool's layout is the compositor's call, and the buffer's own
  fact"** (2026-11-07, present only on `backup-image-count-20261107-0904`, one commit behind
  that branch's tip; **not** an ancestor of `performance-report`).
  - `icd_shim/src/{export,proxy,scanout}.rs`, `modules/gndrm-android/src/{asc,ffi}.rs`,
    `modules/gndrm-uapi/src/wire.rs` — 6 files, +219/−25.
  - Its own measurement of the starting point: *"a 720p one came back at exactly `w * h * 4`
    with no metadata plane, so the shim imported it as `VK_IMAGE_TILING_LINEAR` and every
    frame ran without Adreno's lossless bandwidth compression."*
  - Its mechanism is the one §5 recommends: consumer decides → `SCANOUT_REQ_COMPRESSIBLE` →
    pool drops CPU-visible usage → import follows the buffer (`AhbFormatProperties` grows the
    layout words) → `params_add` carries the real modifier.
  - Verified only to *build* (clippy, 114+99+36+13+8+2+20 tests, `build-android.sh`,
    `check-exports`). The device line it asks for is unrecorded. That is the gap.
- `8a4e3df` (ancestor of HEAD) — *"the daemon stops refusing an ADDFB2 whose modifier the
  primary plane did not advertise"* and the compressed-length index. This is the residue of
  the same exploration: compression has been seen arriving through gamescope's direct
  scan-out, and the daemon was taught to *tolerate* it without ever being taught to
  *produce* it.
- `87dfe10` (branch tip) — diagnostics only, unrelated.

The absence of `e14c2a7` from `performance-report` is not evidence that it failed on device;
it is evidence that it was never measured, then overtaken by the image-count and pitch work
(`#32`, `#59`) that landed on top of `slot_init`'s LINEAR contract. Anyone re-opening item 7
should read that diff first — most of the code is written, and its negotiation seam is the
only part that is not already in HEAD.
