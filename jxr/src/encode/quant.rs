//! Forward quantization: transform coefficient → quantized level. The exact
//! inverse of the decoder's dequant (`level * scaling_factor`). Ported from
//! libjxr `strPredQuant.c` (`remapQP`, `QUANT`): a uniform scalar quantizer
//! with a 3/8 deadzone. The scaling factor itself is taken straight from the
//! decoder's `quant_map`, so encode/decode can never disagree on it.
//!
//! Quantize the *raw* coefficients (DC/LP/HP) right after the forward transform;
//! all band prediction then runs in the level domain. Because the prediction is
//! linear and the per-band scaling is uniform, the decoder's
//! dequantize-then-predict telescopes to exactly `level * sf` — open-loop, no
//! drift (lossless `sf == 1` is the identity special case).

use crate::decode::consts::DC;
use crate::decode::state::quant_map;

/// Per-band quantizers for one grayscale plane (the QP *bytes* written into the
/// plane header; `quant_map` turns each into a scaling factor).
#[derive(Debug, Clone, Copy)]
pub struct QpSet {
    pub dc: u8,
    pub lp: u8,
    pub hp: u8,
}

impl QpSet {
    /// Lossless (every band QP = 0 ⇒ scaling factor 1).
    pub const LOSSLESS: QpSet = QpSet { dc: 0, lp: 0, hp: 0 };
}

/// Scaling factor for a grayscale (luma / component 0) plane at quantizer `qp`,
/// non-scaled arithmetic. `qp == 0` ⇒ 1. Identical to what the decoder derives,
/// so `level * scaling_factor(qp)` is the exact dequantization. (`band` is
/// irrelevant for component 0 with `scaled_flag = false`.)
pub fn scaling_factor(qp: u8) -> i32 {
    quant_map(qp as u32, 0, false, DC).unwrap_or(1)
}

/// Quantize one coefficient to a level: `sign(c) * ((|c| + offset) / sf)` with
/// libjxr's deadzone `offset = (sf*3 + 1) >> 3`. `sf <= 1` ⇒ identity.
#[inline]
pub fn quantize(coeff: i32, sf: i32) -> i32 {
    if sf <= 1 {
        return coeff;
    }
    let offset = (sf * 3 + 1) >> 3;
    let m = coeff >> 31; // sign mask: 0 if >=0, -1 if <0
    let abs = (coeff ^ m) - m;
    let level = (abs + offset) / sf;
    (level ^ m) - m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossless_is_identity() {
        assert_eq!(scaling_factor(0), 1);
        for c in [-5000, -1, 0, 1, 127, 9999] {
            assert_eq!(quantize(c, 1), c);
        }
    }

    #[test]
    fn requantizing_a_dequantized_level_is_stable() {
        // The idempotence that makes encode∘decode∘encode a fixpoint:
        // quantize(level * sf) == level for every level (offset < sf).
        for qp in 1u8..=80 {
            let sf = scaling_factor(qp);
            for level in [-300i32, -17, -1, 0, 1, 5, 42, 300] {
                assert_eq!(quantize(level * sf, sf), level, "qp={qp} sf={sf} level={level}");
            }
        }
    }

    #[test]
    fn quantize_matches_decoder_dequant_direction() {
        // |c - dequant(quantize(c))| stays within the quantizer step.
        for qp in [4u8, 8, 16, 24, 40, 64] {
            let sf = scaling_factor(qp);
            for c in [-3000i32, -513, -1, 0, 1, 200, 1234, 5000] {
                let recon = quantize(c, sf) * sf;
                assert!((c - recon).abs() <= sf, "qp={qp} sf={sf} c={c} recon={recon}");
            }
        }
    }
}
