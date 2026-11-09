# 05 — The daemon present path: per-frame allocations and the per-frame JNI lookup

Status: **analysis + reviewable diffs. Nothing applied to the tree, nothing built.** Every hunk is
keyed to `gamenative-drm` at `3588fa9` (`performance-report`). Verification done: each of the 32
hunks' context was matched against the working tree and matches it **exactly once**, and the six
patch groups were applied in order to an in-memory copy of the tree, cleanly. Not done (this task
is read-only): `cargo check`/`clippy` on the result, so treat type-level details as reviewed, not
compiled.

Reference frame: doc 24 (cause **E**), doc 06 (publisher), doc 07 (commit model). Publish rate
≈ **340/s** (doc 24 §1: 339.2 fps measured, panel 120 Hz — the daemon publishes the guest's frame
rate, SurfaceFlinger latches at the panel rate). "Per frame" below always means per published
frame, i.e. ~340 Hz, on a path the doc calls **exactly one deep**: every µs here is serial.

---

## 0. Verdict on the suspect list

| # | suspect (doc 24 §4) | verdict | where | freq | allocation size |
|---|---|---|---|---|---|
| F1 | fresh `HashSet<u64>` of every live GEM inode per commit | **confirmed** | `commit.rs:540`–`546` | per commit | 1 hashbrown table, ~64–256 B + `N` entries; `N` = distinct live inodes |
| F2 | ~4 `Vec` allocations per atomic-request parse | **confirmed, and it is 5** | `commit.rs:311`,`:318`,`:330`,`:337`,`:344` | per commit | 4·n, 4·n, 4·total, 8·total scratch + 24·total output; `n`=objs (3–4), `total`=props (40–60) |
| F3 | rebuilt `Vec<pollfd>` per scan-out iteration | **confirmed, and it is 2 `Vec`s per turn** | `asc.rs:2561`,`:2565` | per poll turn (2–3 per frame) | 6+retired pollfds (96 B) + retired fd list |
| F4 | `GetStaticMethodID` per published frame for `onFramePublished` | **confirmed, and worse than stated**: also 2 `CString`s and a `JNIString` cesu8 re-encode per call | `lib.rs:387` (`call_static_method`) ← `asc.rs:3980`/`:1650` | per produced frame | 2 heap strings + an ART method lookup |
| F5 | flip-event emission → `gndrm-daemon: commit #N` path allocates per event | **new** | `server.rs:415`–`417` | per flip event (per frame) | 2–3 `Vec`s (incl. `pod.rs:34`'s `tmp`) |
| F6 | RPC response frame built fresh per request | **new** | `server.rs:451` | per ioctl, incl. every commit | 1 `Vec` + 1 growth |
| F7 | plane census string rebuilt per commit while the diag gate is on | **new** (diag only) | `commit.rs:242`–`256` | per commit **when `GNDRM_LOG_DAEMON` is on** | ~6 `String`s + 1 `Vec<String>` |
| F8 | `fds_except()` allocates a `Vec` per poll turn | folded into F3 | `pushes.rs:74` | per poll turn | 1 `Vec` |
| — | `build_show` 8-op transaction build (`asc.rs:3557`) | **refuted** | — | — | zero Rust allocation: `(api.st_*)` C calls only |
| — | `retire()`/`retire_to_release()` per publish (`asc.rs:760`, `:3208`) | **refuted as a steady-state cost** | — | — | `victims`/`out`/`removed` are empty `Vec`s (no allocation) when nothing retires; O(pool) filter over ~4 entries remains |
| — | `drain_flip_events()` (`lib.rs:1031`) | **refuted** | — | — | `.drain(..).collect()` on an empty queue does not allocate |
| — | `PresentDone::encode()` (`wire.rs:727`) | **refuted** | — | — | returns `[u8; 24]`, stack |
| — | `server.rs` serve pollfd array (`:386`) | **refuted** | — | — | fixed `[pollfd; 2]` on the stack |
| — | `dump_published` env lookup (`asc.rs:149`) | **refuted** | — | — | `OnceLock`, one lookup per process |
| — | log macros on the hot path | **refuted** | `log.rs:20` | — | gate is tested before `fmt::format`; `obf!` decodes into a stack array (`gndrm-obf/src/lib.rs:46`), no heap |

Non-allocation hot-path cost found while verifying, listed for completeness, **not** in this
plan item: `Device::diag_on()` (`lib.rs:903`) takes the logger `Mutex` once per commit — a
lock/unlock on the serve thread's present path in a shipped session, where the answer is always
`false`.

Honest scale, up front: F1+F2 ≈ 0.6–1.7 µs/commit, F3 ≈ 0.4–0.9 µs/frame, F4 ≈ 1.5–4 µs/frame —
together **≈ 0.9–2.3 ms of CPU per second** at 340 Hz. The daemon's excess in doc 24 is
**+7.2 pp of one core = 72 ms/s**. Item 4 therefore recovers ~1.5–3 % of cause E and cannot be
its explanation. Its value is different and still real: the present path is one frame deep
(§3), so removing heap work from it removes a source of *unbounded* latency (allocator arena
contention with the daemon's other threads: `gndrm-warm`, `gndrm-diag`, `gndrm-scanout`, ART),
not just a few µs. Doc 24 §5 item 4 already calls this "small but free"; the numbers agree.

---

## 1. Who runs what, per frame

| thread (`lib.rs` name) | per frame | allocation sites |
|---|---|---|
| `gndrm` (serve, `server.rs:380`) | `poll` → `recvmsg` → `dev.dispatch` → `ioctl_atomic` → `apply_commit` → `sendmsg`; plus one flip event per publish | **F1, F2, F6** (`poll`/`recvmsg`/`sendmsg` are the only syscalls) |
| `gndrm-commit` (`lib.rs:750`) | `presenter.publish_with_source` → pool retire/resolve → `build_show` → `apply` | none in the steady state (refuted above) |
| `gndrm-scanout` (`lib.rs:623`) | `poll` over 6+retired fds → alloc/doorbell requests, present-done notes, `FrameAcquired` → `notify_frame_published` | **F3, F8, F4** |
| SF binder thread | `on_complete` → `note_present_done` (`asc.rs:362`) | none (atomics + a `Mutex<Vec>` push) |

F4 fires from `gndrm-scanout` (`asc.rs:3980`, inside `note_produced`'s per-frame loop) and, in a
session whose shim sends no `FrameAcquired` reports, from `gndrm-commit` via `asc.rs:1650`.
Either way it is one JNI transition per published frame.

---

## 2. Findings, with the code

### F1 — `HashSet` of every live inode, per commit. `commit.rs:536`–`546`

```rust
let live_inos: HashSet<u64> = self
    .stores
    .gems
    .values()
    .map(|g| g.ino)
    .chain(self.stores.gems_closed.values().map(|g| g.ino))
    .collect();
```

Cost: one hashbrown allocation + `N` SipHash-1-3 inserts (~5 ns each) + two full bucket-array
traversals (random-pointer chasing over `gems` and `gems_closed`), then one free. `N` is the
number of **distinct live dma-buf inodes**, which in a healthy gamescope session is the pool
size — 3–4 buffers, so ~0.4–0.8 µs. `N` is **not bounded by the pool** in the pathological case
the set exists for: a client that imports a fresh dma-buf per frame and never releases it grows
`gems`/`gems_closed` monotonically, and this becomes an O(N) hash + traverse per commit at
340 Hz. The existing commit probe prints `fbs={}` (`commit.rs:209`) — a live on-device bound on
the table size; read it before and after the change.

The set is *not* derivable from `gem_by_ino` (`objects.rs:202`), which is the tempting shortcut:
`ioctl_gem_close` removes the inode from `gem_by_ino` (`lib.rs:1910`) *before* it moves the
object into `gems_closed`, and `ADDFB2`'s fresh handles deliberately bypass the index
(`lib.rs:2047` comment). Either fact makes `gem_by_ino` a strict subset of the live set, and a
subset here frees a buffer the client still imports — so the shortcut is rejected (§6).

### F2 — five `Vec`s per atomic parse. `commit.rs:311`,`:318`,`:330`,`:337`,`:344`

```rust
let mut objs = Vec::with_capacity(n);        // :311
...
let mut counts = Vec::with_capacity(n);      // :318
...
let mut prop_ids = Vec::with_capacity(total); // :330
...
let mut values = Vec::with_capacity(total);   // :337
...
let mut sets = Vec::with_capacity(total);     // :344
```

Four of these are pure scratch (`objs`, `counts`, `prop_ids`, `values`) and are discarded at the
end of the call; `sets` is the returned `ParsedCommit`. Typical commit: `n`=3–4 objects,
`total`=40–60 props (doc 24 §3 item 7: gamescope builds ~40–60 `drmModeAtomicAddProperty` per
commit) ⇒ ~16 B + 16 B + 240 B + 480 B + 1.4 KB, i.e. 5 alloc/free pairs ≈ 0.5–1.2 µs/commit.

The scratch is unnecessary: the wire layout (module docs, `commit.rs:3`–`11`) is fixed-stride, so
every element's offset is a closed-form function of the header, `n` and the flat index.
`parse_atomic` takes `&self` (`commit.rs:301`), so a `Device`-owned scratch buffer would need a
`&mut` signature change and a take/put-back dance on six return paths; reading the payload
directly is smaller *and* faster (no copy at all). Patch P2.

### F3 — two `Vec`s per scan-out poll turn. `asc.rs:2561`,`:2565`

```rust
let retired = gndrm_daemon::pushes::fds_except(fd);      // :2561 — allocates (pushes.rs:74)
...
let mut pfds = Vec::with_capacity(6 + retired.len());    // :2565 — allocates
```

Per *turn*, not per frame: a turn is a request, a present-done eventfd wake, a cap change, a
vsync change, a listener event, or a retired-connection report — in steady state 2–3 turns per
frame. So 4–6 alloc/free pairs per frame, ≈ 0.4–0.9 µs. `fds_except` also takes the global
`SINKS` mutex per turn, which P3 removes the allocation half of.

### F4 — JNI lookup + two heap strings per frame. `lib.rs:387`

```rust
match env.call_static_method(class, method, "()V", &[]) {
```

`JNIEnv::call_static_method` (jni 0.21.1, `wrapper/jnienv.rs:1458`) is
`call_static_method_unchecked(class, (class, name, sig), …)`, i.e. it calls `GetStaticMethodID`
**on every call**. Worse than doc 24 states: the `Desc` for `(class, name, sig)` converts
`&str` → `JNIString` (`wrapper/strings/ffi_str.rs:46`: `to_java_cesu8(..).into_owned()` +
`CString::from_vec_unchecked`), so each call also allocates **two** CESU-8 `CString`s, for the
name and for the signature. Per frame: 2 heap strings + an ART method lookup under the class
lock (~0.5–2 µs) + the JNI transition, ≈ 1.5–4 µs. This is the **largest** of the four and
doc 24 lists it last; the order in §4 should be read as size-of-fix, not size-of-cost.

Call sites: `asc.rs:3980` (once per produced frame, inside `note_produced`'s loop) and
`asc.rs:1650` (the fallback when the shim sends no producer reports). Reached from
`notify_frame_published` (`lib.rs:312`) ← `call_app_static`.

### F5 — flip event: 2–3 `Vec`s per frame. `server.rs:415`–`417`

```rust
for ev in dev.drain_flip_events() {
    let mut bytes = Vec::new();
    pod_write(&mut bytes, &ev);
```

`Vec::new` + `pod_write`'s own `tmp` (`pod.rs:34`: `vec![0u8; size_of::<T>()]`) + one growth in
`extend_from_slice`. `DrmEventVblank` is a 32-byte POD (`drm.rs:102`), so this is a 3-allocation
path for 32 bytes, once per flip event — i.e. per published frame, on the thread that also parses
the next commit. Patch P5.

### F6 — response frame per request. `server.rs:451`

```rust
let mut frame = resp.encode().to_vec();   // [u8; 24] → heap
frame.extend_from_slice(&result.body);    // + growth
```

Per RPC request, which includes every `MODE_ATOMIC` — so per frame too. `respx.encode()` is
already a stack array (`wire.rs:105`); only the `.to_vec()` is the problem. Patch P5.

### F7 — the census allocates per commit *while logging is on*. `commit.rs:242`–`256`

```rust
if self.diag_on() {
    let census = self.plane_census_text();      // ~5 allocations
    let changed = census != self.plane_census;
    if changed { self.plane_census = census.clone(); }   // +1
```

`plane_census_text` (`commit.rs:266`) builds two `String`s, a `Vec<String>`, a `join` and an
outer `format!`. This runs on **every** commit whenever the logger is installed — and the only
way to read `gndrm-daemon: commit #N` on device is to install it. Two consequences:
(a) it is the single largest per-commit allocation cluster in the daemon *when measuring*;
(b) a before/after run made with `GNDRM_LOG_DAEMON=1` measures F7, not F1/F2. Patch P6 makes it
free, and §5 says how to measure without it either way.

---

## 3. Patches

Invariant stated per patch; none changes an observable behaviour (errno, log text, published
bytes, trait signatures). `Presenter::publish`/`publish_with_source` are **unchanged** in all
patches, so `tests/golden.rs` (which implements `Presenter` at `:76`, `:1404`, `:2081`, `:2612`,
`:2927`) is untouched.

### P1 — reference-counted live-inode set, cached and shared

Invariant preserved: *the set handed to a commit equals the set of inodes named by any entry in
`gems ∪ gems_closed` at the moment the commit was applied* — exactly what `commit.rs:540`
computed inline. Strengthened, not changed, by a `debug_assert` recount of both tables on every
rebuild, so a mutation site that misses a `gem_ino_ref`/`gem_ino_unref` fails every debug build
and test run instead of silently retiring a live buffer.

Design: `ino_refs` (one count per inode across both tables) + a membership `ino_gen` bumped only
when a count crosses 0. Handle churn (gamescope opens/closes a handle per buffer, `lib.rs:1964`
comment) moves counts but does not change membership, so the snapshot is *not* invalidated per
frame. The snapshot is an `Arc` shared with the commits in flight, so queuing a commit is an
atomic increment, not a copy. Reference counting is required, not defensive: the same inode can
be named twice — an orphaned `gems_closed` entry plus a re-imported handle (only `gem_by_ino`
dedups, and it is emptied on close), or two `ADDFB2` fresh handles (`lib.rs:2057`).

```diff
--- a/modules/gndrm-daemon/src/objects.rs
+++ b/modules/gndrm-daemon/src/objects.rs
@@ -25,2 +25,3 @@
-use std::collections::HashMap;
+use std::collections::{HashMap, HashSet};
 use std::os::fd::RawFd;
+use std::sync::Arc;
@@ -208,2 +209,13 @@
     pub blobs: HashMap<u32, Blob>,
+    /// One reference count per inode named by a `gems` or `gems_closed` entry
+    /// (`gem_by_ino` is not a superset of them — see `Device::ioctl_gem_close`).
+    /// The presenter's liveness set is derived from this instead of being
+    /// walked out of both tables on every commit.
+    ino_refs: HashMap<u64, u32>,
+    /// Membership generation of `ino_refs`: bumped only when an inode enters or
+    /// leaves it, never when a count changes — a client that re-imports and
+    /// re-closes a buffer per frame must not invalidate the cached set.
+    ino_gen: u64,
+    /// The set for `ino_gen`, shared with every commit still in flight.
+    ino_snapshot: Option<(u64, Arc<HashSet<u64>>)>,
     /// Freed ids, one list per space, handed back out before the monotonic
@@ -233,2 +245,5 @@
             blobs: HashMap::new(),
+            ino_refs: HashMap::new(),
+            ino_gen: 0,
+            ino_snapshot: None,
             free_gems: Vec::new(),
@@ -246,2 +261,59 @@
 impl Stores {
+    /// Record one more GEM entry naming `ino`.
+    pub fn gem_ino_ref(&mut self, ino: u64) {
+        let n = self.ino_refs.entry(ino).or_insert(0);
+        *n = n.saturating_add(1);
+        if *n == 1 {
+            self.ino_gen = self.ino_gen.wrapping_add(1);
+        }
+    }
+
+    /// Record one fewer GEM entry naming `ino`; the entry naming it is already
+    /// gone. A count that reaches zero takes the inode out of the live set.
+    pub fn gem_ino_unref(&mut self, ino: u64) {
+        use std::collections::hash_map::Entry;
+        let Entry::Occupied(mut e) = self.ino_refs.entry(ino) else {
+            // An unref without a ref: the recount assertion in `live_inos`
+            // (debug builds) is what catches a real accounting error.
+            return;
+        };
+        let n = *e.get();
+        *e.get_mut() = n.saturating_sub(1);
+        if n <= 1 {
+            e.remove();
+            self.ino_gen = self.ino_gen.wrapping_add(1);
+        }
+    }
+
+    /// The inodes still backed by a GEM object, as of now — the presenter's
+    /// retire signal. Rebuilt only when membership changed since the last call,
+    /// and handed out as a shared `Arc`, so a steady-state commit allocates
+    /// nothing and a commit in flight keeps the snapshot it was queued with.
+    #[must_use]
+    pub fn live_inos(&mut self) -> Arc<HashSet<u64>> {
+        if let Some((gen, set)) = &self.ino_snapshot {
+            if *gen == self.ino_gen {
+                return Arc::clone(set);
+            }
+        }
+        let set: HashSet<u64> = self.ino_refs.keys().copied().collect();
+        // The incremental counts must agree with a recount: a mutation site
+        // that forgot to call `gem_ino_ref`/`gem_ino_unref` shows up here, in
+        // every debug build and test run, rather than as a pooled buffer
+        // retired while the client still imports it.
+        debug_assert_eq!(
+            set.len(),
+            self.gems
+                .values()
+                .map(|g| g.ino)
+                .chain(self.gems_closed.values().map(|g| g.ino))
+                .collect::<HashSet<_>>()
+                .len(),
+            "ino_refs tracks every GEM entry"
+        );
+        let set = Arc::new(set);
+        self.ino_snapshot = Some((self.ino_gen, Arc::clone(&set)));
+        set
+    }
+
     /// Next-never-allocated id per space (`[gem, fb, blob, syncobj]`): the
```

```diff
--- a/modules/gndrm-daemon/src/lib.rs
+++ b/modules/gndrm-daemon/src/lib.rs
@@ -1676,5 +1676,6 @@
         self.stores
             .gems
             .insert(handle, objects::GemObject { fd, ino, size });
+        self.stores.gem_ino_ref(ino);
         self.stores.gem_by_ino.insert(ino, handle);
         c.handle = handle;
@@ -1747,5 +1748,8 @@
         self.stores
             .gems
             .insert(handle, objects::GemObject { fd, ino, size });
+        // Dedup by `st_ino` returned early above, so this inode is new to the
+        // live set.
+        self.stores.gem_ino_ref(ino);
         self.stores.gem_by_ino.insert(ino, handle);
         let mut body = Vec::new();
@@ -1908,8 +1912,13 @@
         match self.stores.gems.remove(&c.handle) {
             Some(gem) => {
                 self.stores.gem_by_ino.remove(&gem.ino);
                 if fb_referenced {
+                    // The orphan table names the same inode: the object only
+                    // moves, so its reference count is unchanged.
                     self.stores.gems_closed.insert(c.handle, gem);
                 } else {
+                    // The last reference to the object: one inode leaves the
+                    // live set (`Stores::live_inos`).
+                    self.stores.gem_ino_unref(gem.ino);
                     close_fd(gem.fd);
                     // No FB references it: the handle is truly gone, so its id
@@ -2071,4 +2080,7 @@
                                     },
                                 );
+                                // A fresh handle for an inode that may already
+                                // be named: the census counts entries.
+                                self.stores.gem_ino_ref(ino);
                                 new_handle
                             }
@@ -2127,2 +2139,3 @@
             if let Some(gem) = self.stores.gems_closed.remove(&h) {
+                self.stores.gem_ino_unref(gem.ino);
                 close_fd(gem.fd);
@@ -2171,3 +2184,6 @@
     /// Inodes still backed by a GEM object at commit time (the presenter's
     /// retire signal).
-    live_inos: HashSet<u64>,
+    ///
+    /// Shared with the object store's cached snapshot (`Stores::live_inos`), so
+    /// queuing a commit copies an `Arc`, not the set.
+    live_inos: Arc<HashSet<u64>>,
```

`lib.rs:2285` (`&job.live_inos` → `&HashSet<u64>`) needs **no edit**: `&Arc<HashSet<u64>>`
deref-coerces at the call. `lib.rs` already imports `Arc` (`:34`).

```diff
--- a/modules/gndrm-daemon/src/commit.rs
+++ b/modules/gndrm-daemon/src/commit.rs
@@ -18,1 +18,0 @@
-use std::collections::HashSet;
@@ -497,2 +496,10 @@
     fn apply_commit(&mut self, commit: &ParsedCommit, fence_fd: Option<RawFd>) {
         let old_primary_fb = self.primary_state().fb_id;
+        // Inodes still backed by a GEM object — the presenter's retire signal
+        // ("client stopped importing"). The pool reuses a retired buffer only
+        // once the presenter has reported the frame's completion (see
+        // `scanout`). The object store keeps this set reference-counted and
+        // cached (`Stores::live_inos`); nothing between here and the queue
+        // mutates the GEM tables, so taking it at entry is the same snapshot
+        // the old inline walk took below.
+        let live_inos = self.stores.live_inos();
@@ -536,11 +543,0 @@
-        // Inodes still backed by a GEM object — the presenter's retire signal
-        // ("client stopped importing"). The pool reuses a retired buffer only
-        // once the presenter has reported the frame's completion (see
-        // `scanout`).
-        let live_inos: HashSet<u64> = self
-            .stores
-            .gems
-            .values()
-            .map(|g| g.ino)
-            .chain(self.stores.gems_closed.values().map(|g| g.ino))
-            .collect();
```

### P2 — decode the atomic request out of the payload, no scratch arrays

Invariant preserved: the same triples, in the same commit order, with the same errno for every
rejected frame — `EINVAL` for a short frame or an out-of-bounds count, `ENOENT` for an
unresolved `(obj, prop)`, `EPERM`/etc. from validation unchanged. Error **precedence** is
preserved because the whole frame's length is checked before any `(obj, prop)` is resolved,
which is exactly what the old decoder did by reading all four arrays before resolving.

Bound argument: `n ≤ 64` and every `counts[i] ≤ 64`, so `total ≤ 4096` and no offset can
overflow. One allocation remains — `sets` (`Vec<PropSet>`, 24 B/triple, ~1.4 KB here). A
`Device`-pooled `sets` was rejected in §6 (borrowck), a fixed 4096-entry array was rejected as a
96 KiB stack frame.

```diff
--- a/modules/gndrm-daemon/src/commit.rs
+++ b/modules/gndrm-daemon/src/commit.rs
@@ -299,81 +299,97 @@
     /// Decode the linearized extension into `(obj, prop, value)` triples.
+    ///
+    /// The wire layout is fixed-stride, so every element's offset is a
+    /// closed-form function of the header, `count_objs` and the running flat
+    /// index: the four scratch arrays the obvious decoder needs (`objs`,
+    /// `counts`, `prop_ids`, `values`) are never materialized, and the only
+    /// allocation left is the `sets` vector this returns.
     fn parse_atomic(
         &self,
         a: &DrmModeAtomic,
         payload: &[u8],
     ) -> Result<ParsedCommit, DispatchResult> {
-        let mut off = core::mem::size_of::<DrmModeAtomic>();
+        const HDR: usize = core::mem::size_of::<DrmModeAtomic>();
         let n = a.count_objs as usize;
         // Kernel caps the object count; mirror with a sane bound.
         if n == 0 || n > 64 {
             return Err(DispatchResult::err(errno::EINVAL));
         }
-        let mut objs = Vec::with_capacity(n);
-        for _ in 0..n {
-            let Some(id) = take_u32(payload, &mut off) else {
-                return Err(DispatchResult::err(errno::EINVAL));
-            };
-            objs.push(id);
-        }
-        let mut counts = Vec::with_capacity(n);
-        for _ in 0..n {
-            let Some(c) = take_u32(payload, &mut off) else {
-                return Err(DispatchResult::err(errno::EINVAL));
-            };
-            if c > 64 {
-                return Err(DispatchResult::err(errno::EINVAL));
-            }
-            counts.push(c);
-        }
-        // Two flat arrays: all prop ids, then all values (wire doc above).
-        let total: usize = counts.iter().map(|&c| c as usize).sum();
-        let mut prop_ids = Vec::with_capacity(total);
-        for _ in 0..total {
-            let Some(p) = take_u32(payload, &mut off) else {
-                return Err(DispatchResult::err(errno::EINVAL));
-            };
-            prop_ids.push(p);
-        }
-        let mut values = Vec::with_capacity(total);
-        for _ in 0..total {
-            let Some(v) = take_u64(payload, &mut off) else {
-                return Err(DispatchResult::err(errno::EINVAL));
-            };
-            values.push(v);
-        }
+        // Section offsets, in wire order (module docs): u32 objs[n], u32
+        // counts[n], u32 prop_ids[total], u64 values[total].
+        let objs_at = HDR;
+        let counts_at = objs_at + 4 * n;
+        // Pass one: the per-object bound the old decoder applied element by
+        // element, and the flat total it needed before it could size the rest
+        // of the frame. Reading the counts implies the object array is present
+        // (it sits before them), so no separate pass over it is needed here.
+        let mut total = 0usize;
+        let mut counts_off = counts_at;
+        for _ in 0..n {
+            let Some(c) = take_u32(payload, &mut counts_off) else {
+                return Err(DispatchResult::err(errno::EINVAL));
+            };
+            if c > 64 {
+                return Err(DispatchResult::err(errno::EINVAL));
+            }
+            total += c as usize;
+        }
+        let prop_ids_at = counts_at + 4 * n;
+        let values_at = prop_ids_at + 4 * total;
+        // `n <= 64` and every count <= 64 bound `total`, so neither offset can
+        // overflow, and one length test decides what four element-wise reads
+        // used to: a short frame is rejected before anything is resolved.
+        if payload.len() < values_at + 8 * total {
+            return Err(DispatchResult::err(errno::EINVAL));
+        }
         let mut sets = Vec::with_capacity(total);
         let mut wants_fence = false;
-        let mut flat = 0;
-        for (i, &obj_id) in objs.iter().enumerate() {
-            for _ in 0..counts[i] {
-                let prop_id = prop_ids[flat];
-                let value = values[flat];
-                flat += 1;
+        // Pass two: resolve in commit order, straight out of the payload. Every
+        // read below is inside the span just checked, so a failure here is
+        // unreachable in practice and stays an errno rather than a panic.
+        let mut objs_off = objs_at;
+        // Rewound for the second pass: the counts were read, not consumed.
+        counts_off = counts_at;
+        let mut prop_off = prop_ids_at;
+        let mut value_off = values_at;
+        for _ in 0..n {
+            let Some(obj_id) = take_u32(payload, &mut objs_off) else {
+                return Err(DispatchResult::err(errno::EINVAL));
+            };
+            let Some(count) = take_u32(payload, &mut counts_off) else {
+                return Err(DispatchResult::err(errno::EINVAL));
+            };
+            for _ in 0..count {
+                let Some(prop_id) = take_u32(payload, &mut prop_off) else {
+                    return Err(DispatchResult::err(errno::EINVAL));
+                };
+                let Some(value) = take_u64(payload, &mut value_off) else {
+                    return Err(DispatchResult::err(errno::EINVAL));
+                };
                 let Some(prop_index) = props::resolve(obj_id, prop_id) else {
                     self.diag(|| {
                         format!(
                             "{}",
                             gndrm_obf::obf!(
                             "atomic: reject ENOENT unresolved prop obj={obj_id} prop=0x{prop_id:x}"
                         )
                         )
                     });
                     return Err(DispatchResult::err(errno::ENOENT));
                 };
                 if prop_index == props::P_PLANE_IN_FENCE_FD && value != u64::MAX {
                     wants_fence = true;
                 }
                 sets.push(PropSet {
                     obj_id,
                     prop_index,
                     value,
                 });
             }
         }
         Ok(ParsedCommit {
             sets,
             flags: a.flags,
             user_data: a.user_data,
             wants_fence,
         })
     }
```

### P3 — one `pollfd` vector for the life of the scan-out channel

Invariant preserved: the same descriptors in the same slot order, with the same `revents`
semantics. Slots 0..6 keep their fixed meaning (`asc.rs:2562` comment) and the retired
subscribers keep rows `6..`; a descriptor list that *shrinks* is handled because `pfds.clear()`
and the two `push` loops rebuild the rows every turn — only the heap object is reused, not the
contents. `Vec::with_capacity(8)` covers six fixed rows plus the two retired subscribers a
session actually has (`pushes` docs: three clients open the listener, one is served); growth
beyond that is one amortized reallocation per channel, not one per turn.

```diff
--- a/modules/gndrm-daemon/src/pushes.rs
+++ b/modules/gndrm-daemon/src/pushes.rs
@@ -70,13 +70,18 @@
 /// The registered descriptors, minus `except`: what a loop serving one
 /// connection should keep reading for the others (a retired subscriber still
 /// reports what it produces — see [`crate::pushes`]).
-#[must_use]
-pub fn fds_except(except: RawFd) -> Vec<RawFd> {
-    SINKS
-        .lock()
-        .unwrap_or_else(PoisonError::into_inner)
-        .iter()
-        .map(|s| s.fd)
-        .filter(|fd| *fd != except)
-        .collect()
-}
+///
+/// `out` is the caller's buffer, cleared and refilled: the scan-out loop polls
+/// once per frame and once per completion, so its descriptor list must not be a
+/// fresh heap object every turn.
+pub fn fds_except_into(out: &mut Vec<RawFd>, except: RawFd) {
+    out.clear();
+    out.extend(
+        SINKS
+            .lock()
+            .unwrap_or_else(PoisonError::into_inner)
+            .iter()
+            .map(|s| s.fd)
+            .filter(|fd| *fd != except),
+    );
+}
```

```diff
--- a/modules/gndrm-android/src/asc.rs
+++ b/modules/gndrm-android/src/asc.rs
@@ -2545,2 +2545,6 @@
     let _present_channel = PresentChannel(present_fd);
+    // Reused across turns: `poll` is rebuilt every turn (the descriptor set
+    // moves), but the two vectors holding it are allocated once per channel.
+    let mut pfds: Vec<libc::pollfd> = Vec::with_capacity(8);
+    let mut retired: Vec<RawFd> = Vec::with_capacity(4);
     loop {
@@ -2561,1 +2565,1 @@
-        let retired = gndrm_daemon::pushes::fds_except(fd);
+        gndrm_daemon::pushes::fds_except_into(&mut retired, fd);
@@ -2565,1 +2569,1 @@
-        let mut pfds = Vec::with_capacity(6 + retired.len());
+        pfds.clear();
```

### P4 — resolve the upcall's `jmethodID` once, not once per frame

Invariant preserved: same method, same `()V` signature, same class global ref, same failure
handling (a missing method logs the *same* text and clears the pending `NoSuchMethodError`, and
returns `AppCall::Failed`), same post-call exception check.

On "released on teardown": a `jmethodID` is not a reference — it owns nothing and cannot be
released, unlike the `jclass` global ref. Its validity is bounded by the class staying loaded,
and `CALLBACK_CLASS` (`lib.rs:239`) holds an `Arc`-like process-lifetime `GlobalRef` to exactly
that class. So the old "release it" instinct would reintroduce the lookup for no gain; the
invariant to record instead is **the cached id must never outlive `CALLBACK_CLASS`**, which is
why the id cache sits beside the class cache and is only ever reached through it. jni 0.21.1's
`JStaticMethodID` is `Copy + Send + Sync` and lifetime-free (`wrapper/objects/jstaticmethodid.rs:25`),
so this needs no `transmute`.

```diff
--- a/modules/gndrm-android/src/lib.rs
+++ b/modules/gndrm-android/src/lib.rs
@@ -63,2 +63,3 @@
-use jni::objects::{GlobalRef, JClass, JObject, JString};
+use jni::objects::{GlobalRef, JClass, JObject, JStaticMethodID, JString};
+use jni::signature::{Primitive, ReturnType};
 use jni::sys::{jboolean, jfloat, jint, jstring};
@@ -241,1 +242,11 @@
 static CALLBACK_CLASS: OnceLock<GlobalRef> = OnceLock::new();
+
+/// The upcalls' `jmethodID`s, resolved once from [`CALLBACK_CLASS`] instead of
+/// on every call: `JNIEnv::call_static_method` runs `GetStaticMethodID` — an ART
+/// method lookup — and builds a CESU-8 `CString` for the name *and* for the
+/// signature on each invocation, which on the per-frame path is one `GetStaticMethodID`
+/// and two heap objects per published frame. A `jmethodID` holds no reference
+/// and is valid as long as its class stays loaded, which `CALLBACK_CLASS`'s
+/// global reference guarantees for the life of the process.
+static CLEAR_SPLASH_METHOD: OnceLock<JStaticMethodID> = OnceLock::new();
+static FRAME_PUBLISHED_METHOD: OnceLock<JStaticMethodID> = OnceLock::new();
@@ -283,1 +294,1 @@
-    let outcome = call_app_static("clearBootingSplash");
+    let outcome = call_app_static("clearBootingSplash", &CLEAR_SPLASH_METHOD);
@@ -313,1 +324,1 @@
-    let _ = call_app_static("onFramePublished");
+    let _ = call_app_static("onFramePublished", &FRAME_PUBLISHED_METHOD);
@@ -362,1 +373,4 @@
-fn call_app_static(method: &str) -> AppCall {
+fn call_app_static(
+    method: &'static str,
+    cached: &'static OnceLock<JStaticMethodID>,
+) -> AppCall {
@@ -386,3 +400,28 @@
     };
-    match env.call_static_method(class, method, "()V", &[]) {
+    // Resolve the target once per process. `class` is the cached `GlobalRef`,
+    // which the jni crate converts to a `JClass` by transmute (`class_desc.rs`),
+    // so neither branch adds a reference.
+    let id = match cached.get() {
+        Some(id) => *id,
+        None => match env.get_static_method_id(class, method, "()V") {
+            Ok(id) => {
+                let _ = cached.set(id);
+                id
+            }
+            Err(e) => {
+                // A missing method throws `NoSuchMethodError`; it must not be
+                // left pending on this thread.
+                log_warn!("{}", gndrm_obf::obf!("{method}: call failed: {e}"));
+                let _ = env.exception_clear();
+                return AppCall::Failed;
+            }
+        },
+    };
+    // SAFETY: `id` was resolved from `class` with the `()V` signature above, and
+    // the class it was resolved against is pinned by `CALLBACK_CLASS` for the
+    // life of the process, so it cannot be unloaded under the cached id.
+    let called = unsafe {
+        env.call_static_method_unchecked(class, id, ReturnType::Primitive(Primitive::Void), &[])
+    };
+    match called {
         Ok(_) => {
```

Remaining per-call cost: `attach_current_thread_permanently` (`lib.rs:377`) = a `GetEnv` through
the `JavaVM` vtable, no syscall and no lock (jni `vm.rs:259` short-circuits on the thread-local).
Left in place deliberately: caching a `JNIEnv` per thread is a lifetime hazard for a ~50–100 ns
win, and it is not an allocation.

---

### P5 — (found while verifying) reusable flip-event and response buffers, `server.rs` + `pod.rs`

Invariant preserved: byte-identical frames, identical `total_len`, identical closing of
`result.fd_out`, identical log lines. `pod_write`'s new body writes the same bytes it used to
copy through a temporary: `resize` grows the tail and the unaligned write fills exactly
`size_of::<T>()` bytes of it, so padding is copied from `*v` as before.

```diff
--- a/modules/gndrm-daemon/src/pod.rs
+++ b/modules/gndrm-daemon/src/pod.rs
@@ -33,8 +33,12 @@
 pub fn pod_write<T: Copy>(out: &mut Vec<u8>, v: &T) {
-    let mut tmp = vec![0u8; core::mem::size_of::<T>()];
-    // SAFETY: tmp is exactly size_of::<T>() bytes; T: Copy.
+    let n = core::mem::size_of::<T>();
+    let start = out.len();
+    // Grow in place: a caller that reuses its buffer then allocates nothing.
+    out.resize(start + n, 0);
+    // SAFETY: the tail `start..start + n` is exactly `size_of::<T>()` bytes of
+    // `out` and was just reserved; `T: Copy`, so a byte-wise copy of `*v` into
+    // it is a valid `T`, and `write_unaligned` does not require alignment.
     unsafe {
-        core::ptr::write_unaligned(tmp.as_mut_ptr().cast::<T>(), *v);
+        core::ptr::write_unaligned(out.as_mut_ptr().add(start).cast::<T>(), *v);
     }
-    out.extend_from_slice(&tmp);
 }
```

```diff
--- a/modules/gndrm-daemon/src/server.rs
+++ b/modules/gndrm-daemon/src/server.rs
@@ -39,1 +39,2 @@
+use gndrm_uapi::drm::DrmEventVblank;
 use gndrm_uapi::wire::{ReqHdr, RespHdr, MAX_FRAME_LEN, REQ_HDR_LEN, RESP_HDR_LEN};
@@ -381,2 +382,7 @@
     let mut buf = vec![0u8; MAX_FRAME_LEN];
     let flip_fd = dev.flip_event_fd();
+    // Reused per event/response: one flip event is one frame's worth of pacing
+    // signal and one response is one syscall's worth of work, so neither may
+    // allocate on the way out.
+    let mut ev_bytes: Vec<u8> = Vec::with_capacity(core::mem::size_of::<DrmEventVblank>());
+    let mut frame: Vec<u8> = Vec::with_capacity(RESP_HDR_LEN);
@@ -414,4 +420,4 @@
             for ev in dev.drain_flip_events() {
-                let mut bytes = Vec::new();
-                pod_write(&mut bytes, &ev);
-                if let Err(e) = send_frame(event_fd, &bytes, None) {
+                ev_bytes.clear();
+                pod_write(&mut ev_bytes, &ev);
+                if let Err(e) = send_frame(event_fd, &ev_bytes, None) {
@@ -451,2 +457,3 @@
-        let mut frame = resp.encode().to_vec();
+        frame.clear();
+        frame.extend_from_slice(&resp.encode());
         frame.extend_from_slice(&result.body);
```

`DrmEventVblank` is not imported in `server.rs` today (`:33` lists only the `wire` items), hence the
first hunk; it is public as `gndrm_uapi::drm::DrmEventVblank` (the same path `commit.rs:21` gets
via its glob import). Its size is 32 bytes (`drm.rs:102`: 8 + 8 + 4×4), so `ev_bytes` never grows
after the first event.

### P6 — (found while verifying) the census is built only when it can print, `commit.rs` + `lib.rs`

Invariant preserved: the *same set* of `planes #N:` lines with the *same text*. The census text is
a pure function of the two `PlaneState`s, so an unchanged `(primary, cursor)` pair means an
unchanged string — and an unchanged string today prints nothing unless the rate policy fires.
The new `due` term reproduces the rate policy, and the `changed` test is still made against the
stored text, not against the key, so the printed decision is bit-identical. `Device` gains one
small `Option` field; `PlaneState` is `Copy + PartialEq` (`objects.rs:165`).

```diff
--- a/modules/gndrm-daemon/src/lib.rs
+++ b/modules/gndrm-daemon/src/lib.rs
@@ -687,2 +687,5 @@
     pub(crate) plane_census: String,
+    /// The plane state the census string was last built from: an unchanged
+    /// pair cannot change the text, so the per-commit build is skipped.
+    pub(crate) census_key: Option<(PlaneState, PlaneState)>,
 }
@@ -802,2 +805,3 @@
             plane_census: String::new(),
+            census_key: None,
         }
```

```diff
--- a/modules/gndrm-daemon/src/commit.rs
+++ b/modules/gndrm-daemon/src/commit.rs
@@ -242,15 +242,24 @@
         if self.diag_on() {
-            let census = self.plane_census_text();
-            let changed = census != self.plane_census;
-            if changed {
-                self.plane_census = census.clone();
-            }
-            if changed || self.commit_count <= 30 || self.commit_count.is_multiple_of(120) {
-                self.diag(|| {
-                    format!(
-                        "{}",
-                        gndrm_obf::obf!("planes #{}: {}", self.commit_count, census)
-                    )
-                });
+            // The text is a function of the two plane states alone, so the
+            // build is skipped when they are unchanged — except on the rate
+            // policy's own samples, which need the text to print it. The
+            // `changed` decision is still made against the stored text.
+            let key = (self.primary_state(), self.cursor_state());
+            let due = self.commit_count <= 30 || self.commit_count.is_multiple_of(120);
+            if due || self.census_key != Some(key) {
+                self.census_key = Some(key);
+                let census = self.plane_census_text();
+                let changed = census != self.plane_census;
+                if changed {
+                    self.plane_census = census.clone();
+                }
+                if changed || due {
+                    self.diag(|| {
+                        format!(
+                            "{}",
+                            gndrm_obf::obf!("planes #{}: {}", self.commit_count, census)
+                        )
+                    });
+                }
             }
         }
```

---

## 4. Ordering and independence

All six patches are independent: none reads another's new code, and each compiles alone.
Suggested order is by (risk × review burden) per line changed, which is the reverse of the
cost ordering:

| order | patch | touches | why here |
|---|---|---|---|
| 1 | **P4** | `gndrm-android/src/lib.rs`, 6 hunks | largest per-frame cost, smallest diff, no shared types; the only patch whose win is visible on one thread's profile without a diff of statistics |
| 2 | **P3** | `asc.rs` (3 hunks), `pushes.rs` (1 fn) | second-largest, self-contained, and it removes a `Mutex`+alloc pair per turn from the same thread P4 is measured on — take it before measuring P4's residual so the two do not have to be separated |
| 3 | **P5**, **P6** | `server.rs`, `pod.rs`, `commit.rs`, daemon `lib.rs` | cheap, mechanical, but they change helpers used elsewhere (`pod_write`) and a diagnostic's internals; land them *before* any before/after CPU run so the baseline is the code that ships |
| 4 | **P1**, **P2** | `objects.rs`, daemon `lib.rs` (5 mutation sites + the job field), `commit.rs` | the only patches that introduce state (a refcount table, a cached `Arc`); they need the `debug_assert` recount exercised by the whole test suite, so they go last and are reviewed as one pair on the same call path |

P1 and P2 are adjacent in `commit.rs` but touch different functions (`apply_commit` vs
`parse_atomic`), at line 497 and 299 respectively, so `git apply` order does not matter — and
only P1's commit.rs hunks overlap the region P6 edits.

**Do not** merge P1 with P2 into one review: P1 changes a lifetime/identity contract
(`CommitJob::live_inos` type, thus `Presenter` argument sources), P2 changes only a decoder's
internals. P1 is the only patch here that can corrupt the display if it is wrong; P2 can only
return an errno.

---

## 5. Measurement

Publish rate is available from markers that already exist — no new instrumentation:

| marker | gate | what it counts | where |
|---|---|---|---|
| `ATOMIC heartbeat #N` | `GNDRM_LOG_SHIM=1` (container `envVars`) | gamescope→daemon atomic commits = the daemon's commit rate | `gndrm_shim.c:226` |
| `proxy commit: … #N` | `GNDRM_LOG_VKSHIM=1` (container) | guest presents | `proxy.rs` |
| `gndrm-daemon: commit #N fb=… prev_fb=… crtc_on=… fbs=` | `GNDRM_LOG_DAEMON` (app process) | the daemon's own commit count, plus `fbs=` = the FB table size (a bound on `N` in F1) | `commit.rs:205`, `:209` |
| `asc hdr decision n=… fb=… ino=…` | `GNDRM_LOG_DAEMON` | publish index, every 121st publish (`PUBLISH_LOG_EVERY`, `asc.rs:123`) | `asc.rs:1536` |

Rate = Δ`#`/Δt from either of the first two, or Δ`n`×121/Δt from the fourth, and `n` against
`commit #N` is the publish-vs-commit ratio.

**Per-thread CPU.** Jiffies are too coarse for this: 0.9–2.3 ms/s is 0.01–0.02 ticks/s at
`USER_HZ`=100. Use `simpleperf` against a tid:

```
adb shell simpleperf stat -t <tid of gndrm-commit|gndrm-scanout|gndrm> \
    -e cpu-clock --duration 120 --app app.gamenative
```
(`ps -T -p <pid> -o tid,comm` names the threads; the names are `gndrm`, `gndrm-commit`,
`gndrm-scanout`, `gndrm-warm`, `gndrm-diag`.) Report **ns/frame = Δcpu-clock / ΔN**, never
ns/s: the point of a fixed rate is that ΔN is constant across arms.

Protocol, per arm: same scene, same save, cap the session (`nativeSetFpsLimit` → `FRAME_CAP`, or
gamescope's own limit) at a value the panel sustains so the produce rate is stable and the
pipeline stays saturated; 120 s window after settling; two runs per arm; report both.

**Allocation counts, which are the decisive proof and are cheap.** On the host, run the existing
harness `gndrm-hostd` (`modules/gndrm-daemon/src/bin/gndrm-hostd.rs`) against a synthetic atomic
stream and count allocations:

```
valgrind --tool=dhat ./target/release/gndrm-hostd … 4000 commits
```
Expected, per 1000 commits: **before** ≈ 6000 allocations on the commit path (F1 1 + F2 5, plus
the response frame F6 and flip event F5 on the same thread); **after P1+P2** ≈ 1000 (the one
`sets` vector); the `Arc` cache means the snapshot itself contributes zero after the first
commit. Per 1000 scan-out turns: 2000 → 0. On device, the equivalent is `simpleperf record -g`
on `gndrm`/`gndrm-scanout` and the share in `malloc`/`free`/scudo before and after — which is
exactly the falsifier doc 24 §4 E names.

**One measurement hazard, stated because it inverts a result.** With `GNDRM_LOG_DAEMON=1` the
commit path builds the plane census (F7) on every commit. If P6 is not landed first, a
before/after run made with the log gate on measures the census *plus* the fix, and the "after"
arm looks worse than it should. Either land P6 first, or take the CPU number with the gate off
and read the rate from `ATOMIC heartbeat #N` (guest side, no app-process env needed).

**Expected result, so a null is not read as a failure.** Total ≈ 0.9–2.3 ms/s across three
threads, on a device whose daemon burns ~25 % of a core (doc 24 §2 per-process table) — i.e.
≤0.5 % of that process's own CPU. In a 120 s window that is 100–280 ms of one thread; visible
with `simpleperf stat`, invisible in `top`. Judge item 4 on (a) allocations/frame = 0 on all
three threads, (b) ns/frame down by the predicted amount, and (c) no change in fps, frame-time
p99 or the log shape — not on a package-power delta, which this cannot produce.

---

## 6. Rejected alternatives

- **`gem_by_ino` as the live set** (`objects.rs:202`) — no allocation, no new state, and wrong:
  `ioctl_gem_close` drops the inode from the index before orphaning the object (`lib.rs:1910`,
  `:1912`), and `ADDFB2`'s fresh handles bypass the index on purpose (`lib.rs:2047`). Either
  makes it a strict subset ⇒ `retire()` (`scanout.rs:524`) would free a buffer the client still
  imports. Rejected on correctness, not cost.
- **A generation counter bumped at every GEM mutation, no refcounts** — the simple version
  rebuilds per commit, because gamescope "opens/closes a handle per buffer" (`lib.rs:1964`
  comment) and `ADDFB2`/`RMFB` run per frame. Membership-aware counting (P1) is the minimum that
  can skip a frame.
- **A `SmallVec`/fixed-capacity array for the parse** — the bound is `64 × 64 = 4096` triples at
  24 B, i.e. a 96 KiB stack frame for the one allocation that is left. Rejected on stack cost;
  the four scratch arrays P2 removes are the ones worth removing.
- **A `Device`-owned reusable `sets` buffer** — `parse_atomic` takes `&self` and `apply_commit`
  takes `&mut self`, so the buffer would have to be `mem::take`n out and put back on six return
  paths, for one ~1.4 KB allocation per commit. Rejected as risk > reward.
- **Batching the `onFramePublished` upcall** (one call per N frames, or a count argument) — the
  real win, since it would remove the JNI transition itself and not just the lookup, but it
  changes the Kotlin API and the app's frame-ring timing: out of scope for a no-behaviour-change
  item. Worth a separate item with the app owner.
- **Caching a `JNIEnv` per thread** — a lifetime hazard for a ~50–100 ns `GetEnv`, and not an
  allocation. Not done.
- **Attacking F7 by reformatting the census with `write!` into a reused `String`** — preserves
  the text only by hand, and the text is a log contract. P6 gets the same saving by not building
  it at all, without touching the formatter.

---

## 7. Files touched by the diffs (for the applying change)

| patch | files | hunks |
|---|---|---|
| P1 | `modules/gndrm-daemon/src/{objects.rs,lib.rs,commit.rs}` | 4 + 6 + 3 |
| P2 | `modules/gndrm-daemon/src/commit.rs` | 1 |
| P3 | `modules/gndrm-android/src/asc.rs`, `modules/gndrm-daemon/src/pushes.rs` | 3 + 1 |
| P4 | `modules/gndrm-android/src/lib.rs` | 6 |
| P5 | `modules/gndrm-daemon/src/{server.rs,pod.rs}` | 4 + 1 |
| P6 | `modules/gndrm-daemon/src/{commit.rs,lib.rs}` | 1 + 2 |

Zero-warning notes: P1 removes `commit.rs:18`'s `HashSet` import (its only user was the deleted
collect) — leaving it would be an `unused_imports` warning, which these diffs do not. P3 replaces
`fds_except` rather than adding alongside it, so no dead `pub fn`. P4 keeps every existing log
string, so no `clippy::literal_string_with_formatting_args` churn. P6's new field is
`pub(crate)` like its neighbours, and `PlaneState` is already in scope in the daemon `lib.rs`
(`pub use objects::{… PlaneState …}`, `lib.rs:42`).
