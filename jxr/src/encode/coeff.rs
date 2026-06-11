//! Coefficient-coding primitives (encode side).
//!
//! These mirror the decoder's per-coefficient codecs. Unlike the transform and
//! static-VLC layers, most of the coefficient path (`decode_block`,
//! `decode_abs_level`, the `mb_*` methods) is **private and stateful** on the
//! decoder — it's validated end-to-end by decoding a full codestream, not
//! per-primitive. The pieces here are the genuinely self-contained ones that
//! touch only the bitstream, so they round-trip against the real decoder
//! directly.

use super::bitstream::BitWriter;
use crate::decode::state::AdaptiveVLC;
use crate::decode::tables;
use std::collections::HashMap;

/// Encode-side inverse of the decoder's `decode_run`: emit a run length `run`
/// in `1..=i_max_run` (`i_max_run` in `1..=14`). Run 0 is signalled by a flag
/// in `decode_block`, not here.
pub fn encode_run(bw: &mut BitWriter, run: u32, i_max_run: u32) {
    use super::entropy::write_huff;
    const REMAP: [u32; 15] = [1, 2, 3, 5, 7, 1, 2, 3, 5, 7, 1, 2, 3, 4, 5];
    const FIXED: [u32; 15] = [0, 0, 1, 1, 3, 0, 0, 1, 1, 2, 0, 0, 0, 0, 1];
    const BINX: [usize; 10] = [10, 10, 5, 5, 5, 5, 0, 0, 0, 0];
    debug_assert!((1..=14).contains(&i_max_run) && (1..=i_max_run).contains(&run));
    if i_max_run < 5 {
        if i_max_run != 1 {
            // i_max_run == 1 ⇒ run is implicitly 1 (no bits).
            write_huff(bw, tables::run_value(i_max_run as usize), run as i32);
        }
    } else {
        let binx = BINX[(i_max_run - 5) as usize];
        // The five candidate indices [binx, binx+4] partition the run range.
        for k in 0..5usize {
            let idx = binx + k;
            let lo = REMAP[idx];
            let hi = lo + (1u32 << FIXED[idx]) - 1;
            if run >= lo && run <= hi {
                write_huff(bw, tables::run_index(), k as i32);
                if FIXED[idx] > 0 {
                    bw.write_bits((run - lo) as u64, FIXED[idx]);
                }
                return;
            }
        }
        unreachable!("run {run} not covered for i_max_run {i_max_run}");
    }
}

/// Encode-side inverse of the decoder's `decode_abs_level` (value path only;
/// the adaptive table *index* is chosen by the caller). Emits `level` (`>= 2`)
/// with the abs-level Huffman `table` plus the fixed/escape suffix bits, and
/// returns the `abs_level_index` (0..=6) the caller folds into the
/// discriminator (`ABS_LEVEL_INDEX_DELTA`).
pub fn encode_abs_level(bw: &mut BitWriter, table: &HashMap<u64, i32>, level: i32) -> i32 {
    use super::entropy::write_huff;
    const REMAP: [i32; 6] = [2, 3, 4, 6, 10, 14];
    const FIXED: [u32; 6] = [0, 0, 1, 2, 2, 2];
    debug_assert!(level >= 2);
    if level <= 17 {
        let idx = match level {
            2 => 0,
            3 => 1,
            4..=5 => 2,
            6..=9 => 3,
            10..=13 => 4,
            _ => 5, // 14..=17
        };
        write_huff(bw, table, idx as i32);
        if FIXED[idx] > 0 {
            bw.write_bits((level - REMAP[idx]) as u64, FIXED[idx]);
        }
        idx as i32
    } else {
        write_huff(bw, table, 6); // escape index
        // i_fixed = floor(log2(level - 2)); level = 2 + 2^i_fixed + extra.
        let mut i_fixed = 4u32;
        while 2 + (1i64 << (i_fixed + 1)) <= level as i64 {
            i_fixed += 1;
        }
        // Nested length code: 4 bits (i_fixed-4), escaping at 19 (+2 bits) and 22 (+3 bits).
        if i_fixed <= 18 {
            bw.write_bits((i_fixed - 4) as u64, 4);
        } else if i_fixed <= 21 {
            bw.write_bits(15, 4);
            bw.write_bits((i_fixed - 19) as u64, 2);
        } else {
            bw.write_bits(15, 4);
            bw.write_bits(3, 2);
            bw.write_bits((i_fixed - 22) as u64, 3);
        }
        let extra = level as i64 - 2 - (1i64 << i_fixed);
        bw.write_bits(extra as u64, i_fixed);
        6
    }
}

/// Encode-side inverse of the decoder's `decode_block` (the run-level
/// orchestration). `pairs` are `(run, level)` for the nonzero **coarse** levels
/// in scan order (run = zeros before each). Emits the index/flag packing
/// (first_index / index_a / index_b), runs, signs, and abs-levels, updating the
/// adaptive discriminators exactly as the decoder does. The caller adapts the
/// VLC tables (`adapt_table2`/`adapt_table1`) after the MB and selects them by
/// band/chroma.
#[allow(clippy::too_many_arguments)]
pub fn encode_block(
    bw: &mut BitWriter,
    pairs: &[(u32, i32)],
    i_location: usize,
    first_ind: &mut AdaptiveVLC,
    ind0: &mut AdaptiveVLC,
    ind1: &mut AdaptiveVLC,
    abs0: &mut AdaptiveVLC,
    abs1: &mut AdaptiveVLC,
) {
    use super::entropy::write_huff;
    use crate::decode::consts::{
        ABS_LEVEL_INDEX_DELTA, FIRST_INDEX_DELTA, INDEX1_DELTA,
    };
    debug_assert!(!pairs.is_empty());

    let abs_one = |bw: &mut BitWriter, abs: &mut AdaptiveVLC, mag: i32| {
        let idx = encode_abs_level(bw, tables::abs_level_index(abs.table_index as usize), mag);
        abs.discrim_val1 += ABS_LEVEL_INDEX_DELTA[0][idx as usize];
    };

    // --- first index ---
    let (r0, l0) = pairs[0];
    let run_is_zero = (r0 == 0) as i32;
    let level_is_not_1 = (l0.abs() != 1) as i32;
    let (n_imm, n_aft) = next_flags(pairs, 0);
    let first_index = run_is_zero + 2 * level_is_not_1 + 4 * n_imm + 8 * n_aft;
    write_huff(bw, tables::first_index(first_ind.table_index as usize), first_index);
    first_ind.discrim_val1 += FIRST_INDEX_DELTA[first_ind.delta_table_index as usize][first_index as usize];
    first_ind.discrim_val2 += FIRST_INDEX_DELTA[first_ind.delta2_table_index as usize][first_index as usize];
    let mut i_context = run_is_zero & n_imm;
    bw.write_flag(l0 < 0);
    if level_is_not_1 != 0 {
        abs_one(bw, if i_context != 0 { abs1 } else { abs0 }, l0.abs());
    }
    if run_is_zero == 0 {
        encode_run(bw, r0, (15 - i_location) as u32);
    }
    let mut i_loc = i_location + r0 as usize + 1;
    let (mut nis, mut nar) = (n_imm, n_aft);

    // --- subsequent indices ---
    let mut j = 1usize;
    while nis != 0 || nar != 0 {
        let (rj, lj) = pairs[j];
        if nis == 0 {
            encode_run(bw, rj, (15 - i_loc) as u32);
        }
        i_loc += rj as usize + 1;
        let lin1 = (lj.abs() != 1) as i32;
        let (ni, na) = next_flags(pairs, j);
        let i_index = lin1 + 2 * ni + 4 * na;
        if i_loc < 15 {
            let ind = if i_context != 0 { &mut *ind1 } else { &mut *ind0 };
            write_huff(bw, tables::index_a(ind.table_index as usize), i_index);
            ind.discrim_val1 += INDEX1_DELTA[ind.delta_table_index as usize][i_index as usize];
            ind.discrim_val2 += INDEX1_DELTA[ind.delta2_table_index as usize][i_index as usize];
        } else if i_loc == 15 {
            write_huff(bw, tables::index_b(), i_index);
        } else {
            bw.write_bits(i_index as u64, 1);
        }
        i_context &= ni;
        bw.write_flag(lj < 0);
        if lin1 != 0 {
            abs_one(bw, if i_context != 0 { abs1 } else { abs0 }, lj.abs());
        }
        nis = ni;
        nar = na;
        j += 1;
    }
}

/// Next-coefficient flags for `pairs[j]`: `(next_is_immediate, next_after_run)`.
fn next_flags(pairs: &[(u32, i32)], j: usize) -> (i32, i32) {
    if j + 1 < pairs.len() {
        if pairs[j + 1].0 == 0 { (1, 0) } else { (0, 1) }
    } else {
        (0, 0)
    }
}

/// Encode one DC coefficient `value` (already a prediction residual) for a
/// single component, as `mb_dc` + `decode_dc` expect: the `b_abs_level` flag,
/// an optional abs-level VLC for the high part, the `model_bits` low-part
/// refinement, then a sign bit iff nonzero. Returns `(b_abs_level, abs_index)`
/// where `abs_index` is `-1` when `b_abs_level` is false.
pub fn encode_dc_value(
    bw: &mut BitWriter,
    value: i32,
    model_bits: i32,
    abs_table: &HashMap<u64, i32>,
) -> (bool, i32) {
    let high = (value.unsigned_abs() as i64 >> model_bits as u32) as i32;
    let b_abs_level = high > 0;
    bw.write_flag(b_abs_level); // mb_dc reads this before decode_dc (grayscale)
    let abs_index = encode_dc_residual(bw, value, model_bits, b_abs_level, abs_table);
    (b_abs_level, abs_index)
}

/// Flag-less DC value body: emit the abs-level VLC (iff `b_abs_level`), the
/// `model_bits` low part, then the sign (iff nonzero). Shared by grayscale
/// ([`encode_dc_value`], which writes the per-component `b_abs` flag first) and
/// **color**, where the three components' `b_abs` flags are bundled into one
/// `val_dc_yuv` symbol written before the components (so no per-component flag).
/// Returns the `abs_level_index` (`-1` if `!b_abs_level`).
pub fn encode_dc_residual(
    bw: &mut BitWriter,
    value: i32,
    model_bits: i32,
    b_abs_level: bool,
    abs_table: &HashMap<u64, i32>,
) -> i32 {
    let mag = value.unsigned_abs() as i64;
    let m = model_bits as u32;
    let mut abs_index = -1;
    if b_abs_level {
        let high = (mag >> m) as i32;
        debug_assert!(high > 0);
        // decode_dc computes i_dc_high = decode_abs_level() - 1, so level = high + 1.
        abs_index = encode_abs_level(bw, abs_table, high + 1);
    }
    if m > 0 {
        bw.write_bits((mag & ((1i64 << m) - 1)) as u64, m);
    }
    if mag != 0 {
        bw.write_flag(value < 0);
    }
    abs_index
}

/// One model's `m_bits`/`m_state`, mirroring `decode::state::Model`.
#[derive(Clone, Copy)]
pub struct ModelState {
    pub m_bits: i32,
    pub m_state: i32,
}

impl ModelState {
    /// `initialize_model_mb(band)`: `m_bits = max((2-band)*4, 0)`, state 0.
    pub fn init(band: i32) -> Self {
        Self {
            m_bits: ((2 - band) * 4).max(0),
            m_state: 0,
        }
    }

    /// Mirror of `update_model_mb` for a single-model (YONLY) plane. `lap_mean`
    /// is the count of abs-level escapes this MB; `band` selects the weight
    /// (0=DC, 1=LP, 2=HP).
    pub fn update(&mut self, lap_mean: i32, band: i32) {
        // `update_model_mb` weights: i_lap_mean[0] *= i_weight0[band]. The HP
        // `>>4` in the decoder applies only to i_lap_mean[1] (the second model),
        // which a single-model (grayscale/YONLY) plane never uses.
        const W0: [i32; 3] = [240, 12, 1];
        let lap = lap_mean * W0[band as usize];
        let i_model_weight = 70;
        let i_delta = (lap - i_model_weight) >> 2;
        if i_delta <= -8 {
            let d = (i_delta + 4).max(-16);
            self.m_state += d;
            if self.m_state < -8 {
                if self.m_bits == 0 {
                    self.m_state = -8;
                } else {
                    self.m_state = 0;
                    self.m_bits -= 1;
                }
            }
        } else if i_delta >= 8 {
            let d = (i_delta - 4).min(15);
            self.m_state += d;
            if self.m_state > 8 {
                if self.m_bits >= 15 {
                    self.m_bits = 15;
                    self.m_state = 8;
                } else {
                    self.m_state = 0;
                    self.m_bits += 1;
                }
            }
        }
    }
}

/// Two-model `m_bits`/`m_state` (luma + chroma) for a **color** plane, mirroring
/// `decode::state::Model` + `Decoder::update_model_mb` with `i_num_models =
/// 2`. Index 0 = luma, index 1 = chroma. (Grayscale uses the single-model
/// [`ModelState`].)
#[derive(Clone, Copy)]
pub struct ColorModel {
    pub m_bits: [i32; 2],
    pub m_state: [i32; 2],
}

impl ColorModel {
    /// `initialize_model_mb(band)`: `m_bits = max((2-band)*4, 0)` for both models.
    pub fn init(band: i32) -> Self {
        let mb = ((2 - band) * 4).max(0);
        Self { m_bits: [mb; 2], m_state: [0; 2] }
    }

    /// Mirror of `update_model_mb` for a 2-model plane. `lap` is the per-model
    /// abs-level escape count this MB (`lap[0]` luma, `lap[1]` chroma); `band`
    /// is 0=DC/1=LP/2=HP; `num_components` selects the chroma weight column.
    /// This is the Table-116 "else" arm (444/YUVK/NCOMPONENT); subsampled
    /// chroma uses [`Self::update_42x`].
    pub fn update(&mut self, mut lap: [i32; 2], band: usize, num_components: usize) {
        const W0: [i32; 3] = [240, 12, 1];
        const W1: [[i32; 16]; 3] = [
            [0, 240, 120, 80, 60, 48, 40, 34, 30, 27, 24, 22, 20, 18, 17, 16],
            [0, 12, 6, 4, 3, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1],
            [0, 16, 8, 5, 4, 3, 3, 2, 2, 2, 2, 1, 1, 1, 1, 1],
        ];
        lap[0] *= W0[band];
        lap[1] *= W1[band][num_components - 1];
        if band == 2 {
            lap[1] >>= 4; // HP: the decoder's `>>4` applies only to model 1
        }
        self.adapt_models(lap);
    }

    /// Table-116 chroma weights for the JOINTLY-coded 420/422 chroma "plane"
    /// (`iWeight2`, no `>>4` on HP) — mirrors `decoder.rs update_model_mb`'s
    /// `INT_YUV420`/`INT_YUV422` arms. This was the Phase-2 hunted bug in the
    /// decode direction; the encoder must diverge identically.
    pub fn update_42x(&mut self, mut lap: [i32; 2], band: usize, is_420: bool) {
        const W0: [i32; 3] = [240, 12, 1];
        const I_WEIGHT2: [i32; 6] = [120, 37, 2, 120, 18, 1];
        lap[0] *= W0[band];
        lap[1] *= I_WEIGHT2[if is_420 { 0 } else { 3 } + band];
        self.adapt_models(lap);
    }

    fn adapt_models(&mut self, lap: [i32; 2]) {
        let i_model_weight = 70;
        for j in 0..2 {
            let i_delta = (lap[j] - i_model_weight) >> 2;
            if i_delta <= -8 {
                self.m_state[j] += (i_delta + 4).max(-16);
                if self.m_state[j] < -8 {
                    if self.m_bits[j] == 0 {
                        self.m_state[j] = -8;
                    } else {
                        self.m_state[j] = 0;
                        self.m_bits[j] -= 1;
                    }
                }
            } else if i_delta >= 8 {
                self.m_state[j] += (i_delta - 4).min(15);
                if self.m_state[j] > 8 {
                    if self.m_bits[j] >= 15 {
                        self.m_bits[j] = 15;
                        self.m_state[j] = 8;
                    } else {
                        self.m_state[j] = 0;
                        self.m_bits[j] += 1;
                    }
                }
            }
        }
    }
}

/// One adaptive-VLC table-1 selector, mirroring `decode::state::AdaptiveVLC`
/// `init_table1` / `adapt_table1`.
#[derive(Clone, Copy, Default)]
pub struct AdaptiveVlc1 {
    pub table_index: u32,
    pub discrim: i32,
}

impl AdaptiveVlc1 {
    pub fn adapt(&mut self) {
        const MAX: u32 = 1;
        if self.discrim < -8 && self.table_index != 0 {
            self.table_index -= 1;
            self.discrim = 0;
        } else if self.discrim > 8 && self.table_index != MAX {
            self.table_index += 1;
            self.discrim = 0;
        } else {
            self.discrim = self.discrim.clamp(-64, 64);
        }
    }
}

/// Encode-side inverse of the decoder's `refine_lp`.
///
/// The decoder reads `model_bits` refinement bits (`coeff_ref`) and folds them
/// into a coarse coefficient `i_coeff` to produce the final `result`:
/// - `i_coeff > 0`: `result = (i_coeff << mb) + coeff_ref`
/// - `i_coeff < 0`: `result = (i_coeff << mb) - coeff_ref`
/// - `i_coeff == 0`: `coeff_ref` is the magnitude; an optional sign bit follows
///   iff it's non-zero.
///
/// Encode one HP **flexbits** refinement under `trim_flexbits`: emit the top
/// `model_bits - trim` bits of the `model_bits`-wide refinement (the decoder
/// reconstructs `flex << trim`), with the sign carried only when the coarse
/// coefficient is zero AND the trimmed refinement is nonzero. `trim = 0`
/// matches the plain refinement bit-for-bit.
pub fn encode_flexbits(bw: &mut BitWriter, i_coeff: i32, result: i32, model_bits: i32, trim: u32) {
    let left = model_bits - trim as i32;
    if left <= 0 {
        return;
    }
    let low = result.unsigned_abs() & ((1u32 << model_bits) - 1);
    let flex_ref = (low >> trim) as i32;
    bw.write_bits(flex_ref as u64, left as u32);
    if i_coeff == 0 && flex_ref != 0 {
        bw.write_flag(result < 0);
    }
}

/// Given the coarse `i_coeff` (known from the quantized levels) and the final
/// `result`, we recover and emit `coeff_ref` (+ sign).
pub fn encode_refine_lp(bw: &mut BitWriter, i_coeff: i32, result: i32, model_bits: i32) {
    let mb = model_bits as u32;
    if i_coeff > 0 {
        let coeff_ref = result.wrapping_sub(i_coeff << model_bits);
        bw.write_bits(coeff_ref as u64, mb);
    } else if i_coeff < 0 {
        let coeff_ref = (i_coeff << model_bits).wrapping_sub(result);
        bw.write_bits(coeff_ref as u64, mb);
    } else {
        bw.write_bits(result.unsigned_abs() as u64, mb);
        if result != 0 {
            bw.write_flag(result < 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decoder::Decoder;

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

    #[test]
    fn refine_lp_roundtrips_via_decoder() {
        let mut r = Lcg(0xfeed_face);
        for _ in 0..20000 {
            let mb = (r.next() % 8 + 1) as i32; // 1..=8 model bits
            let i_coeff = (r.next() % 21) as i32 - 10; // -10..=10 coarse level

            // Craft exactly the bits the decoder consumes, then decode to get a
            // `result` that is valid for (i_coeff, mb).
            let coeff_ref = (r.next() & ((1u64 << mb) - 1)) as i32;
            let mut input = BitWriter::new();
            input.write_bits(coeff_ref as u64, mb as u32);
            if i_coeff == 0 && coeff_ref != 0 {
                input.write_flag(r.next() & 1 == 1);
            }
            let input = input.finish();
            let result = Decoder::new(&input).refine_lp(i_coeff, mb).unwrap();

            // Re-encode `result` and confirm it decodes back identically.
            let mut bw = BitWriter::new();
            encode_refine_lp(&mut bw, i_coeff, result, mb);
            let bytes = bw.finish();
            let back = Decoder::new(&bytes).refine_lp(i_coeff, mb).unwrap();
            assert_eq!(back, result, "i_coeff={i_coeff} mb={mb} result={result}");
        }
    }

    #[test]
    fn run_roundtrips_via_decoder() {
        for i_max_run in 1..=14u32 {
            for run in 1..=i_max_run {
                let mut bw = BitWriter::new();
                encode_run(&mut bw, run, i_max_run);
                let bytes = bw.finish();
                let got = Decoder::new(&bytes).decode_run(i_max_run).unwrap();
                assert_eq!(got, run, "i_max_run={i_max_run} run={run}");
            }
        }
    }
}
