/*
 * gndrm-shim — LD_PRELOAD ioctl interceptor for gamescope.
 *
 * Presents a fake /dev/dri/card* whose fd is a connected Unix socket pair to
 * the gamenative-drm daemon (doc 02-gamenative-drm.md §4/§6):
 *
 *   - open("/dev/dri/cardN")  → three SOCK_SEQPACKET connections to the daemon;
 *                               the *event* connection fd is returned to the
 *                               caller (read/poll on it behave exactly like a
 *                               real DRM fd delivering drm_event bytes). The
 *                               third connection is the scan-out allocator
 *                               channel (see scanout_request): the daemon owns
 *                               the scan-out AHardwareBuffers, because it is
 *                               the process that publishes them, and hands
 *                               back a dma-buf fd here. It is deliberately NOT
 *                               part of the framed RPC stream, which a
 *                               descriptor-carrying exchange would
 *                               desynchronise.
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
 * No glibc-isms; builds with bionic (NDK) and glibc.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
/* The uapi header gives the ioctl number when it is available (host builds and
 * newer NDKs); the literal is the cross-checked fallback. */
#if defined(__has_include)
#if __has_include(<linux/dma-heap.h>)
#include <linux/dma-heap.h>
#endif
#endif
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#include <drm/drm.h>
#include <drm/drm_mode.h>

/* ------------------------------------------------------------------ */
/* wire protocol (mirrors gndrm-uapi/src/wire.rs)                      */
/* ------------------------------------------------------------------ */

#define GNDRM_MAGIC 0x474E4431u /* "GND1" */
#define GNDRM_FLAG_FD (1u << 0)
#define GNDRM_MAX_FRAME (1u << 20)
/* Scratch RPC buffer capacity; the daemon's frames are far below this. */
#define GNDRM_SCRATCH_CAP (GNDRM_MAX_FRAME / 4)
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

/*
 * Scan-out allocator channel (third connection): one request, one response
 * carrying a dma-buf fd via SCM_RIGHTS. Not an ioctl frame, so it has its own
 * magic instead of GNDRM_MAGIC. Mirrors the ScanoutAllocReq/ScanoutAllocResp
 * pair in gndrm-uapi/src/wire.rs.
 */
#define GNDRM_SCANOUT_MAGIC 0x474E5331u /* "GNS1" */
/* HAL AHardwareBuffer format BGRA_8888 (value 5): B,G,R,A in memory, which is
 * the DRM XR24 (0x34325258, fourth byte ignored) scan-out layout gamescope
 * renders. A pool in any other format would reach SurfaceFlinger with swapped
 * channels; value 2 (R8G8B8X8_UNORM) is R,G,B,X and exchanges red and blue. */
#define GNDRM_SCANOUT_FORMAT_BGRA_8888 5u
/* Reply budget and attempt count for one scan-out request. A request that is
 * not answered inside the budget has either been lost or is being served
 * slowly; see scanout_request for why neither re-establishes the channel. */
#define GNDRM_SCANOUT_TIMEOUT_US 250000
#define GNDRM_SCANOUT_ATTEMPTS 3

/*
 * Reply budget for one request on the RPC channel (the ioctl channel).
 *
 * The RPC exchange is synchronous: `rpc()` writes one frame and blocks in
 * `recvmsg()` until the daemon answers. Without a bound, ANY request the
 * daemon does not answer (a render-channel request it cannot serve, a daemon
 * that has stopped reading, a half-paired render connection) blocks the caller
 * forever — and a caller blocked inside the first glamor ioctl means Xwayland
 * never reaches XWM and the session hangs with nothing logged.
 *
 * Generous on purpose: a healthy daemon answers in microseconds (it queues
 * commits and replies inline), so 5 s can only fire when the request is
 * genuinely lost. On a timeout the channel is retired (see rpc()), which turns
 * an unanswered request into a fast, visible failure on every subsequent call:
 * glamor's probe fails, Xwayland falls back and XWM still starts.
 */
#define GNDRM_RPC_TIMEOUT_SEC 5

struct gndrm_scanout_req {
    uint32_t magic;
    uint32_t width;
    uint32_t height;
    uint32_t format;
    uint64_t len;
} __attribute__((packed));
_Static_assert(sizeof(struct gndrm_scanout_req) == 24,
               "ScanoutAllocReq is 24 bytes on the wire");

struct gndrm_scanout_resp {
    uint32_t magic;
    int32_t ret;
    uint64_t ino;
    uint32_t stride;
    uint32_t reserved;
} __attribute__((packed));
_Static_assert(sizeof(struct gndrm_scanout_resp) == 24,
               "ScanoutAllocResp is 24 bytes on the wire");
_Static_assert(GNDRM_SCANOUT_FORMAT_BGRA_8888 == 5u,
               "scan-out pool AHB format is BGRA_8888 (HAL 5)");

/* ------------------------------------------------------------------ */
/* shim state                                                          */
/* ------------------------------------------------------------------ */

static int g_rpc_fd = -1;    /* hidden request/response channel */
static int g_event_fd = -1;  /* fd handed to the app (event channel) */
/*
 * Set once an RPC exchange times out or the channel dies. Retiring the channel
 * (rather than reconnecting under the caller) is deliberate: a timed-out
 * request may still be answered later, and that stale reply would be read as
 * the answer to whatever request came next. Once dead, every rpc() fails
 * immediately with ENOTCONN, so a caller that keeps issuing ioctls fails fast
 * and visibly instead of blocking on a channel that will never answer.
 */
static bool g_rpc_dead;
/*
 * Scan-out allocator channel (a further connection, see scanout_request). Its
 * own socket because it is a request/response exchange that carries a
 * descriptor, not an ioctl frame: mixing it into the framed RPC stream would
 * desynchronise the ioctl responses. A daemon restart closes it, so it is
 * re-established on demand rather than at the one-time init. Only a card node
 * (gamescope) ever uses it; a render node never scans out.
 */
static int g_scanout_fd = -1;
static pthread_mutex_t g_scanout_lock = PTHREAD_MUTEX_INITIALIZER;
static uint64_t g_seq;
static pthread_mutex_t g_rpc_lock = PTHREAD_MUTEX_INITIALIZER;
/*
 * Connection setup is lazy and node-type aware (not pthread_once): a render
 * node (Xwayland) must connect to the daemon's dedicated render listener and
 * must NOT connect to the card listener, whose contract reserves every
 * post-primary connection for gamescope's scan-out channel — connecting there
 * is what made a render-node open come back EPIPE on device. The first
 * intercepted open fixes the kind for the process's lifetime (one node per
 * process).
 */
static pthread_mutex_t g_init_lock = PTHREAD_MUTEX_INITIALIZER;
static int g_init_ok;
static int g_node_kind = -1; /* the kind this process connected as */
static uint32_t g_in_fence_prop; /* discovered at init, 0 = unsupported */

/*
 * Pass-throughs use raw syscalls, never dlsym(RTLD_NEXT) pointers: with
 * LD_PRELOAD the shim also captures libc's *own* calls (e.g. the
 * open/close inside __system_properties_init → PropertyInfoAreaFile::
 * LoadPath), which run during libc's constructor — BEFORE any shim init
 * could populate a dlsym-resolved pointer, so the pass-through would call
 * address 0 (observed on-device 2026-09-24: SIGSEGV at pc=0 in
 * PropertyInfoAreaFile::LoadPath, before main()). Syscalls need no libc
 * state and are safe at any init stage. bionic's open/open64 are plain
 * openat(AT_FDCWD, …) wrappers on arm64, so the mapping is exact.
 */

#define LOG(...)                       \
    do {                               \
        if (getenv("GNDRM_LOG_SHIM"))  \
            fprintf(stderr, "[gndrm-shim] " __VA_ARGS__); \
    } while (0)

/*
 * Render-channel trace: name the first occurrence of each DRM request a client
 * issues on one of our fds (GNDRM_LOG_SHIM=1). The daemon logs the same
 * first-occurrence line per connection, so with both gates on the last request
 * a stalled client reached is visible from both sides — which is what tells a
 * request that never arrived from one that arrived and was never answered.
 * Bounded and once-per-number, so it cannot flood a session log.
 */
static uint32_t g_seen_reqs[16];
static unsigned g_n_seen_reqs;

static void trace_req(uint32_t req)
{
    for (unsigned i = 0; i < g_n_seen_reqs; i++) {
        if (g_seen_reqs[i] == req)
            return;
    }
    if (g_n_seen_reqs < sizeof(g_seen_reqs) / sizeof(g_seen_reqs[0]))
        g_seen_reqs[g_n_seen_reqs++] = req;
    LOG("ioctl first 0x%08x\n", req);
}

/*
 * Sync-object ioctl numbers are spelled out rather than taken from
 * <drm/drm.h>: the NDK's copy is older than the kernel's and its
 * `struct drm_syncobj_handle` lacks the trailing `__u64 point`. `_IOWR`
 * encodes the struct size, so that header yields 0xc01064c1 while callers
 * built against current headers send the kernel's 0xc01864c1 — a silent
 * mismatch that shows up only as "unhandled ioctl". The wire layout is
 * asserted here so it cannot drift again.
 */
struct gndrm_syncobj_handle {
    uint32_t handle;
    uint32_t flags;
    int32_t fd;
    uint32_t pad;
    uint64_t point;
};
_Static_assert(sizeof(struct gndrm_syncobj_handle) == 24,
               "sync-object handle must match the kernel's drm_syncobj_handle");

/* Literal kernel values: `_IOWR('d', 0xBF, struct drm_syncobj_create)` and
 * `_IOWR('d', {0xC0,0xC1,0xC2}, …)` as of Linux 6.x. Literals, not the macro,
 * so no header can move them again. */
#define GNDRM_IOCTL_SYNCOBJ_CREATE 0xc00864bfu
#define GNDRM_IOCTL_SYNCOBJ_DESTROY 0xc00864c0u
#define GNDRM_IOCTL_SYNCOBJ_HANDLE_TO_FD 0xc01864c1u
#define GNDRM_IOCTL_SYNCOBJ_FD_TO_HANDLE 0xc01864c2u

/* ------------------------------------------------------------------ */
/* render-node surface                                                 */
/* ------------------------------------------------------------------ */
/*
 * The same daemon serves two node types. gamescope opens the KMS card node
 * (/dev/dri/cardN); Xwayland's glamor/dri3/drm opens a render node
 * (/dev/dri/renderDN). Both are intercepted to the daemon, but a render-node
 * connection is told apart from a card one: `open()` on a renderD path
 * declares it with the connection-setup frame below, and the daemon then
 * applies the kernel's DRM_RENDER_ALLOW gating (no modesetting, no master) and
 * serves the GEM/PRIME/syncobj surface glamor needs.
 */
#define GNDRM_IOCTL_NODE_KIND 0x474e0001u /* daemon contract (GNDRM_IOCTL_NODE_KIND) */
#define GNDRM_NODE_KIND_CARD 0u
#define GNDRM_NODE_KIND_RENDER 1u

/* Legacy generic GEM ioctls (dropped from modern drm.h, so literals):
 * `_IOWR('d', 0x1b, struct drm_gem_create[16])` and
 * `_IOWR('d', 0x1e, struct drm_gem_mmap[24])`. */
#define GNDRM_IOCTL_GEM_CREATE 0xc010641bu
#define GNDRM_IOCTL_GEM_MMAP 0xc018641eu

struct gndrm_gem_create {
    uint64_t size;
    uint32_t handle;
    uint32_t flags;
};
_Static_assert(sizeof(struct gndrm_gem_create) == 16,
               "drm_gem_create is 16 bytes on the wire");

struct gndrm_gem_mmap {
    uint32_t handle;
    uint32_t pad;
    uint64_t offset;
    uint64_t size;
};
_Static_assert(sizeof(struct gndrm_gem_mmap) == 24,
               "drm_gem_mmap is 24 bytes on the wire");

/* The high half of a GEM_MMAP offset is this token; the low bits carry the GEM
 * handle. `mmap()` on the device fd recognises it and maps the handle's
 * dma-buf, so the legacy handle→offset→mmap sequence works over a socket. */
#define GNDRM_GEM_MMAP_OFFSET_BASE 0x474e000000000000ull
#define GNDRM_GEM_MMAP_HANDLE_MASK 0x0000ffffffffffffull


/* On a host the system <drm/drm.h> is current, so cross-check the literals
 * above against it there. The NDK's copy is older (see above) and cannot be
 * used for this check. */
#ifndef __ANDROID__
_Static_assert(GNDRM_IOCTL_SYNCOBJ_CREATE ==
                   (unsigned int)DRM_IOCTL_SYNCOBJ_CREATE,
               "syncobj create number must match the kernel's");
_Static_assert(GNDRM_IOCTL_SYNCOBJ_DESTROY ==
                   (unsigned int)DRM_IOCTL_SYNCOBJ_DESTROY,
               "syncobj destroy number must match the kernel's");
_Static_assert(GNDRM_IOCTL_SYNCOBJ_HANDLE_TO_FD ==
                   (unsigned int)DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD,
               "syncobj handle-to-fd number must match the kernel's");
_Static_assert(GNDRM_IOCTL_SYNCOBJ_FD_TO_HANDLE ==
                   (unsigned int)DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE,
               "syncobj fd-to-handle number must match the kernel's");
#endif

/*
 * Output-path diagnostics (GNDRM_LOG_SHIM=1). The KMS scan-out gate is an
 * exact (format, modifier) match, so a rejected ADDFB2 is only diagnosable
 * from its arguments; and the ATOMIC stream is the compositor's flip loop, so
 * a rate-limited line there doubles as a liveness heartbeat that stops when
 * the flip loop does.
 */
static unsigned long g_n_addfb2, g_n_addfb2_fail, g_n_atomic, g_n_atomic_fail;
static long g_hb_last;

static int rate_ok(long every_sec)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    if (g_hb_last && ts.tv_sec - g_hb_last < every_sec)
        return 0;
    g_hb_last = ts.tv_sec ? ts.tv_sec : 1;
    return 1;
}

/*
 * Rate gates for the per-allocation scan-out decision lines. Deliberately not
 * rate_ok(): that timer is shared with the ATOMIC heartbeat, so a burst of
 * heap allocations would silence the heartbeat it is supposed to be read
 * alongside. A session logs its first frame-sized allocations in full (enough
 * to see every swapchain image of the startup burst), then samples; the
 * small-heap gate is separate and coarser, so gamescope's scratch churn cannot
 * starve the frame-sized lines that carry the decisions that matter.
 */
static unsigned long g_alloc_small, g_alloc_frame;

static int small_alloc_log_ok(void)
{
    unsigned long n = ++g_alloc_small;
    return n <= 8 || (n % 256) == 0;
}

static int alloc_log_ok(void)
{
    unsigned long n = ++g_alloc_frame;
    return n <= 64 || (n % 64) == 0;
}

/* `st_ino` of `fd`, the identity a dma-buf is registered and resolved under. */
static uint64_t fd_ino(int fd)
{
    struct stat st;
    if (fstat(fd, &st) != 0)
        return 0;
    return (uint64_t)st.st_ino;
}

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
    /* Address convention (matches the daemon's listen side): "/abs/path" is
     * a filesystem socket; "@name" or a bare "name" is abstract namespace. */
    if (path[0] == '@') {
        strncpy(sa.sun_path + 1, path + 1, sizeof(sa.sun_path) - 2);
    } else if (strchr(path, '/')) {
        strncpy(sa.sun_path, path, sizeof(sa.sun_path) - 1);
    } else {
        strncpy(sa.sun_path + 1, path, sizeof(sa.sun_path) - 2);
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
static int scanout_chan_init(void);

/* The socket a node kind connects to. A card node uses the configured socket;
 * a render node uses the daemon's dedicated render listener (main + "-render"),
 * so it never lands on the card listener, whose post-primary connections are
 * all gamescope's scan-out allocator channel. Both sides derive the name the
 * same way (see the daemon's `render_socket_name`). */
static const char *socket_path_for(int node_kind)
{
    const char *path = getenv("GAMENATIVE_DRM_SOCKET");
    if (!path || !*path)
        return NULL;
    if (node_kind != GNDRM_NODE_KIND_RENDER)
        return path;
    static char render_path[256];
    snprintf(render_path, sizeof(render_path), "%s-render", path);
    return render_path;
}

/*
 * Connect as `node_kind`, once per process. Returns true when the process is
 * connected as that kind; false when the socket is unset/unreachable or the
 * process is already connected as a different kind (one node per process).
 *
 * Lazily (on the first intercepted open) rather than at library load: a
 * render-node process must not touch the card listener at all, or its
 * connections become the card listener's scan-out channel and the render-node
 * open it is about to make comes back EPIPE.
 */
static bool ensure_init(int node_kind)
{
    pthread_mutex_lock(&g_init_lock);
    if (g_init_ok) {
        bool same = (g_node_kind == node_kind);
        pthread_mutex_unlock(&g_init_lock);
        return same;
    }
    const char *path = socket_path_for(node_kind);
    if (!path) {
        LOG("GAMENATIVE_DRM_SOCKET not set\n");
        pthread_mutex_unlock(&g_init_lock);
        return false;
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
        pthread_mutex_unlock(&g_init_lock);
        return false;
    }
    /*
     * Bound the synchronous RPC exchange. The fd stays blocking, so a healthy
     * request is unaffected; the timeout only fires when the daemon never
     * answers, which must fail the caller rather than freeze it (see
     * GNDRM_RPC_TIMEOUT_SEC).
     *
     * The event channel is handed to the app as its "DRM fd" and is read/recv'd
     * on to observe flip/event completion. SOCK_SEQPACKET honours
     * SO_RCVTIMEO/SO_SNDTIMEO, so the same bound applies there: an absent or
     * stalled event producer yields EAGAIN for the caller to retry instead of a
     * permanent block on the app's own thread.
     */
    struct timeval rpc_tv = { .tv_sec = GNDRM_RPC_TIMEOUT_SEC, .tv_usec = 0 };
    setsockopt(g_rpc_fd, SOL_SOCKET, SO_RCVTIMEO, &rpc_tv, sizeof(rpc_tv));
    setsockopt(g_rpc_fd, SOL_SOCKET, SO_SNDTIMEO, &rpc_tv, sizeof(rpc_tv));
    setsockopt(g_event_fd, SOL_SOCKET, SO_RCVTIMEO, &rpc_tv, sizeof(rpc_tv));
    setsockopt(g_event_fd, SOL_SOCKET, SO_SNDTIMEO, &rpc_tv, sizeof(rpc_tv));
    g_rpc_dead = false;
    g_node_kind = node_kind;
    g_init_ok = 1;
    if (node_kind == GNDRM_NODE_KIND_CARD) {
        /* Third connection: the scan-out allocator channel. Connected here, in
         * order, because the card daemon accepts the first two inline and
         * everything after as an allocator client. A failure is not fatal: the
         * channel is re-established on demand, and the daemon may not be
         * listening yet. A render node never scans out, so it never opens one. */
        if (scanout_chan_init() != 0)
            LOG("scanout channel connect failed: %s\n", strerror(errno));
        discover_in_fence_prop();
    }
    LOG("connected to %s (rpc=%d event=%d scanout=%d) in_fence prop=%u\n",
        path, g_rpc_fd, g_event_fd, g_scanout_fd, g_in_fence_prop);
    pthread_mutex_unlock(&g_init_lock);
    return true;
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

static int recv_frame(uint8_t *buf, uint32_t cap, uint32_t *len_io, int *fd_out, uint64_t seq)
{
    struct msghdr msg = { 0 };
    struct iovec iov = { .iov_base = buf, .iov_len = cap };
    char cbuf[CMSG_SPACE(sizeof(int))];
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf;
    msg.msg_controllen = sizeof(cbuf);

    ssize_t n = recvmsg(g_rpc_fd, &msg, 0);
    if (n < 0)
        return -errno;
    /* SEQPACKET truncates oversized records: detect before trusting n. */
    if (msg.msg_flags & MSG_TRUNC)
        return -EMSGSIZE;
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
 * Whether an exchange error means the RPC channel can never be trusted again.
 *
 * A reply timeout is fatal to the *synchronous* exchange, unlike on the
 * scan-out channel: the caller has already been failed, and a reply arriving
 * later would be read as the answer to some later request. A malformed or
 * truncated reply is equally unusable. Retiring the channel on these makes
 * every subsequent request fail immediately instead of re-blocking.
 */
static bool rpc_err_fatal(int e)
{
    e = -e;
    return e == EAGAIN || e == EWOULDBLOCK || e == ETIMEDOUT ||
           e == EPIPE || e == ECONNRESET || e == ECONNABORTED ||
           e == ENOTCONN || e == EBADF || e == EPROTO || e == EMSGSIZE;
}

/*
 * Full round trip. `out` receives the frame body (response struct +
 * linearized extensions); *out_len set on success. Returns 0 or -errno.
 * `out` is always the g_resp_buf scratch (g_rpc_lock serializes), so its
 * capacity is a compile-time constant here.
 *
 * Once the channel is dead every call returns -ENOTCONN without touching the
 * socket, so a caller that keeps issuing ioctls after a lost request fails
 * fast and visibly rather than blocking on a channel that will never answer.
 */
static int rpc(uint32_t ioctl_nr, const void *in, uint32_t in_len,
               uint8_t *out, uint32_t *out_len, int fd_send, int *fd_recv)
{
    int ret;
    pthread_mutex_lock(&g_rpc_lock);
    if (g_rpc_dead) {
        pthread_mutex_unlock(&g_rpc_lock);
        return -ENOTCONN;
    }
    uint64_t seq = ++g_seq;
    ret = send_frame(ioctl_nr, in, in_len, fd_send, seq);
    if (ret == 0)
        ret = recv_frame(out, GNDRM_SCRATCH_CAP, out_len, fd_recv, seq);
    if (ret != 0 && rpc_err_fatal(ret)) {
        if (g_rpc_fd >= 0)
            syscall(SYS_close, g_rpc_fd);
        g_rpc_fd = -1;
        g_rpc_dead = true;
        LOG("RPC channel retired (err=%d %s); further ioctls fail fast\n",
            -ret, strerror(-ret));
    }
    pthread_mutex_unlock(&g_rpc_lock);
    return ret;
}

/* Scratch buffers (protected by g_rpc_lock via rpc()). */
static uint8_t g_req_buf[GNDRM_SCRATCH_CAP];
static uint8_t g_resp_buf[GNDRM_SCRATCH_CAP];

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

/* A render node (/dev/dri/renderDN) is the node Xwayland's glamor/dri3 opens;
 * it is served by the same daemon but declares NODE_KIND_RENDER on connect. */
static bool is_drm_render_path(const char *path)
{
    return path && strncmp(path, "/dev/dri/renderD", 16) == 0;
}

static bool is_drm_node_path(const char *path)
{
    return is_drm_card_path(path) || is_drm_render_path(path);
}

/* App-facing fds: the event fd handed out by open_common plus every dup of
 * it. Real KMS fds survive dup() (libliftoff's liftoff_device_create dups
 * the DRM fd and issues ioctls on the copy — day-two on-device finding), so
 * ours must too. fds are small ints; a 1024-bit map covers anything bionic
 * hands out. g_our_fd_count == 0 means the connection is logically closed.
 * The hidden rpc fd is deliberately NOT in the map: ioctls never arrive on
 * it (it's not visible to the app… though nothing stops a pathological app
 * from ioctl()ing it — those go to the kernel and fail, which is fine). */
static uint64_t g_our_fd_map[16]; /* fds 0..1023 */
static int g_our_fd_count;
static bool g_opened; /* single open at a time (phase-1 contract) */

static bool is_our_fd(int fd)
{
    return fd >= 0 && fd < 1024 &&
           (g_our_fd_map[fd >> 6] & (1ULL << (fd & 63))) != 0;
}

static void our_fd_add(int fd)
{
    if (fd >= 0 && fd < 1024 && !is_our_fd(fd)) {
        g_our_fd_map[fd >> 6] |= 1ULL << (fd & 63);
        g_our_fd_count++;
    }
}

/* Drop one app-facing reference. When the last one goes, tear the hidden
 * rpc side down as well (the event socket dies with its last kernel fd). */
static void our_fd_release(int fd)
{
    if (!is_our_fd(fd))
        return;
    g_our_fd_map[fd >> 6] &= ~(1ULL << (fd & 63));
    g_our_fd_count--;
    if (g_our_fd_count == 0) {
        syscall(SYS_close, g_rpc_fd);
        g_rpc_fd = g_event_fd = -1;
        g_opened = false;
        /* note: pthread_once keeps the (now dead) connection state; a
         * second open in the same process fails — phase-1 contract. */
    }
}

/* ------------------------------------------------------------------ */
/* open family                                                         */
/* ------------------------------------------------------------------ */

/*
 * Returns the event-channel fd, or -1 with errno set. Only called when the
 * path matches a DRM card *and* GAMENATIVE_DRM_SOCKET is set; real opens
 * never see card paths in that configuration.
 */
static int open_common(int node_kind)
{
    if (!ensure_init(node_kind)) {
        errno = ECONNREFUSED;
        return -1;
    }
    if (g_opened) {
        errno = EBUSY;
        return -1;
    }
    if (node_kind == GNDRM_NODE_KIND_RENDER) {
        /* Declare the node type before any real ioctl. A failure to reach the
         * daemon here is a failed render-node open, like a real open() on a
         * node the process cannot use. */
        uint32_t kind = GNDRM_NODE_KIND_RENDER;
        uint32_t out_len = 0;
        int fd_recv = -1;
        int ret = rpc(GNDRM_IOCTL_NODE_KIND, &kind, sizeof(kind), g_resp_buf,
                      &out_len, -1, &fd_recv);
        if (ret != 0) {
            LOG("render-node handshake (NODE_KIND) failed: %s\n", strerror(-ret));
            errno = -ret;
            return -1;
        }
        LOG("render-node handshake (NODE_KIND) accepted\n");
    }
    g_opened = true;
    our_fd_add(g_event_fd);
    return g_event_fd;
}

/*
 * Explicit connect API for patched clients (gamescope's D1 patch calls this
 * via a weak symbol instead of relying on open("/dev/dri/cardN")
 * interposition). Requires GAMENATIVE_DRM_SOCKET to be set; returns the
 * event-channel fd exactly like an intercepted open() would, -1/errno on
 * failure (ECONNREFUSED = daemon unreachable, EBUSY = already connected).
 * Default visibility on purpose: this symbol is the shim's public ABI.
 */
int gamenative_drm_connect(void)
{
    return open_common(GNDRM_NODE_KIND_CARD);
}

/*
 * Interception rule: a card or render-node path + configured socket ⇒ always
 * intercept (even on hosts with a real /dev/dri, e.g. the CI box). Without the
 * env var, fall through to the real open untouched.
 */
static bool should_intercept(const char *path)
{
    if (!is_drm_node_path(path))
        return false;
    const char *sock = getenv("GAMENATIVE_DRM_SOCKET");
    return sock && *sock;
}

/* The node kind an intercepted open() impersonates, from its path. */
static int node_kind_for_path(const char *path)
{
    return is_drm_render_path(path) ? GNDRM_NODE_KIND_RENDER : GNDRM_NODE_KIND_CARD;
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
        return open_common(node_kind_for_path(path));
    return syscall(SYS_openat, AT_FDCWD, path, flags, mode);
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
        return open_common(node_kind_for_path(path));
    return syscall(SYS_openat, AT_FDCWD, path, flags, mode);
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
        return open_common(node_kind_for_path(path));
    return syscall(SYS_openat, dirfd, path, flags, mode);
}

int close(int fd)
{
    if (is_our_fd(fd)) {
        our_fd_release(fd);
        return (int)syscall(SYS_close, fd);
    }
    return (int)syscall(SYS_close, fd);
}

/* ------------------------------------------------------------------ */
/* dup family + fcntl: duplicated fds must keep working (kernel DRM    */
/* semantics — libliftoff relies on them, see the fd-map note above)   */
/* ------------------------------------------------------------------ */

int dup(int oldfd)
{
    int ret = (int)syscall(SYS_dup, oldfd);
    if (ret >= 0 && is_our_fd(oldfd))
        our_fd_add(ret);
    return ret;
}

int dup2(int oldfd, int newfd)
{
    bool src = is_our_fd(oldfd);
    bool dst = oldfd != newfd && is_our_fd(newfd); /* dup2 closes newfd first */
#ifdef SYS_dup2
    int ret = (int)syscall(SYS_dup2, oldfd, newfd);
#else
    /* arm64 has no dup2 syscall; libc implements dup2() as dup3(…, 0) */
    int ret = (int)syscall(SYS_dup3, oldfd, newfd, 0);
#endif
    if (ret >= 0) {
        if (dst)
            our_fd_release(newfd);
        if (src)
            our_fd_add(ret);
    }
    return ret;
}

#ifdef SYS_dup3
int dup3(int oldfd, int newfd, int flags)
{
    bool src = is_our_fd(oldfd);
    bool dst = is_our_fd(newfd); /* dup3 rejects oldfd == newfd */
    int ret = (int)syscall(SYS_dup3, oldfd, newfd, flags);
    if (ret >= 0) {
        if (dst)
            our_fd_release(newfd);
        if (src)
            our_fd_add(ret);
    }
    return ret;
}
#endif

int fcntl(int fd, int cmd, ...)
{
    void *arg = NULL;
    va_list ap;
    va_start(ap, cmd);
    arg = va_arg(ap, void *); /* harmless for arg-less cmds (see ioctl) */
    va_end(ap);
    int ret = (int)syscall(SYS_fcntl, fd, cmd, arg);
    if (ret >= 0 && (cmd == F_DUPFD || cmd == F_DUPFD_CLOEXEC) && is_our_fd(fd))
        our_fd_add(ret);
    return ret;
}

/* ------------------------------------------------------------------ */
/* mmap family: map a GEM object through its dma-buf                   */
/* ------------------------------------------------------------------ */
/*
 * The device fd is a socket and cannot be mapped. The legacy sequence is
 * GEM_MMAP (which mints an offset carrying the GEM handle) followed by
 * mmap(dev_fd, offset); this hook recognises the minted offset and maps the
 * handle's dma-buf instead, so a libdrm client that maps a GEM object the
 * legacy way works unchanged. Any other mmap on one of our fds is meaningless
 * and refused, exactly as mapping a socket would be.
 */
static void *gndrm_mmap(void *addr, size_t length, int prot, int flags, int fd,
                        off_t offset)
{
    if (!is_our_fd(fd))
        return (void *)syscall(SYS_mmap, addr, length, prot, flags, fd, offset);

    if (((uint64_t)offset & GNDRM_GEM_MMAP_OFFSET_BASE) !=
        GNDRM_GEM_MMAP_OFFSET_BASE) {
        errno = EINVAL;
        return MAP_FAILED;
    }
    uint32_t handle = (uint32_t)((uint64_t)offset & GNDRM_GEM_MMAP_HANDLE_MASK);
    struct drm_prime_handle ph = { .handle = handle, .flags = 0, .fd = -1 };
    uint32_t out_len = 0;
    int fd_recv = -1;
    int ret = rpc(DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph, sizeof(ph), g_resp_buf,
                  &out_len, -1, &fd_recv);
    if (ret != 0 || fd_recv < 0) {
        errno = ret ? -ret : EINVAL;
        return MAP_FAILED;
    }
    /* The dma-buf mapping lives on after the fd is closed (mmap holds its own
     * reference), so close immediately and hand back only the mapping. */
    void *p = (void *)syscall(SYS_mmap, addr, length, prot, flags, fd_recv, 0);
    close(fd_recv);
    return p;
}

void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset)
{
    return gndrm_mmap(addr, length, prot, flags, fd, offset);
}

void *mmap64(void *addr, size_t length, int prot, int flags, int fd, off64_t offset)
{
    return gndrm_mmap(addr, length, prot, flags, fd, (off_t)offset);
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

/* ------------------------------------------------------------------ */
/* dma-heap scan-out redirect                                          */
/* ------------------------------------------------------------------ */
/*
 * gamescope allocates its scan-out buffers - and the swapchain images it hands
 * to the guest through the gamescope_swapchain protocol - from
 * /dev/dma_heap/system, outside the DRM channel. gndrm therefore never sees the
 * allocation, only the fd later through PRIME_FD_TO_HANDLE. Redirecting that
 * allocation to an AHardwareBuffer is what lets the daemon hand the very buffer
 * gamescope rendered into to SurfaceFlinger, with no copy on either side.
 *
 * DMA_HEAP_IOCTL_ALLOC carries a length only. When that length matches a
 * connector mode the shim already marshals, the geometry is that mode (a
 * scan-out allocation is exactly w * h * 4). When it matches no mode - a game
 * rendering at 800x600 into the mode-sized output - the length is sent alone
 * and the daemon resolves it against the geometries it can serve: its modes,
 * the framebuffer sizes the client committed, and its pool keys. A length the
 * daemon cannot resolve (too small to be a frame, or not a geometry it knows)
 * is left to the kernel untouched, which keeps non-frame heap users unaffected.
 */
#ifndef GNDRM_IOCTL_DMAHEAP_ALLOC
#define GNDRM_IOCTL_DMAHEAP_ALLOC 0xc0184800u /* _IOWR('H', 0, alloc_data) */
#endif
#ifdef DMA_HEAP_IOCTL_ALLOC
_Static_assert(GNDRM_IOCTL_DMAHEAP_ALLOC == DMA_HEAP_IOCTL_ALLOC,
               "dma-heap alloc ioctl number drifted from the uapi header");
#endif

struct gndrm_dmaheap_alloc {
    uint64_t len;
    uint32_t fd;
    uint32_t fd_flags;
    uint64_t heap_flags;
};
_Static_assert(sizeof(struct gndrm_dmaheap_alloc) == 24,
               "dma_heap_allocation_data is 24 bytes on the wire");

#define GNDRM_MAX_MODES 8

static uint32_t g_scan_modes[GNDRM_MAX_MODES][2];
static int g_scan_nmodes;

/* ------------------------------------------------------------------ */
/* scan-out allocator channel                                          */
/* ------------------------------------------------------------------ */
/*
 * The daemon allocates and owns the scan-out AHardwareBuffer, and that is the
 * direction it has to run in: the daemon is the process that calls
 * ASurfaceTransaction_setBuffer, and an AHardwareBuffer cannot be built from a
 * bare dma-buf fd on API 33, so the publisher has to be the allocator. The shim
 * asks here by geometry and receives the buffer's mappable dma-buf through
 * SCM_RIGHTS, exactly as the ioctl path already receives one for
 * PRIME_HANDLE_TO_FD. The daemon files it under that fd's st_ino, which is also
 * how gamescope's PRIME_FD_TO_HANDLE of the same dma-buf identifies it.
 */

/*
 * Establish the allocator channel. 0 on success, -1 with errno set otherwise
 * (the caller retries: the daemon may not be listening yet). Called with
 * g_scanout_lock held.
 */
static int scanout_chan_init(void)
{
    if (g_scanout_fd >= 0)
        return 0;
    const char *path = getenv("GAMENATIVE_DRM_SOCKET");
    if (!path || !*path) {
        errno = ECONNREFUSED;
        return -1;
    }
    int fd = connect_one(path);
    if (fd < 0)
        return -1;
    /* Bound both directions: one small request out, one small reply back, so a
     * daemon that stops answering cannot stall gamescope's allocation path. */
    struct timeval tv = { .tv_sec = 0, .tv_usec = GNDRM_SCANOUT_TIMEOUT_US };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));
    g_scanout_fd = fd;
    return 0;
}

/*
 * Whether an exchange error means the connection itself is gone. A reply
 * timeout (EAGAIN/EWOULDBLOCK/ETIMEDOUT) is deliberately NOT one of these: it
 * says the daemon is slow or the request was lost, not that the socket died.
 * Re-establishing on a timeout is what splits the two sides onto different
 * fds - the daemon still reading this one, the shim writing on a fresh one -
 * so every request after the reconnect is written to a connection the daemon
 * is not reading. Only a genuinely dead channel is dropped and reconnected.
 */
static bool scanout_err_dead(int e)
{
    e = -e;
    /* EPROTO is included because a malformed reply means the framing on this
     * connection can no longer be trusted; it is as unusable as a closed one. */
    return e == EPIPE || e == ECONNRESET || e == ECONNABORTED ||
           e == ENOTCONN || e == EBADF || e == EPROTO;
}

/*
 * Whether an exchange error is a reply timeout rather than a dead channel.
 */
static bool scanout_err_timeout(int e)
{
    e = -e;
    return e == EAGAIN || e == EWOULDBLOCK || e == ETIMEDOUT;
}

/* Milliseconds elapsed since t0 (a CLOCK_MONOTONIC reading). */
static long elapsed_ms_since(const struct timespec *t0)
{
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    long ms = (long)(now.tv_sec - t0->tv_sec) * 1000L +
              (now.tv_nsec - t0->tv_nsec) / 1000000L;
    return ms < 0 ? 0 : ms;
}

/*
 * Retire a channel that failed, so the next request re-establishes it. Called
 * with g_scanout_lock held.
 */
static void scanout_chan_drop(void)
{
    if (g_scanout_fd >= 0)
        syscall(SYS_close, g_scanout_fd);
    g_scanout_fd = -1;
}

/*
 * One request/response exchange on a connected channel. Returns 0 or -errno;
 * on success *resp holds the reply and *fd_out the received descriptor.
 * Called with g_scanout_lock held.
 */
static int scanout_exchange(uint32_t w, uint32_t h, uint64_t len,
                            struct gndrm_scanout_resp *resp, int *fd_out)
{
    struct gndrm_scanout_req req = {
        .magic = GNDRM_SCANOUT_MAGIC,
        .width = w,
        .height = h,
        .format = GNDRM_SCANOUT_FORMAT_BGRA_8888,
        .len = len,
    };
    struct iovec iov = { .iov_base = &req, .iov_len = sizeof(req) };
    struct msghdr msg = { 0 };
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    /* MSG_NOSIGNAL: a daemon that has closed this channel must make the send
     * fail with EPIPE so scanout_request's retry can re-establish it, rather
     * than raising SIGPIPE and taking the client down. */
    if (sendmsg(g_scanout_fd, &msg, MSG_NOSIGNAL) < 0)
        return -errno;

    uint8_t buf[sizeof(struct gndrm_scanout_resp)];
    memset(buf, 0, sizeof(buf));
    struct iovec riov = { .iov_base = buf, .iov_len = sizeof(buf) };
    char cbuf[CMSG_SPACE(sizeof(int))];
    struct msghdr rmsg = { 0 };
    rmsg.msg_iov = &riov;
    rmsg.msg_iovlen = 1;
    rmsg.msg_control = cbuf;
    rmsg.msg_controllen = sizeof(cbuf);
    ssize_t n = recvmsg(g_scanout_fd, &rmsg, 0);
    if (n < 0)
        return -errno;

    /* Take the descriptor before validating anything: whatever arrived has to
     * be closed here if the frame turns out not to be a usable reply. */
    *fd_out = -1;
    if (rmsg.msg_controllen >= CMSG_LEN(sizeof(int))) {
        struct cmsghdr *c = CMSG_FIRSTHDR(&rmsg);
        if (c && c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS)
            memcpy(fd_out, CMSG_DATA(c), sizeof(int));
    }
    if ((size_t)n == sizeof(buf)) {
        memcpy(resp, buf, sizeof(*resp));
        if (resp->magic == GNDRM_SCANOUT_MAGIC) {
            if (resp->ret != 0 && *fd_out >= 0) {
                /* An error reply carries no buffer. */
                syscall(SYS_close, *fd_out);
                *fd_out = -1;
            }
            return 0;
        }
    }
    if (*fd_out >= 0) {
        syscall(SYS_close, *fd_out);
        *fd_out = -1;
    }
    return -EPROTO;
}

/*
 * Ask the daemon for one scan-out buffer: w x h when the shim matched a mode,
 * or the bare `len` (w = h = 0) for the daemon to resolve to a geometry it
 * knows. 0 on success with *out_fd owned by the caller. The channel is
 * re-established (with a bounded backoff)
 * only when it could not be connected or when it is genuinely dead; a reply
 * timeout leaves it in place, because a timeout does not mean the socket died
 * and reconnecting on it is what splits the two sides onto different fds. On
 * any failure *out_attempts and *out_elapsed_ms report which attempt failed
 * and the whole time spent inside this call, so the shim line can be aligned
 * with the daemon's request log.
 */
static int scanout_request(uint32_t w, uint32_t h, uint64_t len, int *out_fd,
                           uint64_t *out_ino, uint32_t *out_stride,
                           int *out_attempts, long *out_elapsed_ms)
{
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    int rc = -ECONNREFUSED;
    int attempts = 0;

    for (int i = 0; i < GNDRM_SCANOUT_ATTEMPTS; i++) {
        attempts = i + 1;
        bool reconnect = false;
        pthread_mutex_lock(&g_scanout_lock);
        if (scanout_chan_init() != 0) {
            /* Not connected (the daemon may not be listening yet): this is the
             * reconnect case, so back off and try to connect again. */
            rc = -ECONNREFUSED;
            reconnect = true;
        } else {
            struct gndrm_scanout_resp resp;
            int fd = -1;
            memset(&resp, 0, sizeof(resp));
            rc = scanout_exchange(w, h, len, &resp, &fd);
            bool timed_out = (rc != 0) && scanout_err_timeout(rc);
            if (rc == 0 && resp.ret != 0)
                rc = resp.ret; /* daemon refusal: the channel is healthy */
            if (rc != 0 && scanout_err_dead(rc)) {
                /* The connection itself is gone: drop it so the next attempt
                 * re-establishes it, and back off first. */
                scanout_chan_drop();
                reconnect = true;
            } else if (timed_out) {
                /* Reply timeout: the peer is slow or the request was lost. The
                 * channel is still the current one, so it is deliberately kept
                 * and not retried - resending could be answered twice and
                 * reconnecting would orphan the fd the daemon is reading. */
                reconnect = false;
            } else if (rc == 0 && fd < 0) {
                rc = -EIO; /* a success reply must carry a buffer */
            } else if (rc == 0) {
                *out_fd = fd;
                if (out_ino)
                    *out_ino = resp.ino;
                if (out_stride)
                    *out_stride = resp.stride;
            }
        }
        pthread_mutex_unlock(&g_scanout_lock);
        if (rc == 0)
            break;
        if (!reconnect)
            break; /* a daemon refusal or a timeout keeps the channel */
        /* 5 ms, 10 ms: a daemon that is restarting needs a moment to listen. */
        struct timespec ts = { 0, (5 * 1000 * 1000L) << i };
        nanosleep(&ts, NULL);
    }
    if (out_attempts)
        *out_attempts = attempts;
    if (out_elapsed_ms)
        *out_elapsed_ms = elapsed_ms_since(&t0);
    return rc;
}

/* Remember a connector mode. Several are offered and gamescope picks one, so
 * they are all kept and matched by size rather than guessed. */
static void scanout_note_mode(uint32_t w, uint32_t h)
{
    if (!w || !h)
        return;
    for (int i = 0; i < g_scan_nmodes; i++)
        if (g_scan_modes[i][0] == w && g_scan_modes[i][1] == h)
            return;
    if (g_scan_nmodes >= GNDRM_MAX_MODES)
        return;
    g_scan_modes[g_scan_nmodes][0] = w;
    g_scan_modes[g_scan_nmodes][1] = h;
    g_scan_nmodes++;
}

/* Seed the geometry from GNDRM_SCANOUT ("WxH", set by the launcher from the
 * same values it passes gamescope). The connector exchange does not always
 * precede the first frame-sized allocation - a buffer allocated before any mode
 * was seen stays a plain heap buffer, which the native presenter cannot
 * publish - so the intended geometry is available up front instead. */
static bool g_scan_env_done;

static void scanout_seed_from_env(void)
{
    if (g_scan_env_done)
        return;
    g_scan_env_done = true;
    const char *v = getenv("GNDRM_SCANOUT");
    unsigned w = 0, h = 0;
    if (v && sscanf(v, "%ux%u", &w, &h) == 2)
        scanout_note_mode(w, h);
}

/*
 * Smallest allocation length treated as a possible scan-out frame. A frame is
 * exactly w * h * 4 and cannot be tiny: the 4096-byte scratch allocations
 * gamescope makes are far below this and stay with the kernel, so no request is
 * ever sent for them. Every window size a game renders into is well above it.
 */
#define GNDRM_SCANOUT_MIN_LEN ((uint64_t)128 * 128 * 4)

/*
 * Ask the daemon for one buffer and, on success, hand its descriptor back to
 * gamescope as this heap allocation. `w`/`h` are the geometry when the shim
 * matched a connector mode, or 0 for a length-only request the daemon resolves;
 * `len` is the allocation length and is always sent. 0 on success, -1 to fall
 * through to the kernel ioctl.
 */
static int scanout_hand_out(struct gndrm_dmaheap_alloc *a,
                            uint32_t w, uint32_t h, uint64_t len)
{
    int fd = -1;
    uint64_t ino = 0;
    uint32_t stride = 0;
    int attempts = 0;
    long elapsed_ms = 0;
    int rc = scanout_request(w, h, len, &fd, &ino, &stride, &attempts, &elapsed_ms);
    if (rc != 0) {
        /* No buffer means no publishable frame; falling through leaves the
         * allocation a plain heap buffer, which is the pre-redirect behaviour
         * rather than a broken one. Name the fault rather than reporting a bare
         * errno: a request that ran out its reply budget, a full pool, an
         * allocator failure, an unsupported geometry or a daemon that is
         * unreachable. The elapsed time and attempt number are logged so this
         * line can be aligned with the daemon's "received (seq=)" receipt (or
         * its absence). */
        const char *why = "allocation failure";
        if (rc == -EAGAIN || rc == -ETIMEDOUT)
            why = "request timed out waiting for a reply";
        else if (rc == -ENOSPC)
            why = "daemon pool exhausted";
        else if (rc == -ENOSYS)
            why = "daemon presenter disabled";
        else if (rc == -EINVAL)
            why = "unsupported geometry/format";
        else if (rc == -ECONNREFUSED)
            why = "allocator channel unreachable";
        else if (rc == -EPROTO)
            why = "malformed daemon reply";
        if (rate_ok(2)) {
            if (w && h)
                LOG("scanout: daemon allocated no %ux%u buffer: %s "
                    "(elapsed=%ldms attempt=%d/%d) (errno %d %s)\n",
                    w, h, why, elapsed_ms, attempts, GNDRM_SCANOUT_ATTEMPTS,
                    -rc, strerror(-rc));
            else
                LOG("scanout: daemon allocated no %llu B buffer: %s "
                    "(elapsed=%ldms attempt=%d/%d) (errno %d %s)\n",
                    (unsigned long long)len, why, elapsed_ms, attempts,
                    GNDRM_SCANOUT_ATTEMPTS, -rc, strerror(-rc));
        }
        return -1;
    }
    if (a->fd_flags & O_CLOEXEC)
        syscall(SYS_fcntl, fd, F_SETFD, FD_CLOEXEC);
    a->fd = (uint32_t)fd;
    a->heap_flags = 0;
    if (w && h)
        LOG("scanout: heap alloc %llu B -> daemon buffer %ux%u fd=%d ino=%llu stride=%u\n",
            (unsigned long long)len, w, h, fd, (unsigned long long)ino, stride);
    else
        LOG("scanout: heap alloc %llu B -> daemon buffer (by length) "
            "fd=%d ino=%llu stride=%u\n",
            (unsigned long long)len, fd, (unsigned long long)ino, stride);
    return 0;
}

/*
 * 0 = redirected to a daemon-owned buffer, -1 = not a scan-out allocation (the
 * caller then lets the real kernel ioctl handle it).
 */
static int scanout_redirect(void *arg)
{
    struct gndrm_dmaheap_alloc *a = arg;
    if (!a)
        return -1;
    scanout_seed_from_env();
    /* Not a frame: too small (the 4096-byte scratch allocations) or not
     * 4-byte-per-pixel sized. Leave it to the kernel without a round trip.
     * Logged, because this is exactly where a scan-out allocation the shim did
     * NOT recognise would disappear: the caller's later fstat of the resulting
     * fd is the only other place its inode appears. */
    if (a->len < GNDRM_SCANOUT_MIN_LEN || (a->len & 3u)) {
        if (small_alloc_log_ok())
            LOG("scanout: alloc len=%llu left to kernel (below %llu B or not "
                "4-byte-per-pixel)\n",
                (unsigned long long)a->len,
                (unsigned long long)GNDRM_SCANOUT_MIN_LEN);
        return -1;
    }
    /* A connector mode the shim already knows names the geometry outright;
     * sending it unchanged keeps the mode-matched path exactly as it was. */
    for (int i = 0; i < g_scan_nmodes; i++) {
        uint32_t w = g_scan_modes[i][0], h = g_scan_modes[i][1];
        if (a->len == (uint64_t)w * h * 4) {
            if (alloc_log_ok())
                LOG("scanout: alloc len=%llu -> mode-matched %ux%u\n",
                    (unsigned long long)a->len, w, h);
            return scanout_hand_out(a, w, h, a->len);
        }
    }
    /* No mode matches: send the length alone and let the daemon resolve it
     * against the geometries it can serve - the connector modes it advertises,
     * the framebuffer sizes the client committed, and its pool keys. A game
     * rendering at 800x600 into the mode-sized output lands here, so its
     * buffers become pool-owned like the mode-sized ones rather than being
     * scanned out as bare client dma-bufs the presenter cannot publish. A
     * length the daemon cannot resolve comes back EINVAL and is left to the
     * kernel, which is the "not a scan-out allocation" pass-through. */
    if (alloc_log_ok())
        LOG("scanout: alloc len=%llu -> length-only request\n",
            (unsigned long long)a->len);
    return scanout_hand_out(a, 0, 0, a->len);
}

static int drm_ioctl(int fd, unsigned long req, void *arg)
{
    (void)fd; /* fd validity already established by the dispatcher */
    int ret, fd_recv = -1, fd_send = -1;
    uint32_t out_len = 0;
    uint32_t in_len = 0;

    trace_req((uint32_t)req);

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
    case GNDRM_IOCTL_GEM_CREATE:
    case GNDRM_IOCTL_GEM_MMAP:
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
        /* The heap allocation that follows is sized from one of these modes. */
        {
            uint32_t mc = v.count_modes ? nm : 0;
            const struct drm_mode_modeinfo *mm = (const void *)v.modes_ptr;
            for (uint32_t mi = 0; mm && mi < mc; mi++)
                scanout_note_mode(mm[mi].hdisplay, mm[mi].vdisplay);
        }
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
        /* Snapshot first: the success path overwrites `arg` with the reply. */
        const struct drm_mode_fb_cmd2 fb = *(const struct drm_mode_fb_cmd2 *)arg;
        const char *op = req == DRM_IOCTL_MODE_ADDFB2 ? "ADDFB2" : "GETFB2";
        unsigned long n = g_n_addfb2++;
        ret = rpc(req, arg, sizeof(struct drm_mode_fb_cmd2),
                  g_resp_buf, &out_len, -1, &fd_recv);
        if (ret) {
            g_n_addfb2_fail++;
            LOG("%s FAIL ret=%d %ux%u fourcc=0x%08x flags=0x%x "
                "mods=%#llx,%#llx,%#llx,%#llx handles=%u,%u,%u,%u "
                "pitches=%u,%u,%u,%u offsets=%u,%u,%u,%u\n",
                op, ret, fb.width, fb.height, fb.pixel_format, fb.flags,
                (unsigned long long)fb.modifier[0], (unsigned long long)fb.modifier[1],
                (unsigned long long)fb.modifier[2], (unsigned long long)fb.modifier[3],
                fb.handles[0], fb.handles[1], fb.handles[2], fb.handles[3],
                fb.pitches[0], fb.pitches[1], fb.pitches[2], fb.pitches[3],
                fb.offsets[0], fb.offsets[1], fb.offsets[2], fb.offsets[3]);
            break;
        }
        memcpy(arg, g_resp_buf, sizeof(struct drm_mode_fb_cmd2));
        if (n < 4)
            LOG("%s ok #%lu %ux%u fourcc=0x%08x flags=0x%x mods=%#llx\n",
                op, n + 1, fb.width, fb.height, fb.pixel_format, fb.flags,
                (unsigned long long)fb.modifier[0]);
        break;
    }
    case DRM_IOCTL_MODE_ATOMIC: {
        struct drm_mode_atomic v = *(struct drm_mode_atomic *)arg;
        uint32_t nobjs = v.count_objs;
        if (!nobjs || nobjs > 64) {
            LOG("ATOMIC REJECT nobjs=%u\n", nobjs);
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
            LOG("ATOMIC REJECT total=%u\n", total);
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
        g_n_atomic++;
        if (ret) {
            g_n_atomic_fail++;
            LOG("ATOMIC FAIL ret=%d objs=%u total=%u\n", ret, nobjs, total);
        } else if (rate_ok(2)) {
            LOG("ATOMIC heartbeat #%lu ok=%lu fail=%lu addfb2=%lu fail=%lu objs=%u total=%u\n",
                g_n_atomic, g_n_atomic - g_n_atomic_fail, g_n_atomic_fail,
                g_n_addfb2, g_n_addfb2_fail, nobjs, total);
        }
        break;
    }
    /* Sync objects (gamescope's DRM backend creates one always-signalled
     * object at init and rides its exported fd as the primary plane's
     * in-fence). HANDLE_TO_FD returns an fd, so it uses the same SCM_RIGHTS
     * path as PRIME_HANDLE_TO_FD. */
    case GNDRM_IOCTL_SYNCOBJ_CREATE: {
        ret = rpc(req, arg, sizeof(struct drm_syncobj_create),
                  g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        memcpy(arg, g_resp_buf, sizeof(struct drm_syncobj_create));
        break;
    }
    case GNDRM_IOCTL_SYNCOBJ_DESTROY: {
        ret = rpc(req, arg, sizeof(struct drm_syncobj_destroy),
                  g_resp_buf, &out_len, -1, &fd_recv);
        break;
    }
    case GNDRM_IOCTL_SYNCOBJ_HANDLE_TO_FD: {
        struct gndrm_syncobj_handle v = *(struct gndrm_syncobj_handle *)arg;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, -1, &fd_recv);
        if (ret)
            break;
        if (fd_recv < 0) {
            ret = -EIO;
            break;
        }
        v.fd = fd_recv;
        *(struct gndrm_syncobj_handle *)arg = v;
        break;
    }
    case GNDRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: {
        /* Forwarded so the daemon owns the answer (it needs real dma-fence
         * plumbing to import a sync file, so it refuses). */
        struct gndrm_syncobj_handle v = *(struct gndrm_syncobj_handle *)arg;
        ret = rpc(req, &v, sizeof(v), g_resp_buf, &out_len, (int)v.fd, &fd_recv);
        break;
    }
    default: {
        /*
         * Generic forward. Every DRM request the switch above does not
         * specially marshal still goes to the daemon, which owns the answer
         * for it (a real errno for the requests it does not model). This is
         * what keeps the render-node contract total: a request Mesa's
         * gbm/glamor path issues but the shim has no bespoke arity for is no
         * longer answered by the shim inventing EOPNOTSUPP, and the daemon log
         * names it. `_IOC_SIZE(req)` bytes are the whole fixed struct; the
         * pointer-carrying arities are handled by the cases above.
         */
        size_t len = _IOC_SIZE(req);
        if (len > sizeof(g_req_buf)) {
            ret = -EINVAL;
            break;
        }
        ret = rpc(req, arg, (uint32_t)len, g_resp_buf, &out_len, -1, &fd_recv);
        if (ret == 0 && out_len) {
            uint32_t n = out_len < len ? out_len : (uint32_t)len;
            memcpy(arg, g_resp_buf, n);
        }
        break;
    }
    }

    if (ret < 0) {
        errno = -ret;
        return -1;
    }
    return ret;
}

#ifdef __ANDROID__
/* bionic declares ioctl(int, int, ...) — match its prototype exactly.
 * Widen through unsigned int: request encodes dir/size/type/nr and
 * _IOR/_IOWR values have bit31 set; widening the int straight to
 * unsigned long would sign-extend (0xFFFFFFFFC04064A0) and never match
 * the drm.h constants. */
int ioctl(int fd, int req, ...)
{
    unsigned long req_l = (unsigned long)(unsigned int)req;
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

    if (req_l == GNDRM_IOCTL_DMAHEAP_ALLOC && scanout_redirect(arg) == 0)
        return 0;

    if (is_our_fd(fd))
        return drm_ioctl(fd, req_l, arg);

    int ret = syscall(SYS_ioctl, fd, req_l, arg);
    /*
     * A frame-sized dma-heap allocation the shim did not redirect: name it and
     * its resulting inode. Without this an unredirected scan-out-sized
     * allocation leaves no trace at all, which is what makes a presenter miss
     * ("no AHardwareBuffer for dma-buf ino N; re-publishing ...") impossible to
     * attribute to the allocation that caused it.
     */
    if (req_l == GNDRM_IOCTL_DMAHEAP_ALLOC && ret == 0) {
        struct gndrm_dmaheap_alloc *a = arg;
        if (a && a->len >= GNDRM_SCANOUT_MIN_LEN && alloc_log_ok())
            LOG("scanout: alloc len=%llu left to kernel -> fd ino=%llu\n",
                (unsigned long long)a->len,
                (unsigned long long)fd_ino((int)a->fd));
    }
    return ret;
}
