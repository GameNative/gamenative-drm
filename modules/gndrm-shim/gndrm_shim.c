/*
 * gndrm-shim — LD_PRELOAD ioctl interceptor for gamescope.
 *
 * Presents a fake /dev/dri/card* whose fd is a connected Unix socket pair to
 * the gamenative-drm daemon (doc 02-gamenative-drm.md §4/§6):
 *
 *   - open("/dev/dri/cardN")  → two SOCK_SEQPACKET connections to the daemon;
 *                               the *event* connection fd is returned to the
 *                               caller (read/poll on it behave exactly like a
 *                               real DRM fd delivering drm_event bytes).
 *   - ioctl(fd, ...)          → marshalled into one RPC frame on the hidden
 *                               RPC connection; the response bytes are
 *                               de-linearized back into the caller's struct
 *                               and pointer-array arguments, exactly as the
 *                               kernel would have written them.
 *   - close(fd)               → closes both channels.
 *
 * read/poll/epoll/fcntl are NOT intercepted: the returned fd is a real
 * socket, so the kernel handles them natively.
 *
 * No glibc-isms beyond RTLD_NEXT dlsym; builds with bionic (NDK) and glibc.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <dlfcn.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/un.h>
#include <unistd.h>

#include <drm/drm.h>
#include <drm/drm_mode.h>

/* ------------------------------------------------------------------ */
/* wire protocol (mirrors gndrm-uapi/src/wire.rs)                      */
/* ------------------------------------------------------------------ */

#define GNDRM_MAGIC 0x474E4431u /* "GND1" */
#define GNDRM_FLAG_FD (1u << 0)
#define GNDRM_MAX_FRAME (1u << 20)
#define GNDRM_REQ_HDR 24
#define GNDRM_RESP_HDR 24

struct gndrm_req {
    uint32_t magic;
    uint32_t total_len;
    uint32_t ioctl_nr;
    uint32_t flags;
    uint64_t seq;
} __attribute__((packed));

struct gndrm_resp {
    uint32_t magic;
    uint32_t total_len;
    uint64_t seq;
    int32_t ret;
    uint32_t out_len;
} __attribute__((packed));

/* ------------------------------------------------------------------ */
/* shim state                                                          */
/* ------------------------------------------------------------------ */

static int g_rpc_fd = -1;    /* hidden request/response channel */
static int g_event_fd = -1;  /* fd handed to the app (event channel) */
static uint64_t g_seq;
static pthread_mutex_t g_rpc_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_once_t g_init_once = PTHREAD_ONCE_INIT;
static int g_init_ok;
static uint32_t g_in_fence_prop; /* discovered at init, 0 = unsupported */

static int (*real_open)(const char *, int, ...) = NULL;
static int (*real_open64)(const char *, int, ...) = NULL;
static int (*real_openat)(int, const char *, int, ...) = NULL;
static int (*real_close)(int) = NULL;
static ssize_t (*real_read)(int, void *, size_t) = NULL;

#define LOG(...)                       \
    do {                               \
        if (getenv("GNDRM_SHIM_DEBUG")) \
            fprintf(stderr, "[gndrm-shim] " __VA_ARGS__); \
    } while (0)

/* ------------------------------------------------------------------ */
/* daemon connection                                                   */
/* ------------------------------------------------------------------ */

static int connect_one(const char *path)
{
    int fd = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    if (fd < 0)
        return -1;

    struct sockaddr_un sa;
    memset(&sa, 0, sizeof(sa));
    sa.sun_family = AF_UNIX;
    if (path[0] == '@') { /* abstract namespace */
        sa.sun_path[0] = '\0';
        strncpy(sa.sun_path + 1, path + 1, sizeof(sa.sun_path) - 2);
    } else {
        strncpy(sa.sun_path, path, sizeof(sa.sun_path) - 1);
    }
    if (connect(fd, (struct sockaddr *)&sa, sizeof(sa)) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

/*
 * One-time property discovery: find the plane property named IN_FENCE_FD so
 * the ATOMIC marshaler knows which u64 value is actually an fd to send.
 * Talks the wire protocol directly (the shim's own ioctls would recurse).
 */
static void discover_in_fence_prop(void);

static void init_shim(void)
{
    real_open = dlsym(RTLD_NEXT, "open");
    real_open64 = dlsym(RTLD_NEXT, "open64");
    real_openat = dlsym(RTLD_NEXT, "openat");
    real_close = dlsym(RTLD_NEXT, "close");
    real_read = dlsym(RTLD_NEXT, "read");

    const char *path = getenv("GAMENATIVE_DRM_SOCKET");
    if (!path || !*path) {
        LOG("GAMENATIVE_DRM_SOCKET not set\n");
        return;
    }
    /* First connection = RPC, second = event channel (wire contract). */
    g_rpc_fd = connect_one(path);
    g_event_fd = connect_one(path);
    if (g_rpc_fd < 0 || g_event_fd < 0) {
        LOG("connect(%s) failed: %s\n", path, strerror(errno));
        if (g_rpc_fd >= 0)
            close(g_rpc_fd);
        if (g_event_fd >= 0)
            close(g_event_fd);
        g_rpc_fd = g_event_fd = -1;
        return;
    }
    g_init_ok = 1;
    discover_in_fence_prop();
    LOG("connected to %s (rpc=%d event=%d) in_fence prop=%u\n",
        path, g_rpc_fd, g_event_fd, g_in_fence_prop);
}

static bool ensure_init(void)
{
    pthread_once(&g_init_once, init_shim);
    return g_init_ok != 0;
}

/* ------------------------------------------------------------------ */
/* RPC exchange (caller must hold g_rpc_lock)                          */
/* ------------------------------------------------------------------ */

static int send_frame(uint32_t ioctl_nr, const void *payload, uint32_t len,
                      int fd_to_send, uint64_t seq)
{
    struct gndrm_req hdr = {
        .magic = GNDRM_MAGIC,
        .total_len = GNDRM_REQ_HDR + len,
        .ioctl_nr = ioctl_nr,
        .flags = fd_to_send >= 0 ? GNDRM_FLAG_FD : 0,
        .seq = seq,
    };
    struct iovec iov[2] = {
        { .iov_base = &hdr, .iov_len = sizeof(hdr) },
        { .iov_base = (void *)payload, .iov_len = len },
    };
    struct msghdr msg = { 0 };
    msg.msg_iov = iov;
    msg.msg_iovlen = 2;

    char cbuf[CMSG_SPACE(sizeof(int))];
    if (fd_to_send >= 0) {
        msg.msg_control = cbuf;
        msg.msg_controllen = sizeof(cbuf);
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &fd_to_send, sizeof(int));
    }
    if (sendmsg(g_rpc_fd, &msg, 0) < 0)
        return -errno;
    return 0;
}

static int recv_frame(uint8_t *buf, uint32_t *len_io, int *fd_out, uint64_t seq)
{
    struct msghdr msg = { 0 };
    struct iovec iov = { .iov_base = buf, .iov_len = GNDRM_MAX_FRAME };
    char cbuf[CMSG_SPACE(sizeof(int))];
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf;
    msg.msg_controllen = sizeof(cbuf);

    ssize_t n = recvmsg(g_rpc_fd, &msg, 0);
    if (n < 0)
        return -errno;
    if ((size_t)n < GNDRM_RESP_HDR)
        return -EPROTO;

    struct gndrm_resp hdr;
    memcpy(&hdr, buf, sizeof(hdr));
    if (hdr.magic != GNDRM_MAGIC || hdr.seq != seq)
        return -EPROTO;
    *len_io = (uint32_t)n - GNDRM_RESP_HDR;
    /* Drop the header in place: callers see the payload at offset 0. */
    memmove(buf, buf + GNDRM_RESP_HDR, *len_io);

    *fd_out = -1;
    if (msg.msg_controllen >= CMSG_LEN(sizeof(int))) {
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        if (c && c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS)
            memcpy(fd_out, CMSG_DATA(c), sizeof(int));
    }
    return hdr.ret; /* 0 or -errno */
}

/*
 * Full round trip. `out` receives the frame body (response struct +
 * linearized extensions); *out_len set on success. Returns 0 or -errno.
 */
static int rpc(uint32_t ioctl_nr, const void *in, uint32_t in_len,
               uint8_t *out, uint32_t *out_len, int fd_send, int *fd_recv)
{
    int ret;
    pthread_mutex_lock(&g_rpc_lock);
    uint64_t seq = ++g_seq;
    ret = send_frame(ioctl_nr, in, in_len, fd_send, seq);
    if (ret == 0)
        ret = recv_frame(out, out_len, fd_recv, seq);
    pthread_mutex_unlock(&g_rpc_lock);
    return ret;
}

/* Scratch buffers (protected by g_rpc_lock via rpc()). */
static uint8_t g_req_buf[GNDRM_MAX_FRAME / 4];
static uint8_t g_resp_buf[GNDRM_MAX_FRAME / 4];

/* ------------------------------------------------------------------ */
/* IN_FENCE_FD discovery (uses rpc() against the daemon's own tables)  */
/* ------------------------------------------------------------------ */

#define GNDRM_PLANE_PRIMARY_ID 0x43 /* daemon contract (objects.rs) */

static void discover_in_fence_prop(void)
{
    struct drm_mode_obj_get_properties gp = {
        .obj_id = GNDRM_PLANE_PRIMARY_ID,
        .obj_type = DRM_MODE_OBJECT_PLANE,
    };
    uint32_t out_len = 0;
    if (rpc(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &gp, sizeof(gp),
            g_resp_buf, &out_len, -1, &(int){-1}) != 0)
        return;
    struct drm_mode_obj_get_properties *ogp = (void *)g_resp_buf;
    uint32_t n = ogp->count_props;
    if (n > 64)
        return;
    /* second call: fetch the id list */
    uint32_t ids[64];
    memset(ids, 0, sizeof(ids));
    gp.props_ptr = (uintptr_t)ids; /* daemon ignores ptr; capacity matters */
    gp.count_props = n;
    if (rpc(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &gp, sizeof(gp),
            g_resp_buf, &out_len, -1, &(int){-1}) != 0)
        return;
    memcpy(ids, g_resp_buf + sizeof(*ogp), n * sizeof(uint32_t));

    for (uint32_t i = 0; i < n; i++) {
        struct drm_mode_get_property pr = { .prop_id = ids[i] };
        if (rpc(DRM_IOCTL_MODE_GETPROPERTY, &pr, sizeof(pr),
                g_resp_buf, &out_len, -1, &(int){-1}) != 0)
            continue;
        struct drm_mode_get_property *rp = (void *)g_resp_buf;
        if (strncmp(rp->name, "IN_FENCE_FD", DRM_PROP_NAME_LEN) == 0) {
            g_in_fence_prop = ids[i];
            return;
        }
    }
}

/* ------------------------------------------------------------------ */
/* path / fd matching                                                  */
/* ------------------------------------------------------------------ */

static bool is_drm_card_path(const char *path)
{
    return path && strncmp(path, "/dev/dri/card", 13) == 0;
}

static bool is_our_fd(int fd)
{
    return fd >= 0 && fd == g_event_fd;
}

/* ------------------------------------------------------------------ */
/* open family                                                         */
/* ------------------------------------------------------------------ */

static bool g_opened; /* single open at a time (phase-1 contract) */

/*
 * Returns the event-channel fd, or -1 with errno set. Only called when the
 * path matches a DRM card *and* GAMENATIVE_DRM_SOCKET is set; real opens
 * never see card paths in that configuration.
 */
static int open_common(void)
{
    if (!ensure_init()) {
        errno = ECONNREFUSED;
        return -1;
    }
    if (g_opened) {
        errno = EBUSY;
        return -1;
    }
    g_opened = true;
    return g_event_fd;
}

/*
 * Interception rule: a card path + configured socket ⇒ always intercept
 * (even on hosts with a real /dev/dri, e.g. the CI box). Without the env
 * var, fall through to the real open untouched.
 */
static bool should_intercept(const char *path)
{
    if (!is_drm_card_path(path))
        return false;
    const char *sock = getenv("GAMENATIVE_DRM_SOCKET");
    return sock && *sock;
}

int open(const char *path, int flags, ...)
{
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list ap;
        va_start(ap, flags);
        mode = va_arg(ap, mode_t);
        va_end(ap);
    }
    if (should_intercept(path))
        return open_common();
    return real_open(path, flags, mode);
}

int open64(const char *path, int flags, ...)
{
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list ap;
        va_start(ap, flags);
        mode = va_arg(ap, mode_t);
        va_end(ap);
    }
    if (should_intercept(path))
        return open_common();
    return real_open64 ? real_open64(path, flags, mode)
                       : real_open(path, flags, mode);
}

int openat(int dirfd, const char *path, int flags, ...)
{
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list ap;
        va_start(ap, flags);
        mode = va_arg(ap, mode_t);
        va_end(ap);
    }
    if (should_intercept(path))
        return open_common();
    return real_openat(dirfd, path, flags, mode);
}

int close(int fd)
{
    if (is_our_fd(fd)) {
        real_close(g_rpc_fd);
        real_close(g_event_fd);
        g_rpc_fd = g_event_fd = -1;
        g_opened = false;
        /* note: pthread_once keeps the (now dead) connection state; a
         * second open in the same process fails with ECONNREFUSED — fine
         * for gamescope's one-open lifetime (phase-1 contract). */
        return 0;
    }
    return real_close(fd);
}

/* ------------------------------------------------------------------ */
/* ioctl marshaling helpers                                            */
/* ------------------------------------------------------------------ */

/* Linearized extension walker: `ext` points past the fixed struct in the
 * response body; each helper consumes the next array. */
struct ext_reader {
    const uint8_t *p;
    const uint8_t *end;
};

static bool ext_take(struct ext_reader *r, void *dst, size_t len)
{
    if ((size_t)(r->end - r->p) < len)
        return false;
    memcpy(dst, r->p, len);
    r->p += len;
    return true;
}

/* Copy a returned u32/u64 array to the caller's buffer, if it offered one. */
static void ext_copy_array(struct ext_reader *r, uintptr_t user_ptr,
                           uint32_t offered, size_t elem_size)
{
    if (!user_ptr || !offered)
        return;
    ext_take(r, (void *)user_ptr, (size_t)offered * elem_size);
}

/* ------------------------------------------------------------------ */
/* ioctl                                                               */
/* ------------------------------------------------------------------ */

static int drm_ioctl(int fd, unsigned long req, void *arg)
{
    (void)fd; /* fd validity already established by the dispatcher */
    int ret, fd_recv = -1, fd_send = -1;
    uint32_t out_len = 0;
    uint32_t in_len = 0;

    switch (req) {
    case DRM_IOCTL_VERSION: {
        struct drm_version v = *(struct drm_version *)arg;
        size_t name_in = v.name_len, date_in = v.date_len, desc_in = v.desc_len;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        struct drm_version *rv = (void *)g_resp_buf;
        struct ext_reader r = { g_resp_buf + sizeof(*rv), g_resp_buf + out_len };
        if (v.name && name_in)
            ext_take(&r, v.name, name_in < rv->name_len ? name_in : rv->name_len);
        else
            r.p += rv->name_len;
        if (v.date && date_in)
            ext_take(&r, v.date, date_in < rv->date_len ? date_in : rv->date_len);
        else
            r.p += rv->date_len;
        if (v.desc && desc_in)
            ext_take(&r, v.desc, desc_in < rv->desc_len ? desc_in : rv->desc_len);
        else
            r.p += rv->desc_len;
        /* kernel semantics: report full lengths */
        v.version_major = rv->version_major;
        v.version_minor = rv->version_minor;
        v.version_patchlevel = rv->version_patchlevel;
        v.name_len = rv->name_len;
        v.date_len = rv->date_len;
        v.desc_len = rv->desc_len;
        *(struct drm_version *)arg = v;
        break;
    }
    case DRM_IOCTL_GET_CAP:
    case DRM_IOCTL_SET_CLIENT_CAP:
    case DRM_IOCTL_GEM_CLOSE:
    case DRM_IOCTL_MODE_RMFB:
    case DRM_IOCTL_MODE_DESTROYPROPBLOB:
    case DRM_IOCTL_MODE_GETENCODER:
    case DRM_IOCTL_SET_MASTER:
    case DRM_IOCTL_DROP_MASTER: {
        size_t len = _IOC_SIZE(req);
        ret = rpc(req, arg, (uint32_t)len, g_resp_buf, &out_len, -1, &fd_recv);
        if (ret == 0 && out_len && out_len <= len)
            memcpy(arg, g_resp_buf, out_len);
        else if (ret == 0 && out_len > len)
            memcpy(arg, g_resp_buf, len);
        break;
    }
    case DRM_IOCTL_MODE_GETRESOURCES: {
        struct drm_mode_card_res v = *(struct drm_mode_card_res *)arg;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        struct drm_mode_card_res *rv = (void *)g_resp_buf;
        struct ext_reader r = { g_resp_buf + sizeof(*rv), g_resp_buf + out_len };
        /* daemon sends min(offered, full) entries per array */
        uint32_t nf = v.count_fbs ? (v.count_fbs < rv->count_fbs ? v.count_fbs : rv->count_fbs) : 0;
        uint32_t nc = v.count_crtcs ? (v.count_crtcs < rv->count_crtcs ? v.count_crtcs : rv->count_crtcs) : 0;
        uint32_t nco = v.count_connectors ? (v.count_connectors < rv->count_connectors ? v.count_connectors : rv->count_connectors) : 0;
        uint32_t ne = v.count_encoders ? (v.count_encoders < rv->count_encoders ? v.count_encoders : rv->count_encoders) : 0;
        ext_copy_array(&r, v.fb_id_ptr, nf, 4);
        ext_copy_array(&r, v.crtc_id_ptr, nc, 4);
        ext_copy_array(&r, v.connector_id_ptr, nco, 4);
        ext_copy_array(&r, v.encoder_id_ptr, ne, 4);
        v.count_fbs = rv->count_fbs;
        v.count_crtcs = rv->count_crtcs;
        v.count_connectors = rv->count_connectors;
        v.count_encoders = rv->count_encoders;
        v.min_width = rv->min_width;
        v.max_width = rv->max_width;
        v.min_height = rv->min_height;
        v.max_height = rv->max_height;
        *(struct drm_mode_card_res *)arg = v;
        break;
    }
    case DRM_IOCTL_MODE_GETCRTC: {
        struct drm_mode_crtc v = *(struct drm_mode_crtc *)arg;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        struct drm_mode_crtc *rv = (void *)g_resp_buf;
        struct ext_reader r = { g_resp_buf + sizeof(*rv), g_resp_buf + out_len };
        uint32_t n = v.count_connectors && rv->count_connectors
            ? (v.count_connectors < rv->count_connectors ? v.count_connectors : rv->count_connectors)
            : 0;
        ext_copy_array(&r, v.set_connectors_ptr, n, 4);
        v.count_connectors = rv->count_connectors;
        v.fb_id = rv->fb_id;
        v.x = rv->x;
        v.y = rv->y;
        v.gamma_size = rv->gamma_size;
        v.mode_valid = rv->mode_valid;
        v.mode = rv->mode;
        *(struct drm_mode_crtc *)arg = v;
        break;
    }
    case DRM_IOCTL_MODE_GETCONNECTOR: {
        struct drm_mode_get_connector v = *(struct drm_mode_get_connector *)arg;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        struct drm_mode_get_connector *rv = (void *)g_resp_buf;
        struct ext_reader r = { g_resp_buf + sizeof(*rv), g_resp_buf + out_len };
        /* array order on the wire: encoders, modes, props, prop_values */
        uint32_t ne = v.count_encoders < rv->count_encoders ? v.count_encoders : rv->count_encoders;
        uint32_t nm = v.count_modes < rv->count_modes ? v.count_modes : rv->count_modes;
        uint32_t np = v.count_props < rv->count_props ? v.count_props : rv->count_props;
        if (v.encoders_ptr)
            ext_copy_array(&r, v.encoders_ptr, v.count_encoders ? ne : 0, 4);
        if (v.modes_ptr)
            ext_copy_array(&r, v.modes_ptr, v.count_modes ? nm : 0, sizeof(struct drm_mode_modeinfo));
        if (v.props_ptr)
            ext_copy_array(&r, v.props_ptr, v.count_props ? np : 0, 4);
        if (v.prop_values_ptr)
            ext_copy_array(&r, v.prop_values_ptr, v.count_props ? np : 0, 8);
        v.count_encoders = rv->count_encoders;
        v.count_modes = rv->count_modes;
        v.count_props = rv->count_props;
        v.encoder_id = rv->encoder_id;
        v.connector_type = rv->connector_type;
        v.connector_type_id = rv->connector_type_id;
        v.connection = rv->connection;
        v.mm_width = rv->mm_width;
        v.mm_height = rv->mm_height;
        v.subpixel = rv->subpixel;
        *(struct drm_mode_get_connector *)arg = v;
        break;
    }
    case DRM_IOCTL_MODE_GETPLANERESOURCES: {
        struct drm_mode_get_plane_res v = *(struct drm_mode_get_plane_res *)arg;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        struct drm_mode_get_plane_res *rv = (void *)g_resp_buf;
        struct ext_reader r = { g_resp_buf + sizeof(*rv), g_resp_buf + out_len };
        uint32_t n = v.count_planes < rv->count_planes ? v.count_planes : rv->count_planes;
        if (v.plane_id_ptr)
            ext_copy_array(&r, v.plane_id_ptr, v.count_planes ? n : 0, 4);
        v.count_planes = rv->count_planes;
        *(struct drm_mode_get_plane_res *)arg = v;
        break;
    }
    case DRM_IOCTL_MODE_GETPLANE: {
        struct drm_mode_get_plane v = *(struct drm_mode_get_plane *)arg;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        struct drm_mode_get_plane *rv = (void *)g_resp_buf;
        struct ext_reader r = { g_resp_buf + sizeof(*rv), g_resp_buf + out_len };
        uint32_t n = v.count_format_types < rv->count_format_types
            ? v.count_format_types : rv->count_format_types;
        if (v.format_type_ptr)
            ext_copy_array(&r, v.format_type_ptr, v.count_format_types ? n : 0, 4);
        v.count_format_types = rv->count_format_types;
        v.crtc_id = rv->crtc_id;
        v.fb_id = rv->fb_id;
        v.possible_crtcs = rv->possible_crtcs;
        v.gamma_size = rv->gamma_size;
        *(struct drm_mode_get_plane *)arg = v;
        break;
    }
    case DRM_IOCTL_MODE_OBJ_GETPROPERTIES: {
        struct drm_mode_obj_get_properties v = *(struct drm_mode_obj_get_properties *)arg;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        struct drm_mode_obj_get_properties *rv = (void *)g_resp_buf;
        struct ext_reader r = { g_resp_buf + sizeof(*rv), g_resp_buf + out_len };
        uint32_t n = v.count_props < rv->count_props ? v.count_props : rv->count_props;
        if (v.props_ptr)
            ext_copy_array(&r, v.props_ptr, v.count_props ? n : 0, 4);
        if (v.prop_values_ptr)
            ext_copy_array(&r, v.prop_values_ptr, v.count_props ? n : 0, 8);
        v.count_props = rv->count_props;
        *(struct drm_mode_obj_get_properties *)arg = v;
        break;
    }
    case DRM_IOCTL_MODE_GETPROPERTY: {
        struct drm_mode_get_property v = *(struct drm_mode_get_property *)arg;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        struct drm_mode_get_property *rv = (void *)g_resp_buf;
        struct ext_reader r = { g_resp_buf + sizeof(*rv), g_resp_buf + out_len };
        uint32_t nv = v.count_values < rv->count_values ? v.count_values : rv->count_values;
        uint32_t nen = v.count_enum_blobs < rv->count_enum_blobs ? v.count_enum_blobs : rv->count_enum_blobs;
        if (v.values_ptr)
            ext_copy_array(&r, v.values_ptr, v.count_values ? nv : 0, 8);
        if (v.enum_blob_ptr)
            ext_copy_array(&r, v.enum_blob_ptr, v.count_enum_blobs ? nen : 0,
                           sizeof(struct drm_mode_property_enum));
        v.count_values = rv->count_values;
        v.count_enum_blobs = rv->count_enum_blobs;
        v.flags = rv->flags;
        memcpy(v.name, rv->name, DRM_PROP_NAME_LEN);
        *(struct drm_mode_get_property *)arg = v;
        break;
    }
    case DRM_IOCTL_MODE_OBJ_SETPROPERTY: {
        ret = rpc(req, arg, sizeof(struct drm_mode_obj_set_property),
                  g_resp_buf, &out_len, -1, &fd_recv);
        break;
    }
    case DRM_IOCTL_MODE_CREATEPROPBLOB: {
        struct drm_mode_create_blob v = *(struct drm_mode_create_blob *)arg;
        if (v.length > sizeof(g_req_buf) - sizeof(v)) {
            ret = -EINVAL;
            break;
        }
        memcpy(g_req_buf, &v, sizeof(v));
        memcpy(g_req_buf + sizeof(v), (void *)(uintptr_t)v.data, v.length);
        in_len = sizeof(v) + v.length;
        ret = rpc(req, g_req_buf, in_len, g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        v.blob_id = ((struct drm_mode_create_blob *)g_resp_buf)->blob_id;
        *(struct drm_mode_create_blob *)arg = v;
        break;
    }
    case DRM_IOCTL_MODE_GETPROPBLOB: {
        struct drm_mode_get_blob v = *(struct drm_mode_get_blob *)arg;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        struct drm_mode_get_blob *rv = (void *)g_resp_buf;
        struct ext_reader r = { g_resp_buf + sizeof(*rv), g_resp_buf + out_len };
        if (v.data && v.length) {
            uint32_t n = v.length < rv->length ? v.length : rv->length;
            ext_take(&r, (void *)(uintptr_t)v.data, n);
        }
        v.length = rv->length;
        *(struct drm_mode_get_blob *)arg = v;
        break;
    }
    case DRM_IOCTL_PRIME_FD_TO_HANDLE: {
        struct drm_prime_handle v = *(struct drm_prime_handle *)arg;
        fd_send = v.fd;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, fd_send, &fd_recv);
        if (ret)
            break;
        v.handle = ((struct drm_prime_handle *)g_resp_buf)->handle;
        *(struct drm_prime_handle *)arg = v;
        break;
    }
    case DRM_IOCTL_PRIME_HANDLE_TO_FD: {
        struct drm_prime_handle v = *(struct drm_prime_handle *)arg;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        if (fd_recv < 0) {
            ret = -EIO;
            break;
        }
        v.fd = fd_recv;
        *(struct drm_prime_handle *)arg = v;
        break;
    }
    case DRM_IOCTL_MODE_ADDFB2:
    case DRM_IOCTL_MODE_GETFB2: {
        ret = rpc(req, arg, sizeof(struct drm_mode_fb_cmd2),
                  g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        memcpy(arg, g_resp_buf, sizeof(struct drm_mode_fb_cmd2));
        break;
    }
    case DRM_IOCTL_MODE_ATOMIC: {
        struct drm_mode_atomic v = *(struct drm_mode_atomic *)arg;
        uint32_t nobjs = v.count_objs;
        if (!nobjs || nobjs > 64) {
            ret = -EINVAL;
            break;
        }
        const uint32_t *objs = (const uint32_t *)(uintptr_t)v.objs_ptr;
        const uint32_t *counts = (const uint32_t *)(uintptr_t)v.count_props_ptr;
        const uint32_t *props = (const uint32_t *)(uintptr_t)v.props_ptr;
        const uint64_t *values = (const uint64_t *)(uintptr_t)v.prop_values_ptr;
        uint32_t total = 0;
        for (uint32_t i = 0; i < nobjs; i++)
            total += counts[i];
        if (total > 512) {
            ret = -EINVAL;
            break;
        }
        /* linearized: objs | counts | prop ids | values */
        uint8_t *w = g_req_buf;
        memcpy(w, &v, sizeof(v));
        w += sizeof(v);
        memcpy(w, objs, nobjs * 4);
        w += nobjs * 4;
        memcpy(w, counts, nobjs * 4);
        w += nobjs * 4;
        memcpy(w, props, total * 4);
        w += total * 4;
        memcpy(w, values, total * 8);
        w += total * 8;
        in_len = (uint32_t)(w - g_req_buf);

        /* at most one fence fd per commit (phase-1 contract) */
        if (g_in_fence_prop) {
            for (uint32_t i = 0; i < total; i++) {
                if (props[i] == g_in_fence_prop && values[i] != (uint64_t)-1) {
                    fd_send = (int)values[i];
                    break;
                }
            }
        }
        ret = rpc(req, g_req_buf, in_len, g_resp_buf, &out_len, fd_send, &fd_recv);
        break;
    }
    default:
        LOG("unhandled ioctl 0x%lx -> ENOSYS\n", req);
        ret = -EOPNOTSUPP;
        break;
    }

    if (ret < 0) {
        errno = -ret;
        return -1;
    }
    return ret;
}

#ifdef __ANDROID__
/* bionic declares ioctl(int, int, ...) — match its prototype exactly */
int ioctl(int fd, int req, ...)
{
    unsigned long req_l = (unsigned long)req;
#else
int ioctl(int fd, unsigned long req, ...)
{
    unsigned long req_l = req;
#endif
    void *arg = NULL;
    va_list ap;
    va_start(ap, req);
    arg = va_arg(ap, void *);
    va_end(ap);

    if (is_our_fd(fd))
        return drm_ioctl(fd, req_l, arg);

    return syscall(SYS_ioctl, fd, req_l, arg);
}
