# 18 — LSFG presenter: driver constraints

What the frame-generation path in the ICD shim depends on, and why. Each item is a
property of the driver or of the presentation model, not a tuning choice; changing
any of them silently disables or corrupts generation.

## The batch submits on a queue of its command pool's family

The shim keeps its queues newest-first, so the list head is not necessarily the
family the generation pool was created for, and the device exposes several
families. A submit whose queue family differs from its command pool's family is
discarded — the call returns success and the fence still signals, which reads as a
batch that finished instantly. `GnQueue` carries its `family` and the pass picks a
wrapper that matches the pool.

A fence that completes far faster than the work it covers is evidence of a dropped
submission, not of a cheap shader.

## The engine writes only into its own images

`vkCmdClearColorImage` and `vkCmdBlitImage` reach an image the shim creates, but a
compute store into that image does not land here; the same dispatch into an image
the chain owns works. Generation therefore targets `LsfgChain::Output()` and the
embedder copies the result into the caller's target, which consequently needs only
`TRANSFER_SRC | TRANSFER_DST`.

## The capture is a blit

The guest's presented frame is BGRA under the BGRA opt-in and the chain is RGBA8,
and a copy performs no conversion — a capture by `vkCmdCopyImage` yields
generated frames with red and blue exchanged. `CopyPresentedFrame` blits with `VK_FILTER_NEAREST`.

## Destinations are three, independent of the multiplier

A destination ring sized `multiplier − 1` cannot overlap one generation with the
next: the second frame's batch is refused while the first is still running, so the
output stays near the source rate no matter what the plan asks for. Three
destinations keep a generation in flight while the next ones build, and the panel
becomes the only ceiling.

## Only a guest slot's release is paced

The frame-cap floor exists to hold the guest to the source rate. Applying it to a
generated frame's release throttles the output back to that same rate, which makes
working generation look inert. `scanout::note_release` takes the slot's role and
paces only guest slots.

## The generate shader is bound as Rgba8

The translated module declares its output storage image as `Unknown`, because the
DXBC UAV is typeless, and a store through an `Unknown`-format binding writes
nothing. `lsfg_shaders.cpp` rewrites `OpTypeImage` to `Rgba8` for storage images,
matching the format the targets are created with; sampled images keep `Unknown`,
since their format comes from the view (RGBA8 frames, RGBA16F motion).

## Settings arrive through the daemon, with the DLL path

The presenter runs in the app's process, where the container's environment does
not exist, so the container's `Lossless.dll` path travels inside `LsfgConfig` on
the scan-out channel. A session the app turned off stays off: `enabled = 0` is a
value, not a fallback to the launch environment.
