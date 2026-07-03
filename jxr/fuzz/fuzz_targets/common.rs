//! Shared input generator for the encoder fuzz targets.
//!
//! `draw_valid` produces inputs the encoder DOCUMENTS as accepted — every
//! sample family the crate encodes (the full closed envelope) × the option
//! surface (chroma, bands/trim, windowing, tiles, overlap, frequency,
//! scaled, QPs, alpha, premultiplied, random `QpPlan`s) — together with the
//! exactness expectation the public contract states for that combination.
//! `draw_raw` produces arbitrary (frequently invalid) option/plane
//! combinations for the no-panic target.

// Shared between the fuzz binaries — each compiles its own copy and uses
// half of it, so per-binary dead-code analysis is meaningless here.
#![allow(dead_code)]

use arbitrary::{Arbitrary, Unstructured};
use jxr::{
    BandQp, BandsPresent, ChromaSampling, ColorMode, EncodeOptions, Overlap, QpPlan, QpSet,
    SamplePlanes, TileQps,
};

/// Owned storage behind a [`SamplePlanes`] borrow.
pub enum Planes {
    U8(Vec<Vec<u8>>),
    U16(Vec<Vec<u16>>),
    I16(Vec<Vec<i16>>),
    I32(Vec<Vec<i32>>),
    F16(Vec<Vec<u16>>),
    F32(Vec<Vec<u32>>),
    Rgbe(Vec<Vec<u8>>),
    P555(Vec<Vec<u16>>),
    P565(Vec<Vec<u16>>),
    P101010(Vec<Vec<u32>>),
    Bw(Vec<Vec<u8>>),
    BwB1(Vec<Vec<u8>>),
}

impl Planes {
    pub fn as_samples(&self) -> SamplePlanes<'_> {
        match self {
            Planes::U8(p) => SamplePlanes::U8(p),
            Planes::U16(p) => SamplePlanes::U16(p),
            Planes::I16(p) => SamplePlanes::I16(p),
            Planes::I32(p) => SamplePlanes::I32(p),
            Planes::F16(p) => SamplePlanes::F16(p),
            Planes::F32(p) => SamplePlanes::F32(p),
            Planes::Rgbe(p) => SamplePlanes::Rgbe(p),
            Planes::P555(p) => SamplePlanes::Packed555(p),
            Planes::P565(p) => SamplePlanes::Packed565(p),
            Planes::P101010(p) => SamplePlanes::Packed101010(p),
            Planes::Bw(p) => SamplePlanes::Bw(p),
            Planes::BwB1(p) => SamplePlanes::BwBlackIsOne(p),
        }
    }

    pub fn num_planes(&self) -> usize {
        match self {
            Planes::U8(p) | Planes::Rgbe(p) | Planes::Bw(p) | Planes::BwB1(p) => p.len(),
            Planes::U16(p) | Planes::F16(p) | Planes::P555(p) | Planes::P565(p) => p.len(),
            Planes::I16(p) => p.len(),
            Planes::I32(p) => p.len(),
            Planes::F32(p) | Planes::P101010(p) => p.len(),
        }
    }

    /// Expected decoded value of plane `c`, sample `i` — in the i32 domain
    /// the decoder's planes use. `None` when the family has no simple
    /// per-sample expectation (RGBE renormalization).
    pub fn expected(&self, c: usize, i: usize) -> Option<i64> {
        Some(match self {
            Planes::U8(p) | Planes::Bw(p) | Planes::BwB1(p) => p[c][i] as i64,
            Planes::U16(p) | Planes::P555(p) | Planes::P565(p) => p[c][i] as i64,
            // Half floats are bit-pattern exact for EVERY pattern except
            // -0.0, which normalizes to +0.0 (single codestream zero).
            Planes::F16(p) => {
                let v = p[c][i];
                if v & 0x7fff == 0 { 0 } else { v as i64 }
            }
            Planes::I16(p) => p[c][i] as i64,
            // BD32S sheds shift_bits = 10: q1 round-trips (x >> 10) << 10.
            Planes::I32(p) => ((p[c][i] >> 10) << 10) as i64,
            Planes::F32(p) | Planes::P101010(p) => p[c][i] as i64,
            Planes::Rgbe(_) => return None,
        })
    }

    /// Channel-equality in the encoder's auto-gray sense (pre-bias domain):
    /// raw equality for integer families, zero-sign-folded equality for the
    /// float patterns, `>> 10` equality for BD32S.
    pub fn channels_equal(&self) -> bool {
        fn eq<T: PartialEq>(p: &[Vec<T>]) -> bool {
            p.windows(2).all(|w| w[0] == w[1])
        }
        match self {
            Planes::U8(p) => p.len() == 3 && eq(p),
            Planes::U16(p) => p.len() == 3 && eq(p),
            Planes::I16(p) => p.len() == 3 && eq(p),
            Planes::I32(p) => {
                p.len() == 3
                    && p.windows(2)
                        .all(|w| w[0].iter().zip(&w[1]).all(|(a, b)| (a >> 10) == (b >> 10)))
            }
            Planes::F16(p) => {
                p.len() == 3
                    && p.windows(2).all(|w| {
                        w[0].iter()
                            .zip(&w[1])
                            .all(|(&a, &b)| a == b || (a & 0x7fff == 0 && b & 0x7fff == 0))
                    })
            }
            Planes::F32(p) => {
                p.len() == 3
                    && p.windows(2).all(|w| {
                        w[0].iter().zip(&w[1]).all(|(&a, &b)| {
                            a == b || (a & 0x7fff_ffff == 0 && b & 0x7fff_ffff == 0)
                        })
                    })
            }
            // RGBE is 4-plane, packed/bw single-plane: never auto-gray.
            _ => false,
        }
    }
}

/// What the documented contract promises for this case.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Expect {
    /// Decoded planes equal `Planes::expected` exactly.
    Exact,
    /// Decoded planes within ±bound of `Planes::expected` (scaled q1:
    /// the chroma/ink half-step floors).
    Bounded(i64),
    /// F32 at lossless QP: arbitrary patterns project onto the custom
    /// float's representable set (lm13 grid; sub-floor magnitudes flush —
    /// the documented sub-2⁻³⁶ reference divergence zone), so the strong
    /// invariant is IDEMPOTENCE: the decoded planes re-encode and re-decode
    /// bit-exactly (the README's "anything this crate's decoder produced is
    /// already on that grid" promise).
    F32Idem,
    /// Only "decodes successfully + headers match" is promised.
    DecodeOnly,
}

pub struct Case {
    pub planes: Planes,
    pub width: u32,
    pub height: u32,
    pub premultiplied: bool,
    pub mode: ColorMode,
    pub opts: EncodeOptions,
    pub expect: Expect,
    /// The encoder will auto-gray-collapse this input.
    pub auto_gray: bool,
    /// Expected `internal_clr_fmt` of plane 0 (`None` = don't assert).
    pub int_fmt: Option<u8>,
}

fn dim(u: &mut Unstructured) -> arbitrary::Result<u32> {
    // Bias toward MB/block edges; cap small for throughput.
    const EDGES: [u32; 12] = [1, 2, 15, 16, 17, 31, 32, 33, 47, 48, 63, 64];
    Ok(if u.arbitrary::<bool>()? {
        EDGES[u.choose_index(EDGES.len())?]
    } else {
        u.int_in_range(1..=72)?
    })
}

fn fill<T: Default + Copy + Arbitrary<'static>>(
    u: &mut Unstructured,
    n: usize,
    np: usize,
    map: impl Fn(u64) -> T,
) -> arbitrary::Result<Vec<Vec<T>>> {
    // Structure bytes come from `u`; bulk pixels from a xorshift stream
    // seeded by `u` so input length doesn't bound image size.
    let mut s = u.arbitrary::<u64>()?.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    Ok((0..np)
        .map(|_| (0..n).map(|_| map(next())).collect())
        .collect())
}

fn qp(u: &mut Unstructured) -> arbitrary::Result<QpSet> {
    Ok(QpSet {
        dc: u.int_in_range(1..=120)?,
        lp: u.int_in_range(1..=160)?,
        hp: u.int_in_range(1..=200)?,
    })
}

/// Draw a small valid `QpPlan` for the given geometry. `lossless` forces
/// every set to QP 0 (the exactness-preserving shape).
fn qp_plan(
    u: &mut Unstructured,
    ntiles: usize,
    mb_cols: usize,
    mb_rows: usize,
    lossless: bool,
) -> arbitrary::Result<QpPlan> {
    let nlp = u.int_in_range(1..=3usize)?;
    let nhp = u.int_in_range(1..=3usize)?;
    let band = |u: &mut Unstructured| -> arbitrary::Result<BandQp> {
        if lossless {
            return Ok(BandQp::uniform(0));
        }
        Ok(match u.int_in_range(0..=2u8)? {
            0 => BandQp::uniform(u.int_in_range(0..=120)?),
            1 => BandQp::separate(u.int_in_range(0..=120)?, u.int_in_range(0..=120)?),
            _ => BandQp([
                u.int_in_range(0..=120)?,
                u.int_in_range(0..=120)?,
                u.int_in_range(0..=120)?,
            ]),
        })
    };
    let per_tile = u.arbitrary::<bool>()?;
    let entries = if per_tile { ntiles } else { 1 };
    let mut tiles = Vec::with_capacity(entries);
    for _ in 0..entries {
        let dc = band(u)?;
        let lp = (0..nlp)
            .map(|_| band(u))
            .collect::<arbitrary::Result<Vec<_>>>()?;
        let hp = (0..nhp)
            .map(|_| band(u))
            .collect::<arbitrary::Result<Vec<_>>>()?;
        tiles.push(TileQps { dc, lp, hp });
    }
    let nmb = mb_cols * mb_rows;
    let map = |u: &mut Unstructured, sets: usize| -> arbitrary::Result<Vec<u8>> {
        if sets == 1 || !u.arbitrary::<bool>()? {
            return Ok(Vec::new());
        }
        let mut m = Vec::with_capacity(nmb);
        for _ in 0..nmb {
            m.push(u.int_in_range(0..=(sets as u8 - 1))?);
        }
        Ok(m)
    };
    let lp_index = map(u, nlp)?;
    let hp_index = map(u, nhp)?;
    Ok(QpPlan {
        tiles,
        lp_index,
        hp_index,
    })
}

/// Draw one VALID case over the full envelope.
pub fn draw_valid(u: &mut Unstructured) -> arbitrary::Result<Case> {
    use jxr::decode::consts::{
        INT_NCOMPONENT, INT_YONLY, INT_YUV420, INT_YUV422, INT_YUV444, INT_YUVK,
    };
    let (w, h) = (dim(u)?, dim(u)?);
    let n = (w * h) as usize;

    // Family selector. Weights are implicit (uniform over the list).
    #[derive(Clone, Copy, PartialEq)]
    enum Fam {
        U8,
        U16,
        I16,
        I32,
        F16,
        F32,
        Rgbe,
        CmykU8,
        CmykU16,
        CmykDirect,
        NComp,
        P555,
        P565,
        P101010,
        Bw,
        BwB1,
    }
    const FAMS: [Fam; 16] = [
        Fam::U8,
        Fam::U16,
        Fam::I16,
        Fam::I32,
        Fam::F16,
        Fam::F32,
        Fam::Rgbe,
        Fam::CmykU8,
        Fam::CmykU16,
        Fam::CmykDirect,
        Fam::NComp,
        Fam::P555,
        Fam::P565,
        Fam::P101010,
        Fam::Bw,
        Fam::BwB1,
    ];
    let fam = FAMS[u.choose_index(FAMS.len())?];

    // Plane count per family.
    let np = match fam {
        Fam::U8 | Fam::U16 | Fam::I16 | Fam::I32 | Fam::F16 | Fam::F32 => {
            *u.choose(&[1usize, 3, 4])?
        }
        Fam::Rgbe => 4,
        Fam::CmykU8 | Fam::CmykU16 | Fam::CmykDirect => *u.choose(&[4usize, 5])?,
        Fam::NComp => u.int_in_range(3..=8usize)?,
        _ => 1,
    };

    let planes = match fam {
        Fam::U8 => Planes::U8(fill(u, n, np, |r| r as u8)?),
        Fam::U16 => Planes::U16(fill(u, n, np, |r| r as u16)?),
        Fam::I16 => Planes::I16(fill(u, n, np, |r| r as i16)?),
        Fam::I32 => Planes::I32(fill(u, n, np, |r| r as i32)?),
        Fam::F16 => Planes::F16(fill(u, n, np, |r| r as u16)?),
        Fam::F32 => Planes::F32(fill(u, n, np, |r| r as u32)?),
        Fam::Rgbe => Planes::Rgbe(fill(u, n, 4, |r| r as u8)?),
        Fam::CmykU8 | Fam::CmykDirect => Planes::U8(fill(u, n, np, |r| r as u8)?),
        Fam::CmykU16 => Planes::U16(fill(u, n, np, |r| r as u16)?),
        Fam::NComp => {
            if u.arbitrary::<bool>()? {
                Planes::U8(fill(u, n, np, |r| r as u8)?)
            } else {
                Planes::U16(fill(u, n, np, |r| r as u16)?)
            }
        }
        Fam::P555 => Planes::P555(fill(u, n, 1, |r| (r as u16) & 0x7fff)?),
        Fam::P565 => Planes::P565(fill(u, n, 1, |r| r as u16)?),
        Fam::P101010 => Planes::P101010(fill(u, n, 1, |r| (r as u32) & 0x3fff_ffff)?),
        Fam::Bw => Planes::Bw(fill(u, n, 1, |r| (r & 1) as u8)?),
        Fam::BwB1 => Planes::BwB1(fill(u, n, 1, |r| (r & 1) as u8)?),
    };

    let mode = match fam {
        Fam::CmykU8 | Fam::CmykU16 => ColorMode::Cmyk,
        Fam::CmykDirect => ColorMode::CmykDirect,
        Fam::NComp => ColorMode::NComponent,
        Fam::Bw | Fam::BwB1 => ColorMode::Grayscale,
        // Packed words are one plane but inherently color.
        Fam::P555 | Fam::P565 | Fam::P101010 => ColorMode::Color,
        _ if np == 1 => {
            // 1-plane encodes gray under either mode; exercise both.
            *u.choose(&[ColorMode::Grayscale, ColorMode::Color])?
        }
        _ => ColorMode::Color,
    };

    let multi = matches!(
        mode,
        ColorMode::Cmyk | ColorMode::CmykDirect | ColorMode::NComponent
    );
    let float_like = matches!(fam, Fam::F16 | Fam::F32 | Fam::Rgbe);
    let packed = matches!(fam, Fam::P555 | Fam::P565 | Fam::P101010);
    // Packed is one plane of WORDS but codes as color — not the gray path.
    let gray_path = !packed && (np == 1 || mode == ColorMode::Grayscale);
    let has_alpha = (np == 4 && !multi && fam != Fam::Rgbe) || (multi && np == 5);

    // Chroma: 444 always legal; decimation only for integer RGB-likes;
    // YOnly only on the non-alpha color paths.
    let chroma = if multi || gray_path {
        ChromaSampling::Yuv444
    } else if float_like {
        ChromaSampling::Yuv444
    } else if has_alpha {
        *u.choose(&[
            ChromaSampling::Yuv444,
            ChromaSampling::Yuv422,
            ChromaSampling::Yuv420,
        ])?
    } else {
        *u.choose(&[
            ChromaSampling::Yuv444,
            ChromaSampling::Yuv444,
            ChromaSampling::Yuv444, // weight 444 up: it's the exactness path
            ChromaSampling::Yuv422,
            ChromaSampling::Yuv420,
            ChromaSampling::YOnly,
        ])?
    };

    // Bands/trim: not implemented with alpha, RGBE, or YOnly.
    let (bands, trim) = if has_alpha || fam == Fam::Rgbe || chroma == ChromaSampling::YOnly {
        (BandsPresent::All, 0u8)
    } else {
        let b = *u.choose(&[
            BandsPresent::All,
            BandsPresent::All,
            BandsPresent::All, // weight All up: the exactness path
            BandsPresent::NoFlexbits,
            BandsPresent::NoHighpass,
            BandsPresent::DcOnly,
        ])?;
        // Weight trim == 0 up: any trim forfeits exactness.
        let t = if b == BandsPresent::All && !u.ratio(3, 4)? {
            u.int_in_range(1..=15)?
        } else {
            0
        };
        (b, t)
    };

    // Scaled: rejected for 32-bit depths.
    let scaled = !matches!(fam, Fam::I32 | Fam::F32) && u.arbitrary::<bool>()?;

    let window_top = u.int_in_range(0..=8u8)?;
    let window_left = u.int_in_range(0..=8u8)?;
    let mb_cols = ((w + window_left as u32).div_ceil(16)) as usize;
    let mb_rows = ((h + window_top as u32).div_ceil(16)) as usize;
    let tile_cols = u.int_in_range(0..=3u16)?.min(mb_cols as u16);
    let tile_rows = u.int_in_range(0..=3u16)?.min(mb_rows as u16);
    let ntiles = tile_cols.max(1) as usize * tile_rows.max(1) as usize;

    let overlap = *u.choose(&[Overlap::None, Overlap::One, Overlap::Two])?;
    let frequency = u.arbitrary::<bool>()?;

    // QP source: lossless / lossy / chroma_qp / qp_plan (color-coded only).
    let lossless = u.ratio(2, 3)?; // weight lossless up: the exactness path
    let base_qp = if lossless { QpSet::LOSSLESS } else { qp(u)? };
    // qp_plan: color-coded paths only — and the alpha-plane path rejects it.
    let plan_legal = !gray_path && !multi && !has_alpha && chroma != ChromaSampling::YOnly;
    let auto_gray = planes.channels_equal();
    let (chroma_qp, plan) = if plan_legal && !auto_gray && u.ratio(1, 3)? {
        let plan_lossless = lossless && u.arbitrary::<bool>()?;
        (
            None,
            Some((
                qp_plan(u, ntiles, mb_cols, mb_rows, plan_lossless)?,
                plan_lossless,
            )),
        )
    } else if !gray_path && !lossless && u.arbitrary::<bool>()? {
        (Some(qp(u)?), None)
    } else {
        (None, None)
    };

    let premultiplied = has_alpha
        && !multi
        && matches!(fam, Fam::U8 | Fam::U16 | Fam::F32)
        && u.arbitrary::<bool>()?;

    let alpha_qp = if has_alpha && u.arbitrary::<bool>()? {
        Some(if lossless { QpSet::LOSSLESS } else { qp(u)? })
    } else {
        None
    };

    // The exactness tier the public contract states for this combination.
    let exact_qp = lossless
        && alpha_qp.map_or(true, |a| a.dc == 0 && a.lp == 0 && a.hp == 0)
        && chroma_qp.is_none()
        && plan.as_ref().map_or(true, |(_, pl)| *pl);
    let full_chroma = matches!(chroma, ChromaSampling::Yuv444) || gray_path || multi;
    let all_bands = bands == BandsPresent::All && trim == 0;
    let expect = if !exact_qp || !all_bands || !full_chroma || chroma == ChromaSampling::YOnly {
        Expect::DecodeOnly
    } else if matches!(fam, Fam::Rgbe) {
        // Arbitrary RGBE renormalizes (value-preserving, not byte-stable).
        Expect::DecodeOnly
    } else if !scaled {
        if matches!(fam, Fam::F32) {
            // Arbitrary F32 patterns project onto the representable set.
            Expect::F32Idem
        } else {
            Expect::Exact
        }
    } else if matches!(
        fam,
        Fam::U8 | Fam::U16 | Fam::I16 | Fam::CmykU8 | Fam::CmykU16
    ) || (fam == Fam::NComp)
        || fam == Fam::CmykDirect
    {
        if gray_path || auto_gray {
            Expect::Exact // gray scaled q1 is exactly invertible
        } else {
            Expect::Bounded(4) // chroma/ink half-step floors
        }
    } else if matches!(fam, Fam::Bw | Fam::BwB1) {
        Expect::Exact
    } else {
        Expect::DecodeOnly // scaled floats / packed: no bound promised
    };

    let int_fmt = if auto_gray || gray_path {
        Some(INT_YONLY)
    } else if multi {
        Some(if mode == ColorMode::NComponent {
            INT_NCOMPONENT
        } else {
            INT_YUVK
        })
    } else {
        match chroma {
            ChromaSampling::Yuv444 => Some(INT_YUV444),
            ChromaSampling::Yuv422 => Some(INT_YUV422),
            ChromaSampling::Yuv420 => Some(INT_YUV420),
            ChromaSampling::YOnly => Some(INT_YONLY),
        }
    };

    Ok(Case {
        planes,
        width: w,
        height: h,
        premultiplied,
        mode,
        opts: EncodeOptions {
            qp: base_qp,
            alpha_qp,
            chroma,
            bands,
            trim_flexbits: trim,
            scaled,
            window_top,
            window_left,
            tile_cols,
            tile_rows,
            overlap,
            frequency,
            chroma_qp,
            qp_plan: plan.map(|(p, _)| p),
        },
        expect,
        auto_gray,
        int_fmt,
    })
}

/// Draw a RAW case: arbitrary plane shapes and option values, frequently
/// invalid. The encoder must return `Ok`/`Err`, never panic; an `Ok` file
/// must decode.
pub fn draw_raw(u: &mut Unstructured) -> arbitrary::Result<Case> {
    // Dims may disagree with plane lengths; keep the allocation bounded.
    let w = u.int_in_range(0..=96u32)?;
    let h = u.int_in_range(0..=96u32)?;
    let np = u.int_in_range(0..=9usize)?;
    // Plane length deliberately decoupled from w*h.
    let len = u.int_in_range(0..=96 * 96usize)?;
    let planes = match u.int_in_range(0..=11u8)? {
        0 => Planes::U8(fill(u, len, np, |r| r as u8)?),
        1 => Planes::U16(fill(u, len, np, |r| r as u16)?),
        2 => Planes::I16(fill(u, len, np, |r| r as i16)?),
        3 => Planes::I32(fill(u, len, np, |r| r as i32)?),
        4 => Planes::F16(fill(u, len, np, |r| r as u16)?),
        5 => Planes::F32(fill(u, len, np, |r| r as u32)?),
        6 => Planes::Rgbe(fill(u, len, np, |r| r as u8)?),
        7 => Planes::P555(fill(u, len, np, |r| r as u16)?),
        8 => Planes::P565(fill(u, len, np, |r| r as u16)?),
        9 => Planes::P101010(fill(u, len, np, |r| r as u32)?),
        10 => Planes::Bw(fill(u, len, np, |r| r as u8)?), // values > 1: must error
        _ => Planes::BwB1(fill(u, len, np, |r| (r & 1) as u8)?),
    };
    let mode = *u.choose(&[
        ColorMode::Grayscale,
        ColorMode::Color,
        ColorMode::Cmyk,
        ColorMode::CmykDirect,
        ColorMode::NComponent,
    ])?;
    let plan = if u.arbitrary::<bool>()? {
        // Arbitrary (often malformed) plan shapes.
        let nt = u.int_in_range(0..=5usize)?;
        let mut tiles = Vec::new();
        for _ in 0..nt {
            let setn = |u: &mut Unstructured| -> arbitrary::Result<Vec<BandQp>> {
                let k = u.int_in_range(0..=18usize)?;
                Ok((0..k).map(|_| BandQp::uniform(7)).collect())
            };
            tiles.push(TileQps {
                dc: BandQp::uniform(3),
                lp: setn(u)?,
                hp: setn(u)?,
            });
        }
        let idx = |u: &mut Unstructured| -> arbitrary::Result<Vec<u8>> {
            let k = u.int_in_range(0..=64usize)?;
            Ok((0..k).map(|i| (i % 19) as u8).collect())
        };
        Some(QpPlan {
            tiles,
            lp_index: idx(u)?,
            hp_index: idx(u)?,
        })
    } else {
        None
    };
    Ok(Case {
        planes,
        width: w,
        height: h,
        premultiplied: u.arbitrary()?,
        mode,
        opts: EncodeOptions {
            qp: QpSet {
                dc: u.arbitrary()?,
                lp: u.arbitrary()?,
                hp: u.arbitrary()?,
            },
            alpha_qp: if u.arbitrary()? {
                Some(QpSet {
                    dc: u.arbitrary()?,
                    lp: u.arbitrary()?,
                    hp: u.arbitrary()?,
                })
            } else {
                None
            },
            chroma: *u.choose(&[
                ChromaSampling::Yuv444,
                ChromaSampling::Yuv422,
                ChromaSampling::Yuv420,
                ChromaSampling::YOnly,
            ])?,
            bands: *u.choose(&[
                BandsPresent::All,
                BandsPresent::NoFlexbits,
                BandsPresent::NoHighpass,
                BandsPresent::DcOnly,
            ])?,
            trim_flexbits: u.arbitrary()?,
            scaled: u.arbitrary()?,
            window_top: u.arbitrary()?,
            window_left: u.arbitrary()?,
            tile_cols: u.int_in_range(0..=5000u16)?,
            tile_rows: u.int_in_range(0..=5000u16)?,
            overlap: *u.choose(&[Overlap::None, Overlap::One, Overlap::Two])?,
            frequency: u.arbitrary()?,
            chroma_qp: if u.arbitrary()? {
                Some(QpSet {
                    dc: u.arbitrary()?,
                    lp: u.arbitrary()?,
                    hp: u.arbitrary()?,
                })
            } else {
                None
            },
            qp_plan: plan,
        },
        expect: Expect::DecodeOnly,
        auto_gray: false,
        int_fmt: None,
    })
}
