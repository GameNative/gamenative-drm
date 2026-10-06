# 07 — Kernel DRM parity: async commit + vblank event queue

gndrm commits behave like a real kernel DRM driver: an atomic commit validates,
queues, and returns immediately, and the flip event arrives later, when the
buffer is latched. Gamescope's buffer-lifecycle logic depends on that contract —
a buffer is not touched again until its flip event fires.

## Model

One commit worker per CRTC (there is one CRTC), an in-flight buffer set, and a
flip event that means "SurfaceFlinger latched it":

```
serve thread                          commit worker (per CRTC)
   |                                      |
atomic commit -> validate (TEST_ONLY,     |
   props, errno) -> queue -> return 0     |
                                          | pop commit
                                          | wait IN_FENCE (acquire fence)
                                          | presenter.publish(buffer, fence)
                                          |   -> setBuffer + apply (async)
                                          | SurfaceFlinger latch / release fence
                                          | emit PAGE_FLIP_EVENT (ordered, once)
                                          | mark buffer reusable
```

## Component responsibilities

### `modules/gndrm-daemon` (device core)

- The serve thread owns the whole `Device`; no lock is held on the device.
- `ioctl_atomic` (the non-`TEST_ONLY` path) validates, queues, and returns `0`
  without presenting. `TEST_ONLY` validates and returns errno without queuing.
- A buffer is **in-flight** from commit until its flip event fires, and is only
  reusable after that. The "retired" signal for pool recycling is flip-event
  delivery.
- The commit worker presents the buffer, waits the SurfaceFlinger latch, then
  emits `PAGE_FLIP_EVENT` and marks the buffer reusable. Events are ordered and
  one per commit.
- `IN_FENCE_FD` passes through as the acquire fence; the presenter's release
  fence is the latch the worker waits on.
- The `Presenter` trait is async: `fn publish(&mut self, fb, plane, stores,
  fence) -> FlipHandle`, with `FlipHandle` resolved on the latch (or a pollable
  variant). The worker emits the event on the latch without blocking the serve
  thread.
- A non-blocking commit whose fence is not signalled returns `EBUSY` rather than
  stalling. The ioctl surface and wire protocol are byte-compatible with the
  shim — the client side does not change.

### `modules/gndrm-android` (presenter host)

- `AscPresenter` implements `publish -> FlipHandle`. The
  `ASurfaceTransaction` completion callback signals `FlipHandle`;
  `ASurfaceTransactionStats_getPreviousReleaseFenceFd` provides the latch.
- The scan-out allocator keeps its own lock and warm spare; "retire" keys on
  flip-event delivery.

### `modules/gndrm-shim`

- No protocol change: ioctl marshalling, `SCM_RIGHTS`, and the scan-out channel
  are as documented in 02. The shim is a synchronous request/reply client that
  serialises ioctls behind one lock.

## Concurrency summary

- **Serve thread** — owns the `Device`, handles all ioctls (no lock on the
  device).
- **Commit worker (per CRTC)** — present → wait latch → emit flip event.
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
