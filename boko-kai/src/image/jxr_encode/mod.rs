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
    if (w, h) != (16, 16) {
        return Err(EncodeError::Unsupported(
            "only a single 16×16 macroblock is supported so far".into(),
        ));
    }
    let luma = &input.planes[0];
    if luma.len() != 256 {
        return Err(EncodeError::Unsupported("plane len != width*height".into()));
    }

    // Pixels → samples (BD8 bias) → forward transform → DC coefficient.
    let mut samples = [0i32; 256];
    for (s, &px) in samples.iter_mut().zip(luma.iter()) {
        *s = px as i32 - 128;
    }
    let mb_buffer = transform::forward_transform_mb(&samples);
    let dc = mb_buffer[0]; // mb_dclp[0]; DC QP = 0 ⇒ coded value == this

    // Assemble the codestream.
    let mut bw = bitstream::BitWriter::new();
    codestream::write_image_header(&mut bw, w, h);
    codestream::write_image_plane_header_gray_dconly(&mut bw, 0); // dc_quant 0 ⇒ scaling 1
    codestream::write_vlw_esc(&mut bw, 0); // subsequent_bytes = 0 (no profile/level)
    codestream::write_common_tile_header(&mut bw);
    // Single top-left MB: NO_PREDICTION, fresh model (m_bits = (2-DC)*4 = 8),
    // fresh abs-level table (index 0).
    let model_bits = 8;
    let abs_table = crate::image::jxr_decode::tables::abs_level_index(0);
    coeff::encode_dc_value(&mut bw, dc, model_bits, abs_table);
    bw.align_to_byte(); // discard_remainder_bits after the tile

    let codestream = bw.finish();
    Ok(container::write_container(
        &codestream,
        w,
        h,
        &container::pixel_format::GRAY8,
    ))
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
        // DCONLY reconstructs a flat block exactly (LP/HP are zero): encode each
        // constant value, decode with the real decoder, expect identical pixels.
        for val in [128u8, 0, 255, 64, 100, 200] {
            let plane = vec![val; 256];
            let input = ImageInput {
                width: 16,
                height: 16,
                planes: std::slice::from_ref(&plane),
            };
            let jxr = encode(&input, ColorMode::Grayscale, 0).expect("encode");
            let decoded = decode_to_planes(&jxr);
            assert_eq!((decoded.width, decoded.height), (16, 16));
            for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                assert_eq!(got, val as i32, "val={val} pixel {i}");
            }
        }
    }
}
