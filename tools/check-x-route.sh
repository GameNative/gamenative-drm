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
#     (with GNDRM_LOG_VKSHIM=1) and the route line must name the decision. This
#     is the local proof that the entry point is intercepted and that a broken
#     route is visible in a session log with the diagnostics gate on.
#  2. structural — the built ICD names the X11 entry point, and the gamescope
#     override/Wayland route is the DEFAULT. The override protocol names are
#     therefore present in the artefact, but reachable ONLY through the
#     GN_VKSHIM_GUEST_SURFACE capability switch: the default is selected by
#     `!use_guest_surface()`, and the guest-surface (DRI3) route runs only when
#     the switch is set to 1. This harness enforces that confinement.
#
# A live DRI3 present (window content, idle-fence release) is exercised
# separately by tools/check-x-present.sh against a real X server + GPU; what
# only a device run can settle is whether *Xwayland* advertises DRI3 there.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

cargo build -p adrenotools_gamescope --bin icd_probe --quiet
cargo build -p mock-vkdriver -p mock-adrenotools -p mock-freedreno --quiet

LIB="$ROOT/target/debug/libadrenotools_gamescope.so"
PROBE="$ROOT/target/debug/icd_probe"
DIR="$ROOT/target/debug"
[ -f "$LIB" ] || { echo "FAIL: $LIB not built" >&2; exit 1; }
[ -x "$PROBE" ] || { echo "FAIL: $PROBE not built" >&2; exit 1; }

fail=0

# ---- 1. the entry point is intercepted; the DEFAULT route is the override ----
# Default (GN_VKSHIM_GUEST_SURFACE unset): the present-target override must be
# chosen and named, and the DRI3 route line must be absent. `--xlib-window 0`
# reports the decision; the route lines now follow GNDRM_LOG_VKSHIM, so the gate
# is turned on here (the override itself cannot engage on the host: no Wayland
# display, so it fails cleanly).
# The override route cannot engage on the host (no Wayland display), so the
# probe legitimately exits non-zero here; assert on the reported route instead.
out="$(env -u DISPLAY -u GN_VKSHIM_GUEST_SURFACE GNDRM_LOG_VKSHIM=1 \
    LD_LIBRARY_PATH="$DIR" \
    ADRENOTOOLS_HOOKS_PATH="$DIR" \
    ADRENOTOOLS_DRIVER_PATH="$DIR" \
    ADRENOTOOLS_DRIVER_NAME="libmock_vkdriver.so" \
    "$PROBE" --xlib-window 0 "$LIB" 2>&1)" || true
if ! grep -qF "PROBE xlib_surface" <<<"$out"; then
    echo "FAIL: the shim did not intercept vkCreateXlibSurfaceKHR" >&2
    echo "$out" >&2
    fail=1
fi
if ! grep -qF "override: XID 0x0" <<<"$out"; then
    echo "FAIL: the default route was not the override route" >&2
    echo "$out" >&2
    fail=1
fi
if grep -qF "x11 route:" <<<"$out"; then
    echo "FAIL: the DRI3 route ran without GN_VKSHIM_GUEST_SURFACE=1" >&2
    echo "$out" >&2
    fail=1
fi

# With GN_VKSHIM_GUEST_SURFACE=1 the guest-surface (DRI3) route must be
# reachable, and its decision reported with GNDRM_LOG_VKSHIM=1.
out_gs="$(env -u DISPLAY GNDRM_LOG_VKSHIM=1 \
    GN_VKSHIM_GUEST_SURFACE=1 \
    LD_LIBRARY_PATH="$DIR" \
    ADRENOTOOLS_HOOKS_PATH="$DIR" \
    ADRENOTOOLS_DRIVER_PATH="$DIR" \
    ADRENOTOOLS_DRIVER_NAME="libmock_vkdriver.so" \
    "$PROBE" --xlib-window 0 "$LIB" 2>&1)" || {
        echo "FAIL: icd_probe could not drive vkCreateXlibSurfaceKHR (GN_VKSHIM_GUEST_SURFACE=1)" >&2
        echo "$out_gs" >&2
        exit 1
    }
if ! grep -qF "PROBE xlib_surface" <<<"$out_gs"; then
    echo "FAIL: the shim did not intercept vkCreateXlibSurfaceKHR (=1)" >&2
    echo "$out_gs" >&2
    fail=1
fi
if ! grep -qF "x11 route: XID 0x0 unavailable (null XID)" <<<"$out_gs"; then
    echo "FAIL: GN_VKSHIM_GUEST_SURFACE=1 did not reach the DRI3 route, or the route decision was not reported under GNDRM_LOG_VKSHIM" >&2
    echo "$out_gs" >&2
    fail=1
fi

# ---- 2. the X11 entry point is named; override names are confined ----------
strings -a "$LIB" >"$ROOT/target/check-x-route.strings"
for marker in "vkCreateXlibSurfaceKHR" "x11 route:" "X11 present engages"; do
    if ! grep -qF -- "$marker" "$ROOT/target/check-x-route.strings"; then
        echo "FAIL: ICD shim is missing the X11 route marker '$marker'" >&2
        fail=1
    fi
done
# Code-path markers: asserted against the built artefact. The gamescope
# override/Wayland protocol names are present (it is the DEFAULT route), so the
# invariant is that they are reachable ONLY through the GN_VKSHIM_GUEST_SURFACE
# capability switch: the override route is selected by `!use_guest_surface()`,
# and both override consumers (the surface-create branch and the
# gamescope_swapchain_factory_v2 registry bind) are gated on `!use_guest_surface()`.
# The guest-surface (DRI3) route is reachable only when the switch is set to 1.
override_present=0
for marker in "override_window_content" "gamescope_swapchain"; do
    if grep -qF -- "$marker" "$ROOT/target/check-x-route.strings"; then
        override_present=1
    fi
done
if [ "$override_present" = 1 ]; then
    if ! grep -qF -- "GN_VKSHIM_GUEST_SURFACE" "$ROOT/target/check-x-route.strings"; then
        echo "FAIL: override protocol present without the GN_VKSHIM_GUEST_SURFACE switch" >&2
        fail=1
    fi
    if ! grep -q 'fn use_guest_surface' "$ROOT/icd_shim/src/proxy.rs"; then
        echo "FAIL: override protocol present but no use_guest_surface switch" >&2
        fail=1
    fi
    if ! grep -q 'fn xlib_override_surface' "$ROOT/icd_shim/src/proxy.rs"; then
        echo "FAIL: override protocol present but no xlib_override_surface route" >&2
        fail=1
    fi
    # The override route is the DEFAULT: selected by the negated switch.
    if ! grep -q 'if !use_guest_surface()' "$ROOT/icd_shim/src/proxy.rs"; then
        echo "FAIL: the override route is not the default (no 'if !use_guest_surface()')" >&2
        fail=1
    fi
    # The override registry bind must be gated the same way (it is the only
    # other consumer of the gamescope_swapchain_factory_v2 global).
    if ! grep -q 'gamescope_swapchain_factory_v2" && !use_guest_surface()' \
        "$ROOT/icd_shim/src/proxy.rs"; then
        echo "FAIL: gamescope_swapchain_factory_v2 bind is not gated on the default route" >&2
        fail=1
    fi
fi
# `vkCreateWin32SurfaceKHR` is asserted against the shim's *implementation*,
# not the whole artefact: the generated registry table
# (icd_shim/src/vk_dispatch_gen.rs) lists every Vulkan command name as data, so
# the bare string legitimately appears in the .so. What must not survive is a
# win32-surface override path in the code, which would be a reference in the
# implementation sources.
if grep -qF -- "vkCreateWin32SurfaceKHR" "$ROOT/icd_shim/src/proxy.rs" \
    "$ROOT/icd_shim/src/lib.rs" "$ROOT/icd_shim/src/x11.rs"; then
    echo "FAIL: ICD shim still carries a vkCreateWin32SurfaceKHR override path" >&2
    fail=1
fi

[ "$fail" = 0 ] || exit 1
echo "PASS: ICD shim hooks the guest's real surface entry point (vkCreateXlibSurfaceKHR) and reports its route under GNDRM_LOG_VKSHIM"
