#!/usr/bin/env bash
# check-elf-deps.sh <artifact> <provider.so> [provider.so...]
#
# Fails the build when an Android artifact references a dynamic symbol none
# of its runtime providers export. Motivation: an extern declaration that
# has no home (e.g. compiler-rt's __clear_cache, which bionic never
# exports and the NDK only static-links) makes dlopen fail on device while
# host-side builds and readelf spot-checks look perfectly healthy.
#
# Providers are the .so files the symbol may resolve from at runtime
# (NDK sysroot stubs for bionic/system libs, termux-prefix libs for the
# payload's bundled ones). Version suffixes (@LIBC, @LIBC_P, …) are
# stripped on both sides before comparing.
set -euo pipefail

NDK="${ANDROID_NDK_HOME:-}"
if [ -z "$NDK" ]; then
    NDK="$(ls -d "$HOME"/Android/Sdk/ndk/* 2>/dev/null | sort -V | tail -1)"
fi
RO="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf"

ART="$1"; shift
[ "$#" -ge 1 ] || { echo "usage: $0 <artifact> <provider.so>..." >&2; exit 64; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Defined dynamic symbols of every provider, versions stripped.
: > "$WORK/defined"
for p in "$@"; do
    [ -f "$p" ] || { echo "check-elf-deps: provider missing: $p" >&2; exit 1; }
    "$RO" --dyn-syms "$p" \
        | awk '$7 != "UND" && $8 != "" {print $8}' \
        | sed 's/@.*$//' >> "$WORK/defined"
done
sort -u -o "$WORK/defined" "$WORK/defined"

# Undefined dynamic symbols of the artifact, versions stripped.
"$RO" --dyn-syms "$ART" \
    | awk '$7 == "UND" && $8 != "" {print $8}' \
    | sed 's/@.*$//' | sort -u > "$WORK/undef"

missing="$(comm -23 "$WORK/undef" "$WORK/defined")"
if [ -n "$missing" ]; then
    echo "check-elf-deps: $ART references symbols no provider exports:" >&2
    echo "$missing" | sed 's/^/  /' >&2
    exit 1
fi
echo "check-elf-deps: $(basename "$ART") ok ($(wc -l < "$WORK/undef") undefined, all provided)"
