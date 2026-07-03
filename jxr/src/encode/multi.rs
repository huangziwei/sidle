//! Multi-component plane encoder (`INT_YUVK` / `INT_NCOMPONENT`) — the
//! forward mirror of the decoder's per-component arms: these formats
//! entropy-code each component as an independent YONLY-style channel inside
//! one plane (decoder `mb_dc`/`mb_lp`/`mb_cbphp`/`mb_hp_flex`, the
//! `INT_YONLY | INT_YUVK | INT_NCOMPONENT` branches — per-component abs
//! flags, raw per-component CBPLP bits, `outer_iters = nc` CBPHP with the
//! YONLY tables), while sharing one set of plane-level adaptive state with
//! the usual two class buckets (component 0 = "luma", everything else =
//! "chroma" — `chroma_component`). YUVK additionally folds components 1–2
//! into the DC/HP prediction-strength sums exactly like YUV444;
//! NCOMPONENT keeps them luma-only (decoder lines `mb_dc`/`mb_hp_flex`).

use super::bitstream::BitWriter;
use super::entropy::write_huff;
use super::quant::QpSet;
use super::{codestream, coeff, hp, transform};
use crate::decode::consts::*;
use crate::decode::math::chroma_component;
use crate::decode::math::num_ones;
use crate::decode::state::{AdaptiveScan, AdaptiveVLC, CBPHPModel};
use crate::decode::tables;

const ABS_DELTA: [i32; 7] = [1, 0, -1, -1, -1, -1, -1]; // ABS_LEVEL_INDEX_DELTA[0]

// From the decoder's `mb_cbphp` (shared with hp.rs).
const I_OUT: [i32; 16] = [0, 15, 3, 12, 1, 2, 4, 8, 5, 6, 9, 10, 7, 11, 13, 14];
const I_OFF: [i32; 6] = [0, 4, 2, 8, 12, 1];
const I_FLC: [u32; 6] = [0, 2, 1, 2, 2, 0];

/// One `nc`-component image plane coded per-component: quantized
/// coefficients plus the shared adaptive entropy state, encodable one
/// macroblock at a time ([`super::gray::YOnlyPlane`] × N components with the
/// class-bucketed state of [`super::color::ColorPlane`]).
pub(super) struct MultiPlane {
    pub(super) mbw: usize,
    pub(super) mbh: usize,
    nc: usize,
    /// `INT_YUVK` or `INT_NCOMPONENT` (selects the prediction-strength arms).
    int_fmt: u8,
    pub(super) bands: u8,
    pub(super) trim: u32,
    tile_origin: (usize, usize),
    tile_w: usize,
    buf_grid: Vec<Vec<Vec<[i32; 256]>>>,
    dclp: Vec<Vec<Vec<[i32; 16]>>>,
    cbphp_grid: Vec<Vec<Vec<i32>>>,
    model_dc: coeff::ColorModel,
    abs_dc_lum: coeff::AdaptiveVlc1,
    abs_dc_chr: coeff::AdaptiveVlc1,
    model_lp: coeff::ColorModel,
    lp_first: [AdaptiveVLC; 2], // [lum, chr]
    lp_ind0: [AdaptiveVLC; 2],
    lp_ind1: [AdaptiveVLC; 2],
    lp_abs0: AdaptiveVLC,
    lp_abs1: AdaptiveVLC,
    scan: AdaptiveScan,
    model_hp: coeff::ColorModel,
    hp_first: [AdaptiveVLC; 2],
    hp_ind0: [AdaptiveVLC; 2],
    hp_ind1: [AdaptiveVLC; 2],
    hp_abs0: AdaptiveVLC,
    hp_abs1: AdaptiveVLC,
    hor_scan: AdaptiveScan,
    ver_scan: AdaptiveScan,
    num_cbphp: AdaptiveVLC,
    num_blk_cbphp: AdaptiveVLC,
    cbphp_model: CBPHPModel,
}

fn fresh_lp_state() -> (
    [AdaptiveVLC; 2],
    [AdaptiveVLC; 2],
    [AdaptiveVLC; 2],
    AdaptiveVLC,
    AdaptiveVLC,
) {
    let mut first = [AdaptiveVLC::default(), AdaptiveVLC::default()];
    let mut ind0 = [AdaptiveVLC::default(), AdaptiveVLC::default()];
    let mut ind1 = [AdaptiveVLC::default(), AdaptiveVLC::default()];
    let mut abs0 = AdaptiveVLC::default();
    let mut abs1 = AdaptiveVLC::default();
    for c in 0..2 {
        first[c].init_table2();
        ind0[c].init_table2();
        ind1[c].init_table2();
    }
    abs0.init_table1();
    abs1.init_table1();
    (first, ind0, ind1, abs0, abs1)
}

impl MultiPlane {
    /// Build from pre-bias component planes (already color-converted —
    /// CMYK arrives lifted into the YUVK domain, NCOMPONENT raw), padded to
    /// the window grid, forward-transformed and quantized per component.
    /// Component QP bytes: component 0 takes `qp`, every other component the
    /// `chroma` set (`COMP_SEPARATE` semantics; equal sets ⇒ `COMP_UNIFORM`
    /// on emission).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        comps: &[Vec<i32>],
        w: u32,
        h: u32,
        qp: QpSet,
        chroma_qp: QpSet,
        int_fmt: u8,
        bands: u8,
        scaled: bool,
        window: (u32, u32),
        overlap: u8,
        tile_cols_mb: &[usize],
        tile_rows_mb: &[usize],
    ) -> Self {
        debug_assert!(matches!(int_fmt, INT_YUVK | INT_NCOMPONENT));
        let nc = comps.len();
        let (wu, hu) = (w as usize, h as usize);
        let (top, left) = (window.0 as usize, window.1 as usize);
        let (pw, ph) = (
            (wu + left).next_multiple_of(16),
            (hu + top).next_multiple_of(16),
        );
        let (mbw, mbh) = (pw / 16, ph / 16);
        let left_mb = super::overlap::bounds(tile_cols_mb, mbw);
        let top_mb = super::overlap::bounds(tile_rows_mb, mbh);
        let tile_x: Vec<usize> = left_mb.iter().map(|&m| m * 16).collect();
        let tile_y: Vec<usize> = top_mb.iter().map(|&m| m * 16).collect();

        // Pad each component (edge replication), overlap pre-filter, stage-1
        // transform. Every component is full-resolution, so the DC pre-filter
        // is the luma geometry per component (the 4:4:4 treatment).
        let mut buf_grid = vec![vec![vec![[0i32; 256]; nc]; mbh]; mbw];
        for (comp, src) in comps.iter().enumerate() {
            let mut plane = vec![0i32; pw * ph];
            for y in 0..ph {
                let sy = y.saturating_sub(top).min(hu - 1);
                for x in 0..pw {
                    plane[y * pw + x] = src[sy * wu + x.saturating_sub(left).min(wu - 1)];
                }
            }
            if overlap != NO_OVERLAP_FILTERING {
                super::overlap::sample_pre_filter(&mut plane, pw, &tile_x, &tile_y);
            }
            for mbx in 0..mbw {
                for mby in 0..mbh {
                    let mut samples = [0i32; 256];
                    for py in 0..16 {
                        for px in 0..16 {
                            samples[py * 16 + px] = plane[(mby * 16 + py) * pw + (mbx * 16 + px)];
                        }
                    }
                    buf_grid[mbx][mby][comp] = transform::forward_stage1_mb(&samples);
                }
            }
            if overlap == FIRST_AND_SECOND_LEVEL_OVERLAP_FILTERING {
                struct CompDc<'a>(&'a mut Vec<Vec<Vec<[i32; 256]>>>, usize);
                impl super::overlap::DcGrid for CompDc<'_> {
                    fn dc(&self, mbx: usize, mby: usize, off: usize) -> i32 {
                        self.0[mbx][mby][self.1][off]
                    }
                    fn set_dc(&mut self, mbx: usize, mby: usize, off: usize, v: i32) {
                        self.0[mbx][mby][self.1][off] = v;
                    }
                }
                super::overlap::dc_pre_filter_luma(
                    &mut CompDc(&mut buf_grid, comp),
                    &left_mb,
                    &top_mb,
                );
            }
        }

        // Stage 2 + quantization + dclp extraction, per component with the
        // component-class scaling factors (the decoder's `quant_map` is
        // class-dependent in scaled mode; component 0 = luma class).
        let csf = super::quant::component_scaling_factor;
        let byte_of = |comp: usize, lum: u8, chr: u8| if comp == 0 { lum } else { chr };
        let mut dclp = vec![vec![vec![[0i32; 16]; nc]; mbh]; mbw];
        for mbx in 0..mbw {
            for mby in 0..mbh {
                for comp in 0..nc {
                    let dc_sf = csf(byte_of(comp, qp.dc, chroma_qp.dc), comp, scaled, DC);
                    let lp_sf = csf(byte_of(comp, qp.lp, chroma_qp.lp), comp, scaled, LP);
                    let hp_sf = csf(byte_of(comp, qp.hp, chroma_qp.hp), comp, scaled, HP);
                    let buf = &mut buf_grid[mbx][mby][comp];
                    // Scaled mode floor-halves the block-DCs of every
                    // component > 0 (the decoder doubles them back,
                    // decoder.rs first_level_inverse_transform — the generic
                    // full-res arm covers YUVK/NCOMPONENT, not just YUV
                    // chroma). Same half-step floor caveat as scaled color:
                    // scaled q1 is NOT bit-lossless for multi-component.
                    transform::forward_stage2_mb(buf, scaled && comp > 0);
                    if hp_sf > 1 {
                        for blk in 0..16 {
                            for pos in 1..16 {
                                let idx = blk * 16 + pos;
                                buf[idx] = super::quant::quantize(buf[idx], hp_sf);
                            }
                        }
                    }
                    let mut c = [0i32; 16];
                    for (j, s) in c.iter_mut().enumerate() {
                        *s = buf[16 * ICT4X4_INV_PERM[j]];
                    }
                    c[0] = super::quant::quantize(c[0], dc_sf);
                    for s in c.iter_mut().skip(1) {
                        *s = super::quant::quantize(*s, lp_sf);
                    }
                    dclp[mbx][mby][comp] = c;
                }
            }
        }

        let (lp_first, lp_ind0, lp_ind1, lp_abs0, lp_abs1) = fresh_lp_state();
        let (hp_first, hp_ind0, hp_ind1, hp_abs0, hp_abs1) = fresh_lp_state();
        let mut num_cbphp = AdaptiveVLC::default();
        let mut num_blk_cbphp = AdaptiveVLC::default();
        num_cbphp.init_table1();
        num_blk_cbphp.init_table1();
        let mut cbphp_model = CBPHPModel::default();
        cbphp_model.cbphp_state = [0, 0];
        cbphp_model.count_ones = [-4, -4];
        cbphp_model.count_zeroes = [4, 4];

        MultiPlane {
            mbw,
            mbh,
            nc,
            int_fmt,
            bands,
            trim: 0,
            tile_origin: (0, 0),
            tile_w: mbw,
            buf_grid,
            dclp,
            cbphp_grid: vec![vec![vec![0i32; nc]; mbh]; mbw],
            model_dc: coeff::ColorModel::init(0),
            abs_dc_lum: coeff::AdaptiveVlc1::default(),
            abs_dc_chr: coeff::AdaptiveVlc1::default(),
            model_lp: coeff::ColorModel::init(1),
            lp_first,
            lp_ind0,
            lp_ind1,
            lp_abs0,
            lp_abs1,
            scan: AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H),
            model_hp: coeff::ColorModel::init(2),
            hp_first,
            hp_ind0,
            hp_ind1,
            hp_abs0,
            hp_abs1,
            hor_scan: AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H_PRIME),
            ver_scan: AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_V_PRIME),
            num_cbphp,
            num_blk_cbphp,
            cbphp_model,
        }
    }

    pub(super) fn begin_tile(&mut self, first_mbx: usize, first_mby: usize, tile_w: usize) {
        self.tile_origin = (first_mbx, first_mby);
        self.tile_w = tile_w;
        self.model_dc = coeff::ColorModel::init(0);
        self.abs_dc_lum = coeff::AdaptiveVlc1::default();
        self.abs_dc_chr = coeff::AdaptiveVlc1::default();
        self.model_lp = coeff::ColorModel::init(1);
        let (f, i0, i1, a0, a1) = fresh_lp_state();
        (
            self.lp_first,
            self.lp_ind0,
            self.lp_ind1,
            self.lp_abs0,
            self.lp_abs1,
        ) = (f, i0, i1, a0, a1);
        self.scan = AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H);
        self.model_hp = coeff::ColorModel::init(2);
        let (f, i0, i1, a0, a1) = fresh_lp_state();
        (
            self.hp_first,
            self.hp_ind0,
            self.hp_ind1,
            self.hp_abs0,
            self.hp_abs1,
        ) = (f, i0, i1, a0, a1);
        self.hor_scan = AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H_PRIME);
        self.ver_scan = AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_V_PRIME);
        self.num_cbphp = AdaptiveVLC::default();
        self.num_blk_cbphp = AdaptiveVLC::default();
        self.num_cbphp.init_table1();
        self.num_blk_cbphp.init_table1();
        self.cbphp_model.cbphp_state = [0, 0];
        self.cbphp_model.count_ones = [-4, -4];
        self.cbphp_model.count_zeroes = [4, 4];
    }

    /// Emit one macroblock: per-component DC, per-component LP behind the
    /// raw CBPLP bits, all components' CBPHP, then per-component HP blocks +
    /// flexbits — the decoder's exact section and component order.
    pub(super) fn encode_mb(&mut self, sink: &mut codestream::Sink, mbx: usize, mby: usize) {
        let nc = self.nc;
        let (mbxt, mbyt) = (mbx - self.tile_origin.0, mby - self.tile_origin.1);
        let (is_left, is_top) = (mbxt == 0, mbyt == 0);
        let cadence = mbxt % 16 == 0 || mbxt == self.tile_w - 1;

        // ---------- DC ----------
        let bw = sink.dc();
        let dc_of = |s: &Self, mx: usize, my: usize, c: usize| s.dclp[mx][my][c][0];
        // Prediction-mode strength: component 0; YUVK adds components 1–2
        // with the 4:4:4 weighting (decoder `mb_dc`, Table 128 `_ => 2` arm);
        // NCOMPONENT stays luma-only.
        let dmode = if is_left && is_top {
            NO_PREDICTION
        } else if is_left {
            PREDICT_FROM_TOP
        } else if is_top {
            PREDICT_FROM_LEFT
        } else {
            let (mut sh, mut sv);
            let (l, t, tl) = (
                dc_of(self, mbx - 1, mby, 0),
                dc_of(self, mbx, mby - 1, 0),
                dc_of(self, mbx - 1, mby - 1, 0),
            );
            if self.int_fmt == INT_YUVK {
                sh = (tl - l).abs() * 2;
                sv = (tl - t).abs() * 2;
                for c in 1..3 {
                    sh += (dc_of(self, mbx - 1, mby - 1, c) - dc_of(self, mbx - 1, mby, c)).abs();
                    sv += (dc_of(self, mbx - 1, mby - 1, c) - dc_of(self, mbx, mby - 1, c)).abs();
                }
            } else {
                sh = (tl - l).abs();
                sv = (tl - t).abs();
            }
            if sh * 4 < sv {
                PREDICT_FROM_TOP
            } else if sv * 4 < sh {
                PREDICT_FROM_LEFT
            } else {
                PREDICT_FROM_TOP_LEFT
            }
        };
        let mut lap_dc = [0i32; 2];
        for comp in 0..nc {
            let pred = match dmode {
                PREDICT_FROM_TOP => dc_of(self, mbx, mby - 1, comp),
                PREDICT_FROM_LEFT => dc_of(self, mbx - 1, mby, comp),
                PREDICT_FROM_TOP_LEFT => {
                    (dc_of(self, mbx, mby - 1, comp) + dc_of(self, mbx - 1, mby, comp)) >> 1
                }
                _ => 0,
            };
            let cls = chroma_component(comp);
            // Per-component abs flag + body; the abs-level VLC is the LUMA
            // table for every component (decoder `decode_dc(…, false, …)`),
            // while model bits come from the component's class bucket.
            let (b_abs, abs_idx) = coeff::encode_dc_value(
                bw,
                self.dclp[mbx][mby][comp][0] - pred,
                self.model_dc.m_bits[cls],
                tables::abs_level_index(self.abs_dc_lum.table_index as usize),
            );
            if b_abs {
                self.abs_dc_lum.discrim += ABS_DELTA[abs_idx as usize];
                lap_dc[cls] += 1;
            }
        }
        self.model_dc.update(lap_dc, 0, nc);
        if cadence {
            self.abs_dc_lum.adapt();
            self.abs_dc_chr.adapt();
        }
        if self.bands == DCONLY {
            return;
        }

        // ---------- LP ----------
        let bw = sink.lp();
        if mbxt % 16 == 0 {
            self.scan.reset_totals();
        }
        let lp_mode = match dmode {
            PREDICT_FROM_LEFT => PREDICT_FROM_LEFT,
            PREDICT_FROM_TOP => PREDICT_FROM_TOP,
            _ => NO_PREDICTION,
        };
        // Residuals + coarse per component; the CBPLP bit per component is
        // raw (decoder's non-YUV arm: one bit per component, LSB-first).
        let mut res = vec![[0i32; 16]; nc];
        let mut coarse = vec![[0i32; 16]; nc];
        for comp in 0..nc {
            let mbits = self.model_lp.m_bits[chroma_component(comp)] as u32;
            for j in 1..16usize {
                let pred = if lp_mode == PREDICT_FROM_LEFT && matches!(j, 1 | 2 | 3) {
                    self.dclp[mbx - 1][mby][comp][j]
                } else if lp_mode == PREDICT_FROM_TOP && matches!(j, 4 | 8 | 12) {
                    self.dclp[mbx][mby - 1][comp][j]
                } else {
                    0
                };
                res[comp][j] = self.dclp[mbx][mby][comp][j] - pred;
                let m = res[comp][j].unsigned_abs() >> mbits;
                coarse[comp][j] = if res[comp][j] < 0 {
                    -(m as i32)
                } else {
                    m as i32
                };
            }
        }
        for comp in 0..nc {
            let bit = (1..16).any(|j| coarse[comp][j] != 0);
            bw.write_flag(bit);
        }
        let mut lap_lp = [0i32; 2];
        for comp in 0..nc {
            let cls = chroma_component(comp);
            let cbp = (1..16).any(|j| coarse[comp][j] != 0);
            if cbp {
                let mut pairs: Vec<(u32, i32)> = Vec::new();
                let mut run = 0u32;
                for i in 1..16usize {
                    let pos = self.scan.translate(i);
                    if coarse[comp][pos] != 0 {
                        pairs.push((run, coarse[comp][pos]));
                        run = 0;
                        self.scan.adapt(i);
                    } else {
                        run += 1;
                    }
                }
                lap_lp[cls] += pairs.len() as i32;
                coeff::encode_block(
                    bw,
                    &pairs,
                    1,
                    &mut self.lp_first[cls],
                    &mut self.lp_ind0[cls],
                    &mut self.lp_ind1[cls],
                    &mut self.lp_abs0,
                    &mut self.lp_abs1,
                );
            }
            let mbits = self.model_lp.m_bits[cls];
            if mbits > 0 {
                for j in 1..16 {
                    coeff::encode_refine_lp(bw, coarse[comp][j], res[comp][j], mbits);
                }
            }
        }
        self.model_lp.update(lap_lp, 1, nc);
        if cadence {
            for c in 0..2 {
                self.lp_first[c].adapt_table2(4);
                self.lp_ind0[c].adapt_table2(3);
                self.lp_ind1[c].adapt_table2(3);
            }
            self.lp_abs0.adapt_table1();
            self.lp_abs1.adapt_table1();
        }

        if self.bands == NOHIGHPASS {
            return;
        }

        // ---------- HP ----------
        if mbxt % 16 == 0 {
            self.hor_scan.reset_totals();
            self.ver_scan.reset_totals();
        }
        // HP prediction mode (shared by all components): component-0 LP
        // strengths; YUVK adds components 1–2 (decoder `mb_hp_flex`, the
        // non-(YONLY|NCOMPONENT) arm in our transposed storage).
        let (mut s_hor, mut s_ver) = (
            self.dclp[mbx][mby][0][1].abs()
                + self.dclp[mbx][mby][0][2].abs()
                + self.dclp[mbx][mby][0][3].abs(),
            self.dclp[mbx][mby][0][4].abs()
                + self.dclp[mbx][mby][0][8].abs()
                + self.dclp[mbx][mby][0][12].abs(),
        );
        if self.int_fmt == INT_YUVK {
            for c in 1..3 {
                s_hor += self.dclp[mbx][mby][c][1].abs();
                s_ver += self.dclp[mbx][mby][c][4].abs();
            }
        }
        let hp_mode = if s_hor * 4 < s_ver {
            PREDICT_FROM_TOP
        } else if s_ver * 4 < s_hor {
            PREDICT_FROM_LEFT
        } else {
            NO_PREDICTION
        };

        // Residuals + coarse + this MB's cbphp, per component (model bits
        // are the PRE-update HP bits, as in the decoder's read order).
        let mut hp_res = vec![[[0i32; 16]; 16]; nc];
        let mut hp_coarse = vec![[[0i32; 16]; 16]; nc];
        let mut mb_cbphp = vec![0i32; nc];
        for comp in 0..nc {
            let mbits = self.model_hp.m_bits[chroma_component(comp)] as u32;
            let buf = &self.buf_grid[mbx][mby][comp];
            for blk in 0..16 {
                for pos in 1..16 {
                    hp_res[comp][blk][pos] = buf[blk * 16 + pos];
                }
            }
            if hp_mode == PREDICT_FROM_TOP {
                for &blk in &[1usize, 2, 3, 5, 6, 7, 9, 10, 11, 13, 14, 15] {
                    for &k in &[2usize, 10, 9] {
                        hp_res[comp][blk][k] = buf[blk * 16 + k] - buf[(blk - 1) * 16 + k];
                    }
                }
            } else if hp_mode == PREDICT_FROM_LEFT {
                for blk in 4..16 {
                    for &k in &[1usize, 5, 6] {
                        hp_res[comp][blk][k] = buf[blk * 16 + k] - buf[(blk - 4) * 16 + k];
                    }
                }
            }
            for blk in 0..16 {
                for pos in 1..16 {
                    let r = hp_res[comp][blk][pos];
                    let c = (r.unsigned_abs() >> mbits) as i32;
                    hp_coarse[comp][blk][pos] = if r < 0 { -c } else { c };
                }
            }
            for k in 0..16 {
                let blk = I_HIER_SCAN_ORDER[k];
                if (1..16).any(|pos| hp_coarse[comp][blk][pos] != 0) {
                    mb_cbphp[comp] |= 1 << k;
                }
            }
        }

        // CBPHP for ALL components first (decoder `mb_cbphp`): per component,
        // un-predict with the class state — the model updates after each
        // component's prediction, exactly the decoder's two-loop order
        // (VLC reads are comp-ordered, model updates are comp-ordered; both
        // sequences interleave identically when done in one pass here).
        {
            let bw = sink.hp();
            for comp in 0..nc {
                let cls = chroma_component(comp);
                let state = self.cbphp_model.cbphp_state[cls];
                let i_diff = if state == 0 {
                    let neighbor = if is_left {
                        if is_top {
                            1
                        } else {
                            (self.cbphp_grid[mbx][mby - 1][comp] >> 10) & 1
                        }
                    } else {
                        (self.cbphp_grid[mbx - 1][mby][comp] >> 5) & 1
                    };
                    hp::unpredict_cascade(mb_cbphp[comp]) ^ neighbor
                } else if state == 2 {
                    mb_cbphp[comp] ^ 0xFFFF
                } else {
                    mb_cbphp[comp]
                };
                self.write_cbphp_diff(bw, i_diff);
                // UpdateCBPHPModel with this component's predicted popcount.
                let n_orig = num_ones(mb_cbphp[comp] as u32) as i32;
                let m = &mut self.cbphp_model;
                m.count_ones[cls] = (m.count_ones[cls] + n_orig - 3).clamp(-16, 15);
                m.count_zeroes[cls] = (m.count_zeroes[cls] + (16 - n_orig) - 3).clamp(-16, 15);
                m.cbphp_state[cls] = if m.count_ones[cls] < 0 {
                    if m.count_ones[cls] < m.count_zeroes[cls] {
                        1
                    } else {
                        2
                    }
                } else if m.count_zeroes[cls] < 0 {
                    2
                } else {
                    0
                };
                self.cbphp_grid[mbx][mby][comp] = mb_cbphp[comp];
            }
        }

        // HP blocks + flexbits per component (decoder `mb_hp_flex`).
        let emit_flex = self.bands == ALL_BANDS;
        let mut lap_hp = [0i32; 2];
        for comp in 0..nc {
            let cls = chroma_component(comp);
            let mbits = self.model_hp.m_bits[cls] as u32;
            let mut cbp = mb_cbphp[comp];
            for k in 0..16 {
                let blk = I_HIER_SCAN_ORDER[k];
                if cbp & 1 != 0 {
                    let pairs = {
                        let scan = if hp_mode == PREDICT_FROM_TOP {
                            &mut self.ver_scan
                        } else {
                            &mut self.hor_scan
                        };
                        let mut pairs: Vec<(u32, i32)> = Vec::new();
                        let mut run = 0u32;
                        for i in 1..16usize {
                            let pos = scan.translate(i);
                            if hp_coarse[comp][blk][pos] != 0 {
                                pairs.push((run, hp_coarse[comp][blk][pos]));
                                run = 0;
                                scan.adapt(i);
                            } else {
                                run += 1;
                            }
                        }
                        pairs
                    };
                    lap_hp[cls] += pairs.len() as i32;
                    coeff::encode_block(
                        sink.hp(),
                        &pairs,
                        1,
                        &mut self.hp_first[cls],
                        &mut self.hp_ind0[cls],
                        &mut self.hp_ind1[cls],
                        &mut self.hp_abs0,
                        &mut self.hp_abs1,
                    );
                }
                cbp >>= 1;
                if mbits > 0 && emit_flex {
                    for &n in &I_TRANSPOSE_FLEX[1..] {
                        coeff::encode_flexbits(
                            sink.flex(),
                            hp_coarse[comp][blk][n],
                            hp_res[comp][blk][n],
                            mbits as i32,
                            self.trim,
                        );
                    }
                }
            }
        }
        self.model_hp.update(lap_hp, 2, nc);
        if cadence {
            for c in 0..2 {
                self.hp_first[c].adapt_table2(4);
                self.hp_ind0[c].adapt_table2(3);
                self.hp_ind1[c].adapt_table2(3);
            }
            self.hp_abs0.adapt_table1();
            self.hp_abs1.adapt_table1();
            self.num_cbphp.adapt_table1();
            self.num_blk_cbphp.adapt_table1();
        }
    }

    /// Write one component's CBPHP difference pattern — the VLC half of
    /// [`hp::encode_cbphp`] against this plane's shared `num_cbphp`/
    /// `num_blk_cbphp` state (multi-component planes use the YONLY tables:
    /// `NUM_CBPHP` for both levels, `DELTA1`).
    fn write_cbphp_diff(&mut self, bw: &mut BitWriter, i_diff: i32) {
        let nibbles = [
            i_diff & 0xF,
            (i_diff >> 4) & 0xF,
            (i_diff >> 8) & 0xF,
            (i_diff >> 12) & 0xF,
        ];
        let i_cbphp = (0..4).fold(0i32, |m, b| m | (((nibbles[b] != 0) as i32) << b));
        let num = num_ones(i_cbphp as u32) as i32;
        write_huff(
            bw,
            tables::num_cbphp(self.num_cbphp.table_index as usize),
            num,
        );
        self.num_cbphp.discrim_val1 +=
            NUM_CBPHP_DELTA[self.num_cbphp.delta_table_index as usize][num as usize];
        match num {
            1 => bw.write_bits(i_cbphp.trailing_zeros() as u64, 2),
            2 => write_huff(bw, tables::ref_cbphp1(), i_cbphp),
            3 => bw.write_bits((0x0F ^ i_cbphp).trailing_zeros() as u64, 2),
            _ => {}
        }
        for &nib in nibbles.iter() {
            if nib == 0 {
                continue;
            }
            let i_code = I_OUT.iter().position(|&v| v == nib).unwrap() as i32;
            let i_val = (1..=5usize)
                .find(|&v| {
                    let lo = I_OFF[v];
                    i_code >= lo && i_code < lo + (1 << I_FLC[v])
                })
                .unwrap();
            let num_blk = (i_val - 1) as i32;
            write_huff(
                bw,
                tables::num_cbphp(self.num_blk_cbphp.table_index as usize),
                num_blk,
            );
            self.num_blk_cbphp.discrim_val1 += NUM_BLK_CBPHP_DELTA1
                [self.num_blk_cbphp.delta_table_index as usize][num_blk as usize];
            if I_FLC[i_val] != 0 {
                bw.write_bits((i_code - I_OFF[i_val]) as u64, I_FLC[i_val]);
            }
        }
    }
}

impl codestream::TileEncode for MultiPlane {
    fn begin_tile(&mut self, first_mbx: usize, first_mby: usize, tile_w: usize) {
        MultiPlane::begin_tile(self, first_mbx, first_mby, tile_w);
    }
    fn encode_mb_at(&mut self, sink: &mut codestream::Sink, mbx: usize, mby: usize) {
        self.encode_mb(sink, mbx, mby);
    }
}

/// Depth-general multi-component driver (`OUT_CMYK` / `OUT_CMYKDIRECT` /
/// `OUT_NCOMPONENT` over an `INT_YUVK` or `INT_NCOMPONENT` plane): `comps`
/// already forward-converted ([`super::convert`] — CMYK lifted into YUVK,
/// CMYKDIRECT/NCOMPONENT plain-biased), optional alpha image plane (CMYKA).
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_multi_prebias(
    comps: &[Vec<i32>],
    w: u32,
    h: u32,
    qp: QpSet,
    chroma_qp: QpSet,
    alpha: Option<(&[i32], QpSet)>,
    int_fmt: u8,
    out_clr_fmt: u8,
    bands: u8,
    scaled: bool,
    trim: u8,
    window: (u32, u32),
    tiles: (&[usize], &[usize]),
    overlap: u8,
    frequency: bool,
    depth: &super::convert::Depth,
    guid: &[u8; 16],
) -> Vec<u8> {
    let trim = if bands == ALL_BANDS { trim } else { 0 };
    let mut primary = MultiPlane::new(
        comps, w, h, qp, chroma_qp, int_fmt, bands, scaled, window, overlap, tiles.0, tiles.1,
    );
    primary.bands = bands;
    primary.trim = trim as u32;
    let nc = comps.len();
    let mut spec = codestream::ImageHeaderSpec::new(w, h, out_clr_fmt);
    spec.output_bitdepth = depth.bitdepth;
    spec.frequency_mode = frequency;
    spec.overlap_mode = overlap;
    spec.trim_flexbits = trim;
    spec.margins = super::color::window_margins(w, h, window);
    spec.tile_cols_mb = tiles.0.to_vec();
    spec.tile_rows_mb = tiles.1.to_vec();
    let (mbw, mbh) = (primary.mbw, primary.mbh);
    let body = match alpha {
        Some((a, alpha_qp)) => {
            spec.alpha_image_plane = true;
            let mut alpha_plane =
                super::gray::YOnlyPlane::new(a, w, h, alpha_qp, window, overlap, tiles, scaled);
            let mut pair = MultiAlphaPair {
                primary: &mut primary,
                alpha: &mut alpha_plane,
            };
            codestream::emit_codestream(
                &spec,
                |head| {
                    codestream::write_image_plane_header_multi(
                        head, int_fmt, nc, ALL_BANDS, qp, chroma_qp, scaled, depth,
                    );
                    codestream::write_image_plane_header_gray_bands(
                        head,
                        ALL_BANDS,
                        alpha_qp.dc,
                        alpha_qp.lp,
                        alpha_qp.hp,
                        scaled,
                        depth,
                    );
                },
                &codestream::classic_tile_headers(0),
                4,
                mbw,
                mbh,
                &mut pair,
            )
        }
        None => codestream::emit_codestream(
            &spec,
            |head| {
                codestream::write_image_plane_header_multi(
                    head, int_fmt, nc, bands, qp, chroma_qp, scaled, depth,
                )
            },
            &codestream::classic_tile_headers(trim),
            codestream::band_count(bands),
            mbw,
            mbh,
            &mut primary,
        ),
    };
    super::container::write_container(&body, w, h, guid)
}

/// A multi-component primary plane + YONLY alpha image plane, per-MB
/// interleaved (the CMYKA arrangement — mirrors [`super::color::AlphaPair`]).
pub(super) struct MultiAlphaPair<'a> {
    pub(super) primary: &'a mut MultiPlane,
    pub(super) alpha: &'a mut super::gray::YOnlyPlane,
}

impl codestream::TileEncode for MultiAlphaPair<'_> {
    fn begin_tile(&mut self, first_mbx: usize, first_mby: usize, tile_w: usize) {
        self.primary.begin_tile(first_mbx, first_mby, tile_w);
        self.alpha.begin_tile(first_mbx, first_mby, tile_w);
    }
    fn encode_mb_at(&mut self, sink: &mut codestream::Sink, mbx: usize, mby: usize) {
        self.primary.encode_mb(sink, mbx, mby);
        self.alpha.encode_mb(sink, mbx, mby);
    }
}
