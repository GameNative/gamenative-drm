# Rework of the fps limiter: cap at acquire, park A34, count acquisitions

Branches: `fix/rework-fps-limiter` in **gamescope** (from `gamenative`) and in
**gamenative-drm** (from `master`, `e51d2a5`). That commit is the squash of the
performance-tuning work whose tree the measurements in §6 come from; `87d38ee`
is the unsquashed equivalent and is no longer on any branch (the parked gate
commit lives on `backup-performance-tuning-gate`). The two repos are independent
of each other except for the wire message in §4, which must ship in the same
payload/APK pair.

## 1. Goal

Move the fps cap to the step where a frame **starts**, stop capping at the step
where a frame is **shown**, and make the app's frame counter count the same
event the cap controls.

| placement | what waits | render work wasted | display delay |
|---|---|---|---|
| acquire (target) | the guest's next frame | none | none |
| present queue | the frame already drawn | the frames it drops | up to one period |
| composite (A34, parked) | the compositor's callback | none | up to one period |

Non-goals: changing the present queue depth, LSFG pacing, or the present-wait
work (already merged, in `master`).

## 2. Current state

- **A34** (`android/patches/gamescope/0034-A34-...patch`) holds *two* changes:
  1. `steamcompmgr.cpp`: the fixed-rate arm defers the vblank callback on
     `last_commit_first_latch_time + cycle`, and the limiter's cycle becomes the
     target frame time instead of a whole number of panel cycles.
  2. `DRMBackend.cpp`: the `-r` mode lookup re-finds the mode by size when the
     grid matches no mode's `vrefresh`.
  Parking the patch as a whole would also revert (2), and `GamescopeState.gridArg()`
  passes `-r 1200` on every launch — a grid that matches no mode. **A34 must be
  split**: (1) is parked, (2) stays as its own patch (`A47`, next free number).
- **The cap** is enforced at release: `scanout::note_release` (`icd_shim/src/scanout.rs:161`)
  advances a process-wide schedule and stamps `slot.floor_ns`/`release_held`;
  `acquire_take_free` frees a held slot when the floor passes. The guest is
  therefore paced indirectly, through the *release* of a slot it is waiting on.
- **The frame counter** is display-side: `asc.rs:1267` calls
  `notify_frame_published()` on the commit worker, once per *published* frame
  (a changed primary framebuffer), which the app's `FrameRating` reads as its
  frame-time ring (`GameNativeDrm.onFramePublished`).
- **The target** travels as `FrameCapNote` (daemon → shim) and is also the value
  `nativeSetFpsLimit` gives gamescope. `lsfg::source_cap()` overrides it while
  frame generation runs.

## 3. Target design

### 3.1 Cap at acquire

Per-swapchain schedule, advanced by the acquire itself:

```
on acquire:
    if target == 0:                        # uncapped
        take a free slot, return
    now = monotonic_ns()
    if now < next_acquire_ns:
        wait until min(next_acquire_ns, timeout)  # condvar, no spin
        retry
    take a free slot
    next_acquire_ns = max(next_acquire_ns + period, now)
```

- `next_acquire_ns` lives on `GnSwapchain`, appended at the end so no existing
  field moves (the device struct has the stricter rule — thunks address `scratch`
  and `real` by fixed offset — but appending costs nothing and keeps that
  invariant uniform).
- The gate is checked **before** a slot is marked acquired, so a waiting guest
  holds no slot — it cannot starve the compositor while it waits.
- The release path stops pacing entirely: `note_release`, `floor_ns`,
  `release_held`, `slice_to_floor`, and the `acquire_take_free` release branch are
  removed. A release just marks the slot free and notifies `cv_free`.
- `CAP_FPS` stays as the *target* (note or `source_cap()`), read by the gate.
- LSFG is unaffected: generated frames are never acquired by the guest, and the
  presenter's grid still reads `source_cap()`.

Why this is better than the release-side floor, in one line: the schedule is
advanced by the event it controls, so a stall does not leave a debt for the next
burst to pay (the release-side schedule recorded the *free instant*, which is
what produced the 49 ms holds measured on Ori).

### 3.2 A34 parked

- Move `0034-A34-fps-cap-lower-bound-and-r-grid-guard.patch` into
  `android/patches/disabled/` (its `steamcompmgr.cpp` hunks are the limiter).
- Add `0034-A47-drm-mode-lookup-by-size.patch` holding only the `DRMBackend.cpp`
  `-r` lookup. It keeps A34's number on purpose: the train applies in glob order,
  and a patch's context is the tree state at its position, so moving it later in
  the train would change what it has to match.
- No app change: `nativeSetFpsLimit` still calls `injector::set_fps_limit`, which
  now has no limiter to arm. It can stay (harmless, and keeps the API stable), or
  be dropped in a follow-up once the rework has device evidence.

### 3.3 Count acquisitions

New wire frame, shim → daemon, on the scan-out channel:

```rust
/// Cumulative frames acquired on this connection. Magic "GNAQ", 24 bytes:
/// magic:u32, reserved:u32, count:u64, reserved:u64.
pub struct FrameAcquired { pub count: u64 }
```

- The shim increments a `static ACQ_COUNT: AtomicU64` on every successful acquire
  and sends the cumulative value with `MSG_DONTWAIT`, after dropping the swapchain
  lock. A dropped send loses nothing: the next message carries the new total.
- The daemon keeps `last_acquired`; on each message it calls
  `notify_frame_published()` for the delta (bounded to, say, 240 per message so a
  long stall cannot produce an unbounded burst).
- Remove the call from `asc.rs:1267`; keep `clear_booting_splash_on_first_frame()`
  there, which is a first-*presented*-frame concern and must stay on that path.

## 4. Task list

### gamenative-drm (`fix/rework-fps-limiter`)

1. `gndrm-uapi/src/wire.rs`: `FRAME_ACQUIRED_MAGIC`, `FrameAcquired { count }`,
   encode/decode, a unit test for the codec.
2. `icd_shim/src/scanout.rs`: `send_frame_acquired(count)` (non-blocking); keep
   `send_present_mark`. Add the `FrameAcquired` case to `poll_notes` **and** to the
   exchange/connect readers (same reason as `PresentDone`: a frame arriving
   mid-exchange must not be read as a dead channel).
3. `icd_shim/src/proxy.rs`:
   - `GnSwapchain.next_acquire_ns` (appended), initialised to 0.
   - `gn_acquire_common`: the gate from §3.1, before `acquire_take_free`; on
     success, `send_frame_acquired(ACQ_COUNT.fetch_add(1)+1)` after `drop(g)`.
   - Delete `slice_to_floor`; `acquire_take_free` loses its `release_held` branch.
   - Remove `note_release`/`floor_ns`/`release_held` from `note_release` callers
     (`x11_reap`, `on_buffer_release`).
4. `icd_shim/src/scanout.rs`: `note_release`, `advance_schedule`, `CAP_FREE_NS`
   and the floor tests go; `CAP_FPS` and `target_fps()` stay.
5. `modules/gndrm-android/src/asc.rs`: `FrameAcquired` handling + delta calls to
   `notify_frame_published()`; remove the publish-path call.
6. Tests: wire codec; gate arithmetic (two acquires one period apart, uncapped
   passes straight through, a stalled gap does not bank time); existing suite
   green; both clippy targets clean.

### gamescope (`fix/rework-fps-limiter`)

7. Split A34: `0034-A47-drm-mode-lookup-by-size.patch` from its `DRMBackend.cpp`
   hunk; move the whole A34 file to `disabled/`.
8. Verify the train: 29 patches apply, A47 reverses cleanly, `-r 1200` still
   resolves a mode (build log or device).
9. No app-side change in this branch.

## 5. Behaviour matrix

| session | cap | before (A34 + floor) | after (acquire gate) |
|---|---|---|---|
| Vulkan, LSFG off | 60 | guest 60, A34 also armed | guest 60 at acquire, no output timer |
| Vulkan, LSFG on | 60×2 | floor paces source, presenter multiplies | unchanged (same source cap) |
| GL through Xwayland | 60 | A34 caps the display | **uncapped** — accepted regression, see §7 |
| uncapped (0) | — | no gate | no gate |
| title ignoring acquire | 60 | floor has nothing to hold | gate also inert; present-queue pacer is the fallback |

## 6. Test plan (device, Nova)

Arms A/B: `master` (`e51d2a5`, the release-side floor; its tree is `87d38ee`'s)
vs the rework, same container, same scene, LSFG off, cap 60, 45 s windows,
alternating, two runs each.

- **Cap holds**: census `presiv` mean ≈ 16.7 ms; `acq`/`present` counters track;
  the app's frame ring reads ≈ 60.
- **Cost**: `/proc` CPU for game/app/gamescope (expect ≈ 150 % game, as measured
  with the floor, not 325 % as without).
- **No held releases**: the floor's `held=` census is gone; `slots=` shows no slot
  stuck in flight.
- **No debt after a stall**: a deliberate hitch (open the quick menu) must not be
  followed by a burst of late frames — the failure mode the release-side schedule
  produced.
- **A34 parked**: gamescope log shows no limiter deferral; the panel interval is
  still 16.7 ms at cap 60.
- **GL title** (if one is available): expect uncapped, recorded as the known gap.

## 7. Risks

- **GL titles lose their cap.** A34 was the only limiter for presents outside the
  ICD. Mitigation: keep the patch in `disabled/`, and decide after measurement
  whether to re-enable it for non-Vulkan sessions (which needs the session-kind
  signal, since gamescope cannot see the client's API).
- **Wire skew.** `FrameAcquired` reaches a daemon that does not know it → logged
  as an unrecognised frame and ignored (existing behaviour), so the counter simply
  stops; the cap is unaffected because it is local to the shim.
- **A guest that ignores acquire** sees no cap at all. Same gap as today's floor;
  not made worse.
- **Schedule reset on swapchain recreation** (resolution change): `next_acquire_ns`
  is per swapchain, so a recreated chain starts unthrottled for one frame. Fine.

## 8. Open questions

- Should `nativeSetFpsLimit` keep calling `injector::set_fps_limit` once A34 is
  parked? Keeping it is harmless and keeps the JNI contract; dropping it is a
  larger app change with no benefit until a non-Vulkan cap exists.
- Should the app's frame ring be fed by the shim's acquire count *or* by the
  daemon's own presentation count for the overlay? The acquire count matches the
  cap; the presentation count matches the panel. Pick acquire for now (it is what
  the limiter controls), and revisit if the overlay's fps disagrees with a
  capture.
