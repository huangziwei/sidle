//! Pure-Rust JPEG-XR decoder + JPEG re-encode for the KFX → EPUB mechanical
//! port.
//!
//! ## Layout
//!
//! - `misc` — bit/byte stream reader used by both the container and
//!   codestream parsers.
//! - `container` — TIFF-like outer file parser (extracts the WMPHOTO
//!   codestream bytes + image dimensions / pixel format UUID).
//! - `consts` / `tables` — constants and Huffman tables from the spec.
//! - `math` — IDCT / butterfly / overlap-filter primitives.
//! - `state` — plane / MB / adaptive-VLC structs.
//! - `decoder` — the codestream decoder pipeline.
//!
//! ## Why pure Rust
//!
//! KFX raw media is `image/jxr` for most images in modern Amazon files
//! (Japanese light novels especially). EPUB readers don't support JXR;
//! calibre transcodes via Pillow → libjxr. We don't want a C FFI dep or
//! a system magick binary for sidle/Tauri distribution, so we ported
//! calibre's pure-Python decoder line-by-line.

pub mod consts;
pub mod container;
pub mod decoder;
pub mod math;
pub mod misc;
pub mod state;
pub mod tables;

use super::ConvertError;

/// Transcode a JPEG-XR file (with outer TIFF container) into a JPEG byte
/// stream. Returns `(bytes, format_symbol)` where `format_symbol` is one of
/// `"jpg"` / `"png"` (matching `resources::FORMAT_*`).
///
/// Per-stage timing for one transcode call. Always collected — `Instant`'s
/// read cost is ~10 ns. Caller may aggregate or ignore.
#[derive(Debug, Default, Clone, Copy)]
pub struct TranscodeTiming {
    pub container_parse: std::time::Duration,
    pub jxr_decode: std::time::Duration,
    pub jpeg_encode: std::time::Duration,
    /// JXR decoder sub-stages (sums to ~`jxr_decode`).
    pub jxr_decode_breakdown: decoder::DecodeTiming,
}

/// On any decoder failure the original bytes pass through with format
/// `"jxr"` so the caller can decide whether to error or bundle as-is.
pub fn transcode(
    jxr_bytes: &[u8],
    resource_name: &str,
) -> Result<(Vec<u8>, String, TranscodeTiming), ConvertError> {
    use std::time::Instant;
    let mut t = TranscodeTiming::default();

    let t0 = Instant::now();
    let container = match container::parse(jxr_bytes) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "kfx_to_epub jxr: container parse failed for {resource_name}: {e}; passing through"
            );
            return Ok((jxr_bytes.to_vec(), "jxr".into(), t));
        }
    };
    t.container_parse = t0.elapsed();

    let t1 = Instant::now();
    let decoded = match decoder::Decoder::new(container.image_data).decode() {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "kfx_to_epub jxr: decode failed for {resource_name}: {e}; passing through"
            );
            return Ok((jxr_bytes.to_vec(), "jxr".into(), t));
        }
    };
    t.jxr_decode = t1.elapsed();
    t.jxr_decode_breakdown = decoded.timing;

    let t2 = Instant::now();
    let bytes = encode_jpeg(&decoded)?;
    t.jpeg_encode = t2.elapsed();
    Ok((bytes, "jpg".into(), t))
}

fn encode_jpeg(img: &decoder::DecodedImage) -> Result<Vec<u8>, ConvertError> {
    use jpeg_encoder::Encoder;
    use consts::*;

    // We currently only emit JPEG for 8-bit RGB / Y outputs. Higher bit
    // depths and RGBA either need PNG or precision reduction.
    if img.output_bitdepth != BD8 {
        return Err(ConvertError::JpegEncode(format!(
            "unsupported bitdepth {} for JPEG re-encode",
            img.output_bitdepth
        )));
    }

    let (rgb_bytes, color) = pack_pixels(img)?;

    let mut out: Vec<u8> = Vec::new();
    let encoder = Encoder::new(&mut out, 95);
    encoder
        .encode(&rgb_bytes, img.width as u16, img.height as u16, color)
        .map_err(|e| ConvertError::JpegEncode(format!("{:?}", e)))?;
    Ok(out)
}

/// Pack the decoded i32 planes into the contiguous byte buffer that
/// jpeg-encoder expects, picking the right ColorType for the layout.
fn pack_pixels(
    img: &decoder::DecodedImage,
) -> Result<(Vec<u8>, jpeg_encoder::ColorType), ConvertError> {
    use jpeg_encoder::ColorType;
    use consts::*;

    let w = img.width as usize;
    let h = img.height as usize;

    // YONLY -> Luma. RGB / NCOMPONENT(3) -> Rgb. RGBA-shaped images cannot
    // round-trip through JPEG; we drop alpha to keep things simple — no
    // JXR+alpha image has shown up in a real fixture yet. Revisit when
    // one appears.
    let is_rgb_like = matches!(img.output_clr_fmt, OUT_RGB)
        || (matches!(img.output_clr_fmt, OUT_NCOMPONENT) && img.num_components >= 3);
    let is_y_like = matches!(img.output_clr_fmt, OUT_YONLY)
        || (matches!(img.output_clr_fmt, OUT_NCOMPONENT) && img.num_components == 1);

    if is_y_like {
        let mut buf = vec![0u8; w * h];
        let plane = &img.image_plane[0];
        for i in 0..w * h {
            buf[i] = clamp_u8(plane[i]);
        }
        Ok((buf, ColorType::Luma))
    } else if is_rgb_like {
        let mut buf = vec![0u8; w * h * 3];
        let r = &img.image_plane[0];
        let g = &img.image_plane[1];
        let b = &img.image_plane[2];
        for i in 0..w * h {
            buf[i * 3] = clamp_u8(r[i]);
            buf[i * 3 + 1] = clamp_u8(g[i]);
            buf[i * 3 + 2] = clamp_u8(b[i]);
        }
        Ok((buf, ColorType::Rgb))
    } else {
        Err(ConvertError::JpegEncode(format!(
            "unsupported color layout: clr_fmt={} num_components={}",
            img.output_clr_fmt, img.num_components
        )))
    }
}

#[inline]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}
