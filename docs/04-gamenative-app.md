# 04 — GameNative app integration (branch `feat/gamescope-display` of `/data/workspace/Projects/GameNative`)

Scope: user-facing Display Server setting + launching gamescope + hosting the
gamenative-drm daemon (JNI) in the display screen.

## 1. Config model: `displayServer` key

Follows the exact existing pattern used by `displayRenderer`:

| File | Change |
|---|---|
| `app/src/main/java/com/winlator/container/Container.java` | `DEFAULT_DISPLAY_SERVER = "x11"`, `DISPLAY_SERVER_GAMESCOPE = "gamescope"`; field+getter/setter; `data.put("displayServer", …)` in save; `case "displayServer":` in load; default-repair in the migration block |
| `app/src/main/java/com/winlator/container/ContainerData.kt` | `val displayServer` + Saver `mapOf` entry + `restore` entry |
| `app/src/main/java/app/gamenative/utils/ContainerUtils.kt` | **Required or the setting silently won't save**: the dialog round-trips through `ContainerData`, so `toContainerData` needs `displayServer = container.displayServer` and `applyToContainer` the reverse; default-config dialog additionally round-trips via `getDefaultContainerData`/`setDefaultContainerData` |
| `app/src/main/java/app/gamenative/PrefManager.kt` | `var displayServer` string pref (default `Container.DEFAULT_DISPLAY_SERVER`) for the default-config path |

## 2. UI: Display Server dropdown in GeneralTab

`app/src/main/java/app/gamenative/ui/component/dialog/GeneralTab.kt`
(inside the bionic-only section, next to the wine version dropdown — gamescope is
a bionic-only feature):

```kotlin
if (config.containerVariant.equals(Container.BIONIC, ignoreCase = true)) {
    val displayServers = listOf("X11", "GameScope")
    val displayServerIds = listOf(Container.DEFAULT_DISPLAY_SERVER, Container.DISPLAY_SERVER_GAMESCOPE)
    val dsIndex = displayServerIds.indexOf(config.displayServer).coerceAtLeast(0)
    SettingsListDropdown(
        colors = settingsTileColors(),
        title = { Text(text = stringResource(R.string.display_server)) },
        value = dsIndex,
        items = displayServers,
        onItemSelected = { idx ->
            state.config.value = config.copy(displayServer = displayServerIds[idx])
        },
    )
}
```

- The GameScope entry is shown **unconditionally**: no part of the gamescope
  path depends on how proton-wine is compiled (05 §5).
- Strings (`res/values/strings.xml`): `display_server`.
- Only shown for `containerVariant == BIONIC` (gamescope runs in the bionic
  prefix).

## 3. Launch path integration

### 3.1 Effective session mode (single predicate for every layer)

The GameScope selection applies to **game launches only**. One effective mode —
`resolveGamescopeMode(...) && !bootToContainer && preInstallCommands.isEmpty()` —
drives every decision point so the layers can never disagree:

| Layer | Behavior in a GameScope session |
|---|---|
| View factory (XServerScreen composable) | `XServerView.setGamescopeMode(true)` — the gamenative-drm daemon consumes the view's Surface instead of the X11 renderer. Computed in a composable `remember` |
| `setupXEnvironment` | Skips `XServerComponent` (no winlator X server); starts the wayland input injector instead |
| Guest launch command | `explorer /desktop=` name is `gamescope` (see §3.5) |
| `BionicProgramLauncherComponent` | `setGamescopeModeOverride(...)` — session override wins over the container check in `isGamescopeMode()`; otherwise a pre-install guest exe would get the gamescope wrapper with no X server inside |
| Registry steering | `applyGamescopeWineDisplayDriver(gamescopeMode, …)` (see §3.3) |

Open-container (boot-to-desktop) and pre-install sessions always run the winlator
X11 stack: gamescope is a single-app compositor (one focused xdg_toplevel scaled
to the output — it is not a window-stacking desktop), and installers expect a
desktop environment. X11 stays the recommended display server for desktop use.

MainViewModel's boot-splash dismissal: the winlator X11 path dismisses the splash
on the first mapped window, which never happens without an X server.
`startBootGameExitWatch` (polls `/proc` for the game exe, clears the splash 15 s
after first seen or on early exit) is additionally armed in GameScope mode; in
X11 sessions the normal `onWindowMapped` path fires first.

### 3.2 Command + env (gamescope sessions)

Command (`BionicProgramLauncherComponent.getFinalCommand`, guest launch only —
`execShellCommand` for `wineserver -k`/winetricks passes `wrapGamescope=false`
because those run without the gamescope lib env):

```
filesDir/gamescope/bin/gamescope --backend drm --expose-wayland --xwayland-count 1 \
    -W <surfaceW> -H <surfaceH> -- <wine>/bin/wine explorer /desktop=gamescope,<WxH> <start command>
```

- `-W/-H` = **output** res = the Surface/mode size (03 §7); game res stays wine's
  window size.
- `--expose-wayland`: gamescope serves xdg-shell clients and sets
  `WAYLAND_DISPLAY=gamescope-0` + `XDG_SESSION_TYPE=wayland` for its children
  itself.
- `--xwayland-count 1`: one XWayland server (the default) — `winex11.drv` dials
  it via `DISPLAY=:0`.

Env (`BionicProgramLauncherComponent.execGuestProgram`, same `EnvVars`):

| Var | Value |
|---|---|
| `GAMENATIVE_DRM_SOCKET` | `gndrm` (abstract socket; shim + daemon rendezvous) |
| `LD_PRELOAD` | prepends `filesDir/gamescope/lib/libgndrm_shim.so` (before evshim/dns entries) |
| `XDG_RUNTIME_DIR` | `<imagefs>/tmp/xdg-run` (created 0700; gamescope's wayland socket lands here; the injector reads it) |
| `LD_LIBRARY_PATH` | prepends `filesDir/gamescope/lib` (payload libs) |
| `PATH` | prepends `filesDir/gamescope/bin` (gamescopereaper — gamescope wraps the wine command via `execvp`, `Utils/Process.cpp`) |
| `TMPDIR` | `filesDir/gamescope/tmp` (bionic `P_tmpdir` = `/data/local/tmp`, not app-writable) |
| `XKB_CONFIG_ROOT` | `filesDir/gamescope/share/xkeyboard-config-2` (honored by both libxkbcommon and libxkbregistry) |
| `GAMESCOPE_PNP_IDS_PATH` | `filesDir/gamescope/share/hwdata/pnp.ids` (patch A9; monitor names, cosmetic) |
| `WLR_XWAYLAND` | `filesDir/xwayland/bin/Xwayland` (the gamenative-xwayland payload) |
| `DISPLAY` | `:0` — `winex11.drv` dials XWayland through it |
| `WAYLAND_DISPLAY` | **absent from gamescope's own env** — gamescope treats it as the *parent* compositor: `CheckWaylandPresentationTime()` connects to it and `exit(1)`s on failure, and `auto_select_backend()` prefers it. Only children get it, via `--expose-wayland` |

Payload-path wiring note: payload binaries carry compiled-in termux-prefix paths,
but every runtime-relevant one is env-overridden (`GAMESCOPE_PNP_IDS_PATH`,
`XKB_CONFIG_ROOT`); SCRIPT_DIR/lua is unused. Nothing resolves outside
`filesDir/gamescope`.

Wine-side runtime libs: `winex11.drv` NEEDs the X11 client stack (libX11, libxcb,
libXext, …), which the termux prefix already ships; `package-gamescope.sh`
resolves them into the payload `lib/`, which the launcher prepends to
`LD_LIBRARY_PATH`. The shim's own wayland connection needs libwayland-client,
also in the payload.

SELinux/exec: `ProcessHelper.exec` prefixes `/system/bin/linker64` for the
top-level command; gamescope's children are execvp'd by gamescope itself — bionic
ELFs with `PT_INTERP=/system/bin/linker64` exec fine from app-private dirs
(termux model). Fallback if a device denies it: `-- /system/bin/linker64
<winePath>/wine …`.

### 3.3 Wine display-driver steering

wine's driver model:

- **explorer.exe owns the graphics driver**: only it reads
  `HKCU\Software\Wine\Drivers → Graphics` (comma list; compiled default
  `mac,x11,wayland`, no env override), loads `wine<name>.drv`, and publishes the
  volatile `HKLM\System\CurrentControlSet\Control\Video\{guid}\0000 →
  GraphicsDriver` value (`programs/explorer/desktop.c`).
- Every other process loads the driver lazily on its first window via
  `load_desktop_driver` (`dlls/win32u/driver.c`), reading that per-device value
  off the desktop window's `__wine_display_device_guid` prop; if explorer hasn't
  published it yet, win32u auto-starts `explorer.exe /desktop` and waits ≤10 s
  (`dlls/win32u/winstation.c`), then fails with `nodrv_CreateWindow`.
  Consequence: the desktop explorer must be **eager** — launched by the session,
  never lazily by the game.

Steering (`XServerScreen.applyGamescopeWineDisplayDriver`, `WineRegistryEditor`
on `.wine/user.reg`, always **before wineserver starts** — wineserver caches the
registry at boot, editing under a running wineserver is a no-op and gets
overwritten): the value is **always written** — `x11` for gamescope sessions
(`winex11.drv` → XWayland) and `x11` otherwise — so a prefix is reliably
restored to a working driver when any session falls back to the winlator X11
stack.

### 3.4 Vulkan ICD shim + surface interception

One vulkan ICD is visible per process: `VK_ICD_FILENAMES` (assembled by the
launcher, §3.2) points at the payload's `share/vulkan/icd.d/adrenotools_gamescope.aarch64.json`
— the shim (gamenative-drm `icd_shim/`, NEEDEDs = libc/libdl/libwayland-client
only, no libvulkan: every driver call goes through function pointers). The shim
picks the real driver per process:

- **gamescope's own process** → the payload's bundled termux turnip
  (`lib/libvulkan_freedreno.so` + `freedreno_icd.aarch64.json`) — the only
  turnip build with wayland WSI; the compositor's renderer runs on it.
- **guest processes** (wine/DXVK, the smoke-test clients) → the user-selected
  adrenotools driver, opened through libadrenotools
  (`adrenotools_open_libvulkan(dlopen_flags, flags, tmp_dir, hooks_dir,
  driver_path, driver_name, redirect_dir, NULL)` — dlopen-compatible handle;
  wrapper fallback `/system/lib64/libvulkan.so`).

Surface interception: the shim advertises `VK_KHR_xlib_surface` and
`VK_KHR_wayland_surface`, strips both from the real instance create, and routes
guest presentation through its own wayland connection: `vkCreateXlibSurfaceKHR`
opens a `wl_display_connect(NULL)`, creates a `gamescope_swapchain` proxy
surface, and binds it to the X window ID via `override_window_content` (05 §4).
Guest frames present as dmabuf buffers on the proxy surface, off the X11 path.

The guest-visible halves are validated standalone by two payload binaries
runnable as the session guest via `GUEST_PROGRAM_LAUNCHER_COMMAND`:
`bin/gn-dmabuf-test` (the output half: client dma-bufs presented via
`zwp_linux_dmabuf_v1` attach/commit — no vulkan WSI at all) and `bin/gn-wsi-test`
(the whole shim surface-interception path). Both cycle red/green/blue/white and
log each presented frame to logcat (`System.out`). Colors on screen = the
corresponding path works end to end.

### 3.5 Desktop naming: `/desktop=gamescope`, never `shell`

GameScope game launches run `wine explorer /desktop=gamescope,<WxH> …`:

- explorer only enables the shell/taskbar for the magic desktop name `"shell"`
  (`programs/explorer/desktop.c:930`). With any other name there are no shell
  toplevels.
- Without a shell, nothing displayable exists until the game's real window maps:
  boot/IME/message windows are positioned but never shown (no `SWP_SHOWWINDOW`,
  no xdg_toplevel role). Black during boot is expected KMS-correct behavior
  (gamescope commits nothing); transient dialogs appear letterboxed when shown.
- The game's toplevels map after the desktop and take gamescope's focus (one
  focused xdg_toplevel scaled to the output). The `wl_output` keeps the wine screen
  at the output size.
- Boot-to-container keeps `"shell"` — a desktop is its content — and runs the
  X11 stack anyway (§3.1).

### 3.6 Operational rules

- **Wrap the guest launch only** in gamescope; shell commands (`wineserver -k`,
  winetricks) run with a different env and a wrapped gamescope binary dies at
  the dynamic linker there.
- **Never set `WAYLAND_DISPLAY` in gamescope's own env** (§3.2).
- **Logcat truncates single lines at ~4 KB** — the full `EnvVars` dump and the
  `ProcessHelper "Executing: …"` line are longer; log key vars on dedicated
  short lines instead.
- **Child stdout/stderr is only drained when a `ProcessHelper` debug callback is
  registered** (`createDebugThread` is conditional). In gamescope mode
  `execGuestProgram` registers a temporary callback piping every child line to
  logcat tag `GAMESCOPE`. For a native backtrace also capture
  `adb logcat -b crash` (tag `DEBUG`) — the main buffer shows only cmdline+pid.
- Shim RPC tracing on device: `GNDRM_SHIM_DEBUG=1` in the container's Env Vars.
- **bionic `ioctl(int, int, …)`: zero-extend the request, never sign-extend** —
  shim widens through `(unsigned int)` first (02).
- **A userspace DRM fd must survive `dup()`** — libliftoff dups the drm fd and
  issues all ioctls on the copy; the shim tracks dup/dup2/dup3/fcntl(F_DUPFD*)
  in a refcounted fd map (02).
- **`IN_FENCE_FD` is SIGNED_RANGE [-1, INT_MAX]** (kernel
  `drm_mode_config.c:488`), and its no-fence sentinel is `(u64)-1` — libliftoff
  range-checks client-side before any test commit (02).
- **GEM_CLOSE on a handle a live FB references must succeed** — gamescope closes
  handles right after ADDFB2; kernel semantics: the FB holds its own reference
  (02).
- **LD_PRELOAD shims must be early-init-safe**: interposition also captures
  libc's own internal calls (e.g. open/close inside `__system_properties_init`,
  before the shim's `pthread_once` resolves `dlsym(RTLD_NEXT)` pass-throughs).
  Pass non-matching calls through with raw syscalls (or lazily resolved pointers
  with a syscall fallback).
- **xkb is needed at three independent levels**: gamescope's wlserver hands
  keymaps to wayland clients; winex11 needs libxkbcommon/libxkbregistry for
  keyboard handling; XWayland compiles the compositor keymap via forking
  `xkbcomp`.
- Wayland debug lever: `WINEDEBUG=+x11drv,err,loaddll,fixme,warn` via container
  Env Vars.

### 3.7 Input

All Android input funnels through five methods on `com.winlator.xserver.XServer`
(`injectPointerMove`, `injectPointerMoveDelta`, `injectPointerButtonPress/Release`,
`injectKeyPress/Release`) — TouchpadView, InputControlsView, external mice,
gamepad-to-key mappings all land there. XServer has an optional `InputEventSink`
delegate; when set (gamescope mode only) the five methods forward to the sink and
skip the X11 path. The sink (`app.gamenative.display.GamescopeInputSink`)
translates to `GameNativeDrm` injector JNI calls: absolute/delta pointer →
`nativePointerAbsolute/Motion`; buttons → `nativePointerButton(BTN_*)`; scroll →
`nativePointerAxis(v/h)` on press; `injectKeyPress/Release(XKeycode)` →
`nativeKey(xKeycode.id - 8, pressed)` (X11 keycode = evdev + 8).

The injector lives inside `libgamenative_drm.so` (single JNI surface): a wayland
client thread connecting to `$XDG_RUNTIME_DIR/gamescope-0` with
retry-until-socket-appears, creating one `zwlr_virtual_pointer_v1` + one
`zwp_virtual_keyboard_v1` (gamescope I1 globals). A minimal canned xkb keymap
blob is uploaded once (gamescope forces its seat keymap onto every device
anyway).

## 4. Gamescope payload packaging (bundled asset, not a content package)

- **Archive**: `app/src/main/assets/gamescope-gamenative-<YYYYMMDD>.tzst`
  (tar+zstd, date-suffixed — the pulseaudio precedent).
- **Contents** (from `android/package-gamescope.sh`): `bin/gamescope`,
  `bin/gamescopereaper`, `lib/*.so` (recursive DT_NEEDED closure from the termux
  prefix + NDK `libc++_shared` + `libgndrm_shim.so` + the wine-side X11 client
  libs of §3.2; excludes bionic system libs and libvulkan — gamescope shares the
  imagefs loader + Turnip ICD with wine), `share/gamescope/`,
  `share/xkeyboard-config-2/` (libxkbcommon keymaps for the gamescope seat +
  winex11), `share/hwdata/pnp.ids`, `share/vulkan/icd.d` + `implicit_layer.d`
  (if present). XWayland and xkbcomp ship in the separate
  `gamenative-xwayland` payload (05 §2).
- **Extraction**: `XServerScreen.refreshComponentsFiles` pair
  `"gamescope-gamenative-<date>.tzst" to File(context.filesDir, "gamescope")`
  via `AssetUtils.extractComponentsWithVersionCheck` (ZSTD).
- **`libgamenative_drm.so` ships in `app/src/main/jniLibs/arm64-v8a/`**, loaded
  with `System.loadLibrary("gamenative_drm")`; built by gamenative-drm's
  `tools/build-android.sh` with `-Wl,-z,max-page-size=16384`. The copy step
  lives in `android/package-gamescope.sh` so one command refreshes asset +
  jniLib.

## 5. Display screen hosting (gamenative-drm lifecycle)

- JNI binding: **`app.gamenative.display.GameNativeDrm`** (Kotlin `object`) —
  `nativeStart(socket, surface, w, h, refreshHz): Int`, `nativeStop()`,
  `nativeSurfaceChanged(surface, w, h, hz)` + the injector calls (§3.7).
- The game display screen is `XServerScreen.kt`; the SurfaceView is
  `com.winlator.widget.XServerView`. **The view instance is kept in gamescope
  mode** (60+ downstream references to `xServerView!!.getxServer()` stay valid)
  with `setGamescopeMode(true)` set right after construction, before `addView`:
  1. `surfaceCreated/Changed/Destroyed` route to `GameNativeDrm.nativeStart /
     nativeSurfaceChanged / nativeStop` **instead of** the X11 renderer (two
     producers on one Surface would be invalid). Same Surface, different
     consumer.
  2. Renderer-selection logic (`displayRenderer`, Vulkan scanout) is skipped; the
     X11-mode path is byte-identical.
  3. `surfaceChanged` fires redundantly on Android (always once after
     `surfaceCreated`, again on relayout/inset passes with identical geometry),
     and a daemon restart permanently severs gamescope's shim socket (no
     reconnect logic), so the view tracks the last-started geometry and no-ops
     unchanged callbacks (±0.5 Hz). Mid-run geometry changes trigger a hard
     restart.
  4. `surfaceDestroyed` / screen exit → `nativeStop()` before the guest is
     killed.
- The daemon presents per KMS semantics: black outside the plane's CRTC rect, the
  FB's SRC rect (16.16 fixed point) scaled into it (02 §7).
- Logging: `GN_DRM` logtag (native side; Kotlin uses Timber), plus a build-stamp
  line at daemon start — the only stale-.so detector on device (no root/run-as).

## 6. Scope

The gamescope path is gated to bionic game launches and runs alongside the
winlator X11 stack (which remains the default): `displayServer` config +
GeneralTab dropdown; payload asset + `refreshComponentsFiles`; launch wrap +
env; registry steering; effective session mode (§3.1); XServerView gamescope
flag + geometry no-op guard; XServer input sink; wayland virtual-input injector;
boot-splash watch for GameScope mode.

## Appendix A — launch pipeline reference

- Launch command construction: `XServerScreen.kt getWineStartCommand(...)`
- Env assembly: `com.winlator.core.envvars.EnvVars`
- Container state for UI: `ContainerConfigDialog.kt` (`ContainerConfigState`)
- Container persistence: `Container.java` save / load / migration
- profile.json parse: `ContentsManager.java:259-288` (`JSONObject`, unknown keys
  ignored)
