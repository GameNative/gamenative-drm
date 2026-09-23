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
    pub const ERANGE: i32 = 34;
    pub const ENOSYS: i32 = 38;
    pub const ENOTSUP: i32 = 95;
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
}
