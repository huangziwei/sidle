//! KFX → EPUB glue: decode a JPEG-XR file with the pure-Rust [`jxr`] codec
//! crate, then re-encode it as JPEG for EPUB readers (which don't support
//! JXR).
//!
//! This is boko-kai pipeline glue, **not** part of the codec: it depends on
//! `ConvertError`, `jpeg_encoder`, and the `BOKO_KFX2EPUB_TRACE` timing, none
//! of which belong in the standalone `jxr` crate.

use jxr::decode::{container, decoder};
use crate::kfx_to_epub::ConvertError;

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

/// Transcode a JPEG-XR file (with outer TIFF container) into a JPEG byte
/// stream. Returns `(bytes, format_symbol, timing)` where `format_symbol` is
/// one of `"jpg"` / `"jxr"` (matching `resources::FORMAT_*`).
///
/// On any decoder failure the original bytes pass through with format
/// `"jxr"` so the caller can decide whether to error or bundle as-is.
pub fn transcode(
    jxr_bytes: &[u8],
    resource_name: &str,
) -> Result<(Vec<u8>, String, TranscodeTiming), ConvertError> {
    use crate::trace::Stopwatch;
    let mut t = TranscodeTiming::default();

    let t0 = Stopwatch::start();
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

    let t1 = Stopwatch::start();
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

    let t2 = Stopwatch::start();
    let bytes = encode_jpeg(&decoded)?;
    t.jpeg_encode = t2.elapsed();
    Ok((bytes, "jpg".into(), t))
}

fn encode_jpeg(img: &decoder::DecodedImage) -> Result<Vec<u8>, ConvertError> {
    use jpeg_encoder::{ColorType, Encoder};
    use jxr::decode::pixels::{ColorModel, SampleType};

    let buf = img
        .to_pixel_buffer()
        .map_err(|e| ConvertError::JpegEncode(e.to_string()))?;

    // We only emit JPEG for 8-bit gray / RGB-shaped layouts. Higher bit
    // depths, packed, CMYK, RGBE etc. pass through (caller bundles as-is).
    if buf.sample != SampleType::U8 {
        return Err(ConvertError::JpegEncode(format!(
            "unsupported sample type {:?} for JPEG re-encode",
            buf.sample
        )));
    }
    let n_color: usize = match buf.color {
        ColorModel::Gray => 1,
        ColorModel::Rgb => 3,
        // N-channel: treat 3+ as RGB, 1 as gray (legacy behavior).
        ColorModel::NChannel(k) if k >= 3 => 3,
        ColorModel::NChannel(1) => 1,
        other => {
            return Err(ConvertError::JpegEncode(format!(
                "unsupported color layout {other:?} for JPEG re-encode"
            )));
        }
    };
    // Strip alpha / extra channels: JPEG can't carry them.
    let ch = buf.channels as usize;
    let bytes: Vec<u8> = if ch == n_color {
        buf.data
    } else {
        buf.data
            .chunks_exact(ch)
            .flat_map(|px| px[..n_color].iter().copied())
            .collect()
    };
    let color = if n_color == 1 { ColorType::Luma } else { ColorType::Rgb };

    let mut out: Vec<u8> = Vec::new();
    let encoder = Encoder::new(&mut out, 95);
    encoder
        .encode(&bytes, img.width as u16, img.height as u16, color)
        .map_err(|e| ConvertError::JpegEncode(format!("{:?}", e)))?;
    Ok(out)
}
