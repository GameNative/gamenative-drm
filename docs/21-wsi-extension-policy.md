# WSI extension policy: which surface, swapchain and present extensions the guest sees

Under the WSI proxy the guest's surfaces and swapchains are the ICD shim's own
(`icd_shim`): the driver never sees one. An extension whose subject is a surface,
a swapchain or a present is therefore a question about the shim, and the
driver's answer says nothing about it. Until this policy the shim passed the
driver's instance and device lists through and hid WSI names by hand (two
predicates in `proxy.rs`), so every WSI extension a newer driver listed and the
shim did not implement reached the guest: present timing, FIFO latest ready,
shared presentable images, incremental present, mutable-format swapchains,
display timing and control, low latency 2, ... Each was a feature a guest could
build on and the shim would silently drop.

## 1. The WSI scope comes from the registry

`tools/gen-vk-dispatch.py` emits `VK_WSI_EXTENSIONS` into
`icd_shim/src/vk_dispatch_gen.rs` (run it after updating `icd_shim/registry/vk.xml`;
`--check` and `tools/check-icd-commands.sh` fail on a stale table). An extension
is WSI-scoped when:

- its `depends` names `VK_KHR_surface`, `VK_KHR_swapchain`, `VK_KHR_display` or
  `VK_KHR_get_surface_capabilities2`, directly or through another WSI-scoped
  extension;
- one of its own structs (outside `depends`-qualified require blocks) extends
  only `VkPresentInfoKHR`, `VkSwapchainCreateInfoKHR`, `VkSurfaceCapabilities2KHR`,
  `VkPhysicalDeviceSurfaceInfo2KHR` or `VkAcquireNextImageInfoKHR` (so
  `VK_EXT_image_compression_control` and `VK_EXT_frame_boundary`, which touch a
  swapchain or a present on the side, are not);
- or a name token starts with `surface`, `swapchain`, `present` or `display`
  (`_present` matches `shared_presentable_image`, not `representative_fragment_test`).

`supported="disabled"` extensions and platforms other than android, xlib, xcb
and wayland are left out: no driver on this device lists them. Each row carries
the name, instance or device, the feature structs (`sType`, leading `VkBool32`
count) and the commands only WSI-scoped extensions require (a command core or
`VK_KHR_device_group` also requires, such as `vkAcquireNextImage2KHR`, is not the
WSI extension's to refuse).

## 2. One table decides (`icd_shim/src/wsi_policy.rs`)

| Class | Guest's list | Creation | Features2 | Commands |
|---|---|---|---|---|
| `SERVED`, `Served::Driver` (`VK_KHR_surface`, `VK_KHR_get_surface_capabilities2`, `VK_EXT_swapchain_colorspace`, `VK_KHR_swapchain`, `VK_EXT_hdr_metadata`) | the driver's copy | forwarded | the driver's | the shim intercepts the ones taking its handles |
| `SERVED`, `Served::Shim` (wayland/xlib/xcb surface, KHR/EXT surface maintenance 1, present id, present wait, KHR/EXT swapchain maintenance 1) | the driver's copy dropped, the shim's appended | masked | the shim's answer (`present::features2`, `maint1::features2`); struct stripped from the driver's chain | the shim's |
| `HIDDEN` (everything else, each with a reason) | dropped | masked | reported false (`zero_hidden_features`); struct stripped from the driver's chain | refused at `vkGetDeviceProcAddr` and in `serve_unlisted`, unless the shim has a handler of its own (the WSI dispatch-coverage set) |

`VK_EXT_hdr_metadata` keeps its special case: the shim adds it when the driver
does not list it and masks it at creation then. `VK_EXT_swapchain_colorspace` is
the driver's copy only; the shim never adds its own (see the enumeration comment
in `proxy.rs`).

Each process logs the policy once, at the first instance and the first device:

```
proxy instance: WSI extensions hidden from the guest: [VK_KHR_android_surface, ...] requested and masked: [...]
proxy device: WSI extensions hidden from the guest: [VK_KHR_present_id2, ...] requested and masked: [...]
```

The first list is what the driver listed and the guest no longer sees; the second
is what the guest asked for anyway and the shim dropped (a per-name
`proxy device: masking ...` line follows for each).

## 3. Classifying a new extension

After a registry update, the unit test `wsi_policy::every_wsi_extension_is_classified`
names every WSI-scoped extension in neither table. For each:

1. If the shim implements it for its own swapchain, add it to `SERVED`
   (`Served::Shim { spec }` for a name the shim advertises itself, with its feature
   answer in the module that serves it; `Served::Driver` only when the driver's
   copy is correct as is because the shim already answers every query and
   command it adds).
2. Otherwise add it to `HIDDEN` with a one-line reason. Hiding is the safe
   default: the guest then behaves as on a driver without the extension.

The other tests pin that no name is in both tables, that the names appended to
the guest's lists are the served `Shim` ones in their historical order, and that a
hidden feature struct never shares an `sType` with a served one.

Non-WSI extensions are filtered by a second rule, not this table: an extension
whose commands do not resolve on the device is hidden too
(`docs/22-unresolvable-extensions.md`).
