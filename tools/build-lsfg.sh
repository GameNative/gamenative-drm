#!/usr/bin/env bash
# build-lsfg.sh — build liblsfg_engine.so: the vendored LSFG frame-generation
# engine plus dxvk's DXBC translator, as one shared library.
#
# Usage:
#   ./tools/build-lsfg.sh            # host x86_64, for the dlopen smoke test
#   ./tools/build-lsfg.sh android    # aarch64 Android, for the payload
#
# Why a separate .so rather than objects linked into the ICD:
#   - the ICD's dynamic table is pinned to its three Khronos loader entry points,
#     and the engine needs none of them;
#   - libc++ links normally inside a leaf library, instead of fighting the
#     rustc/linker stub order inside the ICD;
#   - the engine's own undefined set stays a property of this artifact, so
#     check-elf-deps.sh keeps auditing the ICD against the driver set it expects.
#
# The engine's only dependency on its embedder is the two-entry host table in
# lsfg/engine/lsfg_host.h, installed by gamenative-drm at session start.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/lsfg"
OUT_KIND="${1:-host}"

ENGINE_SOURCES=(
    lsfg_alpha lsfg_beta lsfg_chain lsfg_common lsfg_delta lsfg_gamma
    lsfg_generate lsfg_host lsfg_mipmaps lsfg_pacer lsfg_shaders vkr_lsfg
)

if [ "$OUT_KIND" = "android" ]; then
    NDK="${ANDROID_NDK_HOME:-}"
    if [ -z "$NDK" ]; then
        NDK="$(ls -d "$HOME"/Android/Sdk/ndk/* 2>/dev/null | sort -V | tail -1)"
    fi
    [ -n "$NDK" ] || { echo "error: no NDK found (set ANDROID_NDK_HOME)" >&2; exit 1; }
    BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
    CC="$BIN/aarch64-linux-android28-clang"
    CXX="$BIN/aarch64-linux-android28-clang++"
    OUT="$ROOT/target/aarch64-linux-android/release"
    DEFINES=(-DANDROID)
    CFLAGS=(-fPIC -O3 -DNDEBUG)
    CXXFLAGS=(-fPIC -O3 -DNDEBUG)
    # libc++ statically: this library is a leaf, so it adds no NEEDED and the
    # payload does not have to carry libc++_shared.so. The NDK driver links
    # `-lc++_shared` by default, so the choice is made through its own flag.
    LINK_STDLIB=(-static-libstdc++)
    LINK_EXTRA=(-Wl,--strip-all)
else
    CC="${CC:-cc}"
    CXX="${CXX:-g++}"
    OUT="$ROOT/target/lsfg-host"
    DEFINES=()
    CFLAGS=(-fPIC -O2 -g)
    CXXFLAGS=(-fPIC -O2 -g)
    LINK_STDLIB=()
    LINK_EXTRA=()
fi

INCLUDES=(
    -I"$SRC/engine"
    -I"$SRC/dxbc"
    -I"$SRC/thirdparty/dxbc/include/dxbc"
    -I"$SRC/thirdparty/dxbc/include/spirv"
    -I"$SRC/thirdparty/dxbc/include/util"
    -I"$SRC/thirdparty/dxbc/include/dxvk"
)
if [ "$OUT_KIND" != "android" ]; then
    INCLUDES+=(-I"$SRC/host/include")
fi

OBJ="$OUT/obj"
rm -rf "$OBJ"
mkdir -p "$OBJ" "$OUT"

OBJECTS=()
# Object names carry their directory: dxvk has more than one `str.cpp`, and
# flattening to a basename collides them into one file passed twice.
object_name() {
    echo "$OBJ/$(basename "$(dirname "$1")")_$(basename "${1%.*}").o"
}
compile_cpp() {
    local src="$1" obj
    obj="$(object_name "$src")"
    "$CXX" "${CXXFLAGS[@]}" "${DEFINES[@]}" "${INCLUDES[@]}" -c "$src" -o "$obj"
    OBJECTS+=("$obj")
}
compile_c() {
    local src="$1" obj
    obj="$(object_name "$src")"
    "$CC" "${CFLAGS[@]}" "${DEFINES[@]}" "${INCLUDES[@]}" -c "$src" -o "$obj"
    OBJECTS+=("$obj")
}

echo "building liblsfg_engine.so ($OUT_KIND)"
for unit in "${ENGINE_SOURCES[@]}"; do
    compile_cpp "$SRC/engine/$unit.cpp"
done
# `vk_dispatch.c` is C, and must not be compiled as C++.
compile_c "$SRC/engine/vk_dispatch.c"
compile_cpp "$SRC/dxbc/lsfg_dxbc.cpp"
for dir in dxbc spirv util; do
    while IFS= read -r file; do
        compile_cpp "$file"
    done < <(find "$SRC/thirdparty/dxbc/src/$dir" -name '*.cpp' | sort)
done

"$CXX" -shared "${LINK_STDLIB[@]}" "${LINK_EXTRA[@]}" \
    -Wl,--version-script="$SRC/lsfg_engine.map" \
    -o "$OUT/liblsfg_engine.so" "${OBJECTS[@]}"

echo "built: $OUT/liblsfg_engine.so"
