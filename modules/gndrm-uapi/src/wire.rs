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
    /// 8-bit B,G,R,A. The pool's only layout today.
    EightBit,
}

/// HAL `AHardwareBuffer` format for `layout`.
///
/// `EightBit` is **`AHARDWAREBUFFER_FORMAT_BGRA_8888`** (HAL value 5): B,G,R,A
/// in memory. That is the byte order of the `XR24` (`0x34325258`) the
/// compositor commits for scan-out, whose fourth byte is ignored. It is *not*
/// `R8G8B8X8_UNORM` (value 2), which is R,G,B,X: the two share a stride and
/// nothing else, so allocating the pool as value 2 exchanges red and blue
/// between the guest's frame and the panel.
///
/// There is no alpha-less B,G,R entry in the HAL enum — value 5 is the only
/// B,G,R form — so "declare the fourth byte as X" is not a route the platform
/// offers. The alpha hypothesis for the presented-content defect is falsified
/// by measurement: the guest writes a don't-care fourth byte (Nidhogg's frames
/// are ~92.9% alpha-zero, the rest alpha-1), yet the panel is ~94% dark with
/// either declaration, so that zero-alpha statistic is not the cause of the
/// missing background.
///
/// The 10-bit/HDR entry (`R10G10B10A2_UNORM`-class) belongs here as a
/// `TenBit` arm once the DRM side carries what it needs — 10-bit plane
/// formats, the `Colorspace` property, `HDR_OUTPUT_METADATA`, HDR EDID — none
/// of which exist yet.
#[must_use]
pub const fn scanout_ahb_format(layout: ScanoutLayout) -> u32 {
    match layout {
        ScanoutLayout::EightBit => 5, // AHARDWAREBUFFER_FORMAT_BGRA_8888
    }
}

/// The format the scan-out path requires today, i.e.
/// [`scanout_ahb_format`]`(ScanoutLayout::EightBit)`.
pub const SCANOUT_FORMAT_BGRA_8888: u32 = scanout_ahb_format(ScanoutLayout::EightBit);

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

impl ScanoutAllocReq {
    pub const LEN: usize = 24;

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

#[cfg(test)]
mod tests {
    use super::*;

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
            format: SCANOUT_FORMAT_BGRA_8888,
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
            format: SCANOUT_FORMAT_BGRA_8888,
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
            format: SCANOUT_FORMAT_BGRA_8888,
            len: 1280 * 960 * 4,
        };
        assert_eq!(
            req.encode(),
            [
                0x31, 0x53, 0x4E, 0x47, // "GNS1"
                0x00, 0x05, 0x00, 0x00, // 1280
                0xC0, 0x03, 0x00, 0x00, // 960
                0x05, 0x00, 0x00, 0x00, // BGRA_8888
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
    fn scanout_alloc_rejects_bad_frames() {
        assert_eq!(ScanoutAllocReq::decode(&[]), None);
        assert_eq!(ScanoutAllocReq::decode(&[0u8; ScanoutAllocReq::LEN]), None);
        // Trailing bytes are a protocol violation, not a longer message.
        let mut extra = ScanoutAllocReq {
            width: 1,
            height: 1,
            format: SCANOUT_FORMAT_BGRA_8888,
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
            format: SCANOUT_FORMAT_BGRA_8888,
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
    fn scanout_ahb_format_is_bgra_8888() {
        // HAL value 5 is `BGRA_8888` (B,G,R,A), the byte order the compositor's
        // `XR24` scan-out expects. Value 2 is `R8G8B8X8_UNORM` (R,G,B,X), which
        // would exchange red and blue on the panel.
        assert_eq!(scanout_ahb_format(ScanoutLayout::EightBit), 5);
        assert_eq!(SCANOUT_FORMAT_BGRA_8888, 5);
    }
}
