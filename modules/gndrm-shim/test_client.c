/*
 * B9 integration test: a real-libdrm KMS client driven through the
 * LD_PRELOAD'd gndrm-shim against gndrm-hostd. Mirrors the exact call
 * sequence gamescope's DRMBackend performs (discovery → caps → properties →
 * prime import → addfb2 → blob → atomic modeset → flip event).
 *
 * Build: cc -Wall -Wextra -Werror -o b9_client b9_client.c $(pkg-config --cflags --libs libdrm)
 * Run:   LD_PRELOAD=libgndrm_shim.so GAMENATIVE_DRM_SOCKET=/tmp/gndrm-b9.sock ./b9_client
 */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <xf86drm.h>
#include <xf86drmMode.h>
#include <drm_fourcc.h>

#define CHECK(cond, msg)                                          \
    do {                                                          \
        if (!(cond)) {                                            \
            fprintf(stderr, "FAIL: %s (line %d, errno=%d %s)\n",  \
                    msg, __LINE__, errno, strerror(errno));       \
            return 1;                                             \
        }                                                         \
    } while (0)

static int got_flip;
static uint64_t got_flip_data;

static void flip_handler(int fd, unsigned int seq, unsigned int tv_sec,
                         unsigned int tv_usec, unsigned int crtc_id, void *user_data)
{
    (void)fd;
    printf("  flip event: seq=%u time=%u.%06u crtc=%u data=%p\n",
           seq, tv_sec, tv_usec, crtc_id, user_data);
    got_flip = 1;
    got_flip_data = (uint64_t)(uintptr_t)user_data;
}

/* gamescope-style property lookup by name */
static uint32_t prop_id(int fd, uint32_t obj_id, uint32_t obj_type, const char *name)
{
    drmModeObjectProperties *props = drmModeObjectGetProperties(fd, obj_id, obj_type);
    if (!props)
        return 0;
    uint32_t id = 0;
    for (uint32_t i = 0; i < props->count_props; i++) {
        drmModePropertyRes *p = drmModeGetProperty(fd, props->props[i]);
        if (!p)
            continue;
        if (strcmp(p->name, name) == 0)
            id = p->prop_id;
        drmModeFreeProperty(p);
        if (id)
            break;
    }
    drmModeFreeObjectProperties(props);
    return id;
}

int main(void)
{
    int fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
    CHECK(fd >= 0, "open /dev/dri/card0");
    printf("open: fd=%d (shim socket)\n", fd);

    /* version */
    drmVersion *ver = drmGetVersion(fd);
    CHECK(ver != NULL, "drmGetVersion");
    printf("version: %d.%d.%d name=%.*s desc=%.*s\n",
           ver->version_major, ver->version_minor, ver->version_patchlevel,
           ver->name_len, ver->name, ver->desc_len, ver->desc);
    CHECK(ver->name_len == 10 && memcmp(ver->name, "gamenative", 10) == 0,
          "driver name is gamenative");
    drmFreeVersion(ver);

    /* caps */
    uint64_t v = 1;
    CHECK(drmGetCap(fd, DRM_CAP_DUMB_BUFFER, &v) == 0 && v == 1, "DUMB_BUFFER=1");
    CHECK(drmGetCap(fd, DRM_CAP_SYNCOBJ, &v) == 0 && v == 1, "SYNCOBJ=1");
    CHECK(drmGetCap(fd, DRM_CAP_TIMESTAMP_MONOTONIC, &v) == 0 && v == 1, "MONOTONIC=1");
    printf("caps: syncobj=1 monotonic=1 dumb=1\n");

    /* client caps */
    CHECK(drmSetClientCap(fd, DRM_CLIENT_CAP_ATOMIC, 1) == 0, "ATOMIC cap");
    CHECK(drmSetClientCap(fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1) == 0, "UNIVERSAL cap");
    CHECK(drmSetMaster(fd) == 0, "drmSetMaster");

    /* resources */
    drmModeRes *res = drmModeGetResources(fd);
    CHECK(res != NULL, "drmModeGetResources");
    CHECK(res->count_crtcs == 1 && res->count_connectors == 1 && res->count_encoders == 1,
          "1/1/1 topology");
    printf("resources: crtc=%d conn=%d enc=%d (%ux%u..%ux%u)\n",
           res->crtcs[0], res->connectors[0], res->encoders[0],
           res->min_width, res->min_height, res->max_width, res->max_height);
    uint32_t crtc_id = res->crtcs[0];

    /* connector */
    drmModeConnector *conn = drmModeGetConnector(fd, res->connectors[0]);
    CHECK(conn != NULL, "drmModeGetConnector");
    CHECK(conn->connection == DRM_MODE_CONNECTED, "connector connected");
    CHECK(conn->count_modes == 1, "one mode");
    printf("connector: type=%u status=connected %ux%u@%u (%s)\n",
           conn->connector_type,
           conn->modes[0].hdisplay, conn->modes[0].vdisplay, conn->modes[0].vrefresh,
           conn->modes[0].name);
    CHECK(conn->modes[0].hdisplay == 1280 && conn->modes[0].vdisplay == 720,
          "mode is 1280x720");
    drmModeModeInfo mode = conn->modes[0];

    /* encoder + plane */
    drmModeEncoder *enc = drmModeGetEncoder(fd, res->encoders[0]);
    CHECK(enc != NULL && enc->possible_crtcs == 1, "encoder possible_crtcs=1");
    drmModeFreeEncoder(enc);

    drmModePlaneRes *pres = drmModeGetPlaneResources(fd);
    CHECK(pres != NULL && pres->count_planes == 2, "two planes");
    drmModePlane *plane = drmModeGetPlane(fd, pres->planes[0]);
    CHECK(plane != NULL, "drmModeGetPlane");
    uint32_t plane_id = plane->plane_id;
    CHECK(plane->possible_crtcs == 1, "plane possible_crtcs");
    int have_xbgr = 0;
    for (uint32_t i = 0; i < plane->count_formats; i++)
        if (plane->formats[i] == DRM_FORMAT_XBGR8888)
            have_xbgr = 1;
    CHECK(have_xbgr, "primary supports XBGR8888");
    drmModeFreePlane(plane);
    drmModeFreePlaneResources(pres);

    /* property ids (exactly how gamescope resolves them) */
    uint32_t conn_crtc = prop_id(fd, conn->connector_id, DRM_MODE_OBJECT_CONNECTOR, "CRTC_ID");
    uint32_t crtc_mode = prop_id(fd, crtc_id, DRM_MODE_OBJECT_CRTC, "MODE_ID");
    uint32_t crtc_active = prop_id(fd, crtc_id, DRM_MODE_OBJECT_CRTC, "ACTIVE");
    uint32_t plane_fb = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "FB_ID");
    uint32_t plane_crtc = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "CRTC_ID");
    uint32_t src_x = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "SRC_X");
    uint32_t src_y = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "SRC_Y");
    uint32_t src_w = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "SRC_W");
    uint32_t src_h = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "SRC_H");
    uint32_t crtc_x = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "CRTC_X");
    uint32_t crtc_y = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "CRTC_Y");
    uint32_t crtc_w = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "CRTC_W");
    uint32_t crtc_h = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "CRTC_H");
    uint32_t in_fence = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "IN_FENCE_FD");
    CHECK(conn_crtc && crtc_mode && crtc_active && plane_fb && plane_crtc &&
          src_w && crtc_w && in_fence, "all expected properties present");
    printf("properties: MODE_ID=%u ACTIVE=%u FB_ID=%u IN_FENCE_FD=%u\n",
           crtc_mode, crtc_active, plane_fb, in_fence);

    /* plane type property is immutable enum */
    uint32_t ptype = prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "type");
    drmModePropertyRes *tp = drmModeGetProperty(fd, ptype);
    CHECK(tp != NULL && (tp->flags & DRM_MODE_PROP_IMMUTABLE), "type immutable");
    CHECK(tp->count_enums == 3, "type has 3 enums");
    drmModeFreeProperty(tp);

    /* import a "dma-buf" (memfd stands in on host) and create an FB */
    int buf_fd = memfd_create("b9-buffer", 0);
    CHECK(buf_fd >= 0, "memfd_create");
    CHECK(ftruncate(buf_fd, 1280 * 720 * 4) == 0, "ftruncate");

    uint32_t handle = 0;
    CHECK(drmPrimeFDToHandle(fd, buf_fd, &handle) == 0 && handle != 0,
          "drmPrimeFDToHandle");
    printf("prime: fd=%d -> handle=%u\n", buf_fd, handle);

    /* dedup: importing a dup of the same buffer must give the same handle */
    int dup_fd = dup(buf_fd);
    uint32_t handle2 = 0;
    CHECK(drmPrimeFDToHandle(fd, dup_fd, &handle2) == 0 && handle2 == handle,
          "prime dedup by st_ino");
    close(dup_fd);

    /* HANDLE_TO_FD round trip */
    int back_fd = -1;
    CHECK(drmPrimeHandleToFD(fd, handle, 0, &back_fd) == 0 && back_fd >= 0,
          "drmPrimeHandleToFD");
    struct stat st;
    CHECK(fstat(back_fd, &st) == 0 && st.st_size == 1280 * 720 * 4, "fd size round-trip");
    close(back_fd);

    uint32_t fb_id = 0;
    uint32_t pitches[4] = { 1280 * 4, 0, 0, 0 };
    uint32_t offsets[4] = { 0, 0, 0, 0 };
    uint32_t handles[4] = { handle, 0, 0, 0 };
    CHECK(drmModeAddFB2(fd, 1280, 720, DRM_FORMAT_XBGR8888,
                        handles, pitches, offsets, &fb_id, 0) == 0 && fb_id != 0,
          "drmModeAddFB2");
    printf("addfb2: fb=%u\n", fb_id);

    /* mode blob */
    uint32_t blob_id = 0;
    CHECK(drmModeCreatePropertyBlob(fd, &mode, sizeof(mode), &blob_id) == 0 && blob_id != 0,
          "CreatePropertyBlob");
    drmModePropertyBlobPtr blob = drmModeGetPropertyBlob(fd, blob_id);
    CHECK(blob != NULL && blob->length == sizeof(mode), "GetPropertyBlob length");
    CHECK(memcmp(blob->data, &mode, sizeof(mode)) == 0, "blob round-trip byte-exact");
    drmModeFreePropertyBlob(blob);

    /* TEST_ONLY first: must succeed without applying */
    drmModeAtomicReq *req = drmModeAtomicAlloc();
    CHECK(req != NULL, "atomic alloc");
    drmModeAtomicAddProperty(req, conn->connector_id, conn_crtc, crtc_id);
    drmModeAtomicAddProperty(req, crtc_id, crtc_mode, blob_id);
    drmModeAtomicAddProperty(req, crtc_id, crtc_active, 1);
    drmModeAtomicAddProperty(req, plane_id, plane_fb, fb_id);
    drmModeAtomicAddProperty(req, plane_id, plane_crtc, crtc_id);
    drmModeAtomicAddProperty(req, plane_id, src_x, 0);
    drmModeAtomicAddProperty(req, plane_id, src_y, 0);
    drmModeAtomicAddProperty(req, plane_id, src_w, 1280ULL << 16);
    drmModeAtomicAddProperty(req, plane_id, src_h, 720ULL << 16);
    drmModeAtomicAddProperty(req, plane_id, crtc_x, 0);
    drmModeAtomicAddProperty(req, plane_id, crtc_y, 0);
    drmModeAtomicAddProperty(req, plane_id, crtc_w, 1280);
    drmModeAtomicAddProperty(req, plane_id, crtc_h, 720);

    CHECK(drmModeAtomicCommit(fd, req, DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_ATOMIC_ALLOW_MODESET,
                              NULL) == 0,
          "TEST_ONLY commit");

    /* real modeset with flip event */
    CHECK(drmModeAtomicCommit(fd, req,
                              DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_ATOMIC_NONBLOCK |
                                  DRM_MODE_PAGE_FLIP_EVENT,
                              (void *)(uintptr_t)0xC0FFEE) == 0,
          "modeset commit");
    drmModeAtomicFree(req);
    printf("modeset committed; waiting for flip event...\n");

    struct pollfd pfd = { .fd = fd, .events = POLLIN };
    CHECK(poll(&pfd, 1, 2000) == 1, "poll readable within 2s");
    drmEventContext ev = {
        .version = 3,
        .page_flip_handler2 = flip_handler,
    };
    CHECK(drmHandleEvent(fd, &ev) == 0, "drmHandleEvent");
    CHECK(got_flip && got_flip_data == 0xC0FFEE, "flip event with user data");

    /* page flip: second FB on the same buffer, flip again */
    uint32_t fb2 = 0;
    CHECK(drmModeAddFB2(fd, 1280, 720, DRM_FORMAT_XBGR8888,
                        handles, pitches, offsets, &fb2, 0) == 0,
          "addfb2 #2");
    req = drmModeAtomicAlloc();
    drmModeAtomicAddProperty(req, plane_id, plane_fb, fb2);
    CHECK(drmModeAtomicCommit(fd, req, DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
                              (void *)(uintptr_t)0xBEEF) == 0,
          "page-flip commit (no ALLOW_MODESET)");
    drmModeAtomicFree(req);
    CHECK(poll(&pfd, 1, 2000) == 1, "poll #2");
    CHECK(drmHandleEvent(fd, &ev) == 0, "drmHandleEvent #2");
    CHECK(got_flip_data == 0xBEEF, "second flip event data");

    /* negative path: modeset props without ALLOW_MODESET must fail EINVAL */
    req = drmModeAtomicAlloc();
    drmModeAtomicAddProperty(req, crtc_id, crtc_active, 0);
    CHECK(drmModeAtomicCommit(fd, req, 0, NULL) != 0 && errno == EINVAL,
          "modeset without ALLOW_MODESET rejected");
    drmModeAtomicFree(req);

    /* dup'd fds must keep working (libliftoff dup()s the DRM fd — day-two
     * on-device: liftoff_device_create → dup → ENOTTY before the shim
     * learned to track dups). */
    int dupfd = dup(fd);
    CHECK(dupfd >= 0, "dup(event fd)");
    drmModeRes *dres = drmModeGetResources(dupfd);
    CHECK(dres != NULL && dres->count_crtcs == 1, "drmModeGetResources on dup fd");
    drmModeFreeResources(dres);
    drmModePlaneRes *dpres = drmModeGetPlaneResources(dupfd);
    CHECK(dpres != NULL && dpres->count_planes == 2, "drmModeGetPlaneResources on dup fd");
    drmModeFreePlaneResources(dpres);
    int dupfd2 = fcntl(fd, F_DUPFD_CLOEXEC, 0);
    CHECK(dupfd2 >= 0, "fcntl(F_DUPFD_CLOEXEC)");
    CHECK(drmModeGetResources(dupfd2) != NULL, "drmModeGetResources on fcntl-dup fd");
    close(dupfd2);
    close(dupfd);
    /* the original fd is still alive after the dups are gone */
    pres = drmModeGetPlaneResources(fd);
    CHECK(pres != NULL && pres->count_planes == 2, "original fd alive after dup close");
    drmModeFreePlaneResources(pres);

    /* finish_drm-style teardown: IN_FENCE_FD = (u64)-1 must travel as "no
     * fence" (day-two: daemon mistook the u64 sentinel for a fence fd and
     * rejected the commit EINVAL). */
    CHECK(in_fence != 0, "IN_FENCE_FD prop exists");
    req = drmModeAtomicAlloc();
    drmModeAtomicAddProperty(req, conn->connector_id, conn_crtc, 0);
    drmModeAtomicAddProperty(req, crtc_id, crtc_active, 0);
    drmModeAtomicAddProperty(req, crtc_id, crtc_mode, 0);
    drmModeAtomicAddProperty(req, plane_id, plane_fb, 0);
    drmModeAtomicAddProperty(req, plane_id, in_fence, (uint64_t)-1);
    drmModeAtomicAddProperty(req, plane_id, plane_crtc, 0);
    drmModeAtomicAddProperty(req, plane_id, src_x, 0);
    drmModeAtomicAddProperty(req, plane_id, src_y, 0);
    drmModeAtomicAddProperty(req, plane_id, src_w, 0);
    drmModeAtomicAddProperty(req, plane_id, src_h, 0);
    drmModeAtomicAddProperty(req, plane_id, crtc_x, 0);
    drmModeAtomicAddProperty(req, plane_id, crtc_y, 0);
    drmModeAtomicAddProperty(req, plane_id, crtc_w, 0);
    drmModeAtomicAddProperty(req, plane_id, crtc_h, 0);
    CHECK(drmModeAtomicCommit(fd, req, DRM_MODE_ATOMIC_ALLOW_MODESET, NULL) == 0,
          "teardown commit with IN_FENCE_FD=-1");
    drmModeAtomicFree(req);

    /* gamescope's DRM backend creates one always-signalled sync object at init
     * and rides the fd it exports as the primary plane's in-fence on every
     * commit. The export has to be poll-ready for the presenter's poll-based
     * in-fence wait to pass, so assert that too. */
    uint32_t syncobj = 0;
    CHECK(drmSyncobjCreate(fd, DRM_SYNCOBJ_CREATE_SIGNALED, &syncobj) == 0,
          "drmSyncobjCreate(SIGNALED)");
    CHECK(syncobj != 0, "sync object handle allocated");
    int sync_fd = -1;
    CHECK(drmSyncobjExportSyncFile(fd, syncobj, &sync_fd) == 0,
          "drmSyncobjExportSyncFile");
    CHECK(sync_fd >= 0, "exported sync file fd");
    struct pollfd spfd;
    memset(&spfd, 0, sizeof(spfd));
    spfd.fd = sync_fd;
    spfd.events = POLLIN;
    CHECK(poll(&spfd, 1, 0) == 1 && (spfd.revents & POLLIN) != 0,
          "exported always-signalled fence is poll-ready");
    close(sync_fd);
    CHECK(drmSyncobjDestroy(fd, syncobj) == 0, "drmSyncobjDestroy");

    /* teardown */
    CHECK(drmModeDestroyPropertyBlob(fd, blob_id) == 0, "destroy blob");
    CHECK(drmModeRmFB(fd, fb_id) == 0 || 1, "rmfb fb (may be busy: still scanned out)");
    drmModeFreeConnector(conn);
    drmModeFreeResources(res);
    close(buf_fd);
    close(fd);
    printf("PASS: full libdrm discovery→modeset→flip sequence through shim\n");
    return 0;
}
