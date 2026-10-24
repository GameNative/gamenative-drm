# 08 — Xwayland on the render node

Xwayland's GPU path (glamor/dri3/drm/present) runs against the same
gamenative-drm userspace device gamescope already drives, so an external
buffer — the guest's rendered frame — can become an X11 window's content as a
real dma-buf instead of through gamescope's content-override.

Device-side counterpart of `05-xwayland-reintroduction.md` (which describes the
shm-only shape) and `07-kernel-drm-parity.md` (commit/flip semantics).

## 1. Card node vs render node

One daemon connection serves exactly one node type, declared by the client at
open time. The two differ only in which ioctls the daemon permits and in how
much of the KMS object model the client uses:

| | card node (`/dev/dri/cardN`) | render node (`/dev/dri/renderDN`) |
|---|---|---|
| client | gamescope (`DRMBackend`) | Xwayland (glamor/dri3/drm) |
| connect | `gamenative_drm_connect()` or `open("/dev/dri/cardN")` | `open("/dev/dri/renderDN")` |
| declared via | node-kind frame `NODE_KIND_CARD` (default) | node-kind frame `NODE_KIND_RENDER` |
| modeset (CRTC/connector/plane/ADDFB2/ATOMIC) | yes | refused `EACCES` |
| master (SET_MASTER/DROP_MASTER) | accepted | refused `EACCES` |
| GEM/PRIME/syncobj | yes | yes |
| identification (VERSION/GET_CAP) | yes | yes |

A render-node connection is gated like the kernel's `DRM_RENDER_ALLOW` table:
identification and rendering (GEM/PRIME/syncobj) are allowed, modesetting and
master acquisition are not. The gate is what makes "this is a render node" an
observable property rather than a label — a render client cannot accidentally
modeset.

Only Xwayland's render-node opens go to the daemon. In every other process
(gamescope, the guests) `/dev/dri/renderD*` reaches the kernel: a Vulkan driver
whose kernel interface is a render node (PowerVR on Tensor G5) runs there, and
the daemon's node is not its GPU. In Xwayland too, an open by an OS library, or
one made while the daemon connection is already held, reaches the kernel.
`GNDRM_RENDER_NODE_CLIENT=1` makes any process a render-node client (the host
harness's test clients). The shim logs every `/dev/dri` decision with the
opener's library.

The node kind is per connection, not per device, because the daemon is the
device: both clients connect to the same `Device` implementation and the wire
protocol is identical; only the permitted request set and the object model
reachable behind it differ.

## 2. ioctl inventory

Request numbers are the exact encoded `_IOC` values the kernel dispatches on
(all `asm-generic`, LP64). The daemon's frame header carries the full number,
so the shim never needs to know more than the number.

**Existing (served on both node types unless noted):**

| request | value | notes |
|---|---|---|
| `DRM_IOCTL_VERSION` | `0xc0406400` | driver name/date/desc |
| `DRM_IOCTL_GET_CAP` | `0xc010640c` | see caps below |
| `DRM_IOCTL_SET_CLIENT_CAP` | `0xc008640d` | ATOMIC / UNIVERSAL_PLANES / WRITEBACK |
| `DRM_IOCTL_SET_MASTER` | `0x0000641e` | card only |
| `DRM_IOCTL_DROP_MASTER` | `0x0000641f` | card only |
| `DRM_IOCTL_MODE_GETRESOURCES` | `0xc04064a0` | card only |
| `DRM_IOCTL_MODE_GETCRTC` | `0xc06864a1` | card only |
| `DRM_IOCTL_MODE_GETCONNECTOR` | `0xc05064a7` | card only |
| `DRM_IOCTL_MODE_GETENCODER` | `0xc01464a6` | card only |
| `DRM_IOCTL_MODE_GETPLANERESOURCES` | `0xc01064b5` | card only |
| `DRM_IOCTL_MODE_GETPLANE` | `0xc02064b6` | card only |
| `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` | `0xc02064b9` | card only |
| `DRM_IOCTL_MODE_GETPROPERTY` | `0xc04064aa` | card only |
| `DRM_IOCTL_MODE_CREATEPROPBLOB` | `0xc01064bd` | card only |
| `DRM_IOCTL_MODE_GETPROPBLOB` | `0xc01064ac` | card only |
| `DRM_IOCTL_MODE_DESTROYPROPBLOB` | `0x400464be` | card only |
| `DRM_IOCTL_MODE_ADDFB2` | `0xc06864b8` | card only |
| `DRM_IOCTL_MODE_GETFB2` | `0xc06864ce` | card only |
| `DRM_IOCTL_MODE_RMFB` | `0x400464bb` | card only |
| `DRM_IOCTL_MODE_ATOMIC` | `0xc03864bc` | card only |
| `DRM_IOCTL_GEM_CLOSE` | `0x40086409` | both |
| `DRM_IOCTL_PRIME_HANDLE_TO_FD` | `0xc00c642d` | both |
| `DRM_IOCTL_PRIME_FD_TO_HANDLE` | `0xc00c642e` | both |
| `DRM_IOCTL_SYNCOBJ_CREATE` | `0xc00864bf` | both |
| `DRM_IOCTL_SYNCOBJ_DESTROY` | `0xc00864c0` | both |
| `DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD` | `0xc01864c1` | both |
| `DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE` | `0xc01864c2` | both (refused: needs real dma-fence import) |

**Newly implemented for the render node:**

| request | value | notes |
|---|---|---|
| `DRM_IOCTL_GEM_CREATE` | `0xc010641b` | allocate a buffer object; returns a handle |
| `DRM_IOCTL_GEM_MMAP` | `0xc018641e` | mint the mmap offset that carries the handle |
| `GNDRM_IOCTL_NODE_KIND` | `0x474e0001` | connection setup (not a kernel ioctl): declare card vs render |

`GEM_CREATE` and `GEM_MMAP` are the legacy *generic* GEM ioctls; modern
`drm.h` carries only `GEM_CLOSE`/`FLINK`/`OPEN`, so their encodings are pinned
by a Rust unit test rather than the C-header ground-truth test (which cannot
see them). `GNDRM_IOCTL_NODE_KIND` sits far outside the DRM request space so
it can never collide with a real ioctl.

`GEM_CREATE` allocates a real, CPU-mappable buffer (an anonymous memfd sized to
the request) and stores it in the same GEM table `PRIME_FD_TO_HANDLE` fills, so
a created object and an imported one are indistinguishable afterwards: both
dedup by `st_ino`, both export through `HANDLE_TO_FD`, both can back an
`ADDFB2`. `GEM_MMAP` returns `GEM_MMAP_OFFSET_BASE | handle`; the shim
recognises that token on `mmap(dev_fd, offset)` and maps the handle's dma-buf
in place of the socket.

**`GET_CAP` values:** `DUMB_BUFFER` (1) = 1; `PRIME` (5) =
`IMPORT|EXPORT` (3); `TIMESTAMP_MONOTONIC` (6) = 1; `CURSOR_WIDTH` (8) /
`CURSOR_HEIGHT` (9) = 64; `ADDFB2_MODIFIERS` (0x10) = 1; `SYNCOBJ` (0x13) = 1;
`SYNCOBJ_TIMELINE` (0x14) = 0. `PRIME` and `DUMB_BUFFER` are the caps Mesa's
`gbm_create_device` and glamor's device probe read first, and
`SYNCOBJ_TIMELINE` is the one Xwayland's `xwl_gbm_supports_syncobjs` reads
before it will use explicit sync — a kernel answers an unsupported cap with
value 0 and success, so the daemon does too rather than erroring. An unknown
capability not listed here is refused `EINVAL`, exactly as the kernel does.

The daemon answers **every** request a connection sends: `dispatch` has a
catch-all that returns `EINVAL` for a request number it does not model, and a
render-node connection gates the request against the `DRM_RENDER_ALLOW` table
first (so a modesetting/master request is refused `EACCES`). The shim therefore
never invents an errno of its own: a request it does not specially marshal is
forwarded to the daemon (generic forward), so the render-channel contract has
no silent holes and every request gets the daemon's answer.

## 3. Routing: `libgndrm_shim.so` + `GAMENATIVE_DRM_SOCKET`

When `GAMENATIVE_DRM_SOCKET` is set the shim is `LD_PRELOAD`ed and the DRM fd
is a connected `SOCK_SEQPACKET` to the daemon (D1, unchanged):

```
open("/dev/dri/cardN")   ──┐
open("/dev/dri/renderDN") ─┤ intercepted when the env var is set
gamenative_drm_connect() ──┘        │
                                    ├─ card:      return the event-channel socket
                                    └─ render:    send GNDRM_IOCTL_NODE_KIND=RENDER,
                                                  then return the event-channel socket
ioctl(fd, req, arg)  → drm_ioctl(): marshal one RPC frame → daemon → de-linearize
mmap(fd, offset)     → gem-token offset: map the handle's dma-buf (PRIME export)
close/dup/dup2/dup3/fcntl(F_DUPFD) → keep the fd-set coherent (libliftoff dups the fd)
read/poll/epoll      → native: the fd is a real socket delivering drm_event bytes
```

Xwayland acquires its render node in `xwayland-glamor-gbm.c`. Normally it
resolves the compositor's linux-dmabuf main device and probes it with
`drmGetDevice2`/`drmGetNodeTypeFromFd`; a socket has no `/sys` node, so under
`GAMENATIVE_DRM_SOCKET` the node path comes from the environment
(`GAMENATIVE_DRM_NODE`, default `/dev/dri/renderD128`) and the fd is treated as
a render node directly. Both acquisition sites (`xwl_drm_handle_device` from
`wl_drm`, and `xwl_glamor_gbm_init_main_dev`/`xwl_glamor_gbm_init_egl`) take the
same path.

**Observed node, and why the present path does not depend on it.** A device
capture showed only the *guest*'s Vulkan driver completing the `gndrm` render
handshake, not Xwayland: Xwayland's glamor/DRI3 device is then the real
`/dev/dri/renderD128` msm node, not the userspace daemon. That is consistent
with the two-fd hazard in section 6 — glamor needs a real GL device, and one fd
cannot be both the daemon (generic GEM only) and the GPU allocator. The DRI3
present path is node-agnostic: the buffer handed to Xwayland is the shim's own
allocation (the daemon scan-out-pool AHB or a dma-heap buffer), a standard
Android dma-buf that either node's import path accepts, so no per-node
adjustment is required.

## 4. Xwayland build

`android/build-xwayland.sh` configures:

```
-Dglamor=true -Dglx=false -Ddri3=true -Ddrm=true
```

plus the shm/xkb options it already had. `present` is unconditional in this
tree (built with `PRESENT 1`), so no flag selects it. glx stays off — no GLX
clients and it would pull a second GL configuration. The resulting config has
`GLAMOR 1`, `DRI3 1`, `PRESENT 1`, `GLAMOR_HAS_GBM 1`,
`GBM_BO_FD_FOR_PLANE 1` (set by meson's gbm API probe, so glamor compiles the
dma-buf path rather than `EGL_NATIVE_PIXMAP_KHR`), and the binary links
`libdrm`, `libgbm`, `libepoxy`.

Prefix dependencies (termux aarch64 prefix): `libdrm` (`drm.pc`), `libgbm`
(`gbm.pc`), `libepoxy` (`epoxy.pc`), `libEGL`, `libGLESv2`, `xcb-dri3`,
`xcb-present`, `xcb-sync`, `xcb-xfixes`, `presentproto`. The prefix ships
libgbm and its pkg-config but **not** `gbm.h` (only `gbm_backend_abi.h`); the
client header is vendored under `android/deps/gbm.h` and installed into the
build overlay prefix by the build script, the same mechanism the tree already
uses for xtrans.

## 5. Window-buffer flow (DRI3 + Present, implemented)

```
game (vulkan, turnip)
  └─ gn ICD shim: guest renders into a shim-owned dma-buf swapchain image
       │
       │  winex11.drv: vkCreateXlibSurfaceKHR(XID) → shim learns the window
       ▼
     shim X11 present path (`icd_shim/src/x11.rs`, on the guest's X connection)
       ├─ xcb_connect(DISPLAY) + DRI3/Present version probe + window geometry
       ├─ xcb_dri3_pixmap_from_buffer(fd, size, w, h, stride, depth, bpp)
       │   → Xwayland gbm_bo_import()s the dma-buf → glamor texture
       ├─ xcb_present_pixmap(window, pixmap, serial, idle_fence)
       │   → Present makes the pixmap the window's content; the idle fence
       │     triggers when the server is done with it
       └─ xcb_sync_query_fence(idle_fence) on the present worker: the fence
            firing is the slot-reuse contract (the wayland arm's
            wl_buffer.release)
            │
            ▼
          Xwayland composites the window's own content (glamor)
            │
            ▼
          gamescope composites the X11 window through its normal X11 path
```

The gamescope override is gone: `wlproto` no longer declares the swapchain
protocol and the shim's X11 arm no longer opens its own Wayland connection. A
window's single content is the guest's frame, so 2D content wine's GDI draws
into the same X window is composited by Xwayland alongside it.

What the implementation actually uses, and what it does not:

- **No `xcb_dri3_open`.** The shim already owns the imported dma-buf fd, so
  `PixmapFromBuffer` on that fd is all DRI3 needs; opening Xwayland's render
  node a second time buys nothing.
- **No `PixmapFromBuffers` / modifier.** DRI3 1.0's `PixmapFromBuffer` carries
  no modifier. Slots are LINEAR (`slot_init`'s explicit-LINEAR export;
  `GN_VKSHIM_SC_MOD` is the only way to ask for a tiled/compressed layout), so
  this suffices; a non-linear slot is refused with a named line, never
  presented wrong.
- **Release rides an X sync fence, not IdleNotify.** The fence is created once
  per slot (`xcb_sync_create_fence`), reset with `xcb_sync_reset_fence` before
  each present, and passed as `xcb_present_pixmap`'s `idle_fence`; the present
  worker polls it with `xcb_sync_query_fence`. It triggers when the pixmap is
  superseded by the next present — exactly when the slot is reusable. On the
  dev host's Xwayland, Present's IdleNotify/CompleteNotify events are not
  delivered for this window class at all, so events were not an option and the
  fence is.
- **No Present acquire fence.** Present's `wait_fence` needs a
  driver-exported `sync_file`; the adrenotools driver family exports none (the
  same reason the shim owns the swapchain and any completion wait is a queue
  idle, never a re-wait of the guest's signalled semaphores). GPU ordering
  rides the dma-buf's implicit reservation, exactly as the wayland arm's
  `wl_buffer` commit does.
- **Failure is bounded and named.** Connect, extension probe, geometry, pixmap
  import, fence create and present each log a named failure and drop the frame
  (freeing the slot), so a broken X path degrades to no video, never a hang.
  Every X call is a round trip to the local server; the present worker never
  blocks on an external event.

Status: implemented. The device surface (sections 1–3), the Xwayland build
(section 4) and the shim's X11 present arm are all in place; the previous
override / `gamescope_swapchain` path is deleted. Host coverage is section 6.

## 6. Named risks

- **Render-target acceptance (primary).** The proven direction is "turnip
  samples our dma-bufs". Making the guest render *into* them is new: the dma-buf
  must have the right (format, modifier, layout) and the driver must accept it
  as a colour attachment. If it does not, the fallback is a GPU blit into the
  window buffer rather than zero copy.
- **Two DRM fds in one process.** Mesa's gbm/DRI path may issue driver-specific
  GEM ioctls (`MSM_GEM_NEW`, `I915_GEM_CREATE`, …) on the fd `gbm_create_device`
  was given. Our fd is the daemon socket, which serves *generic* GEM only. If
  the driver needs its own allocator, that allocation must go to the real GPU
  node while the generic surface stays on the daemon — one fd cannot be both.
- **`/sys` probing beyond the two acquisition sites.** Any other libdrm call
  that describes the fd through sysfs (`drmGetDevices2`, `drmDeviceFromDevId`,
  `drmGetNodeTypeFromFd`) sees a socket and fails; the env-gated hook covers the
  glamor acquisition paths but not every caller.
- **linux-dmabuf feedback main device is NULL.** Skipping the compositor's main
  device leaves `xwl_gbm_get_main_device()` NULL; `xwayland-dmabuf.c` consumers
  must tolerate it (the X11 window path does not go through that feedback).
- **Explicit sync / timeline syncobj.** The daemon implements only
  create-signalled + export. Xwayland's explicit-sync path uses
  `drmSyncobjTimelineWait`/`Transfer`/`Eventfd`, none of which exist yet, so
  implicit sync must remain in force on this device until they are added.
- **Legacy GEM ioctls are not ground-truthed.** `GEM_CREATE`/`GEM_MMAP` are
  absent from the modern header, so their numbers are pinned by a Rust test, not
  the C ABI test. A future kernel-side revival under the same numbers would
  agree; a different encoding would need both sides changed together.
- **Host-only validation.** The device surface and the shim routing are proven
  on the host through a real libdrm client and the real socket. The DRI3 +
  Present route is now host-proven end to end too: `tools/check-x-present.sh`
  imports a real gbm dma-buf with `PixmapFromBuffer`, presents it into an X
  window on a real Xwayland, reads it back with `XGetImage`, and asserts the
  idle fence released the slot. What remains device-only is render-target
  acceptance (the guest rendering into the shim's buffer) and the visible
  co-existence of GDI 2D with the guest's frame in one window.

## 7. Failure model: a render-channel request can never hang the caller

The ioctl path is synchronous: the shim writes one frame on the RPC channel and
blocks in `recvmsg()` until the daemon answers. The kernel fd underneath is a
socket, so there is no kernel-side deadline either. If the daemon does not
answer a request — it cannot serve it, it is not reading, or the render
connection is only half-paired (its event half never arrived, so no serve
thread exists to read the RPC half) — the caller blocks **forever**. Blocked
inside its first glamor ioctl, Xwayland never reaches XWM: the session hangs
with nothing logged, which is exactly the observed failure. The scan-out
allocator channel already bounds its exchange; the RPC channel did not.

The RPC channel is now bounded (`GNDRM_RPC_TIMEOUT_SEC`, generous: a healthy
daemon answers inline in microseconds). On a timeout, or on a dead or malformed
frame, the channel is **retired** rather than reconnected: a reply that arrives
after the caller was failed would otherwise be read as the answer to a later
request. Once retired, every subsequent `rpc()` fails immediately with
`ENOTCONN`. A caller that keeps issuing ioctls therefore fails fast and visibly,
which is the contract Xwayland's glamor init needs: the failed probe disables
glamor, Xwayland continues in software, and XWM still starts — the same shape
the pre-gbm-fix builds had, but now deterministic rather than dependent on how
the failure happened to surface.

## 8. Render-channel diagnostics

Both halves of the render channel are observable with the existing gates; both
default off.

- **shim** (`GNDRM_LOG_SHIM=1`, container env — Xwayland inherits it with the
  `LD_PRELOAD`): names the first occurrence of each request number it forwards
  (`ioctl first 0x…`), the render-node handshake result
  (`render-node handshake (NODE_KIND) accepted|failed`), the connect line
  (`connected to <path>-render …`), and a channel retirement
  (`RPC channel retired (err=…)`). Seeing the requests but no matching daemon
  lines localises a stall to the wire; a retirement line localises it to an
  unanswered request.
- **daemon** (`GNDRM_LOG_DAEMON=1`, set with `Os.setenv` before
  `nativeStart` — the daemon runs in the app process): the render listener logs
  each accepted RPC half awaiting its event partner
  (`render client rpc accepted (pid N), awaiting event channel`) and each
  completed pair (`render client paired (pid N)`), and the per-connection
  Device logs the first occurrence of each request (`ioctl: first <NAME>` /
  `render node: refused <NAME>`). A request named by the shim but absent here
  never reached the daemon; an `awaiting event channel` with no `paired` line
  is the half-paired handshake.

Launcher lines for one diagnostic device run:

```
GNDRM_LOG_SHIM=1      # in the container env (gamescope/Xwayland)
GNDRM_LOG_DAEMON=1    # Os.setenv on the app side before nativeStart
```

## 9. Startup hang containment

The startup window is a rendezvous with no supervisor: Xwayland cannot write its
`-displayfd` until its Wayland handshake completes, and the compositor's whole
`wlserver_init()` ready-wait (`src/wlserver.cpp`) is the only thing servicing the
Wayland loop. Either side stalling pins the session with no output. Three bounds
make a stall impossible to hide:

- **Compositor ready-wait** (`wait_xwayland_server_ready()`, `wlserver.cpp`):
  keeps the blocking dispatch for the healthy path, flushes after every dispatch
  (so replies and events produced while servicing the loop reach the child before
  the loop blocks again), and fails with a named `wlr_log.errorf` after 15 s
  instead of pinning until the caller's own budget expires.
- **Xwayland startup roundtrip** (`xwl_screen_roundtrip()`,
  `hw/xwayland/xwayland-screen.c`): the globals, wl_drm / linux-dmabuf and
  output waits go through it. It carries the same sync-and-wait semantics but
  bounds each `wl_display.sync` and the whole drain loop; a reply that never
  comes is logged (`ErrorF`, with `expecting_event`) and skipped so Xwayland
  still signals ready and the session proceeds. It cannot cover a wait that
  never enters it, though — see the next bullet.
- **Xwayland display socket** (`xwl_bound_display_socket()`,
  `hw/xwayland/xwayland-screen.c`, applied right after `wl_display_connect`):
  a blocking read or write on the Wayland fd is a syscall parked outside the
  poll that carries the roundtrip deadline, so the roundtrip bound cannot
  interrupt it (this is the case the 15 s deadline actually caught). The fd
  carries `SO_RCVTIMEO`/`SO_SNDTIMEO`, the same bound the gndrm channels use,
  so a read or write with no progress returns `EAGAIN` instead of pinning the
  thread. libwayland issues non-blocking syscalls on a healthy connection, so
  the healthy path is untouched; a wedged peer now surfaces as `EAGAIN` and
  Xwayland continues (glamor disabled if the GPU wait failed) so it still
  writes `-displayfd`.

- **Shim event channel** (`gndrm_shim.c`): the fd handed to the app is the
  daemon's event socket. Like the RPC half it now carries
  `SO_RCVTIMEO`/`SO_SNDTIMEO` (`GNDRM_RPC_TIMEOUT_SEC`), so a read with no
  producer returns `EAGAIN` rather than blocking the app's thread forever.

Host coverage for the event-channel bound is `tools/b9-test.sh` phase 5
(`event_fd_read_client.c`); the compositor and Xwayland bounds are only
exercised by a device run, where their log lines name the stall if it recurs.
