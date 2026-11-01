//! EDID generation for the emulated connector.
//!
//! gamescope's `CDRMConnector::ParseEDID()` early-returns when the connector
//! has no `EDID` property, and only sets `HDR.bExposeHDRSupport` with
//! `EOTF_PQ` — which is what makes `IsHDR10()` true and the session enter
//! HDR10 — when the EDID's CTA-861 extension carries **both** a Colorimetry
//! Data Block with the `bt2020_rgb` bit and an HDR Static Metadata Data Block
//! with the ST2084 (PQ) EOTF bit (`DRMBackend.cpp:2818-2855`). So the block
//! exists only when the display really is HDR; an SDR session gets a base
//! block alone and gamescope stays in its SDR path.
//!
//! The block bytes are the CTA-861 encoding libdisplay-info decodes (the same
//! library gamescope links):
//!
//! * desired content max / max-frame-average luminance: `50 * 2^(v/32)` cd/m²
//!   (0 = unset), `v` = `round(32 * log2(nits / 50))`.
//! * desired content min luminance: `max * (v/255)^2 / 100` cd/m² where `max`
//!   is the *decoded* max above, `v` = `round(255 * sqrt(min * 100 / max))`.

use gndrm_uapi::mode::*;

use crate::objects::PanelDesc;
use crate::HdrCapability;

/// EDID header magic.
const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// BT.709 / sRGB primaries, the set gamescope assumes when the EDID carries
/// none. Encoded 10-bit as `round(coord * 1024)`.
const PRIMARIES_709: [(f32, f32); 3] = [(0.640, 0.330), (0.300, 0.600), (0.150, 0.060)];
const WHITE_D65: (f32, f32) = (0.3127, 0.3290);

/// CTA-861 extended data-block tags.
const CTA_EXT_COLORIMETRY: u8 = 0x05;
const CTA_EXT_HDR_STATIC_METADATA: u8 = 0x06;
/// Standard tag that introduces an extended-tagged block.
const CTA_TAG_EXTENDED: u8 = 0x07;

/// Build the connector EDID. `cap` is the session's HDR capability: `Some`
/// appends the CTA extension with BT2020 colorimetry and an ST2084 HDR static
/// metadata block; `None` returns a base block with no HDR information.
#[must_use]
pub fn build(cap: Option<HdrCapability>, mode: &DrmModeModeinfo, panel: &PanelDesc) -> Vec<u8> {
    let mut base = [0u8; 128];
    base[0..8].copy_from_slice(&HEADER);
    // Manufacturer "GNX" (a made-up PNP id; nothing keys off it) + product 1.
    // PNP id: five bits per letter, A=1.
    let mfg = ((u16::from(b'G' - 64)) << 10) | ((u16::from(b'N' - 64)) << 5) | u16::from(b'X' - 64);
    base[8] = (mfg >> 8) as u8;
    base[9] = (mfg & 0xFF) as u8;
    base[10] = 0x01;
    base[16] = 0;
    base[17] = 36; // manufactured 2026
    base[18] = 0x01; // EDID 1.3
    base[19] = 0x03;
    base[20] = 0x80; // digital input, interface unspecified
    base[23] = 0x78; // gamma 2.20
    base[24] = 0x02; // first detailed timing is the preferred timing
    write_chromaticity(&mut base, PRIMARIES_709, WHITE_D65);
    // Standard timings: the "unused" code (0x01 0x01), not all-zero.
    for b in &mut base[38..54] {
        *b = 0x01;
    }
    base[54..72].copy_from_slice(&detailed_timing(mode, panel));
    base[72..90].copy_from_slice(&display_descriptor_product_name(b"GameNative"));
    // Two empty descriptors.
    base[90..108].copy_from_slice(&[0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    base[108..126].copy_from_slice(&[0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    base[126] = u8::from(cap.is_some());
    base[127] = checksum(&base[..127]);
    let mut edid = base.to_vec();
    if let Some(cap) = cap {
        edid.extend_from_slice(&cta_extension(&cap));
    }
    edid
}

/// The CTA-861 extension: colorimetry (BT2020 RGB) + HDR static metadata
/// (ST2084, type 1, the panel's real luminances). No DTDs — the connector
/// mode list is authoritative, and the base block already carries the
/// preferred timing.
fn cta_extension(cap: &HdrCapability) -> [u8; 128] {
    let mut ext = [0u8; 128];
    ext[0] = 0x02; // CTA extension tag
    ext[1] = 0x03; // revision 3
    let mut off = 4usize;
    // Colorimetry Data Block (extended tag 5): byte 0 bit 7 = BT2020 RGB.
    ext[off] = (CTA_TAG_EXTENDED << 5) | 3;
    off += 1;
    ext[off] = CTA_EXT_COLORIMETRY;
    off += 1;
    ext[off] = 0x80; // bt2020_rgb
    off += 1;
    ext[off] = 0x00; // reserved
    off += 1;
    // HDR Static Metadata Data Block (extended tag 6).
    let (max, fall, min) = encode_luminances(cap);
    ext[off] = (CTA_TAG_EXTENDED << 5) | 6;
    off += 1;
    ext[off] = CTA_EXT_HDR_STATIC_METADATA;
    off += 1;
    ext[off] = 0x04; // EOTF: PQ (ST2084), bit 2
    off += 1;
    ext[off] = 0x01; // descriptor: static metadata type 1
    off += 1;
    ext[off] = max;
    off += 1;
    ext[off] = fall;
    off += 1;
    ext[off] = min;
    off += 1;
    ext[2] = off as u8; // DTD offset = end of the block collection
    ext[3] = 0x00; // no native DTDs
    ext[127] = checksum(&ext[..127]);
    ext
}

/// Encode the panel's luminances into the three CTA static-metadata bytes.
///
/// `max`/`fall` use `50 * 2^(v/32)` cd/m²; the min byte is relative to the
/// *decoded* max, because libdisplay-info's `parse_min_luminance` scales by the
/// max it just decoded.
#[must_use]
fn encode_luminances(cap: &HdrCapability) -> (u8, u8, u8) {
    let max = encode_max_luminance(cap.max_nits);
    let fall = encode_max_luminance(cap.fall_nits);
    let decoded = decode_max_luminance(max);
    let min = encode_min_luminance(cap.min_nits, decoded);
    (max, fall, min)
}

/// `2^(1/32)`: the CTA static-metadata luminance step.
const LUM_STEP: f32 = 1.021_897_1;

/// Decode a CTA max-luminance byte to cd/m² (`50 * 2^(v/32)`) by stepping
/// [`LUM_STEP`] — no libm, so the shipped library carries no `exp2f`/`log2f`
/// import (the import audit rejects those).
fn decode_max_luminance(v: u8) -> f32 {
    let mut lum = 50.0f32;
    for _ in 0..v {
        lum *= LUM_STEP;
    }
    lum
}

/// The byte whose decoded luminance is nearest `nits` (0 = unset). A linear
/// search over the 256 codes, minimising the decode error directly.
fn encode_max_luminance(nits: f32) -> u8 {
    if !nits.is_finite() || nits <= 0.0 {
        return 0;
    }
    let mut lum = 50.0f32;
    let mut best = 1u8;
    let mut best_err = f32::MAX;
    for v in 1u8..=255 {
        lum *= LUM_STEP;
        let err = (lum - nits).abs();
        if err < best_err {
            best_err = err;
            best = v;
        }
    }
    best
}

/// The byte whose decoded min luminance (`max * (v/255)^2 / 100`) is nearest
/// `min_nits` (0 = unset). Same linear-search approach as
/// [`encode_max_luminance`].
fn encode_min_luminance(min_nits: f32, decoded_max: f32) -> u8 {
    if !min_nits.is_finite() || min_nits <= 0.0 || !decoded_max.is_finite() || decoded_max <= 0.0 {
        return 0;
    }
    let mut best = 1u8;
    let mut best_err = f32::MAX;
    for v in 1u8..=255 {
        let f = f32::from(v) / 255.0;
        let decoded = decoded_max * f * f / 100.0;
        let err = (decoded - min_nits).abs();
        if err < best_err {
            best_err = err;
            best = v;
        }
    }
    best
}

/// Chromaticity block bytes 25-34, 10-bit coords packed per EDID 1.3.
fn write_chromaticity(edid: &mut [u8; 128], primaries: [(f32, f32); 3], white: (f32, f32)) {
    let enc = |c: f32| -> u16 { ((c * 1024.0).round() as i32).clamp(0, 1023) as u16 };
    let rx = enc(primaries[0].0);
    let ry = enc(primaries[0].1);
    let gx = enc(primaries[1].0);
    let gy = enc(primaries[1].1);
    let bx = enc(primaries[2].0);
    let by = enc(primaries[2].1);
    let wx = enc(white.0);
    let wy = enc(white.1);
    edid[25] = (((rx & 3) << 6) | ((ry & 3) << 4) | ((gx & 3) << 2) | (gy & 3)) as u8;
    edid[26] = (((bx & 3) << 6) | ((by & 3) << 4) | ((wx & 3) << 2) | (wy & 3)) as u8;
    edid[27] = (rx >> 2) as u8;
    edid[28] = (ry >> 2) as u8;
    edid[29] = (gx >> 2) as u8;
    edid[30] = (gy >> 2) as u8;
    edid[31] = (bx >> 2) as u8;
    edid[32] = (by >> 2) as u8;
    edid[33] = (wx >> 2) as u8;
    edid[34] = (wy >> 2) as u8;
}

/// One 18-byte detailed timing descriptor matching the connector's mode.
///
/// The pixel clock is the mode's own (`PanelDesc::modeinfo` builds it from the
/// mode's rate), so the descriptor and the connector describe one timing. The
/// 16-bit 10 kHz field tops out at 655.35 MHz, which only a frame-generated rate
/// above 4K reaches; such a mode falls back to a nominal 60 Hz. gamescope reads
/// the mode from the connector and the EDID only for colorimetry and HDR
/// metadata, so these timings are for consistency, not for mode selection.
fn detailed_timing(mode: &DrmModeModeinfo, panel: &PanelDesc) -> [u8; 18] {
    let hact = u32::from(mode.hdisplay);
    let hblank = u32::from(mode.htotal).saturating_sub(hact);
    let hfront = u32::from(mode.hsync_start).saturating_sub(hact);
    let hsync = u32::from(mode.hsync_end).saturating_sub(u32::from(mode.hsync_start));
    let vact = u32::from(mode.vdisplay);
    let vblank = u32::from(mode.vtotal).saturating_sub(vact);
    let vfront = u32::from(mode.vsync_start).saturating_sub(vact);
    let vsync = u32::from(mode.vsync_end).saturating_sub(u32::from(mode.vsync_start));
    let mut d = [0u8; 18];
    let clock_khz = if (10..=655_350).contains(&mode.clock) {
        mode.clock
    } else {
        u32::from(mode.htotal) * u32::from(mode.vtotal) * 60 / 1000
    };
    let pc = (clock_khz / 10) & 0xFFFF; // pixel clock in 10 kHz units
    d[0] = (pc & 0xFF) as u8;
    d[1] = (pc >> 8) as u8;
    d[2] = (hact & 0xFF) as u8;
    d[3] = (hblank & 0xFF) as u8;
    d[4] = (((hact >> 8) << 4) | ((hblank >> 8) & 0x0F)) as u8;
    d[5] = (vact & 0xFF) as u8;
    d[6] = (vblank & 0xFF) as u8;
    d[7] = (((vact >> 8) << 4) | ((vblank >> 8) & 0x0F)) as u8;
    d[8] = (hfront & 0xFF) as u8;
    d[9] = (hsync & 0xFF) as u8;
    d[10] = (((vfront & 0x0F) << 4) | (vsync & 0x0F)) as u8;
    d[11] = (((hfront >> 8) << 6)
        | (((hsync >> 8) & 3) << 4)
        | (((vfront >> 4) & 3) << 2)
        | ((vsync >> 4) & 3)) as u8;
    d[12] = (panel.mm_width & 0xFF) as u8;
    d[13] = (panel.mm_height & 0xFF) as u8;
    d[14] = ((((panel.mm_width >> 8) & 0x0F) << 4) | ((panel.mm_height >> 8) & 0x0F)) as u8;
    d[17] = 0x1E; // digital separate sync, +h +v
    d
}

/// An `0xFC` display descriptor carrying the product name.
fn display_descriptor_product_name(name: &[u8]) -> [u8; 18] {
    let mut d = [0u8; 18];
    d[3] = 0xFC;
    let n = name.len().min(13);
    d[5..5 + n].copy_from_slice(&name[..n]);
    if n < 13 {
        d[5 + n] = b'\n';
    }
    d
}

/// EDID block checksum: two's complement of the sum of the first 127 bytes.
fn checksum(block: &[u8]) -> u8 {
    block
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b))
        .wrapping_neg()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode() -> DrmModeModeinfo {
        PanelDesc::default().modeinfo(crate::UNKNOWN_PANEL_HZ)
    }

    #[test]
    fn the_detailed_timing_carries_the_modes_own_rate() {
        let panel = PanelDesc {
            width: 1280,
            height: 960,
            mm_width: 0,
            mm_height: 0,
        };
        let mode = panel.modeinfo(120_000);
        let edid = build(None, &mode, &panel);
        let pc = u32::from(edid[54]) | (u32::from(edid[55]) << 8);
        // The descriptor's timings imply the connector's rate, not a nominal one.
        let hz = u64::from(pc) * 10_000 / (u64::from(mode.htotal) * u64::from(mode.vtotal));
        assert!((119..=120).contains(&hz), "descriptor implies {hz} Hz");
    }

    #[test]
    fn a_clock_the_field_cannot_hold_falls_back_to_60_hz() {
        let panel = PanelDesc {
            width: 1920,
            height: 1080,
            mm_width: 0,
            mm_height: 0,
        };
        let mode = panel.modeinfo(500_000);
        assert!(
            mode.clock > 655_350,
            "the mode must exceed the 10 kHz field"
        );
        let edid = build(None, &mode, &panel);
        let pc = u32::from(edid[54]) | (u32::from(edid[55]) << 8);
        assert_eq!(pc * 10, 2000 * 1100 * 60 / 1000);
    }

    #[test]
    fn sdr_edid_has_no_cta_extension() {
        let edid = build(None, &mode(), &PanelDesc::default());
        assert_eq!(edid.len(), 128);
        assert_eq!(edid[126], 0);
        assert_eq!(checksum(&edid[..127]), edid[127]);
    }

    #[test]
    fn hdr_edid_carries_the_hdr_static_metadata_block() {
        let cap = HdrCapability {
            max_nits: 420.0,
            fall_nits: 420.0,
            min_nits: 0.323,
        };
        let edid = build(Some(cap), &mode(), &PanelDesc::default());
        assert_eq!(edid.len(), 256);
        assert_eq!(edid[126], 1);
        assert_eq!(&edid[128..130], &[0x02, 0x03]);
        assert_eq!(checksum(&edid[..127]), edid[127]);
        assert_eq!(checksum(&edid[128..255]), edid[255]);
        // Colorimetry Data Block: extended tag 5, bt2020_rgb.
        let ext = &edid[128..];
        assert_eq!(ext[4], (CTA_TAG_EXTENDED << 5) | 3);
        assert_eq!(ext[5], CTA_EXT_COLORIMETRY);
        assert_eq!(ext[6], 0x80);
        // HDR Static Metadata Data Block: extended tag 6, PQ, type 1.
        assert_eq!(ext[8], (CTA_TAG_EXTENDED << 5) | 6);
        assert_eq!(ext[9], CTA_EXT_HDR_STATIC_METADATA);
        assert_eq!(ext[10], 0x04);
        assert_eq!(ext[11], 0x01);
        // 420 nits -> round(32*log2(8.4)) = 98 -> 417.7 cd/m^2 decoded.
        assert_eq!(ext[12], 98);
        assert_eq!(ext[13], 98);
        assert!((decode_max_luminance(98) - 417.71).abs() < 0.1);
        // 0.323 nits against 417.7 -> round(255*sqrt(0.07735)) = 71.
        assert_eq!(ext[14], 71);
    }

    #[test]
    fn luminance_round_trip_is_within_a_few_percent() {
        for nits in [100.0f32, 420.0, 1000.0, 1499.0] {
            let v = encode_max_luminance(nits);
            let back = decode_max_luminance(v);
            assert!((back - nits).abs() / nits < 0.05, "{nits} -> {v} -> {back}");
        }
    }
}
