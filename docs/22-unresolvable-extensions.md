# The resolve probe: the guest is never promised a core version whose commands do not resolve

> The file name is kept for its number. The first version of this rule
> (gamenative-drm PR #54) also hid device extensions with an unresolvable
> command; that part was removed (GameNative/gamenative-drm#55): it hid nothing
> on any device it ran on (Retroid Pocket 6, Galaxy S24 Ultra, Galaxy Z Fold 7,
> Honor GT all reported 0 hidden), and the Honor GT failure it was written for
> turned out to be a core-version one (section 1). What is left is the
> core-version clamp.

The ICD shim (`icd_shim`) must never report to the guest a core Vulkan version
whose device commands it cannot resolve for that physical device. It used to
forward the driver's apiVersion as it was. A guest that trusts a version and
then gets NULL from `vkGetDeviceProcAddr` for one of its core commands has
nowhere to go: Mesa zink (in Xwayland) installs an abort stub for every NULL
entry point and dies on the first call, "ZINK: a Vulkan function was called
without being loaded".

## 1. The case

Honor GT (SD 8 Gen 3, MagicOS Android 16). Xwayland creates its instance at
1.4.315, Turnip reports 1.4.359, yet every Vulkan 1.4 core device command
(`vkCmdBindDescriptorSets2`, `vkCopyMemoryToImage`, `vkCmdSetLineStipple`, ...)
resolves NULL through the shim on Xwayland's device. The OEM system loader
hands Turnip an instance below 1.4, and Mesa's runtime
(`vk_device_get_proc_addr` -> `vk_device_dispatch_table_get_if_supported`)
answers a core device command only when the instance's
`app_info.api_version` covers its version (a promoted alias such as
`vkCopyMemoryToImageEXT` only when that extension is enabled on the device).
zink, trusting 1.4, enables no promoted extension, so it gets neither spelling
and aborts at its first host-image-copy call. On a Retroid Pocket 6
(Android 13) the same names resolve.

## 2. The probe (`icd_shim/src/resolve_probe.rs`)

The first time the shim answers for a physical device (its properties or
features) it:

1. skips everything when the probe cannot clamp anything: the apiVersion the
   guest's instance handed the system loader (the `api=` of
   `proxy instance create`) is 1.0 (Wine helpers, apps with no app info), or
   the driver reports 1.0;
2. otherwise creates a device of its own on the guest's real instance (the
   same instance and the same chain link the guest's creates carry), one queue
   on family 0, no extensions, no features. No extension because Mesa gates a
   core command on the instance's apiVersion alone, so a bare device answers
   the exact core names as the guest's would, and it is the cheapest create;
3. resolves the device-level commands of each core version, 1.1 upwards up to
   min(driver apiVersion, the instance's forwarded apiVersion), by exact core
   name through `proxy::device_gate`, the gate the guest's lookups use (the
   real instance's `vkGetDeviceProcAddr` on the real device), or the shim's own
   handler (`proxy::device_func`). Versions above the instance's are not
   judged: Mesa answers NULL for them on every device by design and the guest
   asked for no more;
4. destroys the device.

The registry decides which commands belong to which version:
`tools/gen-vk-dispatch.py` emits `VK_CORE_DEVICE_COMMANDS` (per core version
1.1 to 1.4: the device-level commands its base, compute and graphics blocks
add, and its `VkPhysicalDeviceVulkan1xFeatures` struct).

The result is cached per GPU (vendor, device, driver version, pipeline cache
UUID) and the apiVersion the guest's instance asked for, so a process with
several instances probes each GPU once. There is no per-frame code: after the
probe the hooks read the cached outcome.

## 3. What the guest sees

The first version with a command that does not resolve clamps:

| | |
|---|---|
| `vkGetPhysicalDeviceProperties[2]` | `apiVersion` lowered to the version below it (1.0 when even 1.1's do not resolve); variant, major and patch kept |
| `vkGetPhysicalDeviceFeatures2` | `VkPhysicalDeviceVulkan1xFeatures` of every version above the clamp reported false; promoted extensions' structs stay, as do the extensions |
| `vkEnumerateDeviceExtensionProperties`, `vkCreateDevice` | unchanged |

On the Honor GT the guest sees 1.3.359, `VkPhysicalDeviceVulkan14Features`
false, and zink creates a 1.3 device that enables `VK_EXT_host_image_copy` and
the other promoted extensions explicitly, whose commands then resolve.

When everything resolves nothing changes (`nothing_changes_when_everything_resolves`).
A probe that cannot run (no `vkCreateDevice`, or the probe device's create
fails) changes nothing.

One line per GPU and process, ungated, with the probe's cost (create, resolve,
destroy):

```
proxy pd: resolve probe: core version 1.4 (N.N ms)
proxy pd: resolve probe: core version 1.3 (driver 1.4; vkCmdBindDescriptorSets2 unresolved) (N.N ms)
proxy pd: resolve probe: skipped (instance 1.0)
proxy pd: resolve probe: did not run (probe vkCreateDevice returned -3); core version 1.4 kept (N.N ms)
```

## 4. Escape hatch

`GN_VKSHIM_NO_RESOLVE_PROBE=1` skips the probe: no probe device, the driver's
apiVersion and features pass through as before this rule.

## 5. Limits

- Only core versions are judged. An extension the driver lists whose own
  commands do not resolve is still listed (the removed per-extension rule never
  found one).
- The probe device has no extensions or features enabled. A driver whose core
  lookup depends on either is judged on the bare device.

## 6. History

- PR #54: hid extensions with any unresolvable command and clamped core
  versions, a core command counting as resolved through any alias. On the
  Honor the probe device enabled every listed extension, so every 1.4 alias
  resolved there and 1.4 "passed"; zink's device (no promoted extension
  enabled) got NULL for both spellings.
- PR #55: core versions by exact name, judged only up to the instance's
  forwarded apiVersion (a first cut without that bound clamped the RP6's
  gamescope, instance 1.3, to 1.3 and Wine helpers to 1.0); extension hiding
  removed; probe skipped for 1.0 instances; elapsed time logged.
