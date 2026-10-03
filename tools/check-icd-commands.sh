#!/usr/bin/env bash
# check-icd-commands.sh — guard the ICD shim's dispatch coverage.
#
# The shim no longer decides how to serve a name from hand-maintained tables.
# Every entry point routes a name it does not statically intercept through
# `proxy::serve_unlisted`, which reads the command's level from the table
# generated out of vk.xml (`icd_shim/src/vk_dispatch_gen.rs`). A device-level
# command (argument 0 is VkDevice/VkQueue/VkCommandBuffer, a handle the shim
# wraps) is served by a wrapper-swapping thunk on every entry point; every other
# level is the driver's own handle and passes through. That is what closes the
# `vkCreateSamplerYcbcrConversion` crash class by construction: a device-level
# command can no longer be handed to the client as the driver's raw entry point.
#
# This check asserts, host-side:
#   1. the generated table is in sync with the vendored registry;
#   2. every required intercepted command is present in the built artefact;
#   3. every required command is classified by the generated registry;
#   4. the hand-written alias table is gone and the alias lookup is generated;
#   5. all three loader/guest entry points route through the shared policy;
#   6. the command the crash class is named after is device-level.
#
# Usage:
#   tools/check-icd-commands.sh [path-to-libadrenotools_gamescope.so]
#
# Default artefact: target/aarch64-linux-android/release/libadrenotools_gamescope.so.
# Exit: 0 = all invariants hold; 1 = a violation.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SO="${1:-$ROOT/target/aarch64-linux-android/release/libadrenotools_gamescope.so}"
GEN="$ROOT/icd_shim/src/vk_dispatch_gen.rs"
PROXY="$ROOT/icd_shim/src/proxy.rs"
LIB="$ROOT/icd_shim/src/lib.rs"

fail() { echo "FAIL: $*" >&2; exit 1; }

# Every command the shim has been known to intercept/proxy and must keep
# intercepting. Superset of the last known-good build's set; the additions in
# `required` below are the device commands whose loss produced on-device
# crashes. Keep this list append-only for anything a client is known to resolve
# and call with a shim handle.
REQUIRED=(
    # --- last known-good shim (build 202610021950/1959) ---
    vkAcquireNextImage2KHR
    vkAcquireNextImageKHR
    vkAllocateCommandBuffers
    vkAllocateMemory
    vkBindImageMemory
    vkCmdExecuteCommands
    vkCmdSetLineStippleEXT
    vkCreateDevice
    vkCreateFence
    vkCreateGraphicsPipelines
    vkCreateImage
    vkCreateInstance
    vkCreateSemaphore
    vkCreateSwapchainKHR
    vkCreateWaylandSurfaceKHR
    vkCreateXlibSurfaceKHR
    vkDestroyDevice
    vkDestroyFence
    vkDestroyImage
    vkDestroyInstance
    vkDestroySemaphore
    vkDestroySurfaceKHR
    vkDestroySwapchainKHR
    vkDeviceWaitIdle
    vkEnumerateDeviceExtensionProperties
    vkEnumerateInstanceExtensionProperties
    vkEnumeratePhysicalDevices
    vkFreeCommandBuffers
    vkFreeMemory
    vkGetDeviceProcAddr
    vkGetDeviceQueue
    vkGetDeviceQueue2
    vkGetImageMemoryRequirements
    vkGetImageSubresourceLayout
    vkGetInstanceProcAddr
    vkGetPhysicalDeviceSurfaceCapabilitiesKHR
    vkGetPhysicalDeviceSurfaceFormatsKHR
    vkGetPhysicalDeviceSurfacePresentModesKHR
    vkGetPhysicalDeviceSurfaceSupportKHR
    vkGetPhysicalDeviceWaylandPresentationSupportKHR
    vkGetPhysicalDeviceXlibPresentationSupportKHR
    vkGetRenderingAreaGranularityKHR
    vkGetSwapchainImagesKHR
    vkQueuePresentKHR
    vkQueueSubmit
    vkQueueSubmit2
    vkQueueWaitIdle
    vkResetFences
    vkWaitForFences
    vkWaitSemaphores
    vkWaitSemaphoresKHR
    # --- must stay intercepted (device-init / instrumentation regressions) ---
    vkCreateSamplerYcbcrConversion
    vkCreateSamplerYcbcrConversionKHR
    vkCmdSetViewport
    vkCmdSetViewportWithCount
    vkCmdSetViewportWithCountEXT
    vkCmdSetScissor
    vkCmdSetScissorWithCount
    vkCmdSetScissorWithCountEXT
    vkCreateRenderPass
)

# The command whose loss crashed gamescope at Vulkan device init; it must be
# device-level in the generated registry, so the policy swaps its handle.
CRASH_CLASS="vkCreateSamplerYcbcrConversion"

# --- 1. the generated dispatch is in sync with the vendored registry ---------
[ -f "$GEN" ] || fail "generated dispatch missing: $GEN"
python3 "$ROOT/tools/gen-vk-dispatch.py" --check || fail "generated dispatch is stale (see above)"

# --- 2. required commands present in the built artefact ----------------------
if [ -f "$SO" ]; then
    # Every vk* token in the artefact, deduped. `strings` pools adjacent `&str`
    # literals with no separator, so two names can land in one blob (observed:
    # `…LineStippleEXTvkGetRenderingAreaGranularityKHR`); split before every
    # embedded `vk<upper>` first — "vk" never occurs inside a vk name — so each
    # token is still matched exactly.
    present="$(strings -a "$SO" | sed 's/vk\([A-Z]\)/\nvk\1/g' | grep -oE 'vk[A-Z][A-Za-z0-9]+' | sort -u)"
    missing_so=()
    for name in "${REQUIRED[@]}"; do
        grep -qxF "$name" <<<"$present" || missing_so+=("$name")
    done
    if [ "${#missing_so[@]}" -ne 0 ]; then
        echo "FAIL: ${#missing_so[@]} required command(s) missing from the built ICD:" >&2
        printf '  - %s\n' "${missing_so[@]}" >&2
        exit 1
    fi
    echo "ICD command guard: $SO"
    echo "artefact: required=${#REQUIRED[@]} present=$((${#REQUIRED[@]} - ${#missing_so[@]}))"
else
    echo "note: artefact not found ($SO) — skipping the built-binary presence check"
    echo "      build it with ./tools/build-android.sh to include it"
fi

# --- 3. every required command is classified by the generated registry -------
missing_gen=()
for name in "${REQUIRED[@]}"; do
    grep -qF "(\"$name\"," "$GEN" || missing_gen+=("$name")
done
[ "${#missing_gen[@]}" -eq 0 ] || {
    echo "FAIL: ${#missing_gen[@]} required command(s) absent from the generated registry:" >&2
    printf '  - %s\n' "${missing_gen[@]}" >&2
    exit 1
}
echo "registry: ${#REQUIRED[@]}/${#REQUIRED[@]} required commands classified"

# --- 4. the hand-written alias table is gone, the alias lookup is generated --
! grep -q 'fn alias_of' "$PROXY" || fail "hand-written alias_of still present in proxy.rs"
grep -q 'pub fn alias_of' "$GEN" || fail "generated alias_of missing from vk_dispatch_gen.rs"

# --- 5. all three entry points route through the shared policy ---------------
[ "$(grep -c 'proxy::serve_unlisted(' "$LIB")" -ge 2 ] \
    || fail "the two loader-facing ICD entry points must use proxy::serve_unlisted"
grep -q 'serve_unlisted(name, target, DispatchCtx::Device)' "$PROXY" \
    || fail "gn_vkGetDeviceProcAddr must use the shared unlisted-name policy"
grep -q 'pub fn serve_unlisted' "$PROXY" || fail "proxy::serve_unlisted is missing"

# --- 6. the crash-class command is device-level ------------------------------
grep -qF "(\"$CRASH_CLASS\", Level::Device," "$GEN" \
    || fail "$CRASH_CLASS is not device-level in the generated registry"

# --- 7. the loader entry points gate every handler on the proxy --------------
# A shim handler may only be served while the WSI proxy is active — the only
# time the shim owns wrapper handles. Without the gate the fallback path is
# handed the shim's own proxy-owned WSI commands, whose surfaces the proxy alone
# creates, and swapchain creation fails with VK_ERROR_INITIALIZATION_FAILED.
grep -q 'pub fn entrypoint_handler' "$PROXY" \
    || fail "proxy::entrypoint_handler (the proxy gate) is missing"
awk '/pub fn entrypoint_handler/{f=1} f&&/return None;/{ok=1} f&&/^}/{f=0} END{exit ok?0:1}' "$PROXY" \
    || fail "entrypoint_handler must return None when the proxy is inactive"
[ "$(grep -c 'proxy::entrypoint_handler(' "$LIB")" -ge 2 ] \
    || fail "both ICD entry points must gate their handler lookup on the proxy"

# --- 8. nested wrapped-handle translation is registry-driven -----------------
# Command-buffer wrappers nested in submit/execute arrays are translated by the
# generated plan (`VK_NESTED_HANDLES`) — not by a hand-maintained list. Assert
# the plan covers the three paths round-W named and that the shim intercepts
# every command it has a plan for.
grep -q 'pub static VK_NESTED_HANDLES' "$GEN" || fail "VK_NESTED_HANDLES missing from the generated table"
grep -q 'pub fn nested_handles' "$GEN" || fail "nested_handles() lookup missing from the generated table"
for member in 'VkSubmitInfo, pCommandBuffers' \
              'VkSubmitInfo2, pCommandBufferInfos' \
              'VkCommandBufferSubmitInfo, commandBuffer'; do
    grep -qF "$member" "$GEN" || fail "nested-handle plan does not cover $member"
done
grep -qF '("vkCmdExecuteCommands"' "$GEN" || fail "vkCmdExecuteCommands has no nested plan"
grep -q 'fn translate_nested' "$PROXY" || fail "translate_nested applier missing from proxy.rs"
grep -qF 'c"vkCmdExecuteCommands", HandlerScope::Always' "$PROXY" \
    || fail "vkCmdExecuteCommands is not intercepted (the round-W storm path)"
echo "nested handles: plan covers the command-buffer submit/execute paths"

echo "PASS: generated dispatch covers every required command and the policy is single"
