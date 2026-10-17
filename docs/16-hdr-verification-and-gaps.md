# 16 — HDR: the chain, the scaler composite's alpha, and what is left

HDR output works end to end on the Odin 3 with The Witcher 3: the panel is driven
in BT.2020/PQ, the scan-out is 10-bit, and every filter now presents. One bug was
found and fixed — **the scalers' composite stored alpha 0, so the published layer
composited away** (§5). One is open: **the blit store is over-exposed** while
every RCAS filter is correct (§6, P4).

This document records what the device established (§1), the symptom and the
reading it defeated (§2), the instruments this work falsified (§3), the chain
(§4), the cause and its fix (§5), the mechanisms eliminated on the way (§5a), the
plan (§6), the invariants (§7), and what is open (§8).

## 1. What the device established

Witcher 3, Odin 3 (`3a59aa7b`), HDR Output on, payload `20261017g` (with the fix)
against `20261016f` (before it). Each link verified by its own observation:

| Link | Evidence | Result |
|---|---|---|
| panel capability | `GraphicsTab.kt:128-139` | HDR10 + HLG, 420 cd/m² |
| launch flags | gamescope's command line | `--hdr-enabled -r 1200` |
| connector gate | gamescope | `HDR output enabled`, no `(hdr_content_driven)` |
| mode | gamescope | `selecting mode 1920x1080@120Hz` |
| scan-out format | `asc hdr decision` | `fmt=0x30334258`, `buffer10=true` |
| published layer | `dumpsys SurfaceFlinger` | `RGBA_1010102`, dataspace `0x09c60000` |
| buffer alpha | `asc hdr decision` | **`a=3`** after the fix, `a=0` before, under FSR and SGSR |

`conn_hdr=true` in every arm, failing ones included, so the connector's HDR state
cannot discriminate a failure and must not gate the publish. Format and dataspace
are identical in working and failing arms: the difference was the publish's
alpha, and then the composite store's exposure (§6).

**HDR off is the working control**: without HDR every filter works, which puts
each of these defects on a path taken only in an HDR session.

## 2. The symptom, and the reading it defeated

Before the fix, FSR and SGSR showed a flat grey field while LINEAR, NEAREST,
INTEGER and NIS showed the game. The field measured 170/171 across ~90 % of the
game area with a fixed 1-bit lattice, and the game area changed **0 pixels**
between captures while the HUD strip changed 1.04 %.

That evidence is genuinely ambiguous, and the first reading of it — "gamescope
composites a constant image" — was wrong. Every counter said the composite was
running: `n` equalled the commit count 1:1 at 40–48/s in all three arms, the
published inode cycled three pool buffers, and SurfaceFlinger walked the same
three handles at 41/s. What no counter can see is whether the frame is *visible*,
and it was not: the buffer held a correct image carrying alpha 0.

The counters constrain production, never presentation. That is the lesson of the
whole round.

## 3. Instruments this work falsified

**Frame statistics, as a pass/fail test.** Two LINEAR arms on the same build
disagreed: 175,618 distinct colours with the dominant code at 20.7 %, versus
24,789 at **87 %**. `screencap` maps a BT2020/PQ layer through Android's
tone-mapping into an 8-bit bitmap, so absolute codes are instrument-mediated.
The verdict that A35's inverse tone mapping "washed" LINEAR is withdrawn with it.

**A content hash, as a change detector.** FNV over 8-bit-masked words reported
"changing" for a constant field, twice: RCAS dithers ±2 and those bits straddle
the 8-bit boundary (a constant 683 renders 683–686, i.e. 170/171). Dither-proof
quantisation needs 6 bits per channel; the hash was removed rather than tuned.

**A `texelFetch` copy probe, as a sampling test.** `texelFetch` is unnormalized by
construction: on a 2× upscale (960×540 → 1920×1080) it fills exactly one quadrant,
which reads as a layout bug and is not one.

**The gralloc dataspace warning, as a lead.** `gatherBufferInfo: Received unknown
dataspace from gralloc` runs at 57.30/s under LINEAR, steady across fifteen
ten-second buckets, against 51.5/s before a filter switch and ~53/s under FSR and
SGSR. Uncorrelated. Still a real defect: the layer declares `0x09c60000` while the
gralloc buffers behind it report `0x0` — the transaction's dataspace is set
(`asc.rs:2354`), the buffer's never is.

## 4. The chain, for reference

**HDR is a session-level decision here, not content-driven.** The emulated
connector's EDID carries PNP `GNX`, product 1 (`gndrm-daemon/src/edid.rs:46-51`),
matching no entry in gamescope's known-display table, so `bContentDrivenHDR` stays
false and `bOutputHDRActive = bOutputHDRCapable`
(`src/steamcompmgr.cpp:10380-10407`). Once `--hdr-enabled` is on and the connector
looks HDR, the panel is driven in PQ for the whole session — desktop and boot
splash included — and an SDR title reaches it through inverse tone mapping
(`src/shaders/composite.h:99-103`).

**The gate** is `CDRMConnector::SupportsHDR10()`
(`src/Backends/DRMBackend.cpp:433-436`):

```
!!GetProperties().Colorspace && !!GetProperties().HDR_OUTPUT_METADATA && GetHDRInfo().IsHDR10()
```

with `IsHDR10()` = `bExposeHDRSupport && eOutputEncodingEOTF == EOTF_PQ`
(`src/backend.h:147-152`). The properties exist (`gndrm-daemon/src/props.rs:92-96`);
the EDID content decides (`DRMBackend.cpp:2839-2846`, CTA Colorimetry `bt2020_rgb`
plus HDR Static Metadata with the PQ bit, built at `edid.rs:70-118`, appended only
when the session capability is `Some`); `cv_hdr_enabled` comes from
`--hdr-enabled` (`steamcompmgr.cpp:10005`); activation is
`bOutputHDRCapable = (g_bSupportsHDR_CachedValue || g_bForceHDR10OutputDebug) && cv_hdr_enabled`
(`:10380`), logged at `:10406`. The commit adds `bWantsHDR10`
(`DRMBackend.cpp:3271`) with `SupportsHDR10()` (`:3273`), writing
`DRM_MODE_COLORIMETRY_BT2020_RGB` (`:3282`) and the metadata blob (`:3438-3439`).

**Plane formats** (`gndrm-daemon/src/lib.rs:62-108`): the SDR set with `XRGB8888`
first, plus `XBGR2101010` only when `hdr_output_enabled()`. `XRGB2101010` must stay
absent — its channel order is the opposite of the pool's `R10G10B10A2` layout
(`:80-92`). The 10-bit entry cannot sit in a flat list, or an SDR session moves
onto a 10-bit buffer (`:73-85`). `pick_plane_format`
(`DRMBackend.cpp:721-745`) is chosen once at backend init (`:1676`).

**The publish** (`modules/gndrm-android/src/asc.rs`): the pool serves
`R10G10B10A2_UNORM` when `hdr_output_enabled()` (`:2198-2204`); the dataspace is
`ADATASPACE_BT2020_PQ` when the committed fourcc is 2:10:10:10 (`:1054-1061`,
`:2354`); the transaction asserts `ASURFACE_TRANSACTION_TRANSPARENCY_OPAQUE`
unconditionally (`:2451`) — necessary but **not sufficient** for a 10-bit layer,
because SurfaceFlinger honours that layer's own alpha regardless (§5). Publishing
is one `build_show` call on the single `gndrm-scanout` layer (`:2421-2466`);
`build_hide` is the other transaction.

**The probes.** `commit #N fb= prev_fb= crtc_on= fbs=`
(`gndrm-daemon/src/commit.rs:169-201`, first 30 then every 120th, ungated) and

```
asc hdr decision n= fb= ino= a= c= p= fmt= buffer10= conn_hdr= dataspace=
```

(`asc.rs:1093-1117`, on a decision change and every 121st publish — 121 is coprime
with the three-buffer pool, so the published inode is seen cycling).

## 5. The cause and the fix

**Both RCAS composite variants stored alpha 0 into the scan-out buffer.**
`cs_composite_rcas.comp:94` and `cs_composite_rcas_1px.comp:94` ended with
`imageStore(dst, rotateOutputCoord(pos, u_rotation), vec4(outputValue, 0))`. On a
DRM scan-out plane that alpha is not meaningful — the fourcc is X-prefixed — but
the same buffer is published as an Android layer, and a 10-bit layer's alpha *is*
honoured there: the frame composited away and the panel showed what was behind
the layer.

That explains the split exactly. The filters that end at RCAS (FSR, SGSR) lost
their alpha; the ones that end at the blit store (LINEAR, NEAREST, INTEGER, NIS)
kept the layer's. It also explains why the same session with HDR off was fine —
the 8-bit pool buffer is not a 10-bit layer.

Measured from the published word's top two bits, same session, same scene:

| Arm | `20261016f` | `20261017g` |
|---|---|---|
| LINEAR | `a=3` | `a=3` |
| FSR | `a=0` | **`a=3`** |
| SGSR | `a=0` | **`a=3`** |

**Fix**: store `vec4(outputValue, 1)` in both variants, as
`android/patches/gamescope/0036-A36-composite-output-opaque.patch` (gamescope
`36759122`). Done at the source rather than by another presenter workaround: a
scan-out plane's alpha is not meaningful data, and every consumer should see it
that way. The presenter's opacity assertion stays — it is what covers the 8-bit
layer.

## 5a. Mechanisms eliminated on the way

Each is a real difference between the paths; none was the cause. Recorded so they
are not re-tested.

1. **The pre-pass writes a constant** — refuted. With the pre-pass writing a
   normalized 2× copy, the published buffer decodes as a full image (10-bit fields
   span 0..581 of 1023 across 146 levels).
2. **RCAS discards the pre-pass** — refuted. `layer0 offset=(0.00,0.00)`,
   `tmp=1920x1080`, `out=1920x1080`, `layers=1`: the guard passes for every pixel.
   The offset is negative only on overflow, and this title runs a clean 2× upscale
   from 960×540.
3. **The daemon stops publishing, or publishes the wrong buffer** — refuted.
   Publishes track commits 1:1 at 40–48/s, the inode cycles three pool buffers,
   and SurfaceFlinger cycles three handles at 41/s.
4. **The composite output never reaches the panel** — refuted. With RCAS storing
   solid red, both the published buffer and the panel were red.

## 6. Plan

### P4 — the blit store's exposure (open, next)

After the alpha fix the same session reports the inverse split on *exposure*:

| Arm | composite store | exposure |
|---|---|---|
| LINEAR, NEAREST, INTEGER, NIS | blit | **over-exposed** — sky and snow clip, 4.5 % of pixels at 250+ |
| FSR, SGSR | RCAS | normal |

Both stores call the same `apply_layer_color_mgmt` (`composite.h:99-131`) and
`encodeOutputColor` (`:207`); what differs is what they are fed. The blit path
runs the chain on the layer's own 10-bit PQ texture through
`colorspace_plane_degamma_tf` inside `sampleLayerEx` (`:198`); RCAS runs it on the
8-bit pre-pass image. One of them is being colour-managed in the wrong transfer
function, and the 8-bit hop may be correcting it by accident.

Cheapest first observation: change only the LUT's SDR target
(`--hdr-sdr-content-nits`) and see whether both paths move together. If they do,
the term is shared and the difference is upstream of the LUT; if only the blit
path moves, the chains are genuinely different and the blit path's transfer
function is what to trace.

The device's shader selection matters for any patch here: `SHADER(EASU,
cs_easu_fp16)` (`rendervulkan.cpp:976-984`) and `SHADER(RCAS,
cs_composite_rcas_1px)` for vendor `0x5143` (`:966-975`) — the fp16 and 1px
variants are the ones that run on Adreno.

### P5 — the composite target's storage qualifier (latent)

`layout(binding = 1, rgba8) writeonly uniform image2D dst` (`descriptor_set.h:48`)
declares an 8-bit storage image while the HDR target is `A2B10G10R10`. The alpha
fix proves writes land, but the qualifier misdescribes the image on every path
and must be corrected.

### Harness requirements

- **Capture from before launch.** The logcat ring is 256 KiB ≈ 2.5 minutes; the
  launch flags, `HDR output enabled` and the first `asc hdr decision` all arrive
  before a post-launch capture begins.
- **A filter arm is valid only after the QuickMenu is closed and the picture is
  proven live** by a frame delta; the QuickMenu suspends the session.
- **Do not trust a bare protocol number.** "Unknown dataspace from gralloc" ran at
  ~55/s in every arm, and `screencap` statistics cannot separate a dithered
  constant from content. Each was believed once and cost a round.

## 7. Invariants that must not regress

1. **Truthful EDID.** The CTA Colorimetry and HDR Static Metadata blocks appear
   only when the panel reported HDR types and the launcher asked for HDR
   (`edid.rs:70-75`, `lib.rs:426`).
2. **All three properties stay exposed** (`props.rs:92-96`); dropping one makes
   `SupportsHDR10()` false and silently returns the path to SDR.
3. **`Colorspace` keeps `BT2020_RGB` in its enum and accepts the write**
   (`props.rs:46-66`, `commit.rs:538-543`).
4. **One flag, three consequences** — plane-format advertisement
   (`lib.rs:86-108`), pool AHB format (`asc.rs:2198-2204`) and published dataspace
   (`asc.rs:1054-1061`) follow `hdr_output_enabled()` together.
5. **`XRGB2101010` stays out of the plane list** (`lib.rs:80-92`).
6. **The composite output is opaque** — `vec4(outputValue, 1)` in both RCAS
   variants (gamescope `0036`). The present path must never rely on the
   transparency hint alone for a 10-bit layer.
7. **The publish asserts opacity unconditionally** (`asc.rs:2451`).
8. **HDR stays a launch-time decision** (`DRMBackend.cpp:1676`; the daemon's
   per-session cache `lib.rs:110-138`, dropped at
   `modules/gndrm-android/src/lib.rs:705`).
9. **The SDR path is unchanged** — no 10-bit plane format, no CTA extension, no PQ
   dataspace, no `R10G10B10A2` pool buffer.
10. **The connector's HDR state is not a publish gate** — `conn_hdr=true` in every
    arm measured, including working ones.

## 8. Also open, not blocking

- **Inverse tone mapping** — `--hdr-itm-enabled` is gone from the launch line and
  A35's patch is deleted. Upstream's shader declares `c_itm_enable` at
  `constant_id 7` while the host's specialization map supplies ids 0..6, so the
  flag could never have reached `composite.h:99-103`. Real ITM would need that
  constant id corrected first, and it is for SDR input only (`src/main.cpp:216`).
- **Audited luminances.** The panel reports ~420 nits through
  `Display.HdrCapabilities`, and the EDID carries that; the panel's own peak is
  reported as 650. Raising it moves the LUT's tonemap target and is a
  one-variable change to make on a known-good picture, not during a bug hunt.
- **The gralloc buffer dataspace (§3).**
- **`--hdr-sdr-content-nits`** defaults to 400; live-tunable through the
  `gamescopeSDROnHDRContentBrightness` atom.
- **`ASurfaceTransaction_setHdrMetadata_cta861_3`** is never called, so the panel
  is not asked to enter its HDR mode.
- **The app logs `applied 'scaling_sharpness 0'` while the UI shows Sharpness 5**
  — the value does not reach gamescope.
- **The `(Beta)` suffix** (`strings.xml:979`) stands until HDR is correct.
- **`docs/02-gamenative-drm.md:111-115`** still lists `Colorspace`,
  `HDR_OUTPUT_METADATA` and `EDID` as absent while `props.rs:92-96` exposes all
  three; **`docs/README.md:33-46`** lists documents only up to 14.
