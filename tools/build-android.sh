#!/usr/bin/env bash
# build-android.sh — cross-build libgamenative_drm.so (the gndrm-android
# cdylib: JNI + SEQPACKET server + EGL presenter) for aarch64-android.
#
# Usage:
#   ./tools/build-android.sh           # release → target/aarch64-linux-android/release/libgamenative_drm.so
#   ./tools/build-android.sh debug     # debug profile
#
# NDK resolution matches build-shim.sh / the gamescope toolchain:
# $ANDROID_NDK_HOME, then the newest ~/Android/Sdk/ndk/*.
# API level 28 (16 KB page support comes from the NDK default linker flags
# plus the workspace's max-page-size setting in the daemon binaries — for a
# cdylib loaded by the app, the NDK clang driver's defaults apply).

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

PROFILE="--release"; DIR=release
if [ "${1:-}" = "debug" ]; then PROFILE=""; DIR=debug; fi

export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CLANG"
# rust-std component for the tier-2 target must be installed (rustup).
cargo build $PROFILE --target aarch64-linux-android -p gndrm-android

SO="$ROOT/target/aarch64-linux-android/$DIR/libgamenative_drm.so"
file "$SO"
echo "built:  $SO (NDK ${NDK##*/}, API 28)"
