#!/usr/bin/env bash
# build-shim.sh — build libgndrm_shim.so for the host (glibc, for the B9
# integration test) and/or for Android (NDK clang, for on-device use).
#
# Usage:
#   ./tools/build-shim.sh           # host build → target/shim/libgndrm_shim.so
#   ./tools/build-shim.sh android   # + Android build → target/shim/android/aarch64/
#
# NDK path resolution matches the gamescope toolchain: $ANDROID_NDK_HOME,
# then the newest ~/Android/Sdk/ndk/*.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/modules/gndrm-shim/gndrm_shim.c"
OUT="$ROOT/target/shim"
CFLAGS="-Wall -Wextra -Werror -O2 -fPIC"
# NOTE: no -fvisibility=hidden — the interposed symbols (open/ioctl/close)
# must stay exported for LD_PRELOAD interposition to work at all.

mkdir -p "$OUT"

# --- host ----------------------------------------------------------------
cc $CFLAGS -shared -o "$OUT/libgndrm_shim.so" "$SRC" -ldl -lpthread
echo "host:   $OUT/libgndrm_shim.so"

# --- test client (host, real libdrm) --------------------------------------
if pkg-config --exists libdrm; then
    # shellcheck disable=SC2046
    cc -Wall -Wextra -Werror -O2 -o "$OUT/b9_client" \
        "$ROOT/modules/gndrm-shim/test_client.c" $(pkg-config --cflags --libs libdrm)
    echo "host:   $OUT/b9_client"
    # shellcheck disable=SC2046
    cc -Wall -Wextra -Werror -O2 -o "$OUT/render_node_client" \
        "$ROOT/modules/gndrm-shim/render_node_client.c" $(pkg-config --cflags --libs libdrm)
    echo "host:   $OUT/render_node_client"
    # shellcheck disable=SC2046
    cc -Wall -Wextra -Werror -O2 -o "$OUT/render_blame" \
        "$ROOT/modules/gndrm-shim/render_blame.c" $(pkg-config --cflags --libs libdrm)
    echo "host:   $OUT/render_blame"
    # shellcheck disable=SC2046
    cc -Wall -Wextra -Werror -O2 -o "$OUT/event_fd_read_client" \
        "$ROOT/modules/gndrm-shim/event_fd_read_client.c" $(pkg-config --cflags --libs libdrm)
    echo "host:   $OUT/event_fd_read_client"
fi

# --- android ---------------------------------------------------------------
if [ "${1:-}" = "android" ]; then
    NDK="${ANDROID_NDK_HOME:-}"
    if [ -z "$NDK" ]; then
        NDK="$(ls -d "$HOME"/Android/Sdk/ndk/* 2>/dev/null | sort -V | tail -1)"
    fi
    [ -n "$NDK" ] || { echo "error: no NDK found" >&2; exit 1; }

    API=28
    TC="$NDK/toolchains/llvm/prebuilt/linux-x86_64"
    AOUT="$OUT/android/aarch64"
    mkdir -p "$AOUT"
    # bionic has dl* in libc; pthread is built in. Only libc needed.
    "$TC/bin/aarch64-linux-android$API-clang" $CFLAGS -shared \
        -o "$AOUT/libgndrm_shim.so" "$SRC" -ldl -landroid
    echo "android: $AOUT/libgndrm_shim.so (NDK $(basename "$NDK"), API $API)"
fi
