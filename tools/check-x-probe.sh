#!/usr/bin/env bash
# check-x-probe.sh — guard the ICD shim against an init-time X-server dial.
#
# Xwayland loads this ICD from its glamor/zink init. Any code on that path that
# dials an X server and waits for the setup reply deadlocks the server process
# on itself: the compositor binds the X listener socket before Xwayland execs,
# so connect() succeeds, but the single-threaded server *is* the caller and can
# never answer the handshake read. The shim must therefore carry no such probe.
# This asserts the built ICD still contains none of the removed diagnostic
# markers, so a re-added dial fails here instead of hanging a session.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

cargo build -p adrenotools_icd

LIB="$ROOT/target/debug/libadrenotools_icd.so"
[ -f "$LIB" ] || { echo "FAIL: $LIB not built" >&2; exit 1; }

fail=0
for marker in "x11 probe" "xcb_connect_to_fd"; do
    if strings -a "$LIB" | grep -qF -- "$marker"; then
        echo "FAIL: ICD shim still dials the X server at init (marker: '$marker')" >&2
        fail=1
    fi
done

[ "$fail" = 0 ] || exit 1
echo "PASS: ICD shim has no init-time X-server dial (no blocking setup read)"
