#!/usr/bin/env bash
# check-icd-commands.sh — guard the ICD shim's intercepted/proxied command set.
#
# The shim decides what it intercepts from `G_DEV_FUNCS`/`G_INST_FUNCS` (both
# static slices — no fixed capacity) and, for device-level names not in those
# tables, from the wrapper-swapping thunk in `gn_vkGetDeviceProcAddr`. A
# device-level command that is NOT intercepted is handed to the client as the
# driver's raw entry point, which faults the moment the client passes it a shim
# device wrapper. That is exactly how losing `vkCreateSamplerYcbcrConversion`
# crashed gamescope at Vulkan device init (Turnip `vkCreateSamplerYcbcrConversion`
# SIGSEGV, fault addr 0x0, from `CVulkanDevice::createLayouts`).
#
# This check extracts every `vk*` name the built shim references and fails
# loudly if any known-required command is absent, so that class of regression
# is caught host-side instead of on device. It intentionally over-approximates:
# a name counts as present if it appears anywhere in the artefact (table entry,
# `driver_sym(c"…")`, or a `resolve!` literal).
#
# Usage:
#   tools/check-icd-commands.sh [path-to-libadrenotools_gamescope.so]
#
# Default artefact: target/aarch64-linux-android/release/libadrenotools_gamescope.so.
# Exit: 0 = every required command present; 1 = one or more missing.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SO="${1:-$ROOT/target/aarch64-linux-android/release/libadrenotools_gamescope.so}"

if [ ! -f "$SO" ]; then
    echo "FAIL: ICD not found: $SO" >&2
    echo "      build it first: ./tools/build-android.sh   (or pass a path)" >&2
    exit 1
fi

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

# Every vk* token in the artefact, deduped.
present="$(strings -a "$SO" | grep -oE 'vk[A-Z][A-Za-z0-9]+' | sort -u)"

missing=()
for name in "${REQUIRED[@]}"; do
    if ! grep -qxF "$name" <<<"$present"; then
        missing+=("$name")
    fi
done

total="${#REQUIRED[@]}"
found=$((total - ${#missing[@]}))
echo "ICD command guard: $SO"
echo "required=$total present=$found"

if [ "${#missing[@]}" -ne 0 ]; then
    echo "FAIL: ${#missing[@]} required intercepted command(s) missing from the built ICD:" >&2
    for name in "${missing[@]}"; do
        echo "  - $name" >&2
    done
    echo "A device-level command that is not intercepted is handed to the client raw," >&2
    echo "which crashes the client when it passes a shim device wrapper (see header)." >&2
    exit 1
fi

echo "PASS: all required intercepted commands present"
