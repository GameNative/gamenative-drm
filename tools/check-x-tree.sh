#!/usr/bin/env bash
# check-x-tree.sh — device evidence: dump the live X window tree and the
# presenting window's attributes while a gamescope session runs.
#
# This is a diagnostic, not a gate: it needs a running session and an
# authorized device, so it reports and exits 0 unless the probe cannot build.
# It builds tools/xtree-probe.c for aarch64, installs it into the app sandbox
# with run-as, and runs it against the session's own X socket.
#
# Why it matters: gamescope (steamcompmgr) only tracks X windows that are
# direct children of the X root (CreateNotify/ReparentNotify parent==root), so
# the presenting window's parent chain is what says whether the compositor can
# ever see it. Under `wine explorer /desktop=shell` the app's toplevel is a
# child of wine's desktop X window, never a root child.
#
# Usage: ./tools/check-x-tree.sh [target-xid-hex]
# Env:   ADB_SERIAL (default: first device), GN_PKG (default app.gamenative),
#        X_SOCK (default the gamescope session socket, app-relative)

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PKG="${GN_PKG:-app.gamenative}"
X_SOCK="${X_SOCK:-files/imagefs/usr/tmp/.X11-unix/X0}"
PROBE="$ROOT/target/aarch64-linux-android/release/gn-xtree-probe"
# The probe ships in the gamescope payload as bin/gn-xtree-probe; when a device
# already has the payload, that copy can be run directly:
#   adb shell run-as $PKG env LD_LIBRARY_PATH=files/gamescope/lib \
#       files/gamescope/bin/gn-xtree-probe -display :0
ADB=(adb)
[ -n "${ADB_SERIAL:-}" ] && ADB=(adb -s "$ADB_SERIAL")

bash "$ROOT/tools/build-xtree-probe.sh"
"${ADB[@]}" push "$PROBE" /data/local/tmp/gn-xtree-probe >/dev/null
"${ADB[@]}" shell "run-as $PKG cp /data/local/tmp/gn-xtree-probe files/gn-xtree-probe"
"${ADB[@]}" shell "run-as $PKG chmod 755 files/gn-xtree-probe"

echo "session X socket (app-relative): $X_SOCK"
# shellcheck disable=SC2086
timeout 30 "${ADB[@]}" shell "run-as $PKG env LD_LIBRARY_PATH=files/imagefs/usr/lib files/gn-xtree-probe '$X_SOCK' ${1:-}"
echo "diagnostic only: the success case is the presenting window IsViewable with its"
echo "top-level ancestor a direct child of the root (so gamescope composes it)."
