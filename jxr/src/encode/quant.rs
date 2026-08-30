//! Forward quantization: transform coefficient → quantized level. The exact

use crate::decode::consts::{DC, HP, LP};
use crate::decode::state::quant_map;

/// Per-band quantizers for one grayscale plane (the QP *bytes* written into the
/// plane header; `quant_map` turns each into a scaling factor).
#[derive(Debug, Clone, Copy)]
pub struct QpSet {
    /// DC-band quantizer byte (0 = lossless).
    pub dc: u8,
    /// LP-band quantizer byte.
    pub lp: u8,
    /// HP-band (+flexbits) quantizer byte.
    pub hp: u8,
}

impl QpSet {
    /// Lossless (every band QP = 0 ⇒ scaling factor 1).
    pub const LOSSLESS: QpSet = QpSet {
        dc: 0,
        lp: 0,
        hp: 0,
    };
}

/// Per-band, per-component-class scaling factors at quantizer set `qp` — the
pub fn scaling_factors_for(qp: QpSet, chroma: bool, scaled: bool) -> (i32, i32, i32) {
    let comp = usize::from(chroma);
    (
        quant_map(qp.dc as u32, comp, scaled, DC).unwrap_or(1),
        quant_map(qp.lp as u32, comp, scaled, LP).unwrap_or(1),
        quant_map(qp.hp as u32, comp, scaled, HP).unwrap_or(1),
    )
}

/// One band's per-component QP bytes `(Y, U, V)`. The component mode is
/// DERIVED on emission: all equal ⇒ `COMP_UNIFORM` (one byte), `U == V` ⇒
/// `COMP_SEPARATE` (luma + chroma bytes), else `COMP_INDEPENDENT` (three).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BandQp(pub [u8; 3]);

impl BandQp {
    /// All components at one QP byte.
    pub fn uniform(q: u8) -> Self {
        BandQp([q, q, q])
    }
    /// Luma + shared chroma.
    pub fn separate(luma: u8, chroma: u8) -> Self {
        BandQp([luma, chroma, chroma])
    }
}

/// One tile's QP sets: a single DC set, and 1–16 LP/HP sets — more than one
/// makes the band per-MB DQUANT (each MB picks its set by index).
#[derive(Clone, Debug)]
pub struct TileQps {
    /// The tile's single DC set.
    pub dc: BandQp,
    /// 1–16 LP sets (more than one ⇒ per-MB DQUANT on the LP band).
    pub lp: Vec<BandQp>,
    /// 1–16 HP sets (more than one ⇒ per-MB DQUANT on the HP band).
    pub hp: Vec<BandQp>,
}

/// The 4e quantization plan for the primary plane: per-tile QP sets (one
#[derive(Clone, Debug)]
pub struct QpPlan {
    /// One entry (image-uniform) or one per tile, raster order.
    pub tiles: Vec<TileQps>,
    /// Per-MB LP set indices over the window-padded MB grid
    /// (`mby * mb_cols + mbx`); empty = every MB uses set 0.
    pub lp_index: Vec<u8>,
    /// Per-MB HP set indices, as [`Self::lp_index`].
    pub hp_index: Vec<u8>,
}

impl QpPlan {
    /// The classic single-set plan (optionally with separate chroma bytes).
    pub fn uniform(qp: QpSet, chroma_qp: Option<QpSet>) -> Self {
        let c = chroma_qp.unwrap_or(qp);
        QpPlan {
            tiles: vec![TileQps {
                dc: BandQp::separate(qp.dc, c.dc),
                lp: vec![BandQp::separate(qp.lp, c.lp)],
                hp: vec![BandQp::separate(qp.hp, c.hp)],
            }],
            lp_index: Vec::new(),
            hp_index: Vec::new(),
        }
    }
    /// LP sets per tile (the image-level `num_lp_qps` shape).
    pub fn num_lp_qps(&self) -> usize {
        self.tiles[0].lp.len()
    }
    /// HP sets per tile (the image-level `num_hp_qps` shape).
    pub fn num_hp_qps(&self) -> usize {
        self.tiles[0].hp.len()
    }
}

/// Scaling factor for one component at one QP byte (the decoder's
/// `quant_map`, mode- and component-aware).
pub fn component_scaling_factor(q: u8, component: usize, scaled: bool, band: u8) -> i32 {
    quant_map(q as u32, component, scaled, band).unwrap_or(1)
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

/// Test-only single-band shorthand for the unit tests.
#[cfg(test)]
fn scaling_factor(qp: u8) -> i32 {
    quant_map(qp as u32, 0, false, DC).unwrap_or(1)
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
                assert_eq!(
                    quantize(level * sf, sf),
                    level,
                    "qp={qp} sf={sf} level={level}"
                );
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
                assert!(
                    (c - recon).abs() <= sf,
                    "qp={qp} sf={sf} c={c} recon={recon}"
                );
            }
        }
    }
}
