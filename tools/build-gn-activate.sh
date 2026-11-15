#!/usr/bin/env bash
# build-gn-activate.sh — cross-build tools/gn-activate/gn-activate.c as a Win32
# PE, which is what makes it able to activate the game's window at all.
#
# The activation has to be a Win32 call (SetForegroundWindow) because
# WM_ACTIVATE/WM_SETFOCUS can only be raised from inside the wine prefix. The
# X-level substitute was measured and fails: XSetInputFocus on the game's
# override-redirect child sticks and changes nothing, because wine receives the
# focus change on a window it does not manage and never activates it. gamescope's
# old A48 patch existed for exactly this reason and reached winhandler.exe over
# UDP; this program replaces the whole hop with "wait for the window, activate
# it", so no compositor patch and no IPC are involved.
#
# Built both bitnesses on purpose: the prefix is win64 with WoW64, so either
# runs, and trying 32-bit first costs nothing if 64-bit is refused. The two are
# separate executables rather than one, because a PE has a fixed machine type.
#
# Two on-device constraints found while running it by hand, both silent in
# confusing ways:
#
#  - the .exe must live inside the wine prefix (e.g. drive_c/). Run from a unix
#    path outside it and the translator faults before main with signal 11, which
#    looks exactly like a broken executable. A minimal hello-world PE built with
#    the same toolchain fails the same way, so it is the location, not the code.
#  - the process must join the game's desktop. `explorer /desktop=shell` puts the
#    game on a desktop named shell; a fresh wine process starts on the default
#    desktop, where EnumWindows returns nothing at all. gn-activate joins it with
#    --desktop (default "shell"). Launching it from the explorer command line,
#    the way winhandler.exe is launched, gets the right desktop for free.
#
# Also note the HWND is not the X window id (measured: HWND 0x0001008a for the X
# window 0x02400001), which is why the target is matched by process image.
#
# Usage: ./tools/build-gn-activate.sh
# Output: target/win32/gn-activate.exe   (i686, run in a WoW64 prefix)
#         target/win32/gn-activate64.exe (x86_64)

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

GCC32="${MINGW32_GCC:-i686-w64-mingw32-gcc}"
GCC64="${MINGW64_GCC:-x86_64-w64-mingw32-gcc}"

OUTDIR="$ROOT/target/win32"
mkdir -p "$OUTDIR"

# -municode is not used: the program is plain main() with the ANSI APIs, since
# wine's window classes and titles here are ASCII and the calls stay readable.
# -O2 -Wall -Wextra, and static libgcc so the exe has no mingw runtime deps
# beyond msvcrt/kernel32/user32, which every wine prefix has.
CFLAGS=(-O2 -Wall -Wextra -static-libgcc)
LIBS=(-luser32 -lkernel32)

if command -v "$GCC32" >/dev/null 2>&1; then
    "$GCC32" "${CFLAGS[@]}" -o "$OUTDIR/gn-activate.exe" \
        tools/gn-activate/gn-activate.c "${LIBS[@]}"
    echo "built:  $OUTDIR/gn-activate.exe (i686)"
else
    echo "skip:   $GCC32 not found" >&2
fi

if command -v "$GCC64" >/dev/null 2>&1; then
    "$GCC64" "${CFLAGS[@]}" -o "$OUTDIR/gn-activate64.exe" \
        tools/gn-activate/gn-activate.c "${LIBS[@]}"
    echo "built:  $OUTDIR/gn-activate64.exe (x86_64)"
else
    echo "skip:   $GCC64 not found" >&2
fi

file "$OUTDIR"/gn-activate*.exe
