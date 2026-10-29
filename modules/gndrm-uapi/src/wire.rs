//! Wire protocol between `gndrm-shim` (gamescope process) and `gndrm-daemon`
//! (`GameNative` app), over a `SOCK_SEQPACKET` Unix domain socket.
//!
//! ## Framing
//!
//! ```text
//! request : ReqHdr  | ioctl-struct bytes | linearized extension arrays
//! response: RespHdr | ioctl-struct bytes | linearized extension arrays
//! ```
//!
//! - One frame == one `send()` == one `recv()` (`SOCK_SEQPACKET` preserves
//!   message boundaries; the daemon caps frames at [`MAX_FRAME_LEN`]).
//! - The fixed ioctl struct is carried **verbatim** — its userspace pointers
//!   are meaningless in the daemon's address space and are ignored there.
//! - Pointer-carrying ioctls (GETCONNECTOR, ATOMIC, ...) append their array
//!   payloads after the struct, in field-declaration order.
//! - dma-buf fds travel out-of-band via `SCM_RIGHTS`; `flags & FLAG_FD`
//!   signals that one fd accompanies the frame.
//!
//!
//! All multi-byte fields are little-endian (both supported platforms are LE;
//! the encoding is explicit so the format is platform-independent).

/// Frame magic: ASCII "GND1".
pub const MAGIC: u32 = 0x474E_4431;

/// Hard cap on one frame (header + payload). Anything larger is a protocol
/// violation; the largest legitimate frames are mode/atomic payloads.
pub const MAX_FRAME_LEN: usize = 1 << 20;

/// `flags` bit: an fd is attached to this frame via `SCM_RIGHTS`.
pub const FLAG_FD: u32 = 1 << 0;

pub const REQ_HDR_LEN: usize = 24;
pub const RESP_HDR_LEN: usize = 24;

/// Request frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReqHdr {
    /// Total frame length including this header.
    pub total_len: u32,
    /// The DRM ioctl request number being invoked.
    pub ioctl_nr: u32,
    /// `FLAG_*` bits.
    pub flags: u32,
    /// Sender-chosen sequence number, echoed by the response.
    pub seq: u64,
}

impl ReqHdr {
    #[must_use]
    pub fn encode(self) -> [u8; REQ_HDR_LEN] {
        let mut b = [0u8; REQ_HDR_LEN];
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&self.total_len.to_le_bytes());
        b[8..12].copy_from_slice(&self.ioctl_nr.to_le_bytes());
        b[12..16].copy_from_slice(&self.flags.to_le_bytes());
        b[16..24].copy_from_slice(&self.seq.to_le_bytes());
        b
    }

    /// # Errors
    /// [`WireError::Short`] if `b` is shorter than the header,
    /// [`WireError::BadMagic`] on a magic mismatch, [`WireError::BadLength`]
    /// if the encoded `total_len` is out of range.
    ///
    /// # Panics
    /// Never panics: all slice conversions are guarded by the length check.
    /// (The `try_into().unwrap()` calls are on statically-sized sub-slices.)
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < REQ_HDR_LEN {
            return Err(WireError::Short);
        }
        if u32::from_le_bytes(b[0..4].try_into().unwrap()) != MAGIC {
            return Err(WireError::BadMagic);
        }
        let total_len = u32::from_le_bytes(b[4..8].try_into().unwrap());
        if total_len as usize > MAX_FRAME_LEN || (total_len as usize) < REQ_HDR_LEN {
            return Err(WireError::BadLength);
        }
        Ok(Self {
            total_len,
            ioctl_nr: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            flags: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            seq: u64::from_le_bytes(b[16..24].try_into().unwrap()),
        })
    }
}

/// Response frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RespHdr {
    /// Total frame length including this header.
    pub total_len: u32,
    /// Sequence number of the request being answered.
    pub seq: u64,
    /// ioctl return value: 0 on success, negative errno otherwise.
    pub ret: i32,
    /// Length of the out-payload following this header (may be 0 on error).
    pub out_len: u32,
}

impl RespHdr {
    #[must_use]
    pub fn encode(self) -> [u8; RESP_HDR_LEN] {
        let mut b = [0u8; RESP_HDR_LEN];
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&self.total_len.to_le_bytes());
        b[8..16].copy_from_slice(&self.seq.to_le_bytes());
        b[16..20].copy_from_slice(&self.ret.to_le_bytes());
        b[20..24].copy_from_slice(&self.out_len.to_le_bytes());
        b
    }

    /// # Errors
    /// [`WireError::Short`] if `b` is shorter than the header,
    /// [`WireError::BadMagic`] on a magic mismatch, [`WireError::BadLength`]
    /// if the encoded `total_len` is out of range.
    ///
    /// # Panics
    /// Never panics: all slice conversions are guarded by the length check.
    /// (The `try_into().unwrap()` calls are on statically-sized sub-slices.)
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < RESP_HDR_LEN {
            return Err(WireError::Short);
        }
        if u32::from_le_bytes(b[0..4].try_into().unwrap()) != MAGIC {
            return Err(WireError::BadMagic);
        }
        let total_len = u32::from_le_bytes(b[4..8].try_into().unwrap());
        if total_len as usize > MAX_FRAME_LEN || (total_len as usize) < RESP_HDR_LEN {
            return Err(WireError::BadLength);
        }
        Ok(Self {
            total_len,
            seq: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            ret: i32::from_le_bytes(b[16..20].try_into().unwrap()),
            out_len: u32::from_le_bytes(b[20..24].try_into().unwrap()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// Fewer bytes than a header.
    Short,
    BadMagic,
    /// `total_len` out of range (smaller than the header or over the cap).
    BadLength,
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Short => f.write_str("frame shorter than header"),
            Self::BadMagic => f.write_str("bad frame magic"),
            Self::BadLength => f.write_str("frame length out of range"),
        }
    }
}

impl std::error::Error for WireError {}

/// errno values the daemon produces (negated on the wire, as the kernel would
/// return them through the C library ioctl wrapper: `-1` + `errno` set — the
/// shim maps `ret < 0` to `errno = -ret`).
pub mod errno {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EINTR: i32 = 4;
    pub const EIO: i32 = 5;
    pub const EBADF: i32 = 9;
    pub const EAGAIN: i32 = 11;
    pub const ENOMEM: i32 = 12;
    pub const EACCES: i32 = 13;
    pub const EFAULT: i32 = 14;
    pub const EBUSY: i32 = 16;
    pub const EINVAL: i32 = 22;
    pub const ENOSPC: i32 = 28;
    pub const ERANGE: i32 = 34;
    pub const ENOSYS: i32 = 38;
    pub const EPROTO: i32 = 71;
    pub const ENOTSUP: i32 = 95;
}

// ------------------------------------------------------- scan-out allocator
//
// A fourth message shape on the wire, carried by its own `SOCK_SEQPACKET`
// connection: the scan-out allocator channel. It is not an ioctl frame — it is
// a one-request/one-response exchange — so it gets its own magic instead of
// the `MAGIC`/`ReqHdr` framing, and the response carries one dma-buf fd
// out-of-band via `SCM_RIGHTS` exactly like `PRIME_HANDLE_TO_FD` does.
//
// Direction of ownership: the daemon allocates and owns the
// `AHardwareBuffer`, because the daemon is the process that publishes it
// through `SurfaceControl` — an `AHardwareBuffer` cannot be reconstructed from
// a bare dma-buf fd on this platform, so whoever must call
// `ASurfaceTransaction_setBuffer` has to be the process that called
// `AHardwareBuffer_allocate`. The shim only asks for a buffer by geometry and
// receives a descriptor to hand to gamescope as its heap allocation.
//
// Correlation needs no field on the wire: the daemon fstats the descriptor it
// sends and registers the buffer under that `stat.st_ino`; gamescope's later
// `PRIME_FD_TO_HANDLE` of the same dma-buf lands on the same inode, so the
// presenter resolves a committed framebuffer to the buffer it allocated.

/// Scan-out allocator message magic: ASCII "GNS1".
pub const SCANOUT_MAGIC: u32 = 0x474E_5331;

/// The scan-out pool's colour layout. Allocation sites pick a layout through
/// this key and [`scanout_ahb_format`] rather than naming a HAL constant, so a
/// future capability adds one arm below instead of a hardcoded value chasing
/// across the wire, the C shim and the presenter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanoutLayout {
    /// 8-bit R,G,B,A. The pool's only layout today.
    EightBit,
}

/// HAL `AHardwareBuffer` format for `layout`.
///
/// `EightBit` is **`AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM`** (HAL value 1,
/// `RGBA_8888`): R,G,B,A in memory. That is the byte order of the `XB24`
/// (`0x34324258`) / `AB24` the compositor commits for scan-out, whose fourth
/// byte the presenter declares opaque. The primary plane offers only those
/// R-low formats, so the compositor never places a B,G,R buffer on it.
///
/// The pool was `BGRA_8888` (HAL value 5) matching `XR24`, but the Skia render
/// engine in `SurfaceFlinger` cannot wrap value 5 (Android 16 aborts the whole
/// UI on it) and Qualcomm gralloc has no display constraints for it.
///
/// The 10-bit/HDR entry (`R10G10B10A2_UNORM`-class) belongs here as a
/// `TenBit` arm once the DRM side carries what it needs — 10-bit plane
/// formats, the `Colorspace` property, `HDR_OUTPUT_METADATA`, HDR EDID — none
/// of which exist yet.
#[must_use]
pub const fn scanout_ahb_format(layout: ScanoutLayout) -> u32 {
    match layout {
        ScanoutLayout::EightBit => 1, // AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM
    }
}

/// The format the scan-out path requires today, i.e.
/// [`scanout_ahb_format`]`(ScanoutLayout::EightBit)`.
pub const SCANOUT_FORMAT_RGBA_8888: u32 = scanout_ahb_format(ScanoutLayout::EightBit);

/// Allocation request.
///
/// Two shapes, told apart by `width`/`height`:
/// - a **geometry** request (both non-zero) when the shim matched the
///   allocation length to a connector mode it already knows;
/// - a **length-only** request (`width == 0 && height == 0`, `len != 0`) when
///   it did not: the daemon resolves `len` to a geometry it can serve (a
///   connector mode, a framebuffer the client registered, or a geometry
///   already in its pool), so the shim does not have to know every window size
///   in advance.
///
/// Encoded little-endian, 24 bytes:
/// `magic:u32, width:u32, height:u32, format:u32, len:u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanoutAllocReq {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    /// Allocation length in bytes. Carried on every request: it is the
    /// geometry on a length-only request and a cross-check on a geometry one.
    pub len: u64,
}

/// Set in a request's `format` when the client also wants the buffer's
/// `AHardwareBuffer` itself: after the reply and its descriptor, the daemon
/// sends the handle with `AHardwareBuffer_sendHandleToUnixSocket` on the same
/// channel. A daemon that predates the flag sees an unknown format and refuses
/// the request, so an out-of-step pair fails fast instead of desynchronising.
pub const SCANOUT_REQ_AHB_HANDLE: u32 = 1 << 31;

/// Set in a request's `format` to look up a buffer the pool already handed
/// out instead of allocating one: `len` carries the `stat.st_ino` of its
/// dma-buf and `width`/`height` are zero. The reply is the same as for an
/// allocation (a fresh descriptor of the buffer, then its `AHardwareBuffer`
/// when [`SCANOUT_REQ_AHB_HANDLE`] is also set), or a refusal with `-ENOENT`
/// when no handed-out buffer has that inode. Nothing is allocated or
/// registered. A daemon that predates the flag refuses the unknown format.
pub const SCANOUT_REQ_LOOKUP_INO: u32 = 1 << 30;

impl ScanoutAllocReq {
    pub const LEN: usize = 24;

    /// A lookup of the handed-out buffer whose dma-buf has inode `ino`,
    /// asking for its `AHardwareBuffer` too.
    #[must_use]
    pub fn lookup_ahb(ino: u64) -> Self {
        Self {
            width: 0,
            height: 0,
            format: SCANOUT_FORMAT_RGBA_8888 | SCANOUT_REQ_AHB_HANDLE | SCANOUT_REQ_LOOKUP_INO,
            len: ino,
        }
    }

    /// Whether the client asked for the `AHardwareBuffer` handle as well.
    #[must_use]
    pub fn wants_ahb_handle(&self) -> bool {
        self.format & SCANOUT_REQ_AHB_HANDLE != 0
    }

    /// Whether this is a lookup by inode ([`SCANOUT_REQ_LOOKUP_INO`]); the
    /// inode is `len`.
    #[must_use]
    pub fn is_lookup(&self) -> bool {
        self.format & SCANOUT_REQ_LOOKUP_INO != 0
    }

    /// The request with its flag bits cleared, i.e. the plain format.
    #[must_use]
    pub fn without_flags(self) -> Self {
        Self {
            format: self.format & !(SCANOUT_REQ_AHB_HANDLE | SCANOUT_REQ_LOOKUP_INO),
            ..self
        }
    }

    /// Whether this request carries a length instead of a geometry.
    #[must_use]
    pub fn is_length_only(&self) -> bool {
        self.width == 0 && self.height == 0 && self.len != 0
    }

    #[must_use]
    pub fn encode(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&SCANOUT_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&self.width.to_le_bytes());
        b[8..12].copy_from_slice(&self.height.to_le_bytes());
        b[12..16].copy_from_slice(&self.format.to_le_bytes());
        b[16..24].copy_from_slice(&self.len.to_le_bytes());
        b
    }

    /// `None` when `b` is not exactly one encoded request (short frame, bad
    /// magic, or trailing bytes).
    #[must_use]
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN || u32::from_le_bytes(b[0..4].try_into().ok()?) != SCANOUT_MAGIC {
            return None;
        }
        Some(Self {
            width: u32::from_le_bytes(b[4..8].try_into().ok()?),
            height: u32::from_le_bytes(b[8..12].try_into().ok()?),
            format: u32::from_le_bytes(b[12..16].try_into().ok()?),
            len: u64::from_le_bytes(b[16..24].try_into().ok()?),
        })
    }
}

/// Allocation response, accompanied by one dma-buf fd via `SCM_RIGHTS`.
///
/// Encoded little-endian, 24 bytes:
/// `magic:u32, ret:i32, ino:u64, stride:u32, reserved:u32`.
/// `ret` is 0 or negative errno; `ino`/`stride` are meaningless unless it is
/// 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanoutAllocResp {
    pub ret: i32,
    pub ino: u64,
    pub stride: u32,
}

impl ScanoutAllocResp {
    pub const LEN: usize = 24;

    #[must_use]
    pub fn encode(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&SCANOUT_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&self.ret.to_le_bytes());
        b[8..16].copy_from_slice(&self.ino.to_le_bytes());
        b[16..20].copy_from_slice(&self.stride.to_le_bytes());
        b
    }

    /// `None` when `b` is not exactly one encoded response.
    #[must_use]
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN || u32::from_le_bytes(b[0..4].try_into().ok()?) != SCANOUT_MAGIC {
            return None;
        }
        Some(Self {
            ret: i32::from_le_bytes(b[4..8].try_into().ok()?),
            ino: u64::from_le_bytes(b[8..16].try_into().ok()?),
            stride: u32::from_le_bytes(b[16..20].try_into().ok()?),
        })
    }
}

// ------------------------------------------------------- frame cap
//
// The user's fps cap is decided in the daemon and enforced in the ICD shim.
// The daemon holds the target (the app pushes it through `nativeSetFpsLimit`),
// and the scan-out channel is the only link the shim has to it. The shim holds
// the target it was last told and the only gate that can block the guest (the
// guest blocks in `vkAcquireNextImageKHR` until a slot frees), so it derives
// the release floor from the target itself, at each release it sees: one
// period (`1 / target`) since its own previous release. A per-buffer deadline
// cannot be handed out for that, because a hand-out happens once per pool
// buffer per session while a release happens every frame.
//
// A note therefore carries exactly one value, the current target, and no
// schedule. It rides the scan-out channel the shim already holds; no new
// socket and no second channel is introduced.
//
// Compatibility. The channel is a strict one-request/one-response exchange, so
// an unsolicited frame would desynchronise a shim that does not expect one.
// The daemon therefore sends a [`FrameCapNote`] only on a connection that has
// opted in with a [`FrameCapSubscribe`]: a shim that never subscribes sees
// exactly the frames it saw before, and a new shim against an older daemon
// simply never receives a note (its subscribe is logged and skipped, as every
// unknown magic already is). A mismatched pair therefore stays uncapped
// instead of faulting the scan-out allocator they still share.
//
// The shim's obligation, in two sentences. A note updates the target it
// enforces, and a release of a slot must not free it before one target period
// has passed since this shim's own previous release. That floor is at or below
// the guest's own interval whenever the target is at or above the guest's
// rate, so honouring it can never make a title slower than it would be
// uncapped: a release already at or after the floor is not delayed at all, and
// one before it is only moved up to the floor. The rate a title delivers is
// therefore its own rate when it is below the target, and the target when it is
// above.

/// Frame-cap note magic: ASCII "GNFC". Daemon -> shim.
pub const FRAME_CAP_MAGIC: u32 = 0x474E_4643;

/// Frame-cap subscribe magic: ASCII "GNCS". Shim -> daemon.
pub const FRAME_CAP_SUBSCRIBE_MAGIC: u32 = 0x474E_4353;

/// The `fps` of a [`FrameCapNote`] when no cap is in force: the guest presents
/// at its own rate.
pub const FPS_UNCAPPED: u32 = 0;

/// The `caps` bit a [`FrameCapSubscribe`] sets once the shim can honour
/// [`FrameCapNote`]: it enforces the release floor the target implies.
pub const CAP_FRAME_CAP: u32 = 1 << 0;

/// The `caps` bit a [`FrameCapSubscribe`] sets once the shim reads
/// [`VsyncNote`]. A daemon pushes the note only to a connection that set it: an
/// older shim's reply reader treats a frame it does not know as a dead channel.
pub const CAP_VSYNC_NOTE: u32 = 1 << 1;

/// The fps target, daemon -> shim, on the scan-out channel.
///
/// Encoded little-endian, 24 bytes:
/// `magic:u32, fps:u32, reserved:u64, reserved:u64`. The tail is reserved:
/// written as zero and ignored on decode. Its width is the shape this
/// capability was defined with, and a shim matches on the frame length and the
/// magic, so the two must not change under one.
///
/// `fps` is the current target, [`FPS_UNCAPPED`] when the limiter is off. The
/// shim adopts it on every note and derives the release floor from it; the
/// floor itself is not on the wire, because it is measured from the shim's own
/// previous release, which only the shim observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameCapNote {
    /// The current target, [`FPS_UNCAPPED`] when uncapped.
    pub fps: u32,
}

impl FrameCapNote {
    pub const LEN: usize = 24;

    #[must_use]
    pub fn encode(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&FRAME_CAP_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&self.fps.to_le_bytes());
        b
    }

    /// `None` when `b` is not exactly one encoded note (short frame, bad magic,
    /// or trailing bytes). The reserved tail carries no value and is not
    /// validated, so a note from a daemon that still wrote it decodes.
    #[must_use]
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN || u32::from_le_bytes(b[0..4].try_into().ok()?) != FRAME_CAP_MAGIC {
            return None;
        }
        Some(Self {
            fps: u32::from_le_bytes(b[4..8].try_into().ok()?),
        })
    }
}

/// The shim's opt-in to frame-cap notes, shim -> daemon, on the scan-out
/// channel.
///
/// Encoded little-endian, 24 bytes:
/// `magic:u32, caps:u32, reserved:u64, reserved:u64`.
/// The daemon latches the capability for the connection and answers with the
/// current target as a [`FrameCapNote`]; it pushes the same note again whenever
/// the target changes. `caps` is a bitmask of what the shim can honour:
/// [`CAP_FRAME_CAP`] and [`CAP_VSYNC_NOTE`]. A shim that never sends this is sent no
/// [`FrameCapNote`] -- one frame per request, as before -- which is what keeps an
/// older shim on the channel it expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameCapSubscribe {
    /// Capability bits: [`CAP_FRAME_CAP`], [`CAP_VSYNC_NOTE`].
    pub caps: u32,
}

impl FrameCapSubscribe {
    pub const LEN: usize = 24;

    #[must_use]
    pub fn encode(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&FRAME_CAP_SUBSCRIBE_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&self.caps.to_le_bytes());
        b
    }

    /// `None` when `b` is not exactly one encoded subscribe.
    #[must_use]
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN
            || u32::from_le_bytes(b[0..4].try_into().ok()?) != FRAME_CAP_SUBSCRIBE_MAGIC
        {
            return None;
        }
        Some(Self {
            caps: u32::from_le_bytes(b[4..8].try_into().ok()?),
        })
    }
}

/// Frame-acquired magic: ASCII "GNAQ". Shim -> daemon, on the scan-out channel.
///
/// One message per acquire would be a sendmsg per frame; this carries the running
/// total instead, so a message that cannot be sent (a full socket) loses
/// nothing — the next one carries the new total and the daemon reports the
/// difference. It is also why the app's frame ring can follow the rate the
/// limiter controls: a frame is counted when the guest starts it, not when the
/// compositor shows it.
pub const FRAME_ACQUIRED_MAGIC: u32 = 0x474E_4151;

/// Cumulative frames acquired on this connection.
///
/// Encoded little-endian, 24 bytes:
/// `magic:u32, reserved:u32, count:u64, reserved:u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameAcquired {
    /// Frames acquired since this shim connected.
    pub count: u64,
}

impl FrameAcquired {
    pub const LEN: usize = 24;

    #[must_use]
    pub fn encode(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&FRAME_ACQUIRED_MAGIC.to_le_bytes());
        b[8..16].copy_from_slice(&self.count.to_le_bytes());
        b
    }

    #[must_use]
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN
            || u32::from_le_bytes(b[0..4].try_into().ok()?) != FRAME_ACQUIRED_MAGIC
        {
            return None;
        }
        Some(Self {
            count: u64::from_le_bytes(b[8..16].try_into().ok()?),
        })
    }
}

/// Present-mark magic: ASCII "GNPM". Shim -> daemon, on the scan-out channel.
///
/// The shim tells the daemon which present id the frame it is about to commit
/// carries, so the daemon can name that id when the transaction completes. Sent
/// immediately before the commit reaches the compositor, so it is always ahead
/// of the atomic commit it belongs to. A frame with no id (an app that did not
/// chain [`VkPresentIdKHR`]) sends nothing.
pub const PRESENT_MARK_MAGIC: u32 = 0x474E_504D;

/// Present-done magic: ASCII "GNPD". Daemon -> shim, on the scan-out channel.
///
/// The daemon reports that the transaction carrying a marked frame completed,
/// with the panel's latch time for it. Only a connection that declared
/// [`CAP_PRESENT_ID`] is sent one.
pub const PRESENT_DONE_MAGIC: u32 = 0x474E_5044;

/// The `caps` bit a [`FrameCapSubscribe`] sets when the shim can send
/// [`PresentMark`] and read [`PresentDone`]. A daemon pushes no done note to a
/// connection that did not set it.
pub const CAP_PRESENT_ID: u32 = 1 << 2;

/// A frame's present id, shim -> daemon.
///
/// Encoded little-endian, 24 bytes:
/// `magic:u32, reserved:u32, ino:u64, id:u64`. `ino` is the scan-out inode the
/// frame's slot was allocated from; `id` is the present id the guest chained in
/// `VkPresentIdKHR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentMark {
    /// The scan-out inode of the buffer being committed.
    pub ino: u64,
    /// The guest's present id for this frame.
    pub id: u64,
}

impl PresentMark {
    pub const LEN: usize = 24;

    #[must_use]
    pub fn encode(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&PRESENT_MARK_MAGIC.to_le_bytes());
        b[8..16].copy_from_slice(&self.ino.to_le_bytes());
        b[16..24].copy_from_slice(&self.id.to_le_bytes());
        b
    }

    #[must_use]
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN || u32::from_le_bytes(b[0..4].try_into().ok()?) != PRESENT_MARK_MAGIC {
            return None;
        }
        Some(Self {
            ino: u64::from_le_bytes(b[8..16].try_into().ok()?),
            id: u64::from_le_bytes(b[16..24].try_into().ok()?),
        })
    }
}

/// A completed present, daemon -> shim.
///
/// Encoded little-endian, 24 bytes:
/// `magic:u32, reserved:u32, id:u64, latch_ns:u64`. `latch_ns` is the panel's
/// latch time for the transaction that carried the frame, in
/// `CLOCK_MONOTONIC` nanoseconds, or 0 when the platform reported none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentDone {
    /// The present id the frame was marked with.
    pub id: u64,
    /// The panel's latch time for it, 0 when unknown.
    pub latch_ns: u64,
}

impl PresentDone {
    pub const LEN: usize = 24;

    #[must_use]
    pub fn encode(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&PRESENT_DONE_MAGIC.to_le_bytes());
        b[8..16].copy_from_slice(&self.id.to_le_bytes());
        b[16..24].copy_from_slice(&self.latch_ns.to_le_bytes());
        b
    }

    #[must_use]
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN || u32::from_le_bytes(b[0..4].try_into().ok()?) != PRESENT_DONE_MAGIC {
            return None;
        }
        Some(Self {
            id: u64::from_le_bytes(b[8..16].try_into().ok()?),
            latch_ns: u64::from_le_bytes(b[16..24].try_into().ok()?),
        })
    }
}

/// Vsync note magic: ASCII "GNVS". Daemon -> shim.
pub const VSYNC_MAGIC: u32 = 0x474E_5653;

/// The panel's latch grid, daemon -> shim, on the scan-out channel.
///
/// Encoded little-endian, 24 bytes:
/// `magic:u32, reserved:u32, anchor_ns:u64, period_ns:u32, lead_ns:u32`.
///
/// `SurfaceFlinger` takes the newest buffer a layer holds at each of its latch
/// points, which sit one panel period apart. `anchor_ns` is one such point in
/// `CLOCK_MONOTONIC`, `period_ns` their spacing, and `lead_ns` how long before a
/// latch point a frame has to be committed to the compositor to be the buffer
/// that point takes. The daemon measures all three from its own publishes and
/// pushes a note whenever one of them moves. The period is the panel rate the
/// app reported; the phase and the lead are measured. A shim that never
/// receives one paces on its own clock, as before, and beats against the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsyncNote {
    /// A latch point, `CLOCK_MONOTONIC` ns.
    pub anchor_ns: u64,
    /// The spacing of latch points, ns.
    pub period_ns: u32,
    /// How far ahead of a latch point a commit has to be made to land on it, ns.
    pub lead_ns: u32,
}

impl VsyncNote {
    pub const LEN: usize = 24;

    #[must_use]
    pub fn encode(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&VSYNC_MAGIC.to_le_bytes());
        b[8..16].copy_from_slice(&self.anchor_ns.to_le_bytes());
        b[16..20].copy_from_slice(&self.period_ns.to_le_bytes());
        b[20..24].copy_from_slice(&self.lead_ns.to_le_bytes());
        b
    }

    /// `None` when `b` is not exactly one encoded note.
    #[must_use]
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN || u32::from_le_bytes(b[0..4].try_into().ok()?) != VSYNC_MAGIC {
            return None;
        }
        Some(Self {
            anchor_ns: u64::from_le_bytes(b[8..16].try_into().ok()?),
            period_ns: u32::from_le_bytes(b[16..20].try_into().ok()?),
            lead_ns: u32::from_le_bytes(b[20..24].try_into().ok()?),
        })
    }
}

/// Frame-generation settings, asked for by the shim and answered by the daemon.
///
/// One type, two directions: the shim sends a request (all fields zero) and the
/// daemon replies with the settings the app last pushed through
/// `GameNativeDrm.nativeSetLsfgConfig`. A request carries no fields so a shim can
/// ask without knowing the shape of the answer.
///
/// Encoded little-endian, 280 bytes: `magic:u32, enabled:u32, multiplier:u32,
/// flow_per_mille:u32, performance:u32, fps_limit:u32, dll:[u8; 256]`.
pub const LSFG_MAGIC: u32 = 0x474E_4C31;

/// Bytes of `Lossless.dll` path carried in a config, NUL-padded.
pub const LSFG_DLL_PATH_MAX: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LsfgConfig {
    /// Non-zero when the user wants frame generation. Explicit, because
    /// `multiplier` alone cannot express "off": a session that was started with
    /// the environment set would otherwise fall back to it and keep generating.
    pub enabled: u32,
    /// Generations per source frame, 2-4.
    pub multiplier: u32,
    /// Flow scale x 1000, so the wire stays integer-only.
    pub flow_per_mille: u32,
    /// Non-zero when the engine should use the lower-cost shader set.
    pub performance: u32,
    /// The user's cap on the **source** rate; the presenter presents at
    /// `fps_limit * multiplier`. 0 = uncapped.
    pub fps_limit: u32,
    /// Absolute path of the container's `Lossless.dll`, NUL-padded. It travels
    /// with the settings because the presenter runs in the app's process, where
    /// the container's environment is not present.
    pub dll: [u8; LSFG_DLL_PATH_MAX],
    /// The panel's refresh in millihertz. The engine paces against it and clamps
    /// its generations to what the display can afford (`floor(refresh/source)`
    /// frames per source frame); zero means unknown, which leaves it unclamped.
    pub refresh_mhz: u32,
}

impl Default for LsfgConfig {
    fn default() -> Self {
        Self {
            enabled: 0,
            multiplier: 0,
            flow_per_mille: 0,
            performance: 0,
            fps_limit: 0,
            dll: [0; LSFG_DLL_PATH_MAX],
            refresh_mhz: 0,
        }
    }
}

impl LsfgConfig {
    pub const LEN: usize = 24 + LSFG_DLL_PATH_MAX + 4;

    /// The DLL path as a string, or `None` when it is empty.
    #[must_use]
    pub fn dll_path(&self) -> Option<&std::ffi::CStr> {
        let end = self.dll.iter().position(|b| *b == 0)?;
        if end == 0 {
            return None;
        }
        std::ffi::CStr::from_bytes_with_nul(&self.dll[..=end]).ok()
    }

    /// Store a path, truncating one that does not fit rather than losing the
    /// rest of the settings with it.
    pub fn set_dll_path(&mut self, path: &str) {
        self.dll = [0; LSFG_DLL_PATH_MAX];
        let bytes = path.as_bytes();
        let len = bytes.len().min(LSFG_DLL_PATH_MAX - 1);
        self.dll[..len].copy_from_slice(&bytes[..len]);
    }

    /// What the shim sends: no fields, only the magic.
    #[must_use]
    pub fn request() -> Self {
        Self::default()
    }

    /// Whether these settings are still the untouched request shape.
    #[must_use]
    pub fn is_request(&self) -> bool {
        *self == Self::default()
    }

    #[must_use]
    pub fn encode(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&LSFG_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&self.enabled.to_le_bytes());
        b[8..12].copy_from_slice(&self.multiplier.to_le_bytes());
        b[12..16].copy_from_slice(&self.flow_per_mille.to_le_bytes());
        b[16..20].copy_from_slice(&self.performance.to_le_bytes());
        b[20..24].copy_from_slice(&self.fps_limit.to_le_bytes());
        b[24..24 + LSFG_DLL_PATH_MAX].copy_from_slice(&self.dll);
        b[24 + LSFG_DLL_PATH_MAX..].copy_from_slice(&self.refresh_mhz.to_le_bytes());
        b
    }

    /// `None` when `b` is not exactly one encoded frame.
    #[must_use]
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN || u32::from_le_bytes(b[0..4].try_into().ok()?) != LSFG_MAGIC {
            return None;
        }
        Some(Self {
            enabled: u32::from_le_bytes(b[4..8].try_into().ok()?),
            multiplier: u32::from_le_bytes(b[8..12].try_into().ok()?),
            flow_per_mille: u32::from_le_bytes(b[12..16].try_into().ok()?),
            performance: u32::from_le_bytes(b[16..20].try_into().ok()?),
            fps_limit: u32::from_le_bytes(b[20..24].try_into().ok()?),
            dll: {
                let mut dll = [0u8; LSFG_DLL_PATH_MAX];
                dll.copy_from_slice(&b[24..24 + LSFG_DLL_PATH_MAX]);
                dll
            },
            refresh_mhz: u32::from_le_bytes(b[24 + LSFG_DLL_PATH_MAX..].try_into().ok()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vsync_note_round_trips_and_rejects_other_magics() {
        let note = VsyncNote {
            anchor_ns: 0x0123_4567_89ab_cdef,
            period_ns: 8_333_333,
            lead_ns: 4_000_000,
        };
        let b = note.encode();
        assert_eq!(b.len(), VsyncNote::LEN);
        assert_eq!(VsyncNote::decode(&b), Some(note));
        assert_eq!(VsyncNote::decode(&b[..23]), None);
        assert_eq!(VsyncNote::decode(&FrameCapNote { fps: 60 }.encode()), None);
        assert_eq!(FrameCapNote::decode(&b), None);
    }

    /// Deterministic xorshift64 — no external deps for fuzz-ish coverage.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    #[test]
    fn req_hdr_round_trip() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..10_000 {
            let hdr = ReqHdr {
                total_len: (REQ_HDR_LEN as u32) + (rng.next() as u32 % 4096),
                ioctl_nr: rng.next() as u32,
                flags: rng.next() as u32 & FLAG_FD,
                seq: rng.next(),
            };
            assert_eq!(ReqHdr::decode(&hdr.encode()), Ok(hdr));
        }
    }

    #[test]
    fn resp_hdr_round_trip() {
        let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
        for _ in 0..10_000 {
            let hdr = RespHdr {
                total_len: (RESP_HDR_LEN as u32) + (rng.next() as u32 % 4096),
                seq: rng.next(),
                ret: (rng.next() as i32).clamp(-4095, 0),
                out_len: rng.next() as u32 % 4096,
            };
            assert_eq!(RespHdr::decode(&hdr.encode()), Ok(hdr));
        }
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(ReqHdr::decode(&[]), Err(WireError::Short));
        assert_eq!(ReqHdr::decode(&[0u8; 4]), Err(WireError::Short));

        let mut bad_magic = ReqHdr {
            total_len: REQ_HDR_LEN as u32,
            ioctl_nr: 0,
            flags: 0,
            seq: 0,
        }
        .encode();
        bad_magic[0] ^= 0xFF;
        assert_eq!(ReqHdr::decode(&bad_magic), Err(WireError::BadMagic));

        let mut bad_len = ReqHdr {
            total_len: REQ_HDR_LEN as u32,
            ioctl_nr: 0,
            flags: 0,
            seq: 0,
        }
        .encode();
        bad_len[4..8].copy_from_slice(&(u32::MAX).to_le_bytes());
        assert_eq!(ReqHdr::decode(&bad_len), Err(WireError::BadLength));

        bad_len[4..8].copy_from_slice(&((REQ_HDR_LEN as u32) - 1).to_le_bytes());
        assert_eq!(ReqHdr::decode(&bad_len), Err(WireError::BadLength));
    }

    #[test]
    fn scanout_alloc_round_trip() {
        let req = ScanoutAllocReq {
            width: 1280,
            height: 960,
            format: SCANOUT_FORMAT_RGBA_8888,
            len: 1280 * 960 * 4,
        };
        let enc = req.encode();
        assert_eq!(enc.len(), ScanoutAllocReq::LEN);
        assert_eq!(enc[0..4], SCANOUT_MAGIC.to_le_bytes());
        assert_eq!(ScanoutAllocReq::decode(&enc), Some(req));
        assert!(!req.is_length_only());

        // A length-only request resolves to the same bytes, differing only in
        // the two geometry fields.
        let by_len = ScanoutAllocReq {
            width: 0,
            height: 0,
            format: SCANOUT_FORMAT_RGBA_8888,
            len: 800 * 600 * 4,
        };
        assert_eq!(ScanoutAllocReq::decode(&by_len.encode()), Some(by_len));
        assert!(by_len.is_length_only());

        let resp = ScanoutAllocResp {
            ret: 0,
            ino: 0xDEAD_BEEF_1234_5678,
            stride: 5120,
        };
        let enc = resp.encode();
        assert_eq!(enc.len(), ScanoutAllocResp::LEN);
        assert_eq!(ScanoutAllocResp::decode(&enc), Some(resp));

        let err = ScanoutAllocResp {
            ret: -errno::EINVAL,
            ..Default::default()
        };
        assert_eq!(ScanoutAllocResp::decode(&err.encode()), Some(err));
    }

    #[test]
    fn scanout_alloc_wire_layout_is_pinned() {
        // These are the exact bytes the shim's packed structs produce, and the
        // C mirror asserts the same two struct sizes; pinning them here keeps
        // the two language sides from drifting silently.
        let req = ScanoutAllocReq {
            width: 1280,
            height: 960,
            format: SCANOUT_FORMAT_RGBA_8888,
            len: 1280 * 960 * 4,
        };
        assert_eq!(
            req.encode(),
            [
                0x31, 0x53, 0x4E, 0x47, // "GNS1"
                0x00, 0x05, 0x00, 0x00, // 1280
                0xC0, 0x03, 0x00, 0x00, // 960
                0x01, 0x00, 0x00, 0x00, // RGBA_8888
                0x00, 0x00, 0x4B, 0x00, 0x00, 0x00, 0x00, 0x00, // 4915200
            ]
        );
        let resp = ScanoutAllocResp {
            ret: 0,
            ino: 0xDEAD_BEEF_1234_5678,
            stride: 5120,
        };
        assert_eq!(
            resp.encode(),
            [
                0x31, 0x53, 0x4E, 0x47, // "GNS1"
                0x00, 0x00, 0x00, 0x00, // ret = 0
                0x78, 0x56, 0x34, 0x12, 0xEF, 0xBE, 0xAD, 0xDE, // ino
                0x00, 0x14, 0x00, 0x00, // 5120
                0x00, 0x00, 0x00, 0x00, // reserved
            ]
        );
    }

    #[test]
    fn scanout_lookup_carries_the_inode_and_both_flags() {
        let req = ScanoutAllocReq::lookup_ahb(0x1234_5678_9ABC);
        let dec = ScanoutAllocReq::decode(&req.encode()).unwrap();
        assert!(dec.is_lookup());
        assert!(dec.wants_ahb_handle());
        assert_eq!(dec.len, 0x1234_5678_9ABC);
        assert_eq!(dec.without_flags().format, SCANOUT_FORMAT_RGBA_8888);
        assert!(!dec.without_flags().is_lookup());
        // A daemon that predates the lookup clears only the handle flag and
        // sees a format it does not serve, so it refuses.
        assert_ne!(
            dec.format & !SCANOUT_REQ_AHB_HANDLE,
            SCANOUT_FORMAT_RGBA_8888
        );
        let alloc = ScanoutAllocReq {
            width: 64,
            height: 64,
            format: SCANOUT_FORMAT_RGBA_8888 | SCANOUT_REQ_AHB_HANDLE,
            len: 64 * 64 * 4,
        };
        assert!(!alloc.is_lookup());
    }

    #[test]
    fn scanout_alloc_rejects_bad_frames() {
        assert_eq!(ScanoutAllocReq::decode(&[]), None);
        assert_eq!(ScanoutAllocReq::decode(&[0u8; ScanoutAllocReq::LEN]), None);
        // Trailing bytes are a protocol violation, not a longer message.
        let mut extra = ScanoutAllocReq {
            width: 1,
            height: 1,
            format: SCANOUT_FORMAT_RGBA_8888,
            len: 4,
        }
        .encode()
        .to_vec();
        extra.push(0);
        assert_eq!(ScanoutAllocReq::decode(&extra), None);
        // A frame of the right magic but the wrong length is rejected too.
        let mut short = ScanoutAllocReq {
            width: 1,
            height: 1,
            format: SCANOUT_FORMAT_RGBA_8888,
            len: 4,
        }
        .encode()
        .to_vec();
        short.truncate(ScanoutAllocReq::LEN - 1);
        assert_eq!(ScanoutAllocReq::decode(&short), None);
        // The request carries the allocation length in the last eight bytes, so
        // it is now the same length as the response; the two are told apart by
        // direction on the channel (requests travel shim -> daemon, responses
        // daemon -> shim), not by shape. Nothing decodes a response as a
        // request on either side.
    }

    #[test]
    fn scanout_ahb_format_is_rgba_8888() {
        // HAL value 1 is `RGBA_8888` (R,G,B,A), the byte order the compositor's
        // `XB24` scan-out expects. Value 5 is `BGRA_8888` (B,G,R,A), which
        // would exchange red and blue on the panel.
        assert_eq!(scanout_ahb_format(ScanoutLayout::EightBit), 1);
        assert_eq!(SCANOUT_FORMAT_RGBA_8888, 1);
    }

    #[test]
    fn frame_cap_note_round_trip() {
        let note = FrameCapNote { fps: 120 };
        let enc = note.encode();
        assert_eq!(enc.len(), FrameCapNote::LEN);
        assert_eq!(enc[0..4], FRAME_CAP_MAGIC.to_le_bytes());
        assert_eq!(FrameCapNote::decode(&enc), Some(note));

        // The reserved tail carries nothing: the note is the target alone, and
        // the uncapped target is FPS_UNCAPPED.
        assert_eq!(FrameCapNote::decode(&note.encode()), Some(note));
        let uncapped = FrameCapNote { fps: FPS_UNCAPPED };
        assert_eq!(FrameCapNote::decode(&uncapped.encode()), Some(uncapped));

        // Its magic is its own: no other shape on the channel decodes as a note.
        assert_ne!(FRAME_CAP_MAGIC, SCANOUT_MAGIC);
        assert_ne!(FRAME_CAP_MAGIC, FRAME_CAP_SUBSCRIBE_MAGIC);
        assert_eq!(FrameCapNote::decode(&[0u8; FrameCapNote::LEN]), None);
        assert_eq!(FrameCapNote::decode(&[]), None);
        let mut extra = note.encode().to_vec();
        extra.push(0);
        assert_eq!(FrameCapNote::decode(&extra), None);
        let mut short = note.encode().to_vec();
        short.truncate(FrameCapNote::LEN - 1);
        assert_eq!(FrameCapNote::decode(&short), None);
    }

    #[test]
    fn frame_cap_note_wire_layout_is_pinned() {
        // The exact bytes the shim matches on: magic "GNFC", then fps, then the
        // reserved tail, all LE. A note whose tail a daemon still writes
        // decodes to the same target, which is what keeps the shipped frame
        // shape usable in either direction.
        let note = FrameCapNote { fps: 60 };
        let mut pinned = [0u8; FrameCapNote::LEN];
        pinned[0..4].copy_from_slice(&[0x43, 0x46, 0x4E, 0x47]); // "GNFC"
        pinned[4..8].copy_from_slice(&[0x3C, 0x00, 0x00, 0x00]); // 60
        assert_eq!(note.encode(), pinned);
        for tail in [
            [0u8; 16],
            [
                0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13,
                0x12, 0x11,
            ],
        ] {
            let mut bytes = pinned;
            bytes[8..24].copy_from_slice(&tail);
            assert_eq!(FrameCapNote::decode(&bytes), Some(note));
        }
    }

    #[test]
    fn frame_cap_subscribe_round_trip() {
        let sub = FrameCapSubscribe {
            caps: CAP_FRAME_CAP,
        };
        let enc = sub.encode();
        assert_eq!(enc.len(), FrameCapSubscribe::LEN);
        assert_eq!(enc[0..4], FRAME_CAP_SUBSCRIBE_MAGIC.to_le_bytes());
        assert_eq!(FrameCapSubscribe::decode(&enc), Some(sub));
        assert_eq!(
            FrameCapSubscribe::decode(&[0u8; FrameCapSubscribe::LEN]),
            None
        );
        // A subscribe never decodes as a note and vice versa.
        assert_eq!(FrameCapNote::decode(&enc), None);
        assert_eq!(FrameCapSubscribe::decode(&sub.encode()), Some(sub));
    }
}
