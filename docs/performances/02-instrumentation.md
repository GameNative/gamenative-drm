# 02 — Instrumenting the gamescope present path (the zero-rebuild run)

Target of `docs/01-gamescope-vs-x11-cpu.md` §5 step 1 (`:215`–`:220`). One session,
one container edit (two env tokens), no rebuild, no code change. It must answer:

* **B** — does the guest use `VK_KHR_present_id` / `VK_KHR_present_wait` **per frame**,
  and if so, is the wait the per-frame stall? (`docs/performances/01:156`–`163`)
* **C** — is the guest starving on swapchain acquire, i.e. spending the frame
  waiting for a slot the compositor has not released? (`docs/performances/01:165`–`169`)

Two corrections to the plan text, both load-bearing, are in §1.2 and §2.3.

Everything below is grounded in the tree at
`gamenative-drm @ performance-report`, `icd_shim/src/*` as of `20261110.1`.

---

## 1. The instrument set, and what each gate really turns on

### 1.1 Gates

| gate | read at | semantics | reachable from the container? |
|---|---|---|---|
| `GNDRM_LOG_VKSHIM` | `icd_shim/src/lib.rs:104`–`105` | on iff set, non-empty, ≠ `"0"`; cached in an `AtomicU8` (`lib.rs:97`–`110`) | **yes** — guest env |
| `GNDRM_LOG_SHIM` | `modules/gndrm-shim/gndrm_shim.c:226` | on iff **non-NULL** — `"0"` still turns it on | **yes** — guest env |
| `GNDRM_LOG_DAEMON` | `modules/gndrm-daemon/src/lib.rs:262`–`273` | on iff non-empty ≠ `"0"` | **no** — app process |
| `GNDRM_LOG_ASC` | `modules/gndrm-daemon/src/log_policy.rs` (`Gate::Asc`) | `asc_log!` sink | **no** |
| `GNDRM_LOG_INPUT` | `log_policy.rs:56` | injector | **no** |

Container `envVars` are handed to the launched guest process tree
(`BionicProgramLauncherComponent.java:533`, `ProcessHelper.java:315`). The daemon
lives in `app.gamenative` itself, so its gates need an app-side env, i.e. an APK
rebuild (`docs/performances/01:131`–`133`).

### 1.2 Correction 1 — `gndrm-daemon: commit #N` is *not* available in this run

`docs/performances/01:123` lists `gndrm-daemon: commit #N` as usable. It is not, in a
no-rebuild run: the line is emitted through the *ungated sink*
(`commit.rs:205` → `Device::always_log`, `lib.rs:825`) but it sits **inside**
`if want_commit_log && (…)` (`commit.rs:179`, guard at `:184`), and
`want_commit_log = crate::verbose_logging()` (`commit.rs:179` → `lib.rs:262`),
which reads `GNDRM_LOG_DAEMON`. So the "ungated" refers to the logcat sink, not
to the condition. Same for `gndrm-daemon: asc hdr decision n=` (`asc.rs:1538`,
`asc_log!`) — that needs `GNDRM_LOG_DAEMON`/`_ASC`, hence a rebuild.

Consequence: **the publish leg cannot be counted from the daemon in step 1.** It
is replaced by the two shim-side counters that observe the same events from the
other end (§5.2): `releases=` (gamescope hands the buffer back,
`proxy.rs:8077`) and `presented=` (gamescope's `wp_presentation` feedback fired
at flip, `present.rs:117`).

### 1.3 Correction 2 — the C shim gate *does* reach gamescope

`gndrm_shim.c:230`–`232` claims "the launcher does not pass a container's
environment to gamescope, so a gated line can never be turned on there". That is
stale: gamescope is spawned by wrapping the guest command line
(`BionicProgramLauncherComponent.java:650`–`661`, `wrapWithGamescope`), and
`LD_PRELOAD=…/libgndrm_shim.so` is injected into the *same* `envVars`
(`:551`–`553`). If the env did not reach gamescope, the interposer would not load
and the KMS path would not exist at all. So `GNDRM_LOG_SHIM=1` in the container
does arm `ATOMIC heartbeat #N`.

### 1.4 Marker inventory (all literals verified by `grep -F` in the working tree)

Guest, re-emitted into logcat by the app (see §3.1):

| # | literal | source | cadence | monotonic field |
|---|---|---|---|---|
| G0 | `gn-vkshim: ICD loaded [shim …] pid= comm= first entry` | `lib.rs:154` | once per process, **ungated** | — (gives pid, build stamp) |
| G1 | `gn-vkshim: proxy hb: fmt= fmt2= caps= caps2= pmodes= create= destroy= images= acq= present=` | `proxy.rs:307` | 1 Hz (`RATE_HB_MS`, `:297`) | `acq=`, `present=` |
| G2 | `gn-vkshim: proxy commit: sc= idx= WxH flush_rc= dpy_err= fence=…ms commit=…ms #N …` | `proxy.rs:2175` | first 8, then **every 120th** commit, or `fence≥100 ms` (`:2173`) | trailing `#N` = `N_COMMIT_OK+1` |
| G3 | `gn-vkshim: present mode: declared= requested= source= route= cap= gate= lsfg=` | `proxy.rs:10528` | once, then on change, **ungated** (`logi!`) | — |
| G4 | `gn-vkshim: present mode: … acquires= commits= declared_fifo= declared_other= releases= feedback= presented= discarded= fallback= at_queue= gate_holds= gate_late= skipped= slot_waits= commit_lat_us= spacing=[…]` | `proxy.rs:10547` | every 10 s, gated (`:10539`) | `acquires=`, `commits=`, `releases=` … ; `spacing=` is **per-window** (`:10545`) |
| G5 | `gn-vkshim: proxy present: first pNext chain sTypes=[…] present_id=` | `proxy.rs:3174` (called `:3307`, once per process, `:3153`) | once | `present_id=` |
| G6 | `gn-vkshim: present wait #N: id=… done\|unarmed\|latched\|timeout after … us` | `present.rs:301` (`note_outcome`, `:296`–`308`) | first 8, then every 600th wait | `#N` (one per `vkWaitForPresentKHR`) |
| G7 | `gn-vkshim stall: acquire found no free slot (timeout=…)` | `proxy.rs:2826` | ≤1/s (`RATE_STALL`, `:2824`), only when the acquire *returns* with no image | `starved=` in payload |
| G8 | `gn-vkshim release: slot=… acq= pres= rel= commit= starved= in_flight= slots=[…] acqus=[…] presiv=[…] cap=[…]` | `proxy.rs:8085` | ≤1/s (`RATE_REL`) | all counters in payload |
| G9 | `gn-vkshim idle: present worker has no work …` | `proxy.rs:1952` | 1 per 2 s while idle | same payload |
| G10 | `proxy acquire: sc=… slot=… wait=…ms` | `proxy.rs:2898` | first 8 / every 120th / `≥100 ms` | — |
| G11 | `proxy wait: n=… want=[…] last_signalled=… timeout=…` | `proxy.rs:3597` | ≤1/s | — (`vkWaitSemaphores`, DXVK latency-sleep path) |
| G12 | `x11 stall ring (acquire blocked >=1s for a swapchain slot): …` | `proxy.rs:104` (`:2893`) | once per process | — |
| G13 | `x11 release: slot=…` (X11 route only — see note) | `proxy.rs:708` | ≤1/s | — |

Gamescope process (via the interposer, `stderr`):

| # | literal | source | cadence | monotonic field |
|---|---|---|---|---|
| S1 | `[gndrm-shim] ATOMIC heartbeat #N ok= fail= addfb2= fail= objs= total=` | `gndrm_shim.c:2274`, counter `g_n_atomic` `:2272` | ≤1 per 2 s (`rate_ok(2)`) | `#N` |
| S2 | `[gndrm-shim] ATOMIC FAIL ret=…` | `gndrm_shim.c:2270` | on failure | — |

Note on G13: the string in the source is `"x11 release: slot={slot:?} {}"`
(`proxy.rs:708`), emitted by the X11 route only; the override/Wayland route
emits G8 (`proxy.rs:8085`). Both carry `slot_flags`. Expect **G8, not G13**, on
this container (`displayServer=gamescope`).

`slot_flags` (`proxy.rs:360`–`408`) is the census payload: `acq= pres= rel=
commit= starved= in_flight= slots=[i:AF …] acqus=[n= sum= max= nz= ge8= ge16=]
presiv=[n= sum= min= max= ge16667= ge33333=] cap=[fps= notes= gate_holds= gate_us=
gate_avg_us= gate_max_us= acq_fps=]`. `slots=[0:A- 1:-F 2:--]` means slot 0
acquired by the guest, slot 1 committed and awaiting release, slot 2 free; the
list length **is** the swapchain depth (`guest_slots`).

Units: `acqus`/`gate_us`/`presiv` are **microseconds** (`proxy.rs:151`–`159`);
`commit=`, `pres=`, `rel=` are counts.

---

## 2. The container edit

### 2.1 Field and value

One field, `envVars`, two tokens appended. Nothing else changes.

```jsonc
// before
"envVars": "WRAPPER_MAX_IMAGE_COUNT=0 … VKD3D_SHADER_MODEL=6_0 PULSE_LATENCY_MSEC=26"
// after
"envVars": "WRAPPER_MAX_IMAGE_COUNT=0 … VKD3D_SHADER_MODEL=6_0 PULSE_LATENCY_MSEC=26 GNDRM_LOG_VKSHIM=1 GNDRM_LOG_SHIM=1"
```

Measured on the real file (`container.gamescope.json`, the Hades container):
`4715 → 4751` bytes, **+36**, 76 other keys byte-identical, restore is
byte-identical (`cmp` clean). Do **not** touch `dxwrapperConfig` (that is step
2's `dxvk.latencySleep=False` arm, `docs/performances/01:162`), `rendererPresentMode`,
`sessionMetadata`, or `displayServer`.

### 2.2 Where the file is

`ContainerManager` is built from `ImageFs.find(context).getRootDir()` +
`/home` (`ContainerManager.java:38`–`40`), enumerates directories named
`xuser-*` (`:55`), and each container's config is `<dir>/.container`
(`Container.java:703`–`705`). `ImageFs` roots at `context.getFilesDir()/imagefs`
(`ImageFs.java:51`), `USER = "xuser"` (`:16`). So:

```
/data/data/app.gamenative/files/imagefs/home/xuser-STEAM_1145350/.container
```

`STEAM_1145350` is the JSON's own `id`, which is `"${gameSource}_${gameId}"`
(`IntentLaunchManager.kt:103`).

### 2.3 Trap: do not use the launch-intent `container_config` extra

`am start … -a app.gamenative.LAUNCH_GAME --es container_config '<json>'` looks
like a free lunch — it is applied in memory only
(`IntentLaunchManager.kt:109`–`140`, `saveToDisk = false`) — but
`mergeConfigurations` (`:297`–`:372`) is **not schema-complete**: it reconstructs
a `ContainerData` and never carries `displayServer`, `rendererPresentMode`,
`sfCompatMode`, `extraData`, `sessionMetadata`. `ContainerData.displayServer`
defaults to `DefaultVersion.DISPLAY_SERVER` (`ContainerData.kt:22`), which is
`"x11"` (`DefaultVersion.java:29`). Passing that extra would silently switch the
arm to the X11 backend — the exact confound this experiment cannot afford.

### 2.4 Apply, byte-safely

Prerequisites, in order:

1. `adb shell run-as app.gamenative true` → debug build, no root needed.
2. else `adb shell su -c true` → rooted.
3. else the file is not writable from the host. Use the in-app path —
   *Settings → container → Environment Variables* → add `GNDRM_LOG_VKSHIM=1`
   and `GNDRM_LOG_SHIM=1` (`EnvironmentTab.kt:219`–`221` builds exactly this
   `envVars` string) — then run with `NOFLIP=1`, which writes nothing.

The edit itself never re-serialises JSON (§8.1 `container-env-flip.py`): it replaces only
the characters between the quotes of the `"envVars"` value. Procedure:

```bash
adb shell am force-stop app.gamenative        # drop the in-memory container copy
adb exec-out run-as app.gamenative cat files/imagefs/home/xuser-STEAM_1145350/.container > orig.container
python3 container-env-flip.py edit orig.container gated.container GNDRM_LOG_VKSHIM=1 GNDRM_LOG_SHIM=1
python3 container-env-flip.py check orig.container gated.container       # proves only envVars moved
adb push gated.container /data/local/tmp/plan1.new && adb shell chmod 644 /data/local/tmp/plan1.new
adb shell "run-as app.gamenative sh -c \
  'cat /data/local/tmp/plan1.new > files/imagefs/home/xuser-STEAM_1145350/.container.tmp && \
   mv -f files/imagefs/home/xuser-STEAM_1145350/.container.tmp files/imagefs/home/xuser-STEAM_1145350/.container'"
adb exec-out run-as app.gamenative cat files/imagefs/home/xuser-STEAM_1145350/.container | cmp - gated.container
```

Why this shape:

* **force-stop first.** `ContainerManager` loads every `.container` in its
  constructor (`ContainerManager.java:41`, `:48`–`80`) and `XServerScreen` grabs
  the container via `ContainerUtils.getContainer` (`XServerScreen.kt:565`–`566`).
  A live process holds the old string and will write it back on the next
  `saveData()` — including `BionicProgramLauncherComponent.java:855`.
* **same-directory temp + `mv -f`.** `rename(2)` inside one directory is atomic;
  a half-written `.container` is not merely ignored, `loadContainers` logs
  "Could not load container" and the container disappears from the app
  (`ContainerManager.java:71`–`75`).
* **`chown/chmod --reference` + `restorecon`** for the `su` path, or the app
  cannot read its own config and SELinux may deny it.
* **never `adb uninstall`, never `pm clear`.** Both destroy the container, the
  Steam login and the save this experiment depends on.
* **verify by re-reading bytes**, not by trusting the push.

Restore is the **inverse byte edit**, not a wholesale overwrite: the app writes
`sessionMetadata` (avg_fps, total_frames) into `.container` at session end, and
that is the cheapest independent fps record for the arm
(`container.gamescope.json` carries `avg_fps 345.17`, `total_frames 187857`). So
`unset` the two keys from the *current* file and keep everything else; the
resulting `envVars` equals the original string, and `container-env-flip.py check` proves
nothing but `envVars` (and the app's own telemetry) moved.

---

## 3. Capture

### 3.1 Which buffer, and why the filter must be content-based

The guest's lines arrive twice, under two different tags:

* `System.out` — `createDebugThread` prints every child line with
  `System.out.println` whenever `PRINT_DEBUG` is set, and it is `true`
  (`ProcessHelper.java:30`, `:424`). This is the "stderr surfaces as
  `System.out`" of `lib.rs:34`.
* a Timber tag — `XServerScreen.kt:1893` registers a `ProcessHelper` callback
  unconditionally (`ProcessHelper.java:322`–`324`, `:419`–`428`) that forwards
  every child line to `Timber.i`; the tag depends on the build
  (`Timber.DebugTree` vs `ReleaseTree`, `PluviaApp.kt:81`–`83`,
  `ReleaseTree.kt:12`–`27`).

So **do not filter by tag**, and expect every line twice. Counters are immune
(they are first-vs-last deltas); line *counts* are not — `gamescope-cpu-parse.py` drops a
repeat of the same marker body within 50 ms (verified: a capture with every line
duplicated under two tags yields the same rates and half the "stall lines").

### 3.2 Command

```bash
adb logcat -c; adb logcat -G 16M
adb logcat -v threadtime -b main -b system -b crash > cap.txt &
sleep 2
adb shell am start -n app.gamenative/.MainActivity \
  -a app.gamenative.LAUNCH_GAME --ei app_id 1145350 --es game_source STEAM
```

`--ei app_id 1145350 --es game_source STEAM` → `appId = "STEAM_1145350"`
(`IntentLaunchManager.kt:26`–`28`, `:67`, `:103`). The action is exported
(`AndroidManifest.xml:104`). Do not use the `container_config` extra (§2.3).

`-G 16M` because the app's own `Timber` traffic plus gamescope's stderr is
continuous; the default 256 KB main buffer will roll over inside a 90 s window.

### 3.3 Duration

`SETTLE=30 s` (shader/pipeline warm-up, title screen → first safe area) then
`WINDOW=90 s` steady state. Justification: G4 is a **10 s** census
(`MODE_CENSUS_EVERY`, `proxy.rs:263`), so 90 s yields 9 windows — enough for a
median and for spotting drift; G1/G8 give 90 samples each at 1 Hz. Discard
everything before `SETTLE` in the parser (`--settle 30`).

Two host-side marks confirm the run is valid before any number is read:

* `env vars are …` in the app log must contain `GNDRM_LOG_VKSHIM=1`
  (`BionicProgramLauncherComponent.java:510`, unredacted for these keys —
  `EnvVarRedaction.java:13`–`21`).
* `gn-vkshim: ICD loaded … comm=Hades2.exe` must appear (G0), proving the ICD
  shim is in the **guest** address space, i.e. the whole reason `docs/performances/01:75`–`82`
  attributes >half the delta to the guest.

Optional but free: run the existing 1 Hz `/proc` sampler
(`hades-cmp/sample.sh`) in parallel so this arm also yields the CPU/power numbers
for the same session; the app's `sessionMetadata.avg_fps` is the fps cross-check.

---

## 4. Grep strings (exact, from source)

```bash
C=cap.txt
grep -cF 'gn-vkshim: ICD loaded'                       $C   # G0  must be 1
grep -cF 'proxy hb: '                                  $C   # G1  ~90 in window
grep -cF 'proxy commit: sc='                           $C   # G2  #N ladder
grep -cF 'present mode: declared='                     $C   # G3+G4
grep  -F 'present mode: declared='                     $C | grep -F 'acquires='   # G4 only
grep -F 'proxy present: first pNext chain'             $C   # G5  must be 1
grep -F 'present wait #'                               $C   # G6  counter ladder
grep -F 'gn-vkshim stall: acquire found no free slot'  $C   # G7
grep -F 'gn-vkshim release: slot='                     $C   # G8
grep -F 'gn-vkshim idle: present worker has no work'   $C   # G9
grep -F 'proxy acquire: sc='                           $C   # G10
grep -F 'proxy wait: n='                               $C   # G11
grep -F 'x11 stall ring ('                             $C   # G12
grep -F 'ATOMIC heartbeat #'                           $C   # S1
grep -F 'ATOMIC FAIL'                                  $C   # S2
```

Field extraction, by line:

| line | take | how |
|---|---|---|
| G1 | `acq`, `present` | `segrep '([a-z0-9]+)=([0-9]+)'` after `proxy hb: ` |
| G2 | commit index `#N`, `fence=Nms` | `rsplit(' #',1)` |
| G4 | every `k=v`, plus `spacing=[…]` | `k=v` scalar scan; `spacing` is per-window |
| G5 | `sTypes=[…]`, `present_id=` | decisive for B |
| G6 | `#N`, `id=`, outcome word, `… us` | counter + duration histogram |
| G8 | `slots=[…]`, `acqus=[…]`, `presiv=[…]`, `cap=[…]` | bracket scan |
| S1 | `#N` | `ATOMIC heartbeat #(\d+)` |

Presence of G1 at all is itself proof the gate reached the guest: the 1 Hz
heartbeat is `logf!`-gated (`proxy.rs:306`) and its *counters* are only
incremented under the same gate (`proxy.rs:3243`–`3251`).

---

## 5. Parser design

`§8.2 gamescope-cpu-parse.py` implements this; it was run against three synthetic
captures built from the exact format strings above (steady state with present
ids; starvation with FIFO; a capture with no census line) and reproduces the
planted values exactly.

### 5.1 Window and delta rule

For every cumulative counter, the value is the **first-vs-last delta inside the
window**, never a sum of lines:

```
rate(key) = (v_last − v_first) / (t_last − t_first)          # t from logcat stamps
```

`t_first`/`t_last` are taken **per series**, not per capture: the 1 Hz series and
the 10 s series do not start or end on the same line, and reusing one
denominator across both biases every rate by up to 3 % (verified: a 120/s
heartbeat reads 116.7/s if it borrows the census span). For the 10 s census the
"first" sample is the first line at `t ≥ SETTLE`; its counters have already been
running since process start, which is exactly why the delta — not the absolute —
is used.

Per-window-only fields (`spacing=[<15:… >20:…]`) are swapped to zero on print
(`proxy.rs:10545`), so they are a count **within that 10 s window**, not a
cumulative: `commits/s ≈ sum(spacing)/10`. Assert it against
`Δcommits/Δt`; disagreement means commits were rate-limited or the census line
was dropped.

### 5.2 Per-layer frame rate

| layer | counter | series | what it counts |
|---|---|---|---|
| L0 guest present | `present=` | G1 | `vkQueuePresentKHR` entries (`proxy.rs:3246`) |
| L1 guest acquire | `acq=` | G1 | `vkAcquireNextImageKHR` entries (`proxy.rs:2705`) |
| L2 guest→gamescope commit | `commits=` | G4 | `N_COMMIT_OK`, one wayland `commit` (`proxy.rs:2172`) |
| L2' | `#N` | G2 | same counter, 1-in-120 sampling — consistency check |
| L3 gamescope release | `releases=` | G4 | `wl_buffer.release` (`REL_ALL_N`, `proxy.rs:8077`) |
| L4 displayed | `presented=` | G4 | `wp_presentation.presented` (`present.rs:117`) |
| L4' replaced | `discarded=` | G4 | mailbox replace (`present.rs:124`) |
| L3.5 daemon atomic | `#N` | S1 | every `DRM_IOCTL_MODE_ATOMIC` gamescope issues (`gndrm_shim.c:2272`) — **upper bound** on daemon commits (`objs=`/`total=` distinguish the 40–60-property frame commits of `docs/performances/01:108` from cursor/modeset atomics) |

`presented=` is only nonzero for frames that carried a present id
(`expect_feedback` is reached only when `wait_id != 0`, behind the
`swapchain_obj`/`PRESENTATION` guards at `proxy.rs:2046`–`2077`),
so **L4 doubles as a B detector**; if B is refuted, L4 is structurally 0 and
`releases=` (L3) is the publish proxy — gamescope returns the buffer once the
daemon's atomic commit has been applied, so L3 is publish-rate-minus-one-slot.

Derived:

```
guest frames per displayed frame = ΔL2 / ΔL4      (should be ~2.9 at 345 fps vs 120 Hz)
guest work not shown             = ΔL2 − ΔL4      ≈ Δdiscarded  (mailbox)
in-flight at end                 = in_flight= from the last G8
```

### 5.3 Cause C — acquire starvation

| number | formula | source |
|---|---|---|
| `slot_wait_frac` | `Δslot_waits / Δacquires` | G4 `slot_waits=` = `SLOT_WAIT_N`, incremented when an acquire, *past the cap gate*, found no free slot (`proxy.rs:2761`–`2766`) |
| `starve_n` | `Δstarved` | G8 payload = `N_STARVE`, incremented only when the acquire **returns** without an image (`proxy.rs:2819`–`2829`) |
| `dwell_ms` | `Δ(acqus.sum)/Δ(acqus.n)/1000` | `proxy.rs:2875`–`2879` |
| `p_ge8` / `p_ge16` | `Δge8/Δn`, `Δge16/Δn` | `proxy.rs:2883`–`2888` |
| `p_blocked` | `Δnz/Δn` | `proxy.rs:2880`–`2882` |
| `gate_ms`, `gate_holds` | `Δ(gate_us)/Δ(gate_holds)`, `Δgate_holds` | `proxy.rs:2812`–`2817` |
| `depth`, `mean_free`, `mean_inflight` | from `slots=[…]` lists | G8/G9, `proxy.rs:360`–`375` |

Read it like this:

* **`starve_n` is not the C signal.** With `timeout = UINT64_MAX` the acquire
  loops forever releasing the lock and reaping in 4 ms slices
  (`REAP_SLICE`, `proxy.rs:2657`, loop `:2779`–`:2804`), so `N_STARVE` only
  increments on a *finite*-timeout call. `docs/performances/01:169` names `starved=` — that
  will read 0 even under total slot starvation. **`slot_waits` and `dwell` are
  the C numbers.**
* `slot_wait_frac ≈ 1` with `dwell ≈ 1/fps` and `mean_free ≈ 0` is starvation:
  every acquire waits for `wl_buffer.release`.
* `slot_wait_frac ≈ 1` with `dwell ≪ 1/fps` and `mean_free > 0` is *not*
  starvation — the count is per-acquire, the dwell is what costs CPU.
* `gate_holds > 0` means the frame cap was parking acquires; subtract
  `gate_ms × gate_holds` from the dwell before attributing anything to slots.
  In this container the cap is 0 (`dxwrapperConfig … framerate=0`) and G4 must
  report `gate=off`, so `gate_holds` should be 0 — a clean C measurement. If
  `gate≠off`, the run is contaminated and the numbers must be split.
* `p_ge8` is the one that ties C to the observed −1.6 %: an 8.33 ms acquire is a
  missed 120 Hz interval, exactly the 18–21 ms idle frames of `docs/performances/01:241`.

### 5.4 Cause B — present id / present wait

| number | how |
|---|---|
| `present_id_used` | `1000294000 ∈ sTypes` **or** `present_id ≠ 0` on G5. `VK_STRUCTURE_TYPE_PRESENT_ID_KHR = 1000294000`; the walk is `present.rs:451`–`479`, printed at `proxy.rs:3173` |
| `wait_rate` | `Δ#N / Δt` from G6 — one count per `vkWaitForPresentKHR` call (`proxy.rs:3544`, `present.rs:359`) |
| outcome mix | `done` = latched by the daemon's mark (`present.rs:381`), `latched` = **grace fallback** (`:405`–`:420`), `unarmed` (`:389`), `timeout` (`:427`) |
| `at_queue` | `Δat_queue` — waits answered when the present was queued (`present.rs:208`), i.e. the `CapRateVsync` path (`proxy.rs:3103`) |
| `fallback` | `Δfallback` — waits the daemon's latch did **not** answer: the buffer release (`present.rs:316`–`:324`) **or** the 2-refresh grace (`:405`–`:420`). Both increment the same counter; only the per-wait µs in G6 separates them (grace ≈ `2/refresh` = 16.7 ms at 120 Hz, release ≈ one frame + a vblank) |
| `semwait_rate` | G11 count / Δt — `vkWaitSemaphores`, the DXVK latency-sleep shape |

The verdict must be **mode-aware**, because the container sets
`MESA_VK_WSI_PRESENT_MODE=mailbox`:

* `present_wait_answer` returns `AtQueue` only when the route is gamescope **and**
  `why == CapRateVsync`; `AtCommit` when either the guest's requested mode or the
  live mode is mailbox/immediate; `Later` (release-or-grace) otherwise
  (`proxy.rs:3095`–`3110`, `answers_present_wait_at_commit` `:10280`–`10282`).
* `Later` splits into "answered on `wl_buffer.release`" (`complete_on_release`,
  `present.rs:316`–`:324`, called from `proxy.rs:8079`) and "answered by grace at
  `2/refresh_hz`" — at 120 Hz, **16.7 ms** (`present.rs:161`–`164`, `:405`–`:420`).

So `docs/performances/01:156`–`160`'s mechanism ("the shim only produces the answer when
*some other thread* runs the reap loop") presupposes `Later`. If G4 says
`declared=mailbox source=env`, waits are answered at commit and that mechanism
cannot fire; the residual B cost is the call plus a futex handoff, not a
per-frame stall. That is a materially different verdict, and the table in §7
separates it.

The grace path also gives a second, independent check: `docs/performances/01:158` claims the
wait "can present as busy polling". The shim's own wait is a `Condvar`
(`present.rs:435`–`441`, bounded to 1 s at `:365`), i.e. **not** busy. If waits
are slow *and* CPU is high, the spin is on the guest side (DXVK's latency
tracker), which is consistent with `docs/performances/01:145`–`151` (A) and with
`present.rs:188`–`193`'s note on throttling the guest's frame-latency loop —
worth saying out loud, because it changes where step 2 lands.

---

## 6. What this run cannot decide

* The daemon's own per-frame cost (`docs/performances/01:178`, E) — needs `GNDRM_LOG_DAEMON`,
  i.e. a rebuild (§1.2).
* The publish rate as the daemon sees it (`asc hdr decision n=`) — same reason;
  L3/L4 substitute.
* Bandwidth/tiling (D) — needs GPU counters, not these gates.
* Whether the *cap* or the *compositor* sets the guest's cadence if `gate≠off`:
  the run is then invalid for C and must be repeated with the cap at 0.

---

## 7. Decision table

Assume the run is valid: G0 once, G1 present, `env vars are …` contains
`GNDRM_LOG_VKSHIM=1`, and (if the container was edited) the restore check passed.

### 7.1 Cause B

| observation | verdict |
|---|---|
| G5 `sTypes=[]`, `present_id=0`, zero G6 lines, `presented=0`, `at_queue=0` | **B refuted at precondition.** No `VkPresentIdKHR`, no `vkWaitForPresentKHR`. Nothing about present-wait explains any of the +1.5 pts; drop it and go to A/D/E/G. |
| G5 has `1000294000`/`present_id≠0`, G6 ladder advancing at ≈ the L2 commit rate, outcomes mostly `latched`, `fallback` growing, per-wait durations ≈ `2/refresh` = 16.7 ms (≫ the 2.9 ms frame interval) | **B confirmed** as *the* per-frame stall: the guest waits a full refresh pair per frame on an answer the shim defers. `docs/performances/01:221`–`227` (step 2) is the fix, and the CPU is the guest's latency loop, not the shim. |
| ids present, G6 advancing, outcomes `done`, durations ≪ 1 ms, `at_queue≈commits` | **B refuted as mechanism** (advertisement is inert): the wait is satisfied at queue/commit, so it cannot serialise the frame. Remaining B cost ≤ the call. Look at A (submit+worker+futex per present) and D. |
| ids present, G6 advancing, mixed `done`/`timeout`, durations clustered at ~1 s | **B in its degraded form**: `MAX_WAIT_NS = 1 s` (`present.rs:365`) means the daemon's mark is not arriving. That is a *bug*, not a tuning issue — stop the CPU work and fix the mark path first. |
| ids present, `presented=0` while `releases=` advances at the commit rate | present marks are not reaching the daemon (`send_present_mark` is gated on `present_id != 0 && ino != 0`, `proxy.rs:2031`–`2036`); B is *unmeasurable* and the fifo/mailbox accounting above is the fallback. |

### 7.2 Cause C

| observation | verdict |
|---|---|
| `slot_wait_frac ≥ 0.5`, `dwell ≥ 8 ms`, `p_ge8 > 0.1`, `mean_free ≈ 0`, `depth = 3` | **C confirmed.** Depth + release-gated slots is the pacing constraint; `docs/performances/01:228`–`231` (step 3) applies, and the "add a 4th depth" half of it is testable next by `dxvk.numBackBuffers=4`. |
| `slot_wait_frac ≤ 0.05`, `dwell < 1 ms`, `p_blocked ≈ 0` | **C refuted.** Slots are always available; the guest is render-bound (or shim-bound) and no acquire time is lost. |
| `slot_wait_frac ≥ 0.5` but `dwell < 1 ms` and `mean_free > 0` | **C refuted as cost** (the wait count is real, the time is not). The cost is per-present work, i.e. A. |
| `starve_n > 0` | the guest calls acquire with a **finite** timeout (not the usual `UINT64_MAX`), so its own frame pacing treats a missing image as a timeout — this *is* acquire starvation in the guest's accounting, and it means dropped frames by the guest's estimate. Escalate: read `present.rs`'s callers and `G7`'s `timeout=` value. |
| `gate_holds > 0` / `gate≠off` | **invalid for C** — the dwell belongs to the cap. Re-run with `framerate=0`. |

### 7.3 Null result (the expected shape, and what it means)

`present_id=0`, `slot_wait_frac ≈ 0`, `dwell < 1 ms`, `p_ge8 ≈ 0`,
`starve_n = 0`, all four layers advancing at their expected rates
(L0 ≈ L1 ≈ 345/s, L2 ≈ 340/s, L3 ≈ L2, L4 ≈ 120/s — the panel rate).

That is **not** a failed experiment: it closes B and C and leaves the delta to
A (per-present guest syscalls: the extra `vkQueueSubmit` at `proxy.rs:3416`, the
worker wake + `vkWaitForFences` at `proxy.rs:1974`, the two SEQPACKET datagrams,
and `libgndrm_shim.so`'s `open/close/ioctl/mmap` hooks — `docs/performances/01:145`–`153`),
D (`docs/performances/01:171`–`176`) and E (`docs/performances/01:178`–`185`). The next cheap move is then
`simpleperf record -p <Hades2 pid>` in the same scene (A's own falsifier,
`docs/performances/01:152`) — not step 2's latency-sleep arm, whose premise B would have
already refuted.

Record the *shape* either way: the run also yields `ΔL2/ΔL4` (frames not shown)
and `mean_inflight`, which is what tells you whether the pipeline is
publish-limited or render-limited before anyone edits code.

---

## 8. Ready-to-run host scripts

All three live beside this document and **the on-disk copies are canonical** —
the listings below are the same logic, with the long `NOFLIP`/privilege messages
trimmed. `container-env-flip.py` was exercised on the real `container.gamescope.json`
(byte-identical round trip, 76 other keys untouched); `gamescope-cpu-parse.py` was run on
three synthetic captures built from the format strings above and reproduced every
planted value (§5), including a capture with each line duplicated under two tags.
`plan1-arm.sh` is syntax-checked only — it needs the device.

### 8.1 `container-env-flip.py`

```python
#!/usr/bin/env python3
"""Byte-anchored edit of one container's envVars value.

  container-env-flip.py edit  <in> <out> K=V [K=V ...]
  container-env-flip.py unset <in> <out> K [K ...]
  container-env-flip.py check <in> <out>        # only envVars moved?
  container-env-flip.py show  <file>
"""
import json, re, sys

PAT = re.compile(r'"envVars"\s*:\s*"((?:[^"\\]|\\.)*)"')

def split(blob):
    s = blob.decode("utf-8")
    m = PAT.search(s)
    if not m:
        sys.exit('FATAL: no "envVars" string (wrong file?)')
    return s, m, json.loads('"' + m.group(1) + '"')

def rewrite(blob, toks):
    s, m, _ = split(blob)
    enc = json.dumps(" ".join(toks), ensure_ascii=False)[1:-1]
    out = s[:m.start(1)] + enc + s[m.end(1):]
    try:
        json.loads(out)
    except Exception as e:
        sys.exit(f"FATAL: edited file is not valid JSON: {e}")
    return out.encode()

def apply(blob, pairs, remove=()):
    _, _, cur = split(blob)
    drop = {k for k, _ in pairs} | set(remove)
    toks = [t for t in cur.split(" ") if t and t.split("=", 1)[0] not in drop]
    return rewrite(blob, toks + [f"{k}={v}" for k, v in pairs])

def main():
    cmd, src = sys.argv[1], sys.argv[2]
    blob = open(src, "rb").read()
    if cmd == "show":
        for t in split(blob)[2].split(" "):
            if t: print(t)
        return
    out = sys.argv[3]
    if cmd == "check":
        a, b = json.loads(blob), json.loads(open(out, "rb").read())
        bad = [k for k in set(a) | set(b) if k != "envVars" and a.get(k) != b.get(k)]
        if bad: sys.exit(f"FATAL: keys changed besides envVars: {bad}")
        print(f"ok: envVars only, {len(a) - 1} other keys identical")
        return
    ob = apply(blob, [a.split("=", 1) for a in sys.argv[4:]]) if cmd == "edit" \
        else apply(blob, [], remove=sys.argv[4:])
    open(out, "wb").write(ob)
    print(f"{src} -> {out}: {len(blob)} -> {len(ob)} B ({len(ob) - len(blob):+d})")

main()
```

Measured: `4715 → 4751 (+36)`; `check` reports "ok: envVars only, 76 other keys
identical"; `unset` returns the file byte-identically (`cmp` clean).

### 8.2 `gamescope-cpu-parse.py`

Shipped complete at `analysis/gamescope-cpu-parse.py` (≈260 lines), with
`analysis/synthetic.log` as its self-test fixture (a present-id capture;
expected output `120 presents/s`, `dwell 2.9 ms`, `depth 3`, `B ACTIVE … fast`).
Core:

```python
def d(series, key, t0, t1, sub=None):
    vals = [p[key].get(sub) if sub else p[key] for t, p in series
            if (t0 is None or t >= t0) and (t1 is None or t <= t1)
            and isinstance(p, dict)]
    return vals[-1] - vals[0] if len(vals) >= 2 else None

def per_s(series, key, t0, t1, sub=None):
    pts = [(t, p[key].get(sub) if sub else p[key]) for t, p in series
           if (t0 is None or t >= t0) and (t1 is None or t <= t1)]
    return (pts[-1][1] - pts[0][1]) / (pts[-1][0] - pts[0][0])   # own span
```

`window()` picks the first/last **full** census line (G4, fallback G8) at
`t ≥ settle`; every rate uses its own series' span (§5.1). Verdicts are the
predicates in §7.

### 8.3 `plan1-arm.sh` — flip → launch → sample → restore → parse, one arm

```bash
#!/usr/bin/env bash
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
ADB=${ADB:-adb}; PKG=${PKG:-app.gamenative}
CID=${CID:-STEAM_1145350}; GID=${GID:-1145350}; SRC=${SRC:-STEAM}
SETTLE=${SETTLE:-30}; WINDOW=${WINDOW:-90}; LABEL=${LABEL:-step1}
W=${W:-$here/run-$LABEL}
CFG=/data/data/$PKG/files/imagefs/home/xuser-$CID/.container
REL=files/imagefs/home/xuser-$CID/.container
TMP=/data/local/tmp/plan1.new
GATES=(GNDRM_LOG_VKSHIM=1 GNDRM_LOG_SHIM=1)
mkdir -p "$W"; sh_() { "$ADB" shell "$@"; }; say() { printf '\n== %s\n' "$*"; }

# 0. privilege
"$ADB" get-state >/dev/null || { echo "FATAL: no device"; exit 1; }
if   sh_ "run-as $PKG true" >/dev/null 2>&1; then MODE=run-as
elif sh_ "su -c true"       >/dev/null 2>&1; then MODE=su
else MODE=none; fi
echo "privilege: $MODE"

read_cfg() { if [ "$MODE" = su ]; then "$ADB" exec-out su -c "cat $CFG"
             else "$ADB" exec-out run-as "$PKG" cat "$REL"; fi; }

push_cfg() {   # $1 local file; same-dir temp + rename, owner/mode kept
  "$ADB" push "$1" "$TMP" >/dev/null; sh_ "chmod 644 $TMP"
  if [ "$MODE" = su ]; then
    sh_ "su -c 'cp $TMP $CFG.tmp && chown --reference=$CFG $CFG.tmp && \
          chmod --reference=$CFG $CFG.tmp && (restorecon $CFG.tmp 2>/dev/null || true) && \
          mv -f $CFG.tmp $CFG'"
  else
    sh_ "run-as $PKG sh -c 'cat $TMP > $REL.tmp && mv -f $REL.tmp $REL'"
  fi
}

# 1. back up
say "reading $CFG"
read_cfg > "$W/orig.container"
[ -s "$W/orig.container" ] || { echo "FATAL: empty read"; exit 3; }
python3 -c "import json;json.load(open('$W/orig.container'))" || {
  echo "FATAL: device config is not JSON; refusing to touch it"; exit 3; }
cp "$W/orig.container" "$W/orig.container.keep"
python3 "$here/container-env-flip.py" show "$W/orig.container" > "$W/orig.env.txt"

if [ "${NOFLIP:-0}" != 1 ]; then
  [ "$MODE" = none ] && { echo "FATAL: no run-as/su: edit envVars in the app, then NOFLIP=1"; exit 2; }
  # 2. edit, prove, force-stop, swap, verify
  say "editing envVars"
  python3 "$here/container-env-flip.py" edit  "$W/orig.container" "$W/gated.container" "${GATES[@]}"
  python3 "$here/container-env-flip.py" check "$W/orig.container" "$W/gated.container"
  sh_ "am force-stop $PKG" || true
  push_cfg "$W/gated.container"
  read_cfg > "$W/after_push.container"
  cmp "$W/gated.container" "$W/after_push.container" || {
    echo "FATAL: device bytes differ; restoring"; push_cfg "$W/orig.container"; exit 4; }
else
  say "NOFLIP"; grep -qx 'GNDRM_LOG_VKSHIM=1' "$W/orig.env.txt" || {
    echo "FATAL: gate missing from envVars"; exit 3; }
fi

# 3. cold launch + capture
say "launch + capture (${SETTLE}s settle + ${WINDOW}s window)"
sh_ "am force-stop $PKG" || true
"$ADB" logcat -c || true; "$ADB" logcat -G 16M >/dev/null 2>&1 || true
( "$ADB" logcat -v threadtime -b main -b system -b crash > "$W/cap.txt" 2>&1 ) & LOGPID=$!
sleep 2
sh_ "am start -n $PKG/.MainActivity -a app.gamenative.LAUNCH_GAME \
     --ei app_id $GID --es game_source $SRC" || { kill $LOGPID; exit 5; }
sleep $(( SETTLE + WINDOW ))
kill $LOGPID 2>/dev/null || true; wait $LOGPID 2>/dev/null || true

# 4. stop, restore (inverse byte edit, keeps sessionMetadata)
say "restoring"
sh_ "am force-stop $PKG" || true; sleep 2
if [ "${NOFLIP:-0}" != 1 ]; then
  read_cfg > "$W/final.container"
  python3 "$here/container-env-flip.py" unset "$W/final.container" "$W/restored.container" \
          GNDRM_LOG_VKSHIM GNDRM_LOG_SHIM
  python3 "$here/container-env-flip.py" check "$W/final.container" "$W/restored.container"
  push_cfg "$W/restored.container"
  read_cfg > "$W/after_restore.container"
  cmp "$W/restored.container" "$W/after_restore.container" && echo "restore verified"
  python3 - "$W/orig.container.keep" "$W/after_restore.container" <<'PY'
import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:3])
print("envVars identical:", a["envVars"] == b["envVars"])
print("changed keys:", sorted(k for k in set(a) | set(b) if a.get(k) != b.get(k)))
m = b.get("sessionMetadata") or {}
print("session fps:", m.get("avg_fps"), "frames:", m.get("total_frames"))
PY
fi

# 5. parse
say "parse"
python3 "$here/gamescope-cpu-parse.py" "$W/cap.txt" --settle "$SETTLE" | tee "$W/summary.txt"
for pat in 'gn-vkshim: ICD loaded' 'proxy hb: ' 'present mode: declared=' \
           'proxy present: first pNext chain' 'present wait #' \
           'gn-vkshim stall: acquire found no free slot' 'ATOMIC heartbeat #'; do
  printf '%-46s %s\n' "$pat" "$(grep -cF "$pat" "$W/cap.txt" || true)"
done
```

Run it twice: `LABEL=gated ./gamescope-cpu-arm.sh`, and — if B/C come back null — a
second `LABEL=latency-sleep-off` arm with `dxvk.latencySleep = False` in the
container's `dxvk.conf` (`docs/performances/01:162`) only to confirm the null, since the
gates answer B directly.

---

## 9. Cost and risk

| item | cost |
|---|---|
| sessions | 1 (plus 1 optional control) |
| device writes | one `.container`, back up + inverse-edit restore, byte-verified |
| log volume | ~1–2 lines/s from the shim + gamescope's own stderr; 16 MB buffer, 120 s |
| CPU perturbation | the gate adds a `logf!` string build per census line only; counters are already incremented unconditionally at the hot sites (`proxy.rs:2705`, `:8077`), so the arm's CPU numbers remain comparable to the ungated baseline |
| correctness risk | the emitters are `eprintln!` through the app's Timber bridge; if the bridge line is >4 KB logcat truncates it (`docs/04-gamenative-app.md:251`). Longest line here is G4 at ~330 B — safe. |

---

## 10. Executed — results (2026-11-10, payload `20261110.1`)

Both arms were run with the tooling in this repo (`tools/gamescope-cpu-arm.sh`,
`tools/container-env-flip.py`, `tools/gamescope-cpu-parse.py`), 35 s settle +
95 s window, launched by `app_id` to the title screen — the census questions do
not need the safe area, because a title streams frames too.

```
LABEL=gamescope DS=gamescope SETTLE=35 WINDOW=95 tools/gamescope-cpu-arm.sh
LABEL=x11       DS=x11       SETTLE=35 WINDOW=95 tools/gamescope-cpu-arm.sh
```

Both arms restored the container **byte-identical to the original** (`restore:
byte-identical`, `changed keys: []`), including the `displayServer` the arm
temporarily moved.

### gamescope arm (the measurement)

| reading | value |
|---|---|
| guest presents/s (`vkQueuePresentKHR`) | 357.3 |
| guest acquires/s | 357.3 |
| guest `commits=` | 357.2 |
| daemon atomic ioctls/s (`ATOMIC heartbeat #N`) | **241.9** |
| compositor releases/s (`wl_buffer.release`) | 357.2 |
| present-id on the first pNext chain | **False** (sType 1000275001 only) |
| present-wait lines in window | **0** |
| `slot_waits` / acquires | **0.0** |
| acquire dwell mean | **0.013 ms** |
| acquire ≥ 8.33 ms / ≥ 16.67 ms | **0.0 / 0.0** |
| `starved` (finite-timeout acquire failures) | **0** |
| slot depth | **4** |
| mean in_flight / mean free slots | 1.66 / 2.08 |
| declared / requested / source / gate | **mailbox / immediate / env / off** |
| `declared_fifo` / `declared_other` | **0 / 25009** |
| commit_lat_us (last) | 1228 |

### X11 arm (the control, and it is a control)

`gn-vkshim: ICD loaded` = **0**. With `displayServer=x11` the guest's WSI is
Mesa's, so the ICD shim is never loaded and no census line can exist. That is
the structural fact behind the +5.1 pp guest delta in doc 24: in gamescope mode
the shim exists in the guest, and in X11 mode it does not exist at all.

### What this decides

- **Cause B is dead.** No `VkPresentIdKHR` is chained and `vkWaitForPresentKHR`
  is never called (0 lines). Nothing waits on a present id, so *no* change to
  `present_wait_answer` can save anything on this title. Plan item 2 →
  **closed** (`03-present-wait.md` keeps the design for a title that
  does opt in, and it is now explicitly conditional on that).
- **Cause C is dead.** Zero slot waits, 0.013 ms dwell, 1.66 slots in flight and
  2.08 free, `starved=0`. The acquire is not stalling. Plan item 3 → **closed**,
  including its "add a 4th slot" sub-item: **the depth is already 4**, not 3.
- **Cause F cannot even be reached.** `declared_fifo=0` over 25 009 presents: the
  container's `MESA_VK_WSI_PRESENT_MODE=mailbox` overrides the title's own
  `immediate` request and the shim never enters a FIFO path. Plan item 5 stays
  closed for a stronger reason than the semantic one in
  `06-present-mode-declaration.md`.
- **A new, unexplained number worth one look:** the daemon serves **241.9**
  atomic commits/s while the guest presents **357.3/s**. Under mailbox the
  compositor legitimately drops replaced frames, so this is not a defect — but
  it means ~115 presents/s are answered without ever reaching an atomic commit,
  and it is the one asymmetry in the pipeline that no doc yet explains. It is a
  pacing question, not a CPU one.

### Cost of the run, as designed

One session per arm, no rebuild, no APK change; the gate adds one `logf!` build
per census line and the counters are already unconditional at the hot sites, so
the arm's CPU remains comparable. The whole thing is two commands.

---

## 11. Second run: the app now strips the present-mode and image-count overrides

`XServerScreen.kt` gained, on the gamescope route only:

```kotlin
if (!isX11Display) {
    envVars.remove("MESA_VK_WSI_PRESENT_MODE")
    envVars.remove("WRAPPER_MAX_IMAGE_COUNT")
}
```

so the guest's own request reaches the shim instead of the container's. Re-ran the
census arm on the resulting APK (`DS=gamescope`, same payload, same window):

| reading | before (§10) | after | note |
|---|---|---|---|
| declared / requested / source | mailbox / immediate / **env** | **immediate / immediate / guest** | the change works as intended |
| slot depth | 4 | **3** | caused by the **present mode**, not the image count — see below |
| `slot_waits` / acquires | 0.0 | **0.098 (9.8 %)** | new, and caused by the depth drop |
| acquire dwell mean | 0.013 ms | 0.028 ms | still sub-30 µs |
| acquire ≥ 8.33 ms | 0.0 | **0.0** | no frame-rate-affecting stall |
| `starved` | 0 | 0 | no failed acquire |
| mean free slots | 2.08 | 1.07 | tighter, as expected at depth 3 |
| guest presents/s | 357.3 | 357.3 | unchanged |
| daemon atomic ioctls/s | 241.9 | **242.0** | **unchanged — so the 1.48 ratio is not a mailbox artefact** |
| `VkPresentIdKHR` / present waits | none / 0 | none / 0 | still refuted |

### Correcting the depth story: it is the mode, not `WRAPPER_MAX_IMAGE_COUNT`

The first write-up of this run said "the removed ceiling had been granting a 4th
slot". That is wrong, and the source says so plainly.

`WRAPPER_MAX_IMAGE_COUNT` is read in exactly one place, `max_swapchain_images()`
(`proxy.rs:7387`):

```rust
match std::env::var("WRAPPER_MAX_IMAGE_COUNT").ok().and_then(|v| v.trim().parse::<u32>().ok()) {
    Some(v) if v >= 2 => v.min(GN_MAX_SC_IMAGES as u32),   // 8
    _                => GN_MAX_SC_IMAGES as u32,           // 8
}
```

This container set it to **`0`**, and `0` fails the `v >= 2` guard, so it took the
`_` arm and returned `GN_MAX_SC_IMAGES` = 8 — **identical to leaving it unset**.
Removing that variable from the env was therefore a no-op here; it only matters
when the value is ≥ 2, where it lowers both the advertised `maxImageCount`
(`proxy.rs:10016`) and the slot ceiling.

The 4th slot came from the **mode**, in `swapchain_slots()` (`proxy.rs:7364`):

```rust
let base = asked.clamp(2, ceiling.max(2));
if mode == VK_PRESENT_MODE_MAILBOX_KHR {
    base.max(SC_MAILBOX_IMAGES.min(ceiling.max(2)))   // SC_MAILBOX_IMAGES = 4
} else {
    base                                              // immediate/FIFO: the ask
}
```

`SC_MAILBOX_IMAGES = 4` is Mesa's `WSI_WL_BUMPED_NUM_IMAGES` — "for true mailbox
mode we need at least 4 images" — so a mailbox swapchain is floored at 4 while
the ask (`SC_DEFAULT_IMAGES`, and DXVK's 3) is 3. Switching the guest from mailbox
to immediate removes the floor, and the depth falls to the asked-for 3 on its own.

So the single effective component of the app change is the **present-mode
removal**; the image-count removal did nothing for this container. Consequence
for the Kotlin comment: "a ceiling set here would compete with what the surface
capabilities report" is true only for values ≥ 2 — a `0` (which is what the
container ships) is already treated as absent.

**The atomic rate not moving is the interesting part.** Switching the guest from
mailbox to immediate does not change how often gamescope commits (242/s against
357 presents/s), which means the compositor's commit cadence is paced by
something else — not by the present mode and not by frame arrival. That is the
one unexplained number in the pipeline and it is documented here as such.

### CPU effect, measured two ways

The two obvious in-game arms are NOT a controlled comparison: the save now spawns
in a different room than the §2 pair, and that room is heavier. So the control is
the one scene that is byte-identical across runs — the title screen.

| scene | metric | old APK (mailbox, depth 4) | new APK (immediate, depth 3) |
|---|---|---|---|
| **title** (controlled) | CPU busy | 27.3 % | **25.7 %** (−1.6) |
| | CPU sys | 14.9 % | 13.8 % (−1.1) |
| | package power | 8.06 W | **7.84 W** (−0.22) |
| | GPU busy / clock | 38.4 % / 607 MHz | 38.5 % / 607 MHz |

So on the only scene where nothing but the APK differs, the change is a small
**win** (−1.6 pts of 8 cores, −0.22 W). The in-game pair on the new room measures
+3.9 pts against X11 (46.1 % → 50.0 %, sys +3.5, 19.63 → 19.96 W, fps 338.8 vs
339.3) — but that room has no old-APK counterpart, so it cannot be laid at this
change's door and must not be compared with the +0.3 pts of the §2 pair.

**What is still owed:** one in-game A/B of the two configurations on the *same*
room, to separate *mode* from *depth*. A rebuild is not needed for that: set
`dxvk.numBackBuffers = 4` in the container, which makes the guest ask for 4 and
restores the depth under immediate (`base = asked = 4`), so the 9.8 % slot
pressure can be isolated from the mode change itself. If depth-4+immediate lands
on depth-3+immediate, the slot pressure costs nothing and the mode is the whole
effect.
