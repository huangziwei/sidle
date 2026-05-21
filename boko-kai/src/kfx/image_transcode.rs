//! Image transcoding for KFX bundling.
//!
//! Kindle's reflowable renderer is finicky about embedded image formats:
//! KDP's "Image Guidelines — Reflowable" explicitly warns that multi-frame
//! GIFs and images with transparent areas don't render correctly, and
//! Amazon's authoritative KFX pipeline transcodes every input image to
//! JPEG-XR on their backend. We don't have a JPEG-XR encoder on hand, but
//! plain JPEG is a KFX-supported format and renders consistently on every
//! Kindle reader, so this module re-encodes any non-JPEG image (GIF, PNG,
//! WebP, BMP) as JPEG before the EPUB→KFX export bundles it.
//!
//! Animated images keep only the first frame — the device can't play
//! frames anyway and most CJK ebook GIFs are single-frame (gaiji glyphs,
//! photo-style illustrations).
//!
//! SVG is not transcoded here: it's already vector, and the export path
//! already has a `MediaFormat::Svg` arm. JXR is handled separately in
//! `kfx_to_epub/jxr/mod.rs` for the reverse direction.

use image::{DynamicImage, ImageReader};

/// Re-encode any non-JPEG raster image (GIF, PNG, WebP, BMP) as JPEG.
/// Returns `None` for JPEG input (no work needed), SVG (vector — caller
/// should bundle as-is), unknown formats, or any decode/encode failure.
/// On failure the caller should fall back to the original bytes — KFX
/// may not render them well, but at least the resource is still bundled.
pub fn transcode_to_jpeg(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || is_jpeg(data) {
        return None;
    }

    let reader = ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let img = reader.decode().ok()?;

    encode_as_jpeg(&img)
}

fn encode_as_jpeg(img: &DynamicImage) -> Option<Vec<u8>> {
    // Composite alpha onto white before encoding. JPEG has no alpha
    // channel, and Kindle's renderer is documented as not compositing
    // transparency correctly — flattening here matches what a publisher
    // pipeline would do and avoids surprise gray fringes around gaiji
    // glyphs that come with a soft alpha edge.
    let rgb_bytes = flatten_to_rgb(img);
    let width = u16::try_from(img.width()).ok()?;
    let height = u16::try_from(img.height()).ok()?;
    if width == 0 || height == 0 {
        return None;
    }

    let mut out: Vec<u8> = Vec::new();
    // Quality 90: high enough that line-art (gaiji, diagrams) stays
    // crisp, low enough that ebook payloads stay tight. Matches the
    // JXR→JPEG re-encode quality used by `kfx_to_epub/jxr/mod.rs`
    // (which uses 95) within a hair — we drop 5 points because source
    // GIF/PNG content is often already palette-quantised or downsampled,
    // making the extra precision wasted.
    let encoder = jpeg_encoder::Encoder::new(&mut out, 90);
    encoder
        .encode(&rgb_bytes, width, height, jpeg_encoder::ColorType::Rgb)
        .ok()?;
    Some(out)
}

fn flatten_to_rgb(img: &DynamicImage) -> Vec<u8> {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 3);
    for pixel in rgba.pixels() {
        let [r, g, b, a] = pixel.0;
        // Alpha-over-white composite: result = src * alpha + 255 * (1-alpha)
        let a_f = a as u32;
        let inv = 255 - a_f;
        let r = ((r as u32 * a_f + 255 * inv + 127) / 255) as u8;
        let g = ((g as u32 * a_f + 255 * inv + 127) / 255) as u8;
        let b = ((b as u32 * a_f + 255 * inv + 127) / 255) as u8;
        out.push(r);
        out.push(g);
        out.push(b);
    }
    out
}

fn is_jpeg(data: &[u8]) -> bool {
    data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_input_returns_none() {
        // Real JPEG SOI marker + APP0 — `transcode_to_jpeg` should
        // short-circuit so the caller bundles the original bytes.
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert!(transcode_to_jpeg(&jpeg).is_none());
    }

    #[test]
    fn empty_input_returns_none() {
        assert!(transcode_to_jpeg(&[]).is_none());
    }

    #[test]
    fn random_bytes_return_none() {
        assert!(transcode_to_jpeg(b"not an image at all").is_none());
    }

    #[test]
    fn minimal_static_gif_round_trips_to_jpeg() {
        // Hand-crafted 1×1 GIF89a, single solid pixel.
        let gif: &[u8] = &[
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61, // GIF89a
            0x01, 0x00, 0x01, 0x00, // 1×1 logical screen
            0x80, 0x00, 0x00, // GCT flag, bg=0, no aspect
            0xFF, 0x00, 0x00, // color 0 = red
            0x00, 0x00, 0x00, // color 1 = black
            0x2C, // image separator
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, // image desc
            0x02, 0x02, 0x44, 0x01, 0x00, // image data (LZW)
            0x3B, // trailer
        ];
        let out = transcode_to_jpeg(gif).expect("GIF decode succeeds");
        assert!(out.starts_with(&[0xFF, 0xD8]), "output starts with JPEG SOI");
        assert!(out.len() > 100, "JPEG should be at least a few hundred bytes");
    }

    #[test]
    fn minimal_png_transcodes_to_jpeg() {
        // The tiniest valid PNG: 1×1 transparent pixel. Sourced from
        // the canonical "smallest PNG" reference.
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG magic
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            0x49, 0x48, 0x44, 0x52, // IHDR
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1×1
            0x08, 0x06, 0x00, 0x00, 0x00, // 8-bit RGBA
            0x1F, 0x15, 0xC4, 0x89, // IHDR CRC
            0x00, 0x00, 0x00, 0x0A, // IDAT length
            0x49, 0x44, 0x41, 0x54, // IDAT
            0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, // zlib
            0x0D, 0x0A, 0x2D, 0xB4, // IDAT CRC
            0x00, 0x00, 0x00, 0x00, // IEND length
            0x49, 0x45, 0x4E, 0x44, // IEND
            0xAE, 0x42, 0x60, 0x82, // IEND CRC
        ];
        let out = transcode_to_jpeg(png).expect("PNG decode succeeds");
        assert!(out.starts_with(&[0xFF, 0xD8]), "output starts with JPEG SOI");
    }
}
