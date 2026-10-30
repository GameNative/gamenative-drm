# 03 — gamescope Android build (branch `gamenative` of `/data/workspace/Projects/gamescope`)

Model: proton-wine's patch-train approach — the tree stays upstream-clean; Android
support is a set of patches + build scripts, built with the NDK against a
termux-style prefix, packaged as a GameNative content archive with a
`profile.json` (same shape as WCP packages: `bin/ lib/ share/ profile.json`).

Patch-regeneration pitfalls: `git diff > file` against **HEAD** folds in every
not-yet-committed train patch touching the same file (e.g. a wlroots patch
capturing the previous patch's hunks in the same file) — the train then can't
forward-apply it after the predecessors. Generate with the predecessors applied
and diff against that state (or strip the foreign hunks after), and NEVER
`git checkout --` a single file to pristine when a later train patch needs its
context: the train is strictly sequential, so reset the whole `src/` tree +
delete new-file artifacts (`src/Utils/AtomicSharedPtr.h`) and let it re-apply.

## 1. What we build

Exactly one artifact is needed at runtime:

```
gamescope [gamescope-args] -- <wine>/bin/wine <game>.exe
```

So: the `gamescope` executable + its private shared libs + data (`share/gamescope`).
The WSI layer (`libVkLayer_FROG_gamescope_wsi.so`) is not needed (games render
through wine→XWayland→gamescope's wayland server, not the implicit layer). The
integration is XWayland-based: wine runs `winex11.drv` against XWayland
(`--xwayland-count 1`, the default), and the XWayland binary itself ships as the
separate `gamenative-xwayland` payload (05 §2). OpenVR off, SDL backend off (no
window system to nest in), Headless backend off (DRM backend only), pipewire
off, libei off.

### Meson configuration

```
meson setup build \
  --cross-file android/cross/aarch64-linux-android.txt \
  --prefix /data/local/tmp/gn-gamescope  (packaging-time; runtime prefix from profile) \
  -Denable_gamescope=true \
  -Ddrm_backend=enabled \
  -Dsdl2_backend=disabled \
  -Denable_openvr_support=false \
  -Denable_gamescope_wsi_layer=false \
  -Denable_tests=false \
  -Denable_zenity=false \
  -Dpipewire=disabled \
  -Dinput_emulation=disabled \
  -Davif_screenshots=disabled \
  -Drt_cap=disabled \
  -Dbenchmark=disabled
```

(`-Dforce_fallback_for=libliftoff,vkroots` stays as upstream requires; wlroots +
libdisplay-info also resolve via vendored subprojects — see §3.)

## 2. Dependency strategy (termux prefix)

proton-wine cross-compiles against an **unpacked termux aarch64 rootfs on the
host** (`build-scripts/build-step-arm64ec.sh:7-8,42-75`):

```bash
export deps="$HOME/termuxfs/aarch64/data/data/com.termux/files/usr"
export RUNTIME_PATH="/data/data/com.termux/files/usr"          # on-device rpath
export PKG_CONFIG_LIBDIR=$deps/lib/pkgconfig:$deps/share/pkgconfig
export CPPFLAGS="--sysroot=$TOOLCHAIN/../sysroot -idirafter $deps/include"
export LDFLAGS="-L$deps/lib -Wl,-rpath=$RUNTIME_PATH/lib -Wl,-z,max-page-size=16384"
```

Gamescope reuses this exact pattern. Contents of the local prefix
(`~/termuxfs/aarch64`, 560 pkgconfig files):

**Present** ✔ libdrm (+freedreno), wayland client/server/cursor +
wayland-scanner, xkbcommon(+x11, registry), pixman, SDL2, libdecor-0, vulkan
loader (+lvp/lavapipe), mesa GLES, full X11/xcb stack (x11, xcb, xcomposite,
xcursor, xdamage, xext, xfixes, xi, xmu, xrender, xtst, xxf86vm),
glslang/glslangValidator binaries, libcap-ng.

**Missing** ✘ libevdev, mtdev, libudev, luajit (in termux repo, downloadable),
libxres (termux x11-repo, downloadable), libliftoff + libdisplay-info (vendored
subprojects anyway), libei (disabled). **libinput: not needed — cut by design
(patch M3); all input is app-injected via virtual-input protocols (patch I1).**
libevdev/mtdev stay installed in the prefix (harmless termux debs) but nothing
links them once M3 lands.

Local prefix versions (dpkg status): libdecor 0.2.5, libdrm 2.4.131 (≥2.4.113),
libpixman 0.46.4, libwayland 1.24.0 (≥1.21), libxkbcommon 1.13.1, mesa 25.3.5
(+libgbm, dri/, lvp ICD json), sdl2 2.32.10, vulkan loader + `lvp_icd.aarch64.json`.

termuxfs provenance: produced by a GHA workflow (`termux-on-gha` repo, also local
at `/data/workspace/Projects/termux-on-gha`), downloaded + extracted in
proton-wine's `.github/workflows/build-proton.yml` ("Download and extract
termuxfs" step). Adding gamescope deps = extend that workflow's `pkg install`
list (libxres, luajit, libevdev, mtdev, hwdata) so both host builds and CI stay
in sync.

**Vulkan driver**: GameNative ships adrenotools/Turnip drivers and points guests
at them via a configurable `libvulkanPath` (`GeneralComponents` /
graphicsDriverConfig `adrenotoolsTurnip` flow). Gamescope runs in the same guest
env and inherits the same driver selection (prefix mesa loader + ICD json).
GameScope mode requires a driver with dma-buf external-memory support → Turnip
(the proprietary Adreno driver lacks it in practice). Enforce/warn in the UI
(04 §2) and note it in the setting subtitle.

## 3. wlroots-0.20 subproject on Android

gamescope pins `subprojects/wlroots` with
`default_options: ['default_library=static', 'examples=false', 'xwayland=enabled', 'backends=libinput', 'renderers=[]', 'allocators=[]', 'session=enabled']`.

Configuration (wlroots subproject options; `session`/`backends` are set by
`build-gamescope.sh`'s meson args, the rest are the in-tree defaults):

- `session=disabled` — no seatd/logind/VT on Android. Gamescope's session code
  is behind `IsSessionBased()`; the `GAMENATIVE_DRM_SOCKET` patch makes the DRM
  backend non-session.
- `backends=[]` — drop the libinput backend (no `/dev/input`); virtual-input
  injection (patch I1) replaces it. This also drops wlroots' libudev usage;
  gamescope's own libinput/udev call sites are compile-guarded by patch M3.
- `renderers=[]`, `allocators=[]` — already empty (gamescope renders itself);
  keeps GBM and the GL renderer out of wlroots entirely. No wlroots render
  codepath is compiled in.
- `xwayland=enabled` stays — the live client-windowing path (needs X11 libs +
  xcb from the prefix).
- wlroots' `render/vulkan` and `types/wlr_linux_drm_syncobj_v1` compile; their
  ioctls only run if gamescope calls them.

## 4. Gamescope patch list (branch `gamenative`, one commit per patch)

Patches are numbered files in `android/patches/{gamescope,wlroots}/`, applied in
filename order by `android/build-gamescope.sh`. File numbers run `0001`–`0011`,
then `0018`, `0019`, `0021`–`0032` (no `0012`–`0017`, `0020`); wlroots runs
`0001`–`0003`.

| # | Patch | Files | Size | Purpose |
|---|---|---|---|---|
| M3 | drop libinput/libudev requirement | `src/meson.build`, `src/wlserver.cpp` | S | `dependency('libinput')`→`required:false` + `HAVE_LIBINPUT` guards on the touch→monitor heuristic (`wlserver.cpp:427-448`), the backend-create block (`:2183-2188`), and the `<wlr/backend/libinput.h>` include (`:30`); `LibInputHandler.cpp` compiled only when libinput found (its only user, OpenVRBackend, is already conditional); `dependency('libudev')`→`required:false`. Nothing on Android can deliver evdev input — input is app-injected via I1. |
| M4 | session-less build (HAVE_SESSION=0) | `src/meson.build`, `src/wlserver.{hpp,cpp}` | S | `wlserver.hpp:28` tied `HAVE_SESSION` to `HAVE_DRM`; made meson-overridable + `-DHAVE_SESSION=0` from wlroots' `have_session` var; inert `wlsession_open_kms/close_kms/active` stubs under `#else` (callers in DRMBackend stay intact; D1 reroutes them). |
| W1 | wlroots: memfd shm for bionic | `subprojects/wlroots/util/shm.c` | S | bionic has no `shm_open`/`shm_unlink`; `memfd_create` declared only API 30+, target is 28 → raw `SYS_memfd_create` + `/proc/self/fd` re-open in `allocate_shm_file{,_pair}()`. |
| W2 | wlroots: WLR_X11_SOCKET_DIR for X11 fs paths | `subprojects/wlroots/xwayland/sockets.c` | S | Android has no `/tmp`: `open_display_sockets()` creates `/tmp/.X<n>-lock` + binds the fs socket under `/tmp/.X11-unix` → all displays ENOENT. Env redirects lock files/socket dir/fs socket; the ABSTRACT socket keeps the standard `/tmp/.X11-unix/X%d` name (kernel namespace, what X11 clients dial). The launcher pre-creates `<imagefs>/tmp/.X11-unix` 0700. |
| W3 | wlroots: linux-dmabuf without DRM-fd gate | `subprojects/wlroots/render/wlr_renderer.c` + `types/wlr_linux_dmabuf_v1.c` | XS | `wlr_renderer_init_wl_display` only advertised `zwp_linux_dmabuf_v1` when `renderer_get_drm_fd() >= 0`; gamescope's vulkan renderer can't provide one without `VK_EXT_physical_device_drm` (Turnip) → the global would be missing. Three gates dropped: the outer advertisement gate (the fd only feeds legacy `wl_drm`); `wlr_linux_dmabuf_feedback_v1_init_with_options`'s hard-fail (its `main_device` is just a client allocation hint → 0); and `set_default_feedback`'s `drmGetDeviceFromDevId(main_device=0)` (`main_device_fd` stays -1, which `check_import_dmabuf` already tolerates). Format-set requirement kept (populated from `sampledDRMFormats`). |
| D1 | `GAMENATIVE_DRM_SOCKET` env path | `src/Backends/DRMBackend.cpp` (`init_drm`, `IsSessionBased`) | S | THE display patch: env set → skip `vulkan_primary_dev_id`/`drmGetDeviceFromDevId`/`wlsession_open_kms`; `drm->fd = gamenative_drm_connect()` — weak symbol provided by the LD_PRELOAD'ed shim (unresolved-with-env-set = hard error with clear message); `IsSessionBased()→false` (no VT session). `drmIsKMS` needs no special-casing: the daemon answers it. |
| I1 | virtual-input injection | `src/wlserver.cpp`, `src/wlserver.hpp` | S | creates `zwlr_virtual_pointer_manager_v1` + `zwp_virtual_keyboard_manager_v1` globals (vendored wlroots implements both); `new_virtual_pointer`/`new_virtual_keyboard` → `wl_signal_emit(&multi_backend->events.new_input, dev)` → stock device→seat path (`wlserver_new_input`). Runs on the wayland thread (owns wlserver state). |
| A4 | `std::atomic<std::shared_ptr<T>>` compat | new `src/Utils/AtomicSharedPtr.h` + 5 sites | S | NDK libc++ has no `atomic<shared_ptr>` specialization; C++11 free functions are deprecated (warnings). Mutex-based drop-in `gamescope::atomic_shared_ptr<T>`. |
| A5 | FP `std::from_chars` workaround | `src/Utils/Parsers.h` | XS | NDK libc++ deletes FP `from_chars`; route `Parse<float/double>` through `strtof/strtod` (same semantics). |
| A6 | sol2 noexcept(member-template) workaround | `thirdparty/sol/sol.hpp` (3 sites) | XS | NDK clang+libc++: `noexcept(std::is_nothrow_copy_assignable_v<T>)` on a member function template of a class template fails substitution when taken as `lua_CFunction`; hoisting to a constexpr member works, plain `noexcept` chosen (moot under `-fno-exceptions`). |
| A7 | `[[maybe_unused]]` for pipewire-disabled | `src/steamcompmgr.cpp` | XS | `window_last_done_commit_id` is only called from `paint_pipewire()`. |
| A8 | accept any Vulkan physical device on gndrm | `src/Backends/DRMBackend.cpp` (`ValidPhysicalDevice`) | XS | the DRM backend requires `VK_EXT_physical_device_drm` to node-match the GPU against the DRM device; with the userspace KMS socket there is no node to match, and the adrenotools Wrapper(Turnip) ICD doesn't enumerate the extension → `Failed to create backend`. When `GAMENATIVE_DRM_SOCKET` is set, accept any physical device (the environment already picked the only GPU). |
| A9 | `GAMESCOPE_PNP_IDS_PATH` env override | `src/Backends/DRMBackend.cpp` (`load_pnps`) | XS | compiled `HWDATA_PNP_IDS` is the build-host termux prefix path. Payload ships `share/hwdata/pnp.ids`; launcher exports `GAMESCOPE_PNP_IDS_PATH` (04 §3.2). Falls back to the compiled path when unset. |
| A10 | CloseAllFds: skip the dirfd | `src/Utils/Process.cpp` (`CloseAllFds`) | XS | `opendir("/proc/self/fd")` owns an fd; the close-everything loop closed it too, then `closedir()` closed it a **second** time — glibc tolerates, bionic **fdsan aborts** in `gamescopereaper` pre-exec. Fix skips `dirfd(pProcDir)` in the loop. |
| A17 | present-path -EINVAL diagnostics | `src/Backends/DRMBackend.cpp` (4 hunks) | XS | `drm_prepare_liftoff` has four -EINVAL paths that are silent at the default log level (state-cache HIT at :2692; `pDrmFb == nullptr` at :2705; `liftoff_output_apply` errors swallowed by libliftoff's quiet test commits; `liftoff_output_needs_composition` at :2904 forces -EINVAL after a successful apply). Each now `errorf`s a distinct `gamenative A17 diag:` line. Diagnostics only — no behavior change. |
| A18 | wlserver_finish: unregister virtual-input manager listeners | `src/wlserver.cpp` | XS | `wlserver_finish` unregisters six global-manager listeners but not `new_virtual_pointer`/`new_virtual_keyboard`; wlroots asserts those listener lists empty at `wl_display_destroy`. Remove both links before display teardown. |
| A20 | X11 display socket path override | `src/wlserver.cpp`, `src/main.cpp` | XS | the bundled termux libX11/libxcb resolve `:0` to a compiled-in socket dir that does not exist on device; `GAMESCOPE_X11_DISPLAY` carries the exact filesystem socket wlroots bound (W2) as the display name for `XOpenDisplay` and for the `DISPLAY` handed to children (libxcb accepts a full socket path). |
| A21 | glm include from the submodule | `meson.build` | XS | glm is a tracked git submodule (header-only); depend on its include dir directly instead of resolving the wrap, whose meson overlay is not part of the submodule. |
| A23 | present-path diagnostics | `src/steamcompmgr.cpp`, `src/Backends/DRMBackend.cpp` | XS | rate-limited logs of the paint loop (scene contents) and the DRM flip/present path, so a stalled pipeline still leaves a trail. Diagnostics only — no behavior change. |
| A24 | bound the Xwayland ready-wait | `src/wlserver.cpp`, `src/wlserver.hpp`, `src/main.cpp` | S | overall deadline on the Xwayland ready-wait; on expiry dumps each Xwayland thread's `wchan` and open fds mapped back to `/proc/net/unix` endpoints so a wedged startup handshake is named instead of pinning the process; adds `wlserver_get_pid`, and `main.cpp` `_exit(1)`s on an unrecoverable `wlserver_init` failure rather than unwinding through joinable backend threads. |
| A25 | startup/present diagnostics | `src/wlserver.cpp`, `src/steamcompmgr.cpp` | S | wlserver commit routing, texture-acquire drops, override bind/unbind/link, plus `import_commit` failure detail in steamcompmgr. Diagnostics only — no behavior change. |
| A26 | uniform app id | `src/steamcompmgr.cpp` | XS | Without `--steam`, `w->appID` is the Xwayland window id — a per-launch value the app process cannot know, since `request_app_performance_stats()` is called from the app with no X11 connection. Every window gets one fixed `kGameNativeUniformAppId`, so `wlserver_app_presented()`'s match against `wlserver.app_perf_requests` needs no cross-process channel; the app side uses the same constant (`GameNativeDrm.GAMESCOPE_APP_ID`). |
| A27 | reaper: SIGKILL the descendant tree, don't wait | `src/Apps/gamescopereaper.cpp` | XS | Wine's wineserver (plus explorer/services.exe) outlives the game and ignores SIGTERM. On the primary child's exit, kill the reparented descendant tree with SIGKILL and return without `WaitForAllChildren()` — blocking there kept the reaper, and gamescope, alive past the game. |
| A28 | end the session with the game process | `src/Apps/gamescopereaper.cpp`, `src/Utils/Process.{h,cpp}` | S | The primary child is wine's wrapper chain (explorer → winhandler), which outlives the game, so waiting on it alone keeps the session alive after the game quits. When `GAMENATIVE_GAME_PROCESS` names the game image, end the session once that process is gone — but only after it has been seen once, and only after two consecutive misses; without the name, fall back to the primary child. |
| A29 | keep override-surface windows | `src/steamcompmgr.cpp` | XS | The 1x1-window filter dropped a 1x1 override-redirect wine helper even when it held the app's real frame on a content override (Dishonored). Return "not useless" when `w->override_surface()` is set; a genuine 1x1 helper with no override is still dropped. |
| A30 | the override surface is the app's frame | `src/steamcompmgr.cpp`, `src/steamcompmgr_shared.hpp` | S | A window with a content override draws its real frame on the override surface (the guest's Vulkan swapchain the WSI shim bound to it); wine's own X11 surface commits — frame, decorations, the 1x1 helper — are furniture, and texturing them is a full-frame dropout. Once the override has committed, `window_last_done_commit_index()` tracks only override-surface commits (`bHasOverrideContent`), so the base plane holds the last real frame instead of painting wine's window furniture. |
| A31 | composite while a colour look is set | `src/steamcompmgr.cpp`, `src/Backends/DRMBackend.cpp` | XS | Colour-management look LUTs are baked into the composite LUT that only `vulkan_composite()` samples, so a single fullscreen SDR layer with no scaling filter direct-scanouts and the look never reaches the screen (this backend advertises no colour-mgmt plane properties). New `ColorLookInUse()` is OR'd into `bNeedsFullComposite` in the DRM backend, so the composite runs only while a look is loaded. |
| A32 | fps cap by a lower-bound delay, not a vblank grid | `src/steamcompmgr.cpp` | XS | The fixed-rate path dropped callbacks on a `vblank_idx % (refresh / target)` grid, dragging a title already at its own ceiling below it. Pace by a lower-bound delay on the target frame time (`1/target`), as the VRR branch already does, and use `1/target` as the limiter cycle instead of `AppRefreshCycle * floor(refresh / target)` — which also mis-capped targets that do not divide the refresh (60 on 90Hz → no cap). No-op for a real VRR panel. |

Build output: the patch train lands as `android/patches/{gamescope,wlroots}/*.patch`
(applied by `android/build-gamescope.sh`). `ninja` completes from clean;
`build-android/src/gamescope` = aarch64 API-28 PIE ELF with
`w gamenative_drm_connect` (weak) and the virtual-input globals linked in; only
warnings are two pre-existing upstream clang ones (`reshade_effect_manager.cpp`
unused field, `DRMBackend.cpp` unused `m_pBackend`).

## 5. Cross file (new: `android/cross/aarch64-linux-android.txt`)

Mirror proton-wine's toolchain exactly (`build-step-arm64ec.sh:12-14`):
NDK `~/Android/Sdk/ndk/27.3.13750724`, target triple `aarch64-linux-android28`
(SDK 28 + `-Wl,-z,max-page-size=16384` = 16 KB page support). Host tools:
meson 1.12.0 (wlroots-0.20 needs ≥1.3 ✔), ninja 1.13.2, pkg-config 3.0.7.

```ini
[binaries]
c       = '/home/joshua/Android/Sdk/ndk/27.3.13750724/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android28-clang'
cpp     = '.../aarch64-linux-android28-clang++'
ar      = '.../llvm-ar'
strip  = '.../llvm-strip'
pkgconfig = 'pkg-config'
# host (build-machine) tools — NEVER the aarch64 ones from the prefix
wayland-scanner  = '/usr/bin/wayland-scanner'
glslangValidator = '/usr/bin/glslangValidator'

[host_machine]
system = 'linux'           # keep 'linux' so #ifdef __ANDROID__ comes from the NDK, target_family checked in meson if needed
cpu_family = 'aarch64'
cpu = 'armv8-a'
endian = 'little'

[built-in options]
# -Drindex=strrchr: bionic has the symbol but no header decl at API 28.
# -D_LIBCPP_ENABLE_EXPERIMENTAL (cpp only): NDK clang does not set
#   __has_feature(experimental_library), so libc++ hides std::jthread/stop_token
#   behind _LIBCPP_HAS_NO_EXPERIMENTAL_STOP_TOKEN (rendervulkan.hpp needs them).
c_args   = ['-g0', '-O2', '-DANDROID_SUPPORT_FLEXIBLE_PAGE_SIZES', '-Drindex=strrchr', '-idirafter', '/home/joshua/termuxfs/aarch64/data/data/com.termux/files/usr/include']
cpp_args = ['-g0', '-O2', '-DANDROID_SUPPORT_FLEXIBLE_PAGE_SIZES', '-Drindex=strrchr', '-D_LIBCPP_ENABLE_EXPERIMENTAL', '-idirafter', '...same...']
c_link_args   = ['-L/home/joshua/termuxfs/aarch64/data/data/com.termux/files/usr/lib', '-Wl,-rpath=/data/data/com.termux/files/usr/lib', '-Wl,-z,max-page-size=16384']
cpp_link_args = ['...same...']

[properties]
# NO sys_root: meson prepends it to pkg-config -I paths and NDK clang already has
# a built-in sysroot. pkg_config_libdir covers the cross-machine .pc search.
pkg_config_libdir = ['<prefix>/lib/pkgconfig', '<prefix>/share/pkgconfig']
```

**Environment pitfall**: do NOT export `PKG_CONFIG_LIBDIR` in the shell
running meson. Meson forwards it to *native* pkg-config calls too, so
`wayland-scanner.pc` from the aarch64 prefix wins and codegen tries to execute the
aarch64 binary (`cannot execute binary file`, exit 126). The cross-file property
above already covers cross-machine lookups; `build-gamescope.sh` runs meson under
`env -u PKG_CONFIG_LIBDIR`. Related: `meson setup --reconfigure` does NOT re-read
cross-file `[built-in options]` — wipe the build dir when the cross file changes.

## 6. Build scripts (mirror proton-wine layout)

```
android/
├── patches/
│   ├── gamescope/*.patch              # M3, M4, A4-A10, A17, A18, A20, A21, A23-A32, D1, I1 — applied in filename order
│   └── wlroots/*.patch                # W1-W3 — applied inside the wlroots submodule
├── cross/aarch64-linux-android.txt
├── build-gamescope.sh                 # patch train + meson setup + ninja
└── package-gamescope.sh               # GameNative asset + jniLib bundling (see §6.1)
```

`build-gamescope.sh` is idempotent (`git apply --check` / `-R --check` per patch),
honours `NDK`/`TERMUXFS`/`BUILD_DIR` env overrides, and has `--skip-patches` /
`--setup-only` modes. Prerequisite on a fresh checkout:
`git submodule update --init --recursive` (all 7: subprojects/{wlroots,libliftoff,
vkroots,libdisplay-info,openvr}, `src/reshade`, `thirdparty/SPIRV-Headers` — the
script aborts early with this hint if they're missing).

Wrap-git subprojects (glm, stb, v4l-utils) are plain `meson`-fetched clones, not
git submodules. If a meson regen leaves one empty ("Subproject exists but has no
meson.build file"), restore it by hand from its `subprojects/*.wrap` pin:
`git init` + `fetch --depth 1 origin <revision>` + `checkout -f FETCH_HEAD`,
then copy `subprojects/packagefiles/<name>/.` over it when the wrap names a
`patch_directory`. Never run `git -C subprojects/<name> …` before confirming
that directory has its own `.git` — without one, git walks up and operates on
the parent gamescope repo.

### 6.1 Runtime payload: `package-gamescope.sh`

**Bundled APK asset, not a downloadable content package** (see 04 §4):

```
android/package-gamescope.sh [YYYYMMDD]
  → GameNative/app/src/main/assets/gamescope-gamenative-<date>.tzst   (tar+zstd)
  → GameNative/app/src/main/jniLibs/arm64-v8a/libgamenative_drm.so    (16 KB pages)
```

Archive layout (extracted to `filesDir/gamescope/` by
`XServerScreen.refreshComponentsFiles`; `TarCompressorUtils` chmods everything
0771, so exec bits are covered):

```
bin/gamescope, bin/gamescopereaper
bin/Xwayland, bin/xkbcomp   # absorbed from the gamenative-xwayland build (05 §2)
bin/gn-dmabuf-test   # client-dmabuf smoke test (04 §3.4): native wayland+vulkan
                   # client presenting solid-color dma-buf frames via
                   # zwp_linux_dmabuf_v1; run as the session guest via
                   # GUEST_PROGRAM_LAUNCHER_COMMAND
bin/gn-wsi-test      # WSI-proxy smoke test (04 §3.4): plain swapchain client
                   # (clear-color presents) driven through the shim's proxy
                   # path end to end
lib/*.so           # recursive DT_NEEDED closure from termux prefix + NDK
                   # (libc++_shared) + libgndrm_shim.so + libadrenotools_gamescope.so
                   # (the per-process driver-picking vulkan ICD, 04 §3.4) +
                   # libvulkan_freedreno.so (payload turnip, gamescope's own
                   # driver) + wine-side X11 client libs (libX11/xcb —
                   # winex11.drv's NEEDs, 04 §3.2) + libwayland-client +
                   # libxkbcommon/libxkbregistry (gamescope's wayland server and
                   # the shim's proxy connection); EXCLUDES bionic system libs
                   # and libvulkan (gamescope must share the imagefs loader +
                   # turnip ICD with wine)
share/vulkan/icd.d/{adrenotools_gamescope,freedreno_icd}.aarch64.json
                   # shim (api 1.3.0) + payload-freedreno (api 1.4.354) ICD
                   # manifests; bare-soname library_path
share/gamescope/   # meson-installed data (shaders, base EDID if any)
share/xkeyboard-config-2/   # libxkbcommon keymaps (gamescope seat + winex11)
share/hwdata/pnp.ids        # monitor names (A9)
```

The gamescope payload carries XWayland and xkbcomp too — `package-gamescope.sh`
absorbs the `gamenative-xwayland` build (05 §2) so a single asset ships; the
launcher points `WLR_XWAYLAND` at `bin/Xwayland` inside it. Library resolution:
`LD_LIBRARY_PATH` (launcher prepends `filesDir/gamescope/lib`; beats the binary's
build-time DT_RUNPATH — no patchelf).

Every shipped ELF is 16 KB page aligned: our own objects are linked with
`-Wl,-z,max-page-size=16384` (each cross file, plus the daemon's `RUSTFLAGS`),
and the libraries staged from the termux prefix are current package builds, which
are. Android 15+ devices use 16 KB pages and cannot map a library aligned below
that, so `package-gamescope.sh` refuses to archive a staged tree containing one
and `check-payload-closure.sh` reports the alignment of every shipped ELF. The
same holds for the APK's own `jniLibs/arm64-v8a/` set.

## 7. Runtime env assembled by GameNative (handoff to 04)

```
GAMENATIVE_DRM_SOCKET=gndrm          # abstract socket
LD_PRELOAD=<pkg>/lib/libgndrm_shim.so
XDG_RUNTIME_DIR=<imagefs>/tmp/xdg-run   # wayland socket dir (0700)
TMPDIR=<pkg>/tmp                        # bionic P_tmpdir is not app-writable
XKB_CONFIG_ROOT=<pkg>/share/xkeyboard-config-2
GAMESCOPE_PNP_IDS_PATH=<pkg>/share/hwdata/pnp.ids
WLR_XWAYLAND=<pkg>/bin/Xwayland
# WAYLAND_DISPLAY absent from gamescope's own env (04 §3.2) — children get
# WAYLAND_DISPLAY=gamescope-0 via --expose-wayland; DISPLAY=:0 is set for the
# guest so winex11 dials XWayland.
```

Launch: `gamescope --backend drm --expose-wayland --xwayland-count 1 -W <scanoutW> -H <scanoutH> -w <desktopW> -h <desktopH> -- <wine>/bin/wine explorer /desktop=gamescope,<WxH> <start command>`
(`-W/-H` = the session's scan-out size, i.e. the container's Upscaling Resolution,
which the daemon emulates its connector mode at; `-w/-h` = the container's screen
size, the guest desktop. Game res stays wine's window size.)
`--backend drm` explicit: auto-select would also land on DRM when no parent
wayland/X11 exists, but never rely on detection.
