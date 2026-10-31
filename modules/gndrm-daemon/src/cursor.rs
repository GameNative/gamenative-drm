//! How the cursor plane is handed to the display's composer.
//!
//! The Android presenter puts the cursor plane on a small layer of its own
//! above the scan-out layer. Handed over as KMS describes it (a 256x256 padded
//! buffer, scaled by the scan-out fit, buffer transform 0), the composer sees a
//! scaled layer that also carries the display's rotation whenever the panel is
//! run rotated (a portrait panel in landscape use: SurfaceFlinger's
//! `ROTATION_90`). On the Retroid Pocket 6 (SM8550) such a layer has no
//! hardware pipe left: the inline rotator takes only UBWC, so a rotated linear
//! layer has to go through the single inline writeback path, which the scan-out
//! layer already owns, and the cursor falls back to GPU composition.
//!
//! So the presenter stages the cursor instead: the cursor's opaque extent is
//! copied out of the padded buffer, scaled to the size it is shown at, and
//! rotated clockwise by the display rotation; the layer gets the inverse buffer
//! transform. In window space the two cancel (the picture is the plane's,
//! whatever rotation was assumed), and the composer, which applies the display
//! rotation on top of the buffer transform, sees an unrotated, unscaled layer.
//!
//! The rotation itself is derived here (see [`derive_rotation`]); the pixel and
//! rect work is pure so it is host-tested.

use crate::scanout::Edges;

/// The display rotation assumed for the cursor layer, in degrees clockwise.
///
/// No public NDK call reports the display's rotation relative to the panel's
/// natural orientation, and the presenter only knows the window it draws into.
/// So the panel's natural orientation is assumed (`natural_landscape`: the
/// presenter takes tablets to be landscape panels and everything else to be
/// portrait panels, the phone and handheld case) and the rotation is 90 when the
/// window's orientation differs from it, 0 when it matches. A square or empty
/// window is taken as unrotated.
///
/// Limitation: 90 and 270 cannot be told apart this way (a device held the other
/// way round is `ROTATION_270`), and a panel whose natural orientation is not
/// the assumed one gets the wrong rotation. Neither changes the picture (the
/// staging cancels out in window space); the composer then sees a residual
/// rotation (180 for 270, 90/270 for a wrong natural orientation), which some
/// composers handle on a plain pipe (a 180 is two flips) and some send to the
/// GPU, as with an unstaged cursor.
#[must_use]
pub fn derive_rotation(window: (u32, u32), natural_landscape: bool) -> u32 {
    let (w, h) = window;
    if w == 0 || h == 0 || w == h {
        return 0;
    }
    if (w > h) == natural_landscape {
        0
    } else {
        90
    }
}

/// Snap an override in degrees to 0/90/180/270 (anything else rounds down to
/// the quarter turn below it, modulo 360).
#[must_use]
pub fn normalize_rotation(deg: u32) -> u32 {
    deg % 360 / 90 * 90
}

/// The buffer transform (`NATIVE_WINDOW_TRANSFORM_*`) that undoes a clockwise
/// pre-rotation of `rot` degrees.
#[must_use]
pub fn inverse_rotation_transform(rot: u32) -> i32 {
    match normalize_rotation(rot) {
        90 => 7,  // ROT_270
        180 => 3, // ROT_180
        270 => 4, // ROT_90
        _ => 0,
    }
}

/// The cursor's opaque extent inside `src` (buffer pixels,
/// `(left, top, right, bottom)`), measured from the rect's top-left corner so
/// that corner, and with it the hotspot offset, stays where KMS put it.
/// `pixels` is tightly packed 4-byte pixels `width` wide with alpha in byte 3.
/// Returns `(width, height)`; `(0, 0)` for a fully transparent cursor.
#[must_use]
pub fn opaque_extent(pixels: &[u8], width: u32, src: Edges) -> (u32, u32) {
    let rows = if width == 0 {
        0
    } else {
        pixels.len() / (width as usize * 4)
    };
    let l = src.0.max(0) as usize;
    let t = src.1.max(0) as usize;
    let r = (src.2.max(0) as usize).min(width as usize);
    let b = (src.3.max(0) as usize).min(rows);
    let (mut ex, mut ey) = (0usize, 0usize);
    for y in t..b {
        for x in l..r {
            if pixels[(y * width as usize + x) * 4 + 3] != 0 {
                ex = ex.max(x - l + 1);
                ey = ey.max(y - t + 1);
            }
        }
    }
    (ex as u32, ey as u32)
}

/// What the presenter puts on the cursor layer for one cursor position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StagePlan {
    /// The part of the padded buffer copied, `(x, y, width, height)`.
    pub crop: (u32, u32, u32, u32),
    /// The crop's size once scaled to the size it is shown at, before rotation.
    pub out: (u32, u32),
    /// Clockwise pre-rotation in degrees.
    pub rot: u32,
    /// The staged buffer's size (`out`, swapped by a quarter turn).
    pub buffer: (u32, u32),
    /// The layer's destination in window pixels: the plane's top-left, `out`
    /// in size.
    pub dst: Edges,
    /// The layer's buffer transform (undoes `rot`).
    pub transform: i32,
}

/// Plan the staged cursor from the plane's visible source and destination
/// ([`crate::scanout::cursor_rects`]), the cursor's opaque extent in that source
/// ([`opaque_extent`]) and the display rotation. The crop is the extent rounded
/// up to 4 pixels and bounded by the source; its scale is the plane's
/// (`dst / src` per axis). `None` for an empty source or destination.
#[must_use]
pub fn stage_plan(src: Edges, dst: Edges, extent: (u32, u32), rot: u32) -> Option<StagePlan> {
    let (sw, sh) = (src.2 - src.0, src.3 - src.1);
    let (dw, dh) = (dst.2 - dst.0, dst.3 - dst.1);
    if sw <= 0 || sh <= 0 || dw <= 0 || dh <= 0 || src.0 < 0 || src.1 < 0 {
        return None;
    }
    let (sw, sh, dw, dh) = (sw as u64, sh as u64, dw as u64, dh as u64);
    let pad = |e: u32, limit: u64| u64::from(e.max(1)).div_ceil(4).saturating_mul(4).min(limit);
    let (cw, ch) = (pad(extent.0, sw), pad(extent.1, sh));
    // Round to the nearest pixel, never below one.
    let scale = |c: u64, d: u64, s: u64| ((c * d * 2 + s) / (2 * s)).max(1);
    let out = (scale(cw, dw, sw) as u32, scale(ch, dh, sh) as u32);
    let rot = normalize_rotation(rot);
    let buffer = if matches!(rot, 0 | 180) {
        out
    } else {
        (out.1, out.0)
    };
    Some(StagePlan {
        crop: (src.0 as u32, src.1 as u32, cw as u32, ch as u32),
        out,
        rot,
        buffer,
        dst: (dst.0, dst.1, dst.0 + out.0 as i32, dst.1 + out.1 as i32),
        transform: inverse_rotation_transform(rot),
    })
}

/// Crop `crop = (x, y, w, h)` out of the tightly packed `src` (`src_w` wide),
/// scale it to `out = (w, h)` (bilinear on premultiplied pixels; a plain copy
/// at 1:1) and rotate the result clockwise by `rot` degrees (0/90/180/270).
/// Returns `(pixels, width, height)`, tightly packed. The crop must lie inside
/// `src`.
#[must_use]
pub fn transform_cursor(
    src: &[u8],
    src_w: u32,
    crop: (u32, u32, u32, u32),
    out: (u32, u32),
    rot: u32,
) -> (Vec<u8>, u32, u32) {
    let (cx, cy, cw, ch) = (
        crop.0 as usize,
        crop.1 as usize,
        (crop.2 as usize).max(1),
        (crop.3 as usize).max(1),
    );
    let (ow, oh) = (out.0.max(1) as usize, out.1.max(1) as usize);
    let sw = src_w as usize;
    let px = |x: usize, y: usize| -> [u8; 4] {
        let o = ((cy + y) * sw + cx + x) * 4;
        [src[o], src[o + 1], src[o + 2], src[o + 3]]
    };
    let mut scaled = vec![0u8; ow * oh * 4];
    for y in 0..oh {
        for x in 0..ow {
            let p = if ow == cw && oh == ch {
                px(x, y)
            } else {
                let fx =
                    ((x as f32 + 0.5) * cw as f32 / ow as f32 - 0.5).clamp(0.0, (cw - 1) as f32);
                let fy =
                    ((y as f32 + 0.5) * ch as f32 / oh as f32 - 0.5).clamp(0.0, (ch - 1) as f32);
                let (x0, y0) = (fx as usize, fy as usize);
                let (x1, y1) = ((x0 + 1).min(cw - 1), (y0 + 1).min(ch - 1));
                let (ax, ay) = (fx - x0 as f32, fy - y0 as f32);
                let (a, b, c, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
                let mut o = [0u8; 4];
                for i in 0..4 {
                    let top = f32::from(a[i]) * (1.0 - ax) + f32::from(b[i]) * ax;
                    let bot = f32::from(c[i]) * (1.0 - ax) + f32::from(d[i]) * ax;
                    o[i] = (top * (1.0 - ay) + bot * ay + 0.5) as u8;
                }
                o
            };
            let o = (y * ow + x) * 4;
            scaled[o..o + 4].copy_from_slice(&p);
        }
    }
    rotate_cw(&scaled, ow, oh, normalize_rotation(rot))
}

/// Rotate tightly packed 4-byte pixels clockwise by 0/90/180/270 degrees.
fn rotate_cw(img: &[u8], w: usize, h: usize, rot: u32) -> (Vec<u8>, u32, u32) {
    if rot == 0 {
        return (img.to_vec(), w as u32, h as u32);
    }
    let (rw, rh) = if rot == 180 { (w, h) } else { (h, w) };
    let mut r = vec![0u8; rw * rh * 4];
    for y in 0..rh {
        for x in 0..rw {
            let (sx, sy) = match rot {
                90 => (y, h - 1 - x),
                180 => (w - 1 - x, h - 1 - y),
                _ => (w - 1 - y, x),
            };
            let s = (sy * w + sx) * 4;
            let d = (y * rw + x) * 4;
            r[d..d + 4].copy_from_slice(&img[s..s + 4]);
        }
    }
    (r, rw as u32, rh as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanout::cursor_rects;

    /// What a layer with buffer transform `t` shows in window space: the
    /// composer's rotation of the buffer (`NATIVE_WINDOW_TRANSFORM_ROT_*` are
    /// clockwise).
    fn displayed(img: &[u8], w: u32, h: u32, t: i32) -> (Vec<u8>, u32, u32) {
        let rot = match t {
            0 => 0,
            4 => 90,
            3 => 180,
            7 => 270,
            _ => panic!("transform {t}"),
        };
        rotate_cw(img, w as usize, h as usize, rot)
    }

    /// A `w` x `h` image, transparent except opaque pixels at `marks`, each
    /// carrying its index in the red channel.
    fn image(w: u32, h: u32, marks: &[(u32, u32)]) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for (i, &(x, y)) in marks.iter().enumerate() {
            let o = ((y * w + x) * 4) as usize;
            v[o..o + 4].copy_from_slice(&[i as u8 + 1, 0, 0, 255]);
        }
        v
    }

    fn opaque_at(img: &[u8], w: u32, x: u32, y: u32) -> bool {
        img[((y * w + x) * 4 + 3) as usize] != 0
    }

    #[test]
    fn rotation_follows_the_window_against_the_panel() {
        // Portrait panel (phone, handheld) run in landscape: the RP6 case.
        assert_eq!(derive_rotation((1920, 1080), false), 90);
        assert_eq!(derive_rotation((1080, 1920), false), 0);
        // Landscape panel (tablet).
        assert_eq!(derive_rotation((2560, 1600), true), 0);
        assert_eq!(derive_rotation((1600, 2560), true), 90);
        // Nothing to go on.
        assert_eq!(derive_rotation((1000, 1000), false), 0);
        assert_eq!(derive_rotation((0, 1080), false), 0);
    }

    #[test]
    fn overrides_snap_to_quarter_turns() {
        assert_eq!(normalize_rotation(0), 0);
        assert_eq!(normalize_rotation(90), 90);
        assert_eq!(normalize_rotation(200), 180);
        assert_eq!(normalize_rotation(270), 270);
        assert_eq!(normalize_rotation(450), 90);
        assert_eq!(inverse_rotation_transform(0), 0);
        assert_eq!(inverse_rotation_transform(90), 7);
        assert_eq!(inverse_rotation_transform(180), 3);
        assert_eq!(inverse_rotation_transform(270), 4);
    }

    #[test]
    fn extent_is_measured_from_the_source_origin() {
        let img = image(64, 64, &[(0, 0), (11, 3), (5, 17)]);
        assert_eq!(opaque_extent(&img, 64, (0, 0, 64, 64)), (12, 18));
        // A source cropped at the left edge (cursor hanging off the mode).
        assert_eq!(opaque_extent(&img, 64, (4, 0, 64, 64)), (8, 18));
        // Fully transparent, and a rect past the buffer is bounded.
        assert_eq!(opaque_extent(&image(8, 8, &[]), 8, (0, 0, 8, 8)), (0, 0));
        assert_eq!(opaque_extent(&img, 64, (0, 0, 300, 300)), (12, 18));
    }

    /// The RP6 case: 1280x720 mode in a 1920x1080 window (x1.5), a 12x16
    /// arrow in a 256x256 buffer, display at 90. The logged device geometry
    /// was buffer 24x18, dst 18x24, transform 7.
    #[test]
    fn plan_matches_the_measured_rp6_geometry() {
        let (src, dst) = cursor_rects(
            (683, 344, 256, 256),
            (0, 0, 256, 256),
            (1280, 720),
            (1920, 1080),
        )
        .unwrap();
        let p = stage_plan(src, dst, (12, 16), 90).unwrap();
        assert_eq!(p.crop, (0, 0, 12, 16));
        assert_eq!(p.out, (18, 24));
        assert_eq!(p.buffer, (24, 18));
        assert_eq!(p.transform, 7);
        assert_eq!((p.dst.0, p.dst.1), (dst.0, dst.1));
        assert_eq!((p.dst.2 - p.dst.0, p.dst.3 - p.dst.1), (18, 24));
    }

    #[test]
    fn plan_pads_the_extent_and_keeps_inside_the_source() {
        let p = stage_plan((0, 0, 64, 64), (0, 0, 64, 64), (13, 1), 0).unwrap();
        assert_eq!((p.crop.2, p.crop.3), (16, 4));
        // Clipped to a 6 px wide visible source: never past it.
        let p = stage_plan((58, 0, 64, 64), (0, 0, 6, 64), (6, 10), 0).unwrap();
        assert_eq!(p.crop, (58, 0, 6, 12));
        assert_eq!(p.out, (6, 12));
        // Transparent cursor: a minimal 4x4 layer, still placed.
        let p = stage_plan((0, 0, 64, 64), (10, 20, 74, 84), (0, 0), 0).unwrap();
        assert_eq!(p.dst, (10, 20, 14, 24));
        assert!(stage_plan((0, 0, 0, 64), (0, 0, 64, 64), (1, 1), 0).is_none());
        assert!(stage_plan((0, 0, 64, 64), (0, 0, 64, 0), (1, 1), 0).is_none());
    }

    /// Pre-rotation plus the inverse buffer transform is the identity in window
    /// space for every rotation, so a wrong rotation guess never turns the
    /// cursor: only the composer's view of the layer changes.
    #[test]
    fn staging_cancels_out_in_window_space_for_every_rotation() {
        let marks = [(0, 0), (7, 1), (2, 6), (5, 3)];
        let img = image(8, 8, &marks);
        for rot in [0, 90, 180, 270] {
            let p = stage_plan((0, 0, 8, 8), (0, 0, 8, 8), (8, 8), rot).unwrap();
            let (staged, w, h) = transform_cursor(&img, 8, p.crop, p.out, p.rot);
            assert_eq!((w, h), p.buffer, "rot {rot}");
            let (shown, sw, sh) = displayed(&staged, w, h, p.transform);
            assert_eq!((sw, sh), p.out, "rot {rot}");
            assert_eq!(shown, img, "rot {rot}");
        }
    }

    /// The hotspot sits at the image's top-left offset; after a x1.5 pre-scale
    /// and a 90 pre-rotation it lands at 1.5x that offset from the layer's
    /// top-left in window space, and the layer's top-left is the plane's.
    #[test]
    fn hotspot_scales_with_the_plane_and_stays_anchored() {
        // A 2x2 opaque block at (8, 4) in a 16x16 cursor.
        let block = [(8, 4), (9, 4), (8, 5), (9, 5), (15, 15)];
        let img = image(16, 16, &block);
        let (src, dst) =
            cursor_rects((100, 50, 16, 16), (0, 0, 16, 16), (1280, 720), (1920, 1080)).unwrap();
        assert_eq!((dst.0, dst.1), (150, 75));
        let extent = opaque_extent(&img, 16, src);
        let p = stage_plan(src, dst, extent, 90).unwrap();
        assert_eq!(p.out, (24, 24));
        assert_eq!((p.dst.0, p.dst.1), (150, 75));
        let (staged, w, h) = transform_cursor(&img, 16, p.crop, p.out, p.rot);
        let (shown, sw, _) = displayed(&staged, w, h, p.transform);
        // The block's centre (9, 5) maps to (13.5, 7.5) in window pixels.
        assert!(opaque_at(&shown, sw, 13, 7));
        assert!(!opaque_at(&shown, sw, 13, 2));
        assert!(!opaque_at(&shown, sw, 6, 7));
        // Nothing left of the block's left edge (x 8 -> 12) is opaque on its row.
        assert!((0..11).all(|x| !opaque_at(&shown, sw, x, 7)));
    }

    /// A cursor hanging off the mode's left edge: the visible source starts
    /// inside the buffer, the layer starts at the mode edge, and what is
    /// shown is the buffer's columns from that point on, not the whole cursor
    /// squashed.
    #[test]
    fn a_cursor_clipped_at_the_edge_shows_only_its_visible_part() {
        let marks = [(1, 0), (6, 0), (7, 3)];
        let img = image(8, 8, &marks);
        let (src, dst) =
            cursor_rects((-4, 10, 8, 8), (0, 0, 8, 8), (1920, 1080), (1920, 1080)).unwrap();
        assert_eq!(src, (4, 0, 8, 8));
        assert_eq!(dst, (0, 10, 4, 18));
        let p = stage_plan(src, dst, opaque_extent(&img, 8, src), 90).unwrap();
        assert_eq!(p.crop, (4, 0, 4, 4));
        assert_eq!(p.dst, (0, 10, 4, 14));
        let (staged, w, h) = transform_cursor(&img, 8, p.crop, p.out, p.rot);
        let (shown, sw, _) = displayed(&staged, w, h, p.transform);
        // (1, 0) is off the mode; (6, 0) -> (2, 0); (7, 3) -> (3, 3).
        assert!(!opaque_at(&shown, sw, 0, 0) && !opaque_at(&shown, sw, 1, 0));
        assert!(opaque_at(&shown, sw, 2, 0));
        assert!(opaque_at(&shown, sw, 3, 3));
    }
}
