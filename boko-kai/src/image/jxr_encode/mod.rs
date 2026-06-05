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
/// codestream). `quality` is `0..=100` (maps to QP; 100 = lossless).
///
/// Scaffolding: the codestream stages land in Phases 1–4.
pub fn encode(
    _input: &ImageInput<'_>,
    _mode: ColorMode,
    _quality: u8,
) -> Result<Vec<u8>, EncodeError> {
    Err(EncodeError::NotImplemented(
        "codestream assembly (quant/entropy/container pending)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip oracle: decode JXR bytes straight to i32 planes via the
    /// decoder, bypassing its JPEG re-encode in `jxr_decode::transcode`.
    #[allow(dead_code)]
    fn decode_to_planes(jxr: &[u8]) -> crate::image::jxr_decode::decoder::DecodedImage {
        let container = crate::image::jxr_decode::container::parse(jxr).expect("container parse");
        crate::image::jxr_decode::decoder::Decoder::new(container.image_data)
            .decode()
            .expect("decode")
    }

    #[test]
    #[ignore = "encoder codestream not implemented yet (phases 1-4); harness wired, enable when encode() is real"]
    fn roundtrip_grayscale_pixels() {
        let (w, h) = (16u32, 16u32);
        let plane: Vec<u8> = (0..w * h).map(|i| (i % 251) as u8).collect();
        let input = ImageInput {
            width: w,
            height: h,
            planes: std::slice::from_ref(&plane),
        };
        let jxr = encode(&input, ColorMode::Grayscale, 100).expect("encode");
        let decoded = decode_to_planes(&jxr);
        assert_eq!((decoded.width, decoded.height), (w, h));
        for (i, &px) in plane.iter().enumerate() {
            assert_eq!(decoded.image_plane[0][i], px as i32, "pixel {i} mismatch");
        }
    }
}
