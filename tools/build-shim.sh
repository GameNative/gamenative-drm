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
# Reproducibility: a caller-supplied SOURCE_DATE_EPOCH wins; otherwise pin to
# the tree's last commit so identical inputs rebuild identically.
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || echo 0)}"
# -ffile-prefix-map scrubs the build-host path from __FILE__/debug records so it
# cannot leak into the shipped .so (the Rust side has --remap-path-prefix).
CFLAGS="-Wall -Wextra -Werror -O2 -fPIC -ffile-prefix-map=$ROOT=. -ffile-prefix-map=$PWD=."
# The shim's log text is obfuscated at build time: obf-shim.py rewrites each
# LOG(...) site to decode an XOR-encoded literal, so `strings` on the shipped
# .so no longer recovers the messages. The .c stays the readable source.
GEN="$OUT/gen"
mkdir -p "$GEN"
CSRC="$GEN/gndrm_shim.c"
python3 "$ROOT/tools/obf-shim.py" "$SRC" "$CSRC"
# NOTE: no -fvisibility=hidden — the interposed symbols (open/ioctl/close)
# must stay exported for LD_PRELOAD interposition to work at all. What pins the
# exported set is the linker version script below: it makes everything except
# the interposer entry points local, without touching the visible ones.
MAP="$ROOT/tools/exports/libgndrm_shim.map"
LDFLAGS="-Wl,--version-script=$MAP -Wl,--exclude-libs,ALL"

mkdir -p "$OUT"

# --- host ----------------------------------------------------------------
cc $CFLAGS -shared $LDFLAGS -o "$OUT/libgndrm_shim.so" "$CSRC" -ldl -lpthread
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
    # The version script pins the export allowlist; --exclude-libs,ALL drops
    # archive symbols. llvm-strip then removes the static symbol table from the
    # shipped .so (the leak this hardening closes).
    # 16 KB page alignment: the payload ships this .so and Android 16 KB-page
    # devices (and Play's 16 KB requirement) refuse a library whose LOAD
    # segments are aligned below that. NDK r27's driver default is not enough
    # here, so it is explicit, like the gamescope toolchain's flag.
    "$TC/bin/aarch64-linux-android$API-clang" $CFLAGS -shared $LDFLAGS \
        -Wl,-z,max-page-size=16384 \
        -o "$AOUT/libgndrm_shim.so" "$CSRC" -ldl -landroid
    "$TC/bin/llvm-strip" --strip-unneeded "$AOUT/libgndrm_shim.so"
    echo "android: $AOUT/libgndrm_shim.so (NDK $(basename "$NDK"), API $API)"

    bash "$ROOT/tools/check-exports.sh" \
        "$AOUT/libgndrm_shim.so" "$MAP"
fi
