# 06 — Presenter: SurfaceControl publish

The presenter publishes committed scan-out buffers to Android. It mirrors the
winlator X11 path's `ASurfaceRenderer` shape: gralloc buffers plus hardware
fences handed straight to SurfaceFlinger, which composites (or scans out) in
hardware — zero-copy, no intermediate buffer queue, and no format conversion.

## Scan-out allocation

The daemon allocates and owns the scan-out `AHardwareBuffer` pool (HAL format
`RGBA_8888`, value 1, usage `GPU_SAMPLED_IMAGE | GPU_COLOR_OUTPUT |
COMPOSER_OVERLAY`). When gamescope's renderer asks for a scan-out buffer, the
shim requests a pool buffer by geometry over the DRM socket and receives a
dup'd dma-buf fd via `SCM_RIGHTS`; gamescope renders into that fd, and the
*same* `AHardwareBuffer` is published. The daemon owns the buffer end to end —
no import, no copy, and no cross-process manufacture of an `AHardwareBuffer`
from a bare fd.

## Format identity

The pool is RGBA (`RGBA_8888`), which is byte-for-byte identical to DRM
`XBGR8888`/`ABGR8888` (`XB24`/`AB24`), so the frames gamescope renders need no
conversion and no shader. The primary plane advertises exactly the formats the
pool is laid out in, so gamescope cannot elect a layout the pool does not have.
`XRGB8888`/`ARGB8888` are offered on the cursor plane only, so they stay
sampleable client formats that gamescope composites rather than scans out.

## Alpha semantics (scan-out alpha is not meaningful)

The pool's `AHardwareBuffer` is `RGBA_8888`, whose alpha channel SurfaceFlinger
**honours** by default. The frame gamescope renders is written for a DRM primary
plane, whose X-prefixed fourcc (`XBGR8888`/`XB24`) carries no alpha: a shader
pass may store alpha **0** without blanking the plane. gamescope's RCAS composite
shaders do exactly that — `imageStore(dst, …, vec4(outputValue, 0))` in
`cs_composite_rcas.comp` / `cs_composite_rcas_1px.comp:94`. Storing 0 is
**correct** for scan-out, because DRM ignores alpha; it is only wrong once the
same bytes are published through SurfaceFlinger, which does not ignore it.

Invariant: **a scan-out format that carries no meaningful alpha must not be
published as transparent.** The publisher, not the producer, owns this. The
presenter asserts `ASURFACE_TRANSACTION_TRANSPARENCY_OPAQUE` on every show
transaction (`modules/gndrm-android/src/asc.rs` `build_show`; constant and
rationale in `ffi.rs`), so the layer composites opaque no matter what alpha the
scan-out pass left behind. The scan-out format table is not the fix and needs no
change: the daemon's `PRIMARY_FORMATS` already lists `XBGR8888` first (an
X-prefixed fourcc, alpha ignored).

Without the assertion the primary layer is composited with the buffer's alpha,
and a full-frame `alpha = 0` renders the whole frame transparent — black. The
tell is which upscaler measured black: the FSR and SGSR paths go through the RCAS
composite that writes alpha 0, while NIS, nearest and pixel do not. The
falsifying measurements are a frame's *content*, not the publish path: a rendered
frame's screenshot is ≈2.6–2.8 MB with mean luma ~54–58 and >300k distinct
colours; a black frame is ≈37–41 KB with mean luma ~1.2 and ~1.4k colours. The
render side is therefore ruled out — FSR/SGSR arithmetic, the RCAS pass
configuration, the shader body, the descriptor layout, pipeline creation — and so
is the ICD shim, which logs command *names*, not arguments, and so cannot see
formats or `VkResult`s.

## Publish tail

Presentation uses `ASurfaceControl` / `ASurfaceTransaction`, resolved at runtime
by `dlopen("libandroid.so")` — the NDK gates these symbols at API 29 and the
build targets API 28, so they cannot be linked statically. One child
`SurfaceControl` is created from the window (`ASurfaceControl_createFromWindow`),
and each presented buffer is published with
`ASurfaceTransaction_setBuffer` + `setGeometry` + `apply`.

## Back-pressure and warm spare

- Back-pressure comes from
  `ASurfaceTransactionStats_getPreviousReleaseFenceFd`: a slot is not reused
  while SurfaceFlinger still holds its buffer.
- A warm spare keeps one pre-allocated pool buffer ready so a commit is never
  stalled waiting on allocation.

## Locks

The scan-out pool and the presenter state each have their own lock. No lock is
held across a gralloc or SurfaceFlinger call, so a slow SurfaceFlinger cannot
stall the rest of the daemon.
