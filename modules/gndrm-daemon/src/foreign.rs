//! Copy plan for a committed framebuffer that is not backed by the scan-out
//! pool.
//!
//! The presenter can only put its own `AHardwareBuffer`s on the layer. A
//! client that renders into memory the shim did not redirect (an allocator or
//! kernel interface the shim does not hook) commits a dma-buf the pool has
//! never seen, and without a fallback that frame is dropped. The presenter
//! copies such a buffer into a pool-format buffer of its own instead; this
//! module decides whether the copy is possible and moves the rows, so the
//! layout rules are host-testable.

use gndrm_uapi::mode::{
    DRM_FORMAT_ABGR2101010, DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB2101010, DRM_FORMAT_ARGB8888,
    DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XBGR2101010, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XRGB8888, DRM_MODE_FB_MODIFIERS,
};

use crate::Framebuffer;

/// `DRM_FORMAT_MOD_INVALID`: an explicit "no modifier" that some clients pass
/// with the modifiers flag set. Treated like an implicit modifier.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// How a foreign framebuffer's rows map onto the copy target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyPlan {
    pub width: u32,
    pub height: u32,
    /// Bytes of pixel data per row (`width * 4`).
    pub row_bytes: usize,
    /// Byte offset of the first row in the source dma-buf.
    pub src_offset: usize,
    /// Source row pitch in bytes.
    pub src_pitch: usize,
    /// The pixels are a 2:10:10:10 layout, so the target must be the 10-bit
    /// buffer format; otherwise the 8-bit one.
    pub ten_bit: bool,
}

/// Why a foreign framebuffer cannot be copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyRefusal {
    /// A tiled or compressed layout: the rows are not addressable by pitch.
    Modifier(u64),
    /// A pixel format that is not 32 bits per pixel.
    Format(u32),
    /// A second plane, a zero size, or a pitch shorter than a row.
    Geometry,
    /// The rows reach past the end of the dma-buf.
    OutOfBounds { need: u64, have: u64 },
}

impl core::fmt::Display for CopyRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Modifier(m) => write!(f, "{}", gndrm_obf::obf!("modifier {m:#x} is not linear")),
            Self::Format(fourcc) => write!(
                f,
                "{}",
                gndrm_obf::obf!("format {} is not 32 bpp", fourcc_name(*fourcc))
            ),
            Self::Geometry => write!(f, "{}", gndrm_obf::obf!("unsupported plane geometry")),
            Self::OutOfBounds { need, have } => write!(
                f,
                "{}",
                gndrm_obf::obf!("rows need {need} bytes, dma-buf has {have}")
            ),
        }
    }
}

/// Decide whether `fb`, whose first plane lives in a dma-buf of `src_len`
/// bytes, can be copied row by row into a pool-format buffer.
///
/// # Errors
/// The reason the copy is not possible.
pub fn plan_copy(fb: &Framebuffer, src_len: u64) -> Result<CopyPlan, CopyRefusal> {
    if fb.flags & DRM_MODE_FB_MODIFIERS != 0 {
        let m = fb.modifier[0];
        if m != DRM_FORMAT_MOD_LINEAR && m != DRM_FORMAT_MOD_INVALID {
            return Err(CopyRefusal::Modifier(m));
        }
    }
    let ten_bit = match fb.pixel_format {
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 | DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => {
            false
        }
        DRM_FORMAT_XRGB2101010
        | DRM_FORMAT_XBGR2101010
        | DRM_FORMAT_ARGB2101010
        | DRM_FORMAT_ABGR2101010 => true,
        other => return Err(CopyRefusal::Format(other)),
    };
    if fb.width == 0 || fb.height == 0 || fb.handles[1] != 0 {
        return Err(CopyRefusal::Geometry);
    }
    let row_bytes = u64::from(fb.width) * 4;
    let pitch = u64::from(fb.pitches[0]);
    if pitch < row_bytes {
        return Err(CopyRefusal::Geometry);
    }
    let need = u64::from(fb.offsets[0]) + pitch * u64::from(fb.height - 1) + row_bytes;
    if need > src_len {
        return Err(CopyRefusal::OutOfBounds {
            need,
            have: src_len,
        });
    }
    Ok(CopyPlan {
        width: fb.width,
        height: fb.height,
        row_bytes: row_bytes as usize,
        src_offset: fb.offsets[0] as usize,
        src_pitch: pitch as usize,
        ten_bit,
    })
}

impl CopyPlan {
    /// Bytes a target with row pitch `dst_pitch` must expose.
    #[must_use]
    pub fn dst_len(&self, dst_pitch: usize) -> usize {
        dst_pitch * (self.height as usize - 1) + self.row_bytes
    }

    /// Copy the rows from `src` (the whole mapped dma-buf) into `dst` (the
    /// locked target, row pitch `dst_pitch`). Both pitches are honoured; a run
    /// with equal, unpadded pitches is one copy.
    ///
    /// # Panics
    /// When either slice is shorter than the plan requires; the caller checks
    /// both against [`plan_copy`] and [`Self::dst_len`].
    pub fn copy_rows(&self, src: &[u8], dst: &mut [u8], dst_pitch: usize) {
        let rows = self.height as usize;
        if self.src_pitch == dst_pitch && dst_pitch == self.row_bytes {
            let n = self.row_bytes * rows;
            dst[..n].copy_from_slice(&src[self.src_offset..self.src_offset + n]);
            return;
        }
        for y in 0..rows {
            let s = self.src_offset + y * self.src_pitch;
            let d = y * dst_pitch;
            dst[d..d + self.row_bytes].copy_from_slice(&src[s..s + self.row_bytes]);
        }
    }
}

/// A fourcc as its four characters, for log lines.
#[must_use]
pub fn fourcc_name(fourcc: u32) -> String {
    fourcc
        .to_le_bytes()
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fb(width: u32, height: u32, fourcc: u32, pitch: u32, offset: u32) -> Framebuffer {
        Framebuffer {
            id: 1,
            width,
            height,
            pixel_format: fourcc,
            flags: 0,
            handles: [1, 0, 0, 0],
            pitches: [pitch, 0, 0, 0],
            offsets: [offset, 0, 0, 0],
            modifier: [0; 4],
        }
    }

    #[test]
    fn linear_eight_bit_plans() {
        let p = plan_copy(&fb(4, 3, DRM_FORMAT_XRGB8888, 32, 0), 96).unwrap();
        assert_eq!(p.row_bytes, 16);
        assert_eq!(p.src_pitch, 32);
        assert!(!p.ten_bit);
        assert!(
            plan_copy(&fb(4, 3, DRM_FORMAT_XBGR2101010, 16, 0), 48)
                .unwrap()
                .ten_bit
        );
    }

    #[test]
    fn refusals() {
        let mut tiled = fb(4, 3, DRM_FORMAT_XRGB8888, 16, 0);
        tiled.flags = DRM_MODE_FB_MODIFIERS;
        tiled.modifier[0] = 0x0500_0000_0000_0001;
        assert_eq!(
            plan_copy(&tiled, 1 << 20),
            Err(CopyRefusal::Modifier(0x0500_0000_0000_0001))
        );
        tiled.modifier[0] = DRM_FORMAT_MOD_INVALID;
        assert!(plan_copy(&tiled, 1 << 20).is_ok());
        assert!(matches!(
            plan_copy(
                &fb(4, 3, gndrm_uapi::mode::DRM_FORMAT_RGB565, 8, 0),
                1 << 20
            ),
            Err(CopyRefusal::Format(_))
        ));
        assert_eq!(
            plan_copy(&fb(4, 3, DRM_FORMAT_XRGB8888, 8, 0), 1 << 20),
            Err(CopyRefusal::Geometry)
        );
        assert_eq!(
            plan_copy(&fb(4, 3, DRM_FORMAT_XRGB8888, 16, 4), 48),
            Err(CopyRefusal::OutOfBounds { need: 52, have: 48 })
        );
    }

    #[test]
    fn copy_honours_both_pitches() {
        let plan = plan_copy(&fb(2, 3, DRM_FORMAT_XRGB8888, 12, 4), 64).unwrap();
        let src: Vec<u8> = (0..64u8).collect();
        let mut dst = vec![0xaa; plan.dst_len(16)];
        plan.copy_rows(&src, &mut dst, 16);
        for y in 0..3 {
            let s = 4 + y * 12;
            assert_eq!(&dst[y * 16..y * 16 + 8], &src[s..s + 8]);
            if y < 2 {
                assert!(dst[y * 16 + 8..y * 16 + 16].iter().all(|&b| b == 0xaa));
            }
        }
    }

    #[test]
    fn tight_copy_is_whole() {
        let plan = plan_copy(&fb(2, 2, DRM_FORMAT_ABGR8888, 8, 0), 16).unwrap();
        let src: Vec<u8> = (0..16u8).collect();
        let mut dst = vec![0; 16];
        plan.copy_rows(&src, &mut dst, 8);
        assert_eq!(dst, src);
    }

    #[test]
    fn names_fourcc() {
        assert_eq!(fourcc_name(DRM_FORMAT_XRGB8888), "XR24");
    }
}
