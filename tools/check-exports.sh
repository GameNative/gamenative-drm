#!/usr/bin/env bash
# check-exports.sh — build-time export drift guard for the shipped libraries.
#
# For each library, the linker version script in tools/exports/ IS the export
# allowlist (it both pins the dynamic table at link time and documents the
# contract). This reads the built artifact's defined dynamic symbols and diffs
# them against that allowlist, failing on either:
#   * a MISSING export — a JNI symbol the app resolves with dlsym (dropping it
#     is a NoSuchMethodError at session start), an ICD entry point the Khronos
#     loader needs (a missing one means the shim cannot load at all), or an
#     interposer symbol (a lost one silently no-ops LD_PRELOAD); and
#   * a LEAKED export — any new internal symbol reaching .dynsym, i.e. the
#     reverse-engineering surface this hardening closes.
#
# Run by tools/build-android.sh (the two Rust cdylibs) and tools/build-shim.sh
# (the C shim), so the next person who adds a JNI/ICD/interposer symbol is told
# by the build instead of by a device crash.
#
# Usage:
#   tools/check-exports.sh                 # audit the three shipped libraries
#   tools/check-exports.sh <artifact> <map> [<artifact> <map> ...]
# Exit: 0 = every export set matches its allowlist; 1 = drift.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

NDK="${ANDROID_NDK_HOME:-}"
if [ -z "$NDK" ]; then
    NDK="$(ls -d "$HOME"/Android/Sdk/ndk/* 2>/dev/null | sort -V | tail -1)"
fi
RO=""
if [ -n "$NDK" ] && [ -x "$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf" ]; then
    RO="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf"
else
    RO="llvm-readelf"
fi

REL="$ROOT/target/aarch64-linux-android/release"
SHIM="$ROOT/target/shim/android/aarch64"
EXPORTS="$ROOT/tools/exports"

# Default pairings (artifact, allowlist).
if [ "$#" -ge 2 ]; then
    PAIRS=("$@")
else
    PAIRS=(
        "$REL/libgamenative_drm.so"        "$EXPORTS/libgamenative_drm.map"
        "$REL/libadrenotools_gamescope.so" "$EXPORTS/libadrenotools_gamescope.map"
        "$SHIM/libgndrm_shim.so"           "$EXPORTS/libgndrm_shim.map"
    )
fi

fail=0
i=0
while [ "$i" -lt "${#PAIRS[@]}" ]; do
    artifact="${PAIRS[$i]}"
    map="${PAIRS[$((i + 1))]}"
    i=$((i + 2))

    name="$(basename "$artifact")"
    [ -f "$artifact" ] || { echo "check-exports: note: $artifact not built — skipped"; continue; }
    [ -f "$map" ] || { echo "check-exports: allowlist missing: $map" >&2; fail=1; continue; }

    # Allowlist: the `global:` section of the version script, one symbol per line.
    expected="$(awk '/^[[:space:]]*global:/{g=1;next} /^[[:space:]]*local:/{g=0} g{print}' "$map" \
        | sed 's/;.*//; s/[[:space:]]//g' | grep -vE '^$' | sort -u)"

    # Actual: defined (non-UND) dynamic symbols, symbol versions stripped.
    # Field 1 is the symbol index ("123:") — requiring it skips the table header.
    actual="$("$RO" -W --dyn-syms "$artifact" \
        | awk '$1 ~ /^[0-9]+:$/ && $7 != "UND" && $8 != "" {print $8}' \
        | sed 's/@.*$//' | sort -u)"

    missing="$(comm -23 <(printf '%s\n' "$expected") <(printf '%s\n' "$actual"))"
    leaked="$(comm -13 <(printf '%s\n' "$expected") <(printf '%s\n' "$actual"))"

    if [ -n "$missing" ]; then
        echo "FAIL: $name is MISSING required export(s):" >&2
        printf '  - %s\n' $missing >&2
        fail=1
    fi
    if [ -n "$leaked" ]; then
        echo "FAIL: $name LEAKS non-allowlisted export(s):" >&2
        printf '  + %s\n' $leaked >&2
        fail=1
    fi
    if [ -z "$missing" ] && [ -z "$leaked" ]; then
        echo "check-exports: $name ok ($(printf '%s\n' "$expected" | grep -c .) exports, matches allowlist)"
    fi
done

[ "$fail" -eq 0 ] || exit 1
echo "PASS: every shipped library's dynamic export set matches its allowlist"
