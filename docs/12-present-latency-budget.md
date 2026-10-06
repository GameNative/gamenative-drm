# 12 — the present-latency budget: what the paced path pays, and the fix

Current state only (no chronology). Extends doc `11` (the refresh model); the
measurement rig is the same (`drm: A23 flip commit` counter pairs, Dave the
Diver menu, gamescope mode).

## The per-frame timeline (measured)

Instrumented the daemon for one measurement round (`gndrm-daemon::timeline`,
since removed) and captured per-frame monotonic marks at: commit ingest (serve
thread), publish start/finish, `ASurfaceTransaction` apply, the SurfaceFlinger
completion callback, and the page-flip-event emission. 2,678 frames on the
gamescope/uncapped path:

| segment | median | p10 | p90 | max |
|---|---|---|---|---|
| `apply_done` → SF `onComplete` (the SurfaceFlinger round trip) | **9.81 ms** | 8.22 | 11.86 | 15.74 |
| SF `onComplete` → flip-event emit (original ordering) | 0.39 ms | 0.25 | 0.64 | 5.48 |
| publish (build + `apply`) | 1.03 ms | 0.80 | 1.41 | 4.62 |
| flip-event period (frame period) | 16.92 ms | 15.70 | 25.60 | — |

The frame period is **the grid interval plus the SurfaceFlinger round trip**
(period − roundtrip = 8.54 ms ≈ one vblank). That is the additive term C.

## The owning code site

The daemon emitted the page-flip event only *after* the presenter reported
SurfaceFlinger's completion, gating the whole round trip into the event:

```
// modules/gndrm-daemon/src/lib.rs, commit_worker_loop (before the fix)
// Wait for the latch, bounded. ...
let mut latched = handle.is_latched();
while !latched { ... latched = handle.wait_latched_timeout(slice); }
...
if let Some(mut ev) = job.event.take() {        // emitted only after the wait
    let (sec, usec) = (clock.lock()...)();
    ev.tv_sec = sec; ev.tv_usec = usec;
    flip_queue.lock()....push_back(ev); write_eventfd(flip_event_fd);
}
```

It matters because **gamescope's DRM backend blocks its present loop on this
event** — `src/Backends/DRMBackend.cpp`:

```cpp
if ( isPageFlip ) {
    // Wait for bPendingFlip to change from true -> false.
    drm->uPendingFlipCount.wait( uNewPendingFlipCount );
    assert( drm->uPendingFlipCount == 0 );
}
```

So every paced frame's present returned only after SurfaceFlinger had finished,
adding C ≈ the round trip on top of the pacing grid.

## The fix

`commit_worker_loop` now emits the page-flip event **immediately after the
publish** (when the frame is queued to SurfaceFlinger) instead of after the
completion callback, and drops the frame from the in-flight set at the same
point. The latch wait is kept, after the event, as the bounded completion
acknowledgement and teardown back-pressure. A KMS page-flip event reports when
the flip is *queued* for the next vblank — which is what the event now means.

Buffer-reuse safety is unchanged: the scan-out pool still caches a buffer for
reuse only once the presenter reports SurfaceFlinger's completion (the slot's
`latched` flag), and the next publish still blocks on the previous release
fence. Only the *event* — a pacing/visibility signal to the client — stops
carrying the round trip. The two golden tests that encoded the old
"event follows the latch" contract were updated to the new one.

## Measured effect (Dave menu, gamescope mode, `A23` counter pairs)

| condition | before (no fix) | after (fix) |
|---|---|---|
| capped, request path target 60 | **39.87 /s** (1045→1972 over 23.25 s) | **55.41 /s** (1596→2881 over 23.19 s) |
| uncapped (`-r 120` / request 120) | 55.72 /s (1491→2782 over 23.17 s) | 55.25 /s (1464→2745 over 23.19 s) |

The paced path gained **+39 %** and now sits at the uncapped ceiling. The
**uncapped** rate is unchanged: it is bounded upstream (the client's own
present lap through the ICD shim to gamescope), not by the flip-event path.

### The FIFO declaration is not the throughput cost (measured)

Removing the shim's per-commit `swapchain_set_present_mode(FIFO)` (both sites)
leaves the **uncapped** rate unchanged — 56.12 /s vs 56.69 /s — and the capped
mapping unchanged (`-r 60` 39.77 vs 39.81, measured earlier). With no target
FPS the deferral gate does not engage, so this is expected; the declaration is
still required for the request/divisor cap and stays in the tree.

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
present on the flip event — the deferral/blocking pair is where C lived.

The X11 **rate** was not reliably measured this round. Two sources were tried
and both are invalid on this device: `screenrecord` + `ffprobe avg_frame_rate`
read 42.9 /s (limiter off) and 10.8 /s (target 60), and `dumpsys SurfaceFlinger
--latency` (default layer) returned 127 timestamps spanning 463 s (frozen, not
the app's present stream). So the X11 ceiling is unproven — do not read a
number into the arm asymmetry from this.

## What is NOT proven / open

- The uncapped ceiling (~56 /s on this scene) is **not** the flip-event path;
  it is the client↔gamescope lap (`vkAcquireNextImageKHR` in the ICD shim
  blocking on the compositor's `wl_buffer.release`). Not measured end to end
  this round; no fix attempted.
- The fix treats a committed frame as no longer in flight (RMFB allowed at
  publish). No `drmModeRmFB failed` and no crash/abort appeared in the fix
  runs, but the daemon's own logs are gated off on device, so the guard change
  was validated by the golden tests + the absence of gamescope RMFB errors, not
  by a daemon trace.
- The SurfaceFlinger round trip (9.81 ms) is measured on the Dave menu at 120 Hz
  only; it is one panel frame + SF latency and will differ with load.
