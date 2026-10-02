/*
 * render_blame.c — enumerate the exact DRM requests Mesa's gbm/kopper/glamor
 * path issues on a render-node fd, through the gndrm shim, and report which
 * one the daemon does not answer (or answers with the wrong errno) and which
 * one blocks the caller forever.
 *
 * Each step is guarded by an alarm(2) so a request the daemon never answers is
 * reported as HANG instead of hanging the harness. The sequence mirrors, in
 * order, the libdrm/GBM/EGL calls Xwayland's glamor setup makes:
 *
 *   loader:  drmGetVersion, drmGetDevice2, drmGetNodeTypeFromFd,
 *            drmGetCap(DUMB_BUFFER/PRIME/SYNCOBJ/SYNCOBJ_TIMELINE)
 *   gbm:     GEM_CREATE, GEM_MMAP (+ mmap), PRIME_HANDLE_TO_FD,
 *            PRIME_FD_TO_HANDLE, GEM_CLOSE, MODE_CREATE_DUMB/MAP_DUMB/DESTROY_DUMB
 *   glamor:  syncobj create/export/eventfd/timeline-wait/destroy
 *   legacy:  GEM_FLINK, GEM_OPEN
 *
 * Build: cc -Wall -Wextra -Werror -o render_blame render_blame.c \
 *            $(pkg-config --cflags --libs libdrm)
 * Run:   LD_PRELOAD=libgndrm_shim.so GAMENATIVE_DRM_SOCKET=/tmp/x.sock \
 *            ./render_blame
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#include <xf86drm.h>
#include <xf86drmMode.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

/* Legacy generic GEM ioctls: dropped from modern drm.h, so define the wire
 * structs and request numbers here exactly as the daemon/shim agree on them. */
struct gen_gem_create {
    uint64_t size;
    uint32_t handle;
    uint32_t flags;
};
struct gen_gem_mmap {
    uint32_t handle;
    uint32_t pad;
    uint64_t offset;
    uint64_t size;
};
#define HARNESS_IOCTL_GEM_CREATE DRM_IOWR(0x1b, struct gen_gem_create)
#define HARNESS_IOCTL_GEM_MMAP DRM_IOWR(0x1e, struct gen_gem_mmap)

static int hang_seen;

static void on_alarm(int sig)
{
    (void)sig;
    hang_seen = 1;
    const char msg[] = "\nHANG: request never answered within 5s\n";
    ssize_t w = write(2, msg, sizeof(msg) - 1);
    (void)w;
    _exit(3);
}

#define STEP(label, expr)                                                     \
    do {                                                                      \
        alarm(5);                                                             \
        hang_seen = 0;                                                        \
        errno = 0;                                                            \
        long _r = (long)(expr);                                               \
        alarm(0);                                                             \
        printf("%-34s ret=%ld errno=%d%s\n", label, _r, errno,                \
               _r == 0 ? "" : " (failed)");                                   \
    } while (0)

int main(void)
{
    struct sigaction sa = { .sa_handler = on_alarm };
    sigaction(SIGALRM, &sa, NULL);
    setvbuf(stdout, NULL, _IONBF, 0);

    printf("== render-node request matrix (Mesa gbm/kopper/glamor) ==\n");

    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        printf("open renderD128: %s\n", strerror(errno));
        return 2;
    }

    drmVersionPtr v = drmGetVersion(fd);
    printf("%-34s %s\n", "DRM_IOCTL_VERSION", v ? v->name : "(null)");
    if (v)
        drmFreeVersion(v);

    drmDevicePtr dev = NULL;
    STEP("drmGetDevice2", drmGetDevice2(fd, 0, &dev));
    if (dev)
        drmFreeDevice(&dev);
    STEP("drmGetNodeTypeFromFd", drmGetNodeTypeFromFd(fd));

    uint64_t cap = 0;
    STEP("GET_CAP DUMB_BUFFER", drmGetCap(fd, DRM_CAP_DUMB_BUFFER, &cap));
    STEP("GET_CAP PRIME", drmGetCap(fd, DRM_CAP_PRIME, &cap));
    STEP("GET_CAP SYNCOBJ", drmGetCap(fd, DRM_CAP_SYNCOBJ, &cap));
    STEP("GET_CAP SYNCOBJ_TIMELINE",
         drmGetCap(fd, DRM_CAP_SYNCOBJ_TIMELINE, &cap));
    STEP("GET_CAP ADDFB2_MODIFIERS",
         drmGetCap(fd, DRM_CAP_ADDFB2_MODIFIERS, &cap));

    /* --- gbm / generic GEM --- */
    struct gen_gem_create gc = { .size = 4096 };
    STEP("GEM_CREATE (0x1b)",
         drmIoctl(fd, HARNESS_IOCTL_GEM_CREATE, &gc));

    struct gen_gem_mmap gm = { .handle = gc.handle };
    STEP("GEM_MMAP (0x1e)", drmIoctl(fd, HARNESS_IOCTL_GEM_MMAP, &gm));
    if (gm.offset) {
        void *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
                       (off_t)gm.offset);
        printf("%-34s %s\n", "mmap(dev_fd, gem offset)",
               p == MAP_FAILED ? strerror(errno) : "ok");
        if (p != MAP_FAILED)
            munmap(p, 4096);
    }

    int dmafd = -1;
    STEP("PRIME_HANDLE_TO_FD", drmPrimeHandleToFD(fd, gc.handle, 0, &dmafd));
    uint32_t imp = 0;
    if (dmafd >= 0)
        STEP("PRIME_FD_TO_HANDLE", drmPrimeFDToHandle(fd, dmafd, &imp));
    if (gc.handle)
        STEP("GEM_CLOSE", drmCloseBufferHandle(fd, gc.handle));
    if (dmafd >= 0)
        close(dmafd);

    /* --- dumb buffers (glamor sw / screen pixmap fallback) --- */
    uint32_t dumb_h = 0;
    struct drm_mode_create_dumb cd = {
        .width = 64, .height = 64, .bpp = 32, .flags = 0,
    };
    STEP("MODE_CREATE_DUMB", drmIoctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &cd));
    dumb_h = cd.handle;
    if (cd.pitch) {
        struct drm_mode_map_dumb md = { .handle = cd.handle };
        STEP("MODE_MAP_DUMB", drmIoctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &md));
    }
    if (dumb_h) {
        struct drm_mode_destroy_dumb dd = { .handle = dumb_h };
        STEP("MODE_DESTROY_DUMB", drmIoctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &dd));
    }

    /* --- syncobj (glamor/dri3 explicit sync probe) --- */
    uint32_t sobj = 0;
    STEP("SYNCOBJ_CREATE", drmSyncobjCreate(fd, DRM_SYNCOBJ_CREATE_SIGNALED, &sobj));
    if (sobj) {
        int sfd = -1;
        STEP("SYNCOBJ_HANDLE_TO_FD", drmSyncobjHandleToFD(fd, sobj, &sfd));
        if (sfd >= 0)
            close(sfd);
        STEP("SYNCOBJ_EVENTFD",
             drmSyncobjEventfd(fd, sobj, 0, -1, 0));
        uint32_t sobjs[1] = { sobj };
        uint64_t pts[1] = { 0 };
        STEP("SYNCOBJ_TIMELINE_WAIT",
             drmSyncobjTimelineWait(fd, sobjs, pts, 1, 0, 0, 0));
        STEP("SYNCOBJ_DESTROY", drmSyncobjDestroy(fd, sobj));
    }

    /* --- legacy flink/open --- */
    if (gc.handle)
        (void)0;
    struct drm_gem_flink fl = { .handle = 1 };
    STEP("GEM_FLINK (0x0a)", drmIoctl(fd, DRM_IOCTL_GEM_FLINK, &fl));

    printf("== done (no hang) ==\n");
    close(fd);
    return hang_seen ? 3 : 0;
}
