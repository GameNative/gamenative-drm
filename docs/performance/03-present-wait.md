# 03 — `VK_KHR_present_wait`: what the shim answers, and where it could answer sooner

Design for `docs/01-gamescope-vs-x11-cpu.md` §5 item 2. Branch `performance-report`,
tree as it stands. Read-only analysis; nothing here has been built or measured.

**Verdict up front.** The change is small (one decision function, one release-callback
unification, one event source on the DRI3 route) and it is worth doing, but the premise
as written in `docs/performance/01` — "the shim only produces the answer when *some other thread*
runs the reap loop, a wait that can degenerate into busy polling" — **does not match the
code**. A present wait today is a `Condvar` under `present::WAITS`
(`present.rs:72`, `:181`, `:435`), not a poll: it blocks, and it is woken by whichever
thread produces the answer. There is no spin inside `present::wait`. The polled loop the
report is thinking of is `x11_reap` (`proxy.rs:674`) called from the acquire gate every
`REAP_SLICE = 4 ms` (`proxy.rs:2657`, `:2785`–`:2804`), and it is a *no-op* on the
gamescope/override route (`x11_reap` returns immediately when `surf.x11` is null,
`proxy.rs:675`–`:678`; that pointer is only set on the DRI3 route, `proxy.rs:9778`).
So item 2 as scoped cannot be the source of +1.6 pts sys, and §6 below gives the sizing
that says so, plus the experiment that decides it before any code moves.

What item 2 *does* buy: (a) it deletes a per-frame cross-thread dependency (guest blocks
until the present worker has waited the GPU fence and committed), which is the mechanism
`docs/15` §3 already documents costing 42 fps on a latency-1 guest; (b) it is the
precondition for item 3 (event-driven release), because a wait that never blocks cannot
be starved by a missed wakeup; (c) it makes a polling caller cost one call, not a spin.

---

## 1. What the tree does today, exactly

### 1.1 Advertisement

The guest's swapchain is the shim's, so the names are the shim's:

```rust
// wsi_policy.rs:65-66, inside `SERVED`
(crate::present::EXT_PRESENT_ID, Served::Shim { spec: 1 }),
(crate::present::EXT_PRESENT_WAIT, Served::Shim { spec: 1 }),
```

`Served::Shim` means *the driver's copy is dropped and the shim's appended*
(`wsi_policy.rs:127`–`:129`, `:133`–`:138`). The v2 pair is hidden with a reason
(`wsi_policy.rs:92`, `:94`), and the pinned device list is
`present_id, present_wait, swapchain_maintenance1, EXT_swapchain_maintenance1`
(`wsi_policy.rs:313`–`:321`). Features are the shim's answer, not the driver's:
`present::features2` patches
`VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PRESENT_ID_FEATURES_KHR` / `..._WAIT_...` to true
(`present.rs:502`–`:531`), and `gn_vkCreateDevice` records what the guest actually
enabled before stripping the structs from the driver's chain:

```rust
// proxy.rs:11130-11136
let guest_id   = names.iter().any(|&o| unsafe { CStr::from_ptr(o) } == crate::present::EXT_PRESENT_ID);
let guest_wait = names.iter().any(|&o| unsafe { CStr::from_ptr(o) } == crate::present::EXT_PRESENT_WAIT);
crate::present::note_enabled(guest_id, guest_wait);
```

`present::enabled()` is the OR of the two (`present.rs:234`–`:236`) and
`gn_vkWaitForPresentKHR` refuses with `VK_ERROR_EXTENSION_NOT_PRESENT` when it is false
(`proxy.rs:3552`–`:3554`). The command is always served from the shim's dispatch table
(`proxy.rs:5363`–`:5368`) and `vkWaitForPresent2KHR` is refused by the policy
(`wsi_policy.rs:150`–`:159`).

### 1.2 Arming

Per present, per swapchain index, the id comes out of the guest's `VkPresentIdKHR`
(`present.rs:451`–`:492`), is stashed on the slot twice and armed:

```rust
// proxy.rs:3341-3347
if present_id != 0 {
    unsafe { (*slot).present_id = present_id; (*slot).wait_id = present_id; }
    crate::present::arm(present_id, sc as usize);
}
```

Two fields on purpose (`proxy.rs:7663`–`:7670`): `present_id` is the mark sent to the
daemon at commit so it can report the real latch time; `wait_id` is what a wait is
answered against, and it is *consumed* by whichever path answers first.

### 1.3 Where a wait is answered — the current table

```rust
// proxy.rs:3097-3110
fn present_wait_answer(gamescope_route: bool, why: Declared,
                       requested: VkPresentModeKHR, live: VkPresentModeKHR) -> WaitAnswer {
    if gamescope_route && why == Declared::CapRateVsync { WaitAnswer::AtQueue }
    else if answers_present_wait_at_commit(requested) || answers_present_wait_at_commit(live) { WaitAnswer::AtCommit }
    else { WaitAnswer::Later }
}
```
with `answers_present_wait_at_commit(m) = mailbox || immediate` (`proxy.rs:10280`–`:10282`).

The three points, precisely:

| point | site | modes | what it claims |
|---|---|---|---|
| **queue** | `present_dispatch` `proxy.rs:3122`–`:3133`, via `present::answered_at_queue` (`present.rs:208`) | only `Declared::CapRateVsync` (cap at the display rate, container non-FIFO, guest asked FIFO) on the override route | nothing about display; the guest is told "presented" the instant it queued |
| **commit** | `commit_real` `proxy.rs:2057`–`:2079` → `present::committed(..., answer_now=true)` (`present.rs:194`–`:200`) | mailbox/immediate, either requested or live (container mode, frame generation) | the guest's wait semaphores were consumed and the GPU fence for the frame has signalled |
| **later** (release / feedback / grace) | `present::expect_feedback` at `proxy.rs:2077`; `complete_on_release` from `on_buffer_release` `proxy.rs:8079` or `x11_reap` `proxy.rs:703`; `wait()`'s grace at `present.rs:395`–`:422` | FIFO and FIFO-relaxed | for the override route, gamescope's `wp_presentation_feedback.presented`/`discarded`; otherwise the buffer release, else `commit + 2 periods` |

Because present ids restart at 1 per swapchain, everything is keyed `(id, swapchain)`
(`present.rs:49`–`:63`), and a destroyed swapchain drops both its armed and its
unconsumed answers (`present.rs:339`–`:353`).

### 1.4 The waitable object

There is none in the Vulkan sense. The waitable object is a process-global mutex+condvar
pair, `static WAITS: Mutex<Waits>` (`present.rs:72`) and `static WAIT_CV: Condvar`
(`present.rs:181`); every answer calls `WAIT_CV.notify_all()` (`present.rs:178`, `:291`).
`wait()` (`present.rs:359`–`:443`) is a loop under that mutex with three exits: a `done`
entry for `(id, sc)` (`:375`–`:383`), "not armed" ⇒ success (`:384`–`:391`, i.e. an id
the shim never saw presented), or the grace deadline reached (`:405`–`:422`). Its
`timeout == 0` case returns `VK_TIMEOUT` on the first pass (`:423`–`:429`), so a polling
caller does not sleep — it *spins in the guest*, which is the one shape of "busy polling"
this design does exist for, and it is a caller property, not a shim property.

Consequences that matter for the edit:
- `record_for` (`present.rs:275`–`:292`) removes from `armed` and pushes to `done`, so
  **any** later answer for that id is a no-op (`:280`–`:282` early-returns when the id is
  no longer armed). Early answering is therefore already idempotent with the commit,
  release, discard and feedback paths; the comment at `proxy.rs:3123`–`:3125` says so.
- `done` is bounded at `CAP = 64` and an unconsumed answer can be evicted
  (`present.rs:70`, `:286`–`:289`). A wait that finds neither `done` nor `armed` returns
  **success** (`:384`–`:391`). So an over-eager early answer degrades to "presented", not
  to a hang — but it also means the shim cannot distinguish "answered" from "evicted".
- `forget_swapchain` drops answers for that swapchain only (`present.rs:348`).

### 1.5 Who frees a slot, and how

Two routes, two mechanisms, one handshake field (`in_flight`, always cleared under
`(*sc).shared`, always with `cv_free.notify_all()`):

- **override/Wayland (default)**: `on_buffer_release` (`proxy.rs:8049`, listener installed
  per slot) — `in_flight = false; cv_free.notify_all(); wait_id → complete_on_release`
  (`proxy.rs:8069`–`:8080`). Event-driven already (Wayland dispatch thread, `dispatch_main`).
- **DRI3 + Present (`GN_VKSHIM_GUEST_SURFACE=1`)**: a per-slot X Sync fence, polled by
  `x11_reap` (`proxy.rs:674`–`:713`) with `xcb_sync_query_fence` **for every slot**
  (`x11.rs:1170`–`:1188`). Callers: the worker's idle tick (`proxy.rs:1949`), `commit_real`
  after the present (`:2094`), the inline sync path (`:3042`), and the acquire gate's
  starve loop every 4 ms (`:2786`, `REAP_SLICE` at `:2657`).

The DRI3 idle-fence requirement is real and must survive: the server owns the pixmap until
the fence triggers, and `x11_present_slot` (`proxy.rs:576`) is the only thing that emits
the present. `slot_release` frees the pixmap and the fence at destroy (`proxy.rs:11636`).

### 1.6 Where the daemon's latch report arrives

`commit_real` sends `PresentMark{ino,id}` (`proxy.rs:2031`–`:2036` → `scanout.rs:679`), and
`PresentDone` comes back on the same SEQPACKET channel. It is drained in three places —
the subscription handshake (`scanout.rs:308`), inside the allocation exchange
(`scanout.rs:558`), and **one datagram per buffer release** via `poll_notes()`
(`proxy.rs:8060` → `scanout.rs:687`–`:709`, single `recv`). That single-datagram drain is
why a PresentDone can sit in the socket for a frame or more; it is irrelevant once the wait
is answered at queue, and it is the reason the *census* of `presented=`/`fallback=` can
lag.

---

## 2. The contract if the shim answers at queue time

Spec text (Vulkan docs project, `vkWaitForPresentKHR` and `VK_KHR_present_wait`):

- "The call to `vkWaitForPresentKHR` will block until either the presentId associated with
  `swapchain` is greater than or equal to `presentId`, or `timeout` nanoseconds passes."
- "There is no requirement for any precise timing relationship between the presentation of
  the image to the user and the update of the presentId value, but implementations should
  make this as close as possible to the presentation of the first pixel in the next image
  being presented to the user." (Issue 1 repeats it: "The wait will finish when the present
  is visible to the user. There is no requirement for any precise timing relationship…".)
- Mailbox replacement: "any wait of this type associated with such an image must be
  signaled no later than a wait associated with the replacing image would be signaled."
- Out-of-date: on a retired swapchain the call returns `VK_SUCCESS` (if the image reached
  the presentation engine) or early `VK_ERROR_OUT_OF_DATE_KHR`; VUID-…-04997 forbids
  calling it on a retired swapchain at all.
- Issue 4 resolves out-of-order completion with OPTION B: finishing both waits when the
  earliest present completes "will complete the later present wait earlier than the actual
  presentation … allowed", which is the spec's own blessing for coalesced/early answers.

So, what the shim may and may not claim when it answers at queue:

1. **Legal.** Answering at queue violates no valid-usage rule. The timing clause is a
   *should*, and the spec explicitly contemplates the presentId being updated earlier than
   the presentation. `VK_SUCCESS` at queue means "this present id is satisfied", which the
   shim can truthfully say about a present it has accepted and whose image it owns.
2. **It may not be a lie about the *swapchain's* state.** The contract is monotone in id
   (`>= presentId`). The shim today answers by *exact* `(id, sc)` match
   (`present.rs:375`–`:378`, `:280`) and has no notion of a per-swapchain completed id. Two
   things follow: a wait on an id *lower* than one already answered must also succeed (it
   does today only because `arm` retains lower ids and they are answered separately — with
   queue-time answering every id is answered in order anyway, so the property is preserved
   by construction, but §3.5 turns it into an explicit invariant and a test); and ids the
   guest never presented must not be reported as presented unless we keep the existing
   deliberate deviation (`present.rs:356`–`:358`: an unarmed id is answered as presented to
   avoid a hang on a swapchain handle mismatch). Keep that, and log it.
3. **It breaks the *pacing* function for callers who use it as a throttle.** This is the
   honest cost. DXVK's latency limiter and vkd3d-proton's present task call
   `vkWaitForPresentKHR` precisely to bound frames-in-flight; at queue the call returns
   immediately, the throttle disappears, and the bound moves to the swapchain's slot count
   (the guest blocks at `vkAcquireNextImageKHR` instead, `proxy.rs:2722`–`:2805`). Legal,
   but: queue latency rises to `slots − 1` frames (`docs/15` §"Queue latency"), and a title
   whose frame loop assumes the wait is slow will report a present that "took 0 µs" to any
   in-game stats. Mesa's X11 WSI does not advertise the extension at all, so the guest
   never makes this trade on the X11 arm — which is exactly why item 2 is asymmetric.
4. **It costs the measurements in `docs/15` §3.** `commit_latency_ns` smoothing
   (`proxy.rs:2019`–`:2026`) measures acquire→commit, not acquire→present, so it survives;
   but `at_queue=` (`proxy.rs:10564`) stops distinguishing CapRateVsync from everything
   else, and the "answered two vblanks late" fallback counters (`fallback=`,
   `presented=`, `discarded=`, `proxy.rs:10547`) go to zero because the release path no
   longer answers anything. The census needs a replacement (see E13) or `docs/15` becomes
   unfalsifiable.
5. **It must not fake `VK_ERROR_OUT_OF_DATE_KHR`.** Answering early must not change the
   destroy path: `forget_swapchain` still wakes waiters with success
   (`present.rs:339`–`:353`) and `gn_vkWaitForPresentKHR` still resolves the handle first
   (`proxy.rs:3555`–`:3558`).

---

## 3. The change

### 3.1 Decision function (E1)

`proxy::present_wait_answer` (`proxy.rs:3097`) becomes policy-driven:

```
fn present_wait_answer(route, why, requested, live) -> WaitAnswer
    match wait_policy() {                    // OnceLock, GN_VKSHIM_PRESENT_WAIT
        Queue   => AtQueue,                  // default
        Commit  => if answers_at_commit(requested) || answers_at_commit(live) { AtCommit } else { Later },
        Release => Later,
    }
```

with `wait_policy()` reading `GN_VKSHIM_PRESENT_WAIT ∈ {queue, commit, release}`, default
`queue`, parsed once at first present (the shape `parse_present_mode_override` used
before `refactor/vsync` deleted it). `route`, `why`, `requested`, `live` stay in the signature so the
`commit`/`release` arms remain exactly today's behaviour and the existing table test
(`proxy.rs:7124`–`:7167`) keeps its meaning by being run under `Commit`.
Reason to keep three arms: §4 risks 1, 3 and 5 all need a per-container revert without a
rebuild, and the container already carries env (`docs/performance/01` §"Log gates").

### 3.2 Queue-time consumption (E2)

No structural change: `present_dispatch` already takes `wait_id` off the slot and calls
`present::answered_at_queue` **before** it hands the slot to a discipline function
(`proxy.rs:3122`–`:3133` then `:3137`–`:3146`), which is the ordering that makes
`in_flight` irrelevant to the answer. Two additions:

- take the id and answer under the same `(*sc).shared` lock acquisition that reads it
  (already the case, `:3126`–`:3132`), and
- leave `(*slot).present_id` alone, so the daemon mark at commit still fires
  (`proxy.rs:2031`–`:2036`) and the latch/latency accounting survives.

### 3.3 Release path becomes one function (E5, E6)

Extract the tail of `on_buffer_release` (`proxy.rs:8069`–`:8080`) and of `x11_reap`
(`proxy.rs:695`–`:704`) into

```
unsafe fn slot_release_wake(sc, slot, src: ReleaseSource)   // Wayland | X11Idle | Drop
```
which does exactly: under `shared` → `in_flight = false`, `cv_free.notify_all()`,
`mem::replace(&mut wait_id, 0)`; outside the lock → `complete_on_release(wait_id, …)` if
non-zero, then the per-source census (`N_REL`, `GEN_RELEASES`, `REL_ALL_N`). All four drop
paths (`proxy.rs:759`, `:2103`, `:2117`, `:3225`) call it with `src = Drop` so the
accounting of a dropped frame is no longer a fourth copy. Invariant: **`in_flight` is
cleared in exactly one function**, and the answer — if the policy is `release`/`commit` —
is produced there.

### 3.4 Event-driven release on the DRI3 route (E7, E8, E9)

Keep the idle fence as the authority; change *when* it is consulted.

- `x11::X11Present::select_idle(&self)` → `xcb_present_select_input(conn, window,
  XCB_PRESENT_EVENT_MASK_IDLE_NOTIFY)` on the target window, plus `event_fd()` =
  `xcb_get_file_descriptor(conn)` and `next_event()` = `xcb_poll_for_event` (freeing the
  event). Present event selection is per-client, so selecting it does not steal events from
  wine. This is new surface on `X11Present` (`x11.rs:797`–`:830`), which today is
  request/reply only: `check_err` at `x11.rs:820`.
- `GnSlot` gains `x11_serial: u32`, set in `x11_present_slot` from `present()`'s return
  (`x11.rs:1124`–`:1165` returns the serial; the call site is `proxy.rs:648`–`:650`).
  `x11_reap` becomes `x11_reap_slot(sc, i)` — one slot, one `fence_idle` — used (a) by the
  new event thread, (b) once per acquire when the slot the guest is about to be given is
  still `in_flight`.
- New thread `gn-x11-events`, spawned next to the present worker (`proxy.rs:2476`–`:2488`)
  and joined in destroy with it: `poll(event_fd, POLLIN)` → drain events → for
  `IdleNotify{serial}` map serial→slot index → `fence_idle(slot.xfence)` (**the
  requirement, kept**) → `slot_release_wake(.., X11Idle)`. `serial == 0` in IdleNotify
  means "idle in the current state" — treat it as "check every in-flight slot of this
  swapchain once", which is one round trip per such event, not per slot per 4 ms.
- The acquire gate drops the 4 ms poll loop (`proxy.rs:2785`–`:2804`) and blocks on
  `cv_free` with a watchdog slice only (`WD_SLICE`, 250 ms, whose timeout re-runs one
  `x11_reap` scan and is logged once at `loud()` level). `REAP_SLICE` (`proxy.rs:2657`) is
  then used only by the worker's idle tick.

### 3.5 PresentDone drain (E12)

Add `present::drain_answers()` / keep `poll_notes` as-is, and make the *release* caller call
a bounded loop (`scanout::drain_notes(max = 32)`) that only consumes frames which decode as
`PresentDone`/`FrameCapNote`/`VsyncNote`/`LsfgConfig`; the first frame that does not decode
as a note is left in the socket and the loop stops — so an in-flight exchange's reply is
never eaten. This fixes the "one note per release" stall (`proxy.rs:8060`) without touching
the exchange protocol (`scanout.rs:535`–`:565`).

### 3.6 Monotone id rule in `present::wait` (E3)

Add, before the exact-match test (`present.rs:375`), a scan of `done` for an entry of the
same swapchain with `d > id`; if present, return success without consuming it. This makes
the queue-time answers obey `completed_id >= presentId` even when the guest waits on an id
it skipped. Keep the exact-match consume for the common case. Keep `MAX_WAIT_NS = 1 s`
(`present.rs:365`) — with queue-time answers it is unreachable, and its existence is the
guarantee that a missed answer still cannot hang the session.

### 3.7 What must NOT change

- `in_flight`/`acquired` handshake and its single lock; `cv_free` notified on every clear.
- `present::arm` before dispatch and `record_for`'s "already consumed" early return
  (`present.rs:280`–`:282`) — this is what makes early answering safe against the commit,
  release, discard, feedback and `forget_swapchain` paths.
- `(*slot).present_id` → `scanout::send_present_mark` at commit (`proxy.rs:2031`–`:2036`);
  one mark per frame, cleared after sending.
- maintenance1: present fences, the second empty submit, `fence_sem`
  (`proxy.rs:3366`–`:3428`), `maint1::present_mode_request` handling (`:3313`–`:3326`).
- The cap: `acquire_pacing` / `release.book_paced` (`proxy.rs:2713`, `:2757`) and
  `commit_latency_ns` smoothing (`:2019`–`:2026`).
- Frame-ring counters: `ACQ_COUNT`, `send_frame_acquired` (`proxy.rs:2867`–`:2874`).
- The mode census (`proxy.rs:10504`–`:10570`) — extend it, never redefine `declared=`/
  `source=`/`gate=`.
- `x11::fence_idle` and `slot_release`'s pixmap/fence destruction (`proxy.rs:11636`).
- `present::features2` answers: still true for both features (hiding them again is a
  different change with a different risk).

### 3.8 New census (E13) — required, or the change is unmeasurable

On the `present mode:` line (`proxy.rs:10547`–`:10564`), add
`wait_n= wait_blocked= wait_us_max=` from `present`: `wait_n` (calls), `wait_blocked` (calls
that had to enter the condvar — the number item 2 exists to drive to zero for a
queue-time policy), and the max observed block time. Keep `at_queue=`. Without
`wait_blocked`, "the per-frame wait is gone" is only assertable from CPU numbers, which
§6 says are confounded.

---

## 4. Risks and mitigations

1. **A title that uses present_id for frame pacing** (DXVK latency limiter, vkd3d-proton
   present task). At queue the throttle vanishes; frames-in-flight rises to `slots − 1` and
   latency follows (`docs/15` §"Queue latency"). *Mitigation:* `GN_VKSHIM_PRESENT_WAIT=commit`
   reverts per container; measure p99 frame time on a cap-limited scene before shipping the
   default; do not pair this change with an increase in depth (item 3) in the same build.
2. **The DRI3 route.** A server without Present `IdleNotify` (or an event we fail to parse)
   would never free a slot. *Mitigation:* `select_idle` is version-checked against
   `present_version` (`x11.rs:891`–`:893`); on failure keep the old `x11_reap` scan on the
   watchdog slice only (250 ms), and the acquire-side single-slot check still frees the slot
   the guest is asking for. The fence is still the authority, so a missing event costs
   latency, never correctness.
3. **Multi-swapchain.** Ids are per swapchain and `present_dispatch` runs per index inside
   one `vkQueuePresentKHR` (`proxy.rs:3297`–`:3456`); a queue-time answer must be per
   `(id, sc)`, never per present call. *Mitigation:* E1/E2 keep the `(id, sc)` keying
   untouched; a test with two swapchains presenting alternating ids, asserting both waits
   are answered and neither swapchain's `done` entry is consumed by the other
   (`present.rs:689` already tests the recreate case).
4. **Suspend/resume.** Frames queued before a suspend may lose their daemon `PresentDone`
   (the channel is per session). Today that is caught by the grace and the 1 s cap; at queue
   it is moot. *Mitigation:* none needed; assert `wait_blocked == 0` across a
   suspend/resume cycle as part of acceptance.
5. **The fps cap path.** With no present-wait throttle the guest can run ahead; the cap is
   applied at acquire (`book_paced`), so the *rate* holds, but `gate_holds`/`gate_late` and
   the `spacing=[…]` histogram will shift and a `cap-rate-vsync` session loses the overlap
   behaviour `docs/15` §3 documents as its reason to exist. *Mitigation:* measure
   `gate_holds`/`gate_late`/spacing on a capped scene; if the cap degrades, keep
   `CapRateVsync → AtQueue` as today and move only the other modes (the function already
   distinguishes them, so this is a one-line policy arm, not a redesign).
6. **Census regression** (risk 4 of §2): `presented=/discarded=/fallback=` go to zero and
   `docs/15`'s "answered two vblanks late" claim becomes untestable. *Mitigation:* E13, and
   a note in `docs/15` §3 that the release answer is only reached under
   `GN_VKSHIM_PRESENT_WAIT=release` after this change.

---

## 5. Falsification experiment — before any code

No rebuild: container `dxvk.conf` / env only, plus the existing `GNDRM_LOG_VKSHIM=1`
(docs/performance/01 §"Log gates" — it reaches the guest from container `envVars`).

**Knobs.** In the container's `dxvk.conf` (or `DXVK_CONFIG="…"` if the bundled DXVK is
2.3+, else `DXVK_CONFIG_FILE`):

```
dxvk.maxFrameLatency = 0      # 0 disables the latency limiter that waits on present ids
dxvk.latencySleep   = False   # disables the present-wait-driven sleep
```
Control arm: same session with both unset. Second arm (isolates C):
`dxvk.numBackBuffers = 4` and the latency knobs unset.

**Instrument (guest side, one 60 s run per arm, same scene):**
`proxy present: first pNext chain sTypes=… present_id=` (`proxy.rs:3173`–`:3177`) — is
`VkPresentIdKHR` in the chain at all; `present wait #N: id=… {done|unarmed|latched|timeout}
after N us` (`present.rs:296`–`:307`) — how often and how long; `at_queue=` and
`slot_waits=`/`starved=` from the `present mode:` line (`proxy.rs:10547`–`:10564`);
`proxy hb: acq=/pres=/starved=`.

**Readings and what each means.**

| observation | conclusion |
|---|---|
| Wait lines present per frame, block ≈ frame time, CPU(sync) does **not** move when the knobs are set | **B refuted at the shim**: the wait is real but cheap (2 futex ops), and the +1.6 pts is A (submits/commits) and/or the daemon legs |
| Wait lines show `timeout` repeatedly, or wait frequency ≫ frame rate | **B confirmed in its strong form** (guest polling): queue-time answering collapses it to one call per wait; expect sys to fall toward X11 |
| Knobs on → CPU(sync) → X11 **and** fps unchanged, but `starved=`/`slot_waits=` rise | B confirmed as *coupling*, C unmasked: apply item 2 then item 3, in that order, and expect the win only after both |
| Knobs on → no change; `numBackBuffers=4` → CPU falls and `starved=` → 0 | **B refuted, C confirmed** (depth/release-gated slots, docs/performance/01 §4 C). Item 2 is then only worth its cost as item 3's precondition |
| Both arms flat, `starved=0`, waits ≈ frames | B and C both refuted: the delta is A (per-present submits, wayland attach/flush, PresentMark datagram) — go to items 1/3/5 in docs/performance/01 §5, not item 2 |

**Size test that must be run with it** (this is what makes the experiment decisive rather
than suggestive): +1.6 pts of 8 cores at 344 fps is `0.128 / 344 s ≈ 372 µs` of CPU per
frame. A blocking present wait costs two futex operations plus two uncontended mutex
acquisitions — order 2–5 µs, i.e. ≤1.5 % of the delta — and a polling caller is the only
shape that can exceed it. So an arm that removes the wait and leaves CPU *within noise* is
not a failed experiment: it is the refutation of B as the cause, and it should be reported
as such before the code change is justified by effect rather than by tidiness. Run it, log
`wait_blocked` (E13 is additive and can be landed first on its own), and only then decide
whether item 2 ships as a fix or as item 3's precondition.

---

## 6. Acceptance criteria for the change

- `wait_blocked == 0` and `at_queue == wait_n` for a mailbox/immediate session at the
  container default (queue policy).
- `presented=`, `discarded=`, `fallback=` no longer grow; `releases=` still grows at the
  frame rate (the release still happens; it just no longer answers anything).
- A capped session keeps `gate_holds`/`gate_late` and the `spacing=` histogram within
  noise of the current build, and fps within 0.5 %.
- DRI3 route (`GN_VKSHIM_GUEST_SURFACE=1`): `starved=` unchanged or lower, no
  `acquire blocked >=1s` line (`proxy.rs:2892`–`:2894`), and no slot freed without
  `fence_idle` true.
- Swapchain recreate / VSync toggle loop (docs/15 §5 "six toggles") with no
  `slot init failed` and no wait answered by the 1 s cap.
- `GN_VKSHIM_PRESENT_WAIT=commit` reproduces the pre-change census numbers exactly — the
  proof that nothing else moved.
