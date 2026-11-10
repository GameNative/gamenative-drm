# 06 — Present-mode declaration: why the per-frame re-send is load-bearing

Scope: the override/Wayland present route (the default, `use_guest_surface()` false,
`proxy.rs:9655`), per presented frame, as it exists on branch `performance-report`.
Read: doc 24 §4 F (`docs/01-gamescope-vs-x11-cpu.md:187-189`) and §5 item 5
(`:235-237`), docs 09/10/11/15, `icd_shim/src/proxy.rs` 615-800 / 1900-2184 /
2260-2300 / 2860-3071 / 3225-3380 / 7625-7760 / 7900-8035 / 9660-9760 /
10286-10605, and gamescope's `src/wlserver.cpp` + `src/steamcompmgr.cpp` in
`/data/workspace/Projects/gamescope`.

**Verdict up front.** The one per-frame wayland request that is *not* required by
the protocol's intent, `gamescope_swapchain.set_present_mode` (`proxy.rs:2137`), is
nevertheless **required by this gamescope payload's implementation**: `PrepareCommit`
clears `oCurrentPresentMode` after *every* commit (`wlserver.cpp:180`). So item 5 as
worded — "only call `set_present_mode` when it differs from the applied value" — is
**not a shim-only change and is incorrect if made alone**: it silently un-paces every
FIFO commit after the first. It becomes correct only with a two-file change (gamescope
retains the declaration, shim suppresses the repeat), given below. Its measured value
is **noise**: ~0.15-0.45 µs of guest CPU per frame against a gap of ~377 µs per frame
(§6). Recommend closing item 5 as "measured, not worth it" or landing it only as
hygiene, and spending the same effort on the two per-frame SEQPACKET datagrams
(§6.3), which are 5-10× larger and also not the gap.

## 1. The full per-frame sequence, classified

`P` = per presented frame on the override route. Wire sizes are the wayland message
headers (8 B: object id, size+opcode) plus 4 B per word argument.

| # | site | protocol request / syscall | when | bytes | class |
|---|---|---|---|---|---|
| 1 | `proxy.rs:1974-1987` | `vkWaitForFences` on the slot's render fence (sliced by `GEN_WAIT_SLICE_NS`) | every frame | — | (a) required |
| 2 | `proxy.rs:2031-2036` → `scanout.rs:679-685` | `PresentMark` datagram (SEQPACKET `send`) | when the guest chained a present id | 24 B + syscall | (a) required (the daemon's present-id channel; not wayland) |
| 3 | `proxy.rs:2047-2079` | `wp_presentation.feedback` (opcode 1) — registers the wait answer for a FIFO frame | only when `wait_id != 0` **and** the answer is `Later` **and** `PRESENTATION` is bound | 16 B | (a) required, already conditional (state: `feedback_token`) |
| 4 | `proxy.rs:2108-2121` | `zwp_linux_dmabuf_v1.create_params` + `add` + `create_immed` → `wl_buffer` | **first present of each slot only** (`(*s).buf` cache) | — | (b), already dirty-tracked |
| 5 | `proxy.rs:2136-2141` | `gamescope_swapchain.set_present_mode` (opcode 3) | every commit | 12 B | **(b) by intent, (a) by this payload** — see §2 |
| 6 | `proxy.rs:2142` | `wl_surface.attach` (opcode 1) | every commit (a different slot buffer each frame) | 20 B | (a) required |
| 7 | `proxy.rs:2143-2149` | `wl_surface.damage_buffer` (opcode 9) | every commit, full extent | 24 B | (a) required: damage is double-buffered surface state, consumed by the commit; gamescope drops a commit with no damage (`steamcompmgr.cpp:8533-8534`, `buffer_damage.extents` must exceed 2 px both axes) |
| 8 | `proxy.rs:2167` | `wl_surface.commit` (opcode 6) | every commit | 8 B | (a) required |
| 9 | `proxy.rs:2168` | `wl_display_flush` → one `write(2)` of the accumulated out-buffer | every commit (5 and 6-8 are already one write) | 64 B (80 with #3) | (a) required for latency; **the syscall is the cost, and it does not go away** |
| 10 | `proxy.rs:2266-2297` | `gamescope_swapchain.swapchain_feedback` (opcode 2) | once per `vkCreateSwapchainKHR` | 7 words + string | (b), already dirty-tracked (recreation is the dirty bit) |
| 11 | `proxy.rs:9715-9725` | `wl_compositor.create_surface`, `…create_swapchain`, `…override_window_content` | once per `VkSurfaceKHR` | — | (b), already dirty-tracked |
| 12 | `proxy.rs:3052-3067` | the same five requests as 5-9, on the **inline** `present_sync_commit` path (job queue full, `proxy.rs:2953`) | only under backpressure | — | same classes as 5-9 |
| 13 | `proxy.rs:767-776` (`commit_generated`) | attach + damage + commit + flush, **no `set_present_mode`** | per generated (LSFG destination) frame | 52 B | (a) for the commit; the *absence* of a declaration is load-bearing — see §5 |
| 14 | `proxy.rs:3113-3114` | `mode_census()`: one global `Mutex<Option<ModeLine>>` + struct compare + `live_choice()` | every present | — | **(c) ceremony** — see below |
| 15 | `proxy.rs:2871` → `scanout.rs:662-671` | `FrameAcquired` datagram (SEQPACKET `send`) | every acquire | 24 B + syscall | (a) as designed, but a per-frame syscall that only feeds the app's frame ring |
| 16 | `proxy.rs:7913-7984` | dispatch thread: `poll`, `read_events`, and a `wl_display_flush` per wake (empty buffer ⇒ no syscall) | per release event | — | (a); pure receive side |

Nothing else in the shim writes to the compositor: `grep -n 'wlproto::' proxy.rs`
outside `wlproto.rs` yields only the sites above plus the ones in this table.

So the per-frame wire contribution is **4 requests + 1 conditional + 1 write, 64-80
bytes**, of which exactly **one (12 B) is a candidate for suppression**.

The one class-(c) item on the present path is not a protocol request at all:
`mode_census(sc)` runs on **every present** (`proxy.rs:3114`) to decide whether to
print a line — it takes the process-global `MODE_LINE` mutex, builds a 8-field
`ModeLine`, and re-runs `live_choice` + `acquire_pacing` (four atomics and a
`cap_fps_note()`), then compares (`:10523-10538`). Because `MODE_LINE` is shared by
every swapchain and the census runs on the guest's present thread at ~330 Hz, it is a
global critical section on the present path taken to produce nothing in the steady
state. It costs ~100-300 ns per present (≈68-200 µs/s, 0.007-0.02 % of one core,
**0.0009-0.0025 pts**) — ~1/600-1/1700 of the gap, and about the same size as the
`set_present_mode` saving, for a zero-risk edit: cache the last `ModeLine` per
swapchain (a `Copy` struct in `GnSwapchain`) and only take the global mutex when the
local copy differs — the ungated change-only line and the `setpm` marker in §7 keep
working, since the line is printed exactly when the local copy moves.

## 2. `set_present_mode`: why the premise is wrong, and the exact state

`proxy.rs:2126-2134` states the invariant the parent brief assumed away, and it is
true of this payload:

> gamescope reads the declared mode once per commit and paces only the commits it
> reads as FIFO — so the declaration rides every commit.

Verified in gamescope (`/data/workspace/Projects/gamescope`, payload `20261110.1`):

- the handler stores the argument verbatim on the **wl_surface's** info
  (`wlserver.cpp:1115-1123`, `wl_info->oCurrentPresentMode = VkPresentModeKHR(present_mode)`);
- `PrepareCommit` reads it into `ResListEntry_t{fifo, async}` for *this* commit and
  then **clears it**: `wl_surf->oCurrentPresentMode = std::nullopt;`
  (`wlserver.cpp:180`, inside `PrepareCommit` at `:143`, reached from
  `xwayland_surface_commit` → `:247` through the per-surface commit listener at `:750`);
- `wlserver_surface_is_fifo` / `_is_async` answer `false` when it is `nullopt`
  (`:3412-3425` / `:3391-3410`), and `fifo` is what makes steamcompmgr defer a commit
  to the limiter's vblank (`steamcompmgr.cpp:8238`, `:8258`, `:8320`);
- by contrast `swapchain_feedback`, set the same way on the same object
  (`wlserver.cpp:1037`), is **not** cleared in `PrepareCommit` — so "sticky per
  surface" is already the normal shape of this protocol; the present mode reset is an
  outlier, not a requirement.

Consequence: with the shim's suppression alone, frame N+1's commit carries no
declaration, gamescope reads `fifo=false, async=false`, and **every mode degrades to
"present as soon as ready"**: FIFO sessions lose the limiter (and, per
`gamescope_route_pacing` `proxy.rs:10454-10456`, the acquire gate is *off* for a FIFO
declaration, so the cap is enforced by nothing), and mailbox/immediate sessions lose
the async tag that drives tearing/commit-queue policy. That is a pacing regression,
not a micro-optimisation.

Therefore the item splits:

- **5A (prerequisite, gamescope):** latch the declaration until it changes —
  drop the reset at `wlserver.cpp:180`, clear it instead when a swapchain binds to a
  window (`:1016-1023`).
- **5B (shim):** suppress the repeat by comparing the *computed* declaration with the
  last one sent, per wl_surface.

### State variables

| state | where it lives today | re-send condition |
|---|---|---|
| last declaration sent on this wl_surface | **nowhere** → new field `GnSurface::decl_pm` (`proxy.rs:7625`) | — |
| the declaration this commit wants | computed per commit by `gamescope_present_mode(declared_present_mode(sc))` (`proxy.rs:10595-10605`, `:10461-10469`) from `live_choice`: `sc.present_mode`, `sc.mode_source`, `sc.requested_mode`, `lsfg::live().0`, `scanout::cap_fps_note()`, `vsync_gate::current_grid()` (`proxy.rs:10353-10366`) | value differs from `decl_pm` |
| surface / swapchain identity | `GnSurface` (`proxy.rs:7625`), created at `xlib_override_surface` (`:9715-9722`), destroyed at `:9903-9913` | new surface ⇒ `decl_pm = NO_DECL` ⇒ first commit declares |

Keying the dirty bit on the **computed value**, not on the inputs, is what makes the
cap/refresh cases fall out for free, with no extra bookkeeping and no risk of missing
one:

| change | effect on the computed value | re-sent? |
|---|---|---|
| initial attach (first commit on a surface) | `decl_pm = NO_DECL` ≠ any wire value | yes, invariant 1 |
| present mode change (`effective_present_mode`, `proxy.rs:2302`) | value changes | yes |
| fps cap set/cleared (`scanout::cap_fps_note`, `:173`) | `capped` flips ⇒ `Declared::Mode` ↔ `CapRate`/`CapCompat` | yes |
| **display type / refresh change** (`vsync_gate::current_grid` period, `vsync_gate.rs:73-85`) | `at_display_rate(cap, grid)` (`vsync_gate.rs:218`) flips ⇒ FIFO ↔ the swapchain's mode. 60-cap on 60 Hz = FIFO; same 60-cap on 120 Hz = mailbox ⇒ 2→1 | yes, because the value moved. If the value does *not* move (same refresh, same cap) there is nothing to tell gamescope |
| frame generation on/off (`lsfg::live()`) | `Declared::Generating` ⇒ mailbox, and back | yes (§5) |
| swapchain recreate (format/colourspace/extent) | `swapchain_feedback` re-sent at `:2279`; the mode is unchanged, so not re-sent | correct: the wl_surface outlives the swapchain and gamescope's state is per wl_surface |
| second swapchain on the same `GnSurface` | same wl_surface, same gamescope state | cache is per surface, so the two swapchains cannot disagree with the server |
| resume from suspend | nothing in gamescope or the shim resets the per-surface state | not needed |
| **gamescope restart** | `wl_display` dies; `dispatch_main` latches `G_WL.failed` (`proxy.rs:7973`) and `wl_setup` returns false forever (`:7988-7990`) — **there is no reconnect today**, so the session ends and the invariant is vacuous. If a reconnect is ever added it must recreate the surface (or set `decl_pm = NO_DECL`), since the new compositor knows nothing. Documented at the field and in the helper. |

## 3. Unified diff — 5A (gamescope: latch the declaration)

```diff
--- a/src/wlserver.cpp
+++ b/src/wlserver.cpp
@@ -1016,6 +1016,13 @@ static void gamescope_swapchain_override_window_content( struct wl_client *client, struct wl_resource *resource, uint32_t server_id, uint32_t x11_window )
 {
 	wlserver_wl_surface_info *wl_surface_info = (wlserver_wl_surface_info *)wl_resource_get_user_data( resource );
 
+	// A (re)bind to a window starts undeclared. The present mode otherwise
+	// lives on the surface until the client changes it: a game whose fps limit
+	// or mode did not move must not have to re-declare it on every commit.
+	if ( wl_surface_info )
+		wl_surface_info->oCurrentPresentMode = std::nullopt;
+
 	gamescope_xwayland_server_t *server = wlserver_get_xwayland_server( server_id );
 	assert( server );
 	server->handle_override_window_content(client, resource, wl_surface_info->wlr, x11_window);
@@ -174,10 +181,9 @@ ResListEntry_t PrepareCommit( struct wlr_surface *surf, struct wlr_buffer *buf )
 	wl_surf->present_id = std::nullopt;
 	wl_surf->desired_present_time = 0;
 	wl_surf->pending_presentation_feedbacks.clear();
-	wl_surf->oCurrentPresentMode = std::nullopt;
 
 	struct wlr_surface *pConstraintSurface = wlserver_surface_to_main_surface( surf );
```

`present_id`/`desired_present_time` stay per-commit (they name *this* frame); only the
mode becomes input state, like `swapchain_feedback` already is. The write to
`ResListEntry_t.async/fifo` (`:165-186`) is unchanged, so pacing semantics are
bit-identical to today for a client that declares every commit, and identical to
"declare once" for a client that does not.

## 4. Unified diff — 5B (shim: dirty tracking)

Not compiled here (read-only task); read-checked against every literal of `GnSurface`
(`proxy.rs:9593`, `:9728`, `:9790` — all three need the new field or it will not build).

```diff
--- a/icd_shim/src/proxy.rs
+++ b/icd_shim/src/proxy.rs
@@ -238,6 +238,11 @@
 static DECL_FIFO_N: AtomicUsize = AtomicUsize::new(0);
 static DECL_OTHER_N: AtomicUsize = AtomicUsize::new(0);
+/// `gamescope_swapchain.set_present_mode` requests actually put on the wire:
+/// one per declared-mode change, against one per commit before
+/// `declare_present_mode` suppressed the repeats. Reported by the `present
+/// mode:` census beside `commits=`, which is one per frame.
+static SET_PM_N: AtomicUsize = AtomicUsize::new(0);
@@ -7628,6 +7633,12 @@ pub struct GnSurface {
     pub parent: *mut wl_proxy,        // commit target wl_surface
     pub own_surface: *mut wl_proxy,   // X11 mode: shim-created wl_surface
     pub swapchain_obj: *mut wl_proxy, // X11 mode: gamescope_swapchain
+    // The last `set_present_mode` value sent on `swapchain_obj`, `NO_DECL`
+    // before the first send. gamescope holds the declaration on the
+    // wl_surface (`wlserver_wl_surface_info`), so this is per surface and not
+    // per swapchain: two swapchains on one surface declare the same state.
+    // Written and read only under `SEND_LOCK`.
+    pub decl_pm: u32,
     pub x11: *mut crate::x11::X11Present, // X11 route (DRI3+Present) presenter
@@ -9596,6 +9607,7 @@ unsafe extern "C" fn gn_vkCreateWaylandSurfaceKHR(
         parent: ci.surface.cast(),
         own_surface: ptr::null_mut(),
         swapchain_obj: ptr::null_mut(),
+        decl_pm: NO_DECL,
         x11: ptr::null_mut(),
         window: 0,
         next: ptr::null_mut(),
@@ -9730,6 +9742,7 @@ unsafe fn xlib_override_surface(
             parent: surf,
             own_surface: surf,
             swapchain_obj: scobj,
+            decl_pm: NO_DECL,
             x11: ptr::null_mut(),
             // The size gamescope must see this layer at is the window's, not
             // the guest's requested swapchain extent (see the note in caps).
@@ -9790,6 +9803,7 @@ unsafe fn gn_vkCreateXlibSurfaceKHR(
         parent: ptr::null_mut(),
         own_surface: ptr::null_mut(),
         swapchain_obj: ptr::null_mut(),
+        decl_pm: NO_DECL,
         x11,
         window,
         next: ptr::null_mut(),
@@ -10602,6 +10616,48 @@ fn gamescope_present_mode(mode: VkPresentModeKHR) -> u32 {
     }
 }
 
+/// No declaration sent on this surface yet. A wire value is always 0-4, so the
+/// sentinel can never collide with one.
+const NO_DECL: u32 = u32::MAX;
+
+/// The wire value for "this commit declares nothing": gamescope leaves
+/// `oCurrentPresentMode` unset, so `wlserver_surface_is_fifo` and
+/// `wlserver_surface_is_async` both answer false. `4` is not a
+/// `VkPresentModeKHR` and gamescope stores the argument verbatim, so it reads
+/// exactly as the undeclared state a frame generated by `commit_generated`
+/// presents under today.
+const GS_MODE_NONE: u32 = 4;
+
+/// Whether the gamescope payload latches `set_present_mode` across commits
+/// (`PrepareCommit` no longer clears it). It moves with the **payload**, not
+/// with this crate: against a payload that clears it, every commit after the
+/// first would read as undeclared, which un-paces FIFO sessions (a FIFO
+/// declaration is also what turns the acquire gate off, `gamescope_route_pacing`).
+const GS_PM_LATCHED: bool = true;
+
+/// Declare `want` to gamescope on `sc`'s swapchain, unless this surface already
+/// carries it. Caller holds `SEND_LOCK`.
+///
+/// The declaration is per wl_surface in gamescope
+/// (`wlserver_wl_surface_info::oCurrentPresentMode`, `wlserver.cpp:1121`), so it
+/// is cached on `GnSurface`, and a new surface (or a reconnect, which today
+/// ends the session: `dispatch_main` latches `failed` and `wl_setup` never
+/// retries) starts from `NO_DECL` and therefore re-issues everything.
+unsafe fn declare_present_mode(sc: *mut GnSwapchain, want: u32) {
+    unsafe {
+        let surf = (*sc).surf;
+        if (*surf).swapchain_obj.is_null() {
+            return; // guest-surface (DRI3) route: nothing to declare to
+        }
+        if GS_PM_LATCHED && (*surf).decl_pm == want {
+            return;
+        }
+        wlproto::swapchain_set_present_mode((*surf).swapchain_obj, want);
+        (*surf).decl_pm = want;
+        SET_PM_N.fetch_add(1, Ordering::Relaxed);
+    }
+}
+
 fn offered_present_modes(
@@ -2132,14 +2188,9 @@ unsafe fn commit_real(sc: *mut GnSwapchain, idx: i32, fence_ms: u128) {
             // reads as FIFO — so the declaration rides every commit.
             // Declaring the effective mode (not a hardcoded FIFO) is what
             // lets a window's fps limit, or the absence of one, be seen by
             // the compositor as the shim applies it.
             // The guest-surface route has no gamescope swapchain to declare to.
-            if !(*(*sc).surf).swapchain_obj.is_null() {
-                wlproto::swapchain_set_present_mode(
-                    (*(*sc).surf).swapchain_obj,
-                    gamescope_present_mode(declared_present_mode(sc)),
-                );
-            }
+            // `declare_present_mode` suppresses the repeat only when the
+            // payload latches it; `declared_present_mode` stays per commit so
+            // the `declared_fifo`/`declared_other` census keeps its meaning.
+            declare_present_mode(sc, gamescope_present_mode(declared_present_mode(sc)));
@@ -3048,12 +3099,7 @@ unsafe fn present_sync_commit(sc: *mut GnSwapchain, slot: *mut GnSlot, idx: i32) {
                 // Declare the mode in force, as on the worker path above
                 // (gamescope reads the mode once per commit).
-                if !(*(*sc).surf).swapchain_obj.is_null() {
-                    wlproto::swapchain_set_present_mode(
-                        (*(*sc).surf).swapchain_obj,
-                        gamescope_present_mode(declared_present_mode(sc)),
-                    );
-                }
+                declare_present_mode(sc, gamescope_present_mode(declared_present_mode(sc)));
@@ -765,6 +820,11 @@ unsafe fn commit_generated(sc: *mut GnSwapchain, slot_index: usize) {
     let _sg = SEND_LOCK.lock().unwrap();
     unsafe {
+        // A generated frame presents undeclared ("neither FIFO nor async"). A
+        // latching gamescope would otherwise keep the last real frame's
+        // mailbox and call the generated frame async, so say "neither"
+        // explicitly — once, not per frame (the cache settles after the first).
+        declare_present_mode(sc, GS_MODE_NONE);
         wlproto::surface_attach(parent, (*s).buf, 0, 0);
@@ -10544,7 +10604,7 @@ unsafe fn mode_census(sc: *mut GnSwapchain) {
     crate::logf!(gndrm_obf::obf!(
-        "present mode: declared={} requested={} source={} route={} cap={} gate={} lsfg={} acquires={} commits={} declared_fifo={} declared_other={} releases={} feedback={} presented={} discarded={} fallback={} at_queue={} gate_holds={} gate_late={} skipped={} slot_waits={} commit_lat_us={} spacing=[<15:{} 15-16.2:{} 16.2-17.2:{} 17.2-20:{} >20:{}]",
+        "present mode: declared={} requested={} source={} route={} cap={} gate={} lsfg={} acquires={} commits={} declared_fifo={} declared_other={} setpm={} releases={} feedback={} presented={} discarded={} fallback={} at_queue={} gate_holds={} gate_late={} skipped={} slot_waits={} commit_lat_us={} spacing=[<15:{} 15-16.2:{} 16.2-17.2:{} 17.2-20:{} >20:{}]",
@@
         DECL_FIFO_N.load(Ordering::Relaxed),
         DECL_OTHER_N.load(Ordering::Relaxed),
+        SET_PM_N.load(Ordering::Relaxed),
         REL_ALL_N.load(Ordering::Relaxed),
```

Notes on the invariants the brief named:

1. **First frame after any state change carries the full state** — by construction:
   the comparison is against the *computed* declaration, and the cache starts at
   `NO_DECL` which no wire value equals, so the very first commit on any surface
   sends. Note that only the mode is cached; attach/damage/commit stay per frame
   (they are per-frame state).
2. **Gamescope restart re-issues everything** — trivially today: no reconnect exists
   (`proxy.rs:7973`, `:7988-7990`), the session ends. The invariant is preserved by
   keeping the cache on `GnSurface` (dies with the wl_surface it describes, symmetric
   with gamescope's `wl_surface_info`, `wlserver.cpp:738-740`, `:750`): a reconnect
   that recreates the surface gets `NO_DECL`; one that reuses it must reset the field
   by hand, and that requirement is written at the field.
3. **FPS cap path re-issues on a display type change** — §2's table: the cap and the
   grid both enter `choose_live_mode`, so a panel/refresh change that alters *what
   gamescope should do* changes the value and re-sends. The change is not keyed on
   the display event itself, which is the point: an event that does not move the
   declaration has nothing to tell gamescope.

Warning-free: `NO_DECL` (3 literals + compare), `GS_MODE_NONE` (one call),
`GS_PM_LATCHED` (one `if`), `decl_pm` (read + written), `SET_PM_N` (incremented and
formatted) are all used; `declare_present_mode` is called from three sites; no new
`unsafe` outside an existing `unsafe` block or `SEND_LOCK`.

## 5. What the generated path costs under the change

`choose_live_mode` returns `Declared::Generating` ⇒ `MAILBOX` while frame generation
runs (`proxy.rs:10391-10392`), so real frames declare `1` and generated frames would
declare `GS_MODE_NONE` = `4` under the latch: **two declarations per output pair**,
i.e. one per output frame — exactly the count today, no saving and no regression
while LSFG is on. Under generation the item is a no-op in both directions; the saving
applies to the non-generating session, which is the one in doc 24. Leaving the
generated path undeclared under a latch would have made generated frames inherit
mailbox/async from the real frames, which is a real behaviour change on the path docs
17-19 tune; the `4` above exists to avoid it.

## 6. Honest estimate of the saving

Numbers from doc 24 §2 (`docs/01-gamescope-vs-x11-cpu.md:36-43`): 339 fps, CPU busy
45.8 → 47.3 % (+1.5 pts of 8 cores = +12.8 % of one core), CPU sys 20.3 → 21.9 %
(+1.6 pts).

### 6.1 Per frame, now vs after

| | now | after |
|---|---|---|
| wayland marshals per commit | 4 always + 1 when a present wait is chained | 3 always + 1 |
| wayland bytes | 64 B (80 B) | 52 B (68 B) |
| `write(2)` syscalls (wayland) | 1 | **1** |
| SEQPACKET datagrams | 1 per acquire (`FrameAcquired`), 1 per commit (`PresentMark`) | unchanged |

Only one 12-byte message and its ~100-200 ns marshal go away; the flush — the only
syscall on this path — is untouched, and the datagrams (the actual per-frame syscall
cost, cause A) are untouched.

### 6.2 Against the measured gap

- Gap: 1.6 pts of 8 cores = 0.128 core = **≈377 µs of extra guest CPU per frame** at
  339 fps.
- Saving: **0.15-0.45 µs/frame** = 0.005-0.015 % of one core = **0.0006-0.0019 pts**
  of 8-core busy. Ratio to the gap: **~850-2500 : 1**.
- Context: the *entire* wayland send side (4-5 marshals + the write, ≈2 µs per
  frame) is 0.068 % of one core ≈ 0.0085 pts — still under 1/180 of the gap.
  Deleting every wayland request the shim issues would not move this measurement.
- Resolution: a 210 s arm at this rate presents ~71 000 frames; arm-to-arm
  repeatability in doc 24 is nowhere near 0.001 pts (the report's own headline moved
  3.8 → 1.5 pts between payloads, and the per-process sums disagree by 5 %). The
  falsifier doc 24 proposes for F ("drop the call when unchanged; measure guest sys
  time", `:189`) therefore **cannot resolve the effect** — it would be reading noise,
  sign unknown.

**Plainly: this is noise.** Not "small but real" — 3 orders of magnitude below the
measurement's own repeatability, on a delta whose cause the report already attributes
to the guest WSI machinery (A), present-wait (B) and slot starvation (C).

### 6.3 What is bigger on the same line, and still not the gap

The two per-frame SEQPACKET sends (`scanout.rs:662-671`, `:679-685`) are 5-10× the
`set_present_mode` cost: two `send()` syscalls plus two `CHAN` mutex acquisitions per
frame. Coalescing them (piggyback `FrameAcquired` on the `PresentMark` frame, or drop
to a 4-10 Hz ring update since the daemon only turns deltas into a UI ring) would save
~2-2.5 µs/frame ≈ **0.008-0.011 pts** (0.07-0.08 % of one core) — measurable only with
a long run, still ~1/150-1/200 of the gap. If item 5 is to be done at all, this is the
same edit's better half.

## 7. Verification markers (exact strings, verified in source)

- Commits per frame: `proxy commit: sc={sc:?} idx={idx} {}x{} flush_rc={flush_rc} dpy_err={} fence={fence_ms}ms commit={commit_ms}ms #{} {}`
  — `proxy.rs:2175`, `#N` = `N_COMMIT_OK` (`:139`, bumped at `:2172`). Gated by
  `GNDRM_LOG_VKSHIM` for the first 8 and every 120th.
- Requests actually sent: the new `setpm={}` in
  `present mode: declared={} requested={} source={} route={} cap={} gate={} lsfg={} acquires={} commits={} declared_fifo={} declared_other={} releases={} …`
  — `proxy.rs:10547`, printed when the `ModeLine` changes (ungated, `:10523-10538`)
  and every 10 s under `GNDRM_LOG_VKSHIM` (`:10539`, `MODE_CENSUS_EVERY`).
  Expected after: `setpm` = O(1-3) per session (one per mode change) while `commits`
  ≈ frames (~20 000 in 60 s at 339 fps). Before: no `setpm`; `declared_fifo` +
  `declared_other` ≈ `commits` (they can exceed it slightly: `present_sync_commit`
  declares without bumping `N_COMMIT_OK`, `proxy.rs:3052-3067` vs `:2172`).
- Session totals on teardown: `proxy swapchain destroyed: sc={sc:?} {}x{} presents={} commits={}` — `proxy.rs:2612`.
- Non-marker check that the latch actually took effect: with the two changes and a
  FIFO session, `spacing=[…]` in the same census line must keep its mass on the
  refresh period; if the shim ships without 5A, spacing collapses to the guest's
  unthrottled interval and `gate=off` — the regression is visible in one line.

## 8. Risks, stated

1. **Payload coupling.** 5B under a non-latching payload is a pacing regression
   (§2). `GS_PM_LATCHED` exists so the shim can be rolled back with one constant, but
   the real guard is that both files ship in the same payload build.
2. **The "neither" third state.** gamescope's `nullopt` means "neither async nor
   FIFO", which the generator path relies on (§5). The `GS_MODE_NONE = 4` wire value
   reproduces it only because gamescope stores the argument verbatim and compares it
   against two enumerators (`wlserver.cpp:3403-3425`); a future gamescope that
   validates the enum would reject `4`. This is the weakest point in the change and
   the reason not to bother: the win is ~0.001 pts.
3. **Two swapchains, one surface, different declarations** would thrash the cache
   between two values (one send per commit — i.e. today's behaviour). Not a
   regression, and not a real configuration for this guest.
4. Nothing here touches the frame path's ordering, the present-id channel, or
   `commit_generated`'s commit — attach/damage/commit keep their per-frame cadence,
   which is required for correctness.

## 9. Recommendation

Close plan item 5 with this document as the reason: the call *is* redundant by intent
but *is not* redundant against the gamescope this payload ships, and the version that
would make it redundant is worth **≈0.001 pts** of 8-core busy against a 1.6 pt gap —
below the repeatability of the measurement it is supposed to show up in. If hygiene is
wanted anyway, land 5A + 5B together as an enabler (it is the precondition for any
future "declare once per mode" WSI), not as a CPU fix, and fold in the datagram
coalescing of §6.3, which is a real if modest syscall reduction. The gap stays with
doc 24 items 2-4.
