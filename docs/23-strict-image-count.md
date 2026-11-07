# Strict image count

Some games fail unless their swapchain has exactly the image count they asked
for. DOOM (2016) is the known case on gamescope: it stops at startup with
`FATAL ERROR: getSwapchainImagesKHR failed with error (VK_SUCCESS)`. The call
succeeded; the game rejected the result.

The Vulkan spec lets a swapchain have more images than `minImageCount`, and
creation raises the count for mailbox and for frame generation. On real Linux,
Mesa's X11 WSI keeps a list of these games under `vk_x11_strict_image_count`.
Under the WSI proxy the swapchain is the shim's own, so Mesa never sees it, and
the shim carries the list in `icd_shim/src/image_count.rs`.

Creation otherwise follows Mesa's convention (`proxy::swapchain_slots`): the
surface capabilities advertise `SC_DEFAULT_IMAGES` (3, Mesa's
`WSI_WL_DEFAULT_NUM_IMAGES`), the ask is honoured exactly, and the only bump is
mailbox to `SC_MAILBOX_IMAGES` (4, Mesa's `WSI_WL_BUMPED_NUM_IMAGES`). A guest
that sizes its per-image state from `vkGetSwapchainImagesKHR` therefore gets its
own number back, which is what the listed games above reject.

## What changes for a listed game

- The guest sees exactly `minImageCount` images, in every present mode and with
  no bump of any kind — the mailbox floor and the frame-generation floor
  included. The shim's floor of 2 and `WRAPPER_MAX_IMAGE_COUNT` still apply.
- With frame generation on, a listed game therefore has no source image to
  spare, and its hidden slots are idle while generation runs (`spare_in_use` is
  off with generation on, by design): the session paces at whatever two source
  images allow. Generation is left enabled rather than refused, because refusing
  it would change the mode the guest was promised.
- No listed game changes the advertised minimum any more: Mesa's
  `vk_x11_override_min_image_count` (Hades, 3) has nothing to override, because
  every surface already advertises 3.
- The shim logs `strict image count on` and each swapchain's requested and
  created counts, whatever `GNDRM_LOG_VKSHIM` says.

Every other guest gets what it asked for, which for a conforming one is the
advertised 3 and for DXVK (which asks `minImageCount + 1`) 4.

## Hidden slots

DOOM always asks for 2 images. On gamescope's route a slot comes back only on
`wl_buffer.release`, and the compositor keeps the frame on screen until the next
one is latched. So one of the two images is always away, and DOOM can't start a
frame until the last one has finished and been latched. Measured on the Odin 3 in
the same area and equally hot, it holds exactly 30.0 fps, where X11 FIFO runs at
about 51 with about the same GPU time per frame. That this is the reason is
inferred.

A listed game on that route whose swapchain has fewer than 3 images therefore gets
hidden slots behind them. With 3 or more it already has an image to spare, so it
gets its exact count and no copy.

- The present's submit waits the guest's semaphores, then copies the frame to a
  hidden slot. The guest's image comes back when that submit's fence signals, and
  the hidden slot is what the compositor gets and later releases. Present ids and
  waits move with the frame to the hidden slot.
- There are 4 of them: the frame on screen, the one committed behind it, and one
  for each of the two frames the guest can have between its present and the commit.
- A frame that finds no hidden slot free goes out on the guest's own image, as it
  would without any. The guest then waits at its next acquire until the compositor
  releases it, so the compositor can slow the guest but not lose a frame or
  deadlock it.
- These frames go to the worker in present order in every mode, because a copy
  under way can't be replaced. gamescope replaces a committed frame it hasn't
  latched, so mailbox still behaves as mailbox. FIFO can queue as many frames as
  there are hidden slots.
- Frame generation reads the guest's image and has its own destinations. The two
  consumers share one ring behind the guest's images
  (`proxy::dest_ring`): they never run together, so the ring is sized for the
  larger of them — hidden vs `3 × (multiplier − 1) + 2` — and not for their sum.
  Each allocator skips what the other still has out: `in_flight` for a slot at
  the compositor, `gen_due` for a batch queued to be written. That last check is
  what keeps a mid-session generation toggle from letting a copy land in a slot
  the engine is about to draw into. While generation is on, frames take the
  direct path and no copy is made.
- The worker frees the guest's image, and the destroy joins the worker before it
  frees anything, so neither can overtake a copy.
- A present on a queue of another family than the swapchain's first present goes
  out directly: the guest's image belongs to that family and the copy's command
  buffers can only run on the first.

The shim logs the slot count once per swapchain, whatever `GNDRM_LOG_VKSHIM` says.

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
