//! Forward color transform for the encoder's **color** mode (`ColorMode::Color`).
//!
//! JPEG-XR's color path is `RGB → internal YUV (YCoCg-like) → per-plane PCT`.
//! [`rgb_to_yuv444`] is the **exact integer inverse** of the decoder's
//! [`crate::image::jxr_decode::decoder::yuv444_to_rgb`] lifting (which is the
//! spec / libjxr `strInvTransform`). Because every step is integer lifting, the
//! pair is a perfect bijection — lossless 4:4:4 color round-trips bit-exactly.
//!
//! Bias: the decoder applies the YUV→RGB lifting on the reconstructed
//! coefficients and *then* adds `1<<(bd-1)` (128 for BD8) to land in `[0,255]`.
//! So the forward direction subtracts the bias from the input RGB **first**, runs
//! [`rgb_to_yuv444`] on the centered values, and feeds the resulting Y/U/V planes
//! into the same per-plane forward transform grayscale uses (no further −128).
//! See `.claude/plans/jxr-encoder.md` Track 6.1.
//!
//! 4:4:4 only — boko's decoder rejects subsampled chroma (`decoder.rs:474`), so
//! there is no down-sampling here; each chroma plane is full resolution.

use super::bitstream::BitWriter;
use super::entropy::write_huff;
use super::quant::{quantize, scaling_factor, QpSet};
use super::{codestream, coeff, container, transform};
use crate::image::jxr_decode::consts::*;
use crate::image::jxr_decode::decoder::{ceil_div2, floor_div2};
use crate::image::jxr_decode::math::chroma_component;
use crate::image::jxr_decode::state::{AdaptiveScan, AdaptiveVLC};
use crate::image::jxr_decode::tables;

const ABS_DELTA: [i32; 7] = [1, 0, -1, -1, -1, -1, -1]; // ABS_LEVEL_INDEX_DELTA[0]

/// Forward color transform: centered `RGB → (Y, U, V)`, the exact inverse of the
/// decoder's [`yuv444_to_rgb`]. Inputs are **pre-bias** (input pixel − 128 for
/// BD8); outputs are the internal coefficients the per-plane PCT consumes.
///
/// [`yuv444_to_rgb`]: crate::image::jxr_decode::decoder::yuv444_to_rgb
#[inline]
pub fn rgb_to_yuv444(r: i32, g: i32, b: i32) -> (i32, i32, i32) {
    // Invert, in reverse order, the decoder's lifting
    //   t = -U;  G = Y − ⌊t/2⌋;  R = t + G − ⌈V/2⌉;  B = V + R
    let v = b - r; // from B = V + R
    let temp_t = r - g + ceil_div2(v); // from R = t + G − ⌈V/2⌉
    let u = -temp_t; // t = −U
    let y = g + floor_div2(temp_t); // from G = Y − ⌊t/2⌋
    (y, u, v)
}

/// Pad one u8 plane to a 16-aligned grid with edge replication (as `gray.rs`).
fn pad_plane(src: &[u8], wu: usize, hu: usize, pw: usize, ph: usize) -> Vec<u8> {
    let mut p = vec![0u8; pw * ph];
    for y in 0..ph {
        let sy = y.min(hu - 1);
        for x in 0..pw {
            p[y * pw + x] = src[sy * wu + x.min(wu - 1)];
        }
    }
    p
}

/// Encode an RGB image (3 planes, each `w*h` row-major) as a **color** JPEG-XR
/// (`24bppRGB` / `INT_YUV444 → OUT_RGB`). **NOHIGHPASS stage** (Track 6.3): DC +
/// LP — exact for content with zero HP (per-MB-affine); HP band still TODO.
///
/// Mirrors the decoder's `mb_dc` + `mb_lp` YUV paths: a `val_dc_yuv` symbol for
/// the DC abs-flags, per-component DC residuals with chroma models/tables and
/// 3-component-weighted prediction; then `cbplp_yuv1_444` (with the
/// count-escape state) + per-component LP run-level via `encode_block` over a
/// **shared** adaptive scan with luma/chroma index tables, + LP refinement.
pub fn encode_color(r: &[u8], g: &[u8], b: &[u8], w: u32, h: u32, qp: QpSet) -> Vec<u8> {
    let (dc_sf, lp_sf) = (scaling_factor(qp.dc), scaling_factor(qp.lp));
    let (wu, hu) = (w as usize, h as usize);
    let (pw, ph) = (wu.next_multiple_of(16), hu.next_multiple_of(16));
    let (mbw, mbh) = (pw / 16, ph / 16);

    // Pad RGB, then forward color transform per pixel → 3 centered YUV planes.
    let (rp, gp, bp) = (
        pad_plane(r, wu, hu, pw, ph),
        pad_plane(g, wu, hu, pw, ph),
        pad_plane(b, wu, hu, pw, ph),
    );
    let mut yuv = [vec![0i32; pw * ph], vec![0i32; pw * ph], vec![0i32; pw * ph]];
    for i in 0..pw * ph {
        let (y, u, v) = rgb_to_yuv444(rp[i] as i32 - 128, gp[i] as i32 - 128, bp[i] as i32 - 128);
        yuv[0][i] = y;
        yuv[1][i] = u;
        yuv[2][i] = v;
    }

    // Per-MB, per-component quantized DC+LP levels: dclp[mbx][mby][comp][0..16].
    let mut dclp = vec![vec![[[0i32; 16]; 3]; mbh]; mbw];
    for mbx in 0..mbw {
        for mby in 0..mbh {
            for (comp, plane) in yuv.iter().enumerate() {
                let mut samples = [0i32; 256];
                for py in 0..16 {
                    for px in 0..16 {
                        samples[py * 16 + px] = plane[(mby * 16 + py) * pw + (mbx * 16 + px)];
                    }
                }
                let buf = transform::forward_transform_mb(&samples);
                let c = &mut dclp[mbx][mby][comp];
                for (j, slot) in c.iter_mut().enumerate() {
                    *slot = buf[16 * ICT4X4_INV_PERM[j]];
                }
                c[0] = quantize(c[0], dc_sf);
                for s in c.iter_mut().skip(1) {
                    *s = quantize(*s, lp_sf);
                }
            }
        }
    }

    let mut bw = BitWriter::new();
    codestream::write_image_header(&mut bw, w, h, OUT_RGB);
    codestream::write_image_plane_header_color_nohighpass(&mut bw, qp.dc, qp.lp);
    codestream::write_vlw_esc(&mut bw, 0);
    codestream::write_common_tile_header(&mut bw);

    // DC band state.
    let mut model_dc = coeff::ColorModel::init(0);
    let mut abs_dc_lum = coeff::AdaptiveVlc1::default();
    let mut abs_dc_chr = coeff::AdaptiveVlc1::default();
    // LP band state: luma + chroma index tables, shared abs tables + scan, and
    // the cbplp count-escape counters.
    let mut model_lp = coeff::ColorModel::init(1);
    let mut lp_first = [AdaptiveVLC::default(), AdaptiveVLC::default()]; // [lum, chr]
    let mut lp_ind0 = [AdaptiveVLC::default(), AdaptiveVLC::default()];
    let mut lp_ind1 = [AdaptiveVLC::default(), AdaptiveVLC::default()];
    let mut lp_abs0 = AdaptiveVLC::default();
    let mut lp_abs1 = AdaptiveVLC::default();
    for t in lp_first.iter_mut().chain(lp_ind0.iter_mut()).chain(lp_ind1.iter_mut()) {
        t.init_table2();
    }
    lp_abs0.init_table1();
    lp_abs1.init_table1();
    let mut scan = AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H);
    let (mut count_zero_cbplp, mut count_max_cbplp) = (1i32, 1i32);

    for mby in 0..mbh {
        for mbx in 0..mbw {
            let (is_left, is_top) = (mbx == 0, mby == 0);
            let dc_of = |mx: usize, my: usize| {
                [dclp[mx][my][0][0], dclp[mx][my][1][0], dclp[mx][my][2][0]]
            };

            // ---------- DC ----------
            let (dmode, dc_preds): (u8, [i32; 3]) = if is_left && is_top {
                (NO_PREDICTION, [0; 3])
            } else if is_left {
                (PREDICT_FROM_TOP, dc_of(mbx, mby - 1))
            } else if is_top {
                (PREDICT_FROM_LEFT, dc_of(mbx - 1, mby))
            } else {
                let (l, t, tl) = (dc_of(mbx - 1, mby), dc_of(mbx, mby - 1), dc_of(mbx - 1, mby - 1));
                let sh = (tl[0] - l[0]).abs() * 2 + (tl[1] - l[1]).abs() + (tl[2] - l[2]).abs();
                let sv = (tl[0] - t[0]).abs() * 2 + (tl[1] - t[1]).abs() + (tl[2] - t[2]).abs();
                if sh * 4 < sv {
                    (PREDICT_FROM_TOP, t)
                } else if sv * 4 < sh {
                    (PREDICT_FROM_LEFT, l)
                } else {
                    (PREDICT_FROM_TOP_LEFT, [(t[0] + l[0]) >> 1, (t[1] + l[1]) >> 1, (t[2] + l[2]) >> 1])
                }
            };
            let mut dc_res = [0i32; 3];
            let mut babs = [false; 3];
            for comp in 0..3 {
                let mb = model_dc.m_bits[chroma_component(comp)];
                dc_res[comp] = dclp[mbx][mby][comp][0] - dc_preds[comp];
                babs[comp] = (dc_res[comp].unsigned_abs() >> mb as u32) > 0;
            }
            let val = ((babs[0] as i32) << 2) | ((babs[1] as i32) << 1) | (babs[2] as i32);
            write_huff(&mut bw, tables::val_dc_yuv(), val);
            let mut lap_dc = [0i32; 2];
            for comp in 0..3 {
                let chroma = chroma_component(comp);
                let mb = model_dc.m_bits[chroma];
                let abs_vlc = if chroma == 0 { &mut abs_dc_lum } else { &mut abs_dc_chr };
                let abs_table = tables::abs_level_index(abs_vlc.table_index as usize);
                let idx = coeff::encode_dc_residual(&mut bw, dc_res[comp], mb, babs[comp], abs_table);
                if babs[comp] {
                    abs_vlc.discrim += ABS_DELTA[idx as usize];
                    lap_dc[chroma] += 1;
                }
            }
            model_dc.update(lap_dc, 0, 3);
            if mbx % 16 == 0 || mbx == mbw - 1 {
                abs_dc_lum.adapt();
                abs_dc_chr.adapt();
            }

            // ---------- LP ----------
            if mbx % 16 == 0 {
                scan.reset_totals();
            }
            let lp_mode = match dmode {
                PREDICT_FROM_LEFT => PREDICT_FROM_LEFT,
                PREDICT_FROM_TOP => PREDICT_FROM_TOP,
                _ => NO_PREDICTION,
            };
            let mut lp_res = [[0i32; 16]; 3];
            let mut coarse = [[0i32; 16]; 3];
            for comp in 0..3 {
                let mb = model_lp.m_bits[chroma_component(comp)] as u32;
                for j in 1..16 {
                    let pred = if lp_mode == PREDICT_FROM_LEFT && matches!(j, 1 | 2 | 3) {
                        dclp[mbx - 1][mby][comp][j]
                    } else if lp_mode == PREDICT_FROM_TOP && matches!(j, 4 | 8 | 12) {
                        dclp[mbx][mby - 1][comp][j]
                    } else {
                        0
                    };
                    lp_res[comp][j] = dclp[mbx][mby][comp][j] - pred;
                    let m = (lp_res[comp][j].unsigned_abs() >> mb) as i32;
                    coarse[comp][j] = if lp_res[comp][j] < 0 { -m } else { m };
                }
            }
            let cbp = [
                (1..16).any(|j| coarse[0][j] != 0),
                (1..16).any(|j| coarse[1][j] != 0),
                (1..16).any(|j| coarse[2][j] != 0),
            ];
            let i_cbplp = (cbp[0] as i32) | ((cbp[1] as i32) << 1) | ((cbp[2] as i32) << 2);
            // cbplp coding: Huffman (with optional inversion) when the count
            // state says so, else 3 raw bits — mirrors `mb_lp` (decoder.rs:940).
            let i_max = 3 * 4 - 5; // = 7 (all bits set)
            if count_zero_cbplp <= 0 || count_max_cbplp < 0 {
                let cbplp_yuv1 = if count_max_cbplp < count_zero_cbplp {
                    i_max - i_cbplp
                } else {
                    i_cbplp
                };
                write_huff(&mut bw, tables::cbplp_yuv1_444(), cbplp_yuv1);
            } else {
                bw.write_bits(i_cbplp as u64, 3);
            }
            count_zero_cbplp = (count_zero_cbplp + 1 - if i_cbplp == 0 { 4 } else { 0 }).clamp(-8, 7);
            count_max_cbplp = (count_max_cbplp + 1 - if i_cbplp == i_max { 4 } else { 0 }).clamp(-8, 7);

            let mut lap_lp = [0i32; 2];
            for comp in 0..3 {
                let chroma = chroma_component(comp);
                if cbp[comp] {
                    // Build (run, level) pairs in shared-scan order, adapting the
                    // one scan across all components exactly as the decoder does.
                    let mut pairs: Vec<(u32, i32)> = Vec::new();
                    let mut run = 0u32;
                    for i in 1..16usize {
                        let pos = scan.translate(i);
                        if coarse[comp][pos] != 0 {
                            pairs.push((run, coarse[comp][pos]));
                            run = 0;
                            scan.adapt(i);
                        } else {
                            run += 1;
                        }
                    }
                    lap_lp[chroma] += pairs.len() as i32;
                    coeff::encode_block(
                        &mut bw,
                        &pairs,
                        1,
                        &mut lp_first[chroma],
                        &mut lp_ind0[chroma],
                        &mut lp_ind1[chroma],
                        &mut lp_abs0,
                        &mut lp_abs1,
                    );
                }
                let mb = model_lp.m_bits[chroma];
                if mb > 0 {
                    for j in 1..16 {
                        coeff::encode_refine_lp(&mut bw, coarse[comp][j], lp_res[comp][j], mb);
                    }
                }
            }
            model_lp.update(lap_lp, 1, 3);
            if mbx % 16 == 0 || mbx == mbw - 1 {
                for t in lp_first.iter_mut() {
                    t.adapt_table2(4);
                }
                for t in lp_ind0.iter_mut().chain(lp_ind1.iter_mut()) {
                    t.adapt_table2(3);
                }
                lp_abs0.adapt_table1();
                lp_abs1.adapt_table1();
            }
        }
    }
    bw.align_to_byte();
    container::write_container(&bw.finish(), w, h, &container::pixel_format::RGB24)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::jxr_decode::decoder::yuv444_to_rgb;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
    }

    /// The gate: the decoder's real `yuv444_to_rgb` must invert our forward
    /// exactly, for every RGB triple. Pure integer lifting ⇒ bit-exact, no
    /// tolerance. Tests against the *actual decode function* (shared source), so
    /// this can't pass by both sides sharing the same bug.
    #[test]
    fn forward_inverts_decoder_exactly() {
        // Exhaustive over a centered cube that covers BD8's pre-bias range
        // (−128..=127) with margin for out-of-gamut coefficients.
        for r in (-160..=160).step_by(5) {
            for g in (-160..=160).step_by(5) {
                for b in (-160..=160).step_by(5) {
                    let (y, u, v) = rgb_to_yuv444(r, g, b);
                    assert_eq!(yuv444_to_rgb(y, u, v), (r, g, b), "rgb=({r},{g},{b})");
                }
            }
        }
        // Random wide range to catch anything the grid steps over.
        let mut rng = Lcg(0xC0FFEE_1234_5678);
        for _ in 0..200_000 {
            let r = (rng.next() % 1024) as i32 - 512;
            let g = (rng.next() % 1024) as i32 - 512;
            let b = (rng.next() % 1024) as i32 - 512;
            let (y, u, v) = rgb_to_yuv444(r, g, b);
            assert_eq!(yuv444_to_rgb(y, u, v), (r, g, b), "rgb=({r},{g},{b})");
        }
    }

    /// Saturated primaries/secondaries (centered for BD8) — the gamut corners
    /// the color transform must handle without drift.
    #[test]
    fn forward_inverts_saturated_corners() {
        let corners = [
            (127, -128, -128), // red
            (-128, 127, -128), // green
            (-128, -128, 127), // blue
            (127, 127, -128),  // yellow
            (127, -128, 127),  // magenta
            (-128, 127, 127),  // cyan
            (127, 127, 127),   // white
            (-128, -128, -128), // black
            (0, 0, 0),         // mid-gray
        ];
        for &(r, g, b) in &corners {
            let (y, u, v) = rgb_to_yuv444(r, g, b);
            assert_eq!(yuv444_to_rgb(y, u, v), (r, g, b), "corner=({r},{g},{b})");
        }
    }

    fn decode(jxr: &[u8]) -> crate::image::jxr_decode::decoder::DecodedImage {
        let c = crate::image::jxr_decode::container::parse(jxr).expect("container parse");
        crate::image::jxr_decode::decoder::Decoder::new(c.image_data).decode().expect("decode")
    }

    fn assert_rgb_exact(jxr: &[u8], w: usize, h: usize, expected: &[(u8, u8, u8)]) {
        let d = decode(jxr);
        assert_eq!((d.width as usize, d.height as usize), (w, h));
        assert_eq!(d.num_components, 3, "expected RGB");
        assert_eq!(d.output_clr_fmt, OUT_RGB);
        for i in 0..w * h {
            let got = (d.image_plane[0][i], d.image_plane[1][i], d.image_plane[2][i]);
            let (r, g, b) = expected[i];
            assert_eq!(got, (r as i32, g as i32, b as i32), "pixel {i}");
        }
    }

    /// DCONLY is lossless for **flat** content: a whole-image constant color
    /// round-trips exactly, exercising the val_dc_yuv path + chroma DC + model
    /// adaptation across multiple MBs (every non-(0,0) MB predicts to residual 0).
    #[test]
    fn roundtrip_constant_color() {
        for &(w, h) in &[(16usize, 16usize), (32, 16), (48, 32)] {
            for &(r, g, b) in &[(200u8, 50u8, 100u8), (0, 0, 0), (255, 255, 255), (10, 200, 250), (128, 128, 128)] {
                let (rp, gp, bp) = (vec![r; w * h], vec![g; w * h], vec![b; w * h]);
                let jxr = encode_color(&rp, &gp, &bp, w as u32, h as u32, QpSet::LOSSLESS);
                let expected = vec![(r, g, b); w * h];
                assert_rgb_exact(&jxr, w, h, &expected);
            }
        }
    }

    /// Each macroblock a distinct solid color → exercises the 3-component-weighted
    /// DC-direction prediction (LEFT/TOP/TOPLEFT) and per-component residuals
    /// across a multi-MB grid. Still flat *within* each MB, so DCONLY is exact.
    #[test]
    fn roundtrip_per_mb_constant_color() {
        let (mbw, mbh) = (3usize, 2usize);
        let (w, h) = (mbw * 16, mbh * 16);
        let color = |mx: usize, my: usize| -> (u8, u8, u8) {
            (((mx * 70 + 30) % 256) as u8, ((my * 90 + 40) % 256) as u8, ((mx * 50 + my * 33 + 17) % 256) as u8)
        };
        let (mut rp, mut gp, mut bp) = (vec![0u8; w * h], vec![0u8; w * h], vec![0u8; w * h]);
        let mut expected = vec![(0u8, 0u8, 0u8); w * h];
        for my in 0..mbh {
            for mx in 0..mbw {
                let (r, g, b) = color(mx, my);
                for py in 0..16 {
                    for px in 0..16 {
                        let i = (my * 16 + py) * w + (mx * 16 + px);
                        rp[i] = r;
                        gp[i] = g;
                        bp[i] = b;
                        expected[i] = (r, g, b);
                    }
                }
            }
        }
        let jxr = encode_color(&rp, &gp, &bp, w as u32, h as u32, QpSet::LOSSLESS);
        assert_rgb_exact(&jxr, w, h, &expected);
    }

    struct Lcg2(u64);
    impl Lcg2 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0
        }
    }

    /// Exact inverse of `transform::forward_transform_mb` (no overlap) — same as
    /// the grayscale test helper. Used to synthesize **zero-HP** pixel blocks.
    fn inverse_transform_mb(buf: &mut [i32; 256]) -> [i32; 256] {
        use crate::image::jxr_decode::consts::MB_PIXEL_MAP;
        use crate::image::jxr_decode::math::{str_idct4x4_stage1, str_idct4x4_stage2};
        let mut dclp = [0i32; 16];
        for j in 0..16 {
            dclp[j] = buf[j * 16];
        }
        str_idct4x4_stage2(&mut dclp);
        for j in 0..16 {
            buf[j * 16] = dclp[j];
        }
        for j in 0..16 {
            let mut blk = [0i32; 16];
            blk.copy_from_slice(&buf[j * 16..j * 16 + 16]);
            str_idct4x4_stage1(&mut blk);
            buf[j * 16..j * 16 + 16].copy_from_slice(&blk);
        }
        let mut s = [0i32; 256];
        for by in 0..4 {
            for bx in 0..4 {
                let bb = by * 16 + bx * 64;
                for py in 0..4 {
                    for px in 0..4 {
                        s[(by * 4 + py) * 16 + bx * 4 + px] = buf[bb + MB_PIXEL_MAP[px + py * 4]];
                    }
                }
            }
        }
        s
    }

    /// A zero-HP 16×16 block in a centered (YUV-coefficient) domain: random
    /// low-amplitude samples, forward, drop HP (keep the 16 block-DC/LP slots),
    /// inverse → centered pixels with **no** HP energy.
    fn zero_hp_centered(r: &mut Lcg2, amp: i32) -> [i32; 256] {
        let mut samples = [0i32; 256];
        for s in samples.iter_mut() {
            *s = (r.next() % (2 * amp as u64 + 1)) as i32 - amp;
        }
        let mut buf = transform::forward_transform_mb(&samples);
        for (p, v) in buf.iter_mut().enumerate() {
            if p % 16 != 0 {
                *v = 0;
            }
        }
        inverse_transform_mb(&mut buf)
    }

    /// NOHIGHPASS is lossless for **zero-HP** content: synthesize per-MB Y/U/V
    /// blocks with LP energy but no HP, map them to in-gamut RGB (the encoder's
    /// `fwd_color` recovers the exact YUV), and confirm a bit-exact round-trip.
    /// Exercises `cbplp_yuv1_444` + per-component LP run-level + chroma tables +
    /// shared scan + LP prediction + refinement.
    #[test]
    fn roundtrip_zero_hp_color_nohighpass() {
        let mut r = Lcg2(0x7e57_c01a_dead_beef);
        for &(mbw, mbh) in &[(1usize, 1usize), (2, 1), (1, 2), (2, 2), (3, 2)] {
            let (w, h) = (mbw * 16, mbh * 16);
            let (mut rp, mut gp, mut bp) = (vec![0u8; w * h], vec![0u8; w * h], vec![0u8; w * h]);
            let mut expected = vec![(0u8, 0u8, 0u8); w * h];
            for mx in 0..mbw {
                for my in 0..mbh {
                    // Retry until the whole MB lands in [0,255] for all channels.
                    let rgb = loop {
                        let yb = zero_hp_centered(&mut r, 12);
                        let ub = zero_hp_centered(&mut r, 8);
                        let vb = zero_hp_centered(&mut r, 8);
                        let mut rgb = [(0u8, 0u8, 0u8); 256];
                        let mut ok = true;
                        for p in 0..256 {
                            let (cr, cg, cb) = yuv444_to_rgb(yb[p], ub[p], vb[p]);
                            let (rr, gg, bb2) = (cr + 128, cg + 128, cb + 128);
                            if ![rr, gg, bb2].iter().all(|&x| (0..=255).contains(&x)) {
                                ok = false;
                                break;
                            }
                            rgb[p] = (rr as u8, gg as u8, bb2 as u8);
                        }
                        if ok {
                            break rgb;
                        }
                    };
                    for py in 0..16 {
                        for px in 0..16 {
                            let i = (my * 16 + py) * w + (mx * 16 + px);
                            let (rr, gg, bb2) = rgb[py * 16 + px];
                            rp[i] = rr;
                            gp[i] = gg;
                            bp[i] = bb2;
                            expected[i] = (rr, gg, bb2);
                        }
                    }
                }
            }
            let jxr = encode_color(&rp, &gp, &bp, w as u32, h as u32, QpSet::LOSSLESS);
            assert_rgb_exact(&jxr, w, h, &expected);
        }
    }
}
