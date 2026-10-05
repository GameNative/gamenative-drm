/*
 * xtree-probe.c — dump the live X window tree and the attributes of the
 * presenting window, over the guest session's own X connection.
 *
 * Why this exists: under gamescope + `wine explorer /desktop=root` the ICD
 * shim's DRI3/Present route engages on the app's X window, yet gamescope's xwm
 * logs `focus=0x0` and nothing composites. The X map_state of that window
 * (IsUnmapped / IsUnviewable / IsViewable) plus its parent chain is what names
 * the cause, so this client reports exactly that.
 *
 * Connection: the session's X socket is NOT `/tmp/.X11-unix/X0` on Android —
 * it lives in the app sandbox and its path is exposed as GAMESCOPE_X11_DISPLAY
 * (e.g. /data/user/0/app.gamenative/files/imagefs/usr/tmp/.X11-unix/X0). This
 * client opens that socket itself and hands the fd to xcb_connect_to_fd, so it
 * needs no DISPLAY parsing and cannot silently fall back to another server.
 *
 * Usage: xtree-probe [-display <dpy|socket>] [socket-path] [target-xid-hex]
 *   -display <dpy> : display selector. On Android the session's X socket is
 *                    NOT /tmp/.X11-unix/X0 — it lives in the app sandbox and is
 *                    exposed as $GAMESCOPE_X11_DISPLAY, so a bare `:N` selector
 *                    falls back to that env (then $DISPLAY). An absolute-path
 *                    value is used as the socket path directly.
 *   socket-path    : overrides the socket; defaults to $GAMESCOPE_X11_DISPLAY,
 *                    then $DISPLAY.
 * This client opens the socket itself and hands the fd to xcb_connect_to_fd, so
 * a bare `:N` can never silently fall back to another server.
 * Bounded: a connect timeout plus xcb's own request/reply timeouts; it always
 * exits (rc 0 dump printed, 1 connection/usage failure), never blocks forever.
 */

#include <xcb/xcb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <time.h>
#include <sys/socket.h>
#include <sys/un.h>

static const char *map_state_name(uint8_t s)
{
    switch (s) {
    case XCB_MAP_STATE_UNMAPPED:  return "IsUnmapped";
    case XCB_MAP_STATE_UNVIEWABLE:return "IsUnviewable";
    case XCB_MAP_STATE_VIEWABLE:  return "IsViewable";
    default: return "?";
    }
}

static const char *class_name(uint16_t c)
{
    return c == XCB_WINDOW_CLASS_INPUT_OUTPUT ? "InputOutput" : "InputOnly";
}

/* Open the session X socket and perform the setup handshake ourselves. */
static xcb_connection_t *connect_path(const char *path, int *screen_out)
{
    struct sockaddr_un sa;
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) { perror("socket"); return NULL; }

    memset(&sa, 0, sizeof sa);
    sa.sun_family = AF_UNIX;
    if (path[0] == '@') {
        /* abstract namespace */
        size_t n = strlen(path + 1);
        if (n >= sizeof sa.sun_path) { close(fd); fprintf(stderr, "path too long\n"); return NULL; }
        memcpy(sa.sun_path, path + 1, n);
        sa.sun_path[0] = '\0';
    } else {
        if (strlen(path) >= sizeof sa.sun_path) { close(fd); fprintf(stderr, "path too long\n"); return NULL; }
        strncpy(sa.sun_path, path, sizeof sa.sun_path - 1);
    }

    /* bounded connect: never hang a probe on a unresponsive server */
    struct timeval tv = { .tv_sec = 5, .tv_usec = 0 };
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);

    if (connect(fd, (struct sockaddr *)&sa, sizeof sa) < 0) {
        fprintf(stderr, "connect(%s): %s\n", path, strerror(errno));
        close(fd);
        return NULL;
    }

    xcb_connection_t *c = xcb_connect_to_fd(fd, NULL);
    if (!c || xcb_connection_has_error(c)) {
        fprintf(stderr, "xcb_connect_to_fd: failed\n");
        if (c) xcb_disconnect(c); else close(fd);
        return NULL;
    }
    int scr = 0;
    xcb_screen_iterator_t it = xcb_setup_roots_iterator(xcb_get_setup(c));
    *screen_out = scr;
    (void)it;
    return c;
}

static xcb_atom_t intern_pid_atom(xcb_connection_t *c)
{
    static xcb_atom_t a = 0;
    if (!a) {
        xcb_intern_atom_cookie_t ck = xcb_intern_atom(c, 1, 12, "_NET_WM_PID");
        xcb_intern_atom_reply_t *r = xcb_intern_atom_reply(c, ck, NULL);
        if (r) { a = r->atom; free(r); }
    }
    return a;
}

static void print_name_class(xcb_connection_t *c, xcb_window_t w)
{
    xcb_get_property_cookie_t ck = xcb_get_property(c, 0, w, XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 0, 256);
    xcb_get_property_reply_t *r = xcb_get_property_reply(c, ck, NULL);
    if (r && xcb_get_property_value_length(r) > 0) {
        printf("      WM_NAME=\"%.*s\"\n", xcb_get_property_value_length(r),
               (char *)xcb_get_property_value(r));
    }
    if (r) free(r);

    ck = xcb_get_property(c, 0, w, XCB_ATOM_WM_CLASS, XCB_ATOM_STRING, 0, 256);
    r = xcb_get_property_reply(c, ck, NULL);
    if (r && xcb_get_property_value_length(r) > 0) {
        char *p = xcb_get_property_value(r);
        int len = xcb_get_property_value_length(r);
        /* WM_CLASS is two NUL-separated strings: instance\0class\0 */
        printf("      WM_CLASS instance=\"%s\" class=\"%s\"\n", p,
               (strlen(p) + 1 < (size_t)len) ? p + strlen(p) + 1 : "");
    }
    if (r) free(r);

    xcb_atom_t pa = intern_pid_atom(c);
    if (pa) {
        ck = xcb_get_property(c, 0, w, pa, XCB_ATOM_CARDINAL, 0, 1);
        r = xcb_get_property_reply(c, ck, NULL);
        if (r && xcb_get_property_value_length(r) >= 4)
            printf("      _NET_WM_PID=%u\n", *(uint32_t *)xcb_get_property_value(r));
        if (r) free(r);
    }
}

static void print_attrs(xcb_connection_t *c, xcb_window_t w)
{
    xcb_get_window_attributes_cookie_t ac = xcb_get_window_attributes(c, w);
    xcb_get_window_attributes_reply_t *a = xcb_get_window_attributes_reply(c, ac, NULL);
    xcb_get_geometry_cookie_t gc = xcb_get_geometry(c, w);
    xcb_get_geometry_reply_t *g = xcb_get_geometry_reply(c, gc, NULL);

    printf("  window 0x%08x\n", w);
    uint8_t depth = g ? g->depth : 0;
    if (a) {
        const xcb_get_window_attributes_reply_t *ra = a;
        printf("    map_state=%s override_redirect=%u class=%s depth=%u visual=0x%08x\n",
               map_state_name(ra->map_state), ra->override_redirect,
               class_name(ra->_class), depth, ra->visual);
        free(a);
    } else {
        printf("    attributes: <no reply>\n");
    }
    if (g) {
        printf("    geom=(%d,%d) %ux%u border=%u depth=%u root=0x%08x\n",
               g->x, g->y, g->width, g->height, g->border_width, g->depth, g->root);
        free(g);
    }
}

/* depth-first dump, bounded to avoid an infinite loop on a pathological tree */
static void dump_tree(xcb_connection_t *c, xcb_window_t w, int depth, int *budget)
{
    if (depth > 12 || *budget <= 0) return;
    (*budget)--;

    xcb_query_tree_cookie_t ck = xcb_query_tree(c, w);
    xcb_query_tree_reply_t *r = xcb_query_tree_reply(c, ck, NULL);
    int n = r ? xcb_query_tree_children_length(r) : 0;
    printf("%*s0x%08x\n", depth * 2, "", w);
    print_name_class(c, w);
    if (depth == 0) print_attrs(c, w);
    if (r) {
        xcb_window_t *kids = xcb_query_tree_children(r);
        for (int i = 0; i < n; i++) dump_tree(c, kids[i], depth + 1, budget);
        free(r);
    }
}

static void usage(const char *argv0)
{
    fprintf(stderr,
            "usage: %s [-display <dpy|socket>] [socket-path] [target-xid-hex]\n"
            "  -display <dpy> : display selector; a bare `:N` uses\n"
            "                   $GAMESCOPE_X11_DISPLAY then $DISPLAY (the session\n"
            "                   socket is env-provided on Android). An absolute\n"
            "                   path is used as the socket path directly.\n",
            argv0);
}

int main(int argc, char **argv)
{
    const char *path = NULL;
    const char *display = NULL;
    xcb_window_t target = 0;
    int have_target = 0;

    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-display") || !strcmp(argv[i], "--display")) {
            if (++i >= argc) {
                fprintf(stderr, "%s: -display needs a value\n", argv[0]);
                return 1;
            }
            display = argv[i];
        } else if (argv[i][0] == '-' && argv[i][1] != '\0') {
            fprintf(stderr, "%s: unknown option '%s'\n", argv[0], argv[i]);
            usage(argv[0]);
            return 1;
        } else if (!path) {
            path = argv[i];
        } else if (!have_target) {
            target = (xcb_window_t)strtoul(argv[i], NULL, 16);
            have_target = 1;
        } else {
            fprintf(stderr, "%s: too many arguments\n", argv[0]);
            usage(argv[0]);
            return 1;
        }
    }

    /* An explicit display that is a socket path is that path; otherwise the
     * session socket is env-provided (Android's X socket is not /tmp/.X11-unix). */
    if (!path && display && display[0] == '/') path = display;
    if (!path) path = getenv("GAMESCOPE_X11_DISPLAY");
    if (!path) path = getenv("DISPLAY");
    if (!path || !path[0]) {
        fprintf(stderr,
                "%s: no X socket (pass a socket path, or set "
                "$GAMESCOPE_X11_DISPLAY / $DISPLAY)\n",
                argv[0]);
        usage(argv[0]);
        return 1;
    }
    if (display && display[0] != '/')
        fprintf(stderr, "xtree-probe: -display %s -> session socket %s\n",
                display, path);

    int scr = 0;
    xcb_connection_t *c = connect_path(path, &scr);
    if (!c) return 1;

    const xcb_setup_t *setup = xcb_get_setup(c);
    xcb_screen_iterator_t it = xcb_setup_roots_iterator(setup);
    xcb_screen_t *screen = it.data;
    printf("xtree-probe: connected to %s\n", path);
    printf("  setup: vendor=\"%.*s\" %ux%u image-order=%u bitmap-order=%u\n",
           xcb_setup_vendor_length(setup), xcb_setup_vendor(setup),
           screen->width_in_pixels, screen->height_in_pixels,
           setup->image_byte_order, setup->bitmap_format_bit_order);
    printf("  root=0x%08x depth=%u\n", screen->root, screen->root_depth);

    /* extension presence, since the shim's route depends on DRI3/Present */
    xcb_query_extension_cookie_t ec = xcb_query_extension(c, 4, "DRI3");
    xcb_query_extension_reply_t *er = xcb_query_extension_reply(c, ec, NULL);
    printf("  DRI3  present=%u opcode=%u\n", er ? er->present : 0, er ? er->major_opcode : 0);
    if (er) free(er);
    ec = xcb_query_extension(c, 7, "Present");
    er = xcb_query_extension_reply(c, ec, NULL);
    printf("  Present present=%u opcode=%u\n", er ? er->present : 0, er ? er->major_opcode : 0);
    if (er) free(er);

    printf("--- tree (depth-first from root) ---\n");
    int budget = 500;
    dump_tree(c, screen->root, 0, &budget);

    /* gamescope's compositor focus: the WM's _NET_ACTIVE_WINDOW on the root,
     * and the X input focus. This names which window the compositor is
     * actually painting, independent of the override target. */
    {
        xcb_atom_t aw = 0;
        xcb_intern_atom_cookie_t c1 = xcb_intern_atom(c, 1, 15, "_NET_ACTIVE_WINDOW");
        xcb_intern_atom_reply_t *r1 = xcb_intern_atom_reply(c, c1, NULL);
        if (r1) { aw = r1->atom; free(r1); }
        if (aw) {
            xcb_get_property_cookie_t ck = xcb_get_property(c, 0, screen->root, aw, XCB_ATOM_WINDOW, 0, 1);
            xcb_get_property_reply_t *r = xcb_get_property_reply(c, ck, NULL);
            if (r && xcb_get_property_value_length(r) >= 4)
                printf("  _NET_ACTIVE_WINDOW=0x%08x\n", *(uint32_t *)xcb_get_property_value(r));
            else
                printf("  _NET_ACTIVE_WINDOW=<unset>\n");
            if (r) free(r);
        }
        xcb_get_input_focus_cookie_t fc = xcb_get_input_focus(c);
        xcb_get_input_focus_reply_t *fr = xcb_get_input_focus_reply(c, fc, NULL);
        if (fr) {
            printf("  X input focus=0x%08x\n", fr->focus);
            free(fr);
        }
    }

    if (target) {
        printf("--- target 0x%08x: attributes + upward parent chain ---\n", target);
        print_attrs(c, target);
        print_name_class(c, target);
        xcb_window_t w = target;
        for (int i = 0; i < 16; i++) {
            xcb_query_tree_cookie_t ck = xcb_query_tree(c, w);
            xcb_query_tree_reply_t *r = xcb_query_tree_reply(c, ck, NULL);
            if (!r) { printf("  parent of 0x%08x: <query failed / gone>\n", w); break; }
            xcb_window_t parent = r->parent;
            free(r);
            if (parent == 0 || parent == XCB_NONE) break;
            printf("  parent 0x%08x\n", parent);
            print_attrs(c, parent);
            print_name_class(c, parent);
            if (parent == screen->root) { printf("  (reached root)\n"); break; }
            w = parent;
        }
    }

    xcb_disconnect(c);
    return 0;
}
