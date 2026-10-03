#!/usr/bin/env bash
# check-x-route.sh — assert the ICD shim's present is hooked on the surface
# entry point the guest actually reaches, and that the route decision is
# reported without a diagnostic gate.
#
# The entry point is `vkCreateXlibSurfaceKHR`: wine's winex11.drv maps the app
# HWND to an X window and calls it (`dlls/winex11.drv/vulkan.c`,
# X11DRV_vulkan_surface_create -> x11drv_client_surface_create -> the host
# vkCreateXlibSurfaceKHR). `VK_KHR_win32_surface` is the name winevulkan
# exposes to Windows apps; winevulkan maps it onto the active display driver's
# host extension (`dlls/win32u/vulkan.c`, vulkan_init_once ->
# p_map_instance_extensions), so an ICD never receives it.
#
# Two layers:
#  1. behavioural — icd_probe drives vkCreateXlibSurfaceKHR through the shim
#     (mocked driver, no GNDRM_LOG_VKSHIM) and the always-on route line must
#     name the decision. This is the local proof that the entry point is
#     intercepted and that a broken route is visible in any session log.
#  2. structural — the built ICD names the X11 entry point and carries no
#     leftover compositor-override / gamescope_swapchain path.
#
# A live DRI3 present (window content, idle-fence release) is exercised
# separately by tools/check-x-present.sh against a real X server + GPU; what
# only a device run can settle is whether *Xwayland* advertises DRI3 there.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

cargo build -p adrenotools_icd --bin icd_probe --quiet
cargo build -p mock-vkdriver -p mock-adrenotools -p mock-freedreno --quiet

LIB="$ROOT/target/debug/libadrenotools_icd.so"
PROBE="$ROOT/target/debug/icd_probe"
DIR="$ROOT/target/debug"
[ -f "$LIB" ] || { echo "FAIL: $LIB not built" >&2; exit 1; }
[ -x "$PROBE" ] || { echo "FAIL: $PROBE not built" >&2; exit 1; }

fail=0

# ---- 1. the entry point is intercepted and the route line is gate-free ----
out="$(env -u DISPLAY -u GNDRM_LOG_VKSHIM \
    LD_LIBRARY_PATH="$DIR" \
    ADRENOTOOLS_HOOKS_PATH="$DIR" \
    ADRENOTOOLS_DRIVER_PATH="$DIR" \
    ADRENOTOOLS_DRIVER_NAME="libmock_vkdriver.so" \
    "$PROBE" --xlib-window 0 "$LIB" 2>&1)" || {
        echo "FAIL: icd_probe could not drive vkCreateXlibSurfaceKHR" >&2
        echo "$out" >&2
        exit 1
    }
if ! grep -qF "PROBE xlib_surface" <<<"$out"; then
    echo "FAIL: the shim did not intercept vkCreateXlibSurfaceKHR" >&2
    echo "$out" >&2
    fail=1
fi
if ! grep -qF "x11 route: XID 0x0 unavailable (null XID)" <<<"$out"; then
    echo "FAIL: the route decision was not reported without GNDRM_LOG_VKSHIM" >&2
    echo "$out" >&2
    fail=1
fi

# ---- 2. the X11 entry point is named, and no override path survives -------
strings -a "$LIB" >"$ROOT/target/check-x-route.strings"
for marker in "vkCreateXlibSurfaceKHR" "x11 route:" "X11 present engages"; do
    if ! grep -qF -- "$marker" "$ROOT/target/check-x-route.strings"; then
        echo "FAIL: ICD shim is missing the X11 route marker '$marker'" >&2
        fail=1
    fi
done
for marker in "override_window_content" "gamescope_swapchain" "vkCreateWin32SurfaceKHR"; do
    if grep -qF -- "$marker" "$ROOT/target/check-x-route.strings"; then
        echo "FAIL: ICD shim still carries '$marker' (the override path is not the route)" >&2
        fail=1
    fi
done

[ "$fail" = 0 ] || exit 1
echo "PASS: ICD shim hooks the guest's real surface entry point (vkCreateXlibSurfaceKHR) and reports its route ungated"
