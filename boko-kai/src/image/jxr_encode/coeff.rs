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
