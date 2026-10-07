# 02 — gamenative-drm: Rust userspace DRM daemon

Repo: `/data/workspace/Projects/gamenative-drm` (`master`)
Deliverable: `libgamenative_drm.so` (JNI cdylib, loaded by the GameNative app) +
`libgndrm_shim.so` (C, LD_PRELOAD'd into the gamescope process) +
`gndrm-uapi` (shared wire/UAPI definitions).

Reference UAPI: [`include/uapi/drm/drm.h`](https://github.com/torvalds/linux/blob/master/include/uapi/drm/drm.h),
[`drm_mode.h`](https://github.com/torvalds/linux/blob/master/include/uapi/drm/drm_mode.h)
(the same headers libdrm builds against — the shim uses system headers, the Rust
side mirrors them with `#[repr(C)]`).

## 1. Design principle

Gamescope's DRM backend talks to the kernel through exactly two syscalls on the
device fd: **`ioctl`** (everything: discovery, FBs, commits — always via
libdrm's `drmIoctl`) and **`read`** (event delivery inside `drmHandleEvent`),
plus `poll`/`epoll` for readiness. It never mmaps the device, never writes to
it. Therefore:

> We never emulate a device file. We emulate **one connected fd**: ioctls are
> intercepted and turned into RPC; `read()` stays a plain socket read carrying
> raw `struct drm_event` bytes. Everything else (libdrm's `drmMode*` helpers,
> `drmHandleEvent` parsing, gamescope itself) runs unmodified.

The emulated fd also survives **fd-table plumbing**, exactly like a real KMS
fd: `dup`/`dup2`/`dup3`/`fcntl(F_DUPFD*)` are interposed and tracked in an
fd map (refcounted; the connection dies with the last reference). libliftoff's
`liftoff_device_create` does `device->drm_fd = dup(drm_fd)` and issues every
ioctl on the copy — an untracked dup would send those ioctls to the real kernel
on a socket fd and fail with `ENOTTY`.

Mandatory subset first; every optional feature is gated by the cap/property
gamescope already queries (see 01-architecture.md §2). If the daemon doesn't
advertise it, gamescope takes its own fallback path.

## 2. Module layout

```
gamenative-drm/
├── Cargo.toml            (workspace)
├── modules/
│   ├── gndrm-uapi/       # #[repr(C)] structs + ioctl numbers (drm.h/drm_mode.h subset),
│   │                     #   wire framing codec; forbid(unsafe_code); shared by daemon & tests
│   ├── gndrm-daemon/     # the KMS emulator core (no Android deps, unit-testable on host)
│   │                     #   object store: connector/CRTC/plane/FB/props/blobs
│   │                     #   ioctl dispatch → response builder; + gndrm-hostd test binary
│   ├── gndrm-android/    # JNI cdylib: socket server loop, SurfaceControl presenter,
│   │                     #   scan-out AHardwareBuffer pool, commit worker, input injector
│   └── gndrm-shim/       # C (not Rust): LD_PRELOAD ioctl/open interceptor
│                         #   tiny, no deps beyond libc; built with NDK clang directly
└── android/              # Kotlin-facing JNI API surface docs, test harness
```

`gndrm-daemon` compiles and passes its unit tests on the host (x86_64-linux) —
the ioctl semantics are pure data transformation; only `gndrm-android` touches
Android APIs.

## 3. UAPI surface

### 3.1 Core device ioctls (drm.h)

| ioctl | struct | daemon behavior |
|---|---|---|
| `DRM_IOCTL_VERSION` | `drm_version` | name="gamenative", date, desc; two-call length semantics |
| `DRM_IOCTL_GET_CAP` | `drm_get_cap` | caps table below |
| `DRM_IOCTL_SET_CLIENT_CAP` | `drm_set_client_cap` | accept `ATOMIC`, `UNIVERSAL_PLANES`, `WRITEBACK_CONNECTORS`; else EINVAL |
| `DRM_IOCTL_MODE_GETRESOURCES` | `drm_mode_card_res` | 1 fbid pool, 1 CRTC, 1 connector, 1 encoder; min/max size = panel caps |
| `DRM_IOCTL_GEM_CLOSE` | `drm_gem_close` | drop handle; if a live FB references the GEM, move it to the orphan table (handle stops resolving, object dies at the RMFB of its last FB) — kernel semantics; gamescope closes handles right after ADDFB2 |
| `DRM_IOCTL_PRIME_FD_TO_HANDLE` | `drm_prime_handle` | SCM_RIGHTS-received dma-buf fd → store, return GEM handle |
| `DRM_IOCTL_PRIME_HANDLE_TO_FD` | `drm_prime_handle` | reverse (returns the pool buffer's fd) |

Caps reported: `DRM_CAP_DUMB_BUFFER=1`, `DRM_CAP_CURSOR_WIDTH/HEIGHT=64`,
`DRM_CAP_TIMESTAMP_MONOTONIC=1`, **`DRM_CAP_SYNCOBJ=1`**,
**`DRM_CAP_ADDFB2_MODIFIERS=0`** (linear pool buffers); every other cap →
EINVAL (probing clients treat that as unsupported).

### 3.2 KMS object ioctls (drm_mode.h)

| ioctl | notes |
|---|---|
| `DRM_IOCTL_MODE_GETCONNECTOR` | 1 connector, `DRM_MODE_CONNECTED`, type `DRM_MODE_CONNECTOR_DSI`; modes: panel mode from the SurfaceView size; two-call count/array semantics; `count_props` exposes the connector props |
| `DRM_IOCTL_MODE_GETENCODER` | 1 encoder, `possible_crtcs=0x1`, `possible_clones=0` |
| `DRM_IOCTL_MODE_GETCRTC` | current mode = panel mode |
| `DRM_IOCTL_MODE_SETCRTC` | never called on the atomic path; EINVAL (gamescope is atomic-only) |
| `DRM_IOCTL_MODE_GETPLANERESOURCES` | 2 planes: primary + cursor |
| `DRM_IOCTL_MODE_GETPLANE` | formats: primary `XR24`, `AR24` (BGRA-backed); cursor `AR24`; `possible_crtcs=0x1` |
| `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` | per-object-type property list with current values |
| `DRM_IOCTL_MODE_GETPROPERTY` | property metadata: name, flags (range/enum/bitmask/blob), values |
| `DRM_IOCTL_MODE_OBJ_SETPROPERTY` | EINVAL — the only props gamescope sets directly at init are optional ones the daemon deliberately does not expose, so this never fires |
| `DRM_IOCTL_MODE_CREATEPROPBLOB` / `DESTROYPROPBLOB` / `GETPROPBLOB` | blob store (used for `MODE_ID`; gamescope also creates `CTM`/degamma blobs) |
| `DRM_IOCTL_MODE_ADDFB2` | validate against stored PRIME handles; register FB{id→(handles,w,h,fmt,strides,offsets)} |
| `DRM_IOCTL_MODE_RMFB` | drop FB; `EBUSY` if still scanned out (kernel parity); handles stay owned by the client |
| `DRM_IOCTL_MODE_GETFB2` | implemented (returns the stored FB descriptor) |
| `DRM_IOCTL_MODE_ATOMIC` | parse `drm_mode_atomic` (pointer-containing struct: shim marshals the pointed-to arrays into the RPC); apply to pending state; on `DRM_MODE_ATOMIC_COMMIT`: validate, queue, return 0 (presentation is async — see 07) |
| `DRM_IOCTL_MODE_PAGE_FLIP` | unused by this gamescope; ENOSYS |

Property set emulated (names match exactly — gamescope looks them up by string):

- plane: `FB_ID`, `CRTC_ID`, `SRC_X/Y/W/H`, `CRTC_X/Y/W/H`, `type` (IMMUTABLE
  enum), `IN_FENCE_FD` (**SIGNED_RANGE [-1, INT_MAX]** — kernel
  `drm_mode_config.c:488`; an unsigned range makes libliftoff's client-side
  `check_range_prop` reject the -1 "no fence" sentinel), `OUT_FENCE_PTR`.
  `IN_FENCE_FD` fds arrive via SCM_RIGHTS; the daemon takes ownership and hands
  the fence to the presenter as the acquire fence.
- crtc: `MODE_ID` (blob), `ACTIVE`
- connector: `CRTC_ID`, `panel orientation` (IMMUTABLE enum — set from the
  Android display rotation; the daemon exposes rotate-0 only, so gamescope
  composites rotation itself and the daemon always presents panel-orientation
  buffers)
- optional (queried-and-skipped by gamescope when absent): `zpos`, `VRR_ENABLED`,
  `CTM`, `GAMMA_LUT`, `DEGAMMA_LUT`, `non-desktop`, `Colorspace`,
  `HDR_OUTPUT_METADATA`, `content type`, `EDID`. Gamescope's color-mgmt path
  *creates* CTM/LUT blobs when active; CREATEPROPBLOB works unconditionally, and
  absent props mean the blobs are never referenced in a commit.

Commit flags: `DRM_MODE_ATOMIC_TEST_ONLY` → validate, don't present.
`DRM_MODE_PAGE_FLIP_ASYNC` → accepted and honored. `DRM_MODE_ATOMIC_ALLOW_MODESET`,
`NONBLOCK`, `PAGE_FLIP_EVENT` → honored (see 07 for the async commit model).

### 3.3 Events (read side, no ioctl)

Daemon → **event channel** (the second SEQPACKET connection, §4): kernel-ABI
`struct drm_event_vblank` packets, `type=DRM_EVENT_FLIP_COMPLETE`, carrying
sequence + `tv_sec/tv_usec` (CLOCK_MONOTONIC, matching the advertised cap) and
the user_data cookie from the commit. Emitted by the commit worker with the
publish — the frame is queued to SurfaceFlinger, and the event reports the
vblank that flip claims (see 07).

### 3.4 Explicitly out of scope (return ENOSYS/EINVAL, logged)

leases (`MODE_CREATE_LEASE`…), writeback connectors, dumb-buffer ioctls
(`MODE_CREATE_DUMB`/`MAP_DUMB`/`DESTROY_DUMB`), `MODE_DIRTYFB`, `MODE_GETFB`,
`WAIT_VBLANK` (gamescope uses the event queue + timerfd), `MODE_PAGE_FLIP` +
`MODE_SETCRTC` (atomic-only backend), `SET_MASTER`/`DROP_MASTER`/`AUTH_MAGIC` →
return 0 (always master).

### 3.5 gamescope DRM call surface

Every `drm*`/`drmMode*` call site in `src/` (excluding reshade). This is the
*complete* libdrm surface; each row's ioctl mapping is what the daemon
implements.

**KMS helpers (all in `Backends/DRMBackend.cpp`):**
`GetResources`(:847,:929), `GetConnector`(:860,:2174), `ConnectorGetPossibleCrtcs`
(:2205 — libdrm-internal composition over GETCONNECTOR+GETENCODER, no new
ioctl), `GetCrtc`(:939,:2941), `GetPlaneResources`(:946), `GetPlane`(:956,:2929),
`ObjectGetProperties`(:2011), `GetProperty`(:2022,:3364), `GetPropertyBlob`
(:676,:2304), `CreatePropertyBlob`(:3958,:3963), `DestroyPropertyBlob`(:4086),
`AddFB2`(:1673), `RmFB`(:2631), `AtomicAlloc/AddProperty/Commit/Free`
(:1486…:4135), `GetConnectorTypeName`(:2208, userspace), `Free*` (userspace).

**Core libdrm:** `GetCap`(:1296-:1324), `SetClientCap`(:1291), `IsKMS`(:1284 →
GETRESOURCES), `HandleEvent`(:838 → `read()`), `PrimeFDToHandle`(:1647),
direct `drmIoctl`: `GEM_CLOSE`(:1701), syncobj ioctls (Timeline.cpp),
`GetDeviceFromDevId`(:1264, sysfs — skipped by patch D1).

**Zero** GBM symbols anywhere. **Zero** dumb-buffer, GETFB, DIRTYFB, PAGE_FLIP,
SETCRTC, lease, writeback call sites.

Event contract: `drmHandleEvent` uses a v3+ `drmEventContext` — flip handler
signature `(fd, frame, sec, usec, crtc_id, user_data)` (DRMBackend.cpp:759) →
daemon fills `struct drm_event_vblank` including `crtc_id`. Atomic flags seen in
commits: `ALLOW_MODESET`, `NONBLOCK`, `PAGE_FLIP_ASYNC`, `PAGE_FLIP_EVENT` —
daemon honors NONBLOCK (apply+queue, reply immediately, event on present) and
EVENT (send `DRM_EVENT_FLIP_COMPLETE`).

## 4. Wire protocol (versioned, little-endian)

**Two** `SOCK_SEQPACKET` connections, not one: the shim connects twice — first
connection is the RPC channel, second is the event channel (connect order is the
handshake). This avoids any response-vs-event demux race in the shim's
`ioctl()` (which must block on `recvmsg` while unsolicited events may be in
flight), and lets the event fd handed to gamescope be a real socket that
`poll`/`read`/`epoll` service natively — no `read` interception at all. One
frame = one `sendmsg` = one `recvmsg`; `SCM_RIGHTS` carries dma-buf/fence fds
out of band.

```
→ req:  [24B] magic "GND1" | u32 total_len | u32 ioctl_nr | u32 flags | u64 seq
        + ioctl struct verbatim + linearized extension arrays   [+ fd if FLAG_FD]
← resp: [24B] magic | u32 total_len | u64 seq | i32 ret(-errno) | u32 out_len
        + out struct verbatim + linearized extension arrays     [+ fd via SCM_RIGHTS]
E     : raw `struct drm_event_vblank` packets (one per frame), kernel-ABI bytes
```

- `ioctl_nr` = the real `DRM_IOCTL_*` number (computed in `gndrm-uapi::ioctl`,
  cross-checked against the C headers at test time).
- Pointer-carrying UAPI structs are **linearized**: the fixed struct travels
  verbatim (its userspace pointers are ignored), arrays follow in field order.
  Two-call semantics: the **daemon truncates to the offered capacity** and
  returns full counts in the struct; the shim copies what arrived into the
  caller's buffers (identical net effect to the kernel).
- ATOMIC extension layout: `objs[n] | counts[n] | prop_ids[sum] | values[sum]`.
- `IN_FENCE_FD`: the shim discovers the property id at init (name lookup over
  OBJ_GETPROPERTIES/GETPROPERTY RPCs) and attaches the fd to the frame; ≤1 fence
  per commit (gamescope only fences the primary plane).
- **Scan-out buffer request:** the shim asks the daemon for a pool buffer by
  geometry and receives a dup'd dma-buf fd via `SCM_RIGHTS` (see §6 and §7).
- Input is **not** part of this protocol: input enters gamescope through the
  wayland virtual-input protocols from an in-app injector, so this channel stays
  display-only.

## 5. Shim (`gndrm-shim`, C)

- `GAMENATIVE_DRM_SOCKET` unset → fully inert (falls through to real libc).
  Filesystem or `@abstract` socket paths. Set + `/dev/dri/card*` path → intercept
  (wins even on hosts with a real card; that is what the host CI test needs).
- Intercepts `open`/`open64`/`openat` (returns the event-channel fd),
  `ioctl` (RPC marshal/de-linearize, per-ioctl walkers), `close` (tears down
  both channels). Single open at a time. No `read`/`poll`/`stat` interception —
  the event fd is a real socket; gamescope's patched open path means no
  path-stat games are needed.
- Intercepts `/dev/dma_heap/*` `open`/`ioctl(DMA_HEAP_IOCTL_ALLOC)` to source
  scan-out buffers from the daemon's AHardwareBuffer pool (see §7).
- Thread safety: one mutex around the RPC exchange (responses are matched by
  sequence number; single outstanding request per channel) — a synchronous
  request/reply client.
- Build gotchas encoded in `tools/build-shim.sh`: **no** `-fvisibility=hidden`
  (hides the interposed symbols → LD_PRELOAD silently no-ops), bionic declares
  `ioctl(int, int, ...)` vs glibc's `unsigned long` (prototype selected via
  `#ifdef __ANDROID__`).

## 6. Daemon core (`gndrm-daemon`, Rust)

- `struct Device { panel, mode, stores, caps, conn_crtc_id, crtc, primary,
  cursor, presenter, clock, flip_sequence, commit_queue }` with
  `Stores { gems, gem_by_ino, fbs, blobs }`. No separate pending/active
  `AtomicState`: `validate_commit` simulates the commit onto cloned `PlaneState`
  copies, `apply_commit` then writes live state; `TEST_ONLY` never applies.
  `OBJ_SETPROPERTY` is EINVAL — gamescope is atomic-only (§3.2).
- `fn dispatch(&mut self, ioctl_nr: u32, payload: &[u8], fd_in: Option<RawFd>)
  -> DispatchResult` where `DispatchResult { ret, body, fd_out, events }` — pure
  data transformation, host-unit-tested with golden fixtures (byte-in →
  byte-out + state assertions).
- KMS state machine: commit = validate (props belong & are mutable, referenced
  FB/blob/CRTC exist, modeset props require `ALLOW_MODESET`, FB'd plane has a
  CRTC) → queue → the commit worker applies and presents the primary FB when it
  changed under an active CRTC → queue `DRM_EVENT_FLIP_COMPLETE` (monotonic
  timestamp, sequence, user_data) on the event channel when `PAGE_FLIP_EVENT`,
  with the publish (see 07).
- `trait Presenter` — `fn publish(&mut self, fb: &Framebuffer,
  plane: &PlaneState, ino: Option<u64>, live_inos: &HashSet<u64>,
  fence: Option<RawFd>)`. It hands the frame to the compositor and returns;
  the worker does not wait on it.
  `gndrm-android` implements it with the SurfaceControl presenter;
  `NullPresenter` is used for host tests.
- `ImportedBuffer` (`GemObject`): dedup by `stat.st_ino` of the dma-buf fd
  (gamescope re-imports the same dma-buf multiple times — `drmPrimeFDToHandle`
  of the same buffer must return the **same** GEM handle, kernel semantics).
  A `GemObject` that backs a pool buffer carries a reference to the
  `AHardwareBuffer` it came from, which is what the presenter publishes.

## 7. Android half (`gndrm-android`): scan-out pool + SurfaceControl presenter

The presenter publishes committed buffers to Android. Its whole surface is
documented in 06-presenter-latency.md; the shape is:

- **Scan-out allocation is daemon-owned.** The daemon allocates the
  `AHardwareBuffer` pool (HAL format `BGRA_8888`, value 5, usage
  `GPU_SAMPLED_IMAGE | GPU_COLOR_OUTPUT | COMPOSER_OVERLAY`). The shim requests
  a pool buffer by geometry and receives a dup'd dma-buf fd; gamescope renders
  into it; the same `AHardwareBuffer` is published. No import, no copy.
- **Format identity.** BGRA is byte-for-byte `XRGB8888`/`ARGB8888`
  (`XR24`/`AR24`), so no conversion and no shader. The daemon advertises
  exactly these formats.
- **Publish tail.** `ASurfaceControl`/`ASurfaceTransaction`, resolved by
  `dlopen("libandroid.so")` (the NDK gates these symbols at API 29; the build
  targets API 28). One child `SurfaceControl` from the window, one layer per
  pool buffer, `setBuffer` + `setGeometry` + `apply`.
- **Back-pressure.** `ASurfaceTransactionStats_getPreviousReleaseFenceFd` gates
  slot reuse; a warm spare keeps one pre-allocated buffer ready.
- **Locks.** The scan-out pool and the presenter state each have their own
  lock; no lock is held across a gralloc or SurfaceFlinger call.
- **Threads.** The commit worker presents and emits the flip event; the scan-out
  allocator and a doorbell-driven warm worker keep the pool stocked; the input
  injector runs on its own thread (see §8).
- **Vsync.** `Choreographer` (via the `android` crate / JNI up-call) paces
  presents and supplies the flip-event timestamps; a 60 Hz timer thread takes
  over while the activity is paused (gamescope keeps committing while
  backgrounded).
- **Logging.** `__android_log_write`, tag `GN_DRM`.

## 8. JNI surface + threading model

JNI surface (class **`app.gamenative.display.GameNativeDrm`**; the cdylib ships
in the app's `jniLibs/arm64-v8a/` and is loaded via
`System.loadLibrary("gamenative_drm")`, linked `-Wl,-z,max-page-size=16384`):

- `nativeStart(socketName: String, surface: Surface, width: Int, height: Int): Int`
- `nativeStop()`, `nativeRetargetSurface(surface, w, h)`
- injector calls: `nativePointerAbsolute(x, y)`, `nativePointerMotion(dx, dy)`,
  `nativePointerButton(btn, pressed)`, `nativePointerAxis(axis, value)`,
  `nativeKey(evdevCode, pressed)`.

Threading (see 07 for the full concurrency summary):

- **Serve thread** — owns the `Device`, handles all ioctls (no lock on the
  device).
- **Commit worker (per CRTC)** — present → emit flip event.
- **Scan-out allocator thread** — owns the pool and warm spare (its own lock).
- **Warm worker** — doorbell-driven; keeps the warm spare ready.
- **Input injector thread** — wayland virtual-input client.
- **Shim** — synchronous request/reply client, one lock.
