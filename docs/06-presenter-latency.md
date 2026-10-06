# 06 — Presenter: SurfaceControl publish

The presenter publishes committed scan-out buffers to Android. It mirrors the
winlator X11 path's `ASurfaceRenderer` shape: gralloc buffers plus hardware
fences handed straight to SurfaceFlinger, which composites (or scans out) in
hardware — zero-copy, no intermediate buffer queue, and no format conversion.

## Scan-out allocation

The daemon allocates and owns the scan-out `AHardwareBuffer` pool (HAL format
`BGRA_8888`, value 5, usage `GPU_SAMPLED_IMAGE | GPU_COLOR_OUTPUT |
COMPOSER_OVERLAY`). When gamescope's renderer asks for a scan-out buffer, the
shim requests a pool buffer by geometry over the DRM socket and receives a
dup'd dma-buf fd via `SCM_RIGHTS`; gamescope renders into that fd, and the
*same* `AHardwareBuffer` is published. The daemon owns the buffer end to end —
no import, no copy, and no cross-process manufacture of an `AHardwareBuffer`
from a bare fd.

## Format identity

The pool is BGRA (`BGRA_8888`), which is byte-for-byte identical to DRM
`XRGB8888`/`ARGB8888` (`XR24`/`AR24`), so the frames gamescope renders need no
conversion and no shader. The daemon advertises exactly the formats the pool is
laid out in, so gamescope cannot elect a layout the pool does not have.

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
