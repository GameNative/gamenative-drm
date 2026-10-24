#!/usr/bin/env bash
# b9-test.sh — B9 host integration test (doc 02 §8), extended for the render
# node (doc 08):
#   real libdrm client → LD_PRELOAD shim → unix socket → gndrm-hostd → Device.
#
# Three phases, each against a fresh gndrm-hostd:
#   1. card node   — the full KMS sequence (b9_client) on the main socket.
#   2. render node — Xwayland's glamor surface (render_node_client) on the
#                    dedicated render listener (main socket name + "-render"),
#                    served by the same `server::serve_render_listener` the
#                    in-app libgamenative_drm.so runs.
#   3. concurrent  — a render client held open while the card client runs, i.e.
#                    the device scenario (gamescope + Xwayland, one daemon).
#
# Builds everything, starts the daemon, runs the clients, reports PASS/FAIL.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="${TMPDIR:-/tmp}/gndrm-b9.sock"
OUT="$ROOT/target/shim"

cd "$ROOT"
cargo build -p gndrm-daemon --bin gndrm-hostd
./tools/build-shim.sh

HOSTD="$ROOT/target/debug/gndrm-hostd"
HOSTD_PID=""

start_hostd() {
    rm -f "$SOCK" "$SOCK-render"
    "$HOSTD" "$SOCK" &
    HOSTD_PID=$!
    for _ in $(seq 50); do
        [ -S "$SOCK" ] && [ -S "$SOCK-render" ] && return 0
        sleep 0.1
    done
    echo "FAIL: daemon sockets never appeared ($SOCK, $SOCK-render)" >&2
    kill "$HOSTD_PID" 2>/dev/null || true
    exit 1
}

stop_hostd() {
    [ -n "$HOSTD_PID" ] || return 0
    kill "$HOSTD_PID" 2>/dev/null || true
    wait "$HOSTD_PID" 2>/dev/null || true
    HOSTD_PID=""
}

trap 'stop_hostd' EXIT

echo "== phase 1: card node (b9_client) =="
start_hostd
LD_PRELOAD="$OUT/libgndrm_shim.so" \
GAMENATIVE_DRM_SOCKET="$SOCK" \
GNDRM_LOG_SHIM=1 \
    "$OUT/b9_client"
stop_hostd

echo
echo "== phase 2: render node (render_node_client → render listener) =="
start_hostd
LD_PRELOAD="$OUT/libgndrm_shim.so" \
GAMENATIVE_DRM_SOCKET="$SOCK" \
GNDRM_RENDER_NODE_CLIENT=1 \
GNDRM_LOG_SHIM=1 \
    "$OUT/render_node_client"
stop_hostd

echo
echo "== phase 2b: render-node request matrix (Mesa gbm/kopper/glamor) =="
# Every request Mesa's gbm/kopper/glamor path issues on the render fd, issued
# through the shim; each step is alarm-guarded so a request the daemon never
# answers is reported as a hang rather than hanging this script.
start_hostd
LD_PRELOAD="$OUT/libgndrm_shim.so" \
GAMENATIVE_DRM_SOCKET="$SOCK" \
GNDRM_RENDER_NODE_CLIENT=1 \
GNDRM_LOG_SHIM=1 \
    "$OUT/render_blame"
stop_hostd

echo
echo "== phase 3: card + render concurrently on one daemon =="
start_hostd
# The render client holds its connection open (no ioctls during the hold) while
# the card client runs, so both are served by the same daemon at once.
LD_PRELOAD="$OUT/libgndrm_shim.so" \
GAMENATIVE_DRM_SOCKET="$SOCK" \
GNDRM_RENDER_NODE_CLIENT=1 \
GNDRM_HARNESS_HOLD_MS=1500 \
    "$OUT/render_node_client" &
RN_PID=$!
LD_PRELOAD="$OUT/libgndrm_shim.so" \
GAMENATIVE_DRM_SOCKET="$SOCK" \
    "$OUT/b9_client"
wait "$RN_PID"
stop_hostd

echo
echo "== phase 4: an unanswered request fails fast instead of hanging =="
# HARD REQUIREMENT: a render channel that never answers must fail the caller,
# not freeze it. A listener that accepts and never replies stands in for a
# wedged daemon; the shim's NODE_KIND handshake must time out and fail the
# open, so Xwayland falls back to software and still reaches XWM.
rm -f "$SOCK" "$SOCK-render"
python3 - "$SOCK" <<'PY' &
import socket, sys, time
path = sys.argv[1] + "-render"
s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
s.bind(path)
s.listen(8)
conns = []
for _ in range(2):          # RPC then event: accept both, answer neither
    conns.append(s.accept()[0])
time.sleep(300)
PY
SILENT_PID=$!
sleep 0.4
T0=$(date +%s)
set +e
timeout 30 env LD_PRELOAD="$OUT/libgndrm_shim.so" \
    GAMENATIVE_DRM_SOCKET="$SOCK" GNDRM_RENDER_NODE_CLIENT=1 GNDRM_LOG_SHIM=1 \
    "$OUT/render_blame" >/dev/null 2>&1
RC=$?
set -e
T1=$(date +%s)
kill "$SILENT_PID" 2>/dev/null || true
wait "$SILENT_PID" 2>/dev/null || true
rm -f "$SOCK-render"
if [ "$RC" -eq 124 ]; then
    echo "FAIL: render_blame hung on an unanswered request (killed by timeout)" >&2
    exit 1
fi
echo "unanswered request: open failed after $((T1 - T0))s (rc=$RC), no hang"
# The bounded budget is 5s; allow generously for CI scheduling.
if [ "$((T1 - T0))" -gt 20 ]; then
    echo "FAIL: failure took $((T1 - T0))s, longer than the bounded budget" >&2
    exit 1
fi

echo
echo "== phase 5: the event channel is bounded (no permanent block) =="
# The DRM fd handed to the app is the event channel. With no producer it must
# return EAGAIN at the configured bound rather than block the app's thread.
start_hostd
T0=$(date +%s)
set +e
LD_PRELOAD="$OUT/libgndrm_shim.so" \
GAMENATIVE_DRM_SOCKET="$SOCK" GNDRM_RENDER_NODE_CLIENT=1 GNDRM_LOG_SHIM=1 \
    "$OUT/event_fd_read_client"
RC=$?
set -e
T1=$(date +%s)
stop_hostd
if [ "$RC" -ne 0 ]; then
    echo "FAIL: event-channel bound check failed (rc=$RC)" >&2
    exit 1
fi
if [ "$((T1 - T0))" -gt 20 ]; then
    echo "FAIL: event-channel wait took $((T1 - T0))s, longer than the bound" >&2
    exit 1
fi

echo
echo "PASS: b9 — card node, render node, request matrix, and both concurrently, through the shim"
