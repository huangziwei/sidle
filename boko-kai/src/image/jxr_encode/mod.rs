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
    use crate::image::jxr_decode::tables::abs_level_index;
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
    if w == 0 || h == 0 || w % 16 != 0 || h % 16 != 0 {
        return Err(EncodeError::Unsupported(
            "dims must be positive multiples of 16 (windowing not yet implemented)".into(),
        ));
    }
    let (wu, hu) = (w as usize, h as usize);
    let luma = &input.planes[0];
    if luma.len() != wu * hu {
        return Err(EncodeError::Unsupported("plane len != width*height".into()));
    }
    let (mbw, mbh) = (wu / 16, hu / 16);

    // Forward transform every macroblock → its DC coefficient (DC QP = 0 ⇒ the
    // coded DC equals this). `dc[mbx][mby]` is the final/actual DC, which is
    // also what the decoder's neighbour prediction reads back.
    let mut dc = vec![vec![0i32; mbh]; mbw];
    for mbx in 0..mbw {
        for mby in 0..mbh {
            let mut samples = [0i32; 256];
            for py in 0..16 {
                for px in 0..16 {
                    let g = (mby * 16 + py) * wu + (mbx * 16 + px);
                    samples[py * 16 + px] = luma[g] as i32 - 128;
                }
            }
            dc[mbx][mby] = transform::forward_transform_mb(&samples)[0];
        }
    }

    let mut bw = bitstream::BitWriter::new();
    codestream::write_image_header(&mut bw, w, h);
    codestream::write_image_plane_header_gray_dconly(&mut bw, 0); // dc_quant 0 ⇒ scaling 1
    codestream::write_vlw_esc(&mut bw, 0); // subsequent_bytes = 0 (no profile/level)
    codestream::write_common_tile_header(&mut bw);

    // DC band, raster order (mby outer, mbx inner), single tile. Model and
    // abs-level table evolve exactly as the decoder's mb_dc.
    let mut model = coeff::ModelState::init(0); // DC band ⇒ m_bits = 8
    let mut vlc = coeff::AdaptiveVlc1::default(); // table_index 0, discrim 0
    const ABS_DELTA: [i32; 7] = [1, 0, -1, -1, -1, -1, -1]; // ABS_LEVEL_INDEX_DELTA[0]
    for mby in 0..mbh {
        for mbx in 0..mbw {
            let actual = dc[mbx][mby];
            let (is_left, is_top) = (mbx == 0, mby == 0);
            let predictor = if is_left && is_top {
                0 // NO_PREDICTION
            } else if is_left {
                dc[mbx][mby - 1] // PREDICT_FROM_TOP
            } else if is_top {
                dc[mbx - 1][mby] // PREDICT_FROM_LEFT
            } else {
                let (left, top, tl) = (dc[mbx - 1][mby], dc[mbx][mby - 1], dc[mbx - 1][mby - 1]);
                let (sh, sv) = ((tl - left).abs(), (tl - top).abs());
                if sh * 4 < sv {
                    top
                } else if sv * 4 < sh {
                    left
                } else {
                    (top + left) >> 1 // PREDICT_FROM_TOP_LEFT
                }
            };
            let residual = actual - predictor;
            let (b_abs, abs_idx) =
                coeff::encode_dc_value(&mut bw, residual, model.m_bits, abs_level_index(vlc.table_index as usize));
            let lap = if b_abs {
                vlc.discrim += ABS_DELTA[abs_idx as usize];
                1
            } else {
                0
            };
            model.update(lap, 0);
            if mbx % 16 == 0 || mbx == mbw - 1 {
                vlc.adapt(); // reset_context
            }
        }
    }
    bw.align_to_byte(); // discard_remainder_bits after the tile
    Ok(container::write_container(
        &bw.finish(),
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
}
