#!/usr/bin/env bash
# gamescope-cpu-arm.sh - one step-1 arm: gate the guest's WSI census, launch Hades II by
# app_id, capture logcat, put the container back, parse.
#
#   ./gamescope-cpu-arm.sh                 # flip, run, restore, parse
#   LABEL=x11 SETTLE=30 WINDOW=90 ./gamescope-cpu-arm.sh
#   NOFLIP=1 ./gamescope-cpu-arm.sh        # container already carries the gates (UI path):
#                                  # no file write at all, nothing to restore
#
# Writes only under ./run-$LABEL/ on the host and (temporarily) the one
# `.container` on the device. Never uninstalls, never clears app data.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
ADB=${ADB:-adb}
PKG=${PKG:-app.gamenative}
CID=${CID:-STEAM_1145350}          # container id, matches the JSON's "id"
GID=${GID:-1145350}                # Steam appid -> app_id extra
SRC=${SRC:-STEAM}                  # game_source extra -> appId STEAM_1145350
SETTLE=${SETTLE:-30}               # s of scene settle before the census window
WINDOW=${WINDOW:-90}               # s of steady-state sampling
DS=${DS:-}                         # optional displayServer override (gamescope|x11)
LABEL=${LABEL:-${DS:-step1}}
W=${W:-$here/run-$LABEL}
CFG=/data/data/$PKG/files/imagefs/home/xuser-$CID/.container
REL=files/imagefs/home/xuser-$CID/.container
TMP=/data/local/tmp/plan1.new
GATES=(GNDRM_LOG_VKSHIM=1 GNDRM_LOG_SHIM=1)

mkdir -p "$W"
sh_() { "$ADB" shell "$@"; }
say() { printf '\n== %s\n' "$*"; }

# cpuss temperature, the die the workloads actually saturate.
cpuss() {
  sh_ 'for z in /sys/class/thermal/thermal_zone*; do
         [ "$(cat $z/type 2>/dev/null)" = "cpuss-0-0" ] && cat $z/temp; done' | tr -d '\r'
}

# Wait until the die has stopped heating before opening the window. Two arms run
# back to back start from different temperatures, and leakage rises with heat, so
# an ungated window silently compares "gamescope" with "whatever ran second".
# Plateau = the sampled temperature moved less than PLATEAU_DC (tenths of a
# degree) across PLATEAU_S. Gives up after PLATEAU_MAX_S and says so.
wait_plateau() {
  local prev cur waited=0 step=15
  prev=$(cpuss); sleep "$step"; waited=$((waited + step))
  while [ "$waited" -lt "${PLATEAU_MAX_S:-240}" ]; do
    cur=$(cpuss)
    if [ -n "$prev" ] && [ -n "$cur" ]; then
      local d=$(( cur > prev ? cur - prev : prev - cur ))
      if [ "$d" -le "${PLATEAU_DC:-20}" ]; then
        echo "thermal plateau at $(( cur / 1000 )) C (moved $(( d / 10 )) C in ${step}s)"
        return 0
      fi
    fi
    prev=$cur; sleep "$step"; waited=$((waited + step))
  done
  echo "WARNING: no thermal plateau within ${PLATEAU_MAX_S:-240}s (last $(( ${cur:-0} / 1000 )) C)"
  return 1
}

say "device"
"$ADB" get-state >/dev/null || { echo "FATAL: no device"; exit 1; }
if sh_ "run-as $PKG true" >/dev/null 2>&1; then MODE=run-as
elif sh_ "su -c true" >/dev/null 2>&1;      then MODE=su
else MODE=none; fi
echo "privilege: $MODE"

read_cfg() {  # -> stdout, binary-safe
  if [ "$MODE" = su ]; then "$ADB" exec-out su -c "cat $CFG"
  else "$ADB" exec-out run-as "$PKG" cat "$REL"; fi
}

push_cfg() {  # $1 = local file; same-directory temp + rename, owner/mode kept
  "$ADB" push "$1" "$TMP" >/dev/null
  sh_ "chmod 644 $TMP"
  if [ "$MODE" = su ]; then
    sh_ "su -c 'cp $TMP $CFG.tmp && chown --reference=$CFG $CFG.tmp && \
          chmod --reference=$CFG $CFG.tmp && (restorecon $CFG.tmp 2>/dev/null || true) && \
          mv -f $CFG.tmp $CFG'"
  else
    sh_ "run-as $PKG sh -c 'cat $TMP > $REL.tmp && mv -f $REL.tmp $REL'"
  fi
}

# ---- 1. read + back up the container -------------------------------------
say "reading $CFG"
read_cfg > "$W/orig.container"
[ -s "$W/orig.container" ] || { echo "FATAL: empty read (privilege or path wrong)"; exit 3; }
python3 -c "import json,sys;json.load(open('$W/orig.container'))" \
  || { echo "FATAL: device config is not JSON; refusing to touch it"; exit 3; }
python3 "$here/container-env-flip.py" show "$W/orig.container" > "$W/orig.env.txt"
echo "envVars tokens: $(wc -l < "$W/orig.env.txt")"
cp "$W/orig.container" "$W/orig.container.keep"

if [ "${NOFLIP:-0}" != 1 ] && [ "$MODE" = none ]; then
  cat <<'EOM'
FATAL: no run-as and no su, so the container file cannot be edited from the
host. Do it in the app instead, then re-run with NOFLIP=1:
  Settings -> (container) -> Environment Variables -> add
      GNDRM_LOG_VKSHIM=1
      GNDRM_LOG_SHIM=1
EOM
  exit 2
fi

if [ "${NOFLIP:-0}" = 1 ]; then
  say "NOFLIP: using the gates already in the container"
  grep -qx 'GNDRM_LOG_VKSHIM=1' "$W/orig.env.txt" \
    || { echo "FATAL: NOFLIP but GNDRM_LOG_VKSHIM=1 is not in envVars"; exit 3; }
else
  # ---- 2. edit + prove only envVars moved, then write atomically ---------
  say "editing envVars"
  python3 "$here/container-env-flip.py" edit "$W/orig.container" "$W/gated0.container" "${GATES[@]}"
  if [ -n "$DS" ]; then
    echo "displayServer: $(python3 "$here/container-env-flip.py" getfield "$W/orig.container" displayServer) -> $DS"
    python3 "$here/container-env-flip.py" setfield "$W/gated0.container" "$W/gated.container" displayServer "$DS"
  else
    cp "$W/gated0.container" "$W/gated.container"
  fi
  # prove nothing but envVars (and, when asked, displayServer) moved
  python3 - "$W/orig.container" "$W/gated.container" "${DS:-}" <<'PY'
import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:3])
want = sys.argv[3]
allowed = {"envVars"} | ({"displayServer"} if want else set())
bad = sorted(k for k in set(a) | set(b) if k not in allowed and a.get(k) != b.get(k))
if bad:
    sys.exit(f"FATAL: unexpected keys changed: {bad}")
print("ok: only", ", ".join(sorted(allowed)), "differ")
if want:
    assert b["displayServer"] == want, (b["displayServer"], want)
print("displayServer on the pushed file:", b["displayServer"])
PY
  sh_ "am force-stop $PKG" || true                       # drop the in-memory copy
  push_cfg "$W/gated.container"
  read_cfg > "$W/after_push.container"
  cmp "$W/gated.container" "$W/after_push.container" \
    || { echo "FATAL: on-device bytes differ from the pushed file; restoring"; \
         push_cfg "$W/orig.container"; exit 4; }
  echo "on-device bytes verified ($(wc -c < "$W/after_push.container") B)"
fi

# ---- 3. cold launch + capture -------------------------------------------
say "launch + capture (${SETTLE}s settle + ${WINDOW}s window)"
sh_ "am force-stop $PKG" || true
"$ADB" logcat -c || true
"$ADB" logcat -G 16M >/dev/null 2>&1 || true
# shellcheck disable=SC2086
( "$ADB" logcat -v threadtime -b main -b system -b crash > "$W/cap.txt" 2>&1 ) &
LOGPID=$!
sleep 2
sh_ "am start -n $PKG/.MainActivity -a app.gamenative.LAUNCH_GAME --ei app_id $GID --es game_source $SRC" \
  || { kill $LOGPID; echo "FATAL: launch intent failed"; exit 5; }
# Thermal gate before the window, not after: `SETTLE` alone lets a back-to-back
# arm open its window on a die that is still heating.
wait_plateau || true
# The scene is whatever the save spawned into; record what it actually was so two
# arms can be matched (or refused) instead of assumed comparable.
"$ADB" exec-out screencap -p > "$W/scene.png" 2>/dev/null || true
sleep "$WINDOW"
kill $LOGPID 2>/dev/null || true; wait $LOGPID 2>/dev/null || true
echo "captured $(wc -l < "$W/cap.txt") lines"
if [ -s "$W/scene.png" ]; then
  python3 "$here/scene-fingerprint.py" "$W/scene.png" | sed 's/^/scene: /'
fi

# ---- 4. stop the session and restore the container ----------------------
say "restoring"
sh_ "am force-stop $PKG" || true
sleep 2
if [ "${NOFLIP:-0}" != 1 ]; then
  read_cfg > "$W/final.container"
  # keep everything the app wrote during the session (sessionMetadata, extraData),
  # drop only the two gates
  python3 "$here/container-env-flip.py" unset "$W/final.container" "$W/restored0.container" GNDRM_LOG_VKSHIM GNDRM_LOG_SHIM
  if [ -n "$DS" ]; then
    orig_ds=$(python3 "$here/container-env-flip.py" getfield "$W/orig.container.keep" displayServer)
    python3 "$here/container-env-flip.py" setfield "$W/restored0.container" "$W/restored.container" displayServer "$orig_ds"
  else
    cp "$W/restored0.container" "$W/restored.container"
  fi
  cmp -s "$W/restored.container" "$W/orig.container.keep" \
    && echo "restore: byte-identical to the original" \
    || echo "restore: envVars back to the original; other bytes differ only by session telemetry"
  push_cfg "$W/restored.container"
  read_cfg > "$W/after_restore.container"
  cmp "$W/restored.container" "$W/after_restore.container" \
    && echo "restore verified on device"
  python3 - "$W/orig.container.keep" "$W/after_restore.container" <<'PY'
import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:3])
print("envVars identical:", a["envVars"] == b["envVars"])
print("changed keys:", sorted(k for k in set(a) | set(b) if a.get(k) != b.get(k)))
m = b.get("sessionMetadata") or {}
print("session fps (app's own):", m.get("avg_fps"), "frames:", m.get("total_frames"),
      "len_s:", m.get("session_length_sec"))
PY
fi

# ---- 5. parse ------------------------------------------------------------
say "parse"
python3 "$here/gamescope-cpu-parse.py" "$W/cap.txt" --settle "$SETTLE" | tee "$W/summary.txt"
say "gates that matter in this capture"
for pat in 'gn-vkshim: ICD loaded' 'proxy hb: ' 'present mode: declared=' \
           'proxy present: first pNext chain' 'present wait #' \
           'gn-vkshim stall: acquire found no free slot' 'ATOMIC heartbeat #'; do
  printf '%-46s %s\n' "$pat" "$(grep -cF "$pat" "$W/cap.txt" || true)"
done
echo
echo "capture: $W/cap.txt   summary: $W/summary.txt"
