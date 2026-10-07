# 07 — Kernel DRM parity: async commit + vblank event queue

gndrm commits behave like a real kernel DRM driver: an atomic commit validates,
queues, and returns immediately, and the flip event is delivered when the flip
is queued for the next vblank. Gamescope's present loop blocks on that event, so
the worker emits it as soon as it has published — never after SurfaceFlinger's
completion. A buffer's lifetime is pinned to the compositor's own release
signal, not to the event.

## Model

One commit worker per CRTC (there is one CRTC), an in-flight buffer set, and a
flip event that means "the flip is queued for the next vblank":

```
serve thread                          commit worker (per CRTC)
   |                                      |
atomic commit -> validate (TEST_ONLY,     |
   props, errno) -> queue -> return 0     |
                                          | pop commit
                                          | wait IN_FENCE (acquire fence)
                                          | presenter.publish(buffer, fence)
                                          |   -> setBuffer + apply (async)
                                          | emit PAGE_FLIP_EVENT (ordered, once)
                                          | mark buffer reusable
                    SurfaceFlinger completion (release fence) -> pool reuse gate
```

## Component responsibilities

### `modules/gndrm-daemon` (device core)

- The serve thread owns the whole `Device`; no lock is held on the device.
- `ioctl_atomic` (the non-`TEST_ONLY` path) validates, queues, and returns `0`
  without presenting. `TEST_ONLY` validates and returns errno without queuing.
- A buffer is **in-flight** from commit until its flip event fires. The event
  is emitted with the publish; a buffer is reused only once the presenter has
  reported SurfaceFlinger's completion (the pool's `latched` flag) and the next
  publish onto that slot has waited out the previous release fence.
- The commit worker presents the buffer and emits `PAGE_FLIP_EVENT` with the
  publish. Events are ordered and one per commit; the worker never waits on
  SurfaceFlinger.
- `IN_FENCE_FD` passes through as the acquire fence; the presenter's release
  fence gates pool reuse.
- The `Presenter` trait is `fn publish(&mut self, fb, plane, ino, live_inos,
  fence)` — it hands the frame to the compositor and returns. The worker emits
  the event without blocking the serve thread.
- A non-blocking commit whose fence is not signalled returns `EBUSY` rather than
  stalling. The ioctl surface and wire protocol are byte-compatible with the
  shim — the client side does not change.

### `modules/gndrm-android` (presenter host)

- `AscPresenter` implements `publish`. The `ASurfaceTransaction` completion
  callback sets the pool's completion flag;
  `ASurfaceTransactionStats_getPreviousReleaseFenceFd` provides the release
  fence the next publish waits on.
- The scan-out allocator keeps its own lock and warm spare; a buffer is reused
  only after its completion is reported.

### `modules/gndrm-shim`

- No protocol change: ioctl marshalling, `SCM_RIGHTS`, and the scan-out channel
  are as documented in 02. The shim is a synchronous request/reply client that
  serialises ioctls behind one lock.

## Concurrency summary

- **Serve thread** — owns the `Device`, handles all ioctls (no lock on the
  device).
- **Commit worker (per CRTC)** — present → emit flip event.
- **Scan-out allocator thread** — owns the pool and warm spare (its own lock).
- **Warm worker** — doorbell-driven; keeps the warm spare ready.
- **Input injector thread** — wayland virtual-input client.
- **Shim** — synchronous request/reply client, one lock.

## Acceptance criteria

1. A stalled presenter never stalls the serve thread — it no longer blocks on
   SurfaceFlinger.
2. Commits return promptly; flip events trail commits and are ordered.
3. A buffer is never re-presented before its flip event fires.
4. The daemon test suite passes; all build gates are warning-free.
