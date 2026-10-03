/*
 * xpresent-check.c — host end-to-end check of the ICD shim's X11 present path.
 *
 * Reproduces exactly what the shim's `crate::x11` route does with the guest's
 * dma-buf, against a real X server (Xwayland on the dev box) and a real GPU:
 *
 *   xcb_dri3_pixmap_from_buffer(fd, size, w, h, stride, depth, bpp)
 *     -> xcb_present_pixmap(window, pixmap, serial, ..., idle_fence)
 *     -> an X sync fence reports when the server is done with the pixmap
 *        (the slot-reuse contract the shim's slot release rides)
 *
 * It then reads the window back with XGetImage and asserts the presented
 * pixels are visible, and that the first present's idle fence triggered once
 * the window moved on to the second pixmap.
 *
 * Exit: 0 PASS, 1 FAIL, 2 SKIP (no X server / no DRI3+Present / no gbm node).
 *
 * Node: $XPRESENT_NODE, default /dev/dri/renderD128 (falls back to card0).
 * This is the same dma-buf a gralloc/AHB scanout-pool buffer is, so a PASS
 * here means Xwayland's glamor device can import the shim's buffers.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#include <gbm.h>
#include <xcb/dri3.h>
#include <xcb/present.h>
#include <xcb/sync.h>
#include <xcb/xcb.h>

#define W 256
#define H 128
#define PATTERN_A_LEFT 0x00ff0000u /* red   */
#define PATTERN_A_RIGHT 0x0000ff00u /* green */
#define PATTERN_B_LEFT 0x000000ffu /* blue  */
#define PATTERN_B_RIGHT 0x00ffffffu /* white */
#define ALPHA 0xff000000u

static int drain_events(xcb_connection_t *c) {
    xcb_generic_event_t *e;
    int n = 0;
    while ((e = xcb_poll_for_event(c))) {
        n++;
        free(e);
    }
    return n;
}

/* Blocking checked query: returns 1 triggered, 0 not, -1 error. */
static int fence_triggered(xcb_connection_t *c, xcb_sync_fence_t f) {
    xcb_generic_error_t *err = NULL;
    xcb_sync_query_fence_reply_t *r =
        xcb_sync_query_fence_reply(c, xcb_sync_query_fence(c, f), &err);
    if (err) {
        free(err);
        return -1;
    }
    int t = r ? r->triggered : -1;
    free(r);
    return t;
}

struct dmabuf {
    struct gbm_bo *bo;
    uint32_t stride;
    uint64_t modifier;
    int fd;
    void *map;
};

static int open_node(const char *env) {
    const char *node = getenv(env);
    char path[256];
    if (node && *node) {
        snprintf(path, sizeof path, "%s", node);
    } else {
        snprintf(path, sizeof path, "%s", "/dev/dri/renderD128");
    }
    int fd = open(path, O_RDWR | O_CLOEXEC);
    if (fd >= 0) {
        fprintf(stderr, "xpresent-check: using dma-buf node %s\n", path);
        return fd;
    }
    fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
    if (fd >= 0) {
        fprintf(stderr, "xpresent-check: using dma-buf node /dev/dri/card0\n");
    }
    return fd;
}

static int make_dmabuf(struct gbm_device *gbm, uint32_t left, uint32_t right,
                       struct dmabuf *b) {
    memset(b, 0, sizeof *b);
    b->bo = gbm_bo_create(gbm, W, H, GBM_FORMAT_XRGB8888,
                          GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING);
    if (!b->bo)
        return -1;
    b->stride = gbm_bo_get_stride(b->bo);
    b->modifier = gbm_bo_get_modifier(b->bo);
    b->fd = gbm_bo_get_fd(b->bo);
    if (b->fd < 0)
        return -1;
    b->map = mmap(NULL, (size_t)b->stride * H, PROT_READ | PROT_WRITE,
                  MAP_SHARED, b->fd, 0);
    if (b->map == MAP_FAILED)
        return -1;
    for (uint32_t y = 0; y < H; y++) {
        for (uint32_t x = 0; x < W; x++) {
            uint32_t *px = (uint32_t *)((char *)b->map + y * b->stride + x * 4);
            *px = (x < W / 2) ? left : right;
        }
    }
    msync(b->map, (size_t)b->stride * H, MS_SYNC);
    return 0;
}

int main(void) {
    int screen;
    xcb_connection_t *c = xcb_connect(NULL, &screen);
    if (xcb_connection_has_error(c)) {
        printf("SKIP: no X server ($DISPLAY unset/unreachable)\n");
        return 2;
    }
    const xcb_query_extension_reply_t *d3 =
        xcb_get_extension_data(c, &xcb_dri3_id);
    const xcb_query_extension_reply_t *pr =
        xcb_get_extension_data(c, &xcb_present_id);
    if (!d3 || !d3->present || !pr || !pr->present) {
        printf("SKIP: X server has no DRI3 and/or Present\n");
        return 2;
    }
    const xcb_setup_t *setup = xcb_get_setup(c);
    xcb_screen_t *root = xcb_setup_roots_iterator(setup).data;
    if (!root) {
        printf("SKIP: no X screen\n");
        return 2;
    }

    int node = open_node("XPRESENT_NODE");
    if (node < 0) {
        printf("SKIP: no /dev/dri render node (%s) - host has no importable "
               "dma-buf source\n",
               strerror(errno));
        return 2;
    }
    struct gbm_device *gbm = gbm_create_device(node);
    if (!gbm) {
        printf("SKIP: gbm_create_device failed on the DRM node\n");
        return 2;
    }

    uint32_t mask = XCB_CW_BACK_PIXEL | XCB_CW_OVERRIDE_REDIRECT;
    uint32_t vals[2] = {root->black_pixel, 1};
    xcb_window_t win = xcb_generate_id(c);
    xcb_create_window(c, root->root_depth, win, root->root, 0, 0, W, H, 0,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, root->root_visual, mask,
                      vals);
    xcb_map_window(c, win);
    xcb_flush(c);
    sleep(1); /* let the window be mapped before presenting */

    struct dmabuf a, b;
    if (make_dmabuf(gbm, PATTERN_A_LEFT, PATTERN_A_RIGHT, &a) != 0 ||
        make_dmabuf(gbm, PATTERN_B_LEFT, PATTERN_B_RIGHT, &b) != 0) {
        printf("SKIP: could not allocate a linear dma-buf on the node\n");
        return 2;
    }

    xcb_pixmap_t pa = xcb_generate_id(c);
    xcb_pixmap_t pb = xcb_generate_id(c);
    xcb_generic_error_t *err =
        xcb_request_check(c, xcb_dri3_pixmap_from_buffer_checked(
                                c, pa, win, a.stride * H, W, H, a.stride,
                                root->root_depth, 32, a.fd));
    if (err) {
        printf("FAIL: PixmapFromBuffer import failed (X error %d) - Xwayland "
               "cannot import this dma-buf\n",
               err->error_code);
        return 1;
    }
    if (xcb_request_check(c, xcb_dri3_pixmap_from_buffer_checked(
                                 c, pb, win, b.stride * H, W, H, b.stride,
                                 root->root_depth, 32, b.fd))) {
        printf("FAIL: second PixmapFromBuffer import failed\n");
        return 1;
    }

    xcb_sync_fence_t fa = xcb_generate_id(c);
    if (xcb_request_check(
            c, xcb_sync_create_fence_checked(c, win, fa, 0))) {
        printf("FAIL: X sync fence create failed\n");
        return 1;
    }

    /* Present A with its idle fence, then B; B superseding A is what makes A's
     * pixmap idle and triggers fa - the shim's slot-release contract. */
    if (xcb_request_check(c, xcb_present_pixmap_checked(
                                 c, win, pa, 1, 0, 0, 0, 0, 0, 0, fa, 0, 0, 0,
                                 0, 0, NULL))) {
        printf("FAIL: Present of the first pixmap failed\n");
        return 1;
    }
    xcb_flush(c);
    drain_events(c);

    if (xcb_request_check(c, xcb_present_pixmap_checked(
                                 c, win, pb, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                                 0, 0, NULL))) {
        printf("FAIL: Present of the second pixmap failed\n");
        return 1;
    }
    xcb_flush(c);

    int released = 0;
    for (int i = 0; i < 20 && !released; i++) {
        struct pollfd p = {xcb_get_file_descriptor(c), POLLIN, 0};
        poll(&p, 1, 100);
        drain_events(c);
        released = fence_triggered(c, fa) == 1;
    }

    xcb_get_image_reply_t *img = xcb_get_image_reply(
        c, xcb_get_image(c, XCB_IMAGE_FORMAT_Z_PIXMAP, win, 0, 0, W, H,
                         0xffffffff),
        NULL);
    if (!img) {
        printf("FAIL: XGetImage readback failed\n");
        return 1;
    }
    uint32_t *px = (uint32_t *)xcb_get_image_data(img);
    uint32_t got_left = px[0] & 0x00ffffffu;
    uint32_t got_right = px[(W - 1)] & 0x00ffffffu;

    printf("window readback: (0,0)=%#08x (W-1,0)=%#08x  first fence released=%d\n",
           px[0], px[W - 1], released);

    int ok = 1;
    if (got_left != PATTERN_B_LEFT || got_right != PATTERN_B_RIGHT) {
        printf("FAIL: window content is not the last presented frame "
               "(left=%#08x right=%#08x, expected %#08x / %#08x)\n",
               got_left, got_right, PATTERN_B_LEFT, PATTERN_B_RIGHT);
        ok = 0;
    }
    if (!released) {
        printf("FAIL: the first present's idle fence never triggered\n");
        ok = 0;
    }
    printf("%s: DRI3 PixmapFromBuffer + Present reached the X window and the "
           "idle fence released the slot\n",
           ok ? "PASS" : "FAIL");
    return ok ? 0 : 1;
}
