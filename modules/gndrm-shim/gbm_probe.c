/*
 * gbm_probe — local proof that Mesa's gbm_create_device() accepts a
 * gndrm socket-backed render-node fd.
 *
 * Path (identical shape to Xwayland's glamor setup):
 *   open("/dev/dri/renderD128")   -> libgndrm_shim.so (LD_PRELOAD) returns the
 *                                    SOCK_SEQPACKET render-channel fd to the
 *                                    gndrm-hostd daemon
 *   gbm_create_device(fd)         -> Mesa libgbm (_gbm_create_device ->
 *                                    dri_device_create -> driCreateNewScreen3)
 *   eglGetPlatformDisplay(GBM) + eglInitialize  (if reachable)
 *
 * Build (against the patched host libgbm):
 *   cc -Wall -Wextra -Werror -o gbm_probe gbm_probe.c \
 *      -I<mesa>/src/gbm/main -L<build>/src/gbm -lgbm \
 *      -Wl,-rpath,<build>/src/gbm $(pkg-config --cflags --libs egl)
 * Run:
 *   LD_PRELOAD=libgndrm_shim.so GAMENATIVE_DRM_SOCKET=/tmp/...sock \
 *       GNDRM_RENDER_NODE_CLIENT=1 ./gbm_probe
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>
#include <errno.h>
#include <string.h>

#include <gbm.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>

#ifndef EGL_PLATFORM_GBM_KHR
#define EGL_PLATFORM_GBM_KHR 0x31D7
#endif

int
main(void)
{
    const char *node = getenv("GAMENATIVE_DRM_NODE");
    int rc = 0;

    setvbuf(stdout, NULL, _IONBF, 0);

    if (!node || !*node)
        node = "/dev/dri/renderD128";

    int fd = open(node, O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        fprintf(stderr, "open(%s): %s\n", node, strerror(errno));
        return 2;
    }

    struct stat st;
    if (fstat(fd, &st) != 0) {
        fprintf(stderr, "fstat: %s\n", strerror(errno));
        return 2;
    }
    printf("open %s -> fd %d  (st_mode=0%o  S_ISSOCK=%d  S_ISCHR=%d)\n",
           node, fd, (unsigned)st.st_mode,
           S_ISSOCK(st.st_mode) ? 1 : 0, S_ISCHR(st.st_mode) ? 1 : 0);

    printf("calling Mesa gbm_create_device(%d) ...\n", fd);
    struct gbm_device *dev = gbm_create_device(fd);
    printf("gbm_create_device(%d) -> %p\n", fd, (void *)dev);
    if (!dev) {
        printf("FAIL: gbm_create_device returned NULL\n");
        return 1;
    }
    printf("PASS: non-NULL gbm device\n");
    printf("  gbm_device_get_fd          = %d\n", gbm_device_get_fd(dev));
    printf("  gbm_device_get_backend_name= %s\n",
           gbm_device_get_backend_name(dev));

    /* Best-effort EGL, opt-in: needs the gndrm-chosen DRI driver (zink) to
     * initialise, i.e. a working Vulkan driver on the host.  Not a gate. */
    if (!getenv("GBM_PROBE_EGL")) {
        printf("(EGL check skipped; set GBM_PROBE_EGL=1 to attempt)\n");
        gbm_device_destroy(dev);
        printf("done (rc=%d)\n", rc);
        return rc;
    }
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform_display =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)
        eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (get_platform_display) {
        EGLDisplay dpy = get_platform_display(EGL_PLATFORM_GBM_KHR, dev, NULL);
        printf("eglGetPlatformDisplay(GBM, %p) -> %p\n", (void *)dev, (void *)dpy);
        if (dpy != EGL_NO_DISPLAY) {
            EGLint major = 0, minor = 0;
            EGLBoolean ok = eglInitialize(dpy, &major, &minor);
            printf("eglInitialize -> %d (EGL %d.%d)\n", ok, major, minor);
            if (ok) {
                printf("  EGL_VENDOR   = %s\n", eglQueryString(dpy, EGL_VENDOR));
                printf("  EGL_VERSION  = %s\n", eglQueryString(dpy, EGL_VERSION));
            } else {
                printf("(eglInitialize failed, eglGetError=0x%x) -- not a gate\n",
                       eglGetError());
                rc = 0;
            }
            if (ok)
                eglTerminate(dpy);
        }
    } else {
        printf("(no eglGetPlatformDisplayEXT; EGL check skipped)\n");
    }

    gbm_device_destroy(dev);
    printf("done (rc=%d)\n", rc);
    return rc;
}
