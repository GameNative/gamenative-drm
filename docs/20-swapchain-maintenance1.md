# Swapchain maintenance: what the shim serves, and what is left

The ICD shim (`icd_shim`, `crate::maint1`) serves `VK_KHR_swapchain_maintenance1`
and `VK_EXT_swapchain_maintenance1` for its own swapchain, together with the
instance extensions they depend on (`VK_KHR_surface_maintenance1`,
`VK_EXT_surface_maintenance1`). This note says why, what each part does, and what
the follow-up owes.

## 1. Why

Under gamescope the guest's swapchain is the shim's own: the driver never sees
one, so anything a guest chains onto a present, a creation or a surface query
that the shim does not read is dropped on the floor. On the X11 route the Mesa
WSI inside the driver owns the swapchain and implements all of this itself.

Until this change the shim masked the EXT name and let the driver's KHR name
through (Mesa 25.2 and later list `VK_KHR_swapchain_maintenance1`,
`VK_KHR_present_id2` and `VK_KHR_present_wait2` on every WSI driver; the payload's
Turnip is Mesa 26.2). A guest that looked for the KHR name — vkd3d-proton 3.0.1
and later, DXVK after 2.7.1 — read `swapchainMaintenance1 = TRUE` from the
driver's feature answer, chained a present fence on every present, and waited on
it with no timeout when its fence ring wrapped. The present thread stopped and
`Present()` queued up behind it: DX12 titles ran a handful of frames and froze,
`VKD3D_DISABLE_EXTENSIONS=VK_KHR_swapchain_maintenance1` was the workaround. The
versions the app bundles (vkd3d-proton up to 3.0b, DXVK up to 2.7.1) only know
the EXT name and were never affected.

## 2. What is served

| Part | Where | Answer |
|---|---|---|
| Extension names | `gn_vkEnumerateDeviceExtensionProperties`, `gn_vkEnumerateInstanceExtensionProperties` | The driver's copies are hidden, the shim's advertised (`wsi_policy::SERVED`); masked at device and instance creation, so the driver never sees a name it has no swapchain for. |
| `swapchainMaintenance1` feature | `gn_vkGetPhysicalDeviceFeatures2`, device creation | Reported true; the struct is stripped from the driver's create chain like the present-id/wait ones. |
| Present fences (`VkSwapchainPresentFenceInfo`) | `gn_vkQueuePresentKHR` | The present's own submit (the one that consumes the guest's wait semaphores) also signals the slot's `fence_sem`; a second empty submit waits it and carries the guest's fence. The fence signals once the wait semaphores are free, which is its contract. The census prints `maint1=[fences=N ...]`. |
| Surface capabilities (`VkSurfacePresentScalingCapabilities`, `VkSurfacePresentModeCompatibility`) | `gn_vkGetPhysicalDeviceSurfaceCapabilities2KHR` | No scaling (the swapchain is presented at its own extent; gamescope scales the layer); the compatible list is the one mode the query named. |
| Creation (`VkSwapchainPresentModesCreateInfo`, `VkSwapchainPresentScalingCreateInfo`, the deferred-allocation flag) | `gn_vkCreateSwapchainKHR` | Accepted and logged under `GNDRM_LOG_VKSHIM`; every slot is still allocated at creation, which the flag allows. |
| `vkReleaseSwapchainImagesKHR` / `EXT` | `gn_vkReleaseSwapchainImagesKHR` | Acquired, unpresented slots go back to the free list. The acquire gate's booking for them stands (a released image is a frame the guest chose not to show). |
| `VK_KHR_present_id2` / `VK_KHR_present_wait2` | `wsi_policy::HIDDEN`, `maint1::features2` | Hidden and reported false: both guests fall back to the v1 pair the shim serves (`docs/15`). |

DXVK and vkd3d-proton gain what they have on X11: no `vkQueueWaitIdle` or device
idle when a swapchain is recreated or destroyed, and their newest releases run
under gamescope at all. Frame rate and latency are unchanged: those come from the
present wait and the cap gate, which were already served.

## 3. TODO: the follow-up

- **Per-present mode switching.** `VkSwapchainPresentModeInfo` lets a guest switch
  between the modes it listed at creation without recreating the swapchain
  (DXVK's vsync toggle, vkd3d-proton's `SyncInterval` 0 ↔ 1). Today the
  compatibility answer names one mode, so a conforming guest never asks; a request
  for another mode is logged once and the swapchain's mode kept
  (`MODE_SWITCH_NOTED`). Serving it means the requested mode enters
  `guest_present_mode` / `live_present_mode` and the cap gate's
  FIFO-versus-mailbox rule per present (`docs/15` §"present mode"). That is the pacing-sensitive part: it must be
  measured in a scene whose frame time is near the period, with the cap at the
  panel rate, before it ships.
- **`VK_KHR_present_id2` / `VK_KHR_present_wait2`.** Same semantics as v1 with the
  create-time flags and `VkPresentWait2InfoKHR`; worth serving once a guest is
  seen to prefer them in a way v1 cannot express (`VkSurfaceCapabilitiesPresentWait2KHR`).
- **Scaling capabilities.** `VK_PRESENT_SCALING_STRETCH_BIT_KHR` and the gravity
  bits could be honoured by the layer placement gamescope already does.

The naming policy for every WSI extension, served or hidden, is `docs/21-wsi-extension-policy.md`.
