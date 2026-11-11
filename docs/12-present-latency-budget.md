# 12 — the present-latency budget: what the paced path pays

Extends doc `11` (the refresh model); the measurement rig is the same
(`drm: A23 flip commit` counter pairs, Dave the Diver menu, gamescope mode).

## The per-frame timeline (measured)

Instrumented the daemon for one measurement round (`gndrm-daemon::timeline`,
since removed) and captured per-frame monotonic marks at: commit ingest (serve
thread), publish start/finish, `ASurfaceTransaction` apply, the SurfaceFlinger
completion callback, and the page-flip-event emission. 2,678 frames on the
gamescope/uncapped path:

| segment | median | p10 | p90 | max |
|---|---|---|---|---|
| `apply_done` → SF `onComplete` (the SurfaceFlinger round trip) | 9.81 ms | 8.22 | 11.86 | 15.74 |
| publish (build + `apply`) | 1.03 ms | 0.80 | 1.41 | 4.62 |

The SurfaceFlinger round trip is **not** on the paced path: the flip event is
emitted with the publish, so a paced frame's present does not wait it out.
Buffer reuse still rides it — the next publish onto a slot waits on the previous
release fence the completion stored.

## SurfaceFlinger's hold on a buffer (which release rule is safe)

The page-flip event says the frame was *published*, not that SurfaceFlinger has
finished with it. A producer handed its buffer at that event may still be
writing into a buffer on the panel — a scan-out tear, and one no screen
recording can show, because the buffer is repainted before anything reads it
again. Deferring the release by one flip removes the tear by removing a buffer
from every producer: a 3-image swapchain keeps one in flight instead of two, so
a producer can no longer run ahead of the flip and a late frame has no cushion.

SurfaceFlinger's own release fence is the exact answer, and the daemon already
receives it. `ASurfaceTransactionStats_getPreviousReleaseFenceFd` reports, in the
completion of a transaction, the fence of the buffer that transaction
**replaced** — not the one it published:

```
publish X, which replaces W   ──▶  completion reports W's fence
                                   ▼
                            fence signals: W is free
```

So the fence is filed on W's mailbox (`ReleaseCtx::replaced_ctx`) rather than on
the publisher's — that attribution is what makes it usable at all — and it is
waited on off the completion thread. Two things use it:

- the publish that reuses W waits on it before applying, so the pool never
  republishes a buffer the panel is still reading;
- a watcher thread waits on it and pushes `BufferReleased` (`CAP_RELEASE_NOTE`)
  carrying W's scan-out inode. A shim that declares the cap keeps that slot busy
  until the note arrives: the compositor's `wl_buffer.release` only parks it.

A producer the shim does not render for — the guest's own GL blits through
Xwayland — reads no notes, and for that class the deferral above is the only
lever.

`sf hold:` measures the hold where it ends: the watcher samples the gap from the
released buffer's latch to the instant its fence signals.

```
gndrm-daemon: sf hold: n=<samples> <half=<n> half-one=<n> one-1.5=<n> 1.5-2.5=<n> over-2.5=<n> max=<us>us period=<us>us timeouts=<n>
```

Buckets are halves of one display period, and the period is the latch grid's own
(median of the last nine accepted gaps; a gap outside 3–50 ms is a burst or a
stall, not a period). `one-1.5` and above is the share a one-flip deferral does
**not** cover. `timeouts` counts fences that reached `RELEASE_WATCH_MAX` (500 ms)
without signalling — the release is reported anyway, so a stuck compositor
delays a producer instead of hanging it. One line per 10 s.

## Why the event is emitted with the publish

`commit_worker_loop` emits the page-flip event immediately after the publish
(when the frame is queued to SurfaceFlinger) and drops the frame from the
in-flight set at the same point. A KMS page-flip event reports when the flip is
*queued* for the next vblank — which is what the event means.

It matters because **gamescope's DRM backend blocks its present loop on this
event** — `src/Backends/DRMBackend.cpp`:

```cpp
if ( isPageFlip ) {
    // Wait for bPendingFlip to change from true -> false.
    drm->uPendingFlipCount.wait( uNewPendingFlipCount );
    assert( drm->uPendingFlipCount == 0 );
}
```

Gating the event on SurfaceFlinger's completion would serialise that round trip
into every paced frame.

Buffer-reuse safety is unchanged: the scan-out pool still caches a buffer for
reuse only once the presenter reports SurfaceFlinger's completion (the slot's
`latched` flag), and the publish that reuses a buffer still waits on that
buffer's own release fence. Only the *event* — a pacing/visibility signal to the
client — stops carrying the round trip.

## Measured effect

| property | value |
|---|---|
| latch-wait `pl` p50 | 15 us |
| uncapped present rate | ~259 /s |
| frames held two vblanks | 0.03 % |
| present rate at a 60 fps target | 60.0 /s |

The paced path meets its target, and the
uncapped rate is bounded upstream — the client's own present lap through the
ICD shim to gamescope — not by the flip-event path.

### The FIFO declaration is not the throughput cost

The shim's per-commit `swapchain_set_present_mode(FIFO)` declaration does not
bound the uncapped rate — removing both sites left it unchanged. It stays in the
tree: the request/divisor cap needs it.

## The X11 arm as a control — pacing is deadline/timer-based, not deferral

The X11 arm paces the **client socket read** against a deadline grid, not by
withholding a commit. `PresentExtension.scheduleIdleNotify`:

```java
final long frameNs = 1_000_000_000L / targetFps;
WindowTiming wt = windowTimings.computeIfAbsent(window.id, k -> new WindowTiming());
if (wt.nextIdleNs <= now - frameNs) { wt.nextIdleNs = now + frameNs; }
else                                { wt.nextIdleNs += frameNs; }
long fireTime = wt.nextIdleNs - FIRE_EARLY_NS;      // fired at the nearest Choreographer vsync
```

and `ShmFramePacer.framePresented`: `t.nextNs += 1_000_000_000L / fps`, returning
the delay the caller uses to suspend the client's reads. Both are absolute
deadline grids, so the target is hit exactly and no compositor round trip is
added. Contrast gamescope: `steamcompmgr.cpp` *defers* the commit
(`if (entry.fifo && (!entry_vblank || fifo_win_seqs.count(entry.winSeq) > 0))`
holds it in `commits_before_their_time`) and `DRMBackend.cpp` *blocks* the
present on the flip event — that deferral/blocking pair is what serialises the
compositor round trip into a paced frame.

The X11 **rate** is not reliably measurable on this device: `screenrecord` +
`ffprobe avg_frame_rate` and `dumpsys SurfaceFlinger --latency` (default layer)
both give invalid numbers here. So the X11 ceiling is unproven — do not read a
number into the arm asymmetry from this.

## What is NOT proven / open

- `sf hold:` has no device capture yet: the buckets say which release rule is
  sufficient, and nothing has read them on hardware.
- The uncapped ceiling (~259 /s on this scene) is **not** the flip-event path;
  it is the client↔gamescope lap (`vkAcquireNextImageKHR` in the ICD shim
  blocking on the compositor's `wl_buffer.release`). Not measured end to end;
  no fix attempted.
- A committed frame is treated as no longer in flight at publish (RMFB allowed
  there). No `drmModeRmFB failed` and no crash/abort appeared in the runs, but
  the daemon's own logs are gated off on device, so this was validated by the
  golden tests + the absence of gamescope RMFB errors, not by a daemon trace.
- The SurfaceFlinger round trip (9.81 ms) is measured on the Dave menu at the
  panel's ~120 Hz only; it is one panel frame + SF latency and will differ with
  load.
