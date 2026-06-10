//! Grayscale codestream-body encoder: DC + LP + HP (ALL_BANDS), spatial mode,
//! single tile, no overlap, uniform QP = 0 → **lossless for any grayscale
//! image**. Mirrors the decoder's `mb_dc` + `mb_lp` + `mb_cbphp` + `mb_hp_flex`
//! per-macroblock sequence, reusing its `AdaptiveVLC` / `AdaptiveScan` state.

use super::bitstream::BitWriter;
use super::quant::{quantize, scaling_factor, QpSet};
use super::{codestream, coeff, container, hp, transform};
use crate::decode::consts::*;
use crate::decode::state::{AdaptiveScan, AdaptiveVLC};
use crate::decode::tables;

const ABS_DELTA: [i32; 7] = [1, 0, -1, -1, -1, -1, -1]; // ABS_LEVEL_INDEX_DELTA[0]

/// One single-component (`INT_YONLY`) image plane: quantized coefficients plus
/// adaptive entropy state, encodable one macroblock at a time.
///
/// The per-MB granularity is what planar alpha (4a) composes on: an alpha
/// plane is YONLY by spec and per-MB **interleaved** with the primary plane in
/// the codestream, each plane carrying its own model/VLC/scan/CBPHP state — so
/// primary-gray and alpha are two instances of this struct sharing one
/// `BitWriter`.
pub(super) struct YOnlyPlane {
    pub(super) mbw: usize,
    pub(super) mbh: usize,
    buf_grid: Vec<Vec<[i32; 256]>>,
    dclp: Vec<Vec<[i32; 16]>>,
    cbphp_grid: Vec<Vec<i32>>,
    model_dc: coeff::ModelState,
    abs_dc: coeff::AdaptiveVlc1,
    model_lp: coeff::ModelState,
    first_ind: AdaptiveVLC,
    lp_ind0: AdaptiveVLC,
    lp_ind1: AdaptiveVLC,
    lp_abs0: AdaptiveVLC,
    lp_abs1: AdaptiveVLC,
    scan: AdaptiveScan,
    hp_state: hp::HpState,
}

impl YOnlyPlane {
    /// Pad `luma` to the 16-aligned MB grid (edge replication; the decoder
    /// crops back to the true dims since the header carries them and
    /// `windowing_flag = 0`), forward-transform every MB, quantize per band,
    /// and initialize the adaptive entropy state.
    pub(super) fn new(luma: &[u8], w: u32, h: u32, qp: QpSet) -> Self {
        let (dc_sf, lp_sf, hp_sf) =
            (scaling_factor(qp.dc), scaling_factor(qp.lp), scaling_factor(qp.hp));
        let (wu, hu) = (w as usize, h as usize);
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

        // Band state.
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

        YOnlyPlane {
            mbw,
            mbh,
            buf_grid,
            dclp,
            cbphp_grid: vec![vec![0i32; mbh]; mbw],
            model_dc: coeff::ModelState::init(0),
            abs_dc: coeff::AdaptiveVlc1::default(),
            model_lp: coeff::ModelState::init(1),
            first_ind,
            lp_ind0,
            lp_ind1,
            lp_abs0,
            lp_abs1,
            scan: AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H),
            hp_state: hp::HpState::new(),
        }
    }

    /// Emit one macroblock's DC + LP + HP(+flex) bits, updating this plane's
    /// adaptive state exactly as the decoder's per-MB readers do.
    pub(super) fn encode_mb(&mut self, bw: &mut BitWriter, mbx: usize, mby: usize) {
        let c = self.dclp[mbx][mby];
        let (is_left, is_top) = (mbx == 0, mby == 0);

        // ---------- DC ----------
        let (dmode, dc_pred) = if is_left && is_top {
            (NO_PREDICTION, 0)
        } else if is_left {
            (PREDICT_FROM_TOP, self.dclp[mbx][mby - 1][0])
        } else if is_top {
            (PREDICT_FROM_LEFT, self.dclp[mbx - 1][mby][0])
        } else {
            let (left, top, tl) = (
                self.dclp[mbx - 1][mby][0],
                self.dclp[mbx][mby - 1][0],
                self.dclp[mbx - 1][mby - 1][0],
            );
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
            bw,
            c[0] - dc_pred,
            self.model_dc.m_bits,
            tables::abs_level_index(self.abs_dc.table_index as usize),
        );
        let dc_lap = if b_abs {
            self.abs_dc.discrim += ABS_DELTA[abs_idx as usize];
            1
        } else {
            0
        };
        self.model_dc.update(dc_lap, 0);
        if mbx % 16 == 0 || mbx == self.mbw - 1 {
            self.abs_dc.adapt();
        }

        // ---------- LP ----------
        if mbx % 16 == 0 {
            self.scan.reset_totals();
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
                self.dclp[mbx - 1][mby][j]
            } else if lp_mode == PREDICT_FROM_TOP && matches!(j, 4 | 8 | 12) {
                self.dclp[mbx][mby - 1][j]
            } else {
                0
            };
            res[j] = c[j] - pred;
        }
        let mb = self.model_lp.m_bits as u32;
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
                let pos = self.scan.translate(i);
                if coarse[pos] != 0 {
                    pairs.push((run, coarse[pos]));
                    run = 0;
                    self.scan.adapt(i);
                } else {
                    run += 1;
                }
            }
            lp_lap = pairs.len() as i32;
            coeff::encode_block(
                bw,
                &pairs,
                1,
                &mut self.first_ind,
                &mut self.lp_ind0,
                &mut self.lp_ind1,
                &mut self.lp_abs0,
                &mut self.lp_abs1,
            );
        }
        if mb > 0 {
            for j in 1..16 {
                coeff::encode_refine_lp(bw, coarse[j], res[j], mb as i32);
            }
        }
        self.model_lp.update(lp_lap, 1);
        if mbx % 16 == 0 || mbx == self.mbw - 1 {
            self.first_ind.adapt_table2(4);
            self.lp_ind0.adapt_table2(3);
            self.lp_ind1.adapt_table2(3);
            self.lp_abs0.adapt_table1();
            self.lp_abs1.adapt_table1();
        }

        // ---------- HP ----------
        if mbx % 16 == 0 {
            self.hp_state.hor_scan.reset_totals();
            self.hp_state.ver_scan.reset_totals();
        }
        let cbphp_left = if is_left { 0 } else { self.cbphp_grid[mbx - 1][mby] };
        let cbphp_top = if is_top { 0 } else { self.cbphp_grid[mbx][mby - 1] };
        let mbcbp = hp::encode_hp_mb(
            bw,
            &mut self.hp_state,
            &self.buf_grid[mbx][mby],
            &c,
            cbphp_left,
            cbphp_top,
            is_left,
            is_top,
        );
        self.cbphp_grid[mbx][mby] = mbcbp;
        if mbx % 16 == 0 || mbx == self.mbw - 1 {
            self.hp_state.adapt();
        }
    }
}

/// Encode a grayscale image (any size) as ALL_BANDS at the given per-band
/// quantizers. `QpSet::LOSSLESS` (all 0 ⇒ scaling factor 1) is bit-exact.
pub fn encode_grayscale(luma: &[u8], w: u32, h: u32, qp: QpSet) -> Vec<u8> {
    let mut plane = YOnlyPlane::new(luma, w, h, qp);
    let mut bw = BitWriter::new();
    codestream::write_image_header(&mut bw, w, h, OUT_YONLY, false, false);
    codestream::write_image_plane_header_gray_allbands(&mut bw, qp.dc, qp.lp, qp.hp);
    codestream::write_vlw_esc(&mut bw, 0);
    codestream::write_common_tile_header(&mut bw);
    for mby in 0..plane.mbh {
        for mbx in 0..plane.mbw {
            plane.encode_mb(&mut bw, mbx, mby);
        }
    }
    bw.align_to_byte();
    container::write_container(&bw.finish(), w, h, &container::pixel_format::GRAY8)
}
