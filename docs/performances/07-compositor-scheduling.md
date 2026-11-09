# 07 — Compositor scheduling: `rt_cap` cannot work on Android; `nice -20` can

**Verdict first.** Item 6 as written cannot work on a non-rooted Android device, and both halves of it
are dead: `-Drt_cap=enabled` does not build against the current cross prefix and, even if it did, does
nothing at runtime (`HasCapSysNice()` is false, so `SetNice`/`SetRealtime` return before issuing a
syscall); `SCHED_FIFO` "from the app" is unobtainable without `CAP_SYS_NICE`, which an app uid never
holds. What *is* available is `nice -20` by self-renice, because Android's `init.rc` sets
`RLIMIT_NICE` to 40 for every init-spawned process, and the kernel lets `setpriority()` go below 0
without `CAP_SYS_NICE` up to that limit. The workable form of item 6 is therefore **an unprivileged
self-`nice(-20)` inside gamescope at startup, no libcap, no `-Drt_cap`**, optionally prefigured by a
10-line app-side renice that needs no payload bump.

---

## 1. What `rt_cap` actually is

`rt_cap` is a meson feature that only buys `libcap`, and the only thing libcap is used for is a
capability query:

- `meson_options.txt:2` — `option('rt_cap', type: 'feature', description: 'Support for creating real-time threads + compute queues')`
- `src/meson.build:19` — `cap_dep = dependency('libcap', required: get_option('rt_cap'))`
- `src/meson.build:170` — `gamescope_cpp_args += '-DHAVE_LIBCAP=@0@'.format(cap_dep.found().to_int())`

`HAVE_LIBCAP` gates exactly one function, `Process::HasCapSysNice()` (`src/Utils/Process.cpp:880-901`):

```cpp
#if defined(__linux__) && HAVE_LIBCAP
    cap_t pCaps = cap_get_proc();                       // Process.cpp:885
    cap_get_flag( pCaps, CAP_SYS_NICE, CAP_EFFECTIVE, &eNiceCapValue )  // :891
    return eNiceCapValue == CAP_SET;
#else
    return false;                                       // :899
#endif
```

Everything else is gated on that query, so with `HAVE_LIBCAP=0` the whole feature is a compile-time
no-op:

- `Process.cpp:905-925` `SetNice( int nNice )` — `if ( !HasCapSysNice() ) return;` at :908.
  (Latent bug worth naming: the parameter is ignored, `nice( -20 )` is hardcoded at :919. Restore uses
  `nice( *g_oOldNice - *g_oNewNice )` at :945.)
- `Process.cpp:927-955` `RestoreNice()` — same early return at :929; called for every child before
  `exec` from `ProcessPreSpawn()` (`Process.cpp:624-641`, call at :632), which is what keeps the guest
  and XWayland at nice 0.
- `Process.cpp:976-997` `SetRealtime()` — `if ( !HasCapSysNice() ) return false;` at :979, then
  `pthread_setschedparam( pthread_self(), SCHED_RR, &newSched )` at :989 with
  `newSched.sched_priority = sched_get_priority_min( SCHED_RR )` i.e. priority 1 (:988).

**Which threads:** only `pthread_self()` — the call site is `main.cpp:886`, on the initial thread, once.
Nothing else in the tree calls `SetRealtime()` (grep: only `Process.h:82-84`, `main.cpp:886`,
`Process.cpp:976`). There is no compute-queue or per-backend RT wiring in this tree despite the option
description.

**On failure:** neither function fails the launch and neither even prints. `SetNice`/`SetRealtime`
return silently (`:908`, `:979`); the only message is the `else` in `main.cpp:888-891`:
`"No CAP_SYS_NICE, falling back to regular-priority compute and threads.\nPerformance will be
affected.\n"`. So today, on Android, gamescope prints nothing at all — `HAVE_LIBCAP=0` means the
`if` at `main.cpp:881` is false and the warn path *is* taken (stderr, once). Confirmed in the build:
`build-android/build.ninja:2573` carries `-DHAVE_LIBCAP=0`, from `android/build-gamescope.sh:203`
(`-Drt_cap=disabled`).

## 2. What an app uid can actually get

Three independent blockers for `-Drt_cap=enabled`:

1. **It does not configure.** libcap is not in the cross prefix. The prefix has `libcap-ng.so` (a
   different API, no `sys/capability.h`) and no `libcap.pc`; `meson-log.txt:270` records
   "Dependency libcap for host machine skipped: feature rt_cap disabled". Enabling the feature turns
   that skip into a hard `dependency('libcap')` failure at `meson setup`. You would have to build
   libcap for aarch64-linux-android first — for a feature that then does nothing (blocker 2).
2. **It would do nothing at runtime.** `HasCapSysNice()` is `false` for a uid-10xxx process: app
   processes hold no `CAP_SYS_NICE` and there is no way to hand one over (you cannot `setcap` a file
   in app-private storage without `CAP_SETFCAP`, and the app domain is not granted the capability).
   `SetNice`/`SetRealtime` would return at :908/:979 and `main.cpp:890` would print the same warning.
3. **`SCHED_FIFO`/`SCHED_RR` is unconditionally out.** `__sched_setscheduler` requires
   `capable(CAP_SYS_NICE)` for any RT policy, and Android additionally enables RT group scheduling
   and `sched_rt_runtime_us` throttling. No capability, no RT thread — inside gamescope or from the
   app.

**What *is* obtainable: negative nice.** Two facts, both verifiable on the device:

- `system/core/rootdir/init.rc`, `on early-init`: `# set RLIMIT_NICE to allow priorities from 19 to -20`
  / `setrlimit nice 40 40`. Every init-spawned process — so zygote, so every app process, so anything
  the app `fork/exec`s — inherits `RLIMIT_NICE` 40, whose floor is `20 - 40 = -20`.
- `kernel/sys.c` `set_one_prio()`: `if (niceval < task_nice(p) && !can_nice(p, niceval)) return -EACCES;`
  and `set_one_prio_perm()`: permitted when the caller's euid equals the target's uid/euid (or
  `CAP_SYS_NICE`). `can_nice()` is satisfied by the rlimit, not by a capability. There is no SELinux
  gate: `security_task_setnice()` is a `call_int_hook(task_setnice, ...)` in `security/security.c`
  and no upstream LSM used by Android implements `task_setnice` (it is absent from
  `security/selinux/hooks.c`).

So: an app may lower nice to **exactly -20, no further**, on its own threads *and* on a same-uid
sibling's threads. This is already the codebase's operating assumption: `MicrophoneComponent.java:132`
calls `Process.setThreadPriority(THREAD_PRIORITY_AUDIO)` (nice -16) on an app thread, and
`app/src/main/cpp/gnaffinity/jni/gnaffinity.c:6-10` documents the sibling case in situ —
"sched_setaffinity on one task … Linux allows it on any task of the caller's uid without CAP_SYS_NICE:
the app's own threads and the processes it spawned".

## 3. Which thread is the late one

The present path is **one thread**, which changes both the diagnosis and the fix:

- `main.cpp` → `steamcompmgr_main` (`steamcompmgr.cpp:10019`) is the compositor loop; it registers the
  vblank timerfd as a waitable (`steamcompmgr.cpp:10162`, `g_SteamCompMgrWaiter.AddWaitable( &
  GetVBlankTimer() )`) and consumes it at `:10236` (`GetVBlankTimer().ProcessVBlank()`).
- On the DRM backend the **timerfd path is the one in use**: `CVBlankTimer` picks the separate thread
  only when `GetBackend()->NeedsFrameSync()` (`vblankmanager.cpp:33-52`), and
  `CBaseBackend::NeedsFrameSync()` returns `env_to_bool(getenv("GAMESCOPE_DISABLE_TIMERFD"))`
  (`backend.cpp:167-171`), i.e. false. There is therefore **no `gamescope-vblk` thread** in this
  configuration — `vblankmanager.cpp:388` only names a thread that does not exist here. Correcting the
  plan's "commit/vblank thread" framing: the vblank wake, the paint, the composite, the commit and the
  flip wait all happen on **one** thread.
- That thread: `steamcompmgr_main` → `paint_all` (`steamcompmgr.cpp:3259`, called at `:10901`) →
  `pConnector->Present(...)` (`:3697`) → `CDRMBackend::Present` → `HackyDRMPresent`
  (`DRMBackend.cpp:2589`, `:4738`) → `Commit` (`DRMBackend.cpp:4609`) → the flip wait
  `drm->uPendingFlipCount.wait( uNewPendingFlipCount )` (`DRMBackend.cpp:4718`). It has no
  `pthread_setname_np` of its own, so its `comm` is `gamescope` (the only names in `main.cpp` are
  `gamescope-xwm` at :1137 etc.).
- Second thread that matters: **`gamescope-kms`** (`DRMBackend.cpp:865-867`), the page-flip handler
  created at `:1783`, epolling the DRM fd; it calls `GetVBlankTimer().MarkVBlank(...)`
  (`DRMBackend.cpp:840`) which fixes the *next* schedule. Late here ⇒ late vblank ⇒ late next frame.
- Not on the critical path: `gamescope-wl` (`wlserver.cpp:2629`, wayland dispatch) and
  `gamescope-shdr` (`rendervulkan.cpp:1672`, shader compile). Leave them alone.

`SetRealtime()`'s target — `pthread_self()` at the top of `main()` — was in fact the right thread; the
mechanism was wrong.

## 4. The change

### 4a. gamescope source (the permanent form, no libcap, no meson change)

`src/Utils/Process.cpp` — make `SetNice` usable without `CAP_SYS_NICE`, honour its argument, and keep
the child-restore working (this last part is load-bearing: without it the guest inherits -20 and the
whole point is lost):

```cpp
    bool SetNice( int nNice )
    {
#if defined(__linux__)
        // Not CAP_SYS_NICE: RLIMIT_NICE is the other route down. Android's init.rc sets it to 40
        // ("priorities from 19 to -20") for every init-spawned process, so an app-uid gamescope can
        // reach exactly -20. Try it and say what happened instead of assuming a capability.
        errno = 0;
        int nOldNice = nice( 0 );
        if ( nOldNice == -1 && errno != 0 )
            return false;

        errno = 0;
        int nResult = nice( nNice );                    // returns the new nice value
        if ( nResult == -1 && errno != 0 )
        {
            s_ProcessLog.errorf_errno( "SetNice( %d ) refused; RLIMIT_NICE floor applies", nNice );
            return false;
        }

        g_oOldNice = nOldNice;
        g_oNewNice = nResult;
        return true;
#endif
    }
```

and drop the `if ( !HasCapSysNice() ) return;` guard at `Process.cpp:929` in `RestoreNice()` so
`ProcessPreSpawn` (`:624-641`, call at `:632`) still resets the guest and XWayland to nice 0.
`SetRealtime()` stays as it is — it is unreachable on Android by construction.

`src/main.cpp:881-891` — replace the capability gate with an attempt plus an honest log:

```cpp
    // Elevate the compositor. RLIMIT_NICE (40 on Android) reaches -20 without CAP_SYS_NICE, and every
    // gamescope thread created afterwards inherits it; spawned children get it back in ProcessPreSpawn.
    const bool bNiceness = gamescope::Process::SetNice( -20 );
    if ( gamescope::Process::HasCapSysNice() && g_bRt )
        gamescope::Process::SetRealtime();
    if ( !bNiceness )
        fprintf( stderr, "No scheduling headroom (no CAP_SYS_NICE, no RLIMIT_NICE); regular-priority compute and threads.\n"
                         "Performance will be affected.\n" );
```

`android/build-gamescope.sh:203` — **leave `-Drt_cap=disabled`**. That is the whole point: the change
must not depend on libcap. Do not add a meson option; `meson_options.txt:2` and `src/meson.build:19`
stay untouched. If the change is ever wanted from-the-outside too, the fallback is not `setcap` but
4b, below.

Note on ordering: `main.cpp:881` is after the `RestartWithoutSteamOverlay` re-exec (`:876`) and before
any backend/wlserver thread exists, so a single `nice()` there covers `gamescope-kms` (`:1783`), the wl
thread and everything else by inheritance (threads inherit `task_nice` from their creator). If -20 on
the shader-compile and stats threads proves harmful, the surgical variant is the same one line at the
top of `flip_handler_thread_run` (`DRMBackend.cpp:865`) and at the head of `steamcompmgr_main`
(`steamcompmgr.cpp:10019`), instead of in `main()`.

### 4b. app-side (the free experiment; no payload bump)

`PowerManager.findChainThreads()` already enumerates gamescope's TIDs (`PowerManager.kt:1580-1593`,
`File("/proc/$pid/task")`) and `placeCompositorChain` already applies a per-TID syscall with errno
logging (`:1609-1625`), via `TaskAffinity.nativeSetTaskAffinity` → `sched_setaffinity`
(`gnaffinity.c:20`). Add the sibling call to the same JNI file:

```c
/* setpriority on a same-uid task: allowed by set_one_prio_perm(), down to the target's RLIMIT_NICE
 * floor (40 on Android = nice -20). No CAP_SYS_NICE. */
JNIEXPORT jint JNICALL
Java_app_gamenative_powercontrol_TaskAffinity_nativeSetTaskNice(JNIEnv *env, jclass clazz, jint tid, jint nice) {
    (void) env; (void) clazz;
    return setpriority(PRIO_PROCESS, tid, nice) == 0 ? 0 : errno;
}
```

and call it for the gamescope TIDs from the existing session-start hook
(`GamescopeServerComponent.java:54-56`, right where `applyCompositorAffinity` is called today). Re-apply
on the same triggers affinity uses (`PowerManager.kt:221`, `:363`, `:1399`) because gamescope creates
threads after start. This is the cheapest way to *test* the hypothesis before touching the payload; it
is not the permanent form, because it races thread creation and does nothing for a gamescope started
without the app's help.

### 4c. Build and packaging consequence

The change ships in the gamescope payload:

- Rebuild: `android/build-gamescope.sh` (it reuses `build-android` when `build.ninja` exists;
  `GAMESCOPE_BIN=$GS_DIR/build-android/src/gamescope`, `package-gamescope.sh:70`).
- Package: `android/package-gamescope.sh 20261110.2` → `ASSET_NAME="gamescope-gamenative-$DATE.tzst"`
  (`package-gamescope.sh:55`), staged from `$GAMESCOPE_BIN` at `:129`, stale assets removed at `:450`.
- Bump `GamescopePayload.VERSION` (`GameNative/.../display/GamescopePayload.kt:14`, currently
  `"20261110.1"`) to **exactly** the string passed as the date argument — `ARCHIVE` is built from it
  (`:15`) and `isReady()` compares the on-disk `.payload.version` stamp to it (`:18-22`), so a
  mismatch re-extracts forever.
- Gate: `android/check-payload-closure.sh` (defaults to the newest `gamescope-gamenative-*.tzst`).

### 4d. Verification

Measure **frame-time p99 and the count of idle frames**, not CPU. This change is not a CPU reduction —
expect gamescope's CPU share to go *up* slightly, and reading `cpu%` will read as a regression.
Concretely: same scene, same thermal state, 60 s per arm, from the app's frame-time ring
(`GameNativeDrm.onFramePublished`, per `gamenative-drm/docs/19-fps-limiter-acquire-rework.md:44`):
p99 and p99.9 frame interval, plus the count of intervals > 1.5 × the period (2.94 ms at 340 fps, so
the "idle frame" bucket is > 4.4 ms; the recorded 18–21 ms events are 6–7 missed periods). Success is a
falling idle-frame count at equal fps. First confirm the lever landed: `/proc/<gamescope-pid>/stat`
field 19 == -20 for the main thread and for `gamescope-kms`, and `/proc/<pid>/limits` "Max nice
priority" == 40.

If p99 does not move, the attribution in the plan is wrong and the 18–21 ms frames are not scheduling —
they are a blocked presenter or a GPU fence, and the next step is docs/12 and docs/06, not this.

## 5. Risk

- **Weight, not politeness.** `nice 0` is 1024 in `sched_prio_to_weight[]`; `nice -20` is 88761, ~87×.
  On an 8-core already running the guest's box64 threads at nice 0, -20 makes the compositor win every
  contention: good for latch timing, and it takes that CPU from the guest. If the guest is CPU-bound in
  the scene being measured, fps can go *down*. Revert criterion: fps down and p99 not improved.
- **Thermal.** At 102 °C there is no headroom to spend. The extra share is a reordering, not new work,
  but if the compositor's own loop spins or wakes at 340 Hz with any busy path, -20 converts its wakeups
  into preemption of the guest on the same core and real watts at the worst moment — the SoC then
  throttles all eight cores, which costs more than the 18–21 ms tail being chased.
- **Audio and the daemon.** audioserver's RT threads sit on the low-capacity cluster; the existing
  affinity mask already keeps the gamescope chain off it (`PowerManager.kt:1542-1562`,
  `compositorCoreSet`), so -20 must not be paired with widening the mask. `SCHED_FIFO` would have been
  the real hazard here (RT starving audio under Android's 95% `sched_rt_runtime_us`); -20 in CFS cannot
  starve anything, which is the main reason not to mourn the missing capability.
- **No global reach.** No capability means the change cannot affect anything outside the gamescope
  process tree, and `ProcessPreSpawn`'s restore (`Process.cpp:632`) keeps the guest and XWayland at 0.
  That containment is what makes the experiment safe to run and cheap to revert (one line, one payload).
