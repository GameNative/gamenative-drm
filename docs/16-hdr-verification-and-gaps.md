# 16 — HDR: the chain, the guest's swapchain contract, and the open defect

HDR output reaches the panel on the Odin 3: gamescope is launched with
`--hdr-enabled`, the daemon scans out a 10-bit buffer tagged BT.2020/PQ, and
SurfaceFlinger receives a layer with dataspace `0x09c60000`. What is **not**
settled is a D3D12 title presenting *into* an HDR session: the moment the shim
advertises a PQ surface format, The Witcher 3's present task stops creating
swapchains and the picture never returns (§4).

This document states the three decisions that make an HDR session (§1), the
chain (§2), the contract the guest's swapchain imposes on the shim (§3), the
open defect with the controls that localise it (§4), the instruments this work
falsified (§5), the invariants (§6) and what remains open (§7).

## 1. Three decisions, three components

HDR here is a launch-time property of the session, not a property of the
content:

| Decision | Made by | Consequence |
|---|---|---|
| the session is HDR | the launcher (`hdrEnabled` in the container) | `--hdr-enabled` on gamescope's command line, `GNDRM_HDR=1` and `DXVK_HDR=1` in the guest environment |
| the connector is HDR-capable | the daemon's EDID + `HDR_OUTPUT_METADATA` + `Colorspace` | gamescope's `SupportsHDR10()` gate opens and it drives the panel in PQ for the whole session |
| the guest may present HDR | the shim's `served_surface_formats()` | the colour spaces the guest's swapchain may ask for |

The third is the one that is wrong today. The first two are already correct and
measured: the EDID carries CTA Colorimetry `bt2020_rgb` plus the PQ static
metadata block, and gamescope logs `HDR output enabled`.

`GNDRM_HDR` cannot be derived inside either process: gamescope asks the
connector and the plane at startup, before any guest exists, so the answer has to
be known when the launcher builds the command line. It is injected into the
guest's `envp` (`BionicProgramLauncherComponent.setGndrmHdrEnv`) rather than into
the app's own environment, because the shim and DXVK run in the guest.

## 2. The chain

```
game (vkd3d / DXVK)
  → shim: surface formats, colour space, swapchain, present
  → gamescope (--backend drm --hdr-enabled): composite, colour management
  → daemon: scan-out buffer, dataspace, CTA-861.3 + SMPTE-2086 metadata
  → SurfaceFlinger → panel
```

**Connector gate.** `CDRMConnector::SupportsHDR10()`
(`src/Backends/DRMBackend.cpp:433-436`) requires `Colorspace`,
`HDR_OUTPUT_METADATA` and `GetHDRInfo().IsHDR10()`. All three properties are
exposed (`gndrm-daemon/src/props.rs:92-96`); dropping any one silently returns
the session to SDR. Activation is `bOutputHDRCapable = g_bSupportsHDR_CachedValue
&& cv_hdr_enabled` (`src/steamcompmgr.cpp:10380`). Because the emulated
connector's PNP id matches no entry in gamescope's known-display table,
`bContentDrivenHDR` stays false and the session is HDR for every client,
desktop and boot splash included.

**Commit.** `bWantsHDR10` (`DRMBackend.cpp:3271`) writes
`DRM_MODE_COLORIMETRY_BT2020_RGB` (`:3282`) and a metadata blob (`:3438`). On the
SDR path the same line writes the connector's SDR static-metadata blob — that is
why the connector's metalspace cannot be used as an "HDR is on" test (§5).

**Plane formats** (`gndrm-daemon/src/lib.rs:62-108`): the SDR set with
`XRGB8888` first, plus `XBGR2101010` only when `hdr_output_enabled()`.
`XRGB2101010` must stay absent — its channel order is the reverse of the pool's
`R10G10B10A2` layout. The 10-bit entry cannot sit in a flat list, or an SDR
session moves onto a 10-bit buffer; `pick_plane_format` runs once at backend
init.

**Publish** (`modules/gndrm-android/src/asc.rs`): the pool serves
`R10G10B10A2_UNORM` when `hdr_output_enabled()`; the transaction dataspace is
`ADATASPACE_BT2020_PQ` when the committed fourcc is 2:10:10:10; the layer is
declared opaque unconditionally, and the composite output must be opaque too —
a 10-bit Android layer honours alpha, so storing `vec4(x, 0)` composited the
frame away (fixed in gamescope `0036-A36`, gamescope `36759122`).

**Probes.** `commit #N fb= prev_fb= crtc_on= fbs=` (first 30, then every 120th,
ungated) and

```
asc hdr decision n= fb= ino= a= c= p= fmt= buffer10= conn_hdr= dataspace=
```

on a decision change and every 121st publish (coprime with the three-buffer
pool, so the published inode is seen cycling). The shim adds `proxy hb:` once a
second with cumulative counters for every WSI entry point, which is the only
instrument that names the call a client is looping on.

## 3. The guest's swapchain contract

This is the part the shim must satisfy, and it is stricter than the Vulkan
specification allows for:

- **vkd3d-proton matches the exact pair.** `dxgi_vk_swap_chain_select_format`
  (`libs/vkd3d/swapchain.c`) requires one entry where both the format and the
  colour space equal the request. There is a wildcard — `VK_FORMAT_UNDEFINED`
  accepts `R8G8B8A8_UNORM`, `B8G8R8A8_UNORM` and `A8B8G8R8_UNORM_PACK32` — but
  **only when the colour space is sRGB**. For any other colour space the
  function returns false with the comment *"Refuse to present unsupported HDR
  since it will look completely bogus"*, and
  `dxgi_vk_swap_chain_recreate_swapchain_in_present_task` then returns **without
  calling `vkCreateSwapchainKHR` at all**.
- **`supports_color_space` is permissive.** It returns true as soon as *any*
  entry carries the colour space. A client therefore learns "HDR10 is
  available" from an entry it will never be able to pair with its own format.
- **The list must contain exactly what the create path can present.** An entry
  the create path refuses is worse than a missing one: the client takes the
  offer, the create fails, and it retries forever. Measured — advertising
  `R8G8B8A8_UNORM` without the RGBA opt-in made DXVK loop 5150 times on an
  in-game HDR toggle. The served set is therefore only the pool's
  `B8G8R8A8_UNORM` and `A2B10G10R10_UNORM_PACK32` in an HDR session, each with
  both colour spaces, plus `R8G8B8A8_UNORM` when `GN_VKSHIM_ALLOW_RGBA` makes
  the create path accept it.
- **Order decides what a "first match" client takes.** DXVK takes the first
  entry carrying a colour space, so the 10-bit PQ pair is listed before the
  8-bit one.

FP16 is absent for the same reason, and the measurement is direct: when a
`R16G16B16A16_SFLOAT` pair is offered, DXVK takes it (`fmt=97
cs=1000104008`), the create is refused because the scan-out pool cannot serve
16 bits, and the client loops on that refusal instead of falling back. Offering
a format the scan-out path cannot honour is not a larger catalogue; it is a
lost picture.

**Measured end to end (DXVK, Ori, HDR on):** the guest creates
`fmt=58 cs=1000104008` — 10-bit PQ — the daemon publishes `fmt=0x30334258`
(XB30) with dataspace `0x09c60000`, alpha `a=3`, at 60 fps, and the picture is
correct: no banding, no channel swap. The same title with an 8-bit PQ pair as
its only HDR entry also presents at 60 fps but the image is shredded into
cyan/magenta bands, because PQ needs the 10-bit depth — which is why the 10-bit
pair must precede the 8-bit one in the list.

The shim also refuses formats the scan-out pool cannot serve
(`swapchain_format_allowed`): `B8G8R8A8_UNORM`, `R8G8B8A8_UNORM` under the RGBA
opt-in, and `A2B10G10R10_UNORM_PACK32` in an HDR session. A refusal is logged
with the format number rather than rendered with swapped channels.

## 4. The open defect

**Symptom.** With the container's HDR on, The Witcher 3 (DX12, `fmt=44`) creates
two swapchains at 960×540 and presents twice. From then on, every frame runs a
swapchain recreate: the shim's `fmt` and `caps` counters climb at 120/s and
180/s, `create`, `acquire` and `present` stay frozen at 2, and
`dxgi_vk_swap_chain_update_wait_timing_capabilities` — whose only call site is
inside the recreate — logs at 60/s. The picture never appears.

**The controls that localise it.** Each is a single launch, everything else held
constant:

| Arm | guest format list | result |
|---|---|---|
| container HDR **off** | SDR | 60 fps, `acq`/`present` climbing, `fmt=12` total |
| container HDR on, `GNDRM_HDR=0` in the guest | SDR | 60 fps, `acq=742` |
| container HDR on | 18 pairs, sRGB + PQ + scRGB | stalls at `pres=2` |
| container HDR on | 12 pairs, sRGB + PQ | stalls at `pres=2` |

So the stall is caused by the shim's format list alone: the daemon's 10-bit pool,
`--hdr-enabled`, and the guest's `DXVK_HDR` are all present in the working
`GNDRM_HDR=0` arm. scRGB is not the trigger; PQ is.

**What the recreate rules out.** In vkd3d-proton 3.0.1 the recreate has four
exits before `vkCreateSwapchainKHR`, and the once-a-second trace proves the
first two were passed: the format list is re-queried successfully and the
update-wait-timing call that follows it fires at 60/s. The remaining exits are
the occlusion test on `maxImageExtent` and `select_format`. The surface reports
`current=960x540`, `min=1x1`, `max=8192x8192` throughout, so occlusion is false.

`select_format` was then made unfalsifiable by construction: a list of sixteen
pairs — eight formats (`A2B10G10R10_UNORM_PACK32`, `B8G8R8A8_UNORM`,
`B8G8R8A8_SRGB`, `R8G8B8A8_UNORM`, `R8G8B8A8_SRGB`, `A8B8G8R8_UNORM_PACK32`,
`R16G16B16A16_UNORM`, `R16G16B16A16_SFLOAT`) each with sRGB and PQ — produced no
create and no refusal. So did forcing `VKD3D_SWAPCHAIN_PRESENT_MODE=fifo`, whose
branch queries present modes: the `pmodes` counter stayed at 0, i.e. the code
path never reaches the block after `select_format`.

Both creates in the failing session are `960x540 fmt=44 cs=0` — SDR. The title
never obtains an HDR swapchain at all; the stall is its *first* HDR attempt.

That is the contradiction this document leaves open: every exit the source
allows is excluded by measurement, and the only remaining way to see the
request is from inside the shipped `d3d12core.dll`. The next instrument is a
patched build of that library printing `(dxgi_format, colour_space)` at
`dxgi_vk_swap_chain_select_format` — which needs `widl`, available on Arch only
through Wine. Six list shapes have been tried against this symptom; none moved
it.

**Consequences for the features built on it.** The guest-side HDR path is
proven for DXVK (above) and blocked for vkd3d. `--hdr-enabled` with the game's
own HDR off remains correct and safe: gamescope converts SDR to PQ and the
picture is right, so an HDR session never has to be a black screen — it is the
vkd3d *title's* HDR switch that cannot complete.

## 5. Instruments this work falsified

**`screencap` frame statistics, as a pass/fail test.** Two LINEAR arms on the
same build reported 175,618 distinct colours with the dominant code at 20.7 %,
versus 24,789 at 87 %: `screencap` tone-maps a BT2020/PQ layer through Android's
conversion into an 8-bit bitmap, so absolute codes are instrument-mediated.

**A content hash, as a change detector.** FNV over 8-bit-masked words reported
"changing" for a constant field, twice: RCAS dithers ±2 and those bits straddle
the 8-bit boundary. Dither-proof quantisation needs 6 bits per channel.

**`texelFetch`, as a sampling test.** It is unnormalized by construction: on a
2× upscale it fills exactly one quadrant, which reads as a layout bug and is not
one.

**The connector's colour state, as a publish gate.** `conn_hdr` was true in every
arm including failing ones, because gamescope writes the connector's SDR
static-metadata blob on the SDR path (`DRMBackend.cpp:3281`). The mirror now
keys on `Colorspace == BT2020_RGB` alone.

**Per-submit logging.** One line per `vkQueueSubmit2` produced 8,028 lines per
capture and evicted the 256 KiB log ring in seconds. Rate-limited, it became the
tool that made the traces readable.

**A `texelFetch`-style "copy" of the composite, and an 8-bit masked hash**, both
removed rather than tuned.

## 6. Invariants that must not regress

1. **Truthful EDID** — the CTA Colorimetry and static-metadata blocks appear only
   when the panel reported HDR types and the launcher asked for HDR.
2. **All three connector properties stay exposed**, or `SupportsHDR10()` goes
   false and the session silently returns to SDR.
3. **One flag, three consequences** — plane-format advertisement, pool AHB
   format and published dataspace follow `hdr_output_enabled()` together.
4. **`XRGB2101010` stays out of the plane list.**
5. **The composite output is opaque** — `vec4(outputValue, 1)` in both RCAS
   variants; the present path must not rely on the transparency hint for a
   10-bit layer.
- **The dataspace describes the encoding, not the bit depth** — 10-bit sRGB
  content publishes as sRGB, not PQ.
- **A 2:10:10:10 swapchain is `ARGB2101010` on the wire, not `ABGR2101010`.**
  DXGI's `R10G10B10A2` puts red in the high ten bits; a D3D client leaves that
  word in the buffer whatever VkFormat it rendered through. Declaring ABGR made
  gamescope sample red and blue exchanged — the same Ori view read (2,51,89)
  with the game's HDR on against (114,42,28) with it off. gamescope maps
  `ARGB2101010` back to `A2R10G10B10_UNORM_PACK32`, so the import then describes
  the contents.
7. **Every format is advertised with every colour space the shim serves**, and
   the `_SRGB` formats are served too (§3).
8. **The SDR path is unchanged** — no 10-bit plane format, no CTA extension, no
   PQ dataspace, no `R10G10B10A2` pool buffer.
9. **The guest's colour space comes from `VkSwapchainCreateInfoKHR.imageColorSpace`**,
   never inferred from the format: DXVK legitimately asks for the 10-bit format
   with sRGB.

## 7. Open, not blocking

- **The blit store's exposure.** After the alpha fix, LINEAR/NEAREST/INTEGER/NIS
  (blit path) over-expose while FSR/SGSR (RCAS path) are correct. Both stores
  call the same `apply_layer_color_mgmt`; what differs is the input. One
  observation decides it: change only `--hdr-sdr-content-nits` and see whether
  both paths move together.
- **The composite target's storage qualifier** — `descriptor_set.h:48` declares
  `rgba8` while the HDR target is `A2B10G10R10`. Writes land, but the qualifier
  misdescribes the image on every path.
- **FP16 scan-out.** Native scRGB HDR needs a 16-bit pool buffer, a
  `wgpu`-visible fourcc and a gamescope import path for it. Until then scRGB is
  deliberately not advertised.
- **The panel never enters its HDR mode.** `mActiveColorMode=0`,
  `supportedColorModes=[0,9]`, composition `BT2020 SMPTE 2084 → DCI-P3/BT709`
  even when mode 9 is current. Our layer is always scaled and `ROT_90`, so it is
  never HWC-overlay eligible. `ASurfaceTransaction_setHdrMetadata_cta861_3` is
  never called.
- **ITM** — `--hdr-itm-enabled` is gone and A35 is deleted: upstream declares
  `c_itm_enable` at `constant_id 7` while the host's map supplies 0..6. Real ITM
  needs that id corrected, and it is for SDR input only.
- **Audited luminances** — the panel reports ~420 nits; raising the EDID value
  moves the LUT's tonemap target. One-variable change on a known-good picture.
- **The gralloc buffer dataspace** — the layer declares `0x09c60000` while the
  buffers behind it report `0x0`.
- **`docs/02-gamenative-drm.md:111-115`** still lists `Colorspace`,
  `HDR_OUTPUT_METADATA` and `EDID` as absent; **`docs/README.md:33-46`** lists
  documents only up to 14.
