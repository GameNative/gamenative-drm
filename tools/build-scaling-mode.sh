#!/usr/bin/env bash
# build-scaling-mode.sh — cross-build tools/scaling-mode.c for aarch64-android.
#
# scaling-mode sets gamescope's live scaling-mode root property
# (GAMESCOPE_NEW_SCALING_SCALER, a 32-bit XA_CARDINAL = the
# GamescopeUpscaleScaler value) over the guest session's own X connection, so a
# mode change applies without relaunching. See tools/scaling-mode.c.
#
# NDK resolution matches build-android.sh / build-shim.sh: $ANDROID_NDK_HOME,
# then the newest ~/Android/Sdk/ndk/*. The xcb headers/libs come from the
# gamescope termux prefix (the same Android/bionic libxcb the ICD shim and
# gn-xtree-probe link).
#
# Output goes to the same release dir as the other guest clients
# (gn-wsi-test / gn-dmabuf-test / gn-xtree-probe), so package-gamescope.sh ships
# it as bin/gn-scaling-mode next to them. build-android.sh runs this script, so
# one android build produces it. The ELF dep audit (check-elf-deps.sh) is the
# same one the other clients get: every undefined symbol must resolve from a
# runtime provider, so an extern with no home fails here instead of on device.
#
# Usage: ./tools/build-scaling-mode.sh
# Output: target/aarch64-linux-android/release/gn-scaling-mode (aarch64, API 28)

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

NDK="${ANDROID_NDK_HOME:-}"
if [ -z "$NDK" ]; then
    NDK="$(ls -d "$HOME"/Android/Sdk/ndk/* 2>/dev/null | sort -V | tail -1)"
fi
[ -n "$NDK" ] || { echo "error: no NDK found (set ANDROID_NDK_HOME)" >&2; exit 1; }

CLANG="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android28-clang"
[ -x "$CLANG" ] || { echo "error: $CLANG not found" >&2; exit 1; }

PREFIX="${GAMESCOPE_TERMUX_PREFIX:-$HOME/termuxfs/aarch64/data/data/com.termux/files/usr}"
[ -f "$PREFIX/lib/libxcb.so" ] || {
    echo "error: no libxcb in $PREFIX/lib (set GAMESCOPE_TERMUX_PREFIX)" >&2; exit 1
}

OUT="$ROOT/target/aarch64-linux-android/release/gn-scaling-mode"
mkdir -p "$(dirname "$OUT")"

# -idirafter keeps the NDK's bionic headers first; only xcb/* resolves from the
# prefix (its stdio.h etc. would otherwise shadow bionic's).
"$CLANG" -O2 -Wall -Wextra -Wno-nullability-completeness \
    -idirafter "$PREFIX/include" \
    -Wl,-z,max-page-size=16384 \
    -o "$OUT" tools/scaling-mode.c \
    -L "$PREFIX/lib" -lxcb

# ELF dep audit, same style as build-android.sh: bionic stubs model the
# deployment runtime (the test devices run Android 13 / API 33), the prefix's
# libxcb.so provides the xcb entry points the payload ships.
SYSROOT="$NDK/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android/33"
BIONIC=("$SYSROOT/libc.so" "$SYSROOT/libdl.so")
bash "$ROOT/tools/check-elf-deps.sh" "$OUT" "${BIONIC[@]}" "$PREFIX/lib/libxcb.so"

file "$OUT"
echo "built:  $OUT (NDK ${NDK##*/}, API 28)"
