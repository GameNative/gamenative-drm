//! Panel-frame dump conversion: the pure half of the presenter's
//! `gn-fb-latest.ppm` diagnostic (see `asc::dump_published`).
//!
//! Android-free so it is unit-tested on the host (`tests/dump.rs` compiles this
//! same file, mirroring `log_policy.rs`); the crate is otherwise
//! `cfg(target_os = "android")` and empty on the host.

/// Bytes per pixel of the pool format (`SCANOUT_FORMAT_RGBA_8888`).
const BPP: usize = 4;

/// Build a binary P6 PPM from an RGBA image whose rows are `stride` bytes
/// apart. `src` must hold at least `stride * height` bytes, and `stride` must
/// be at least `width * 4` (the caller takes both from the locked buffer).
///
/// Byte 0 of each pixel is red — the pool's layout is RGBA in memory — so
/// bytes `0,1,2` are written as P6 `R,G,B` and the fourth byte is dropped.
#[must_use]
pub(crate) fn ppm_rgba(src: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * 3 + 16);
    out.extend_from_slice(format!("{}", gndrm_obf::obf!("P6\n{width} {height}\n255\n")).as_bytes());
    for row in src.chunks(stride).take(height) {
        for x in 0..width {
            let px = &row[x * BPP..];
            out.extend_from_slice(&[px[0], px[1], px[2]]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x3 image with `stride = 16` (4 pixels: 2 real + 2 padding bytes'
    /// worth of opaque pixels that must be skipped). Every pixel gets a
    /// distinct RGBA value so a swapped or mis-strided byte is caught.
    #[test]
    fn ppm_rgba_emits_rgb_in_channel_and_row_order() {
        let width = 2;
        let height = 3;
        let stride = 16;
        // Each row: (R,G,B,A) for x=0 then x=1, then a padding pixel.
        let rows: [[u8; 4]; 4] = [
            // R    G    B    A
            [30, 20, 10, 255],    // x=0 -> R=30 G=20 B=10
            [60, 50, 40, 255],    // x=1 -> R=60 G=50 B=40
            [200, 201, 202, 255], // padding, must not appear
            [203, 204, 205, 255], // padding, must not appear
        ];
        let mut src = Vec::new();
        for _ in 0..height {
            for px in rows {
                src.extend_from_slice(&px);
            }
        }

        let ppm = ppm_rgba(&src, width, height, stride);
        assert_eq!(&ppm[..11], b"P6\n2 3\n255\n");

        let body = &ppm[11..];
        assert_eq!(
            body,
            &[
                30, 20, 10, 60, 50, 40, // row 0
                30, 20, 10, 60, 50, 40, // row 1
                30, 20, 10, 60, 50, 40, // row 2
            ],
            "rows must be RGB, in order, with the padding pixel skipped"
        );
    }

    #[test]
    fn ppm_rgba_honours_a_stride_wider_than_the_row() {
        // A single 1x1 image whose row is padded out to 8 bytes: the red byte
        // is still byte 0 of the first pixel, not of the padding.
        let src = [9u8, 8, 7, 255, 111, 112, 113, 114];
        let ppm = ppm_rgba(&src, 1, 1, 8);
        assert_eq!(&ppm[..11], b"P6\n1 1\n255\n");
        assert_eq!(&ppm[11..], &[9, 8, 7]);
    }
}
