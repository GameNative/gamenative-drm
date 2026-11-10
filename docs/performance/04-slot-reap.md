# 04 — The swapchain slot reap: route distinction, epochs, and hidden depth

Repo `gamenative-drm`, branch `performance-report`. Read-only analysis; no code
changed. Line numbers are from this tree.

Scope: doc 24 §5 item 3 ("batch and event-drive the slot reap"). The constraint
is absolute: the guest must keep observing the same `vkGetSwapchainImagesKHR`
count it asks for today. That rules out the naive "make the pool 4 deep" and
forces the depth change into the hidden-slot mechanism the strict-image-count
work already built.

---

## 1. Slot lifecycle, end to end

Per swapchain, `n_slots` slots are allocated at creation and split into a
**guest prefix** (`guest_slots`) and, only for a strict game on the gamescope
route, a **hidden ring** (`spare_slots`).

| step | site | what changes |
|---|---|---|
| count decided | `proxy.rs:2313`–`2319` (`swapchain_slots` `:7364`, strict clamp `:2315`) | `n_slots` |
| slots allocated | `proxy.rs:2370`–`2408` (`slot_init` → `scanout::allocate` `:12093`) | one daemon scan-out buffer per slot, `~3.69 MB` at 1280x720 (`stride*height*4`) |
| split | `proxy.rs:2419` `guest_slots = n_slots`; `:2422`–`2443` hidden ring, `spare_slots` | `guest_slots`, `spare_slots`, `slots.len()` |
| **create** | `gn_vkCreateSwapchainKHR`; worker thread `gn-present` spawned `:2484` | `in_flight=false`, `acquired=false` |
| **acquire** | `gn_acquire_common` `:2696`; gate `:2725`–`2747`; `acquire_take_free` `:2672` (scan `0..guest_slots` `:2673`, sets `acquired=true` `:2678`) | `acquired=true`, `acq_seq` |
| guest renders | outside the shim | — |
| **present** | `gn_vkQueuePresentKHR` `:3238`; index validated `< guest_slots` `:3299`; slot fence reset `:3357`; present submit `:3376`–`3405`; discipline `present_fifo` `:2939` / `_relaxed` `:2966` / `_mailbox` `:2990` / `_immediate` `:3017` | `acquired=false`, **`in_flight=true`** (`:2943`, `:2970`, `:2994`, `:3021`) |
| worker | `worker_main` `:1961`; waits the slot fence `:1982`; commit `commit_real` `:2009`; attach/damage/commit/flush `:2142`–`2168` | nothing; `in_flight` stays true |
| **release** | wayland: `on_buffer_release` `:8049` on the `gn-wl-dispatch` thread (`:7913`, spawned `:8024`) | **`in_flight=false`**, `cv_free.notify_all()` `:8073`–`8074` |
| **slot free** | next acquire's `acquire_take_free` `:2672` sees `!acquired && !in_flight` `:2675` | — |

Other paths that clear `in_flight` and must stay consistent:
`spare_handover` for a strict game's guest image at the copy fence `:1141`;
mailbox replacement discards the superseded slot `:3001`; present failure paths
`:2102`, `:2117`; `vkReleaseSwapchainImagesKHR` frees only acquired-and-not-in-
flight guest slots `:3216`; `spare_unclaim` `:1124`.

### Every place a slot can stall

- **S1 — guest render fence.** Worker waits `(*s).fence` (`:1982`), sliced
  `GEN_WAIT_SLICE_NS` while a generation batch is due. GPU-bound, ~one frame,
  not the shim's to remove.
- **S2 — wayland commit pending / compositor latch.** The only release signal
  on the default route. A slot stays `in_flight` from `commit_real`'s
  `surface_commit` (`:2167`) until gamescope hands the `wl_buffer` back
  (`:8049`). gamescope holds a buffer until it latches a replacement, so the
  release of frame *N* is a function of frame *N+k*'s latch, i.e. this is the
  **depth-consuming** stall.
- **S3 — X11 present idle fence.** Polled only: `x11_reap` `:674` →
  `fence_idle` `x11.rs:1170` (a `xcb_sync_query_fence` + blocking
  `xcb_sync_query_fence_reply`, i.e. a full round trip, `x11.rs:1167`–`1188`).
  Entered from the acquire loop `:2786`, from `present_sync_commit` `:3042`,
  after a worker present `:2094`, and from the worker's 2 s idle tick `:1949`.
  **Not reachable on the default route** (below).
- **S4 — maintenance1 release fence.** `:3312`, `:3372`–`3420`. It signals when
  the present's *wait semaphores are consumed* (`docs/20-swapchain-maintenance1.md:34`),
  i.e. at submit completion. It is **not** a buffer release and must never be
  used as one: the compositor still owns the image.
- **S5 — strict image count.** `guest_slots` is exactly the ask, so a 2- or
  3-image game's images are all it has; the hidden ring `:2422` is what lets the
  compositor hold a buffer the guest does not see (`spare_begin` `:1074`,
  `spare_handover` `:1132`). With no hidden slot free the frame goes out on the
  guest's own image and the guest waits at its next acquire (`:1108`–`1110`) —
  by design, and it can regress to the S2 stall.
- **S6 — the cap gate.** `:2725`–`2747` parks on the **same** `cv_free` as the
  slot wait (`:2744` vs `:2803`). Deliberate; must be separated from starvation
  in the census (`gate_holds=` vs `starved=`).
- **S7 — lost wakeup.** The starve path is `drop(lock)` → `x11_reap` (`:2786`)
  → `relock` → `acquire_take_free` (`:2788`) → `cv_free.wait_timeout(REAP_SLICE)`
  (`:2803`, `REAP_SLICE = 4 ms` `:2657`). A release that lands between the
  `drop` and the wait calls `notify_all` with no waiter, and there is no epoch to
  re-check, so the acquire sleeps the **whole 4 ms** even though the slot was
  free ~µs later.
- **S8 — job queue cap.** `GN_JOB_CAP = 16` `:7334`; a full queue makes
  `present_fifo` commit inline on the guest's thread `:2949`–`:2953`.
- **S9 — create-time allocation.** `scanout::allocate` `:12093` →
  `scanout.rs:348`–`365` (three exchange attempts on a single-reader daemon
  channel). Per create, not per frame.

---

## 2. Reap cost, quantified — and the route distinction that reframes the item

**The default route's reap is a no-op.** `x11_reap` returns at `:675`–`678`
when `(*sc).surf).x11` is null; `x11` is only ever set on the
`GN_VKSHIM_GUEST_SURFACE=1` branch (`:9778`, field `:7631`, default null
`:9599`, `:9734`), and the default route is the `!use_guest_surface()` branch
(`:9655`, `:9767`). So on the measured arm the acquire loop's "reap" costs one
null-checked function call and **zero** X requests.

Per-route cost of one `x11_reap` call:

- **wayland (default, measured arm): 0 X work.** The cost the starve path
  actually pays is: 1 mutex unlock/lock pair around `:2785`–`:2787`, a retry of
  `acquire_take_free` (O(`guest_slots`), `:2673`), and one
  `cv_free.wait_timeout` park. With the 4 ms slice against a 2.9 ms frame
  (`1280x720 @ ~340 fps`), a missed notify is **1.38 frame periods** of dead
  time — the self-throttle the `:2779`–`:2784` comment describes.
- **X11 / DRI3 (opt-in, `GN_VKSHIM_GUEST_SURFACE=1`): one round trip per
  in-flight slot per call**, plus one `shared` lock per slot (the `for` at
  `:681` locks inside the loop at `:685`–`:688`). At 3 guest slots with 2–3 in
  flight that is 2–3 `xcb_sync_query_fence` round trips **per acquire
  iteration**, and the starve loop can iterate twice before it parks. On an
  Xwayland unix socket a round trip is ~15–40 µs ⇒ **~45–120 µs per starved
  frame, 1.5–4 % of one core at 340 fps** — which is the right size to explain a
  1.6 pp sys delta, *on the wrong route*.

**What a starved frame looks like in the current code:** `acquire_take_free`
returns nothing for `0..guest_slots`, `SLOT_WAIT_N` increments once per acquire
(`:2765`), the loop drops the lock, calls a reap that does nothing (`:2786`),
re-checks (`:2788`), still finds nothing, and parks on `cv_free` for
`min(deadline-now, 4 ms)` (`:2799`–`:2803`). If every acquire did this, the
ceiling would be `1/4 ms = 250 fps`; at 340 fps observed against 344.6 fps on
X11 (−1.6 %), the average extra latency is only `0.016 × 2.9 ms ≈ 46 µs/frame`,
i.e. **one full 4 ms slice lost about once per 87 frames**. That is the whole
measured size of C before any change.

**Consequence for the plan:** item 3 as written in doc 24 §5 ("one
`xcb_sync_query_fence` per slot per frame becomes one per release event") is a
real 1.5–4 %-of-a-core saving **only on the opt-in DRI3 route**, which is not
the measured arm and which doc 24 §5 explicitly declines to adopt. On the
measured arm item 3 is a **latency** item worth ≲0.1 pp CPU. That has to be
said before the work is reordered, and §5 below gives the measurement that
decides it (`starved=`, currently unreadable at 1 Hz — see §5).

---

## 3. Design

Three sub-changes, deliberately separable. Names are proposals, not existing
symbols.

### (a) Batch the reap; skip it when nothing can have changed

1. `x11_reap` → `slots_reap(sc)`:
   - hoist the route test **before** the lock (`:675`) — default route returns
     without touching `shared` at all;
   - snapshot `(i, xfence, in_flight)` for every slot under **one** lock into a
     fixed array (`GN_MAX_SC_IMAGES = 8`, `:7332`), instead of locking per slot
     (`:685`–`:688`);
   - split `fence_idle` into send/receive: `x11.rs` gains
     `query_fences_send(&[u32]) -> [cookie; N]` (issue N
     `xcb_sync_query_fence`, `xcb_flush` once) and `query_fences_recv(cookies)`
     (N `xcb_sync_query_fence_reply`). One write + typically one read syscall,
     one round trip of *latency* for the whole batch, N requests on the server.
   - **pipeline the batch**: keep the outstanding cookie set on the swapchain;
     if a batch is already outstanding, this call only tries to harvest it. At
     most one batch in flight per swapchain ⇒ ≤1 round trip per swapchain per
     reap interval, not per call and not per slot.
2. Gate the call in the acquire loop (`:2786`): only reap when
   `sc.needs_poll` is set — an `AtomicBool` set by `x11_present_slot` (`:648`,
   the `Some(serial)` arm) and by the worker's commit, cleared when a reap
   harvest finds nothing `in_flight`. On the default route it is never set, so
   the starve path becomes: lock, take, park — no function call, no route test.
3. Replace the fixed slice with an **epoch wait** (this is also (b)'s
   default-route half): `release_epoch: AtomicU64` bumped under `shared` in
   `on_buffer_release` (`:8073`), in `spare_handover` (`:1141`),
   `spare_unclaim` (`:1124`), mailbox replace (`:3001`), release-images
   (`:3216`) and the present-failure paths. The loop captures the epoch before
   `acquire_take_free` and waits on `cv_free` with
   `timeout = min(deadline - now, next gate_ns, IDLE_EVERY)` — i.e. no 4 ms
   fiction. `REAP_SLICE` survives only as the X11 route's poll interval, where
   a bounded poll is the real requirement.

### (b) Make release event-driven, and name the signal honestly

The signal already exists on the route that matters:

- **wayland (default): `wl_buffer.release` → `on_buffer_release`**
  (`:8049`), dispatched by `gn-wl-dispatch` (`:7913`, spawn `:8024`), which
  blocks in `poll()` on the display fd (`:7953`–`:7968`) and calls
  `dispatch_queue_pending` in a loop (`:7935`–`:7943`). Nothing new is needed
  except the epoch in (a.3): the reason the acquire polls is not a missing event,
  it is that it cannot tell whether the epoch moved. **Do not** add a second
  source here; two threads reaping would contend on `shared` and reorder
  `present::complete_on_release`.
- **X11 (opt-in): Present `IdleNotify`.** The protocol has the event the polled
  fence is emulating, and the shim never selects it (`x11.rs:1124`–`1165` does
  `xcb_present_pixmap` and nothing else; no `xcb_present_select_input`
  anywhere). Change: select `XCB_PRESENT_EVENT_MASK_IDLE_NOTIFY` on the window
  after `X11Present::new`, add a `gn-x11-events` thread (mirroring
  `dispatch_main`) blocking in `xcb_wait_for_event`, and map
  `PresentIdleNotify.serial` (or `.pixmap`) → slot. That needs the serial kept
  per slot: append `xserial: u32` **after** `spare` in `GnSlot` (`:7679`) —
  appended so nothing moves, and `size_of::<GnSlot>()` is used symbolically in
  `on_buffer_release`'s index arithmetic (`:8067`), so the layout change is
  safe. `present()` already returns the serial (`x11.rs:1164`); store it in
  `x11_present_slot`'s `Some(serial)` arm (`:649`). Then the X11 route has no
  polling at all and `x11_reap` collapses to the event handler.
- **maintenance1 present fence: not usable.** See S4. Anyone reaching for
  `(*slot).fence_sem` as the release signal will hand the guest an image the
  compositor still scans out. Keep the second submit at `:3415`–`:3420` purely
  a semaphore-reuse contract.
- **daemon flip event (`PresentDone`)**: available (`scanout.rs:719`,
  pushed from `asc.rs:449`) but strictly *later* than the buffer release, and
  currently only read inside the release callback (`scanout.rs:8060`). Use it
  only as a diagnostic bound ("latch of N arrived, release of N has not"), never
  as the release trigger.

### (c) A fourth (and fifth) slot **behind** the guest's images, not in them

The requirement "do not change the image count the guest observes" is satisfied
by extending the existing hidden-slot mechanism, not by raising `n_slots`:

- At creation, when the route is gamescope's (`:2301`), the guest asked for 3 or
  more, and extra depth is enabled, allocate `d` hidden slots after the guest
  prefix and set `spare_slots = d` (`:2422`–`:2443`). `guest_slots` is
  untouched (`:2419`).
- Every present then copies the guest image into a free hidden slot on the
  **present's own submit** (`spare_begin` `:1074`, `spare_record` `:970`–`1066`,
  chained at `:3376`–`:3389`), and the guest image is freed at the copy fence
  (`spare_handover` `:1132`–`1151`) while the compositor gets the hidden slot.
  Present id, wait id and `acquired_ns` move to the hidden slot (`:1138`–`:1143`),
  so `vkWaitForPresentKHR` and the acquire→commit census still see one frame per
  present.
- `d = 2` is the minimum that removes the S2 stall (one on screen, one being
  committed); `d = 3` if the job queue can be two deep. `d` must be requested
  **at creation** — a push past `slots.reserve` moves every `GnSlot` and every
  `wl_buffer` user-data pointer into them (`:2409`–`:2418`) — so the reserve at
  `:2439` must cover `guest_slots + d`.

**Invariants that keep the guest's view identical**

1. `guest_slots` == what the guest asked for (post-`swapchain_slots` clamp).
2. `vkGetSwapchainImagesKHR` reads `guest_slots` only (`:2538`), caps `*count`
   (`:2543`) — never `slots.len()`.
3. Surface caps stay `minImageCount = SC_DEFAULT_IMAGES` (`:10015`) and
   `maxImageCount = max_swapchain_images()` (`:10016`); no call may return
   `slots.len()`.
4. Acquire scans only `0..guest_slots` (`:2673`) and present validates
   `< guest_slots` (`:3299`) — hidden slots are never handed to the guest.
5. Hidden slots are created at creation and covered by `slots.reserve`
   (`:2439`); never pushed while a buffer is out.
6. `spare_in_use` (`:918`) stays false while frame generation is live, and the
   copy state is bound to the first present's queue family (`:926`). The hidden
   ring and the generation destinations share one ring (`dest_ring` `:897`,
   `:2438`).
7. The direct path is preserved: no free hidden slot ⇒ the frame goes out on
   the guest's image (`:1108`–`:1110`) and no frame is lost or deadlocked.
8. Strict games are excluded from the new depth (`hidden_slot_count` already
   gives 2-image strict games 4 hidden slots, `image_count.rs:53`); their count
   rule and their "no image to spare" pacing stay exactly as documented in
   docs/23.

**`WRAPPER_MAX_IMAGE_COUNT`** (`:7387`–`:7395`) = `max_swapchain_images()`: the
ceiling on the **guest's ask** at creation (`:2314`) and the advertised
`maxImageCount` (`:10016`), itself capped at `GN_MAX_SC_IMAGES = 8` (`:7332`).
It is guest-visible, so extra depth must **not** be drawn from it: a new
`GN_VKSHIM_EXTRA_DEPTH` (0–4, default 0) bounds `d`, additionally clamped to
`GN_MAX_SC_IMAGES - guest_slots`. Setting `WRAPPER_MAX_IMAGE_COUNT=3` must not
silently disable depth, and `=8` must not license 8+3 slots.

**Cost of (c)**: one `vkCmdCopyImage` per frame (`:1026`–`:1036`) — 3.69 MB read
+ 3.69 MB write per frame ≈ **2.5 GB/s at 340 fps** on an 84 %-busy GPU, plus
`d` extra scan-out buffers (~7.4 MB at `d = 2`) allocated at create (`:12093`).
It buys depth with bandwidth, on a title already paying the linear-tiling term
(doc 24 §4.D). That is why it lands behind a flag and behind the cheaper test
below.

**Test before building (c):** `dxvk.numBackBuffers=4` in the container's
`dxvk.conf` is doc 24 §4.C's falsifier and costs nothing. Note the open
discrepancy: docs/23-strict-image-count.md:38 says DXVK asks
`minImageCount + 1` (i.e. 4 here), while the premise of this item is that the
guest asks 3. The census settles it: `proxy swapchain … N images;` (`:2501`)
and `slots=[…]` (`:360`–`:380`). If `guest_slots` is already 4 and the guest
still starves, depth is not the cause and (c) must not be built.

---

## 4. Independence and expected CPU

| change | independent? | blast radius | expected effect |
|---|---|---|---|
| **(a) batch + skip + epoch wait** | yes — `x11_reap`, the acquire loop, one new `AtomicU64` | shim only, default route semantics unchanged | default route: ~0 CPU, removes the 4 ms lost-wakeup (≈46 µs/frame at the measured rate ⇒ most of the 1.6 % fps gap if `starved=` is non-zero). DRI3 route: 2–3 round trips/frame → ≤1 (**1.5–4 % of a core**). |
| **(b) X11 IdleNotify pump + serial field** | yes — route-local, new thread, appended field | opt-in route only | removes all X polling on that route (~same 1.5–4 % there), **0 on the measured arm**. |
| **(c) hidden depth (d = 2)** | yes — create + `spare_*` only | largest: copy under way cannot be replaced, present ids move, +GPU bandwidth | removes S2 starvation entirely (`starved=`→0, `slots=` shows a free slot, `acqus.max`/`ge8=` collapse); CPU ~0–0.2 pp; **GPU busy may rise** by the copy. |

Which can land alone: **(a) can**, and should, alone — it is safe on both routes
and is the prerequisite for reading the result. **(b)'s default-route half is
(a.3)** and lands with it; (b)'s X11 half is independent of both and can wait.
**(c) is independent of (a)/(b) in code** (different functions) but not in
*evidence*: it must not be built until `numBackBuffers=4` and `starved=` say
depth is the cause.

Reasoning about the measured sys-time delta (+1.6 pp, sys-bound, guest-heavy,
doc 24 §2): the only per-frame syscall-shaped work this item owns is (i) one
futex park/unpark pair per starved acquire (`:2803`) — ~2–4 µs, ~1 ms/s at
340 fps, ≈0.1 pp of one core, and (ii) on the DRI3 route, the X round trips
above. (i) is real but tiny; if the arms were measured on the default route,
the 1.6 pp is **not** item 3 and belongs to B/E/H. Item 3's honest claim on the
measured arm is latency and frame-time variance, not CPU — and the numbers in
§2 bound it: at most ~46 µs/frame of acquire latency.

---

## 5. Instrumentation that proves it worked

Verified from the source (the strings the docs quote are not all real):

| string, exactly | site | cadence / gate | carries |
|---|---|---|---|
| `proxy hb: fmt=` | `proxy.rs:307` | 1 s, `logf!` | `fmt= fmt2= caps= caps2= pmodes= create= destroy= images= acq= present=` — **no `starved=`, no `slots=`** (doc 24 §3/§5 says otherwise; fix the doc or the line) |
| `proxy commit: sc=` … `#{}` | `proxy.rs:2175` | first N, every 120th, or `fence>=100ms` | `idx= WxH flush_rc= dpy_err= fence=ms commit=ms #N` + `counters()` |
| `proxy acquire: sc=` … `wait=` | `proxy.rs:2898` | first N, every 120th, or `wait>=100ms` | `slot= wait=Nms` |
| `gn-vkshim stall: acquire found no free slot (timeout=` | `proxy.rs:2826` | 1 s, only when a starve happens | `slot_flags()` ⇒ `starved=`, `slots=[…]` |
| `gn-vkshim idle: present worker has no work` | `proxy.rs:1952` | 2 s, only when idle | `slot_flags()` |
| `gn-vkshim release: slot=` | `proxy.rs:8086` | 1 s | `slot_flags()` |
| `proxy release: slot=` | `proxy.rs:8055` | first N per process | — |
| `x11 release: slot=` | `proxy.rs:708` | 1 s | `slot_flags()` (DRI3 route) |
| `proxy swapchain ` … ` images;` | `proxy.rs:2501` | once per swapchain, gated | `WxH fmt -> fourcc, N images; … minImages= presentMode= …` |
| `present mode: declared=` … `slot_waits=` | `proxy.rs:10547` | 10 s (`MODE_CENSUS_EVERY` `:263`) under `GNDRM_LOG_VKSHIM`, called per present from `present_dispatch` `:3114` | `acquires= commits= releases= gate_holds= slot_waits= commit_lat_us= spacing=[<15 … >20]` |
| census body | `slot_flags()` `:360`–`:408`, `counters()` `:411`–`:444` | on the lines above | `acq= pres= rel= commit= starved= in_flight= slots=[…] acqus=[n= sum= max= nz= ge8= ge16=] presiv=[…] cap=[fps= notes= gate_holds= gate_us= gate_avg_us= gate_max_us= acq_fps=]` |

Additions required for this item (all cheap, all on existing rate-limited
lines):

1. `proxy hb:` gains `rel= starved= in_flight= slots=[…]` from `slot_flags()`
   — one `shared` lock at 1 Hz. Without this, "starved is zero" is only
   inferable from the *absence* of a rate-limited stall line.
2. `slots_reap` counters: `reap_calls= reap_sent= reap_skipped=
   reap_round_trips=`, printed on the same 1 Hz line. `reap_round_trips`
   is the direct measure of (a).
3. The census gains `depth= guest_slots= spare_slots=` (on the existing
   `present mode:` line) so (c) is visible without a new gate.

Verification recipe, one 60 s run per arm, `GNDRM_LOG_VKSHIM=1` in the container
envVars (doc 24 §5.1):

```
# before: is the guest actually starving, and on which route?
grep -c 'gn-vkshim stall: acquire found no free slot'
grep -o 'starved=[0-9]*' | sort -u
grep -o 'slots=\[[^]]*\]' | sort | uniq -c | sort -rn | head
grep -o 'in_flight=[0-9]*' | sort | uniq -c
# depth actually granted to the guest (settles the 3-vs-4 question)
grep -o 'proxy swapchain .* images;' | grep -o '[0-9]* images'
# acquire latency distribution + slot waits, 10 s windows
grep 'present mode:' | grep -o 'slot_waits=[0-9]* commit_lat_us=[0-9]*'
grep -o 'acqus=\[n=[0-9]* sum=[0-9]* max=[0-9]*' # max/ge8 should collapse
# rate, from the guest's own cadence
grep -o 'acq_fps=[0-9.]*' | tail -3
# after (a): the batch is one per frame, not per slot per call
grep -o 'reap_round_trips=[0-9]* reap_calls=[0-9]*'
```

Pass criteria: `starved=` stays 0 and `slots=[…]` always contains a `-` slot
(free) at the 1 Hz sample; `acqus.max` drops from ≥8333 µs classes to <1 ms;
`reap_round_trips / reap_calls` ≤ 1/3 on the DRI3 route and 0 on the default
route; `proxy commit: … #N` delta matches `proxy acquire` delta within 1 %;
fps within noise of the X11 arm at equal GPU busy, with `cpu_sys` unchanged or
lower. If `starved=` is already 0 on the baseline, **item 3 is a no-op and
should be dropped**, and the sys delta attributed to B/E/H.

---

## 6. What not to do

- Do not raise `n_slots`/`guest_slots` to 4 "just for DXVK": that changes the
  number `vkGetSwapchainImagesKHR` reports and re-opens exactly the class of
  failure docs/23 exists to prevent.
- Do not use `(*slot).fence_sem` / the maintenance1 present fence as the
  release signal (S4).
- Do not let the acquire hand out a slot that is `in_flight` (the comment at
  `:2663`–`:2671` records the measured 8-commits-against-5334 failure of that
  shortcut).
- Do not add a second reaping thread on the wayland route; `notify_all` under
  `shared` is already the correct wake and a second reaper only adds contention.
- Do not enable (c) while frame generation is live (`spare_in_use` `:918`) or on
  a queue family other than the first present's (`:926`).
