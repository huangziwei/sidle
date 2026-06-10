//! Grayscale codestream-body encoder: DC + LP + HP (ALL_BANDS), spatial mode,
//! single tile, no overlap, uniform QP = 0 → **lossless for any grayscale
//! image**. Mirrors the decoder's `mb_dc` + `mb_lp` + `mb_cbphp` + `mb_hp_flex`
//! per-macroblock sequence, reusing its `AdaptiveVLC` / `AdaptiveScan` state.

use super::bitstream::BitWriter;
use super::quant::{quantize, QpSet};
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
    /// `bands_present` this plane emits (DC always; LP unless DCONLY; HP at
    /// ALL_BANDS/NOFLEXBITS; flexbits only at ALL_BANDS).
    pub(super) bands: u8,
    /// `trim_flexbits` (0–15).
    pub(super) trim: u32,
    /// First MB of the current tile — edge tests and the VLC-adapt cadence
    /// are tile-relative (the decoder's `mbxt`/`mbyt`). Single tile keeps the
    /// constructor's `(0, 0)`.
    tile_origin: (usize, usize),
    /// Current tile width in MBs (`reset_context` fires on the tile's last
    /// column). Single tile = `mbw`.
    tile_w: usize,
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
    /// crops back to the true dims, which the header carries), placing the
    /// image at `(top, left) = window` — `(0, 0)` is the classic
    /// `windowing_flag = 0` derived padding. Forward-transform every MB,
    /// quantize per band, and initialize the adaptive entropy state.
    pub(super) fn new(
        luma: &[u8],
        w: u32,
        h: u32,
        qp: QpSet,
        window: (u32, u32),
        overlap: u8,
        tiles: (&[usize], &[usize]),
    ) -> Self {
        let (wu, hu) = (w as usize, h as usize);
        let (top, left) = (window.0 as usize, window.1 as usize);
        let (pw, ph) = ((wu + left).next_multiple_of(16), (hu + top).next_multiple_of(16));
        let mut centered = vec![0i32; pw * ph];
        for y in 0..ph {
            let sy = y.saturating_sub(top).min(hu - 1);
            for x in 0..pw {
                centered[y * pw + x] =
                    luma[sy * wu + x.saturating_sub(left).min(wu - 1)] as i32 - 128;
            }
        }
        Self::from_centered_padded_ovl(
            &centered,
            pw,
            ph,
            qp,
            false,
            overlap,
            &super::overlap::bounds(tiles.0, pw / 16),
            &super::overlap::bounds(tiles.1, ph / 16),
        )
    }

    /// [`Self::new`] over an already-centered, already-padded `i32` plane
    /// (`pw × ph`, both 16-aligned) — the entry the YONLY-from-color path
    /// uses: the Y plane of `rgb_to_yuv444` is centered but can exceed the
    /// u8 range for saturated colors (the decoder clips on output) — with
    /// overlap filtering: the staged
    /// forward pipeline — sample-domain PRE-filter (overlap ≥ 1) → per-block
    /// stage-1 DCT → block-DC-domain PRE-filter (overlap == 2) → stage-2 DCT
    /// → quantization — the exact reverse of the decoder's
    /// `sample_reconstruction`. `left_mb`/`top_mb` are tile boundaries in MB
    /// units (`len == ntiles + 1`); overlap filters cross soft-tile edges.
    pub(super) fn from_centered_padded_ovl(
        plane: &[i32],
        pw: usize,
        ph: usize,
        qp: QpSet,
        scaled: bool,
        overlap: u8,
        left_mb: &[usize],
        top_mb: &[usize],
    ) -> Self {
        // The QP→scaling-factor map is MODE-dependent (the decoder's
        // `quant_map` scaled branch): a scaled plane must quantize with the
        // scaled factors or the decoder dequantizes with different ones.
        let (dc_sf, lp_sf, hp_sf) = super::quant::scaling_factors_for(qp, false, scaled);
        let (mbw, mbh) = (pw / 16, ph / 16);

        let filtered;
        let plane = if overlap != NO_OVERLAP_FILTERING {
            let mut p = plane.to_vec();
            let tile_x: Vec<usize> = left_mb.iter().map(|&m| m * 16).collect();
            let tile_y: Vec<usize> = top_mb.iter().map(|&m| m * 16).collect();
            super::overlap::sample_pre_filter(&mut p, pw, &tile_x, &tile_y);
            filtered = p;
            &filtered[..]
        } else {
            plane
        };

        // Stage 1: per-MB per-block forward DCT (raw block DCs at the slots).
        let mut buf_grid = vec![vec![[0i32; 256]; mbh]; mbw];
        for mbx in 0..mbw {
            for mby in 0..mbh {
                let mut samples = [0i32; 256];
                for py in 0..16 {
                    for px in 0..16 {
                        let g = (mby * 16 + py) * pw + (mbx * 16 + px);
                        samples[py * 16 + px] = plane[g];
                    }
                }
                buf_grid[mbx][mby] = transform::forward_stage1_mb(&samples);
            }
        }

        if overlap == FIRST_AND_SECOND_LEVEL_OVERLAP_FILTERING {
            struct GrayDc<'a>(&'a mut Vec<Vec<[i32; 256]>>);
            impl super::overlap::DcGrid for GrayDc<'_> {
                fn dc(&self, mbx: usize, mby: usize, off: usize) -> i32 {
                    self.0[mbx][mby][off]
                }
                fn set_dc(&mut self, mbx: usize, mby: usize, off: usize, v: i32) {
                    self.0[mbx][mby][off] = v;
                }
            }
            super::overlap::dc_pre_filter_luma(&mut GrayDc(&mut buf_grid), left_mb, top_mb);
        }

        // Stage 2 (block-DC DCT) + quantization + dclp extraction.
        let mut dclp = vec![vec![[0i32; 16]; mbh]; mbw];
        for mbx in 0..mbw {
            for mby in 0..mbh {
                let buf = &mut buf_grid[mbx][mby];
                transform::forward_stage2_mb(buf, false);
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
            bands: ALL_BANDS,
            trim: 0,
            tile_origin: (0, 0),
            tile_w: mbw,
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

    /// Start a tile at `(first_mbx, first_mby)`, `tile_w` MBs wide: fresh
    /// entropy state, exactly the constructor's init — the encoder mirror of
    /// the decoder's `initialize_context` (which re-inits every band's
    /// models/VLC tables/scans at each tile's first MB).
    pub(super) fn begin_tile(&mut self, first_mbx: usize, first_mby: usize, tile_w: usize) {
        self.tile_origin = (first_mbx, first_mby);
        self.tile_w = tile_w;
        self.model_dc = coeff::ModelState::init(0);
        self.abs_dc = coeff::AdaptiveVlc1::default();
        self.model_lp = coeff::ModelState::init(1);
        self.first_ind = AdaptiveVLC::default();
        self.lp_ind0 = AdaptiveVLC::default();
        self.lp_ind1 = AdaptiveVLC::default();
        self.lp_abs0 = AdaptiveVLC::default();
        self.lp_abs1 = AdaptiveVLC::default();
        self.first_ind.init_table2();
        self.lp_ind0.init_table2();
        self.lp_ind1.init_table2();
        self.lp_abs0.init_table1();
        self.lp_abs1.init_table1();
        self.scan = AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H);
        self.hp_state = hp::HpState::new();
    }

    /// Emit one macroblock's DC + LP + HP(+flex) bits, updating this plane's
    /// adaptive state exactly as the decoder's per-MB readers do. Band
    /// sections route through the [`codestream::Sink`] (one writer in
    /// spatial order; per-band writers in frequency order).
    pub(super) fn encode_mb(&mut self, sink: &mut codestream::Sink, mbx: usize, mby: usize) {
        let c = self.dclp[mbx][mby];
        // Tile-relative position: edge tests, the 16-MB adapt cadence, and the
        // last-column reset are all within-tile (decoder `MB::new` flags).
        let (mbxt, mbyt) = (mbx - self.tile_origin.0, mby - self.tile_origin.1);
        let (is_left, is_top) = (mbxt == 0, mbyt == 0);

        // ---------- DC ----------
        let bw = sink.dc();
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
        if mbxt % 16 == 0 || mbxt == self.tile_w - 1 {
            self.abs_dc.adapt();
        }
        if self.bands == DCONLY {
            return;
        }

        // ---------- LP ----------
        let bw = sink.lp();
        if mbxt % 16 == 0 {
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
        if mbxt % 16 == 0 || mbxt == self.tile_w - 1 {
            self.first_ind.adapt_table2(4);
            self.lp_ind0.adapt_table2(3);
            self.lp_ind1.adapt_table2(3);
            self.lp_abs0.adapt_table1();
            self.lp_abs1.adapt_table1();
        }

        if self.bands == NOHIGHPASS {
            return;
        }
        // ---------- HP ----------
        if mbxt % 16 == 0 {
            self.hp_state.hor_scan.reset_totals();
            self.hp_state.ver_scan.reset_totals();
        }
        let cbphp_left = if is_left { 0 } else { self.cbphp_grid[mbx - 1][mby] };
        let cbphp_top = if is_top { 0 } else { self.cbphp_grid[mbx][mby - 1] };
        let mbcbp = hp::encode_hp_mb(
            sink,
            &mut self.hp_state,
            &self.buf_grid[mbx][mby],
            &c,
            cbphp_left,
            cbphp_top,
            is_left,
            is_top,
            self.bands == ALL_BANDS,
            self.trim,
        );
        self.cbphp_grid[mbx][mby] = mbcbp;
        if mbxt % 16 == 0 || mbxt == self.tile_w - 1 {
            self.hp_state.adapt();
        }
    }
}

impl codestream::TileEncode for YOnlyPlane {
    fn begin_tile(&mut self, first_mbx: usize, first_mby: usize, tile_w: usize) {
        YOnlyPlane::begin_tile(self, first_mbx, first_mby, tile_w);
    }
    fn encode_mb_at(&mut self, sink: &mut codestream::Sink, mbx: usize, mby: usize) {
        self.encode_mb(sink, mbx, mby);
    }
}

/// Encode a grayscale image (any size) as ALL_BANDS at the given per-band
/// quantizers. `QpSet::LOSSLESS` (all 0 ⇒ scaling factor 1) is bit-exact.
pub fn encode_grayscale(luma: &[u8], w: u32, h: u32, qp: QpSet) -> Vec<u8> {
    encode_grayscale_scaled(luma, w, h, qp, false)
}

/// [`encode_grayscale`] with **scaled arithmetic** exposed: the luma samples
/// carry 3 extra fraction bits (`<<3`; no chroma, so no half-step) and the
/// plane header sets `scaled_flag`. Exactly invertible at q1 — the decoder's
/// `(v + bias<<3 + 3) >> 3` output stage recovers every pixel — but kept
/// non-default to match libjxr (scaled is its lossy mode).
pub fn encode_grayscale_scaled(luma: &[u8], w: u32, h: u32, qp: QpSet, scaled: bool) -> Vec<u8> {
    encode_grayscale_options(luma, w, h, qp, scaled, ALL_BANDS, 0, (0, 0), (&[], &[]), 0, false)
}

/// [`encode_grayscale_scaled`] over the band-truncation envelope: any
/// `bands_present` (the plane header and per-MB sections shrink together),
/// `trim_flexbits` (image-header flag + the 4-bit tile value; the flex
/// emission drops the low `trim` bits), explicit window margins
/// (`window = (top, left)`; `(0, 0)` = classic derived windowing), and a
/// tile grid (`tiles = (column widths, row heights)` in MB units covering
/// the padded grid; empty = single tile).
#[allow(clippy::too_many_arguments)]
pub fn encode_grayscale_options(
    luma: &[u8],
    w: u32,
    h: u32,
    qp: QpSet,
    scaled: bool,
    bands: u8,
    trim: u8,
    window: (u32, u32),
    tiles: (&[usize], &[usize]),
    overlap: u8,
    frequency: bool,
) -> Vec<u8> {
    let (wu, hu) = (w as usize, h as usize);
    let (top, left) = (window.0 as usize, window.1 as usize);
    let (pw, ph) = ((wu + left).next_multiple_of(16), (hu + top).next_multiple_of(16));
    let sh = if scaled { 3 } else { 0 };
    let mut centered = vec![0i32; pw * ph];
    for y in 0..ph {
        let sy = y.saturating_sub(top).min(hu - 1);
        for x in 0..pw {
            centered[y * pw + x] =
                (luma[sy * wu + x.saturating_sub(left).min(wu - 1)] as i32 - 128) << sh;
        }
    }
    let mut plane = YOnlyPlane::from_centered_padded_ovl(
        &centered,
        pw,
        ph,
        qp,
        scaled,
        overlap,
        &super::overlap::bounds(tiles.0, pw / 16),
        &super::overlap::bounds(tiles.1, ph / 16),
    );
    plane.bands = bands;
    plane.trim = if bands == ALL_BANDS { trim as u32 } else { 0 };
    let mut spec = codestream::ImageHeaderSpec::new(w, h, OUT_YONLY);
    spec.frequency_mode = frequency;
    spec.overlap_mode = overlap;
    spec.trim_flexbits = if bands == ALL_BANDS { trim } else { 0 };
    spec.margins = super::color::window_margins(w, h, window);
    spec.tile_cols_mb = tiles.0.to_vec();
    spec.tile_rows_mb = tiles.1.to_vec();
    let trim_v = spec.trim_flexbits;
    let (mbw, mbh) = (plane.mbw, plane.mbh);
    let body = codestream::emit_codestream(
        &spec,
        |head| {
            codestream::write_image_plane_header_gray_bands(head, bands, qp.dc, qp.lp, qp.hp, scaled)
        },
        trim_v,
        codestream::band_count(bands),
        mbw,
        mbh,
        &mut plane,
    );
    container::write_container(&body, w, h, &container::pixel_format::GRAY8)
}
