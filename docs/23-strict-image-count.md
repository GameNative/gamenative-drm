# Strict image count

Some games fail unless their swapchain has exactly the image count they asked
for. DOOM (2016) is the known case on gamescope: it stops at startup with
`FATAL ERROR: getSwapchainImagesKHR failed with error (VK_SUCCESS)`. The call
succeeded; the game rejected the result.

The Vulkan spec lets a swapchain have more images than `minImageCount`. The
shim uses that: it allocates `minImageCount + 1`, and one more for mailbox. On
real Linux, Mesa's X11 WSI keeps a list of these games under
`vk_x11_strict_image_count`. Under the WSI proxy the swapchain is the shim's
own, so Mesa never sees it, and the shim carries the list in
`icd_shim/src/image_count.rs`.

## What changes for a listed game

- The swapchain has exactly `minImageCount` images, in every present mode. The
  shim's floor of 2 and `WRAPPER_MAX_IMAGE_COUNT` still apply.
- There is no spare image for mailbox or frame generation. Generation's own
  destination images are separate slots and stay.
- Hades also reports a minimum of 3, Mesa's `vk_x11_override_min_image_count`.
  Rainbow Six's value of 2 is what the shim already reports.
- The shim logs `strict image count on` and each swapchain's requested and
  created counts, whatever `GNDRM_LOG_VKSHIM` says.

Every other game keeps the count it had.

## Matching

Mesa's rules, with two differences:

- An `executable` is compared with `/proc/self/comm`, so only its first 15 bytes
  count.
- `application_name_match` and `engine_name_match` are plain substrings, because
  the list's patterns need nothing more.

## Keeping the list in step

The source is `src/util/00-mesa-defaults.conf`. To list the entries:

    awk '/<application|<engine/ {a=$0} /vk_x11_strict_image_count/ {print a}' 00-mesa-defaults.conf

A game that fails this way and is not in Mesa's list can be added to
`STRICT_GAMES` too.

## X11

The wrapper's X11 path has the same problem. Its `wsi_device_init` call passes no
driconf options, so Mesa's entry is not read and mailbox raises the count. DOOM
runs there on FIFO only. That is a separate fix.
