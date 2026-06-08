//! Pure-Rust JPEG-XR encoder (EPUB→KFX), dual grayscale/color.
//!
//! Forward mirror of [`crate::image::jxr_decode`], built bottom-up against the
//! decoder as a round-trip oracle — see `.claude/plans/jxr-encoder.md`.
//!
//! Status: **grayscale + color complete** — full ALL_BANDS (DC + LP + HP +
//! flexbits), multi-MB prediction, windowing, and per-band quantization
//! ([`quant`]). Grayscale ([`gray`]) is `8bppGray`; color ([`color`]) is
//! `24bppRGB` via the YUV 4:4:4 transform. `QpSet::LOSSLESS` round-trips
//! bit-exact (both modes); `QP > 0` is lossy (ship mode).

pub mod bitstream;
pub mod codestream;
pub mod color;
pub mod container;
pub mod coeff;
pub mod entropy;
pub mod gray;
pub mod hp;
pub mod quant;
pub mod transform;

pub use quant::QpSet;

/// How color is handled on the way into the KFX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Single luma plane (`8bppGray`). Default: every Kindle we target is B&W
    /// e-ink, and the source EPUB retains the color master, so color is only
    /// dropped from the device copy and is recoverable by reconverting.
    Grayscale,
    /// Full color via the internal YUV 4:4:4 transform + chroma planes
    /// (`24bppRGB`). For desktop/Sidle-reader color; the device is still B&W so
    /// grayscale stays the pipeline default. A 1-plane input encodes as
    /// grayscale even in this mode (no chroma to synthesize).
    Color,
}

/// Errors from the encoder.
#[derive(Debug)]
pub enum EncodeError {
    /// A stage that hasn't been built yet (scaffolding placeholder).
    NotImplemented(&'static str),
    /// Input the encoder can't represent.
    Unsupported(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::NotImplemented(s) => write!(f, "jxr encode not implemented: {s}"),
            EncodeError::Unsupported(s) => write!(f, "unsupported: {s}"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// 8-bit pixel input. One plane per component (1 for grayscale, 3 for color),
/// each row-major with `len == width * height`.
pub struct ImageInput<'a> {
    pub width: u32,
    pub height: u32,
    pub planes: &'a [Vec<u8>],
}

/// Encode 8-bit grayscale pixels into a JPEG-XR file (TIFF container + WMPHOTO
/// codestream) at the given per-band quantizers. `QpSet::LOSSLESS` is bit-exact;
/// higher QP trades fidelity for size (ship mode). Output is decodable by
/// `jxr_decode` and structurally clones a real Amazon JXR.
pub fn encode(
    input: &ImageInput<'_>,
    mode: ColorMode,
    qp: QpSet,
) -> Result<Vec<u8>, EncodeError> {
    let (w, h) = (input.width, input.height);
    if w == 0 || h == 0 {
        return Err(EncodeError::Unsupported("zero-size image".into()));
    }
    if w > 1 << 16 || h > 1 << 16 {
        return Err(EncodeError::Unsupported("dims exceed short-header u16 range".into()));
    }
    let n = w as usize * h as usize;
    let check = |p: &[u8]| {
        if p.len() == n {
            Ok(())
        } else {
            Err(EncodeError::Unsupported("plane len != width*height".into()))
        }
    };
    // A 1-plane input is grayscale regardless of mode — there's no chroma to
    // synthesize. Color auto-gray-detect (R==G==B) lives in the pipeline.
    let want_color = mode == ColorMode::Color && input.planes.len() != 1;
    if !want_color {
        if input.planes.len() != 1 {
            return Err(EncodeError::Unsupported(format!(
                "grayscale expects 1 plane, got {}",
                input.planes.len()
            )));
        }
        check(&input.planes[0])?;
        // DC + LP + HP (ALL_BANDS). QpSet::LOSSLESS ⇒ bit-exact; QP>0 ⇒ lossy.
        return Ok(gray::encode_grayscale(&input.planes[0], w, h, qp));
    }
    if input.planes.len() != 3 {
        return Err(EncodeError::Unsupported(format!(
            "color expects 3 planes (RGB), got {}",
            input.planes.len()
        )));
    }
    check(&input.planes[0])?;
    check(&input.planes[1])?;
    check(&input.planes[2])?;
    // Auto-gray: a "color" image whose channels are identical everywhere carries
    // no chroma — emit `8bppGray` (smaller; the chroma planes would be all-zero).
    if input.planes[0] == input.planes[1] && input.planes[1] == input.planes[2] {
        return Ok(gray::encode_grayscale(&input.planes[0], w, h, qp));
    }
    // Full color: RGB → YUV444 → ALL_BANDS. Lossless 4:4:4 is bit-exact.
    Ok(color::encode_color(&input.planes[0], &input.planes[1], &input.planes[2], w, h, qp))
}

/// Map a 0–100 quality knob to per-band quantizers. 100 ⇒ lossless; lower ⇒
/// coarser, with HP quantized hardest (the `1:2:4` dc:lp:hp ratio Amazon-style).
/// Tuned so the mid-80s land near Amazon's per-plate size on LN content; the
/// default is refined against real plates in the pipeline (Phase 5).
pub fn quality_to_qp(quality: u8) -> QpSet {
    if quality >= 100 {
        return QpSet::LOSSLESS;
    }
    let base = (((100 - quality as i32) + 2) / 3).clamp(1, 40) as u8;
    QpSet { dc: base, lp: base.saturating_mul(2), hp: base.saturating_mul(4) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip oracle: decode JXR bytes straight to i32 planes via the
    /// decoder, bypassing its JPEG re-encode in `jxr_transcode::transcode`.
    fn decode_to_planes(jxr: &[u8]) -> crate::image::jxr_decode::decoder::DecodedImage {
        let container = crate::image::jxr_decode::container::parse(jxr).expect("container parse");
        crate::image::jxr_decode::decoder::Decoder::new(container.image_data)
            .decode()
            .expect("decode")
    }

    #[test]
    fn roundtrip_constant_grayscale_dconly() {
        // DCONLY reconstructs a flat block exactly (LP/HP zero). Across several
        // sizes this also exercises multi-MB DC prediction + model/abs-table
        // adaptation: MB(0,0) codes the full DC, every other MB predicts to 0.
        for &(w, h) in &[(16u32, 16u32), (32, 16), (16, 32), (48, 32)] {
            for val in [128u8, 0, 255, 64, 100, 200] {
                let plane = vec![val; (w * h) as usize];
                let input = ImageInput {
                    width: w,
                    height: h,
                    planes: std::slice::from_ref(&plane),
                };
                let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
                let decoded = decode_to_planes(&jxr);
                assert_eq!((decoded.width, decoded.height), (w, h));
                for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                    assert_eq!(got, val as i32, "w={w} h={h} val={val} pixel {i}");
                }
            }
        }
    }

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

    /// Exact inverse of `transform::forward_transform_mb` (no overlap).
    fn inverse_transform_mb(buf: &mut [i32; 256]) -> [i32; 256] {
        use crate::image::jxr_decode::consts::MB_PIXEL_MAP;
        use crate::image::jxr_decode::math::{str_idct4x4_stage1, str_idct4x4_stage2};
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

    /// A zero-HP 16×16 block from random pixels: forward, drop HP, inverse.
    /// `None` if any reconstructed pixel would clip (rare; caller retries).
    fn zero_hp_block(r: &mut Lcg) -> Option<[u8; 256]> {
        let mut samples = [0i32; 256];
        for s in samples.iter_mut() {
            *s = (r.next() % 41) as i32 - 20;
        }
        let mut buf = transform::forward_transform_mb(&samples);
        for (p, v) in buf.iter_mut().enumerate() {
            if p % 16 != 0 {
                *v = 0; // drop HP, keep the per-block DC at [k*16]
            }
        }
        let s = inverse_transform_mb(&mut buf);
        let mut out = [0u8; 256];
        for (o, &v) in out.iter_mut().zip(s.iter()) {
            let p = v + 128;
            if !(0..=255).contains(&p) {
                return None;
            }
            *o = p as u8;
        }
        Some(out)
    }

    #[test]
    fn roundtrip_zero_hp_grayscale_nohighpass() {
        // NOHIGHPASS is lossless for zero-HP content: exercises the full LP
        // run-level + refine + prediction path across multi-MB sizes.
        let mut r = Lcg(0xabcd_ef01);
        for &(mbw, mbh) in &[(1usize, 1usize), (2, 1), (1, 2), (2, 2), (3, 2)] {
            let (w, h) = (mbw * 16, mbh * 16);
            let mut pixels = vec![0u8; w * h];
            for mx in 0..mbw {
                for my in 0..mbh {
                    let blk = loop {
                        if let Some(b) = zero_hp_block(&mut r) {
                            break b;
                        }
                    };
                    for py in 0..16 {
                        for px in 0..16 {
                            pixels[(my * 16 + py) * w + (mx * 16 + px)] = blk[py * 16 + px];
                        }
                    }
                }
            }
            let input = ImageInput {
                width: w as u32,
                height: h as u32,
                planes: std::slice::from_ref(&pixels),
            };
            let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
            let decoded = decode_to_planes(&jxr);
            for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                assert_eq!(got, pixels[i] as i32, "mbw={mbw} mbh={mbh} pixel {i}");
            }
        }
    }

    #[test]
    fn roundtrip_arbitrary_grayscale_allbands_lossless() {
        // The real goal: ANY grayscale image round-trips exactly (ALL_BANDS).
        let mut r = Lcg(0x5151_2727);
        for &(mbw, mbh) in &[(1usize, 1usize), (2, 1), (1, 2), (2, 2), (3, 3)] {
            let (w, h) = (mbw * 16, mbh * 16);
            let pixels: Vec<u8> = (0..w * h).map(|_| (r.next() % 256) as u8).collect();
            let input = ImageInput {
                width: w as u32,
                height: h as u32,
                planes: std::slice::from_ref(&pixels),
            };
            let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
            let decoded = decode_to_planes(&jxr);
            for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                assert_eq!(got, pixels[i] as i32, "mbw={mbw} mbh={mbh} pixel {i}");
            }
        }
    }

    #[test]
    fn roundtrip_non_aligned_grayscale_lossless() {
        // Arbitrary (non-16-aligned) dimensions: edge-pad + decoder crop.
        let mut r = Lcg(0x9090_3434);
        for &(w, h) in &[(17u32, 31u32), (100, 50), (33, 16), (16, 33), (45, 45), (1, 1)] {
            let pixels: Vec<u8> = (0..(w * h) as usize).map(|_| (r.next() % 256) as u8).collect();
            let input = ImageInput {
                width: w,
                height: h,
                planes: std::slice::from_ref(&pixels),
            };
            let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
            let decoded = decode_to_planes(&jxr);
            assert_eq!((decoded.width, decoded.height), (w, h));
            for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                assert_eq!(got, pixels[i] as i32, "w={w} h={h} pixel {i}");
            }
        }
    }

    #[test]
    fn lossy_roundtrip_is_a_fixpoint() {
        // Lossy correctness without an external oracle: a decoded image already
        // sits on the quantization grid, so re-encoding it must yield a
        // byte-identical JXR (encode∘decode∘encode is a fixpoint). This holds
        // iff our forward quantizer is the exact inverse of the decoder's
        // dequant for every band. Mid-range pixels keep the reconstruction in
        // [0,255] so no clamping perturbs the second generation. Aligned sizes
        // only: windowing regenerates edge-padding from the (now lossy) edge, so
        // boundary MBs of a padded image aren't a fixpoint — a property of the
        // padding, not the quantizer.
        let mut r = Lcg(0x1357_9bdf);
        let qps = [
            QpSet { dc: 4, lp: 8, hp: 16 },
            QpSet { dc: 8, lp: 16, hp: 32 },
            QpSet { dc: 1, lp: 4, hp: 6 },
        ];
        for &(w, h) in &[(32u32, 32u32), (48, 32), (64, 48)] {
            let pixels: Vec<u8> = (0..(w * h) as usize).map(|_| 96 + (r.next() % 64) as u8).collect();
            for &qp in &qps {
                let input = ImageInput { width: w, height: h, planes: std::slice::from_ref(&pixels) };
                let jxr1 = encode(&input, ColorMode::Grayscale, qp).expect("encode");
                let dec1 = decode_to_planes(&jxr1);
                let p1: Vec<u8> = dec1.image_plane[0].iter().map(|&v| v.clamp(0, 255) as u8).collect();
                let input2 = ImageInput { width: w, height: h, planes: std::slice::from_ref(&p1) };
                let jxr2 = encode(&input2, ColorMode::Grayscale, qp).expect("re-encode");
                assert_eq!(jxr1, jxr2, "not a fixpoint at qp={qp:?} {w}x{h}");
            }
        }
    }

    #[test]
    fn lossy_error_grows_with_qp() {
        // Clean synthetic original with energy in every band: coarser quant ⇒
        // strictly more error. The fixpoint test only proves self-consistency;
        // this rules out a deadzone/rounding bug that would still round-trip but
        // quantize badly. (Monotonic on a clean master — unlike PSNR vs Amazon's
        // own already-quantized pixels.)
        let (w, h) = (64usize, 64usize);
        let pixels: Vec<u8> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as i32, (i / w) as i32);
                let v = 110 + (x % 17) - (y % 13) + ((x * y) % 11) * 3; // LP + HP energy
                v.clamp(0, 255) as u8
            })
            .collect();
        let mse = |qp: QpSet| -> f64 {
            let input =
                ImageInput { width: w as u32, height: h as u32, planes: std::slice::from_ref(&pixels) };
            let jxr = encode(&input, ColorMode::Grayscale, qp).expect("encode");
            let d = decode_to_planes(&jxr);
            let se: f64 = pixels
                .iter()
                .zip(d.image_plane[0].iter())
                .map(|(&r, &g)| {
                    let e = r as f64 - g.clamp(0, 255) as f64;
                    e * e
                })
                .sum();
            se / (w * h) as f64
        };
        let m0 = mse(QpSet::LOSSLESS);
        let m4 = mse(QpSet { dc: 16, lp: 16, hp: 16 }); // sf = 4
        let m8 = mse(QpSet { dc: 32, lp: 32, hp: 32 }); // sf = 8
        assert_eq!(m0, 0.0, "lossless must be exact");
        assert!(m4 > 0.0 && m8 > m4, "error must grow with QP: m0={m0} m4={m4} m8={m8}");
    }

    #[test]
    fn encode_color_via_public_api() {
        // 3-plane Color round-trips exact; 1-plane Color falls back to grayscale.
        let mut r = Lcg(0x1111_2222_3333_4444);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        // High bits: the LCG's low 8 bits alias at period 256, which would make
        // R==G==B (zero chroma) when n is a multiple of 256.
        let rp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let gp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let bp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let planes = [rp.clone(), gp.clone(), bp.clone()];
        let input = ImageInput { width: w, height: h, planes: &planes };
        let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).expect("color encode");
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 3, "3-plane color must emit RGB");
        for i in 0..n {
            assert_eq!(
                (d.image_plane[0][i], d.image_plane[1][i], d.image_plane[2][i]),
                (rp[i] as i32, gp[i] as i32, bp[i] as i32),
                "pixel {i}"
            );
        }
        // 1-plane in Color mode → grayscale (1 component, no synthesized chroma).
        let gplanes = [rp.clone()];
        let ginput = ImageInput { width: w, height: h, planes: &gplanes };
        let gjxr = encode(&ginput, ColorMode::Color, QpSet::LOSSLESS).expect("gray fallback");
        assert_eq!(decode_to_planes(&gjxr).num_components, 1, "1-plane color-mode ⇒ grayscale");
    }

    #[test]
    fn color_mode_auto_gray_detects_equal_channels() {
        // Three identical channels carry no chroma → must emit 8bppGray.
        let mut r = Lcg(0xaaaa_5555_cccc_3333);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let plane: Vec<u8> = (0..n).map(|_| (r.next() % 256) as u8).collect();
        let planes = [plane.clone(), plane.clone(), plane.clone()];
        let input = ImageInput { width: w, height: h, planes: &planes };
        let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 1, "equal RGB channels must auto-detect to grayscale");
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], plane[i] as i32, "pixel {i}");
        }
    }
}
