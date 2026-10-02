/*
 * Render-node harness: drive the gamenative-drm daemon's render-node surface
 * through the LD_PRELOAD'd gndrm-shim, exactly as Xwayland's glamor/dri3/drm
 * would. Everything here goes through the shim/socket path — the DRM fd is the
 * shim's event-channel socket, and every ioctl is proxied to `gndrm-hostd`.
 *
 * Sequence:
 *   open("/dev/dri/renderD128")        -> render-node connection
 *   VERSION / GET_CAP (DUMB, PRIME)    -> device identification
 *   render-node gating                 -> SET_MASTER and modeset refused EACCES
 *   GEM_CREATE                         -> allocate a buffer object
 *   GEM_MMAP + mmap(fd, offset)        -> map it (shim maps its dma-buf)
 *   PRIME_HANDLE_TO_FD                 -> export a dma-buf fd
 *   PRIME_FD_TO_HANDLE                 -> import it back (dedup: same handle)
 *   GEM_CLOSE                          -> release
 *
 * Build: cc -Wall -Wextra -Werror -o render_node_client render_node_client.c \
 *            $(pkg-config --cflags --libs libdrm)
 * Run:   LD_PRELOAD=libgndrm_shim.so GAMENATIVE_DRM_SOCKET=/tmp/gndrm-rn.sock \
 *            ./render_node_client
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#include <xf86drm.h>
#include <xf86drmMode.h>
#include <drm/drm.h>

#define CHECK(cond, msg)                                                      \
    do {                                                                      \
        if (!(cond)) {                                                        \
            fprintf(stderr, "FAIL: %s (line %d, errno=%d %s)\n", msg,          \
                    __LINE__, errno, strerror(errno));                        \
            return 1;                                                         \
        }                                                                     \
    } while (0)

/* The generic GEM_CREATE/GEM_MMAP ioctls were dropped from modern drm.h (it
 * now carries only GEM_CLOSE/FLINK/OPEN); the daemon and shim agree on these
 * historical encodings, so the harness reproduces them here. */
struct harness_gem_create {
    uint64_t size;
    uint32_t handle;
    uint32_t flags;
};
struct harness_gem_mmap {
    uint32_t handle;
    uint32_t pad;
    uint64_t offset;
    uint64_t size;
};
#define HARNESS_IOCTL_GEM_CREATE DRM_IOWR(0x1b, struct harness_gem_create)
#define HARNESS_IOCTL_GEM_MMAP DRM_IOWR(0x1e, struct harness_gem_mmap)

/* The offset token the daemon mints in GEM_MMAP. */
#define GEM_MMAP_OFFSET_BASE 0x474e000000000000ull
#define GEM_MMAP_HANDLE_MASK 0x0000ffffffffffffull

int main(void)
{
    printf("== render-node harness ==\n");

    /* 1. Render-node open, intercepted by the shim to the daemon. */
    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    CHECK(fd >= 0, "open render node (is LD_PRELOAD/GAMENATIVE_DRM_SOCKET set?)");
    printf("open /dev/dri/renderD128           -> fd %d\n", fd);

    /* 2. VERSION identifies the userspace device. */
    drmVersionPtr ver = drmGetVersion(fd);
    CHECK(ver != NULL, "drmGetVersion");
    CHECK(ver->name_len > 0 && strncmp(ver->name, "gamenative", 10) == 0,
          "version name is 'gamenative'");
    printf("DRM_IOCTL_VERSION                  -> name='%s' %d.%d.%d\n",
           ver->name, ver->version_major, ver->version_minor,
           ver->version_patchlevel);
    drmFreeVersion(ver);

    /* 3. GET_CAP for the caps GBM/Mesa probe. */
    uint64_t dumb = 0, prime = 0;
    CHECK(drmGetCap(fd, DRM_CAP_DUMB_BUFFER, &dumb) == 0, "GET_CAP DUMB_BUFFER");
    CHECK(drmGetCap(fd, DRM_CAP_PRIME, &prime) == 0, "GET_CAP PRIME");
    CHECK(dumb == 1, "DUMB_BUFFER advertised");
    CHECK((prime & DRM_PRIME_CAP_IMPORT) && (prime & DRM_PRIME_CAP_EXPORT),
          "PRIME import+export advertised");
    printf("DRM_IOCTL_GET_CAP                  -> DUMB_BUFFER=%llu PRIME=0x%llx\n",
           (unsigned long long)dumb, (unsigned long long)prime);

    /* 4. Render-node gating: master acquisition and modeset are refused. */
    errno = 0;
    int master = drmSetMaster(fd);
    int master_errno = errno;
    CHECK(master != 0, "drmSetMaster must fail on a render node");
    CHECK(master_errno == EACCES, "drmSetMaster errno is EACCES");
    printf("DRM_IOCTL_SET_MASTER               -> refused (errno=%d %s)\n",
           master_errno, strerror(master_errno));

    errno = 0;
    drmModeRes *res = drmModeGetResources(fd);
    int res_errno = errno;
    CHECK(res == NULL, "drmModeGetResources must fail on a render node");
    CHECK(res_errno == EACCES, "modeset errno is EACCES");
    printf("DRM_IOCTL_MODE_GETRESOURCES        -> refused (errno=%d %s)\n",
           res_errno, strerror(res_errno));

    /* 5. GEM_CREATE. */
    struct harness_gem_create gc = { .size = 4096, .handle = 0, .flags = 0 };
    CHECK(drmIoctl(fd, HARNESS_IOCTL_GEM_CREATE, &gc) == 0, "GEM_CREATE");
    CHECK(gc.handle > 0, "GEM_CREATE returned a handle");
    printf("DRM_IOCTL_GEM_CREATE               -> handle=%u size=%llu\n",
           gc.handle, (unsigned long long)gc.size);

    /* 6. GEM_MMAP mints a handle-carrying offset; mmap() on the device fd is
     *    redirected by the shim to the object's dma-buf. */
    struct harness_gem_mmap gm = { .handle = gc.handle };
    CHECK(drmIoctl(fd, HARNESS_IOCTL_GEM_MMAP, &gm) == 0, "GEM_MMAP");
    CHECK(gm.size == 4096, "GEM_MMAP reports the object size");
    CHECK((gm.offset & GEM_MMAP_OFFSET_BASE) == GEM_MMAP_OFFSET_BASE,
          "GEM_MMAP offset carries the handle token");
    CHECK((gm.offset & GEM_MMAP_HANDLE_MASK) == gc.handle,
          "GEM_MMAP offset carries this handle");
    printf("DRM_IOCTL_GEM_MMAP                 -> offset=0x%llx size=%llu\n",
           (unsigned long long)gm.offset, (unsigned long long)gm.size);

    uint8_t *map = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
                        (off_t)gm.offset);
    CHECK(map != MAP_FAILED, "mmap(device fd, gem offset) via shim");
    memset(map, 0xA5, 4096);
    CHECK(map[0] == 0xA5 && map[4095] == 0xA5, "mapped GEM is writable");
    printf("mmap(dev_fd, offset)               -> %p (wrote 4096 bytes)\n", map);

    /* 7. PRIME_HANDLE_TO_FD exports the same object as a real dma-buf fd. */
    int dmafd = -1;
    CHECK(drmPrimeHandleToFD(fd, gc.handle, 0, &dmafd) == 0,
          "PRIME_HANDLE_TO_FD");
    CHECK(dmafd >= 0, "exported a dma-buf fd");
    printf("DRM_IOCTL_PRIME_HANDLE_TO_FD       -> dma-buf fd %d\n", dmafd);

    /* The dma-buf and the GEM mapping share storage: read the pattern back
     * through the exported fd's own mapping (zero copies). */
    uint8_t *dmap = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, dmafd, 0);
    CHECK(dmap != MAP_FAILED, "mmap exported dma-buf fd");
    CHECK(dmap[0] == 0xA5 && dmap[2048] == 0xA5,
          "dma-buf mapping sees the bytes written through the GEM mapping");
    printf("mmap(dma-buf fd)                   -> %p (sees 0xA5 written via GEM map)\n",
           dmap);
    munmap(dmap, 4096);

    /* 8. PRIME_FD_TO_HANDLE imports it back; the daemon dedups by inode, so the
     *    import resolves to the object already created. */
    uint32_t imported = 0;
    CHECK(drmPrimeFDToHandle(fd, dmafd, &imported) == 0, "PRIME_FD_TO_HANDLE");
    CHECK(imported == gc.handle, "import dedups to the created handle");
    printf("DRM_IOCTL_PRIME_FD_TO_HANDLE       -> handle=%u (dedup == %u)\n",
           imported, gc.handle);

    /* 9. Close the GEM handle; a later export must fail. */
    CHECK(drmCloseBufferHandle(fd, gc.handle) == 0, "GEM_CLOSE");
    int gone = -1;
    CHECK(drmPrimeHandleToFD(fd, gc.handle, 0, &gone) != 0,
          "export after GEM_CLOSE fails");
    printf("DRM_IOCTL_GEM_CLOSE                -> handle %u released\n", gc.handle);

    munmap(map, 4096);
    close(dmafd);

    /* Optional: keep the render-node connection open for
     * GNDRM_HARNESS_HOLD_MS after the sequence, so a concurrency test can drive
     * a card client against the same daemon while this render client is still
     * connected (the device scenario: gamescope + Xwayland on one daemon). */
    const char *hold = getenv("GNDRM_HARNESS_HOLD_MS");
    if (hold) {
        long ms = strtol(hold, NULL, 10);
        if (ms > 0) {
            struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
            nanosleep(&ts, NULL);
        }
    }

    close(fd);

    printf("== OK: render-node surface exercised through the shim ==\n");
    return 0;
}
