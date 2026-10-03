#!/usr/bin/env bash
# build-android.sh — cross-build libgamenative_drm.so (the gndrm-android
# cdylib: JNI + SEQPACKET server + native SurfaceControl presenter) for
# aarch64-android.
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
# 16 KB page-size support (Google Play requirement): matches the gamescope
# toolchain flag in android/cross/aarch64-linux-android.txt.
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-args=-Wl,-z,max-page-size=16384"
# rust-std component for the tier-2 target must be installed (rustup).
cargo build $PROFILE --target aarch64-linux-android -p gndrm-android

# ICD shim + smoke-test clients (gamescope/android C sources, ported):
# libadrenotools_icd.so links libwayland-client from the gamescope termux
# prefix (build.rs reads GAMESCOPE_TERMUX_PREFIX); the test binaries also
# link libvulkan from there.
cargo build $PROFILE --target aarch64-linux-android -p adrenotools_icd -p gn-utils

SO="$ROOT/target/aarch64-linux-android/$DIR/libgamenative_drm.so"
REL="$ROOT/target/aarch64-linux-android/$DIR"

# Undefined-symbol audit: every import must resolve from a runtime provider
# (see tools/check-elf-deps.sh). Catches extern declarations with no home
# before they become on-device dlopen failures.
DEPS="${GAMESCOPE_TERMUX_PREFIX:-$HOME/termuxfs/aarch64/data/data/com.termux/files/usr}"
# Provider stubs model the deployment runtime (the test devices run
# Android 13 / API 33 — e.g. bionic's memfd_create exists there), not the
# API-28 link floor.
SYSROOT="$NDK/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android/33"
BIONIC=("$SYSROOT/libc.so" "$SYSROOT/libdl.so")
bash "$ROOT/tools/check-elf-deps.sh" "$REL/libgamenative_drm.so" \
    "${BIONIC[@]}" "$SYSROOT/liblog.so" "$SYSROOT/libandroid.so"
bash "$ROOT/tools/check-elf-deps.sh" "$REL/libadrenotools_icd.so" \
    "${BIONIC[@]}" "$DEPS/lib/libwayland-client.so" \
    "$DEPS/lib/libxcb.so" "$DEPS/lib/libxcb-dri3.so" "$DEPS/lib/libxcb-present.so" \
    "$DEPS/lib/libxcb-sync.so"
bash "$ROOT/tools/check-elf-deps.sh" "$REL/gn-dmabuf-test" \
    "${BIONIC[@]}" "$DEPS/lib/libwayland-client.so" "$DEPS/lib/libvulkan.so"
bash "$ROOT/tools/check-elf-deps.sh" "$REL/gn-wsi-test" \
    "${BIONIC[@]}" "$DEPS/lib/libwayland-client.so" "$DEPS/lib/libvulkan.so"
bash "$ROOT/tools/check-elf-deps.sh" "$REL/icd_probe" "${BIONIC[@]}"

# X11 window-tree probe (guest evidence client, ships in the gamescope payload
# as bin/gn-xtree-probe). Same build/audit style as the other guest clients;
# its own script keeps the NDK/clang wiring and the ELF dep audit.
bash "$ROOT/tools/build-xtree-probe.sh"

file "$SO"
echo "built:  $SO (NDK ${NDK##*/}, API 28)"
echo "built:  $REL/libadrenotools_icd.so"
echo "built:  $REL/gn-dmabuf-test"
echo "built:  $REL/gn-wsi-test"
echo "built:  $REL/gn-xtree-probe"
