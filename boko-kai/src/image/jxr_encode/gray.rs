//! Grayscale codestream-body encoder: DC + LP + HP (ALL_BANDS), spatial mode,
//! single tile, no overlap, uniform QP = 0 → **lossless for any grayscale
//! image**. Mirrors the decoder's `mb_dc` + `mb_lp` + `mb_cbphp` + `mb_hp_flex`
//! per-macroblock sequence, reusing its `AdaptiveVLC` / `AdaptiveScan` state.

use super::bitstream::BitWriter;
use super::quant::{quantize, scaling_factor, QpSet};
use super::{codestream, coeff, container, hp, transform};
use crate::image::jxr_decode::consts::*;
use crate::image::jxr_decode::state::{AdaptiveScan, AdaptiveVLC};
use crate::image::jxr_decode::tables;

const ABS_DELTA: [i32; 7] = [1, 0, -1, -1, -1, -1, -1]; // ABS_LEVEL_INDEX_DELTA[0]

/// Encode a grayscale image (any size) as ALL_BANDS at the given per-band
/// quantizers. `QpSet::LOSSLESS` (all 0 ⇒ scaling factor 1) is bit-exact.
pub fn encode_grayscale(luma: &[u8], w: u32, h: u32, qp: QpSet) -> Vec<u8> {
    let (dc_sf, lp_sf, hp_sf) = (scaling_factor(qp.dc), scaling_factor(qp.lp), scaling_factor(qp.hp));
    let (wu, hu) = (w as usize, h as usize);
    // Pad to a 16-aligned grid with edge replication; the decoder crops back to
    // (w, h) since the header carries the true dims and windowing_flag = 0.
    let (pw, ph) = (wu.next_multiple_of(16), hu.next_multiple_of(16));
    let (mbw, mbh) = (pw / 16, ph / 16);
    let padded: std::borrow::Cow<[u8]> = if pw == wu && ph == hu {
        std::borrow::Cow::Borrowed(luma)
    } else {
        let mut p = vec![0u8; pw * ph];
        for y in 0..ph {
            let sy = y.min(hu - 1);
            for x in 0..pw {
                p[y * pw + x] = luma[sy * wu + x.min(wu - 1)];
            }
        }
        std::borrow::Cow::Owned(p)
    };

    // Forward transform every MB → full mb_buffer (HP within blocks) + dclp.
    let mut buf_grid = vec![vec![[0i32; 256]; mbh]; mbw];
    let mut dclp = vec![vec![[0i32; 16]; mbh]; mbw];
    for mbx in 0..mbw {
        for mby in 0..mbh {
            let mut samples = [0i32; 256];
            for py in 0..16 {
                for px in 0..16 {
                    let g = (mby * 16 + py) * pw + (mbx * 16 + px);
                    samples[py * 16 + px] = padded[g] as i32 - 128;
                }
            }
            let mut buf = transform::forward_transform_mb(&samples);
            // Quantize the raw coefficients in place; prediction below runs in
            // the level domain. HP lives at within-block positions blk*16+1..16.
            if hp_sf > 1 {
                for blk in 0..16 {
                    for pos in 1..16 {
                        let idx = blk * 16 + pos;
                        buf[idx] = quantize(buf[idx], hp_sf);
                    }
                }
            }
            let mut c = [0i32; 16];
            for (j, s) in c.iter_mut().enumerate() {
                *s = buf[16 * ICT4X4_INV_PERM[j]]; // DC/LP at the block-DC slots
            }
            c[0] = quantize(c[0], dc_sf);
            for s in c.iter_mut().skip(1) {
                *s = quantize(*s, lp_sf);
            }
            buf_grid[mbx][mby] = buf;
            dclp[mbx][mby] = c;
        }
    }

    let mut bw = BitWriter::new();
    codestream::write_image_header(&mut bw, w, h);
    codestream::write_image_plane_header_gray_allbands(&mut bw, qp.dc, qp.lp, qp.hp);
    codestream::write_vlw_esc(&mut bw, 0);
    codestream::write_common_tile_header(&mut bw);

    // Band state.
    let mut model_dc = coeff::ModelState::init(0);
    let mut abs_dc = coeff::AdaptiveVlc1::default();
    let mut model_lp = coeff::ModelState::init(1);
    let mut first_ind = AdaptiveVLC::default();
    let mut lp_ind0 = AdaptiveVLC::default();
    let mut lp_ind1 = AdaptiveVLC::default();
    let mut lp_abs0 = AdaptiveVLC::default();
    let mut lp_abs1 = AdaptiveVLC::default();
    first_ind.init_table2();
    lp_ind0.init_table2();
    lp_ind1.init_table2();
    lp_abs0.init_table1();
    lp_abs1.init_table1();
    let mut scan = AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H);
    let mut hp_state = hp::HpState::new();
    let mut cbphp_grid = vec![vec![0i32; mbh]; mbw];

    for mby in 0..mbh {
        for mbx in 0..mbw {
            let c = dclp[mbx][mby];
            let (is_left, is_top) = (mbx == 0, mby == 0);

            // ---------- DC ----------
            let (dmode, dc_pred) = if is_left && is_top {
                (NO_PREDICTION, 0)
            } else if is_left {
                (PREDICT_FROM_TOP, dclp[mbx][mby - 1][0])
            } else if is_top {
                (PREDICT_FROM_LEFT, dclp[mbx - 1][mby][0])
            } else {
                let (left, top, tl) =
                    (dclp[mbx - 1][mby][0], dclp[mbx][mby - 1][0], dclp[mbx - 1][mby - 1][0]);
                let (sh, sv) = ((tl - left).abs(), (tl - top).abs());
                if sh * 4 < sv {
                    (PREDICT_FROM_TOP, top)
                } else if sv * 4 < sh {
                    (PREDICT_FROM_LEFT, left)
                } else {
                    (PREDICT_FROM_TOP_LEFT, (top + left) >> 1)
                }
            };
            let (b_abs, abs_idx) = coeff::encode_dc_value(
                &mut bw,
                c[0] - dc_pred,
                model_dc.m_bits,
                tables::abs_level_index(abs_dc.table_index as usize),
            );
            let dc_lap = if b_abs {
                abs_dc.discrim += ABS_DELTA[abs_idx as usize];
                1
            } else {
                0
            };
            model_dc.update(dc_lap, 0);
            if mbx % 16 == 0 || mbx == mbw - 1 {
                abs_dc.adapt();
            }

            // ---------- LP ----------
            if mbx % 16 == 0 {
                scan.reset_totals();
            }
            let lp_mode = if dmode == PREDICT_FROM_LEFT {
                PREDICT_FROM_LEFT
            } else if dmode == PREDICT_FROM_TOP {
                PREDICT_FROM_TOP
            } else {
                NO_PREDICTION
            };
            let mut res = [0i32; 16];
            for j in 1..16 {
                let pred = if lp_mode == PREDICT_FROM_LEFT && matches!(j, 1 | 2 | 3) {
                    dclp[mbx - 1][mby][j]
                } else if lp_mode == PREDICT_FROM_TOP && matches!(j, 4 | 8 | 12) {
                    dclp[mbx][mby - 1][j]
                } else {
                    0
                };
                res[j] = c[j] - pred;
            }
            let mb = model_lp.m_bits as u32;
            let mut coarse = [0i32; 16];
            for j in 1..16 {
                let m = res[j].unsigned_abs() >> mb;
                coarse[j] = if res[j] < 0 { -(m as i32) } else { m as i32 };
            }
            let cbp = (1..16).any(|j| coarse[j] != 0);
            bw.write_flag(cbp);
            let mut lp_lap = 0;
            if cbp {
                let mut pairs: Vec<(u32, i32)> = Vec::new();
                let mut run = 0u32;
                for i in 1..16usize {
                    let pos = scan.translate(i);
                    if coarse[pos] != 0 {
                        pairs.push((run, coarse[pos]));
                        run = 0;
                        scan.adapt(i);
                    } else {
                        run += 1;
                    }
                }
                lp_lap = pairs.len() as i32;
                coeff::encode_block(
                    &mut bw, &pairs, 1, &mut first_ind, &mut lp_ind0, &mut lp_ind1, &mut lp_abs0,
                    &mut lp_abs1,
                );
            }
            if mb > 0 {
                for j in 1..16 {
                    coeff::encode_refine_lp(&mut bw, coarse[j], res[j], mb as i32);
                }
            }
            model_lp.update(lp_lap, 1);
            if mbx % 16 == 0 || mbx == mbw - 1 {
                first_ind.adapt_table2(4);
                lp_ind0.adapt_table2(3);
                lp_ind1.adapt_table2(3);
                lp_abs0.adapt_table1();
                lp_abs1.adapt_table1();
            }

            // ---------- HP ----------
            if mbx % 16 == 0 {
                hp_state.hor_scan.reset_totals();
                hp_state.ver_scan.reset_totals();
            }
            let cbphp_left = if is_left { 0 } else { cbphp_grid[mbx - 1][mby] };
            let cbphp_top = if is_top { 0 } else { cbphp_grid[mbx][mby - 1] };
            let mbcbp = hp::encode_hp_mb(
                &mut bw,
                &mut hp_state,
                &buf_grid[mbx][mby],
                &c,
                cbphp_left,
                cbphp_top,
                is_left,
                is_top,
            );
            cbphp_grid[mbx][mby] = mbcbp;
            if mbx % 16 == 0 || mbx == mbw - 1 {
                hp_state.adapt();
            }
        }
    }
    bw.align_to_byte();
    container::write_container(&bw.finish(), w, h, &container::pixel_format::GRAY8)
}
