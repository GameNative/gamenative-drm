/*
 * scaling-mode.c — set gamescope's live scaling-mode root property over the
 * session's own X connection.
 *
 * Why a separate process: the action is a plain X client write, but it must not
 * run inside the app process. A short-lived guest process exits with a status
 * and cannot take the app down, so the live control is isolated by construction.
 *
 * gamescope reads the setting live from the root property
 * GAMESCOPE_NEW_SCALING_SCALER (steamcompmgr.cpp PropertyNotify handler): a
 * single 32-bit XA_CARDINAL whose value is the GamescopeUpscaleScaler enum
 * (AUTO=0, INTEGER=1, FIT=2, FILL=3, STRETCH=4). The command line `--scaler`
 * only seeds the value at launch; this property changes it afterwards without a
 * relaunch. It is deliberately NOT GAMESCOPE_SCALING_FILTER — that is the
 * legacy combined (scaler+filter) property read as a mode 0..4; the scaler
 * alone is the *_NEW_SCALING_SCALER atom.
 *
 * Connection: like gn-xtree-probe, the session's X socket is not
 * /tmp/.X11-unix/X0 on Android — it lives in the app sandbox and is exposed as
 * $GAMESCOPE_X11_DISPLAY. This client opens that socket itself and hands the fd
 * to xcb_connect_to_fd, so it needs no DISPLAY parsing and cannot silently fall
 * back to another server. The Xwayland here accepts a no-auth setup, so no
 * XAUTHORITY is required.
 *
 * Usage: gn-scaling-mode <mode-value>
 *   mode-value : GamescopeUpscaleScaler value 0..4 (AUTO=0 INTEGER=1 FIT=2
 *                FILL=3 STRETCH=4).
 *   socket     : $GAMESCOPE_X11_DISPLAY, else $DISPLAY.
 * Exit: 0 = property set and read back equal; 1 = usage/connection/property
 *       failure. The exit status is the result the caller surfaces.
 */

#include <xcb/xcb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/time.h>

#define SCALER_ATOM "GAMESCOPE_NEW_SCALING_SCALER"
#define SCALER_MODE_MAX 4

static const char *mode_name(unsigned long v)
{
    switch (v) {
    case 0: return "auto";
    case 1: return "integer";
    case 2: return "fit";
    case 3: return "fill";
    case 4: return "stretch";
    default: return "?";
    }
}

/* Open the session X socket and perform the setup handshake ourselves. */
static xcb_connection_t *connect_path(const char *path)
{
    struct sockaddr_un sa;
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) { perror("scaling-mode: socket"); return NULL; }

    memset(&sa, 0, sizeof sa);
    sa.sun_family = AF_UNIX;
    if (strlen(path) >= sizeof sa.sun_path) {
        fprintf(stderr, "scaling-mode: socket path too long: %s\n", path);
        close(fd);
        return NULL;
    }
    strncpy(sa.sun_path, path, sizeof sa.sun_path - 1);

    /* bounded: a stalled server must never hang the caller */
    struct timeval tv = { .tv_sec = 2, .tv_usec = 0 };
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);

    if (connect(fd, (struct sockaddr *)&sa, sizeof sa) < 0) {
        fprintf(stderr, "scaling-mode: connect(%s): %s\n", path, strerror(errno));
        close(fd);
        return NULL;
    }

    xcb_connection_t *c = xcb_connect_to_fd(fd, NULL);
    if (!c || xcb_connection_has_error(c)) {
        fprintf(stderr, "scaling-mode: xcb_connect_to_fd: failed\n");
        if (c) xcb_disconnect(c); else close(fd);
        return NULL;
    }
    return c;
}

static void usage(const char *argv0)
{
    fprintf(stderr,
            "usage: %s <mode-value>\n"
            "  mode-value 0..4 (AUTO=0 INTEGER=1 FIT=2 FILL=3 STRETCH=4)\n"
            "  socket from $GAMESCOPE_X11_DISPLAY, else $DISPLAY\n",
            argv0);
}

int main(int argc, char **argv)
{
    if (argc != 2) { usage(argv[0]); return 1; }

    char *end = NULL;
    unsigned long mode = strtoul(argv[1], &end, 10);
    if (end == argv[1] || *end != '\0' || mode > SCALER_MODE_MAX) {
        fprintf(stderr, "scaling-mode: bad mode '%s' (want 0..%d)\n", argv[1], SCALER_MODE_MAX);
        return 1;
    }

    const char *path = getenv("GAMESCOPE_X11_DISPLAY");
    if (!path || !path[0]) path = getenv("DISPLAY");
    if (!path || !path[0]) {
        fprintf(stderr, "scaling-mode: no X socket ($GAMESCOPE_X11_DISPLAY / $DISPLAY unset)\n");
        return 1;
    }

    xcb_connection_t *c = connect_path(path);
    if (!c) return 1;

    xcb_screen_t *screen = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    if (!screen) {
        fprintf(stderr, "scaling-mode: no screen\n");
        xcb_disconnect(c);
        return 1;
    }

    xcb_intern_atom_cookie_t ac = xcb_intern_atom(c, 1, strlen(SCALER_ATOM), SCALER_ATOM);
    xcb_intern_atom_reply_t *ar = xcb_intern_atom_reply(c, ac, NULL);
    if (!ar) {
        fprintf(stderr, "scaling-mode: InternAtom failed\n");
        xcb_disconnect(c);
        return 1;
    }
    xcb_atom_t atom = ar->atom;
    free(ar);

    uint32_t value = (uint32_t)mode;
    xcb_void_cookie_t ck = xcb_change_property_checked(
        c, XCB_PROP_MODE_REPLACE, screen->root, atom, XCB_ATOM_CARDINAL, 32, 1, &value);
    xcb_generic_error_t *err = xcb_request_check(c, ck);
    xcb_flush(c);
    if (err) {
        fprintf(stderr, "scaling-mode: ChangeProperty error %u\n", err->error_code);
        free(err);
        xcb_disconnect(c);
        return 1;
    }

    /* Read back so a silently-dropped or wrong-typed property is a failure,
     * not a dead control. */
    xcb_get_property_cookie_t gc =
        xcb_get_property(c, 0, screen->root, atom, XCB_ATOM_CARDINAL, 0, 1);
    xcb_get_property_reply_t *r = xcb_get_property_reply(c, gc, NULL);
    int rc = 1;
    if (r && xcb_get_property_value_length(r) >= 4) {
        uint32_t got = *(uint32_t *)xcb_get_property_value(r);
        printf("scaling-mode: %s(%lu) -> GAMESCOPE_NEW_SCALING_SCALER=%u\n",
               mode_name(mode), mode, got);
        rc = (got == value) ? 0 : 1;
    } else {
        fprintf(stderr, "scaling-mode: read-back failed (property not a 32-bit cardinal)\n");
    }
    if (r) free(r);
    xcb_disconnect(c);
    return rc;
}
