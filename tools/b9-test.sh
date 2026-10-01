#!/usr/bin/env bash
# b9-test.sh — B9 host integration test (doc 02 §8):
#   real libdrm client → LD_PRELOAD shim → unix socket → gndrm-hostd → Device.
#
# Builds everything, starts the daemon, runs the client, reports PASS/FAIL.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SOCK="${TMPDIR:-/tmp}/gndrm-b9.sock"
OUT="$ROOT/target/shim"

cd "$ROOT"
cargo build -p gndrm-daemon --bin gndrm-hostd
./tools/build-shim.sh

HOSTD="$ROOT/target/debug/gndrm-hostd"
rm -f "$SOCK"
"$HOSTD" "$SOCK" &
HOSTD_PID=$!
trap 'kill $HOSTD_PID 2>/dev/null || true' EXIT

# wait for the socket
for _ in $(seq 50); do
    [ -S "$SOCK" ] && break
    sleep 0.1
done
[ -S "$SOCK" ] || { echo "FAIL: daemon socket never appeared"; exit 1; }

LD_PRELOAD="$OUT/libgndrm_shim.so" \
GAMENATIVE_DRM_SOCKET="$SOCK" \
GNDRM_LOG_SHIM=1 \
    "$OUT/b9_client"
