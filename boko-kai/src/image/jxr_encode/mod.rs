//! Pure-Rust JPEG-XR encoder (EPUB→KFX), dual grayscale/color.
//!
//! Forward mirror of [`crate::image::jxr_decode`]. Built in phases against the
//! existing decoder as a round-trip oracle — see `.claude/plans/jxr-encoder.md`.
//!
//! Status: scaffolding + Phase 1 forward core-transform ([`transform`]). The
//! quantizer, entropy coder, and container writer land in later phases, after
//! which [`encode`] becomes real and the `#[ignore]`d round-trip test below is
//! enabled.

pub mod bitstream;
pub mod codestream;
pub mod container;
pub mod coeff;
pub mod entropy;
pub mod gray;
pub mod hp;
pub mod transform;

/// How color is handled on the way into the KFX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Single luma plane (`8bppGray`). Default: every Kindle we target is B&W
    /// e-ink, and the source EPUB retains the color master, so color is only
    /// dropped from the device copy and is recoverable by reconverting.
    Grayscale,
    /// Full color via the internal color transform + chroma planes
    /// (`24bppRGB`). Not yet implemented (Phase 6 — gated on a color device).
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

/// Encode 8-bit pixels into a JPEG-XR file (TIFF container + WMPHOTO
/// codestream). `quality` is reserved (lossless DC for now).
///
/// Current support: grayscale, a single 16×16 macroblock, **DCONLY** (so it is
/// exact only for flat blocks — the LP/HP bands and multi-MB land next). This
/// is the first end-to-end slice: container + headers + forward transform + DC
/// coding, decodable by `jxr_decode`.
pub fn encode(
    input: &ImageInput<'_>,
    mode: ColorMode,
    _quality: u8,
) -> Result<Vec<u8>, EncodeError> {
    if mode == ColorMode::Color {
        return Err(EncodeError::NotImplemented("color mode (Phase 6)"));
    }
    if input.planes.len() != 1 {
        return Err(EncodeError::Unsupported(format!(
            "grayscale expects 1 plane, got {}",
            input.planes.len()
        )));
    }
    let (w, h) = (input.width, input.height);
    if w == 0 || h == 0 {
        return Err(EncodeError::Unsupported("zero-size image".into()));
    }
    if w > 1 << 16 || h > 1 << 16 {
        return Err(EncodeError::Unsupported("dims exceed short-header u16 range".into()));
    }
    let (wu, hu) = (w as usize, h as usize);
    let luma = &input.planes[0];
    if luma.len() != wu * hu {
        return Err(EncodeError::Unsupported("plane len != width*height".into()));
    }
    // DC + LP + HP (ALL_BANDS): lossless for any grayscale image.
    Ok(gray::encode_grayscale(luma, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip oracle: decode JXR bytes straight to i32 planes via the
    /// decoder, bypassing its JPEG re-encode in `jxr_decode::transcode`.
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
                let jxr = encode(&input, ColorMode::Grayscale, 0).expect("encode");
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
            let jxr = encode(&input, ColorMode::Grayscale, 0).expect("encode");
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
            let jxr = encode(&input, ColorMode::Grayscale, 0).expect("encode");
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
            let jxr = encode(&input, ColorMode::Grayscale, 0).expect("encode");
            let decoded = decode_to_planes(&jxr);
            assert_eq!((decoded.width, decoded.height), (w, h));
            for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                assert_eq!(got, pixels[i] as i32, "w={w} h={h} pixel {i}");
            }
        }
    }
}
