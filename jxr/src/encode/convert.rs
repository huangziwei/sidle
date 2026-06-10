//! Typed (deep) encode input and the **forward sample conversion** into the
//! "pre-bias" `i32` domain the forward transform consumes. The exact inverse
//! of the decoder's output-formatting chain, stage by stage:
//! `add_bias` → `compute_scaling` → `postscaling_process` read backwards is
//! un-postscale (`>> shift_bits`), un-scale (`<< 3` when `scaled_flag`),
//! un-bias (`− (bias_base >> shift_bits) << 3·scaled`). The transform and
//! entropy core are depth-agnostic (`i32` coefficients end to end), so depth
//! only touches this conversion plus a handful of header fields ([`Depth`]).
//!
//! `shift_bits` policy is **cloned from libjxr** (verified by header-dumping
//! jxrencapp-minted files, and against `strenc.c`): 0 for BD16/BD16S, **10
//! for BD32S** (`strenc.c:785` default) — 32-bit input must shed 10 low bits
//! for `i32` transform headroom, so BD32S is never bit-lossless, even at q1
//! (the reference behaves identically). For the same headroom reason libjxr
//! forces unscaled arithmetic for 32-bit depths (`strenc.c:961`); we reject
//! `scaled` there instead of silently clearing it.

use crate::decode::consts::{BD16, BD16F, BD16S, BD32F, BD32S, BD8};

/// Typed sample planes for [`crate::encode_typed`] — one `Vec` per component
/// (R,G,B\[,A\] order, or a single gray plane), each row-major with
/// `len == width × height`, mirroring `decode::pixels::SampleType`. Each
/// variant selects one T.832 `OUTPUT_BITDEPTH` family and its container
/// pixel-format GUID family.
pub enum SamplePlanes<'a> {
    /// 8-bit unsigned (BD8) — the [`crate::ImageInput`] family
    /// (`8bppGray` / `24bppRGB` / `32bppBGRA`), routed through the identical
    /// classic path (byte-stable).
    U8(&'a [Vec<u8>]),
    /// 16-bit unsigned (BD16): `16bppGray` / `48bppRGB`. Bit-exact at
    /// lossless QP (`shift_bits = 0`; bias `1 << 15`).
    U16(&'a [Vec<u16>]),
    /// 16-bit signed fixed-point (BD16S): `16bppGrayFixed` / `48bppRGBFixed`.
    /// Bit-exact at lossless QP (no bias — the samples are already centered).
    I16(&'a [Vec<i16>]),
    /// 32-bit signed fixed-point (BD32S): `32bppGrayFixed` / `96bppRGBFixed`.
    /// **Never bit-lossless**: the low `shift_bits = 10` bits are dropped on
    /// input (transform headroom; the reference encoder does the same), so
    /// q1 round-trips `(x >> 10) << 10`. Scaled arithmetic is rejected.
    I32(&'a [Vec<i32>]),
    /// IEEE-754 **half** bit patterns (BD16F): `16bppGrayHalf` /
    /// `48bppRGBHalf`. The codestream carries halfs through a sign-magnitude
    /// integer fold, so lossless QP is **bit-pattern-exact** for every
    /// pattern — NaN payloads, infinities, denormals — except `-0.0`
    /// (`0x8000`), which normalizes to `+0.0` (the fold has a single zero;
    /// the reference encoder does the same — probed).
    F16(&'a [Vec<u16>]),
    /// IEEE-754 **single** bit patterns (BD32F): `32bppGrayFloat` /
    /// `128bppRGBFloat`. The codestream codes a custom float
    /// (`len_mantissa = 13`, `exp_bias = 4` — the reference defaults, cloned)
    /// through the same sign-magnitude fold, keeping the top 13 mantissa
    /// bits **rounded** (half-up) — the float analog of BD32S's shift_bits.
    /// Values already on that grid (e.g. anything our decoder produced from
    /// a BD32F file, incl. wild scRGB captures) round-trip bit-exactly at
    /// q1; ±0 normalize to +0, and scaled arithmetic is rejected (i32
    /// headroom, as the reference forces).
    F32(&'a [Vec<u32>]),
}

/// The BD16F sign-magnitude fold: half bits → the pseudo-integer the
/// codestream codes. Exact inverse of the decoder's Table-192 postscale
/// (`(sign << 15) | min(|x|, 32767)`).
#[inline]
fn fold_f16(h: u16) -> i32 {
    let m = (h & 0x7fff) as i32;
    if h & 0x8000 != 0 {
        -m
    } else {
        m
    }
}

/// The BD32F fold: IEEE single bits → the custom-float pseudo-integer
/// (`(E << len_mantissa) | M`, sign-magnitude) — a line-for-line clone of
/// libjxr's `Forward_Float` (strenc.c), the forward inverse of the decoder's
/// `postscale_f32`. Round-half-up on the 23−lm dropped mantissa bits (the
/// carry rolls into the exponent field, which is exactly correct); ±0 → 0.
/// One divergence: libjxr's `m >>= (1 - e1)` is C UB for magnitudes below
/// ~2⁻³⁶ (shift ≥ 32); we do the honest math (flush to zero, which the
/// decoder maps back to ±0).
fn fold_f32(bits: u32, lm: i32, eb: i32) -> i32 {
    if bits & 0x7fff_ffff == 0 {
        return 0;
    }
    let e = ((bits >> 23) & 0xff) as i32;
    let mut m = ((bits & 0x007f_ffff) | 0x0080_0000) as i32;
    let mut e_act = e;
    if e == 0 {
        m ^= 0x0080_0000; // IEEE subnormal: no implicit bit, exponent 1
        e_act = 1;
    }
    let mut e1 = e_act - 127 + eb;
    if e1 <= 1 {
        if e1 < 1 {
            let sh = 1 - e1;
            m = if sh >= 24 { 0 } else { m >> sh };
        }
        e1 = 1;
        if m & 0x0080_0000 == 0 {
            e1 = 0; // custom-subnormal
        }
    }
    m &= 0x007f_ffff;
    let h = (e1 << lm) + ((m + (1 << (23 - lm - 1))) >> (23 - lm));
    if bits >> 31 != 0 {
        -h
    } else {
        h
    }
}

impl SamplePlanes<'_> {
    /// Number of component planes supplied.
    pub fn num_planes(&self) -> usize {
        match self {
            SamplePlanes::U8(p) => p.len(),
            SamplePlanes::U16(p) => p.len(),
            SamplePlanes::I16(p) => p.len(),
            SamplePlanes::I32(p) => p.len(),
            SamplePlanes::F16(p) => p.len(),
            SamplePlanes::F32(p) => p.len(),
        }
    }

    /// Length of plane `i`.
    pub(super) fn plane_len(&self, i: usize) -> usize {
        match self {
            SamplePlanes::U8(p) => p[i].len(),
            SamplePlanes::U16(p) => p[i].len(),
            SamplePlanes::I16(p) => p[i].len(),
            SamplePlanes::I32(p) => p[i].len(),
            SamplePlanes::F16(p) => p[i].len(),
            SamplePlanes::F32(p) => p[i].len(),
        }
    }

    /// The depth descriptor this family emits.
    pub(super) fn depth(&self) -> Depth {
        match self {
            SamplePlanes::U8(_) => Depth::BD8,
            SamplePlanes::U16(_) => Depth::BD16,
            SamplePlanes::I16(_) => Depth::BD16S,
            SamplePlanes::I32(_) => Depth::BD32S,
            SamplePlanes::F16(_) => Depth::BD16F,
            SamplePlanes::F32(_) => Depth::BD32F,
        }
    }

    /// Container GUID for this family's 1-plane (gray) shape.
    pub(super) fn gray_guid(&self) -> &'static [u8; 16] {
        use super::container::pixel_format as pf;
        match self {
            SamplePlanes::U8(_) => &pf::GRAY8,
            SamplePlanes::U16(_) => &pf::GRAY16,
            SamplePlanes::I16(_) => &pf::GRAY16_FIXED,
            SamplePlanes::I32(_) => &pf::GRAY32_FIXED,
            SamplePlanes::F16(_) => &pf::GRAY16_HALF,
            SamplePlanes::F32(_) => &pf::GRAY32_FLOAT,
        }
    }

    /// Container GUID for this family's 3-plane (RGB) shape.
    pub(super) fn rgb_guid(&self) -> &'static [u8; 16] {
        use super::container::pixel_format as pf;
        match self {
            SamplePlanes::U8(_) => &pf::RGB24,
            SamplePlanes::U16(_) => &pf::RGB48,
            SamplePlanes::I16(_) => &pf::RGB48_FIXED,
            SamplePlanes::I32(_) => &pf::RGB96_FIXED,
            SamplePlanes::F16(_) => &pf::RGB48_HALF,
            SamplePlanes::F32(_) => &pf::RGB128_FLOAT,
        }
    }

    /// Forward-convert plane `i` to the pre-bias domain ([`prebias`]).
    pub(super) fn prebias_plane(&self, i: usize, scaled: bool) -> Vec<i32> {
        let d = self.depth();
        match self {
            SamplePlanes::U8(p) => prebias(p[i].iter().map(|&v| v as i32), &d, scaled),
            SamplePlanes::U16(p) => prebias(p[i].iter().map(|&v| v as i32), &d, scaled),
            SamplePlanes::I16(p) => prebias(p[i].iter().map(|&v| v as i32), &d, scaled),
            SamplePlanes::I32(p) => prebias(p[i].iter().copied(), &d, scaled),
            SamplePlanes::F16(p) => prebias(p[i].iter().map(|&v| fold_f16(v)), &d, scaled),
            SamplePlanes::F32(p) => prebias(
                p[i].iter().map(|&v| fold_f32(v, d.len_mantissa as i32, d.exp_bias)),
                &d,
                scaled,
            ),
        }
    }
}

/// `OUTPUT_BITDEPTH` plus its plane-header conversion parameters — what the
/// input family contributes to the image header (`output_bitdepth`), the
/// plane header (`shift_bits`; `len_mantissa`/`exp_bias` for floats), and the
/// forward sample conversion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Depth {
    /// T.832 `OUTPUT_BITDEPTH` code.
    pub bitdepth: u8,
    /// `SHIFT_BITS` plane-header field (BD16/BD16S/BD32S): input is
    /// pre-shifted down by this; the decoder's `PostScalingInt` shifts back.
    pub shift_bits: u32,
    /// `LEN_MANTISSA` (BD32F only).
    pub len_mantissa: u32,
    /// `EXP_BIAS` (BD32F only).
    pub exp_bias: i32,
}

impl Depth {
    pub const BD8: Depth =
        Depth { bitdepth: BD8, shift_bits: 0, len_mantissa: 0, exp_bias: 0 };
    pub const BD16: Depth =
        Depth { bitdepth: BD16, shift_bits: 0, len_mantissa: 0, exp_bias: 0 };
    pub const BD16S: Depth =
        Depth { bitdepth: BD16S, shift_bits: 0, len_mantissa: 0, exp_bias: 0 };
    /// libjxr's default pre-shift for 32-bit input (`strenc.c:785`).
    pub const BD32S: Depth =
        Depth { bitdepth: BD32S, shift_bits: 10, len_mantissa: 0, exp_bias: 0 };
    pub const BD16F: Depth =
        Depth { bitdepth: BD16F, shift_bits: 0, len_mantissa: 0, exp_bias: 0 };
    /// The reference defaults: `len_mantissa = 13` (strenc.c:790),
    /// `exp_bias = 4` (header-dumped from every jxrencapp BD32F mint).
    pub const BD32F: Depth =
        Depth { bitdepth: BD32F, shift_bits: 0, len_mantissa: 13, exp_bias: 4 };

    /// The bias the decoder's `add_bias` adds back for this depth (Table 188
    /// `bias_base`, before its `>> shift_bits` pre-shift): `1 << 7` for BD8,
    /// `1 << 15` for BD16, none for the signed depths.
    fn bias_base(&self) -> i32 {
        match self.bitdepth {
            BD8 => 1 << 7,
            BD16 => 1 << 15,
            _ => 0,
        }
    }
}

/// BD8 forward conversion — the classic `(x − 128) << 3·scaled` ingestion
/// the 8-bit drivers use ([`prebias`] at [`Depth::BD8`]).
pub(super) fn u8_prebias(p: &[u8], scaled: bool) -> Vec<i32> {
    prebias(p.iter().map(|&v| v as i32), &Depth::BD8, scaled)
}

/// Forward-convert integer samples to the pre-bias domain:
/// `((x >> shift_bits) − (bias_base >> shift_bits)) << 3·scaled`.
///
/// Inverts the decoder stage for stage: `postscaling_process` does
/// `<< shift_bits` last, so `>> shift_bits` comes first (this is where BD32S
/// sheds its low 10 bits); `compute_scaling` does `(v + rounding) >> 3` when
/// scaled (rounding 3 or 4 — both floor an exact `<< 3` back), and `add_bias`
/// adds `(bias_base >> shift_bits) << 3·scaled`. libjxr writes the same
/// values as `((x − bias_base) >> shift) << 3·scaled` (`strenc.c:1766`) —
/// identical, since `bias_base` is a multiple of `2^shift`.
fn prebias(samples: impl Iterator<Item = i32>, d: &Depth, scaled: bool) -> Vec<i32> {
    let sh = if scaled { 3 } else { 0 };
    let s = d.shift_bits;
    let bias = d.bias_base() >> s;
    samples.map(|x| ((x >> s) - bias) << sh).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decoder's output-formatting chain for one integer sample (the
    /// `add_bias` + `compute_scaling` + `postscaling_process` arms inlined),
    /// against which the forward conversion must be the exact inverse.
    fn decoder_chain(c: i32, d: &Depth, scaled: bool) -> i32 {
        let i_scale = if scaled { 3 } else { 0 };
        let bias = (d.bias_base() >> d.shift_bits) << i_scale;
        let mut v = c + bias;
        if scaled {
            // BD16 (and BD1/BD8? — Table 189: BD16 rounds with 4, the signed
            // deep depths with 3; both floor an exact `<< 3` identically).
            let rounding = if d.bitdepth == BD16 { 4 } else { 3 };
            v = (v + rounding) >> 3;
        }
        v << d.shift_bits
    }

    #[test]
    fn integer_conversions_invert_the_decoder_chain() {
        // BD16: every 16-bit value, both arithmetic modes, exact.
        for scaled in [false, true] {
            for x in (0u16..=u16::MAX).step_by(7).chain([0, 1, u16::MAX].into_iter()) {
                let c = prebias([x as i32].into_iter(), &Depth::BD16, scaled)[0];
                assert_eq!(decoder_chain(c, &Depth::BD16, scaled), x as i32, "BD16 x={x} scaled={scaled}");
            }
            for x in (i16::MIN..=i16::MAX).step_by(7).chain([i16::MIN, -1, 0, i16::MAX].into_iter()) {
                let c = prebias([x as i32].into_iter(), &Depth::BD16S, scaled)[0];
                assert_eq!(decoder_chain(c, &Depth::BD16S, scaled), x as i32, "BD16S x={x} scaled={scaled}");
            }
        }
        // BD32S: exact after the 10-bit shift quantization (never scaled).
        for x in [i32::MIN, -1_000_000_007, -1024, -1023, -1, 0, 1, 1023, 1024, 1_000_000_007, i32::MAX] {
            let c = prebias([x].into_iter(), &Depth::BD32S, false)[0];
            assert_eq!(decoder_chain(c, &Depth::BD32S, false), (x >> 10) << 10, "BD32S x={x}");
        }
        // BD8 reproduces the classic ingestion expression.
        for scaled in [false, true] {
            let sh = if scaled { 3 } else { 0 };
            for x in 0u8..=255 {
                let c = prebias([x as i32].into_iter(), &Depth::BD8, scaled)[0];
                assert_eq!(c, (x as i32 - 128) << sh, "BD8 x={x} scaled={scaled}");
                assert_eq!(decoder_chain(c, &Depth::BD8, scaled), x as i32);
            }
        }
    }
}
