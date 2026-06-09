//! Forward color transform for the encoder's **color** mode (`ColorMode::Color`).
//!
//! JPEG-XR's color path is `RGB → internal YUV (YCoCg-like) → per-plane PCT`.
//! [`rgb_to_yuv444`] is the **exact integer inverse** of the decoder's
//! `decode::decoder::yuv444_to_rgb` lifting (which is the
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
use super::{codestream, coeff, container, hp, transform};
use crate::decode::consts::*;
use crate::decode::decoder::{ceil_div2, floor_div2};
use crate::decode::math::{chroma_component, num_ones};
use crate::decode::state::{AdaptiveScan, AdaptiveVLC, CBPHPModel};
use crate::decode::tables;

// From the decoder's `mb_cbphp` (i_out/i_off/i_flc), same as `hp.rs`.
const I_OUT: [i32; 16] = [0, 15, 3, 12, 1, 2, 4, 8, 5, 6, 9, 10, 7, 11, 13, 14];
const I_OFF: [i32; 6] = [0, 4, 2, 8, 12, 1];
const I_FLC: [u32; 6] = [0, 2, 1, 2, 2, 0];

const ABS_DELTA: [i32; 7] = [1, 0, -1, -1, -1, -1, -1]; // ABS_LEVEL_INDEX_DELTA[0]

/// Forward color transform: centered `RGB → (Y, U, V)`, the exact inverse of the
/// decoder's `yuv444_to_rgb`. Inputs are **pre-bias** (input pixel − 128 for
/// BD8); outputs are the internal coefficients the per-plane PCT consumes.
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
    let (dc_sf, lp_sf, hp_sf) =
        (scaling_factor(qp.dc), scaling_factor(qp.lp), scaling_factor(qp.hp));
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

    // Per-MB, per-component quantized DC+LP levels (`dclp`) and the full forward
    // buffers (`buf_grid`, HP quantized) for the HP band.
    let mut dclp = vec![vec![[[0i32; 16]; 3]; mbh]; mbw];
    let mut buf_grid = vec![vec![[[0i32; 256]; 3]; mbh]; mbw];
    for mbx in 0..mbw {
        for mby in 0..mbh {
            for (comp, plane) in yuv.iter().enumerate() {
                let mut samples = [0i32; 256];
                for py in 0..16 {
                    for px in 0..16 {
                        samples[py * 16 + px] = plane[(mby * 16 + py) * pw + (mbx * 16 + px)];
                    }
                }
                let mut buf = transform::forward_transform_mb(&samples);
                if hp_sf > 1 {
                    for blk in 0..16 {
                        for pos in 1..16 {
                            let i = blk * 16 + pos;
                            buf[i] = quantize(buf[i], hp_sf);
                        }
                    }
                }
                let c = &mut dclp[mbx][mby][comp];
                for (j, slot) in c.iter_mut().enumerate() {
                    *slot = buf[16 * ICT4X4_INV_PERM[j]];
                }
                c[0] = quantize(c[0], dc_sf);
                for s in c.iter_mut().skip(1) {
                    *s = quantize(*s, lp_sf);
                }
                buf_grid[mbx][mby][comp] = buf;
            }
        }
    }

    let mut bw = BitWriter::new();
    codestream::write_image_header(&mut bw, w, h, OUT_RGB);
    codestream::write_image_plane_header_color_allbands(&mut bw, qp.dc, qp.lp, qp.hp);
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
    // HP band state.
    let mut hp_state = ColorHpState::new();
    let mut cbphp_grid = vec![vec![[0i32; 3]; mbh]; mbw];

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

            // ---------- HP ----------
            if mbx % 16 == 0 {
                hp_state.hor_scan.reset_totals();
                hp_state.ver_scan.reset_totals();
            }
            let cbphp_left = if is_left { [0; 3] } else { cbphp_grid[mbx - 1][mby] };
            let cbphp_top = if is_top { [0; 3] } else { cbphp_grid[mbx][mby - 1] };
            cbphp_grid[mbx][mby] = encode_color_hp_mb(
                &mut bw,
                &mut hp_state,
                &buf_grid[mbx][mby],
                &dclp[mbx][mby],
                cbphp_left,
                cbphp_top,
                is_left,
                is_top,
            );
            if mbx % 16 == 0 || mbx == mbw - 1 {
                hp_state.adapt();
            }
        }
    }
    bw.align_to_byte();
    container::write_container(&bw.finish(), w, h, &container::pixel_format::RGB24)
}

/// HP-band adaptive state for the color path — multi-component analogue of
/// [`hp::HpState`]: 2-model `model_hp`, **chroma-split** index tables (lum+chr),
/// shared abs tables, shared hor/ver scans, shared `num_cbphp`/`num_blk_cbphp`,
/// and a `CBPHPModel` with two `chroma_flag` slots.
struct ColorHpState {
    model: coeff::ColorModel,
    first: [AdaptiveVLC; 2], // [lum, chr]
    ind0: [AdaptiveVLC; 2],
    ind1: [AdaptiveVLC; 2],
    abs0: AdaptiveVLC,
    abs1: AdaptiveVLC,
    hor_scan: AdaptiveScan,
    ver_scan: AdaptiveScan,
    num_cbphp: AdaptiveVLC,
    num_blk_cbphp: AdaptiveVLC,
    cbphp_model: CBPHPModel,
}

impl ColorHpState {
    fn new() -> Self {
        let mut s = ColorHpState {
            model: coeff::ColorModel::init(2), // HP ⇒ m_bits = 0
            first: [AdaptiveVLC::default(), AdaptiveVLC::default()],
            ind0: [AdaptiveVLC::default(), AdaptiveVLC::default()],
            ind1: [AdaptiveVLC::default(), AdaptiveVLC::default()],
            abs0: AdaptiveVLC::default(),
            abs1: AdaptiveVLC::default(),
            hor_scan: AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H_PRIME),
            ver_scan: AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_V_PRIME),
            num_cbphp: AdaptiveVLC::default(),
            num_blk_cbphp: AdaptiveVLC::default(),
            cbphp_model: CBPHPModel::default(),
        };
        for t in s.first.iter_mut().chain(s.ind0.iter_mut()).chain(s.ind1.iter_mut()) {
            t.init_table2();
        }
        s.abs0.init_table1();
        s.abs1.init_table1();
        s.num_cbphp.init_table1();
        s.num_blk_cbphp.init_table1();
        s.cbphp_model.cbphp_state = [0, 0];
        s.cbphp_model.count_ones = [-4, -4];
        s.cbphp_model.count_zeroes = [4, 4];
        s
    }

    fn adapt(&mut self) {
        for t in self.first.iter_mut() {
            t.adapt_table2(4);
        }
        for t in self.ind0.iter_mut().chain(self.ind1.iter_mut()) {
            t.adapt_table2(3);
        }
        self.abs0.adapt_table1();
        self.abs1.adapt_table1();
        self.num_cbphp.adapt_table1();
        self.num_blk_cbphp.adapt_table1();
    }
}

/// Emit the `refine_cbphp` pattern bits for a nibble whose popcount is `num`
/// (inverse of `Decoder::refine_cbphp`). Used for the top-level 4-bit `i_cbphp`
/// and for each chroma nibble.
fn write_cbphp_refine(bw: &mut BitWriter, nibble: i32, num: i32) {
    match num {
        1 => bw.write_bits(nibble.trailing_zeros() as u64, 2),
        2 => write_huff(bw, tables::ref_cbphp1(), nibble),
        3 => bw.write_bits((0x0F ^ nibble).trailing_zeros() as u64, 2),
        _ => {} // 0 and 4 carry no bits
    }
}

/// Encode the three components' `mb_cbphp` (16-bit per-block HP coded-block
/// patterns) — inverse of the YUV444 `mb_cbphp` reader + `pred_cbphp_444`. First
/// unpredict each component (cascade + neighbour bit, per `chroma_flag` state)
/// and update the CBPHP model, then emit the single interleaved structure: a
/// luma `i_cbphp`/`num_cbphp`, and per present block-group a `num_blk_cbphp`
/// carrying the luma nibble plus (via the `i_val≥6` escape) `chr_cbphp` /
/// `val_inc` and the chroma nibbles (`num_ch_blk` + refine).
fn encode_color_cbphp(
    bw: &mut BitWriter,
    st: &mut ColorHpState,
    mb_cbphp: [i32; 3],
    cbphp_left: [i32; 3],
    cbphp_top: [i32; 3],
    is_left: bool,
    is_top: bool,
) {
    // --- unpredict per component + update CBPHP model ---
    let mut i_diff = [0i32; 3];
    for comp in 0..3 {
        let cf = (comp > 0) as usize;
        let neighbor = if is_left {
            if is_top { 1 } else { (cbphp_top[comp] >> 10) & 1 }
        } else {
            (cbphp_left[comp] >> 5) & 1
        };
        i_diff[comp] = match st.cbphp_model.cbphp_state[cf] {
            0 => hp::unpredict_cascade(mb_cbphp[comp]) ^ neighbor,
            2 => mb_cbphp[comp] ^ 0xFFFF,
            _ => mb_cbphp[comp],
        };
        let n_orig = num_ones(mb_cbphp[comp] as u32) as i32;
        let m = &mut st.cbphp_model;
        m.count_ones[cf] = (m.count_ones[cf] + n_orig - 3).clamp(-16, 15);
        m.count_zeroes[cf] = (m.count_zeroes[cf] + (16 - n_orig) - 3).clamp(-16, 15);
        m.cbphp_state[cf] = if m.count_ones[cf] < 0 {
            if m.count_ones[cf] < m.count_zeroes[cf] { 1 } else { 2 }
        } else if m.count_zeroes[cf] < 0 {
            2
        } else {
            0
        };
    }

    // --- code the interleaved structure ---
    let nib = |d: i32, b: usize| (d >> (b * 4)) & 0xF;
    let mut i_cbphp = 0i32;
    for b in 0..4 {
        if nib(i_diff[0], b) != 0 || nib(i_diff[1], b) != 0 || nib(i_diff[2], b) != 0 {
            i_cbphp |= 1 << b;
        }
    }
    let num_cbphp = num_ones(i_cbphp as u32) as i32;
    write_huff(bw, tables::num_cbphp(st.num_cbphp.table_index as usize), num_cbphp);
    st.num_cbphp.discrim_val1 += NUM_CBPHP_DELTA[st.num_cbphp.delta_table_index as usize][num_cbphp as usize];
    write_cbphp_refine(bw, i_cbphp, num_cbphp);

    for b in 0..4 {
        if i_cbphp & (1 << b) == 0 {
            continue;
        }
        let luma_nib = nib(i_diff[0], b);
        let u_nib = nib(i_diff[1], b);
        let v_nib = nib(i_diff[2], b);
        let chroma_bits =
            (if u_nib != 0 { 0x10 } else { 0 }) | (if v_nib != 0 { 0x20 } else { 0 });
        let i_code = I_OUT.iter().position(|&x| x == luma_nib).unwrap() as i32;
        let base = (0..=5usize)
            .find(|&v| i_code >= I_OFF[v] && i_code < I_OFF[v] + (1 << I_FLC[v]))
            .unwrap();
        let num_blk = if chroma_bits != 0 {
            // chroma present: i_val = base + 6, with val_inc for base ≥ 3.
            if base <= 2 { base as i32 + 5 } else { 8 }
        } else {
            base as i32 - 1 // luma-only present group ⇒ base ∈ 1..=5
        };
        write_huff(bw, tables::num_blkcbphp2(st.num_blk_cbphp.table_index as usize), num_blk);
        st.num_blk_cbphp.discrim_val1 +=
            NUM_BLK_CBPHP_DELTA2[st.num_blk_cbphp.delta_table_index as usize][num_blk as usize];
        if chroma_bits != 0 {
            let chr = (chroma_bits >> 4) - 1; // 0=U, 1=V, 2=both
            write_huff(bw, tables::chr_cbphp(), chr);
            if base >= 3 {
                write_huff(bw, tables::val_inc(), base as i32 - 3);
            }
        }
        if I_FLC[base] != 0 {
            bw.write_bits((i_code - I_OFF[base]) as u64, I_FLC[base]);
        }
        if chroma_bits != 0 {
            for k in 0..2 {
                if (chroma_bits >> (k + 4)) & 1 != 0 {
                    let cnib = if k == 0 { u_nib } else { v_nib };
                    let num = num_ones(cnib as u32) as i32;
                    write_huff(bw, tables::num_ch_blk(), num - 1);
                    write_cbphp_refine(bw, cnib, num);
                }
            }
        }
    }
}

/// Encode the HP band of one macroblock for all 3 components — inverse of
/// `mb_cbphp` + `mb_hp_flex` + `hp_transform_coefficient_decoding`. `bufs` are
/// the per-component forward `mb_buffer`s (HP quantized at within-block
/// positions); `lp` the per-component DC+LP levels (for the shared HP pred
/// mode). Returns each component's `mb_cbphp` for neighbour prediction.
fn encode_color_hp_mb(
    bw: &mut BitWriter,
    st: &mut ColorHpState,
    bufs: &[[i32; 256]; 3],
    lp: &[[i32; 16]; 3],
    cbphp_left: [i32; 3],
    cbphp_top: [i32; 3],
    is_left: bool,
    is_top: bool,
) -> [i32; 3] {
    // Shared HP prediction mode: luma LP + chroma LP (1st h/v) strength.
    let mut s_hor = lp[0][1].abs() + lp[0][2].abs() + lp[0][3].abs();
    let mut s_ver = lp[0][4].abs() + lp[0][8].abs() + lp[0][12].abs();
    for c in 1..3 {
        s_hor += lp[c][1].abs();
        s_ver += lp[c][4].abs();
    }
    let mode = if s_hor * 4 < s_ver {
        PREDICT_FROM_TOP
    } else if s_ver * 4 < s_hor {
        PREDICT_FROM_LEFT
    } else {
        NO_PREDICTION
    };

    let mut res = [[[0i32; 16]; 16]; 3];
    let mut coarse = [[[0i32; 16]; 16]; 3];
    let mut mb_cbphp = [0i32; 3];
    for comp in 0..3 {
        let buf = &bufs[comp];
        let r = &mut res[comp];
        for blk in 0..16 {
            for pos in 1..16 {
                r[blk][pos] = buf[blk * 16 + pos];
            }
        }
        if mode == PREDICT_FROM_TOP {
            for &blk in &[1usize, 2, 3, 5, 6, 7, 9, 10, 11, 13, 14, 15] {
                for &k in &[2usize, 10, 9] {
                    r[blk][k] = buf[blk * 16 + k] - buf[(blk - 1) * 16 + k];
                }
            }
        } else if mode == PREDICT_FROM_LEFT {
            for blk in 4..16 {
                for &k in &[1usize, 5, 6] {
                    r[blk][k] = buf[blk * 16 + k] - buf[(blk - 4) * 16 + k];
                }
            }
        }
        let mbits = st.model.m_bits[chroma_component(comp)] as u32;
        for blk in 0..16 {
            for pos in 1..16 {
                let v = r[blk][pos];
                let c = (v.unsigned_abs() >> mbits) as i32;
                coarse[comp][blk][pos] = if v < 0 { -c } else { c };
            }
        }
        let mut cbp = 0i32;
        for k in 0..16 {
            let blk = I_HIER_SCAN_ORDER[k];
            if (1..16).any(|pos| coarse[comp][blk][pos] != 0) {
                cbp |= 1 << k;
            }
        }
        mb_cbphp[comp] = cbp;
    }

    encode_color_cbphp(bw, st, mb_cbphp, cbphp_left, cbphp_top, is_left, is_top);

    let mut lap = [0i32; 2];
    for comp in 0..3 {
        let chroma = chroma_component(comp);
        let mbits = st.model.m_bits[chroma];
        let mut cbp = mb_cbphp[comp];
        for k in 0..16 {
            let blk = I_HIER_SCAN_ORDER[k];
            if cbp & 1 != 0 {
                let scan = if mode == PREDICT_FROM_TOP { &mut st.ver_scan } else { &mut st.hor_scan };
                let mut pairs: Vec<(u32, i32)> = Vec::new();
                let mut run = 0u32;
                for i in 1..16usize {
                    let pos = scan.translate(i);
                    if coarse[comp][blk][pos] != 0 {
                        pairs.push((run, coarse[comp][blk][pos]));
                        run = 0;
                        scan.adapt(i);
                    } else {
                        run += 1;
                    }
                }
                lap[chroma] += pairs.len() as i32;
                coeff::encode_block(
                    bw,
                    &pairs,
                    1,
                    &mut st.first[chroma],
                    &mut st.ind0[chroma],
                    &mut st.ind1[chroma],
                    &mut st.abs0,
                    &mut st.abs1,
                );
            }
            cbp >>= 1;
            if mbits > 0 {
                for &n in &I_TRANSPOSE_FLEX[1..] {
                    coeff::encode_refine_lp(bw, coarse[comp][blk][n], res[comp][blk][n], mbits);
                }
            }
        }
    }
    st.model.update(lap, 2, 3);
    mb_cbphp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decoder::yuv444_to_rgb;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
        /// A well-distributed byte from the LCG's **high** bits. (The low 8 bits
        /// have period 256, so `next() % 256` aliases when plane sizes are a
        /// multiple of 256 — making R/G/B identical and chroma zero. High bits
        /// have full period, so the three channels genuinely differ → the chroma
        /// HP/LP path is actually exercised.)
        fn byte(&mut self) -> u8 {
            (self.next() >> 32) as u8
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

    fn decode(jxr: &[u8]) -> crate::decode::decoder::DecodedImage {
        let c = crate::decode::container::parse(jxr).expect("container parse");
        crate::decode::decoder::Decoder::new(c.image_data).decode().expect("decode")
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

    /// The real goal: ANY color image round-trips **exactly** (ALL_BANDS,
    /// lossless 4:4:4). Exercises the full HP band — `mb_cbphp` YUV coding,
    /// per-component run-level + flex, chroma tables, shared scans — on top of
    /// DC + LP, across multi-MB grids.
    #[test]
    fn roundtrip_arbitrary_color_allbands_lossless() {
        let mut r = Lcg(0x4242_a5a5_1234_5678);
        for &(mbw, mbh) in &[(1usize, 1usize), (2, 1), (1, 2), (2, 2), (3, 3)] {
            let (w, h) = (mbw * 16, mbh * 16);
            let rp: Vec<u8> = (0..w * h).map(|_| r.byte()).collect();
            let gp: Vec<u8> = (0..w * h).map(|_| r.byte()).collect();
            let bp: Vec<u8> = (0..w * h).map(|_| r.byte()).collect();
            let expected: Vec<(u8, u8, u8)> = (0..w * h).map(|i| (rp[i], gp[i], bp[i])).collect();
            let jxr = encode_color(&rp, &gp, &bp, w as u32, h as u32, QpSet::LOSSLESS);
            assert_rgb_exact(&jxr, w, h, &expected);
        }
    }

    /// Arbitrary color at non-16-aligned dims (edge-pad + decoder crop).
    #[test]
    fn roundtrip_non_aligned_color_allbands_lossless() {
        let mut r = Lcg(0x9e37_79b9_7f4a_7c15);
        for &(w, h) in &[(17usize, 31usize), (100, 50), (33, 16), (16, 33), (45, 45), (1, 1)] {
            let rp: Vec<u8> = (0..w * h).map(|_| r.byte()).collect();
            let gp: Vec<u8> = (0..w * h).map(|_| r.byte()).collect();
            let bp: Vec<u8> = (0..w * h).map(|_| r.byte()).collect();
            let expected: Vec<(u8, u8, u8)> = (0..w * h).map(|i| (rp[i], gp[i], bp[i])).collect();
            let jxr = encode_color(&rp, &gp, &bp, w as u32, h as u32, QpSet::LOSSLESS);
            assert_rgb_exact(&jxr, w, h, &expected);
        }
    }

    /// Lossy color is a fixpoint: a decoded image is already on the quant grid,
    /// so re-encoding yields a byte-identical JXR (encode∘decode∘encode). Holds
    /// iff the forward color transform + per-band quant invert the decoder
    /// exactly. Mid-range pixels + aligned sizes avoid clamp/padding perturbation.
    #[test]
    fn lossy_color_roundtrip_is_a_fixpoint() {
        let mut r = Lcg(0x1357_9bdf_2468_ace0);
        let qps = [
            QpSet { dc: 4, lp: 8, hp: 16 },
            QpSet { dc: 8, lp: 16, hp: 32 },
            QpSet { dc: 1, lp: 4, hp: 6 },
        ];
        for &(w, h) in &[(32u32, 32u32), (48, 32), (64, 48)] {
            let n = (w * h) as usize;
            let mk = |r: &mut Lcg| -> Vec<u8> { (0..n).map(|_| 96 + (r.byte() % 64)).collect() };
            let (rp, gp, bp) = (mk(&mut r), mk(&mut r), mk(&mut r));
            for &qp in &qps {
                let jxr1 = encode_color(&rp, &gp, &bp, w, h, qp);
                let d = decode(&jxr1);
                let ch = |c: usize| -> Vec<u8> { d.image_plane[c].iter().map(|&v| v.clamp(0, 255) as u8).collect() };
                let jxr2 = encode_color(&ch(0), &ch(1), &ch(2), w, h, qp);
                assert_eq!(jxr1, jxr2, "not a fixpoint qp={qp:?} {w}x{h}");
            }
        }
    }

    /// Clean synthetic color master with all-band energy per channel: coarser
    /// quant ⇒ strictly more error (rules out a deadzone/rounding bug the
    /// fixpoint alone wouldn't catch).
    #[test]
    fn lossy_color_error_grows_with_qp() {
        let (w, h) = (64usize, 64usize);
        let mk = |off: i32| -> Vec<u8> {
            (0..w * h)
                .map(|i| {
                    let (x, y) = ((i % w) as i32, (i / w) as i32);
                    (110 + off + (x % 17) - (y % 13) + ((x * y) % 11) * 3).clamp(0, 255) as u8
                })
                .collect()
        };
        let (rp, gp, bp) = (mk(0), mk(25), mk(-18));
        let mse = |qp: QpSet| -> f64 {
            let d = decode(&encode_color(&rp, &gp, &bp, w as u32, h as u32, qp));
            let src = [&rp, &gp, &bp];
            let mut se = 0.0f64;
            for c in 0..3 {
                for i in 0..w * h {
                    let e = src[c][i] as f64 - d.image_plane[c][i].clamp(0, 255) as f64;
                    se += e * e;
                }
            }
            se / (3 * w * h) as f64
        };
        let m0 = mse(QpSet::LOSSLESS);
        let m4 = mse(QpSet { dc: 16, lp: 16, hp: 16 });
        let m8 = mse(QpSet { dc: 32, lp: 32, hp: 32 });
        assert_eq!(m0, 0.0, "lossless must be exact");
        assert!(m4 > 0.0 && m8 > m4, "error must grow with QP: m0={m0} m4={m4} m8={m8}");
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
        use crate::decode::consts::MB_PIXEL_MAP;
        use crate::decode::math::{str_idct4x4_stage1, str_idct4x4_stage2};
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
