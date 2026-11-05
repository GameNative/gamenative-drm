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

- The guest sees exactly `minImageCount` images, in every present mode. The
  shim's floor of 2 and `WRAPPER_MAX_IMAGE_COUNT` still apply.
- Hades also reports a minimum of 3, Mesa's `vk_x11_override_min_image_count`.
  Rainbow Six's value of 2 is what the shim already reports.
- The shim logs `strict image count on` and each swapchain's requested and
  created counts, whatever `GNDRM_LOG_VKSHIM` says.

Every other game keeps the count it had.

## Hidden slots

DOOM always asks for 2 images. On gamescope's route a slot comes back only on
`wl_buffer.release`, and the compositor keeps the frame on screen until the next
one is latched. So one of the two images is always away, and DOOM can't start a
frame until the last one has finished and been latched. Measured on the Odin 3 in
the same area and equally hot, it holds exactly 30.0 fps, where X11 FIFO runs at
about 51 with about the same GPU time per frame. That this is the reason is
inferred.

A listed game on that route therefore gets hidden slots behind its images:

- The present's submit waits the guest's semaphores, then copies the frame to a
  hidden slot. The guest's image comes back when that submit's fence signals, and
  the hidden slot is what the compositor gets and later releases. Present ids and
  waits move with the frame to the hidden slot.
- There are `minImageCount + 2` of them: the frame on screen, the one committed
  behind it, and one for each frame the guest can have between its present and the
  commit. `hidden_slot_count` holds the rule.
- A frame that finds no hidden slot free goes out on the guest's own image, as it
  would without any. The guest then waits at its next acquire until the compositor
  releases it, so the compositor can slow the guest but not lose a frame or
  deadlock it.
- These frames go to the worker in present order in every mode, because a copy
  under way can't be replaced. gamescope replaces a committed frame it hasn't
  latched, so mailbox still behaves as mailbox. FIFO can queue as many frames as
  there are hidden slots.
- Frame generation reads the guest's image and has its own destinations, after the
  hidden slots. While it is on, frames take the direct path and no copy is made.
- The worker frees the guest's image, and the destroy joins the worker before it
  frees anything, so neither can overtake a copy.
- A present on a queue of another family than the swapchain's first present goes
  out directly: the guest's image belongs to that family and the copy's command
  buffers can only run on the first.

The shim logs the slot count once per swapchain, and every 600 frames
`strict hidden slots: N frames, M had no free slot`, whatever `GNDRM_LOG_VKSHIM`
says. A high `M` means the slots are too few or the compositor is holding them.

The X11 route has no hidden slots.

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
