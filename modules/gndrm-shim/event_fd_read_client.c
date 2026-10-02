/*
 * Event-channel bound harness.
 *
 * The DRM fd the shim hands the app IS the event channel (a SOCK_SEQPACKET to
 * the daemon). An app that reads it for completion events must not be able to
 * block forever when no producer is present: the shim bounds both channels
 * with SO_RCVTIMEO/SO_SNDTIMEO (GNDRM_RPC_TIMEOUT_SEC), so a read with nothing
 * to deliver returns EAGAIN instead of pinning the caller's thread.
 *
 * This exercises exactly that on the host: open the render node through the
 * shim, do one request to prove the channel is live, then block reading the
 * event fd with no producer and assert the bound fires.
 *
 * Build: cc -Wall -Wextra -Werror -o event_fd_read_client event_fd_read_client.c
 * Run:   LD_PRELOAD=libgndrm_shim.so GAMENATIVE_DRM_SOCKET=/tmp/gndrm.sock \
 *            ./event_fd_read_client
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <drm/drm.h>
#include <sys/ioctl.h>
#include <xf86drm.h>

/* Must match GNDRM_RPC_TIMEOUT_SEC in gndrm_shim.c. */
#define EXPECTED_BOUND_SEC 5

static int64_t now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

int main(void)
{
    printf("== event-channel bound harness ==\n");

    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        fprintf(stderr, "FAIL: open render node (is LD_PRELOAD/GAMENATIVE_DRM_SOCKET set?): %s\n",
                strerror(errno));
        return 1;
    }
    printf("open /dev/dri/renderD128           -> fd %d (event channel)\n", fd);

    /* Prove the channel is live: one request the daemon answers inline. */
    drmVersionPtr ver = drmGetVersion(fd);
    if (ver == NULL) {
        fprintf(stderr, "FAIL: drmGetVersion on the event fd\n");
        return 1;
    }
    printf("DRM_IOCTL_VERSION                  -> name='%s'\n", ver->name);
    drmFreeVersion(ver);

    /* No producer exists on a render node: a blocking recv must hit the bound
     * and fail with EAGAIN, not hang and not report EOF. */
    char buf[64];
    errno = 0;
    int64_t t0 = now_ms();
    ssize_t n = read(fd, buf, sizeof(buf));
    int64_t elapsed = now_ms() - t0;
    int read_errno = errno;

    printf("read(event fd, 64)                 -> n=%zd errno=%d (%s) after %lld ms\n",
           n, read_errno, strerror(read_errno), (long long)elapsed);

    if (n >= 0) {
        fprintf(stderr, "FAIL: read returned %zd without a producer (expected a timeout)\n", n);
        return 1;
    }
    if (read_errno != EAGAIN && read_errno != EWOULDBLOCK) {
        fprintf(stderr, "FAIL: read errno=%d (%s), expected EAGAIN (the fd is unbounded)\n",
                read_errno, strerror(read_errno));
        return 1;
    }

    /* The bound must actually be the configured one, not an immediate error
     * (which would break the healthy blocking path) nor an unbounded wait. */
    if (elapsed < (int64_t)(EXPECTED_BOUND_SEC - 1) * 1000 ||
        elapsed > (int64_t)(EXPECTED_BOUND_SEC + 10) * 1000) {
        fprintf(stderr, "FAIL: read timed out after %lld ms, expected ~%ds\n",
                (long long)elapsed, EXPECTED_BOUND_SEC);
        return 1;
    }

    close(fd);
    printf("== OK: event fd bound fires (EAGAIN at ~%ds), no hang ==\n", EXPECTED_BOUND_SEC);
    return 0;
}
