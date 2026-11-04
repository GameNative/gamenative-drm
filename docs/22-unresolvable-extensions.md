# Unresolvable extensions: the guest is never promised a command the shim cannot resolve

The ICD shim (`icd_shim`) must never advertise to the guest a device extension,
or a core Vulkan version, whose commands it cannot resolve for that physical
device. It used to forward the driver's device extension list and apiVersion
as they were. A guest that enabled an extension and then got NULL from
`vkGetDeviceProcAddr` for one of its commands had nowhere to go: Mesa zink (in
Xwayland) installs an abort stub for every NULL entry point of an enabled
extension and dies on the first call, "ZINK: a Vulkan function was called
without being loaded".

## 1. The case

Honor GT (SD 8 Gen 3, MagicOS Android 16), Turnip at apiVersion 1.4.359. On
Xwayland's device every Vulkan 1.4 core command and every promoted alias of
one resolves to NULL through the shim: `vkCmdBindIndexBuffer2`/`KHR`,
`vkCmdPushDescriptorSet*`/`KHR`, `vkCmdSetRenderingAttachmentLocations`/`KHR`,
`vkCmdSetRenderingInputAttachmentIndices`/`KHR`, `vkCopyMemoryToImage`/`EXT`,
`vkCopyImageToMemory`/`EXT`, `vkCopyImageToImage`/`EXT`,
`vkTransitionImageLayout`/`EXT`, `vkGetImageSubresourceLayout2`/`KHR`/`EXT`,
`vkGetDeviceImageSubresourceLayout`/`KHR`, `vkGetRenderingAreaGranularity`/`KHR`,
`vkCmdSetLineStipple`/`KHR`/`EXT`, `vkMapMemory2`/`KHR`, `vkUnmapMemory2`/`KHR`,
`vkCmdBindDescriptorSets2`/`KHR`, `vkCmdPushConstants2`/`KHR`. The driver lists
1.4 and `VK_KHR_maintenance5`, `VK_KHR_maintenance6`, `VK_KHR_push_descriptor`,
`VK_KHR_dynamic_rendering_local_read`, `VK_EXT_host_image_copy`,
`VK_KHR_line_rasterization`, `VK_KHR_map_memory2`; zink enables them, calls one,
aborts. On a Retroid Pocket 6 (Android 13) the same names resolve. Why the
Honor's system loader answers NULL is a separate question; the shim's rule is
not to promise what it cannot hand out.

## 2. The probe (`icd_shim/src/resolve_probe.rs`)

The first time the shim answers for a physical device (its extension list, its
properties or features, or a device create on it) it:

1. creates a device of its own on the guest's real instance (the same instance
   and the same chain link the guest's creates carry), with every
   driver-listed device extension enabled except the WSI-scoped ones (those are
   `crate::wsi_policy`'s), one queue on family 0 and no features;
2. resolves every device-level command of every listed extension and of every
   core version up to the driver's through `proxy::device_gate`, the gate the
   guest's lookups use (the real instance's `vkGetDeviceProcAddr` on the real
   device). A command counts as resolved when the gate answers for it or for
   any registry alias of it, or when the shim has a handler of its own
   (`proxy::device_func`);
3. destroys the device.

The registry decides which commands belong to what: `tools/gen-vk-dispatch.py`
emits `VK_DEVICE_EXTENSION_COMMANDS` (per device extension, platform none or
android: the device-level commands of its own require blocks, none qualified
by another extension, and its feature structs) and `VK_CORE_DEVICE_COMMANDS`
(per core version 1.1 to 1.4: the device-level commands and feature structs its
base, compute and graphics blocks add). Physical-device-level commands are not
probed: `vkGetDeviceProcAddr` never answers them.

The result is cached per GPU (vendor, device, driver version, pipeline cache
UUID) and the apiVersion the guest's instance asked for, so a process with
several instances probes each GPU once.

## 3. What the guest sees

| | Extension with an unresolved command | Core version with an unresolved command |
|---|---|---|
| `vkEnumerateDeviceExtensionProperties` | dropped | |
| `vkCreateDevice` | masked if a guest names it anyway | |
| `vkGetPhysicalDeviceFeatures2` | its feature structs reported false, unless a still-listed extension or a still-reported core version names the same struct | the structs that version adds (`VkPhysicalDeviceVulkan14Features`, ...) reported false, same exception |
| `vkGetPhysicalDeviceProperties[2]` | | `apiVersion` lowered to the highest version whose commands all resolve (1.0 when even 1.1's do not); variant, major and patch kept |
| `vkGetDeviceProcAddr` | NULL, as before | NULL, as before |

The guest's lookups now try every registry spelling of a name (one level,
`resolve_probe::other_spellings`), not only `alias_of`'s, so a core command the
driver exports only under its second extension spelling (`vkCmdSetLineStipple`
has a KHR and an EXT one) resolves on the guest path as it does in the probe.

When everything resolves nothing changes: the guest sees exactly the driver's
list, apiVersion and features (`nothing_changes_when_everything_resolves`). A
probe that cannot run (no `vkCreateDevice`, or the probe device's create fails)
hides nothing.

One line per GPU, ungated:

```
proxy pd: hiding 8 extensions whose commands do not resolve: [VK_KHR_maintenance5, ...]; core version 1.3 (driver 1.4)
proxy pd: all 187 listed device extensions and core version 1.4 resolve
proxy pd: resolve probe did not run (probe vkCreateDevice with 187 extensions returned -7); every listed extension is kept
```

and, once per process, when a guest names a hidden extension at device
creation:

```
proxy device: extensions whose commands do not resolve requested and masked: [...]
```

## 4. Escape hatch

`GN_VKSHIM_NO_RESOLVE_PROBE=1` skips the probe: no probe device, the driver's
list and apiVersion pass through as before this rule.

## 5. Limits

- The probe enables every listed extension at once. A driver that refuses that
  combination gets no probe (nothing hidden), not a per-extension retry.
- Only the device-level commands of an extension's own require blocks are
  judged; a command an extension adds only together with another extension
  (`vkCmdPushDescriptorSetWithTemplateKHR` with descriptor update templates) is
  not.
- The probe device has no features enabled. A driver whose lookup depends on an
  enabled feature, not on the enabled extension, is judged on the featureless
  device.
