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
use std::collections::HashMap;

/// Encode-side inverse of the decoder's `decode_abs_level` (value path only;
/// the adaptive table *index* is chosen by the caller and the discriminator
/// update is the caller's responsibility across MBs). Emits `level` (`>= 2`)
/// with the abs-level Huffman `table` plus the fixed/escape suffix bits.
pub fn encode_abs_level(bw: &mut BitWriter, table: &HashMap<u64, i32>, level: i32) {
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
    }
}

/// Encode one DC coefficient for a single component with no spatial prediction
/// (e.g. the first macroblock), as `mb_dc` + `decode_dc` expect: the
/// `b_abs_level` flag, an optional abs-level VLC for the high part, the
/// `model_bits` low-part refinement, then a sign bit iff the value is nonzero.
/// `abs_table` is the abs-level Huffman table for the current adaptive index.
/// Returns `b_abs_level` (the caller folds it into the model update).
pub fn encode_dc_value(
    bw: &mut BitWriter,
    value: i32,
    model_bits: i32,
    abs_table: &HashMap<u64, i32>,
) -> bool {
    let mag = value.unsigned_abs() as i64;
    let m = model_bits as u32;
    let high = (mag >> m) as i32; // i_dc >> model_bits
    let b_abs_level = high > 0;
    bw.write_flag(b_abs_level); // mb_dc reads this before decode_dc
    if b_abs_level {
        // decode_dc computes i_dc_high = decode_abs_level() - 1, so level = high + 1.
        encode_abs_level(bw, abs_table, high + 1);
    }
    if m > 0 {
        bw.write_bits((mag & ((1i64 << m) - 1)) as u64, m);
    }
    if mag != 0 {
        bw.write_flag(value < 0);
    }
    b_abs_level
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
    use crate::image::jxr_decode::decoder::Decoder;

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
}
