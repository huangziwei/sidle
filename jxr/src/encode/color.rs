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
//! The emission path is 4:4:4 (full-resolution chroma). 4:2:0/4:2:2 (4b, in
//! progress) downsample the centered U/V planes AFTER the color transform —
//! the libjxr pipeline order — via [`downsample_h`]/[`downsample_v`].

use super::bitstream::BitWriter;
use super::entropy::write_huff;
use super::quant::{quantize, QpSet};
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

/// Pad one u8 plane to a 16-aligned grid with edge replication (as `gray.rs`),
/// the image content placed at `(top, left)` — all four margins replicate the
/// nearest edge (corners replicate the corner sample). `(0, 0)` is the classic
/// derived-windowing placement.
fn pad_plane(
    src: &[i32],
    wu: usize,
    hu: usize,
    pw: usize,
    ph: usize,
    top: usize,
    left: usize,
) -> Vec<i32> {
    let mut p = vec![0i32; pw * ph];
    for y in 0..ph {
        let sy = y.saturating_sub(top).min(hu - 1);
        for x in 0..pw {
            p[y * pw + x] = src[sy * wu + x.saturating_sub(left).min(wu - 1)];
        }
    }
    p
}

/// libjxr's `DF_ODD` 5-tap chroma decimation filter: `[1,4,6,4,1]/16` with
/// round-half-up, centered on the EVEN sample (`d2`) — so the surviving chroma
/// samples are co-sited with even luma positions, matching the
/// `chroma_centering_x/y = 0` the plane header declares (the only values
/// libjxr ever writes, `strenc.c:1299`).
#[inline]
fn df_odd(d0: i32, d1: i32, d2: i32, d3: i32, d4: i32) -> i32 {
    (((d1 + d2 + d3) << 2) + (d2 << 1) + d0 + d4 + 8) >> 4
}

/// Reflect an out-of-range index back into `0..=m` (only ever ±2 over, the
/// filter's reach). Mirrors `downsampleUV`'s boundary handling: on the left,
/// `src[-k] → src[k]`; on the right (padded extent), `src[m+k] → src[m-k]`.
#[inline]
fn reflect(i: isize, m: isize) -> usize {
    (if i < 0 {
        -i
    } else if i > m {
        2 * m - i
    } else {
        i
    }) as usize
}

/// Horizontal 2:1 chroma decimation of a `w×h` centered plane → `w/2 × h`
/// (444→422). Whole-plane port of libjxr `downsampleUV`'s horizontal pass
/// (`strenc.c:1603`); `w` is the padded (16-aligned, hence even) width.
pub fn downsample_h(src: &[i32], w: usize, h: usize) -> Vec<i32> {
    let ow = w / 2;
    let m = (w - 1) as isize;
    let mut out = vec![0i32; ow * h];
    for y in 0..h {
        let row = &src[y * w..y * w + w];
        for (x, o) in out[y * ow..y * ow + ow].iter_mut().enumerate() {
            let c = 2 * x as isize;
            *o = df_odd(
                row[reflect(c - 2, m)],
                row[reflect(c - 1, m)],
                row[c as usize],
                row[reflect(c + 1, m)],
                row[reflect(c + 2, m)],
            );
        }
    }
    out
}

/// Vertical 2:1 chroma decimation of a `w×h` centered plane → `w × h/2`
/// (422→420). Whole-plane port of `downsampleUV`'s vertical pass; `h` is the
/// padded (even) height.
pub fn downsample_v(src: &[i32], w: usize, h: usize) -> Vec<i32> {
    let oh = h / 2;
    let m = (h - 1) as isize;
    let mut out = vec![0i32; w * oh];
    for y in 0..oh {
        let c = 2 * y as isize;
        let r0 = reflect(c - 2, m) * w;
        let r1 = reflect(c - 1, m) * w;
        let r2 = c as usize * w;
        let r3 = reflect(c + 1, m) * w;
        let r4 = reflect(c + 2, m) * w;
        for x in 0..w {
            out[y * w + x] = df_odd(src[r0 + x], src[r1 + x], src[r2 + x], src[r3 + x], src[r4 + x]);
        }
    }
    out
}

/// One 3-component YUV image plane (`INT_YUV444`, or subsampled
/// `INT_YUV422`/`INT_YUV420`): per-component quantized coefficients plus the
/// YUV adaptive entropy state, encodable one macroblock at a time. Mirror of
/// [`super::gray::YOnlyPlane`] for the color path; a color+alpha image (4a)
/// is a `ColorPlane` and a `YOnlyPlane` per-MB interleaved on one
/// `BitWriter`. For subsampled chroma the per-component buffers use a PREFIX
/// of the 444-sized arrays (chroma MB = 4 blocks / 4 dclp slots for 420,
/// 8/8 for 422 — the decoder's fixed-stride `MB_BUF_PER_COMP` layout).
pub(super) struct ColorPlane {
    pub(super) mbw: usize,
    pub(super) mbh: usize,
    /// `INT_YUV444` / `INT_YUV422` / `INT_YUV420`.
    fmt: u8,
    /// `bands_present` this plane emits (DC always; LP unless DCONLY; HP+flex
    /// only at ALL_BANDS — NOFLEXBITS emission is 4c work).
    bands: u8,
    /// Chroma blocks per MB: 16 (444) / 8 (422) / 4 (420).
    nblk_ch: usize,
    /// `trim_flexbits` (0–15): low flexbits dropped on emission.
    trim: u32,
    /// First MB of the current tile — edge tests and the VLC-adapt cadence
    /// are tile-relative (the decoder's `mbxt`/`mbyt`). Single tile keeps the
    /// constructor's `(0, 0)`.
    tile_origin: (usize, usize),
    /// Current tile width in MBs (`reset_context` fires on the tile's last
    /// column). Single tile = `mbw`.
    tile_w: usize,
    /// LP/HP QP sets per tile (DQUANT when > 1): the per-MB set index is
    /// emitted before each MB's band payload and gates LP prediction.
    num_lp_qps: usize,
    num_hp_qps: usize,
    /// Per-MB LP/HP set indices, row-major `mby * mbw + mbx`; empty = all 0.
    lp_idx_map: Vec<u8>,
    hp_idx_map: Vec<u8>,
    /// Chroma dclp slots per MB (1 DC + LP): 16 / 8 / 4.
    jmax_ch: usize,
    buf_grid: Vec<Vec<[[i32; 256]; 3]>>,
    dclp: Vec<Vec<[[i32; 16]; 3]>>,
    cbphp_grid: Vec<Vec<[i32; 3]>>,
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
    count_zero_cbplp: i32,
    count_max_cbplp: i32,
    hp_state: ColorHpState,
}

impl ColorPlane {
    /// Pad RGB to the 16-aligned MB grid, forward-color-transform to centered
    /// YUV 4:4:4, forward-transform + quantize every MB per component, and
    /// initialize the YUV adaptive entropy state.
    pub(super) fn new(r: &[i32], g: &[i32], b: &[i32], w: u32, h: u32, qp: QpSet) -> Self {
        Self::new_fmt(
            r, g, b, w, h, qp, INT_YUV444, ALL_BANDS, false, (0, 0), NO_OVERLAP_FILTERING, &[], &[],
            None,
        )
    }

    /// [`Self::new`] generalized over chroma sampling and `bands_present`:
    /// for `INT_YUV422`/`INT_YUV420` the centered U/V planes are decimated
    /// AFTER the color transform (libjxr pipeline order) and chroma MBs run
    /// through the 4-/8-block forward transforms with the transposed-domain
    /// dclp extraction (T420/T422 — the decoder's write-back tables, read
    /// backwards). `window = (top, left)` places the image inside the padded
    /// grid for explicit windowing (`(0, 0)` = classic derived padding).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_fmt(
        r: &[i32],
        g: &[i32],
        b: &[i32],
        w: u32,
        h: u32,
        qp: QpSet,
        fmt: u8,
        bands: u8,
        scaled: bool,
        window: (u32, u32),
        overlap: u8,
        tile_cols_mb: &[usize],
        tile_rows_mb: &[usize],
        plan: Option<&super::quant::QpPlan>,
    ) -> Self {
        // The quantization plan: per-tile QP sets + per-MB LP/HP set indices.
        // `None` = the classic single uniform set from `qp`. Factors come
        // from the decoder's `quant_map` per (component, scaled, band) — the
        // MODE- and COMPONENT-dependent map.
        let owned_plan;
        let plan = match plan {
            Some(p) => p,
            None => {
                owned_plan = super::quant::QpPlan::uniform(qp, None);
                &owned_plan
            }
        };
        struct TileFactors {
            dc: [i32; 3],
            lp: Vec<[i32; 3]>,
            hp: Vec<[i32; 3]>,
        }
        let csf = super::quant::component_scaling_factor;
        let tile_factors: Vec<TileFactors> = plan
            .tiles
            .iter()
            .map(|t| TileFactors {
                dc: std::array::from_fn(|c| csf(t.dc.0[c], c, scaled, DC)),
                lp: t.lp.iter().map(|q| std::array::from_fn(|c| csf(q.0[c], c, scaled, LP))).collect(),
                hp: t.hp.iter().map(|q| std::array::from_fn(|c| csf(q.0[c], c, scaled, HP))).collect(),
            })
            .collect();
        let (num_lp_qps, num_hp_qps) = (plan.num_lp_qps(), plan.num_hp_qps());
        debug_assert!(
            plan.tiles.iter().all(|t| t.lp.len() == num_lp_qps && t.hp.len() == num_hp_qps),
            "every tile must declare the same LP/HP set counts"
        );
        debug_assert!(num_lp_qps >= 1 && num_lp_qps <= 16 && num_hp_qps >= 1 && num_hp_qps <= 16);
        let (wu, hu) = (w as usize, h as usize);
        let (top, left) = (window.0 as usize, window.1 as usize);
        let (pw, ph) = ((wu + left).next_multiple_of(16), (hu + top).next_multiple_of(16));
        let (mbw, mbh) = (pw / 16, ph / 16);

        // Pad the pre-bias RGB ([`super::convert`] already centered/shifted
        // it — for scaled planes the 3 extra fraction bits enter BEFORE the
        // color lifting, mirroring the decoder's unscale-after-inverse-
        // lifting order), then forward color transform per pixel → 3
        // centered YUV planes.
        let (rp, gp, bp) = (
            pad_plane(r, wu, hu, pw, ph, top, left),
            pad_plane(g, wu, hu, pw, ph, top, left),
            pad_plane(b, wu, hu, pw, ph, top, left),
        );
        let mut yuv = [vec![0i32; pw * ph], vec![0i32; pw * ph], vec![0i32; pw * ph]];
        for i in 0..pw * ph {
            let (y, u, v) = rgb_to_yuv444(rp[i], gp[i], bp[i]);
            yuv[0][i] = y;
            yuv[1][i] = u;
            yuv[2][i] = v;
        }

        // Decimate chroma per format (444 → 422 horizontally, → 420 vertically
        // on top). Chroma plane dims: (pw/2, ph) for 422, (pw/2, ph/2) for 420.
        let cw = if fmt == INT_YUV444 { pw } else { pw / 2 };
        if fmt != INT_YUV444 {
            for plane in yuv.iter_mut().skip(1) {
                let mut p = downsample_h(plane, pw, ph);
                if fmt == INT_YUV420 {
                    p = downsample_v(&p, pw / 2, ph);
                }
                *plane = p;
            }
        }
        let (nblk_ch, jmax_ch) = match fmt {
            INT_YUV420 => (4usize, 4usize),
            INT_YUV422 => (8, 8),
            _ => (16, 16),
        };

        // Tile boundaries in MB units (single tile when the lists are empty);
        // the overlap pre-filters cross soft-tile edges per these bounds.
        let mut left_mb = vec![0usize];
        if tile_cols_mb.is_empty() {
            left_mb.push(mbw);
        } else {
            for &wmb in tile_cols_mb {
                left_mb.push(left_mb.last().unwrap() + wmb);
            }
        }
        let mut top_mb = vec![0usize];
        if tile_rows_mb.is_empty() {
            top_mb.push(mbh);
        } else {
            for &hmb in tile_rows_mb {
                top_mb.push(top_mb.last().unwrap() + hmb);
            }
        }

        // (overlap ≥ 1) sample-domain PRE-filter per component on its padded
        // (and for 42x decimated) plane — tile bounds in component pixels.
        if overlap != NO_OVERLAP_FILTERING {
            for (comp, plane) in yuv.iter_mut().enumerate() {
                let chroma_42x = comp > 0 && fmt != INT_YUV444;
                let dx = if chroma_42x { 2 } else { 1 };
                let dy = if chroma_42x && fmt == INT_YUV420 { 2 } else { 1 };
                let tile_x: Vec<usize> = left_mb.iter().map(|&m| m * 16 / dx).collect();
                let tile_y: Vec<usize> = top_mb.iter().map(|&m| m * 16 / dy).collect();
                let stride = if chroma_42x { cw } else { pw };
                super::overlap::sample_pre_filter(plane, stride, &tile_x, &tile_y);
            }
        }

        // Per-MB, per-component quantized DC+LP levels (`dclp`) and the full forward
        // buffers (`buf_grid`, HP quantized) for the HP band — staged so the
        // block-DC-domain overlap pre-filter can run between the DCT stages.
        const T420: [usize; 4] = [0, 2, 1, 3];
        const T422: [usize; 8] = [0, 2, 1, 3, 4, 6, 5, 7];
        let mut dclp = vec![vec![[[0i32; 16]; 3]; mbh]; mbw];
        let mut buf_grid = vec![vec![[[0i32; 256]; 3]; mbh]; mbw];

        // Stage 1: per-block forward DCT (raw block DCs at the block slots).
        for mbx in 0..mbw {
            for mby in 0..mbh {
                for (comp, plane) in yuv.iter().enumerate() {
                    let chroma_42x = comp > 0 && fmt != INT_YUV444;
                    if !chroma_42x {
                        let mut samples = [0i32; 256];
                        for py in 0..16 {
                            for px in 0..16 {
                                samples[py * 16 + px] =
                                    plane[(mby * 16 + py) * pw + (mbx * 16 + px)];
                            }
                        }
                        buf_grid[mbx][mby][comp] = transform::forward_stage1_mb(&samples);
                    } else if fmt == INT_YUV420 {
                        // Chroma MB footprint 8×8 at (mbx*8, mby*8) in the
                        // decimated plane.
                        let mut samples = [0i32; 64];
                        for py in 0..8 {
                            for px in 0..8 {
                                samples[py * 8 + px] =
                                    plane[(mby * 8 + py) * cw + (mbx * 8 + px)];
                            }
                        }
                        buf_grid[mbx][mby][comp][..64]
                            .copy_from_slice(&transform::forward_stage1_chroma_420(&samples));
                    } else {
                        // 422: chroma MB footprint 8×16 at (mbx*8, mby*16).
                        let mut samples = [0i32; 128];
                        for py in 0..16 {
                            for px in 0..8 {
                                samples[py * 8 + px] =
                                    plane[(mby * 16 + py) * cw + (mbx * 8 + px)];
                            }
                        }
                        buf_grid[mbx][mby][comp][..128]
                            .copy_from_slice(&transform::forward_stage1_chroma_422(&samples));
                    }
                }
            }
        }

        // (overlap == 2) block-DC-domain PRE-filter, between the stages.
        if overlap == FIRST_AND_SECOND_LEVEL_OVERLAP_FILTERING {
            struct CompDc<'a> {
                grid: &'a mut Vec<Vec<[[i32; 256]; 3]>>,
                comp: usize,
            }
            impl super::overlap::DcGrid for CompDc<'_> {
                fn dc(&self, mbx: usize, mby: usize, off: usize) -> i32 {
                    self.grid[mbx][mby][self.comp][off]
                }
                fn set_dc(&mut self, mbx: usize, mby: usize, off: usize, v: i32) {
                    self.grid[mbx][mby][self.comp][off] = v;
                }
            }
            for comp in 0..3 {
                let chroma_42x = comp > 0 && fmt != INT_YUV444;
                let mut g = CompDc { grid: &mut buf_grid, comp };
                if chroma_42x {
                    super::overlap::dc_pre_filter_chroma(
                        &mut g,
                        fmt == INT_YUV420,
                        &left_mb,
                        &top_mb,
                    );
                } else {
                    super::overlap::dc_pre_filter_luma(&mut g, &left_mb, &top_mb);
                }
            }
        }

        // Stage 2 (across-block transform) + quantization + dclp extraction.
        // Per-MB factors: the MB's tile entry (row-major) + its LP/HP set
        // indices from the plan maps.
        let ncols = left_mb.len() - 1;
        let tile_of = |mbx: usize, mby: usize| -> usize {
            if plan.tiles.len() == 1 {
                return 0;
            }
            let tx = left_mb[1..].iter().position(|&b| mbx < b).unwrap_or(ncols - 1);
            let ty =
                top_mb[1..].iter().position(|&b| mby < b).unwrap_or(top_mb.len() - 2);
            ty * ncols + tx
        };
        debug_assert!(
            plan.lp_index.is_empty() || plan.lp_index.len() == mbw * mbh,
            "LP index map must cover the MB grid"
        );
        debug_assert!(plan.hp_index.is_empty() || plan.hp_index.len() == mbw * mbh);
        for mbx in 0..mbw {
            for mby in 0..mbh {
                let tf = &tile_factors[tile_of(mbx, mby)];
                let li = plan.lp_index.get(mby * mbw + mbx).copied().unwrap_or(0) as usize;
                let hi = plan.hp_index.get(mby * mbw + mbx).copied().unwrap_or(0) as usize;
                for comp in 0..3 {
                    let chroma_42x = comp > 0 && fmt != INT_YUV444;
                    let (dc_sf, lp_sf, hp_sf) = (tf.dc[comp], tf.lp[li][comp], tf.hp[hi][comp]);
                    if !chroma_42x {
                        // Scaled chroma (444) floor-halves the block-DCs
                        // between the transform stages; luma never does.
                        let buf = &mut buf_grid[mbx][mby][comp];
                        transform::forward_stage2_mb(buf, scaled && comp > 0);
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
                    } else if fmt == INT_YUV420 {
                        let mut buf = [0i32; 64];
                        buf.copy_from_slice(&buf_grid[mbx][mby][comp][..64]);
                        transform::forward_stage2_chroma_420(&mut buf, scaled);
                        if hp_sf > 1 {
                            for blk in 0..4 {
                                for pos in 1..16 {
                                    let i = blk * 16 + pos;
                                    buf[i] = quantize(buf[i], hp_sf);
                                }
                            }
                        }
                        let c = &mut dclp[mbx][mby][comp];
                        for j in 0..4 {
                            c[j] = buf[16 * T420[j]];
                        }
                        c[0] = quantize(c[0], dc_sf);
                        for s in c.iter_mut().take(4).skip(1) {
                            *s = quantize(*s, lp_sf);
                        }
                        buf_grid[mbx][mby][comp][..64].copy_from_slice(&buf);
                    } else {
                        let mut buf = [0i32; 128];
                        buf.copy_from_slice(&buf_grid[mbx][mby][comp][..128]);
                        transform::forward_stage2_chroma_422(&mut buf, scaled);
                        if hp_sf > 1 {
                            for blk in 0..8 {
                                for pos in 1..16 {
                                    let i = blk * 16 + pos;
                                    buf[i] = quantize(buf[i], hp_sf);
                                }
                            }
                        }
                        let c = &mut dclp[mbx][mby][comp];
                        for j in 0..8 {
                            c[j] = buf[16 * T422[j]];
                        }
                        c[0] = quantize(c[0], dc_sf);
                        for s in c.iter_mut().take(8).skip(1) {
                            *s = quantize(*s, lp_sf);
                        }
                        buf_grid[mbx][mby][comp][..128].copy_from_slice(&buf);
                    }
                }
            }
        }

        // LP band state: luma + chroma index tables, shared abs tables + scan,
        // and the cbplp count-escape counters (DC/HP state in the literal below).
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

        ColorPlane {
            mbw,
            mbh,
            fmt,
            bands,
            trim: 0,
            tile_origin: (0, 0),
            tile_w: mbw,
            num_lp_qps,
            num_hp_qps,
            lp_idx_map: plan.lp_index.clone(),
            hp_idx_map: plan.hp_index.clone(),
            nblk_ch,
            jmax_ch,
            buf_grid,
            dclp,
            cbphp_grid: vec![vec![[0i32; 3]; mbh]; mbw],
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
            count_zero_cbplp: 1,
            count_max_cbplp: 1,
            hp_state: ColorHpState::new(),
        }
    }

    /// Start a tile at `(first_mbx, first_mby)`, `tile_w` MBs wide: fresh
    /// entropy state, exactly the constructor's init — the encoder mirror of
    /// the decoder's `initialize_context` (which re-inits every band's
    /// models/VLC tables/scans/CBP counters at each tile's first MB).
    pub(super) fn begin_tile(&mut self, first_mbx: usize, first_mby: usize, tile_w: usize) {
        self.tile_origin = (first_mbx, first_mby);
        self.tile_w = tile_w;
        self.model_dc = coeff::ColorModel::init(0);
        self.abs_dc_lum = coeff::AdaptiveVlc1::default();
        self.abs_dc_chr = coeff::AdaptiveVlc1::default();
        self.model_lp = coeff::ColorModel::init(1);
        self.lp_first = [AdaptiveVLC::default(), AdaptiveVLC::default()];
        self.lp_ind0 = [AdaptiveVLC::default(), AdaptiveVLC::default()];
        self.lp_ind1 = [AdaptiveVLC::default(), AdaptiveVLC::default()];
        self.lp_abs0 = AdaptiveVLC::default();
        self.lp_abs1 = AdaptiveVLC::default();
        for t in self
            .lp_first
            .iter_mut()
            .chain(self.lp_ind0.iter_mut())
            .chain(self.lp_ind1.iter_mut())
        {
            t.init_table2();
        }
        self.lp_abs0.init_table1();
        self.lp_abs1.init_table1();
        self.scan = AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H);
        self.count_zero_cbplp = 1;
        self.count_max_cbplp = 1;
        self.hp_state = ColorHpState::new();
    }

    /// Emit one macroblock's DC + LP + HP(+flex) bits for all 3 components,
    /// updating this plane's adaptive state exactly as the decoder's per-MB
    /// YUV444 readers do.
    /// This MB's LP QP-set index (0 when no DQUANT map).
    fn lp_idx_at(&self, mbx: usize, mby: usize) -> usize {
        self.lp_idx_map.get(mby * self.mbw + mbx).copied().unwrap_or(0) as usize
    }

    fn hp_idx_at(&self, mbx: usize, mby: usize) -> usize {
        self.hp_idx_map.get(mby * self.mbw + mbx).copied().unwrap_or(0) as usize
    }

    pub(super) fn encode_mb(&mut self, sink: &mut codestream::Sink, mbx: usize, mby: usize) {
        // Tile-relative position: edge tests, the 16-MB adapt cadence, and the
        // last-column reset are all within-tile (decoder `MB::new` flags).
        let (mbxt, mbyt) = (mbx - self.tile_origin.0, mby - self.tile_origin.1);
        let (is_left, is_top) = (mbxt == 0, mbyt == 0);

        // Per-MB QP-set indices (DQUANT) — the decoder reads them before any
        // band payload (`lp_tile_mb_qp`/`hp_tile_mb_qp`): in spatial order
        // they land right here before DC; in frequency order each lands in
        // its band's packet just before this MB's payload.
        if self.bands != DCONLY && self.num_lp_qps > 1 {
            codestream::write_qp_index(sink.lp(), self.lp_idx_at(mbx, mby), self.num_lp_qps);
        }
        if self.bands != DCONLY && self.bands != NOHIGHPASS && self.num_hp_qps > 1 {
            codestream::write_qp_index(sink.hp(), self.hp_idx_at(mbx, mby), self.num_hp_qps);
        }
        let dc_of = |mx: usize, my: usize| {
            [self.dclp[mx][my][0][0], self.dclp[mx][my][1][0], self.dclp[mx][my][2][0]]
        };

        // ---------- DC ----------
        let bw = sink.dc();
        // Table 128: chroma strength weighting scales with subsampling;
        // Table 129: TOP_LEFT prediction of SUBSAMPLED chroma rounds up.
        let i_scale = match self.fmt {
            INT_YUV420 => 8,
            INT_YUV422 => 4,
            _ => 2,
        };
        let chroma_round = if self.fmt == INT_YUV444 { 0 } else { 1 };
        let (dmode, dc_preds): (u8, [i32; 3]) = if is_left && is_top {
            (NO_PREDICTION, [0; 3])
        } else if is_left {
            (PREDICT_FROM_TOP, dc_of(mbx, mby - 1))
        } else if is_top {
            (PREDICT_FROM_LEFT, dc_of(mbx - 1, mby))
        } else {
            let (l, t, tl) = (dc_of(mbx - 1, mby), dc_of(mbx, mby - 1), dc_of(mbx - 1, mby - 1));
            let sh =
                (tl[0] - l[0]).abs() * i_scale + (tl[1] - l[1]).abs() + (tl[2] - l[2]).abs();
            let sv =
                (tl[0] - t[0]).abs() * i_scale + (tl[1] - t[1]).abs() + (tl[2] - t[2]).abs();
            if sh * 4 < sv {
                (PREDICT_FROM_TOP, t)
            } else if sv * 4 < sh {
                (PREDICT_FROM_LEFT, l)
            } else {
                (
                    PREDICT_FROM_TOP_LEFT,
                    [
                        (t[0] + l[0]) >> 1,
                        (t[1] + l[1] + chroma_round) >> 1,
                        (t[2] + l[2] + chroma_round) >> 1,
                    ],
                )
            }
        };
        let mut dc_res = [0i32; 3];
        let mut babs = [false; 3];
        for comp in 0..3 {
            let mb = self.model_dc.m_bits[chroma_component(comp)];
            dc_res[comp] = self.dclp[mbx][mby][comp][0] - dc_preds[comp];
            babs[comp] = (dc_res[comp].unsigned_abs() >> mb as u32) > 0;
        }
        let val = ((babs[0] as i32) << 2) | ((babs[1] as i32) << 1) | (babs[2] as i32);
        write_huff(bw, tables::val_dc_yuv(), val);
        let mut lap_dc = [0i32; 2];
        for comp in 0..3 {
            let chroma = chroma_component(comp);
            let mb = self.model_dc.m_bits[chroma];
            let abs_vlc = if chroma == 0 { &mut self.abs_dc_lum } else { &mut self.abs_dc_chr };
            let abs_table = tables::abs_level_index(abs_vlc.table_index as usize);
            let idx = coeff::encode_dc_residual(bw, dc_res[comp], mb, babs[comp], abs_table);
            if babs[comp] {
                abs_vlc.discrim += ABS_DELTA[idx as usize];
                lap_dc[chroma] += 1;
            }
        }
        match self.fmt {
            INT_YUV420 => self.model_dc.update_42x(lap_dc, 0, true),
            INT_YUV422 => self.model_dc.update_42x(lap_dc, 0, false),
            _ => self.model_dc.update(lap_dc, 0, 3),
        }
        if mbxt % 16 == 0 || mbxt == self.tile_w - 1 {
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
        // LP prediction additionally requires the neighbour to share this
        // MB's LP QP-set index (decoder `mb_lp_mode`) — always true without
        // DQUANT.
        let lp_mode = match dmode {
            PREDICT_FROM_LEFT if self.lp_idx_at(mbx - 1, mby) == self.lp_idx_at(mbx, mby) => {
                PREDICT_FROM_LEFT
            }
            PREDICT_FROM_TOP if self.lp_idx_at(mbx, mby - 1) == self.lp_idx_at(mbx, mby) => {
                PREDICT_FROM_TOP
            }
            _ => NO_PREDICTION,
        };
        let is_42x = self.fmt != INT_YUV444;
        let mut lp_res = [[0i32; 16]; 3];
        let mut coarse = [[0i32; 16]; 3];
        for comp in 0..3 {
            let chroma_42x = is_42x && comp > 0;
            let mb = self.model_lp.m_bits[chroma_component(comp)] as u32;
            let jm = if chroma_42x { self.jmax_ch } else { 16 };
            for j in 1..jm {
                let pred = if !chroma_42x {
                    // Luma and 444 chroma: spec translations {1,2,3} / {4,8,12}.
                    if lp_mode == PREDICT_FROM_LEFT && matches!(j, 1 | 2 | 3) {
                        self.dclp[mbx - 1][mby][comp][j]
                    } else if lp_mode == PREDICT_FROM_TOP && matches!(j, 4 | 8 | 12) {
                        self.dclp[mbx][mby - 1][comp][j]
                    } else {
                        0
                    }
                } else if self.fmt == INT_YUV420 {
                    // Table 133, 420 chroma in our transposed storage
                    // (decoder.rs:1184-1195): ours j=1 from left, j=2 from top.
                    if lp_mode == PREDICT_FROM_LEFT && j == 1 {
                        self.dclp[mbx - 1][mby][comp][1]
                    } else if lp_mode == PREDICT_FROM_TOP && j == 2 {
                        self.dclp[mbx][mby - 1][comp][2]
                    } else {
                        0
                    }
                } else {
                    // Table 133, 422 chroma (decoder.rs:1196-1218): LEFT predicts
                    // ours {4,1,5}; TOP: j4←top[4], j2←top[6], j6←own FINAL j2
                    // (the decoder adds top[6] to j2 BEFORE chaining j2 into j6,
                    // so the j6 residual is against the final j2 value); the
                    // MBDCMode==TOP special chains j6←j2 even with LP
                    // prediction off.
                    if lp_mode == PREDICT_FROM_LEFT && matches!(j, 4 | 1 | 5) {
                        self.dclp[mbx - 1][mby][comp][j]
                    } else if lp_mode == PREDICT_FROM_TOP {
                        match j {
                            4 => self.dclp[mbx][mby - 1][comp][4],
                            2 => self.dclp[mbx][mby - 1][comp][6],
                            6 => self.dclp[mbx][mby][comp][2],
                            _ => 0,
                        }
                    } else if lp_mode == NO_PREDICTION && dmode == PREDICT_FROM_TOP && j == 6 {
                        self.dclp[mbx][mby][comp][2]
                    } else {
                        0
                    }
                };
                lp_res[comp][j] = self.dclp[mbx][mby][comp][j] - pred;
                let m = (lp_res[comp][j].unsigned_abs() >> mb) as i32;
                coarse[comp][j] = if lp_res[comp][j] < 0 { -m } else { m };
            }
        }

        // CBPLP + coded blocks + refinement. 444 codes three per-component
        // flags/blocks; 420/422 code luma + ONE joint chroma "plane"
        // (i_full_planes = 2, Table 53).
        let mut lap_lp = [0i32; 2];
        if !is_42x {
            let cbp = [
                (1..16).any(|j| coarse[0][j] != 0),
                (1..16).any(|j| coarse[1][j] != 0),
                (1..16).any(|j| coarse[2][j] != 0),
            ];
            let i_cbplp = (cbp[0] as i32) | ((cbp[1] as i32) << 1) | ((cbp[2] as i32) << 2);
            // cbplp coding: Huffman (with optional inversion) when the count
            // state says so, else 3 raw bits — mirrors `mb_lp` (decoder.rs:940).
            let i_max = 3 * 4 - 5; // = 7 (all bits set)
            if self.count_zero_cbplp <= 0 || self.count_max_cbplp < 0 {
                let cbplp_yuv1 = if self.count_max_cbplp < self.count_zero_cbplp {
                    i_max - i_cbplp
                } else {
                    i_cbplp
                };
                write_huff(bw, tables::cbplp_yuv1_444(), cbplp_yuv1);
            } else {
                bw.write_bits(i_cbplp as u64, 3);
            }
            self.count_zero_cbplp =
                (self.count_zero_cbplp + 1 - if i_cbplp == 0 { 4 } else { 0 }).clamp(-8, 7);
            self.count_max_cbplp =
                (self.count_max_cbplp + 1 - if i_cbplp == i_max { 4 } else { 0 }).clamp(-8, 7);

            for comp in 0..3 {
                let chroma = chroma_component(comp);
                if cbp[comp] {
                    // Build (run, level) pairs in shared-scan order, adapting the
                    // one scan across all components exactly as the decoder does.
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
                    lap_lp[chroma] += pairs.len() as i32;
                    coeff::encode_block(
                        bw,
                        &pairs,
                        1,
                        &mut self.lp_first[chroma],
                        &mut self.lp_ind0[chroma],
                        &mut self.lp_ind1[chroma],
                        &mut self.lp_abs0,
                        &mut self.lp_abs1,
                    );
                }
                let mb = self.model_lp.m_bits[chroma];
                if mb > 0 {
                    for j in 1..16 {
                        coeff::encode_refine_lp(bw, coarse[comp][j], lp_res[comp][j], mb);
                    }
                }
            }
            self.model_lp.update(lap_lp, 1, 3);
        } else {
            let jmax = self.jmax_ch;
            let cbp_luma = (1..16).any(|j| coarse[0][j] != 0);
            let cbp_chroma =
                (1..jmax).any(|j| coarse[1][j] != 0 || coarse[2][j] != 0);
            let i_cbplp = (cbp_luma as i32) | ((cbp_chroma as i32) << 1);
            let i_max = 2 * 4 - 5; // = 3 (iFullPlanes = 2, Table 53)
            if self.count_zero_cbplp <= 0 || self.count_max_cbplp < 0 {
                let cbplp_yuv1 = if self.count_max_cbplp < self.count_zero_cbplp {
                    i_max - i_cbplp
                } else {
                    i_cbplp
                };
                write_huff(bw, tables::cbplp_yuv1_42x(), cbplp_yuv1);
            } else {
                bw.write_bits(i_cbplp as u64, 2);
            }
            self.count_zero_cbplp =
                (self.count_zero_cbplp + 1 - if i_cbplp == 0 { 4 } else { 0 }).clamp(-8, 7);
            self.count_max_cbplp =
                (self.count_max_cbplp + 1 - if i_cbplp == i_max { 4 } else { 0 }).clamp(-8, 7);

            // n = 0: luma block (adaptive shared scan) + refinement.
            if cbp_luma {
                let mut pairs: Vec<(u32, i32)> = Vec::new();
                let mut run = 0u32;
                for i in 1..16usize {
                    let pos = self.scan.translate(i);
                    if coarse[0][pos] != 0 {
                        pairs.push((run, coarse[0][pos]));
                        run = 0;
                        self.scan.adapt(i);
                    } else {
                        run += 1;
                    }
                }
                lap_lp[0] += pairs.len() as i32;
                coeff::encode_block(
                    bw,
                    &pairs,
                    1,
                    &mut self.lp_first[0],
                    &mut self.lp_ind0[0],
                    &mut self.lp_ind1[0],
                    &mut self.lp_abs0,
                    &mut self.lp_abs1,
                );
            }
            let mb_lum = self.model_lp.m_bits[0];
            if mb_lum > 0 {
                for j in 1..16 {
                    coeff::encode_refine_lp(bw, coarse[0][j], lp_res[0][j], mb_lum);
                }
            }
            // n = 1: ONE joint U/V block with the FIXED interleaved order
            // (Table 53; decoder.rs:1084-1103): coded position k carries
            // component (k&1)+1, coefficient REMAP_ARR[(k>>1)+offset] in our
            // transposed storage. No adaptive-scan participation.
            const REMAP_ARR: [usize; 7] = [4, 1, 2, 3, 5, 6, 7];
            let (offset, count, i_location) =
                if self.fmt == INT_YUV420 { (1usize, 6usize, 10usize) } else { (0, 14, 2) };
            if cbp_chroma {
                let mut pairs: Vec<(u32, i32)> = Vec::new();
                let mut run = 0u32;
                for k in 0..count {
                    let v = coarse[(k & 1) + 1][REMAP_ARR[(k >> 1) + offset]];
                    if v != 0 {
                        pairs.push((run, v));
                        run = 0;
                    } else {
                        run += 1;
                    }
                }
                lap_lp[1] += pairs.len() as i32;
                coeff::encode_block(
                    bw,
                    &pairs,
                    i_location,
                    &mut self.lp_first[1],
                    &mut self.lp_ind0[1],
                    &mut self.lp_ind1[1],
                    &mut self.lp_abs0,
                    &mut self.lp_abs1,
                );
            }
            // Chroma refinement interleaves U,V per coefficient (linear order
            // in our transposed storage; decoder.rs:1120-1129).
            let mb_chr = self.model_lp.m_bits[1];
            if mb_chr > 0 {
                for k in 1..jmax {
                    coeff::encode_refine_lp(bw, coarse[1][k], lp_res[1][k], mb_chr);
                    coeff::encode_refine_lp(bw, coarse[2][k], lp_res[2][k], mb_chr);
                }
            }
            self.model_lp.update_42x(lap_lp, 1, self.fmt == INT_YUV420);
        }
        if mbxt % 16 == 0 || mbxt == self.tile_w - 1 {
            for t in self.lp_first.iter_mut() {
                t.adapt_table2(4);
            }
            for t in self.lp_ind0.iter_mut().chain(self.lp_ind1.iter_mut()) {
                t.adapt_table2(3);
            }
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
        let cbphp_left = if is_left { [0; 3] } else { self.cbphp_grid[mbx - 1][mby] };
        let cbphp_top = if is_top { [0; 3] } else { self.cbphp_grid[mbx][mby - 1] };
        self.cbphp_grid[mbx][mby] = encode_color_hp_mb(
            sink,
            &mut self.hp_state,
            self.fmt,
            self.nblk_ch,
            self.bands == ALL_BANDS,
            self.trim,
            &self.buf_grid[mbx][mby],
            &self.dclp[mbx][mby],
            cbphp_left,
            cbphp_top,
            is_left,
            is_top,
        );
        if mbxt % 16 == 0 || mbxt == self.tile_w - 1 {
            self.hp_state.adapt();
        }
    }
}

impl codestream::TileEncode for ColorPlane {
    fn begin_tile(&mut self, first_mbx: usize, first_mby: usize, tile_w: usize) {
        ColorPlane::begin_tile(self, first_mbx, first_mby, tile_w);
    }
    fn encode_mb_at(&mut self, sink: &mut codestream::Sink, mbx: usize, mby: usize) {
        self.encode_mb(sink, mbx, mby);
    }
}

/// Primary + alpha pair: per-MB plane interleave with per-plane state, the
/// composite the decoder reads as `tile_mb(plane 0)` then `tile_mb(plane 1)`
/// inside each MB slot. Tile resets apply to BOTH planes' state.
pub(super) struct AlphaPair<'a> {
    pub primary: &'a mut ColorPlane,
    pub alpha: &'a mut super::gray::YOnlyPlane,
}

impl codestream::TileEncode for AlphaPair<'_> {
    fn begin_tile(&mut self, first_mbx: usize, first_mby: usize, tile_w: usize) {
        self.primary.begin_tile(first_mbx, first_mby, tile_w);
        self.alpha.begin_tile(first_mbx, first_mby, tile_w);
    }
    fn encode_mb_at(&mut self, sink: &mut codestream::Sink, mbx: usize, mby: usize) {
        self.primary.encode_mb(sink, mbx, mby);
        self.alpha.encode_mb(sink, mbx, mby);
    }
}

/// Encode an RGB image (3 planes, each `w*h` row-major) as a **color** JPEG-XR
/// (`24bppRGB` / `INT_YUV444 → OUT_RGB`), ALL_BANDS (DC + LP + HP + flexbits).
/// Lossless 4:4:4 round-trips bit-exactly; `QP > 0` is lossy.
///
/// Mirrors the decoder's YUV444 paths per macroblock: a `val_dc_yuv` symbol for
/// the DC abs-flags, per-component DC residuals with chroma models/tables and
/// 3-component-weighted prediction; `cbplp_yuv1_444` (with the count-escape
/// state) + per-component LP run-level via `encode_block` over a **shared**
/// adaptive scan with luma/chroma index tables, + LP refinement; YUV
/// `mb_cbphp` + per-component HP run-level + flex.
pub fn encode_color(r: &[u8], g: &[u8], b: &[u8], w: u32, h: u32, qp: QpSet) -> Vec<u8> {
    let conv = super::convert::u8_prebias;
    let (rp, gp, bp) = (conv(r, false), conv(g, false), conv(b, false));
    let mut plane = ColorPlane::new(&rp, &gp, &bp, w, h, qp);
    let mut bw = BitWriter::new();
    codestream::write_image_header(&mut bw, w, h, OUT_RGB, false, false);
    codestream::write_image_plane_header_color_allbands(&mut bw, qp.dc, qp.lp, qp.hp);
    codestream::write_vlw_esc(&mut bw, 0);
    codestream::write_common_tile_header(&mut bw);
    {
        let mut sink = codestream::Sink::Spatial(&mut bw);
        for mby in 0..plane.mbh {
            for mbx in 0..plane.mbw {
                plane.encode_mb(&mut sink, mbx, mby);
            }
        }
    }
    bw.align_to_byte();
    container::write_container(&bw.finish(), w, h, &container::pixel_format::RGB24)
}

/// Staged 4b driver (public for the oracle harness; the final API shape
/// lands at the 4b close-out): encode RGB with the given internal chroma sampling
/// (`INT_YUV444`/`INT_YUV422`/`INT_YUV420`) and `bands_present`. Internal
/// until the 4b API close-out; the public `encode_color` remains the
/// 444 ALL_BANDS path.
#[allow(clippy::too_many_arguments)]
pub fn encode_color_subsampled(
    r: &[u8],
    g: &[u8],
    b: &[u8],
    w: u32,
    h: u32,
    qp: QpSet,
    fmt: u8,
    bands: u8,
) -> Vec<u8> {
    encode_color_scaled(r, g, b, w, h, qp, fmt, bands, false)
}

/// [`encode_color_subsampled`] with **scaled arithmetic** (`scaled_flag = 1`)
/// exposed: 3 extra fraction bits through the transforms, chroma DC-LP coded
/// at half amplitude, the decoder's output stage shifting back down. This is
/// the mode jxrencapp uses for everything lossy (and all 42x).
#[allow(clippy::too_many_arguments)]
pub fn encode_color_scaled(
    r: &[u8],
    g: &[u8],
    b: &[u8],
    w: u32,
    h: u32,
    qp: QpSet,
    fmt: u8,
    bands: u8,
    scaled: bool,
) -> Vec<u8> {
    encode_color_options(r, g, b, w, h, qp, fmt, bands, scaled, 0, (0, 0), (&[], &[]), 0, false, None)
}

/// Window-margins tuple (top, left, bottom, right) for an image placed at
/// `(top, left)` inside its minimal 16-aligned grid: bottom/right are the
/// remaining alignment pads (each ≤ 15).
pub(super) fn window_margins(w: u32, h: u32, window: (u32, u32)) -> (u32, u32, u32, u32) {
    let (top, left) = window;
    let pw = (w + left).next_multiple_of(16);
    let ph = (h + top).next_multiple_of(16);
    (top, left, ph - h - top, pw - w - left)
}

/// [`encode_color_scaled`] plus `trim_flexbits` (image-header flag + the
/// 4-bit spatial-tile value + truncated flex emission) and explicit window
/// margins (`window = (top, left)`; `(0, 0)` = classic derived windowing).
/// The full internal option surface for the 3-plane color path.
#[allow(clippy::too_many_arguments)]
pub fn encode_color_options(
    r: &[u8],
    g: &[u8],
    b: &[u8],
    w: u32,
    h: u32,
    qp: QpSet,
    fmt: u8,
    bands: u8,
    scaled: bool,
    trim: u8,
    window: (u32, u32),
    tiles: (&[usize], &[usize]),
    overlap: u8,
    frequency: bool,
    plan: Option<&super::quant::QpPlan>,
) -> Vec<u8> {
    let conv = super::convert::u8_prebias;
    let (rp, gp, bp) = (conv(r, scaled), conv(g, scaled), conv(b, scaled));
    encode_color_prebias(
        &rp,
        &gp,
        &bp,
        w,
        h,
        qp,
        fmt,
        bands,
        scaled,
        trim,
        window,
        tiles,
        overlap,
        frequency,
        plan,
        &super::convert::Depth::BD8,
        &container::pixel_format::RGB24,
        OUT_RGB,
    )
}

/// Depth-general 3-plane color driver: `r`/`g`/`b` already forward-converted
/// to the pre-bias domain ([`super::convert`]), `depth` carried into the
/// image + plane headers, `guid` into the container. `out_clr_fmt` is
/// `OUT_RGB` for everything except the RGBE path (`OUT_RGBE` — same
/// 3-component YUV444 internal coding, different output formatting).
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_color_prebias(
    r: &[i32],
    g: &[i32],
    b: &[i32],
    w: u32,
    h: u32,
    qp: QpSet,
    fmt: u8,
    bands: u8,
    scaled: bool,
    trim: u8,
    window: (u32, u32),
    tiles: (&[usize], &[usize]),
    overlap: u8,
    frequency: bool,
    plan: Option<&super::quant::QpPlan>,
    depth: &super::convert::Depth,
    guid: &[u8; 16],
    out_clr_fmt: u8,
) -> Vec<u8> {
    let trim = if bands == ALL_BANDS { trim } else { 0 };
    let mut plane = ColorPlane::new_fmt(
        r, g, b, w, h, qp, fmt, bands, scaled, window, overlap, tiles.0, tiles.1, plan,
    );
    plane.trim = trim as u32;
    let mut spec = codestream::ImageHeaderSpec::new(w, h, out_clr_fmt);
    spec.output_bitdepth = depth.bitdepth;
    spec.frequency_mode = frequency;
    spec.overlap_mode = overlap;
    spec.trim_flexbits = trim;
    spec.margins = window_margins(w, h, window);
    spec.tile_cols_mb = tiles.0.to_vec();
    spec.tile_rows_mb = tiles.1.to_vec();
    let (mbw, mbh) = (plane.mbw, plane.mbh);
    let tile_headers: Box<dyn Fn(&mut BitWriter, usize, usize)> = match plan {
        None => Box::new(codestream::classic_tile_headers(trim)),
        Some(p) => {
            let ntiles = tiles.0.len().max(1) * tiles.1.len().max(1);
            assert!(
                p.tiles.len() == 1 || p.tiles.len() == ntiles,
                "QpPlan must carry 1 or ntiles tile entries"
            );
            let p = p.clone();
            let dc_uniform = p.tiles.len() == 1;
            let lp_uniform = dc_uniform && p.num_lp_qps() == 1;
            let hp_uniform = dc_uniform && p.num_hp_qps() == 1;
            Box::new(move |w: &mut BitWriter, tile: usize, band: usize| {
                // Spatial tile-header field order is the decoder's:
                // flex (trim), DC, LP, HP. Frequency packets carry only
                // their band's fields.
                let sp = band == codestream::SPATIAL_BAND;
                if (sp || band == 3) && trim != 0 {
                    codestream::write_trim_flexbits(w, trim);
                }
                let t = &p.tiles[if dc_uniform { 0 } else { tile }];
                if (sp || band == 0) && !dc_uniform {
                    codestream::write_band_qp(w, &[t.dc], 3);
                }
                if (sp || band == 1) && !lp_uniform && bands != DCONLY {
                    w.write_bits(0, 1); // use_dc_qp = 0
                    w.write_bits(t.lp.len() as u64 - 1, 4); // num_lp_qps − 1
                    codestream::write_band_qp(w, &t.lp, 3);
                }
                if (sp || band == 2) && !hp_uniform && bands != DCONLY && bands != NOHIGHPASS {
                    w.write_bits(0, 1); // use_lp_qp = 0
                    w.write_bits(t.hp.len() as u64 - 1, 4); // num_hp_qps − 1
                    codestream::write_band_qp(w, &t.hp, 3);
                }
            })
        }
    };
    let body = codestream::emit_codestream(
        &spec,
        |head| match plan {
            Some(p) => {
                codestream::write_image_plane_header_yuv_plan(head, fmt, bands, p, scaled, depth)
            }
            None => {
                let p = super::quant::QpPlan::uniform(qp, None);
                codestream::write_image_plane_header_yuv_plan(head, fmt, bands, &p, scaled, depth)
            }
        },
        &*tile_headers,
        codestream::band_count(bands),
        mbw,
        mbh,
        &mut plane,
    );
    container::write_container(&body, w, h, guid)
}

/// Encode RGB + alpha (4 planes, each `w*h` row-major) as a color JPEG-XR with
/// a T.832 **alpha image plane**: `32bppBGRA` (or `32bppPBGRA` when
/// `premultiplied`) container, `INT_YUV444` primary plane + `INT_YONLY` alpha
/// plane with its **own** uniform per-band QPs (`alpha_qp`), per-MB
/// interleaved exactly as the decoder reads them (`tile_mb(plane0)` then
/// `tile_mb(plane1)` per MB). Lossless QPs on both planes round-trip all four
/// channels bit-exactly.
///
/// This is what JxrEncApp calls *interleaved* alpha (`-a 3`, in-codestream
/// plane; its `-a 2` "planar" is the separate-container-codestream variant,
/// which is a different, container-level feature).
/// The primary plane may be 444 or subsampled (`fmt`); the alpha plane is
/// always YONLY. Subsampled chroma is lossy by construction; 444 lossless
/// round-trips all four channels bit-exactly.
#[allow(clippy::too_many_arguments)]
pub fn encode_color_alpha(
    r: &[u8],
    g: &[u8],
    b: &[u8],
    a: &[u8],
    w: u32,
    h: u32,
    qp: QpSet,
    alpha_qp: QpSet,
    premultiplied: bool,
    fmt: u8,
    scaled: bool,
    window: (u32, u32),
    tiles: (&[usize], &[usize]),
    overlap: u8,
    frequency: bool,
) -> Vec<u8> {
    let conv = super::convert::u8_prebias;
    let (rp, gp, bp, ap) = (conv(r, scaled), conv(g, scaled), conv(b, scaled), conv(a, scaled));
    let guid = if premultiplied {
        &container::pixel_format::PBGRA32
    } else {
        &container::pixel_format::BGRA32
    };
    encode_color_alpha_prebias(
        &rp,
        &gp,
        &bp,
        &ap,
        w,
        h,
        qp,
        alpha_qp,
        premultiplied,
        fmt,
        scaled,
        window,
        tiles,
        overlap,
        frequency,
        &super::convert::Depth::BD8,
        guid,
    )
}

/// Depth-general RGB+alpha driver ([`encode_color_alpha`] over pre-bias
/// planes): `depth` rides into the image header and BOTH plane headers (the
/// alpha plane carries its own `shift_bits`/float fields). The alpha plane's
/// `scaled_flag` follows the image's (the reference encoder couples them;
/// scaled lossless YONLY stays bit-exact) while its QPs stay independent
/// (`alpha_qp`). `overlap_mode` is an image-header field, so it applies to
/// the alpha plane's reconstruction too — filter it identically.
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_color_alpha_prebias(
    r: &[i32],
    g: &[i32],
    b: &[i32],
    a: &[i32],
    w: u32,
    h: u32,
    qp: QpSet,
    alpha_qp: QpSet,
    premultiplied: bool,
    fmt: u8,
    scaled: bool,
    window: (u32, u32),
    tiles: (&[usize], &[usize]),
    overlap: u8,
    frequency: bool,
    depth: &super::convert::Depth,
    guid: &[u8; 16],
) -> Vec<u8> {
    let mut primary = ColorPlane::new_fmt(
        r, g, b, w, h, qp, fmt, ALL_BANDS, scaled, window, overlap, tiles.0, tiles.1, None,
    );
    let mut alpha = super::gray::YOnlyPlane::new(a, w, h, alpha_qp, window, overlap, tiles, scaled);
    let mut spec = codestream::ImageHeaderSpec::new(w, h, OUT_RGB);
    spec.output_bitdepth = depth.bitdepth;
    spec.frequency_mode = frequency;
    spec.overlap_mode = overlap;
    spec.premultiplied_alpha = premultiplied;
    spec.alpha_image_plane = true;
    spec.margins = window_margins(w, h, window);
    spec.tile_cols_mb = tiles.0.to_vec();
    spec.tile_rows_mb = tiles.1.to_vec();
    let (mbw, mbh) = (primary.mbw, primary.mbh);
    let mut pair = AlphaPair { primary: &mut primary, alpha: &mut alpha };
    let body = codestream::emit_codestream(
        &spec,
        |head| {
            let plan = super::quant::QpPlan::uniform(qp, None);
            codestream::write_image_plane_header_yuv_plan(head, fmt, ALL_BANDS, &plan, scaled, depth);
            // Alpha image plane header: YONLY, own bands + QPs (JxrEncApp
            // analog `-Q`), same depth fields.
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
    );
    container::write_container(&body, w, h, guid)
}

/// Encode an RGB image as **internal YONLY** with `OUT_RGB` output (the
/// JxrEncApp `-d 0` analog): the forward color transform's luma is coded as a
/// single plane and the decoder replicates it into R=G=B on output. The
/// container stays `24bppRGB`. Gray sources (R=G=B) round-trip exactly —
/// their luma IS the gray value; color sources reconstruct as luma.
#[allow(clippy::too_many_arguments)]
pub fn encode_yonly_from_color(
    r: &[u8],
    g: &[u8],
    b: &[u8],
    w: u32,
    h: u32,
    qp: QpSet,
    scaled: bool,
    window: (u32, u32),
    tiles: (&[usize], &[usize]),
    overlap: u8,
    frequency: bool,
) -> Vec<u8> {
    let conv = super::convert::u8_prebias;
    let (rp, gp, bp) = (conv(r, scaled), conv(g, scaled), conv(b, scaled));
    encode_yonly_prebias(
        &rp,
        &gp,
        &bp,
        w,
        h,
        qp,
        scaled,
        window,
        tiles,
        overlap,
        frequency,
        &super::convert::Depth::BD8,
        &container::pixel_format::RGB24,
    )
}

/// Depth-general YONLY-from-color driver ([`encode_yonly_from_color`] over
/// pre-bias planes).
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_yonly_prebias(
    r: &[i32],
    g: &[i32],
    b: &[i32],
    w: u32,
    h: u32,
    qp: QpSet,
    scaled: bool,
    window: (u32, u32),
    tiles: (&[usize], &[usize]),
    overlap: u8,
    frequency: bool,
    depth: &super::convert::Depth,
    guid: &[u8; 16],
) -> Vec<u8> {
    let (wu, hu) = (w as usize, h as usize);
    let (top, left) = (window.0 as usize, window.1 as usize);
    let (pw, ph) = ((wu + left).next_multiple_of(16), (hu + top).next_multiple_of(16));
    let (rp, gp, bp) = (
        pad_plane(r, wu, hu, pw, ph, top, left),
        pad_plane(g, wu, hu, pw, ph, top, left),
        pad_plane(b, wu, hu, pw, ph, top, left),
    );
    let mut y_plane = vec![0i32; pw * ph];
    for i in 0..pw * ph {
        let (y, _, _) = rgb_to_yuv444(rp[i], gp[i], bp[i]);
        y_plane[i] = y;
    }
    let mut plane = super::gray::YOnlyPlane::from_centered_padded_ovl(
        &y_plane,
        pw,
        ph,
        qp,
        scaled,
        overlap,
        &super::overlap::bounds(tiles.0, pw / 16),
        &super::overlap::bounds(tiles.1, ph / 16),
    );
    let mut spec = codestream::ImageHeaderSpec::new(w, h, OUT_RGB);
    spec.output_bitdepth = depth.bitdepth;
    spec.frequency_mode = frequency;
    spec.overlap_mode = overlap;
    spec.margins = window_margins(w, h, window);
    spec.tile_cols_mb = tiles.0.to_vec();
    spec.tile_rows_mb = tiles.1.to_vec();
    let (mbw, mbh) = (plane.mbw, plane.mbh);
    let body = codestream::emit_codestream(
        &spec,
        |head| {
            codestream::write_image_plane_header_gray_bands(
                head,
                ALL_BANDS,
                qp.dc,
                qp.lp,
                qp.hp,
                scaled,
                depth,
            )
        },
        &codestream::classic_tile_headers(0),
        4,
        mbw,
        mbh,
        &mut plane,
    );
    container::write_container(&body, w, h, guid)
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

/// Inverse of the decoder's 420 chroma prediction cascade (Table 68): the
/// decode steps applied in reverse order (each step XORs higher bits with a
/// function of untouched lower bits, so each is its own inverse).
fn unpredict_cascade_420(mut c: i32) -> i32 {
    c ^= (c & 0x3) << 2;
    c ^= 0x02 & (c << 1);
    c
}

/// Inverse of the decoder's 422 chroma prediction cascade (Table 67).
fn unpredict_cascade_422(mut c: i32) -> i32 {
    c ^= (c & 0x30) << 2;
    c ^= (c & 0x0C) << 2;
    c ^= (c & 0x03) << 2;
    c ^= (c & 0x01) << 1;
    c
}

/// Encode the three components' `mb_cbphp` (per-block HP coded-block
/// patterns: 16-bit luma/444, 8-bit 422 chroma, 4-bit 420 chroma) — inverse
/// of the YUV `mb_cbphp` reader + `pred_cbphp_444/422/420`. First unpredict
/// each component (cascade + neighbour bit per `chroma_flag` state; chroma
/// model counts scale ×2 (422) / ×4 (420) to the 16-block frame) and update
/// the CBPHP model, then emit the interleaved structure: `num_cbphp` over the
/// four block-groups, and per present group a `num_blk_cbphp` carrying the
/// luma nibble plus (via the `i_val≥6` escape) `chr_cbphp`/`val_inc` and the
/// chroma parts — nibbles via `num_ch_blk`+refine for 444, a `chr_cbphp`
/// pair-pattern symbol for 422, and nothing further for 420 (the 0x10/0x20
/// flags ARE the per-group single-block chroma bits).
#[allow(clippy::too_many_arguments)]
fn encode_color_cbphp(
    bw: &mut BitWriter,
    st: &mut ColorHpState,
    fmt: u8,
    mb_cbphp: [i32; 3],
    cbphp_left: [i32; 3],
    cbphp_top: [i32; 3],
    is_left: bool,
    is_top: bool,
) {
    let is_42x = fmt != INT_YUV444;
    // --- unpredict per component + update CBPHP model ---
    let mut i_diff = [0i32; 3];
    for comp in 0..3 {
        let cf = (comp > 0) as usize;
        let chroma_42x = is_42x && comp > 0;
        // Neighbour-bit positions + full-pattern mask per predictor
        // (Tables 65/67/68).
        let (top_shift, left_shift, full_mask) = if !chroma_42x {
            (10u32, 5u32, 0xFFFF)
        } else if fmt == INT_YUV420 {
            (2, 1, 0xF)
        } else {
            (6, 1, 0xFF)
        };
        let neighbor = if is_left {
            if is_top { 1 } else { (cbphp_top[comp] >> top_shift) & 1 }
        } else {
            (cbphp_left[comp] >> left_shift) & 1
        };
        i_diff[comp] = match st.cbphp_model.cbphp_state[cf] {
            0 => {
                let un = if !chroma_42x {
                    hp::unpredict_cascade(mb_cbphp[comp])
                } else if fmt == INT_YUV420 {
                    unpredict_cascade_420(mb_cbphp[comp])
                } else {
                    unpredict_cascade_422(mb_cbphp[comp])
                };
                un ^ neighbor
            }
            2 => mb_cbphp[comp] ^ full_mask,
            _ => mb_cbphp[comp],
        };
        let mult = if !chroma_42x {
            1
        } else if fmt == INT_YUV420 {
            4
        } else {
            2
        };
        let n_orig = num_ones(mb_cbphp[comp] as u32) as i32 * mult;
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
    // The chroma "part" a block-group carries: full nibble (444), the single
    // per-group bit (420), or the column-pair pattern at bits {0,2} of the
    // group-shifted byte (422, mask 0b101 — decoder I_SHIFT mapping).
    const I_SHIFT_422: [i32; 4] = [0, 1, 4, 5];
    let chroma_part = |d: i32, b: usize| -> i32 {
        if !is_42x {
            nib(d, b)
        } else if fmt == INT_YUV420 {
            (d >> b) & 1
        } else {
            (d >> I_SHIFT_422[b]) & 5
        }
    };
    let mut i_cbphp = 0i32;
    for b in 0..4 {
        if nib(i_diff[0], b) != 0
            || chroma_part(i_diff[1], b) != 0
            || chroma_part(i_diff[2], b) != 0
        {
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
        let u_part = chroma_part(i_diff[1], b);
        let v_part = chroma_part(i_diff[2], b);
        let chroma_bits =
            (if u_part != 0 { 0x10 } else { 0 }) | (if v_part != 0 { 0x20 } else { 0 });
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
            if !is_42x {
                for &part in &[u_part, v_part] {
                    if part != 0 {
                        let num = num_ones(part as u32) as i32;
                        write_huff(bw, tables::num_ch_blk(), num - 1);
                        write_cbphp_refine(bw, part, num);
                    }
                }
            } else if fmt == INT_YUV422 {
                // CBPHP_CH_BLK: pair pattern {1,4,5} → chr_cbphp symbol 0/1/2
                // (decoder reconstructs via I_SHIFT[(v+1)]).
                for &part in &[u_part, v_part] {
                    if part != 0 {
                        let v = match part {
                            1 => 0,
                            4 => 1,
                            _ => 2, // 5 = both blocks of the pair
                        };
                        write_huff(bw, tables::chr_cbphp(), v);
                    }
                }
            }
            // 420: nothing further — the 0x10/0x20 flags carried it all.
        }
    }
}

/// Encode the HP band of one macroblock for all 3 components — inverse of
/// `mb_cbphp` + `mb_hp_flex` + `hp_transform_coefficient_decoding`. `bufs` are
/// the per-component forward `mb_buffer`s (HP quantized at within-block
/// positions); `lp` the per-component DC+LP levels (for the shared HP pred
/// mode). Returns each component's `mb_cbphp` for neighbour prediction.
#[allow(clippy::too_many_arguments)]
fn encode_color_hp_mb(
    sink: &mut codestream::Sink,
    st: &mut ColorHpState,
    fmt: u8,
    nblk_ch: usize,
    emit_flex: bool,
    trim: u32,
    bufs: &[[i32; 256]; 3],
    lp: &[[i32; 16]; 3],
    cbphp_left: [i32; 3],
    cbphp_top: [i32; 3],
    is_left: bool,
    is_top: bool,
) -> [i32; 3] {
    let is_42x = fmt != INT_YUV444;
    // Shared HP prediction mode: luma LP + chroma LP strength, with the
    // format-specific chroma terms (Table 135 in our transposed mb_dclp
    // storage — mirrors `decoder.rs` CalcHPPredMode).
    let mut s_hor = lp[0][1].abs() + lp[0][2].abs() + lp[0][3].abs();
    let mut s_ver = lp[0][4].abs() + lp[0][8].abs() + lp[0][12].abs();
    for c in 1..3 {
        match fmt {
            INT_YUV420 => {
                s_hor += lp[c][1].abs();
                s_ver += lp[c][2].abs();
            }
            INT_YUV422 => {
                s_hor += lp[c][1].abs() + lp[c][5].abs();
                s_ver += lp[c][2].abs() + lp[c][6].abs();
            }
            _ => {
                s_hor += lp[c][1].abs();
                s_ver += lp[c][4].abs();
            }
        }
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
        let chroma_42x = is_42x && comp > 0;
        let nblk = if chroma_42x { nblk_ch } else { 16 };
        let buf = &bufs[comp];
        let r = &mut res[comp];
        for blk in 0..nblk {
            for pos in 1..16 {
                r[blk][pos] = buf[blk * 16 + pos];
            }
        }
        // Within-MB HP prediction (Table 136). Luma/444 blocks use the
        // permuted 16-block layout (TOP: blk−1 within columns; LEFT: blk−4);
        // 42x chroma blocks are raster-indexed 2-wide, so TOP predicts from
        // blk−2 and LEFT from blk−1 (the decoder's blkId lists/strides).
        if mode == PREDICT_FROM_TOP {
            if !chroma_42x {
                for &blk in &[1usize, 2, 3, 5, 6, 7, 9, 10, 11, 13, 14, 15] {
                    for &k in &[2usize, 10, 9] {
                        r[blk][k] = buf[blk * 16 + k] - buf[(blk - 1) * 16 + k];
                    }
                }
            } else {
                let top_blks: &[usize] =
                    if fmt == INT_YUV420 { &[2, 3] } else { &[2, 4, 6, 3, 5, 7] };
                for &blk in top_blks {
                    for &k in &[2usize, 10, 9] {
                        r[blk][k] = buf[blk * 16 + k] - buf[(blk - 2) * 16 + k];
                    }
                }
            }
        } else if mode == PREDICT_FROM_LEFT {
            if !chroma_42x {
                for blk in 4..16 {
                    for &k in &[1usize, 5, 6] {
                        r[blk][k] = buf[blk * 16 + k] - buf[(blk - 4) * 16 + k];
                    }
                }
            } else {
                let left_blks: &[usize] =
                    if fmt == INT_YUV420 { &[1, 3] } else { &[1, 3, 5, 7] };
                for &blk in left_blks {
                    for &k in &[1usize, 5, 6] {
                        r[blk][k] = buf[blk * 16 + k] - buf[(blk - 1) * 16 + k];
                    }
                }
            }
        }
        let mbits = st.model.m_bits[chroma_component(comp)] as u32;
        for blk in 0..nblk {
            for pos in 1..16 {
                let v = r[blk][pos];
                let c = (v.unsigned_abs() >> mbits) as i32;
                coarse[comp][blk][pos] = if v < 0 { -c } else { c };
            }
        }
        // CBP bit order: hierarchical scan for 16-block planes, raster
        // (identity) for 4/8-block 42x chroma (Tables 69/83).
        let mut cbp = 0i32;
        for k in 0..nblk {
            let blk = if nblk == 16 { I_HIER_SCAN_ORDER[k] } else { k };
            if (1..16).any(|pos| coarse[comp][blk][pos] != 0) {
                cbp |= 1 << k;
            }
        }
        mb_cbphp[comp] = cbp;
    }

    encode_color_cbphp(sink.hp(), st, fmt, mb_cbphp, cbphp_left, cbphp_top, is_left, is_top);

    let mut lap = [0i32; 2];
    for comp in 0..3 {
        let chroma = chroma_component(comp);
        let chroma_42x = is_42x && comp > 0;
        let nblk = if chroma_42x { nblk_ch } else { 16 };
        let mbits = st.model.m_bits[chroma];
        let mut cbp = mb_cbphp[comp];
        for k in 0..nblk {
            let blk = if nblk == 16 { I_HIER_SCAN_ORDER[k] } else { k };
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
                    sink.hp(),
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
            // Flexbits: skipped entirely at NOFLEXBITS; truncated by `trim`
            // when the image header sets trim_flexbits_flag.
            if emit_flex && mbits > 0 {
                for &n in &I_TRANSPOSE_FLEX[1..] {
                    coeff::encode_flexbits(
                        sink.flex(),
                        coarse[comp][blk][n],
                        res[comp][blk][n],
                        mbits,
                        trim,
                    );
                }
            }
        }
    }
    match fmt {
        INT_YUV420 => st.model.update_42x(lap, 2, true),
        INT_YUV422 => st.model.update_42x(lap, 2, false),
        _ => st.model.update(lap, 2, 3),
    }
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

    /// `DF_ODD` filter arithmetic + boundary reflection, pinned against
    /// hand-computed values (filter = [1,4,6,4,1]/16 round-half-up on the
    /// even-centered window; left edge reflects src[-k]→src[k], right edge
    /// src[m+k]→src[m-k] — `strenc.c` `downsampleUV` boundary behavior).
    #[test]
    fn downsample_filter_and_boundaries() {
        // Constant plane → constant (taps sum to 16).
        let c = vec![7i32; 16 * 4];
        assert!(downsample_h(&c, 16, 4).iter().all(|&v| v == 7));
        assert!(downsample_v(&c, 16, 4).iter().all(|&v| v == 7));
        // A 16-impulse at even column 4 lands in the windows centered at
        // 2x = 2, 4, 6 with weights 1, 6, 1 (out[0]'s window 0±2 misses it).
        let mut imp = vec![0i32; 16];
        imp[4] = 16;
        let out = downsample_h(&imp, 16, 1);
        assert_eq!(out, vec![0, 1, 6, 1, 0, 0, 0, 0]);
        // Left-boundary reflection: src = [10, 20, 30, ...]: out[0] window is
        // d0=src[2], d1=src[1], d2=src[0], d3=src[1], d4=src[2] →
        // (30 + 80 + 60 + 80 + 30 + 8) >> 4 = 288>>4 = 18.
        let mut edge = vec![0i32; 16];
        edge[0] = 10;
        edge[1] = 20;
        edge[2] = 30;
        let out = downsample_h(&edge, 16, 1);
        assert_eq!(out[0], (30 + 4 * 20 + 6 * 10 + 4 * 20 + 30 + 8) >> 4);
        // Right-boundary reflection: impulse at the last column (15): only
        // out[7] (center 14) sees it via d3 (real) — and d4 reflects 16→14
        // (value 0). Last-column impulse contributes 4/16 ⇒ (4*16+8)>>4 = 4.
        let mut tail = vec![0i32; 16];
        tail[15] = 16;
        let out = downsample_h(&tail, 16, 1);
        assert_eq!(out[7], 4);
        // Vertical pass mirrors horizontal on a transposed layout.
        let mut vimp = vec![0i32; 2 * 16];
        vimp[4 * 2] = 16; // column 0, row 4
        let out = downsample_v(&vimp, 2, 16);
        let col: Vec<i32> = (0..8).map(|y| out[y * 2]).collect();
        assert_eq!(col, vec![0, 1, 6, 1, 0, 0, 0, 0]);
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

    /// 4b Stage A gate: whole-image constant color at 420/422 **DCONLY**
    /// round-trips END-TO-END exactly (constant chroma survives decimation;
    /// the decoder's centering upsample of a constant is that constant),
    /// across 1-MB, multi-MB, and non-aligned sizes — exercising the 42x
    /// plane header, chroma MB transforms, chroma DC prediction (iScale +
    /// rounding) and the Table-116 model dispatch on real grids.
    #[test]
    fn roundtrip_constant_color_42x_dconly() {
        for &fmt in &[INT_YUV420, INT_YUV422] {
            for &(w, h) in &[(16usize, 16usize), (48, 32), (17, 31), (64, 64)] {
                for &(r, g, b) in
                    &[(200u8, 50u8, 100u8), (0, 0, 0), (255, 255, 255), (128, 128, 128), (10, 200, 250)]
                {
                    let (rp, gp, bp) = (vec![r; w * h], vec![g; w * h], vec![b; w * h]);
                    let jxr = encode_color_subsampled(
                        &rp, &gp, &bp, w as u32, h as u32, QpSet::LOSSLESS, fmt, DCONLY,
                    );
                    let expected = vec![(r, g, b); w * h];
                    assert_rgb_exact(&jxr, w, h, &expected);
                }
            }
        }
    }

    /// 4b Stage B gate (in-crate half): zero-HP **luma** detail over CONSTANT
    /// chroma at 420/422 **NOHIGHPASS** is end-to-end exact — the luma LP path
    /// runs fully inside the 42x MB structure (cbplp_yuv1_42x, the
    /// iFullPlanes=2 raw width, refinement order), while constant chroma
    /// survives decimation/upsampling exactly. Chroma LP *values* are gated
    /// externally via JxrDecApp (decimation is lossy end-to-end).
    #[test]
    fn roundtrip_zero_hp_luma_42x_nohighpass() {
        let mut r = Lcg2(0x42b5_7a6e_0042_b57a);
        for &fmt in &[INT_YUV420, INT_YUV422] {
            for &(mbw, mbh) in &[(1usize, 1usize), (2, 2), (3, 2)] {
                let (w, h) = (mbw * 16, mbh * 16);
                let mut gray = vec![0u8; w * h];
                for mx in 0..mbw {
                    for my in 0..mbh {
                        let blk = loop {
                            let yb = zero_hp_centered(&mut r, 14);
                            let mut ok = true;
                            let mut out = [0u8; 256];
                            for p in 0..256 {
                                let v = yb[p] + 128;
                                if !(0..=255).contains(&v) {
                                    ok = false;
                                    break;
                                }
                                out[p] = v as u8;
                            }
                            if ok {
                                break out;
                            }
                        };
                        for py in 0..16 {
                            for px in 0..16 {
                                gray[(my * 16 + py) * w + (mx * 16 + px)] = blk[py * 16 + px];
                            }
                        }
                    }
                }
                // R=G=B ⇒ U=V=0 everywhere (constant chroma), Y = gray − 128.
                let jxr = encode_color_subsampled(
                    &gray, &gray, &gray, w as u32, h as u32, QpSet::LOSSLESS, fmt, NOHIGHPASS,
                );
                let expected: Vec<(u8, u8, u8)> =
                    gray.iter().map(|&v| (v, v, v)).collect();
                assert_rgb_exact(&jxr, w, h, &expected);
            }
        }
    }

    /// 4b Stage C gate (in-crate half): arbitrary **luma** detail over
    /// CONSTANT chroma at 420/422 **ALL_BANDS** lossless is end-to-end exact —
    /// the full luma DC+LP+HP+flex path runs inside the 42x MB structure
    /// (CBPHP chroma arms see all-zero patterns but the group/escape coding
    /// still runs), and constant chroma survives decimation exactly.
    #[test]
    fn roundtrip_gray_content_42x_allbands() {
        let mut r = Lcg(0xc0a1_e5ce_0042_c0a1);
        for &fmt in &[INT_YUV420, INT_YUV422] {
            for &(w, h) in &[(48usize, 32usize), (17, 31)] {
                let n = w * h;
                let gray: Vec<u8> = (0..n).map(|_| r.byte()).collect();
                let jxr = encode_color_subsampled(
                    &gray, &gray, &gray, w as u32, h as u32, QpSet::LOSSLESS, fmt, ALL_BANDS,
                );
                let expected: Vec<(u8, u8, u8)> = gray.iter().map(|&v| (v, v, v)).collect();
                assert_rgb_exact(&jxr, w, h, &expected);
            }
        }
    }

    /// 4b Stage C structural check: arbitrary content at 420/422 ALL_BANDS
    /// decodes to the right shape across QPs (joint LP + chroma CBPHP/HP all
    /// emitting); pixel exactness is the harness's JxrDecApp gate.
    #[test]
    fn subsampled_allbands_streams_decode() {
        let mut r = Lcg(0xa11b_a4d5_0042_a11b);
        for &fmt in &[INT_YUV420, INT_YUV422] {
            for &(w, h) in &[(48usize, 32usize), (33, 17)] {
                let n = w * h;
                let rp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
                let gp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
                let bp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
                for &qp in &[QpSet::LOSSLESS, QpSet { dc: 16, lp: 32, hp: 64 }] {
                    let jxr = encode_color_subsampled(
                        &rp, &gp, &bp, w as u32, h as u32, qp, fmt, ALL_BANDS,
                    );
                    let d = decode(&jxr);
                    assert_eq!(
                        (d.width as usize, d.height as usize, d.num_components),
                        (w, h, 3),
                        "fmt={fmt} {w}x{h}"
                    );
                }
            }
        }
    }

    /// 4b Stage D: YONLY-from-color (`-d 0` analog). Gray content (R=G=B)
    /// round-trips exactly — its luma IS the gray value and the decoder
    /// replicates YONLY into R=G=B; color content decodes to the right shape.
    #[test]
    fn yonly_from_color_roundtrip() {
        let mut r = Lcg(0xd0d0_0042_d0d0_0042);
        for &(w, h) in &[(48usize, 32usize), (17, 31)] {
            let n = w * h;
            let gray: Vec<u8> = (0..n).map(|_| r.byte()).collect();
            let jxr = encode_yonly_from_color(&gray, &gray, &gray, w as u32, h as u32, QpSet::LOSSLESS, false, (0, 0), (&[], &[]), 0, false);
            let expected: Vec<(u8, u8, u8)> = gray.iter().map(|&v| (v, v, v)).collect();
            assert_rgb_exact(&jxr, w, h, &expected);
            // Color content: structural (decoder replicates luma; not source-exact).
            let rp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
            let gp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
            let bp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
            let jxr = encode_yonly_from_color(&rp, &gp, &bp, w as u32, h as u32, QpSet::LOSSLESS, false, (0, 0), (&[], &[]), 0, false);
            let d = decode(&jxr);
            assert_eq!((d.width as usize, d.height as usize, d.num_components), (w, h, 3));
            // Replication property: all three output channels identical.
            for i in 0..n {
                assert_eq!(d.image_plane[0][i], d.image_plane[1][i]);
                assert_eq!(d.image_plane[1][i], d.image_plane[2][i]);
            }
        }
    }

    /// 4b Stage D: alpha image plane over a SUBSAMPLED (420) primary. Gray
    /// content (constant zero chroma) + noise alpha at lossless is exact on
    /// all four channels — luma and the alpha plane are full-resolution paths.
    #[test]
    fn alpha_over_subsampled_primary_roundtrip() {
        let mut r = Lcg(0xa1fa_0420_a1fa_0420);
        for &fmt in &[INT_YUV420, INT_YUV422] {
            let (w, h) = (48usize, 32usize);
            let n = w * h;
            let gray: Vec<u8> = (0..n).map(|_| r.byte()).collect();
            let alpha: Vec<u8> = (0..n).map(|_| r.byte()).collect();
            let jxr = encode_color_alpha(
                &gray, &gray, &gray, &alpha, w as u32, h as u32,
                QpSet::LOSSLESS, QpSet::LOSSLESS, false, fmt, false, (0, 0), (&[], &[]), 0, false,
            );
            let d = decode(&jxr);
            assert_eq!(d.num_components, 4, "fmt={fmt}");
            assert!(d.has_alpha);
            for i in 0..n {
                assert_eq!(d.image_plane[0][i], gray[i] as i32, "luma px{i}");
                assert_eq!(d.image_plane[1][i], gray[i] as i32);
                assert_eq!(d.image_plane[2][i], gray[i] as i32);
                assert_eq!(d.image_plane[3][i], alpha[i] as i32, "alpha px{i}");
            }
        }
    }

    /// 4b Stage D: scaled arithmetic. Gray content (zero chroma — halving of
    /// zero is exact) at scaled q1 is end-to-end exact for every sampling:
    /// the luma path is `<<3` in, `(v+bias<<3+3)>>3` out, exactly invertible.
    /// Arbitrary content decodes to shape (chroma half-step is lossy by
    /// design; JxrDecApp readback exactness is the harness gate).
    #[test]
    fn scaled_arithmetic_roundtrips() {
        let mut r = Lcg(0x5ca1_ed00_5ca1_ed00);
        for &fmt in &[INT_YUV444, INT_YUV420, INT_YUV422] {
            let (w, h) = (48usize, 32usize);
            let n = w * h;
            let gray: Vec<u8> = (0..n).map(|_| r.byte()).collect();
            let jxr = encode_color_scaled(
                &gray, &gray, &gray, w as u32, h as u32, QpSet::LOSSLESS, fmt, ALL_BANDS, true,
            );
            let expected: Vec<(u8, u8, u8)> = gray.iter().map(|&v| (v, v, v)).collect();
            assert_rgb_exact(&jxr, w, h, &expected);
            // Arbitrary color content: structural + bounded chroma error at q1.
            let rp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
            let gp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
            let bp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
            let jxr = encode_color_scaled(
                &rp, &gp, &bp, w as u32, h as u32, QpSet::LOSSLESS, fmt, ALL_BANDS, true,
            );
            let d = decode(&jxr);
            assert_eq!((d.width as usize, d.height as usize, d.num_components), (w, h, 3));
            if fmt == INT_YUV444 {
                // 444 scaled q1: only the chroma half-step loses — pixel error
                // is tightly bounded (libjxr accepts this; "lossless" mode in
                // libjxr simply never uses scaled).
                for i in 0..n {
                    assert!((d.image_plane[0][i] - rp[i] as i32).abs() <= 2, "px{i}");
                    assert!((d.image_plane[1][i] - gp[i] as i32).abs() <= 2);
                    assert!((d.image_plane[2][i] - bp[i] as i32).abs() <= 2);
                }
            }
        }
    }

    /// 4b Stage B structural check: arbitrary content at 420/422 NOHIGHPASS
    /// (lossless + lossy QPs) yields streams our decoder parses to the right
    /// shape — a desync in the joint-chroma LP coding shows up here as a
    /// decode error. Pixel exactness is the harness's JxrDecApp gate.
    #[test]
    fn subsampled_nohighpass_streams_decode() {
        let mut r = Lcg(0x4242_0042_4242_0042);
        for &fmt in &[INT_YUV420, INT_YUV422] {
            for &(w, h) in &[(48usize, 32usize), (33, 17)] {
                let n = w * h;
                let rp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
                let gp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
                let bp: Vec<u8> = (0..n).map(|_| r.byte()).collect();
                for &qp in &[QpSet::LOSSLESS, QpSet { dc: 16, lp: 32, hp: 0 }] {
                    let jxr = encode_color_subsampled(
                        &rp, &gp, &bp, w as u32, h as u32, qp, fmt, NOHIGHPASS,
                    );
                    let d = decode(&jxr);
                    assert_eq!(
                        (d.width as usize, d.height as usize, d.num_components),
                        (w, h, 3),
                        "fmt={fmt} {w}x{h}"
                    );
                }
            }
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
