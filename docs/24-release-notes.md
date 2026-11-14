# Release notes: how a buffer is reported free

The compositor's release and the display's release are different events. A
compositor hands a buffer back when it has finished *compositing* with it; the
display keeps scanning it out for a while after that — measured as 1.5-2.5 display
periods on Nova and 1-1.5 on Odin 3 (`sf hold`), so the size of the gap is a
property of the device and not a constant.
A producer that reuses the buffer on the compositor's release writes into memory
the panel is still reading, and the panel shows both frames at once for as long as
the two writes overlap. That is the tear Portal 2 showed under direct scan-out.

The shim therefore needs a second, later signal, and only the process holding
SurfaceFlinger's release fence can produce it. This document describes that
channel, the rule that pairs a note with the frame it describes, and what the
three devices actually do.

## The three messages

All three ride the scan-out channel (`SOCK_SEQPACKET`, magic-dispatched).

| message | direction | layout | carries |
|---|---|---|---|
| `PresentMark` | shim → daemon | `magic, reserved, ino, id, ticket` (32 B) | the scan-out inode, the guest's present id (0 when it chained none), and **the shim's own ticket** |
| `PresentDone` | daemon → shim | `magic, reserved, id, latch_ns` (24 B) | the panel's latch time for the marked frame |
| `BufferReleased` | daemon → shim | `magic, reserved, ticket, ino, latch_ns, source` (40 B) | **the ticket of the frame the buffer carried**, and what the release rests on |

`source` is one of `RELEASE_FENCE` (the fence signalled), `RELEASE_FENCE_TIMEOUT`
(it did not, within the watcher's 500 ms budget, so the buffer was reported
anyway), `RELEASE_POOL_COPY` (the pool copied in, so nothing outside holds it) or
`RELEASE_UNWATCHED` (no subscription was watching).

`BufferReleased` was the only one of the three without the ticket for most of its
life, and six mechanisms existed to compensate — inode matching, an order
fallback with a 5 ms reorder window, a one-display-period floor,
`since = commit_ns.min(parked)`, a bound derived from the interval it governed, and
a liveness heuristic. One missing field, six fixes.

## Ownership is inverted

On API 33 an `AHardwareBuffer` cannot be obtained from a bare dma-buf fd, and
`ASurfaceTransaction_setBuffer` must be called by whoever allocated it. So the
daemon allocates and owns the scan-out buffers; the shim requests one over a Unix
socket (`serve_scanout`) and receives a dma-buf fd back, which gamescope renders
into exactly as it would into a dma-heap allocation. The registration key is the
fd's `stat.st_ino`, the same identity the GEM store dedups `PRIME_FD_TO_HANDLE`
imports by.

## The publish path

Shim sends a mark → daemon applies the ASC transaction → `replaced_ctx` points at
the buffer **this transaction replaced** → its SF release fence goes to
`watch_release()` → the `gn-release` thread waits (max 500 ms, at most 32 fences)
→ `report_release()` pushes one `BufferReleased` per fence. Every retirement path
funnels through `report_release`, so a note is never duplicated.

`watch_release` returns early, closing the fd, when `ino == 0` (a pooled copy,
which nothing outside holds) or when `!pushes::wants(CAP_RELEASE_NOTE)` — the
channel is opt-in per process and re-checked per fence, because the subscriber may
connect after the first buffers are already in flight.

## The pairing rule

A park is taken at commit for a synced, non-hidden slot, and is answered by the
first of:

1. **The ticket.** `take_released_ticket()` looks the note up by the commit id the
   shim minted. Exact, O(1), no window. This is the rule the protocol is for: the
   key is a number the process that owns the slot made up, so it cannot be
   ambiguous the way a dma-buf inode can, and it cannot be recycled into a live
   slot the way a freed buffer's inode can.
2. **The inode.** `take_released(ino, since)` — where the shim's slot inode and the
   daemon's registration inode are the same object.
3. **The order.** The oldest park by `commit_ns`, with a floor one display period
   before the commit, because the note that answers a park is emitted when the
   *next* frame latches and so legitimately predates the park.

   Neither fallback takes a note whose ticket is older than the park's own: that
   is a late note for an earlier use of the buffer, or the note of a commit that
   was never parked (a hidden slot, a generation destination), and the frame it
   describes says nothing about the parked one. A newer ticket still pays — a
   note can arrive before its own slot is parked, and the ticket arm only looks
   at parked slots.
4. **The bound.** `min(2 × commit spacing, 4 × panel period)`, floored at two
   periods.
5. **The fail-open.** `PARK_UNANSWERED` counts parks that expired without a note;
   at four in a row the slot is freed on its fence instead, which is the behaviour
   from before the park existed. A `BufferReleased` resets it, so a producer that
   comes back needs no probation timer. Liveness is keyed on `BufferReleased`
   alone — keying it on the socket let RP6 keep parking, because that device sends
   ~5 000 `VsyncNote`s while releasing nothing.

## What each device actually does

Measured with Dave the Diver at 1 280 × 720, 60 Hz panel, cap 60, `fifo`.

| | Nova (Adreno 740) | Odin 3 (Adreno 830) | RP6 (Adreno 740) |
|---|---|---|---|
| notes per session | ~1 per frame (9 528) | ~1 per frame | 17-198 per session |
| `sf mark:` | **hit=4 736, order=1, miss=2, skew=0** — 1:1 with the notes | **hit=5 440, order=1, miss=2, skew=0** | not re-read after the buffer fix; the pre-fix session printed one line in 115 s (`hit=0 order=1 miss=2 last=1`) — see the open question below |
| `tick=` (notes carrying the ticket) | **5 168** of 4 627 | **1 760** of 1 793 (in game) | **15 of 16** after the buffer fix, 0 before |
| pairing | **ticket, with `by_order` frozen at 1 211 across the last 240 frames** — identity on Portal 2 took 6 % order, Dave took 84 % before the ticket | **`by_order` rising (971 at 54 % of acquires) but each answered in ~110 us** — the fallback is load-bearing and cheap | none |
| `channel=` | `unanswered:0 dead:false` | `unanswered:0 dead:false` | `unanswered:6 dead:true` |
| fps, menu / in game | 60 / **60.0** | 60 / **59.7** (`p95=17.2 ms`, `slow=0/118`, `repeated=0`) | 60 / **59.8** |
| `sf hold:` | prints, 1.5-2.5 periods | **76 % at 1-1.5 periods, 22 % at 1.5-2.5**, `timeouts=0` (n=7 442, in game) | **silent** — `latch_ns` is 0 |
| `park_timeouts` / acquires | 114 / 4 739 | — | 7 / 5 186 |

All three rendered the menu at 60 fps, and the row above is menu / in game. For
context against earlier work: Nova held 59.5 before the park was fixed, Odin 3 sat
at 3 fps while the park's bound fed on the interval it governed, and RP6 was 35.9
with no notes to answer a park.

Where the shim actually runs, the mark resolves by identity almost exclusively —
`order=1` on Nova and `order=2` on Odin 3 against thousands of hits — so the mark
path is sound and the ticket it carries is the frame's own.

Three failure points, and they are independent:

- **the watcher may not be armed** — RP6 sends notes but `latch_ns` is 0, so
  `note_hold` never runs and `sf hold:` never prints; `replaced_ctx` is reached
  sometimes (198 notes) but without a latch time;
- **the inode may name another buffer** — Odin 3 measured `slot ino=11411` against
  `released=[11366 11368 11370]`;
- **the note may predate the park** — the order arm's floor exists for this, and
  Nova-on-Dave needed it for 84 % of parks *before* the ticket; after it, `by_order`
  stopped moving altogether.

A fourth is silent and looks like all of them: no subscriber
(`wants(CAP_RELEASE_NOTE)` false).

The inode is therefore a property of the *title's* allocation pattern, not of the
device: the same Nova paired 94 % of Portal 2's parks by identity and 16 % of
Dave's. Any rule that depends on it is a rule that works until a different game
runs.

## Two defects found by measuring this

**`park_timeouts` counted loop turns, not parks.** The bound was re-evaluated on
every pass of the acquire loop over a slot it had already decided, so RP6 read
`park_timeouts=14 544` against 14 679 acquires while the total slot wait was 14.5 s
— under a millisecond per park it claimed to have timed out. `GnSlot.park_expired`
now answers each park once.

**A 24-byte read buffer truncated a 32-byte mark.** The daemon read the channel
into `[0u8; ScanoutAllocReq::LEN]` (24). `SOCK_SEQPACKET` truncates a frame to the
receive buffer and discards the rest, so after `PresentMark` grew to carry the
ticket every mark arrived truncated, `decode` returned `None`, and the marks never
reached `note_present_mark` — `sf mark: hit=0 order=0 miss=3 pending=0` with
`tick=[n:0]` on every line, while both ends looked correct. The reader now uses
`wire::INBOUND_MAX`, derived from the inbound types, and a wire test fails the
build if a new frame does not fit.

## Reading the census

**It needs `GNDRM_LOG_VKSHIM=1`, and a session without it looks broken.**
`present mode:` is printed twice: `logi!` emits the header unconditionally, and
`logf!` emits the census — `acquires=`, `tick=`, `channel=`, `by_order=`, every
figure in this table — only when that variable is set. A device without it produces
a short `present mode:` line and no `proxy commit:` lines at all, which is
indistinguishable from a shim built before the census existed. Odin 3's first two
sessions were misdiagnosed that way, including once as "the payload landed after
launch" on timestamps that did not support it: the process was new, the `.so` was
mapped, and only the variable was missing.

Odin 3 also runs a driver pack rather than the system driver
(`files/contents/adrenotools/Turnip Gen8 V30/libvulkan_freedreno.so`), which is
worth knowing when comparing its numbers with the other two.

### A rate comes from two adjacent lines, not from a counter

Odin 3's first in-game reading looked like a failure — `PowerMetrics fps=36.6`,
`p95=101 ms`, `repeated=20`, and `by_order` climbing through 54 % of acquires. It
was the loading transition: the same ten-second window contains `presiv
max=6 496 260`, a 6.5 s stall while the scene loaded, and `acq_fps=36.03` is a
lifetime average that the stall drags down for the rest of the session.

The steady state, one loading transition later:

    PowerMetrics: fps=59.7  p95=17.2ms  slow=0/118  repeated=0  cpu=21% gpu=67%
    intervals:    120 acquires / 2.003 s = 59.9
    slot growth:  13 224 us per 120 frames = 110 us/frame
    tick=1 760 of rnotes=1 793   channel=[unanswered:0 dead:false]

So `by_order` climbing is **not** a defect signal by itself: those parks are
answered in about a tenth of a millisecond, where Nova's ticket-answered parks cost
63 us. The number that looked alarming — `slot_avg_us=1171` over 89 % of frames —
belongs to the loading phase and decays as the window fills.

`acq_fps`, the `acqus` means and the `presiv` means are all lifetime figures, so
any session with a startup stall reads low long after the stall is over. Take the
rate from two adjacent lines.

### What the ticket bought, measured

Nova, Dave, same payload and scene, before and after:

    before  by_order=3 640 of rnotes=4 328    (84 % of parks)    slot_avg_us=37
    after   by_order=1 211 and frozen         (0 in steady state) slot_avg_us=63
            sf mark: hit=4 736 order=1 miss=2 skew=0
            tick=5 168 of rnotes=4 627, off=0
            fps 60.0, park_timeouts 114 of 4 739 (2.4 %)

`by_order` not moving across 240 consecutive frames is the result that matters on
that device: the order arm is cold there, so for Nova the identity and order paths
are already dead code.

## What is still open

- **RP6 has no latch time.** It runs 60 fps, but no session has produced a
  `sf hold:` sample, so how long SurfaceFlinger holds a buffer there is unknown.
  Whether its output is clean or merely fast has not been measured; the fail-open
  hides it. This is the only gap that is a defect rather than a tuning question.
- **The order arm is still load-bearing on Odin 3** — `by_order` at 54 % of
  acquires, each answered in ~110 us. It cannot be deleted with Nova's evidence:
  on Odin the ticket lookup misses often enough that the fallback carries half the
  parks, which is a timing relationship between park and release that Nova does
  not have. Deleting the arms removes a cost this device does not show.
- **The daemon's counters do not reconcile on RP6**, though they do on Nova
  (4 736 lookups against 4 627 notes at the same instant). RP6 printed one
  `sf mark:` line in 115 s — a single ticket ever assigned — while 15 notes arrived
  carrying tickets. Both cannot be true with `ctx.ticket` written only from
  `take_present_mark`. The cause is likelier to be that RP6 publishes almost
  nothing through the ASC path than a defect in the counter, but that is a
  hypothesis, not a measurement, and a publish counter would settle it.
- **A pooled copy produces no note at all** (`ino == 0` returns early). That is a
  known gap: `RELEASE_POOL_COPY` exists in the wire and nothing sends it.
- **The shim's `by_order` conflates two causes** — a note naming another buffer,
  and a note older than the anchor. They need opposite fixes, and one counter
  hides which is happening.
