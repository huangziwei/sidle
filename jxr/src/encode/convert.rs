//! Typed (deep) encode input and the **forward sample conversion** into the
//! "pre-bias" `i32` domain the forward transform consumes. The exact inverse
//! of the decoder's output-formatting chain, stage by stage:
//! `add_bias` → `compute_scaling` → `postscaling_process` read backwards is
//! un-postscale (`>> shift_bits`), un-scale (`<< 3` when `scaled_flag`),
//! un-bias (`− (bias_base >> shift_bits) << 3·scaled`). The transform and
//! entropy core are depth-agnostic (`i32` coefficients end to end), so depth
//! only touches this conversion plus a handful of header fields ([`Depth`]).
//!
//! `shift_bits` policy is **cloned from libjxr**: 0 for BD16/BD16S, **10
//! for BD32S** (`strenc.c:785` default) — 32-bit input must shed 10 low bits
//! for `i32` transform headroom, so BD32S is never bit-lossless, even at q1
//! (the reference behaves identically). For the same headroom reason libjxr
//! forces unscaled arithmetic for 32-bit depths (`strenc.c:961`); we reject
//! `scaled` there instead of silently clearing it.

use crate::decode::consts::{
    BD1BLACK1, BD1WHITE1, BD5, BD8, BD10, BD16, BD16F, BD16S, BD32F, BD32S, BD565,
};

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
    /// Radiance shared-exponent **RGBE** (`32bppRGBE`, BD8 + `OUT_RGBE`):
    /// exactly 4 byte planes — R, G, B mantissas and the shared exponent E.
    /// Each channel renormalizes against E on the way in (the reference's
    /// `forwardRGBE`, half-bit imputation included), so **normalized** RGBE
    /// (max mantissa ≥ 128, the .hdr convention) round-trips all four planes
    /// byte-exact at lossless QP; unnormalized pixels keep their VALUE but
    /// re-emerge renormalized. Chroma subsampling is rejected (the shared
    /// exponent couples the channels per pixel; the reference refuses too).
    Rgbe(&'a [Vec<u8>]),
    /// Packed 5-5-5 RGB words (`16bppRGB555`, BD5): ONE plane of u16 words,
    /// channel 0 in the low 5 bits (the decode side's layout). Lossless QP
    /// round-trips the packed words exactly.
    Packed555(&'a [Vec<u16>]),
    /// Packed 5-6-5 RGB words (`16bppRGB565`, BD565): ONE plane of u16
    /// words, channel 0 low, the 6-bit channel in the middle. The 5-bit
    /// channels code at doubled amplitude (the decoder's extra `>>1` on
    /// non-green channels — mirrored exactly).
    Packed565(&'a [Vec<u16>]),
    /// Packed 10-10-10 RGB words (`32bppRGB101010`, BD10): ONE plane of u32
    /// words, channel 0 in the low 10 bits.
    Packed101010(&'a [Vec<u32>]),
    /// Bi-level (`BlackWhite` GUID, BD1WHITE1 — stored 1 = white): ONE plane
    /// of 0/1 bytes. Values above 1 are rejected. For the BLACK-is-1
    /// convention use [`SamplePlanes::BwBlackIsOne`].
    Bw(&'a [Vec<u8>]),
    /// [`SamplePlanes::Bw`] with the BD1BLACK1 polarity (stored 1 = black);
    /// same single GUID, the polarity lives in `OUTPUT_BITDEPTH`.
    BwBlackIsOne(&'a [Vec<u8>]),
}

/// The BD16F sign-magnitude fold: half bits → the pseudo-integer the
/// codestream codes. Exact inverse of the decoder's Table-192 postscale
/// (`(sign << 15) | min(|x|, 32767)`).
#[inline]
fn fold_f16(h: u16) -> i32 {
    let m = (h & 0x7fff) as i32;
    if h & 0x8000 != 0 { -m } else { m }
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
    if bits >> 31 != 0 { -h } else { h }
}

/// The RGBE per-channel fold (libjxr `forwardRGBE`, strenc.c:315): mantissa
/// byte + shared exponent → the `(e << 7) | m` pseudo-log value the
/// codestream codes. A sub-128 mantissa is left-normalized against E with a
/// **half-bit imputed on the first shift** — the decoder's
/// `(2x + 1) >> (diff + 1)` recovery then reproduces the source byte
/// exactly.
fn fold_rgbe(mut m: i32, e: i32) -> i32 {
    if e == 0 {
        return 0;
    }
    let mut e = e - 1;
    let mut append = 1;
    while m & 0x80 == 0 && e > 0 {
        m = (m << 1) + append;
        append = 0;
        e -= 1;
    }
    if e == 0 {
        m
    } else {
        (m & 0x7f) + ((e + 1) << 7)
    }
}

/// Forward-convert the four RGBE planes to the three pre-bias channel
/// planes (the shared E plane folds into each channel; the decoder's
/// PostScalingF2 re-derives it as the per-pixel max exponent).
pub(super) fn rgbe_prebias(planes: &[Vec<u8>], scaled: bool) -> (Vec<i32>, Vec<i32>, Vec<i32>) {
    let sh = if scaled { 3 } else { 0 };
    let conv = |k: usize| -> Vec<i32> {
        planes[k]
            .iter()
            .zip(planes[3].iter())
            .map(|(&m, &e)| fold_rgbe(m as i32, e as i32) << sh)
            .collect()
    };
    (conv(0), conv(1), conv(2))
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
            SamplePlanes::Rgbe(p) => p.len(),
            SamplePlanes::Packed555(p) => p.len(),
            SamplePlanes::Packed565(p) => p.len(),
            SamplePlanes::Packed101010(p) => p.len(),
            SamplePlanes::Bw(p) => p.len(),
            SamplePlanes::BwBlackIsOne(p) => p.len(),
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
            SamplePlanes::Rgbe(p) => p[i].len(),
            SamplePlanes::Packed555(p) => p[i].len(),
            SamplePlanes::Packed565(p) => p[i].len(),
            SamplePlanes::Packed101010(p) => p[i].len(),
            SamplePlanes::Bw(p) => p[i].len(),
            SamplePlanes::BwBlackIsOne(p) => p[i].len(),
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
            SamplePlanes::Rgbe(_) => Depth::BD8,
            SamplePlanes::Packed555(_) => Depth::BD5,
            SamplePlanes::Packed565(_) => Depth::BD565,
            SamplePlanes::Packed101010(_) => Depth::BD10,
            SamplePlanes::Bw(_) => Depth::BD1_WHITE1,
            SamplePlanes::BwBlackIsOne(_) => Depth::BD1_BLACK1,
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
            SamplePlanes::Rgbe(_) => &pf::RGBE32, // unreachable: RGBE is 4-plane only
            SamplePlanes::Bw(_) | SamplePlanes::BwBlackIsOne(_) => &pf::BLACKWHITE,
            SamplePlanes::Packed555(_) => &pf::RGB555, // unreachable: packed is color
            SamplePlanes::Packed565(_) => &pf::RGB565,
            SamplePlanes::Packed101010(_) => &pf::RGB101010,
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
            SamplePlanes::Rgbe(_) => &pf::RGBE32,
            SamplePlanes::Packed555(_) => &pf::RGB555,
            SamplePlanes::Packed565(_) => &pf::RGB565,
            SamplePlanes::Packed101010(_) => &pf::RGB101010,
            SamplePlanes::Bw(_) | SamplePlanes::BwBlackIsOne(_) => &pf::BLACKWHITE,
        }
    }

    /// Container GUID for this family's 4-plane (RGB + alpha) shape;
    /// `None` when the GUID family has no such format (no premultiplied
    /// variant exists for the fixed/half families, and RGBE has no alpha).
    pub(super) fn rgba_guid(&self, premultiplied: bool) -> Option<&'static [u8; 16]> {
        use super::container::pixel_format as pf;
        match (self, premultiplied) {
            (SamplePlanes::U8(_), false) => Some(&pf::BGRA32),
            (SamplePlanes::U8(_), true) => Some(&pf::PBGRA32),
            (SamplePlanes::U16(_), false) => Some(&pf::RGBA64),
            (SamplePlanes::U16(_), true) => Some(&pf::PRGBA64),
            (SamplePlanes::I16(_), false) => Some(&pf::RGBA64_FIXED),
            (SamplePlanes::I32(_), false) => Some(&pf::RGBA128_FIXED),
            (SamplePlanes::F16(_), false) => Some(&pf::RGBA64_HALF),
            (SamplePlanes::F32(_), false) => Some(&pf::RGBA128_FLOAT),
            (SamplePlanes::F32(_), true) => Some(&pf::PRGBA128_FLOAT),
            _ => None,
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
                p[i].iter()
                    .map(|&v| fold_f32(v, d.len_mantissa as i32, d.exp_bias)),
                &d,
                scaled,
            ),
            // RGBE folds channels against the shared E plane — use
            // [`rgbe_prebias`], not the per-plane path.
            SamplePlanes::Rgbe(_) => unreachable!("RGBE converts via rgbe_prebias"),
            // Packed words split into three channels — [`packed_prebias`].
            SamplePlanes::Packed555(_)
            | SamplePlanes::Packed565(_)
            | SamplePlanes::Packed101010(_) => {
                unreachable!("packed formats convert via packed_prebias")
            }
            // Bi-level: 0/1 values, no bias (Table 188 lists no BD1 bias).
            SamplePlanes::Bw(p) | SamplePlanes::BwBlackIsOne(p) => {
                let sh = if scaled { 3 } else { 0 };
                p[i].iter().map(|&v| (v as i32) << sh).collect()
            }
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
    pub const BD8: Depth = Depth {
        bitdepth: BD8,
        shift_bits: 0,
        len_mantissa: 0,
        exp_bias: 0,
    };
    pub const BD16: Depth = Depth {
        bitdepth: BD16,
        shift_bits: 0,
        len_mantissa: 0,
        exp_bias: 0,
    };
    pub const BD16S: Depth = Depth {
        bitdepth: BD16S,
        shift_bits: 0,
        len_mantissa: 0,
        exp_bias: 0,
    };
    /// libjxr's default pre-shift for 32-bit input (`strenc.c:785`).
    pub const BD32S: Depth = Depth {
        bitdepth: BD32S,
        shift_bits: 10,
        len_mantissa: 0,
        exp_bias: 0,
    };
    pub const BD16F: Depth = Depth {
        bitdepth: BD16F,
        shift_bits: 0,
        len_mantissa: 0,
        exp_bias: 0,
    };
    pub const BD5: Depth = Depth {
        bitdepth: BD5,
        shift_bits: 0,
        len_mantissa: 0,
        exp_bias: 0,
    };
    pub const BD565: Depth = Depth {
        bitdepth: BD565,
        shift_bits: 0,
        len_mantissa: 0,
        exp_bias: 0,
    };
    pub const BD10: Depth = Depth {
        bitdepth: BD10,
        shift_bits: 0,
        len_mantissa: 0,
        exp_bias: 0,
    };
    pub const BD1_WHITE1: Depth = Depth {
        bitdepth: BD1WHITE1,
        shift_bits: 0,
        len_mantissa: 0,
        exp_bias: 0,
    };
    pub const BD1_BLACK1: Depth = Depth {
        bitdepth: BD1BLACK1,
        shift_bits: 0,
        len_mantissa: 0,
        exp_bias: 0,
    };
    /// The reference defaults: `len_mantissa = 13` (strenc.c:790),
    /// `exp_bias = 4` (header-dumped from every jxrencapp BD32F mint).
    pub const BD32F: Depth = Depth {
        bitdepth: BD32F,
        shift_bits: 0,
        len_mantissa: 13,
        exp_bias: 4,
    };

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

/// Raw integer samples of plane `i`, no bias/shift applied — the CMYK
/// conversions own their bias geometry. U8/U16 only (the CMYK container
/// formats); other families are rejected upstream.
fn raw_plane(samples: &SamplePlanes<'_>, i: usize) -> Vec<i32> {
    match samples {
        SamplePlanes::U8(p) => p[i].iter().map(|&v| v as i32).collect(),
        SamplePlanes::U16(p) => p[i].iter().map(|&v| v as i32).collect(),
        _ => unreachable!("CMYK is U8/U16 only (validated upstream)"),
    }
}

/// CMYK → internal YUVK forward — a clone of libjxr's `_CC_CMYK` lifting +
/// its asymmetric bias placement (strenc.c:415/1970: `U = c, V = −y,
/// K = k, Y = iOffset − m` with `iOffset` the FULL bias): the exact
/// inverse of the decoder's Table-186 lifting + its half-bias CMYK
/// `add_bias` arm (numerically proven over the full byte range, both
/// arithmetic modes). Returns the 4 internal planes in Y,U,V,K order.
pub(super) fn cmyk_prebias(samples: &SamplePlanes<'_>, scaled: bool) -> Vec<Vec<i32>> {
    let d = samples.depth();
    let sh = if scaled { 3 } else { 0 };
    let bias = d.bias_base() << sh;
    let n = samples.plane_len(0);
    let (cp, mp, yp, kp) = (
        raw_plane(samples, 0),
        raw_plane(samples, 1),
        raw_plane(samples, 2),
        raw_plane(samples, 3),
    );
    let mut out = vec![vec![0i32; n]; 4];
    for i in 0..n {
        let (mut c, mut m, mut y, mut k) = (cp[i] << sh, mp[i] << sh, yp[i] << sh, kp[i] << sh);
        y -= c;
        c += ((y + 1) >> 1) - m;
        m += (c >> 1) - k;
        k += (m + 1) >> 1;
        out[0][i] = bias - m; // Y
        out[1][i] = c; // U
        out[2][i] = -y; // V
        out[3][i] = k; // K
    }
    out
}

/// CMYKDIRECT → internal YUVK forward: the inverse of the decoder's
/// Table-187 channel shuffle (`out = (U, V, K, Y)`) + the plain full-bias
/// arm — internal `(Y,U,V,K) = (d3, d0, d1, d2)`, each `(x − bias) << sh`.
pub(super) fn cmykdirect_prebias(samples: &SamplePlanes<'_>, scaled: bool) -> Vec<Vec<i32>> {
    let d = samples.depth();
    let sh = if scaled { 3 } else { 0 };
    let bias = d.bias_base();
    const MAP: [usize; 4] = [3, 0, 1, 2]; // internal Y,U,V,K ← direct channel
    MAP.iter()
        .map(|&src| {
            raw_plane(samples, src)
                .iter()
                .map(|&v| (v - bias) << sh)
                .collect()
        })
        .collect()
}

/// Unpack packed RGB words into the three pre-bias channel planes — the
/// inverse of the decoder's pack (Tables 196/197/198: `c0 + (c1 << hi1) +
/// (c2 << hi2)` over clipped channels) + its per-channel bias/scaling:
/// BD5/BD10 take the plain bias (16/512); BD565's 5-bit channels (0 and 2)
/// carry an extra `<< 1` (the decoder's `j_scale + 1` on non-green channels
/// — `compute_scaling`, even unscaled). Positional channels: the YUV
/// lifting inverts positionally, so no R/B naming is needed (we emit
/// `red_blue_not_swapped_flag = 0`, the no-swap decode path).
pub(super) fn packed_prebias(
    samples: &SamplePlanes<'_>,
    scaled: bool,
) -> (Vec<i32>, Vec<i32>, Vec<i32>) {
    let sh = if scaled { 3 } else { 0 };
    let unpack = |w: i32, hi1: i32, hi2: i32, max0: i32, max1: i32| -> (i32, i32, i32) {
        (w & max0, (w >> hi1) & max1, (w >> hi2) & max0)
    };
    let n = samples.plane_len(0);
    let (mut r, mut g, mut b) = (vec![0i32; n], vec![0i32; n], vec![0i32; n]);
    match samples {
        SamplePlanes::Packed555(p) => {
            for (i, &w) in p[0].iter().enumerate() {
                let (c0, c1, c2) = unpack(w as i32, 5, 10, 31, 31);
                r[i] = (c0 - 16) << sh;
                g[i] = (c1 - 16) << sh;
                b[i] = (c2 - 16) << sh;
            }
        }
        SamplePlanes::Packed565(p) => {
            for (i, &w) in p[0].iter().enumerate() {
                let (c0, c1, c2) = unpack(w as i32, 5, 11, 31, 63);
                r[i] = (c0 << (sh + 1)) - (32 << sh);
                g[i] = (c1 - 32) << sh;
                b[i] = (c2 << (sh + 1)) - (32 << sh);
            }
        }
        SamplePlanes::Packed101010(p) => {
            for (i, &w) in p[0].iter().enumerate() {
                let (c0, c1, c2) = unpack(w as i32, 10, 20, 1023, 1023);
                r[i] = (c0 - 512) << sh;
                g[i] = (c1 - 512) << sh;
                b[i] = (c2 - 512) << sh;
            }
        }
        _ => unreachable!("packed_prebias is for the packed variants"),
    }
    (r, g, b)
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
            for x in (0u16..=u16::MAX)
                .step_by(7)
                .chain([0, 1, u16::MAX].into_iter())
            {
                let c = prebias([x as i32].into_iter(), &Depth::BD16, scaled)[0];
                assert_eq!(
                    decoder_chain(c, &Depth::BD16, scaled),
                    x as i32,
                    "BD16 x={x} scaled={scaled}"
                );
            }
            for x in (i16::MIN..=i16::MAX)
                .step_by(7)
                .chain([i16::MIN, -1, 0, i16::MAX].into_iter())
            {
                let c = prebias([x as i32].into_iter(), &Depth::BD16S, scaled)[0];
                assert_eq!(
                    decoder_chain(c, &Depth::BD16S, scaled),
                    x as i32,
                    "BD16S x={x} scaled={scaled}"
                );
            }
        }
        // BD32S: exact after the 10-bit shift quantization (never scaled).
        for x in [
            i32::MIN,
            -1_000_000_007,
            -1024,
            -1023,
            -1,
            0,
            1,
            1023,
            1024,
            1_000_000_007,
            i32::MAX,
        ] {
            let c = prebias([x].into_iter(), &Depth::BD32S, false)[0];
            assert_eq!(
                decoder_chain(c, &Depth::BD32S, false),
                (x >> 10) << 10,
                "BD32S x={x}"
            );
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
