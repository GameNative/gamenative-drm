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
//!
//! The 8-bit target is in the session's scan-out layout and the 10-bit target
//! is R-low (`R10G10B10A2_UNORM`); a source in the other channel order has red
//! and blue exchanged during the copy.

use gndrm_uapi::mode::{
    DRM_FORMAT_ABGR2101010, DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB2101010, DRM_FORMAT_ARGB8888,
    DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XBGR2101010, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XRGB8888, DRM_MODE_FB_MODIFIERS,
};

use gndrm_uapi::wire::ScanoutLayout;

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
    /// The source's channel order differs from the target's, so red and blue
    /// are exchanged during the copy.
    pub swap_rb: bool,
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
/// bytes, can be copied row by row into a pool-format buffer of `layout`.
///
/// # Errors
/// The reason the copy is not possible.
pub fn plan_copy(
    fb: &Framebuffer,
    src_len: u64,
    layout: ScanoutLayout,
) -> Result<CopyPlan, CopyRefusal> {
    if fb.flags & DRM_MODE_FB_MODIFIERS != 0 {
        let m = fb.modifier[0];
        if m != DRM_FORMAT_MOD_LINEAR && m != DRM_FORMAT_MOD_INVALID {
            return Err(CopyRefusal::Modifier(m));
        }
    }
    let r_low_target = layout == ScanoutLayout::Rgba8888;
    let (ten_bit, swap_rb) = match fb.pixel_format {
        DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => (false, !r_low_target),
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 => (false, r_low_target),
        DRM_FORMAT_XBGR2101010 | DRM_FORMAT_ABGR2101010 => (true, false),
        DRM_FORMAT_XRGB2101010 | DRM_FORMAT_ARGB2101010 => (true, true),
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
        swap_rb,
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
    /// with equal, unpadded pitches is one copy. With [`Self::swap_rb`] red
    /// and blue are exchanged per pixel.
    ///
    /// # Panics
    /// When either slice is shorter than the plan requires; the caller checks
    /// both against [`plan_copy`] and [`Self::dst_len`].
    pub fn copy_rows(&self, src: &[u8], dst: &mut [u8], dst_pitch: usize) {
        match (self.swap_rb, self.ten_bit) {
            (false, _) => self.copy_plain(src, dst, dst_pitch),
            (true, false) => self.copy_swapped(src, dst, dst_pitch, swap_rb_8),
            (true, true) => self.copy_swapped(src, dst, dst_pitch, swap_rb_10),
        }
    }

    fn copy_plain(&self, src: &[u8], dst: &mut [u8], dst_pitch: usize) {
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

    #[inline]
    fn copy_swapped(&self, src: &[u8], dst: &mut [u8], dst_pitch: usize, swap: fn(u32) -> u32) {
        for y in 0..self.height as usize {
            let s = self.src_offset + y * self.src_pitch;
            let d = y * dst_pitch;
            let src_row = &src[s..s + self.row_bytes];
            let dst_row = &mut dst[d..d + self.row_bytes];
            let (dst_px, _) = dst_row.as_chunks_mut::<4>();
            let (src_px, _) = src_row.as_chunks::<4>();
            for (o, i) in dst_px.iter_mut().zip(src_px) {
                *o = swap(u32::from_le_bytes(*i)).to_le_bytes();
            }
        }
    }
}

/// Exchange the 8-bit channels in bytes 0 and 2 of a 32-bit pixel.
#[inline(always)]
fn swap_rb_8(v: u32) -> u32 {
    (v & 0xff00_ff00) | ((v >> 16) & 0xff) | ((v & 0xff) << 16)
}

/// Exchange the 10-bit channels at bits 0-9 and 20-29 of a 2:10:10:10 pixel.
#[inline(always)]
fn swap_rb_10(v: u32) -> u32 {
    (v & 0xc00f_fc00) | ((v >> 20) & 0x3ff) | ((v & 0x3ff) << 20)
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

    const BGRA: ScanoutLayout = ScanoutLayout::Bgra8888;
    const RGBA: ScanoutLayout = ScanoutLayout::Rgba8888;

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
        let p = plan_copy(&fb(4, 3, DRM_FORMAT_XRGB8888, 32, 0), 96, RGBA).unwrap();
        assert_eq!(p.row_bytes, 16);
        assert_eq!(p.src_pitch, 32);
        assert!(!p.ten_bit);
        assert!(
            plan_copy(&fb(4, 3, DRM_FORMAT_XBGR2101010, 16, 0), 48, BGRA)
                .unwrap()
                .ten_bit
        );
    }

    #[test]
    fn swizzle_follows_the_layout() {
        let swaps = |fourcc, layout| {
            plan_copy(&fb(4, 3, fourcc, 16, 0), 48, layout)
                .unwrap()
                .swap_rb
        };
        for fourcc in [DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888] {
            assert!(!swaps(fourcc, BGRA));
            assert!(swaps(fourcc, RGBA));
        }
        for fourcc in [DRM_FORMAT_XBGR8888, DRM_FORMAT_ABGR8888] {
            assert!(swaps(fourcc, BGRA));
            assert!(!swaps(fourcc, RGBA));
        }
        for layout in [BGRA, RGBA] {
            assert!(swaps(DRM_FORMAT_XRGB2101010, layout));
            assert!(swaps(DRM_FORMAT_ARGB2101010, layout));
            assert!(!swaps(DRM_FORMAT_XBGR2101010, layout));
            assert!(!swaps(DRM_FORMAT_ABGR2101010, layout));
        }
    }

    #[test]
    fn refusals() {
        let mut tiled = fb(4, 3, DRM_FORMAT_XRGB8888, 16, 0);
        tiled.flags = DRM_MODE_FB_MODIFIERS;
        tiled.modifier[0] = 0x0500_0000_0000_0001;
        assert_eq!(
            plan_copy(&tiled, 1 << 20, BGRA),
            Err(CopyRefusal::Modifier(0x0500_0000_0000_0001))
        );
        tiled.modifier[0] = DRM_FORMAT_MOD_INVALID;
        assert!(plan_copy(&tiled, 1 << 20, BGRA).is_ok());
        assert!(matches!(
            plan_copy(
                &fb(4, 3, gndrm_uapi::mode::DRM_FORMAT_RGB565, 8, 0),
                1 << 20,
                BGRA
            ),
            Err(CopyRefusal::Format(_))
        ));
        assert_eq!(
            plan_copy(&fb(4, 3, DRM_FORMAT_XRGB8888, 8, 0), 1 << 20, BGRA),
            Err(CopyRefusal::Geometry)
        );
        assert_eq!(
            plan_copy(&fb(4, 3, DRM_FORMAT_XRGB8888, 16, 4), 48, BGRA),
            Err(CopyRefusal::OutOfBounds { need: 52, have: 48 })
        );
    }

    #[test]
    fn copy_honours_both_pitches() {
        let plan = plan_copy(&fb(2, 3, DRM_FORMAT_XBGR8888, 12, 4), 64, RGBA).unwrap();
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
        let plan = plan_copy(&fb(2, 2, DRM_FORMAT_ABGR8888, 8, 0), 16, RGBA).unwrap();
        let src: Vec<u8> = (0..16u8).collect();
        let mut dst = vec![0; 16];
        plan.copy_rows(&src, &mut dst, 8);
        assert_eq!(dst, src);
    }

    #[test]
    fn opposite_order_eight_bit_swaps_red_and_blue() {
        for (fourcc, layout) in [
            (DRM_FORMAT_XRGB8888, RGBA),
            (DRM_FORMAT_ARGB8888, RGBA),
            (DRM_FORMAT_XBGR8888, BGRA),
            (DRM_FORMAT_ABGR8888, BGRA),
        ] {
            let plan = plan_copy(&fb(2, 2, fourcc, 12, 4), 64, layout).unwrap();
            let src: Vec<u8> = (0..64u8).collect();
            let mut dst = vec![0xaa; plan.dst_len(8)];
            plan.copy_rows(&src, &mut dst, 8);
            for y in 0..2 {
                for x in 0..2 {
                    let s = &src[4 + y * 12 + x * 4..][..4];
                    let d = &dst[y * 8 + x * 4..][..4];
                    assert_eq!(d, [s[2], s[1], s[0], s[3]]);
                }
            }
        }
    }

    #[test]
    fn b_low_ten_bit_swaps_red_and_blue() {
        let pack = |a: u32, hi: u32, g: u32, lo: u32| (a << 30) | (hi << 20) | (g << 10) | lo;
        let px = [(0x2, 0x0a5, 0x155, 0x3ff), (0x1, 0x200, 0x3ff, 0x001)];
        for fourcc in [DRM_FORMAT_XRGB2101010, DRM_FORMAT_ARGB2101010] {
            let plan = plan_copy(&fb(2, 1, fourcc, 8, 0), 8, BGRA).unwrap();
            assert!(plan.ten_bit && plan.swap_rb);
            let src: Vec<u8> = px
                .iter()
                .flat_map(|&(a, r, g, b)| pack(a, r, g, b).to_le_bytes())
                .collect();
            let want: Vec<u8> = px
                .iter()
                .flat_map(|&(a, r, g, b)| pack(a, b, g, r).to_le_bytes())
                .collect();
            let mut dst = vec![0; 8];
            plan.copy_rows(&src, &mut dst, 8);
            assert_eq!(dst, want);
        }
        let xb30 = pack(0x2, 0x3ff, 0x155, 0x0a5);
        let plan = plan_copy(&fb(1, 1, DRM_FORMAT_XBGR2101010, 4, 0), 4, BGRA).unwrap();
        assert!(!plan.swap_rb);
        let mut dst = vec![0; 4];
        plan.copy_rows(&xb30.to_le_bytes(), &mut dst, 4);
        assert_eq!(dst, xb30.to_le_bytes());
    }

    #[test]
    fn names_fourcc() {
        assert_eq!(fourcc_name(DRM_FORMAT_XRGB8888), "XR24");
    }
}
