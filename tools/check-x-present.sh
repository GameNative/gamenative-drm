#!/usr/bin/env bash
# check-x-present.sh — host end-to-end check of the ICD shim's X11 present path.
#
# Builds tools/xpresent-check.c and runs it against the host X server. The
# harness performs exactly the shim's `crate::x11` sequence - import a linear
# dma-buf with DRI3 PixmapFromBuffer, make it the window's content with
# Present, release the slot off the present's X sync idle fence - against a
# real Xwayland and a real GPU, then reads the window back with XGetImage.
#
# A SKIP (exit 2) means this host cannot exercise DRI3 present (no X server,
# no DRI3/Present, or no importable dma-buf node); the device-side check is the
# session log line `x11: window 0x... DRI3 1.2 Present 1.2 - X11 route ready`
# plus the game's visible background.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

OUT="$ROOT/target/xpresent-check"
mkdir -p "$(dirname "$OUT")"

if ! pkg-config --exists xcb xcb-dri3 xcb-present xcb-sync gbm; then
    echo "SKIP: host lacks xcb/xcb-dri3/xcb-present/xcb-sync/gbm dev packages"
    exit 2
fi

# shellcheck disable=SC2046
cc -Wall -Wextra -Werror -O2 -o "$OUT" tools/xpresent-check.c \
    $(pkg-config --cflags --libs xcb xcb-dri3 xcb-present xcb-sync gbm)

set +e
"$OUT"
rc=$?
set -e
case "$rc" in
    0) echo "check-x-present: PASS";;
    2) echo "check-x-present: SKIP (host cannot exercise DRI3 present)";;
    *) echo "check-x-present: FAIL (rc=$rc)" >&2; exit 1;;
esac
exit "$rc"
