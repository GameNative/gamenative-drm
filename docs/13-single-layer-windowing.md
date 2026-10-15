# 13 — The window/layer model on the single-layer path

Status: **design, for review.** No code accompanies this document. It records
what the port's window/layer model actually is, why the content-override
mechanism exists, which patches touch the model, and what an ordinary-client
layer design would require. Intended reader: the user, before any rebuild.

Companion to `05-xwayland-reintroduction.md` (client windowing),
`08-xwayland-render-node.md` (the render-node/DRI3 alternative) and
`03-gamescope-build.md` (the patch train). Where those documents disagree with
the code today, this one says so (§8).

---

## 1. The layer model this port actually has

SteamOS's compositor carries three kinds of layer. This port carries one.

| SteamOS role | This port | Where it lives |
|---|---|---|
| Layer 0 — the Steam interface | **Nothing.** The session launches straight into the game. `--steam` is not passed, so gamescope has no store UI, no overlay, no app-identity source — this is why patch A26 exists at all (it forces one uniform `w->appID`, `src/steamcompmgr.cpp`, because the store-derived id does not exist here). | — |
| Layer 1 — the game | **The whole job.** The guest's frame, carried by a single window: the root child the compositor enumerates. Its content is either the window's own Xwayland surface (the guest-surface route) or the shim's bound content-override surface (the route in force today). | `frameInfo.layers` |
| Layers 2..N — in-game quick-access menu, chat | **Not a compositor layer.** The quick menu is an Android-side overlay drawn above the `SurfaceControl` that gndrm publishes; the input injector holds the `gamescope_control` connection for the controls behind it (`09-runtime-features.md`). gamescope never learns it exists and never composites it. | Android / SurfaceFlinger |

**What this implies, stated plainly: gamescope is being used as a
single-layer scan-out path, not as a multi-layer compositor.** The expectation
is `frameInfo.layers.count() == 1` (plus, occasionally, the cursor plane). Every
gamescope facility that exists to make *several* layers work — the reserved-layer
budget, overlay/underlay/decoration plane reservation, the Steam and external
overlay planes, the colour-management composite path — is therefore not on the
critical path for correctness. It is on the critical path only in the negative
sense: it must not add layers, must not steal the one layer's plane, and must
not force the composite path when a single fullscreen SDR layer could
direct-scanout.

This is a *narrowing*, not a redesign: nothing in the port asks gamescope to
composite more than one thing, and no patch adds a second composited layer.
The risk of the single-layer framing is that it is an assumption, not an
invariant — see §7.

---

## 2. How the guest's frame reaches that one layer (the default route)

Step by step, with the override's exact insertion point.

1. **Guest.** DXVK/vkd3d-proton calls `vkCreateXlibSurfaceKHR` for a Win32
   `HWND`. `VK_KHR_win32_surface` is winevulkan's client-side name and is mapped
   by wine onto its display driver's host extension (`winex11` → xlib), so what
   reaches the ICD is already **an X window id** (`dlls/winex11.drv/vulkan.c` →
   `x11drv_client_surface_create` → `p_vkCreateXlibSurfaceKHR`).
2. **ICD shim (default arm).** `gn_vkCreateXlibSurfaceKHR`
   (`icd_shim/src/proxy.rs`) takes the default branch and calls
   `xlib_override_surface(gi, xid, out)`.
3. **Window resolution.** `xlib_override_surface` first calls
   `crate::x11::resolve_override_target(window)`: it walks the named window up to
   its **direct child of the X root**, exactly as gamescope does, and — only if
   that ancestor is degenerate (both extents ≤ 1, the 1×1 override-redirect wine
   helper Dishonored nests under) — swaps it for the root child gamescope
   actually paints, matched by `_NET_WM_PID`/`WM_CLASS`, falling back to the X
   input focus, then `_NET_ACTIVE_WINDOW`, then the largest candidate.
4. **The override is created.** The shim opens **its own wayland connection** to
   gamescope's wayland socket (`wl_setup`), creates **its own `wl_surface`**
   (`wlproto::compositor_create_surface`), wraps it in a `gamescope_swapchain`
   (`swapchain_factory_create_swapchain`), and binds that surface to the X
   window: `swapchain_override_window_content(scobj, server=0, bound_window)` —
   `gamescope_swapchain.override_window_content`.
5. **gamescope accepts it.** `gamescope_xwayland_server_t::handle_override_window_content`
   (`src/wlserver.cpp:836`) resolves the named XID the same way —
   `x11_window = x11_find_toplevel_for_xid( this->ctx->dpy, x11_window )` —
   stores a `wlserver_content_override` in `content_overrides[x11_window]`, and
   calls `wlserver_x11_surface_info_set_wlr( x11_surface, surface, true )`.
   That sets **`surf->override_surface = the shim's surface`**, and drains any
   `g_PendingCommits` already queued for it.
6. **Which surface is composited.** From then on the window's content is
   `wlserver_x11_surface_info::current_surface()`:

   ```c
   struct wlr_surface *current_surface() const
   {
       if ( override_surface )
           return override_surface;
       return main_surface;
   }                                            // steamcompmgr_shared.hpp:20
   ```

   The override **replaces** the window's Xwayland `wl_surface` as the
   composited content. `w->override_surface()` then drives three behaviours:
   `window_last_done_commit_index()` (A30) tracks only override-surface commits;
   `win_is_useless()` (A29) refuses to drop the window; and the layer is sized
   from the committed buffer, not the window's X geometry (A30).
7. **Frames.** Every present, the guest's swapped dma-buf is committed on that
   override `wl_surface`; gamescope latches it as the window's layer texture and
   scan-outs via gndrm to the Android Surface.

The override **enters at step 4** and takes effect at **step 5**. Everything
before step 4 is ordinary X11 windowing; everything after step 6 is ordinary
single-layer scan-out.

---

## 3. Why the override exists at all (the finding)

The honest answer has four parts, and only the first is structural.

### 3.1 The shim is the only WSI provider (structural)

`icd_shim/src/proxy.rs`:

> This is the only WSI provider in the stack: the Vulkan driver (turnip) exports
> no xlib or wayland WSI entry points at all, so handing an X11 surface to it
> cannot work (its ICD carries no `vkCreateXlibSurfaceKHR`, and surface setup
> then fails `VK_ERROR_INITIALIZATION_FAILED`). **The shim must present every
> window itself, so there is no driver-WSI fallback route.**

Because the driver cannot present, the shim owns the swapchain and the dma-buf
images. That is the real constraint. It does **not** by itself force the
override — it forces the shim to be the presenter, and a presenter needs a
channel to the compositor.

### 3.2 There are two such channels, and the override is a *choice* (not forced)

The second channel is a genuine ordinary-client path, and it is implemented.
`icd_shim/src/proxy.rs`, immediately above `use_guest_surface()`:

> [the shim opens its] own X connection to the Xwayland server (see `crate::x11`)
> and presents each rendered slot straight into the window's X pixmap with DRI3
> + Present: **the compositor's own Xwayland IS the client, so the flip reaches
> the compositor without any `gamescope_swapchain` override.**

`GN_VKSHIM_GUEST_SURFACE=1` selects it (`use_guest_surface()`,
`icd_shim/src/proxy.rs`). On that route the frame becomes the window's own
content through an ordinary X11 present — `xcb_dri3_pixmap_from_buffers` +
`xcb_present_pixmap`, release riding an X sync idle fence — and the window's
content is then its normal Xwayland `wl_surface`, which gamescope composites
through the stock `main_surface` path. **No override, no `gamescope_swapchain`,
no shim-owned `wl_surface`.**

### 3.3 Why the override is nonetheless the default

`use_guest_surface()`'s own doc:

> This shim's DEFAULT (`0` or unset) presents the guest's Vulkan frames to
> gamescope's override/Wayland surface (`xlib_override_surface`), ignoring the
> guest's window content: **Vulkan-only, lowest CPU, no dependence on
> Xwayland's DRI3.** Setting `GN_VKSHIM_GUEST_SURFACE=1` involves the guest's
> surface instead — the DRI3 1.2 + Present route into wine's own X window, which
> additionally carries wine's GDI/2D content drawn outside the guest's Vulkan
> surface.

and, on the override arm itself:

> Presenting therefore shows the guest's frames directly — **wine's own GDI/2D
> window content is NOT composited (the known and accepted "chooses, never
> combines" consequence of this route).** That GDI/2D gap is exactly the cost
> the `=1` DRI3 route buys back.

So the trade is explicit and recorded: the override is a **lower-cost, lower-
capability** route (no Xwayland leg on the frame path, but a window can only
ever show the guest's Vulkan frame). The DRI3 route is a **higher-cost,
strictly-more-capable** route (one window carries the guest's frame *and* wine's
GDI/2D). The motivation for the default is the frame-path cost, not an inability
to do the ordinary thing.

### 3.4 What the override *does* require: the XID and the root-child walk

The override is bound by XID and gamescope then resolves that XID itself
(`src/wlserver.cpp:836`), and the compositor creates a window only for a root
child:

```c
case CreateNotify:
    if (ev.xcreatewindow.parent == ctx->root)   // src/steamcompmgr.cpp:8680
```

Under rootless Xwayland a nested client window gets no `wl_surface` at all, so
the XID the override must name is the **root-child ancestor** of the window wine
named — which is why both `resolve_override_target` (shim) and
`x11_find_toplevel_for_xid` (gamescope) walk upwards, and why A29/A30 exist to
keep and correctly size a window whose X geometry says nothing.

### 3.5 Is it required by the virtual desktop, the multi-window case, or something else?

- **Not by the ICD/WSI design as such** — §3.2 is a working counterexample in the
  same repository. What the ICD design forces is that *the shim* presents.
- **Not by the virtual desktop.** `/desktop=shell` is not a gamescope option; it
  is the **Wine explorer** virtual desktop (`wine explorer /desktop=shell,<WxH> …`,
  `XServerScreen.kt`). It nests the game's windows under one full-size root-child
  desktop window. That *works with the override* (the ancestor is non-degenerate,
  so `resolve_override_target` returns it unchanged and the override lands on the
  desktop window — the compositor-visible layer). It is a constraint on the
  *binding target*, not a reason for the mechanism.
- **Not by the multi-window case.** `content_overrides` is an
  `unordered_map<uint32_t /*xid*/, wlserver_content_override *>`; one override
  per window, several windows = several overrides. Both routes handle it.
- **The concrete reason is §3.3**: it is the default the user chose, for
  frame-path cost, over a route that exists and is host-proven.

### 3.6 Provenance: recorded, not reconstructed

Recorded, in three commit messages and in the code comments they left behind:

| commit | date | what it records |
|---|---|---|
| `0f862ac` | 2026-10-03 | *"Adds a first-class X11 present route to the ICD shim, **replacing the deleted gamescope-side X11 override**"* — the ordinary-client route, added on its own merits.
| `eba9ed9` | 2026-10-04 | *"Presenting now targets the gamescope override/Wayland surface by default, so the guest's frames reach the compositor **without the Xwayland leg**. `GN_VKSHIM_GUEST_SURFACE=1` selects the guest-surface route instead… The switch is deliberate: it deviates from the no-knobs rule by the user's decision, with the override route as the documented default."* — the override becomes the default **and the switch is documented as a decision, not scaffolding**.
| `a9e2d92` | 2026-10-05 | *"gamescope resolves the XID named by `swapchain_override_window_content` to its root-child ancestor and composites root children only, so naming the window winex11 named lands the override on the 1x1 override-redirect helper… leaving the panel black while the guest renders normally."* — the walk/`resolve_override_target` rationale.

`wlproto.rs` additionally records where the block came from: *"Recovered from the
pre-DRI3 route that this shim carried at commit `cb89a9f^` … which deleted this
block."* The rationale is therefore **recorded**, with a caveat: `docs/08`
describes the opposite end state as shipped (§8).

---

## 4. Patch audit — layer/window-model verdicts

Every patch in `android/patches/{gamescope,diagnostics,disabled,wlroots}/`.
"Layer/window verdict" = does it touch window creation/classification, the
override/content-override mechanism, focus selection, which surface is
composited, or the `desktop=` argument's effect on the window tree?

### 4.1 Touches the layer/window model

| patch | what it does | layer/window verdict | change vs stock single-layer | needed under an ordinary-client layer? |
|---|---|---|---|---|
| `0029-A29-override-surface-not-useless` | `win_is_useless()` returns "not useless" for a 1×1 window that holds a content override, so the only compositable content is not discarded (Dishonored). | **Yes — window classification.** Changes which windows are dropped. | Stock drops any 1×1 override-redirect window. A29 carves out override holders. | **No — becomes obsolete.** It exists solely because the override can land on a degenerate ancestor. Without the override there is no override-holding 1×1 window, and stock's rule is again correct. |
| `0030-A30-override-content-is-the-apps-frame` | `window_last_done_commit_index()` tracks only override-surface commits once any has landed (`bHasOverrideContent`), holds the last real frame (`overrideContentCommit`) between them, and sizes/places the layer from the committed buffer instead of the window's X geometry. | **Yes — the override/content-override mechanism, which surface is composited, and layer sizing/placement.** The core of the mechanism. | Stock composites `main_surface`, and sizes a non-focused window from its X geometry. A30 makes one window's *override* surface the sole content and its X geometry meaningless. | **No — becomes obsolete.** On the ordinary path the window's content *is* its Xwayland surface and its X geometry *is* meaningful, so stock's `main_surface` tracking, `w == scaleW` sizing and offset maths are correct again. Would delete the patch and the whole override mechanism with it. |
| `diagnostics/0025-A25-startup-present-diagnostics` | First-N commit tracing: buffer shape, texture-acquire drops, wl commit routing, and **override bind/unbind/link**. | **Yes — diagnostics for the override mechanism** (behaviour-free). | None (logging). | **Obsolete with the override.** The "override bind/unbind/link" lines are the only ones that name it; the rest is generic and could be kept if wanted. |
| `diagnostics/0030-A30-override-content-layer-diagnostics` | Names, per painted layer, the window it came from, its committed buffer, and the resulting size/placement (`cmt=ovr|win|none`, `nOverrideHolds`). | **Yes — diagnostics for which surface is composited** (behaviour-free). | None (logging). | **Obsolete with the override.** Its whole vocabulary (`ovr`, `hold`) is override-specific. |
| `disabled/0031-A31-composite-on-active-look` | ORs `ColorLookInUse()` into `bNeedsFullComposite` so a colour look LUT is applied even when a single fullscreen SDR layer would direct-scanout. | **Marginal — the scan-out-vs-composite decision, not the window model.** It is precisely the kind of patch that only matters because the port expects exactly one scanout-able layer. | Stock lets a single layer direct-scanout and never samples the composite LUT. | **Independent of the layer design** — needed on either route while the backend advertises no colour-mgmt plane properties, because either route ends in one fullscreen layer. |

### 4.2 Prerequisites for the layer model but not part of it

| patch | what it does | layer/window verdict |
|---|---|---|
| `wlroots/0003-W3-linux-dmabuf-no-drm-fd-gate` | Advertises `zwp_linux_dmabuf_v1` with no DRM fd (Turnip has no `VK_EXT_physical_device_drm`), and tolerates `main_device == 0`. | **No — buffer protocol.** But it is the gate that makes *any* dma-buf cross to gamescope on this GPU, so both routes depend on it. Keep. |
| `0021-A20-x11-display-socket-path-override` | `GAMESCOPE_X11_DISPLAY` — the exact fs socket as the display name for `XOpenDisplay` and for children's `DISPLAY`. | **No — connection setup.** Prerequisite for the shim's window walk and for the DRI3 route's X connection. Keep. |
| `0024-A24-bound-xwayland-ready-wait` | Bounds the Xwayland ready-wait (15 s) and captures the wedged child's wchan/fd map on expiry. | **No — Xwayland lifecycle/startup**, though it gates when windows can appear. Keep. |
| `0008-I1-virtual-input-protocols` | Instantiates `zwlr_virtual_pointer_manager_v1` / `zwp_virtual_keyboard_manager_v1` and feeds devices through `events.new_input`. | **No — input/seat**, not the window tree. It is the seat whose pointer focus A31 was chasing. Keep. |

### 4.3 Does not touch the layer/window model

Build/toolchain: `0001-M3` (drop libinput/libudev), `0002-M4` (session-less),
`0003-A4` (atomic shared_ptr), `0004-A5` (`Parse<float>`), `0005-A6` (sol2
noexcept), `0006-A7` (`[[maybe_unused]]`), `0022-A21` (glm submodule include).
Display/device: `0007-D1` (gamenative-drm socket), `0009-A8` (accept any
physical device), `0010-A9` (pnp.ids path). Process/fds: `0011-A10`
(`closeallfds` dirfd), `0019-A18` (unregister wlroots listeners),
`0027-A27` (kill descendants with SIGKILL), `0028-A28` (end session with the
game process). Identity/perf: `0026-A26` (uniform `appID`),
`disabled/0032-A32` (fps-cap lower-bound delay). Diagnostics: `0018-A17`
(liftoff prepare path), `0023-A23` (present path). wlroots: `0001-W1`
(memfd shm), `0002-W2` (`WLR_X11_SOCKET_DIR`).

**Summary: five entries touch the model — A29, A30, and the two override
diagnostics (all four exist *because of* the override and become obsolete with
it), plus the parked A31-composite which is orthogonal. No other patch, in any
directory, changes window creation, classification, focus selection, or which
surface is composited.**

---

## 5. Target architecture

### 5.1 Decision: keep the override as the shipped default; narrow it; do not delete it yet

Rationale:

1. It is the **documented, user-chosen default** (`eba9ed9`), and the frame-path
   cost it buys is the reason.
2. The alternative is **not free**: `docs/08 §6` names "render-target acceptance
   (primary)" as the unproven step of the DRI3 route — the guest rendering *into*
   the shim's buffer must be accepted by the driver as a colour attachment, and
   `docs/08 §6` records that Xwayland built for the render node still needs
   `gn_dmabuf`/render-node work to be device-validated. Switching the default
   would trade a working single-layer path for an unmeasured one.
3. The override is **already correctly narrowed by construction**: A30 makes the
   override surface the window's only content, so the compositor's output is one
   layer. Nothing needs to be removed for the single-layer model to hold.

What *does* change is that the override becomes an explicitly-named,
single-purpose mechanism rather than the ambient way frames arrive: the port
should treat "a window with a content override" as **the layer-1 carrier** and
make that the invariant the code asserts, rather than a happy accident. The two
diagnostics in §4.1 are how that invariant is observed.

### 5.2 The shim's binding contract under `desktop=shell` (and why it is the crux)

With `/desktop=shell,<WxH>`, Wine creates one full-size virtual-desktop window
and the game's windows become its **children**. Consequences that must be
designed for, not discovered:

- The compositor enumerates only root children (`steamcompmgr.cpp:8680`), and
  under rootless Xwayland only a root child gets a `wl_surface`. **The
  compositor-visible layer is the Wine desktop window**, not the game's client
  window. The shim's binding target must therefore be the root-child ancestor —
  which is exactly what `resolve_override_target` → `resolve_target` computes,
  and what gamescope's `x11_find_toplevel_for_xid` independently recomputes from
  the XID it is handed.
- The override path is **already correct under `desktop=shell`**: the desktop
  ancestor is non-degenerate, so `resolve_override_target` returns it unchanged
  and the override lands on the desktop window. No change is required for the
  desktop argument itself.
- The sizing half of A30 is what makes this safe: the layer is sized from the
  committed buffer because "wine can leave the window it is bound to at 1×1 at an
  arbitrary offset, as Dishonored does". Under `desktop=shell` the bound window is
  the *desktop* window and its geometry is at least plausible — but A30 must stay,
  because the shim still resolves to whatever gamescope paints, and A29 must stay
  for the degenerate-ancestor case that motivated both.
- **Multi-window:** one override per root child the shim binds. Under
  `desktop=shell` all of a session's game windows resolve to the *same* desktop
  ancestor, so they collapse onto one override — which is the single-layer
  model working as intended, and is why the port cannot express a genuine second
  game window as a second layer today. Accepted; not a regression.

> **Risk to design for (flagged once, not argued):** with a virtual desktop the
> game's window is a child of the desktop window, so the desktop window is what
> the compositor can see as a layer, and a design that binds frames to the game's
> own window instead would produce a black screen.

The design therefore binds frames to **the root-child ancestor**, never to the
window wine named, on both routes.

### 5.3 Consequence for the daemon

**None material.** gndrm is below the layer model: it receives atomic commits on
the daemon socket and publishes the scan-out `AHardwareBuffer` to an Android
`Surface` (`02-gamenative-drm.md §7`). It does not know whether the single layer
came from an override surface or from Xwayland's own `wl_surface`; both arrive
as the same committed buffer. The only daemon-side item is unrelated: the
render-node work (`08-xwayland-render-node.md`) that the ordinary-client route
would need — and that is needed only if the default is ever switched.

### 5.4 Consequence for the patch train

| patch | disposition |
|---|---|
| A29 (0029), A30 (0030), diagnostics A25 (0025), diagnostics A30 (0030) | **Parked in `disabled/`.** The override mechanism itself is **stock** — `current_surface()` returns `override_surface` when one is set and the commit-accept path filters to it — so these are refinements, not the mechanism: A29 removes a 1×1 degenerate-ancestor discard, A30 selects which queued override commit is kept and sizes the layer from the committed buffer instead of X geometry. **Measured with all four disabled**: Dave, Hades II and Dishonored all render through the still-present override with `GN_VKSHIM_GUEST_SURFACE` unset — so deleting them would not remove the override, and restoring them is about A30's hold, not about rendering at all. A30's hold is untested for the case it exists for (a pause or loading gap where the guest briefly stops presenting). |
| A31 — re-assert seat pointer focus (gamescope + diagnostics) | **Removed** (see §6 — abandoned as the wrong direction). It was chasing a `win_surface()` vs `override_surface()` divergence that A30 created; a single-layer model that names the composited surface explicitly would not have that divergence. |
| A31-composite-on-active-look (`disabled/`) | Parked; orthogonal to the layer model (§4.1). |
| All of §4.2, §4.3 | **Keep unchanged.** |

---

## 6. What stays as-is (the boundary of this change)

- **No gamescope patch is added, removed or reshaped by this document.** The only
  change on the gamescope side is the A31 revert: `0031-A31-reassert-seat-pointer-focus.patch`
  and `diagnostics/0031-A31-mouse-focus-gate-diagnostics.patch` are deleted, and
  the tree is restored to the 20-patch train (`android/build-gamescope.sh --clean --setup-only`,
  discarded diff saved under `~/.cache/gamenative/gamescope-clean-*.patch`).
- **The override mechanism, the shim's binding walk, A29 and A30 stay.**
- **The daemon, the WSI/swapchain protocol, the scan-out pool, the presenter and
  the input injector are untouched.**
- **The one app-side change** is the desktop argument: `XServerScreen.kt` collapses
  `if (isX11Display) "shell" else "root"` to `"shell"` for both arms, because the
  conditional now has one value. Nothing keys on the name (§7, item 1). Every other
  uncommitted hunk in that file is left exactly as found.
- **No build, no payload, no install** accompanies this document.

---

## 7. Open questions — measurement, not reasoning

1. **Nothing keys on the desktop name.** Verified by reading: the only
   `/desktop=` producers are `XServerScreen.kt` and `PreInstallSteps.kt` (already
   `shell`); `PerfProcStatTest` and `PreInstallStepsTest` assert `shell`; the
   shim's `resolve_override_target` matches `WM_CLASS` *relative to the named
   window*, never a fixed name; the shim's only literal `"gamescope"` test is
   `comm.contains("gamescope")` (process identity). **But** whether Wine's
   explorer enables different shell/taskbar behaviour for the magic name `shell`
   versus another name (`programs/explorer/desktop.c`) is a Proton-side fact and
   should be measured, not reasoned about — it is the whole content of the
   "backward compatibility" requirement.
2. **Is `frameInfo.layers.count()` actually 1, every frame, on device?** The
   single-layer claim is an assumption today. A23/A25/A30 diagnostics report the
   layer count; a bounded run with them enabled is the measurement that turns §1
   from an assertion into an invariant.
3. **Render-target acceptance on the DRI3 route** (`docs/08 §6`, "primary"):
   does the adrenotools driver accept the shim's dma-buf as a colour attachment,
   and does GDI/2D visibly co-exist with the guest's frame in one window? Until
   this is measured on device, §5.1's "keep the override" is the only defensible
   default.
4. **Does the guest ever present through `VK_KHR_wayland_surface`** rather than
   xlib on this port? The shim advertises both (`05 §4`); if a title takes the
   wayland arm, the override/window walk above does not describe its path.
5. **The `win_surface()` vs `override_surface()` divergence.** `win_surface()`
   returns `main_surface()`, while the composited surface is `current_surface()`
   (which prefers the override), and `gamescope_xwayland_server_t::set_wl_id()`
   sets `surf->main_surface = nullptr` for the duration of a WLSurfaceID update.
   That divergence is real and is what A31 tried to paper over. Whether it can
   still strand the seat's pointer focus (the behaviour A31 was aimed at) is
   **unmeasured** — the A31 diagnostics were the instrument and are now removed.
   If focus/click loss recurs, this is the first thing to instrument.

---

## 8. Documentation to update (recorded conflicts, stated not argued)

Three existing docs now disagree with the code. Recorded here so the next reader
is not misled; updating them is a separate decision.

| doc | says | code says |
|---|---|---|
| `04-gamenative-app.md §3.5` | "Desktop naming: `/desktop=gamescope`, never `shell`" | `XServerScreen.kt` now uses `shell` for both arms (this change); `PreInstallSteps.kt` already used `shell`; the pre-existing code used `shell` for the X11 arm. The doc's stated reason (explorer's shell/taskbar only for the magic name) is the open question in §7.1. |
| `05-xwayland-reintroduction.md §3–4`, `08-xwayland-render-node.md` | the XID-bound override is the path (§05), and "The gamescope override is gone… the previous override / `gamescope_swapchain` path is deleted" (§08, status "implemented") | `eba9ed9` (2026-10-04) put the **override back as the default** two days before `47b72c9` (2026-10-06) committed the docs. The override is the shipped default; the DRI3 route is the opt-in (`GN_VKSHIM_GUEST_SURFACE=1`). §08 describes the target state, not the current one. |
| `03-gamescope-build.md` | A31 = "composite while a colour look is set" | the colour-look patch is parked in `disabled/`; the A31 that was in the train (seat pointer-focus re-assert) is reverted by this change. |
