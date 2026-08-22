//! Image sanitization for KFX bundling: a non-JPEG raster (GIF, PNG, WebP, BMP)
//! re-encodes as JPEG; a JPEG keeps its image data and loses APP1–APP15 + COM.
//! An animated image keeps its first frame; SVG passes through.

use image::{DynamicImage, ImageReader};

/// Minimal JFIF APP0 segment (18 bytes including marker): version 1.01,
/// no units, 1×1 density, no thumbnail. Injected when a JPEG arrives
/// without an existing JFIF APP0 (e.g. EXIF-only camera/Photoshop output).
const JFIF_APP0: [u8; 18] = [
    0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01,
    0x00, 0x00,
];

/// Prepare a raster image for embedding in a KFX: a JPEG loses APP1–APP15 + COM
/// and gains a JFIF APP0; any other raster re-encodes as JFIF JPEG. `None` leaves
/// the caller its original bytes.
pub fn sanitize_for_kfx(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() {
        return None;
    }
    if is_jpeg(data) {
        return strip_jpeg_metadata(data);
    }
    let reader = ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let img = reader.decode().ok()?;
    encode_as_jpeg(&img)
}

/// Decode any raster image and re-encode it as a baseline RGB JPEG, with the
/// pixel dimensions — the `/DeviceRGB` + `/DCTDecode` a PDF Image XObject
/// declares. Alpha flattens onto white. `None` past JPEG's 65535px limit.
pub fn to_baseline_rgb_jpeg(data: &[u8], quality: u8) -> Option<(Vec<u8>, u32, u32)> {
    let reader = ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let img = reader.decode().ok()?;
    let (w, h) = (img.width(), img.height());
    let rgb = flatten_to_rgb(&img);
    let (w16, h16) = (u16::try_from(w).ok()?, u16::try_from(h).ok()?);
    if w16 == 0 || h16 == 0 {
        return None;
    }
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, quality)
        .encode(&rgb, w16, h16, jpeg_encoder::ColorType::Rgb)
        .ok()?;
    Some((out, w, h))
}

/// Walk a JPEG, drop APP1–APP15 and COM segments, and ensure a JFIF APP0 sits
/// right after SOI. Image data (SOS + entropy-coded scan) is copied verbatim.
/// `None` for non-JPEG, malformed, or JFIF-clean input.
pub fn strip_jpeg_metadata(data: &[u8]) -> Option<Vec<u8>> {
    if !is_jpeg(data) {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    let mut i = 2usize;
    let mut had_jfif = false;
    let mut had_metadata = false;
    while i < data.len() {
        // Every segment starts with one or more 0xFF fill bytes followed
        // by a non-0xFF marker byte. Anything else is malformed.
        if data[i] != 0xFF {
            return None;
        }
        while i < data.len() && data[i] == 0xFF {
            i += 1;
        }
        if i >= data.len() {
            return None;
        }
        let marker = data[i];
        i += 1;

        // EOI: header walk done.
        if marker == 0xD9 {
            out.extend_from_slice(&[0xFF, 0xD9]);
            break;
        }
        // SOS: copy SOS header, then the entropy-coded scan and trailing
        // EOI as a single slab. Byte-stuffed FF00 sequences and any
        // restart markers inside the scan don't need parsing here.
        if marker == 0xDA {
            let seg_len = u16::from_be_bytes(data.get(i..i + 2)?.try_into().ok()?) as usize;
            if i + seg_len > data.len() {
                return None;
            }
            out.extend_from_slice(&[0xFF, 0xDA]);
            out.extend_from_slice(&data[i..i + seg_len]);
            i += seg_len;
            out.extend_from_slice(&data[i..]);
            break;
        }

        // Length-prefixed segment.
        let seg_len = u16::from_be_bytes(data.get(i..i + 2)?.try_into().ok()?) as usize;
        if seg_len < 2 || i + seg_len > data.len() {
            return None;
        }
        let seg = &data[i..i + seg_len];
        i += seg_len;

        // Drop APP1–APP15 (camera/editor metadata, XMP, ICC, …) and COM.
        if (0xE1..=0xEF).contains(&marker) || marker == 0xFE {
            had_metadata = true;
            continue;
        }
        // Detect JFIF APP0 against a double injection below.
        if marker == 0xE0 && seg.len() >= 7 && &seg[2..7] == b"JFIF\0" {
            had_jfif = true;
        }
        out.extend_from_slice(&[0xFF, marker]);
        out.extend_from_slice(seg);
    }

    // Nothing dropped and a JFIF APP0 present: the input was clean, and `None`
    // keeps the original.
    if !had_metadata && had_jfif {
        return None;
    }

    if !had_jfif {
        // Inject minimal JFIF APP0 right after SOI.
        let mut prepended = Vec::with_capacity(out.len() + JFIF_APP0.len());
        prepended.extend_from_slice(&out[..2]);
        prepended.extend_from_slice(&JFIF_APP0);
        prepended.extend_from_slice(&out[2..]);
        return Some(prepended);
    }
    Some(out)
}

pub(crate) fn encode_as_jpeg(img: &DynamicImage) -> Option<Vec<u8>> {
    // Composite alpha onto white before encoding. JPEG has no alpha channel, and
    // a Kindle renderer does not composite transparency.
    let rgb_bytes = flatten_to_rgb(img);
    let width = u16::try_from(img.width()).ok()?;
    let height = u16::try_from(img.height()).ok()?;
    if width == 0 || height == 0 {
        return None;
    }

    let mut out: Vec<u8> = Vec::new();
    // Quality 90: line-art (gaiji, diagrams) stays crisp and ebook payloads stay
    // tight. `formats::kfx::jxr` re-encodes at 95.
    let encoder = jpeg_encoder::Encoder::new(&mut out, 90);
    encoder
        .encode(&rgb_bytes, width, height, jpeg_encoder::ColorType::Rgb)
        .ok()?;
    Some(out)
}

/// Composite an image's alpha channel over white, returning an opaque RGB8
/// image. JPEG carries no alpha and the KFX plate formats (8bppGray/24bppRGB)
/// carry none; a dropped channel turns transparent pixels their stored color.
pub(crate) fn flatten_alpha_over_white(img: &DynamicImage) -> DynamicImage {
    let rgb = flatten_to_rgb(img);
    let buf = image::RgbImage::from_raw(img.width(), img.height(), rgb)
        .expect("flatten_to_rgb returns w*h*3 bytes");
    DynamicImage::ImageRgb8(buf)
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
    fn empty_input_returns_none() {
        assert!(sanitize_for_kfx(&[]).is_none());
    }

    #[test]
    fn random_bytes_return_none() {
        assert!(sanitize_for_kfx(b"not an image at all").is_none());
    }

    #[test]
    fn clean_jfif_jpeg_returns_none() {
        // A clean JFIF-tagged JPEG (what the PNG transcode path emits) has no
        // metadata to strip, and sanitize returns `None`.
        let png = minimal_png();
        let jfif = sanitize_for_kfx(png).expect("PNG transcodes to JPEG");
        assert!(jfif.starts_with(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(sanitize_for_kfx(&jfif).is_none(), "clean JPEG → no change");
    }

    #[test]
    fn exif_app1_gets_stripped() {
        // Build: SOI + APP1(EXIF) + (rest of a clean JFIF JPEG starting
        // from its own APP0). The stripper should drop the APP1 segment
        // and keep the JFIF APP0 + image data.
        let mut tampered = vec![0xFF, 0xD8];
        // APP1 (FFE1) segment with the canonical "Exif\0\0" identifier
        // followed by a minimal TIFF header. Length 0x0E = 14 bytes
        // including the two length bytes themselves.
        tampered.extend_from_slice(&[
            0xFF, 0xE1, 0x00, 0x0E, b'E', b'x', b'i', b'f', 0x00, 0x00, b'I', b'I', 0x2A, 0x00,
            0x08, 0x00,
        ]);
        // Append a real JFIF JPEG payload starting AFTER its SOI.
        let clean = sanitize_for_kfx(minimal_png()).unwrap();
        tampered.extend_from_slice(&clean[2..]);

        let stripped = strip_jpeg_metadata(&tampered).expect("APP1 stripped");
        // No more APP1 anywhere in the result.
        assert!(
            !contains_subseq(&stripped, &[0xFF, 0xE1]),
            "APP1 marker should be gone after strip"
        );
        // JFIF APP0 sits right after SOI.
        assert_eq!(&stripped[..4], &[0xFF, 0xD8, 0xFF, 0xE0]);
    }

    #[test]
    fn jpeg_without_jfif_gets_jfif_injected() {
        // Build a JPEG that has ONLY APP1 EXIF (no JFIF APP0). The
        // stripper drops the APP1 and must inject a JFIF APP0.
        let mut tampered = vec![0xFF, 0xD8];
        tampered.extend_from_slice(&[
            0xFF, 0xE1, 0x00, 0x0E, b'E', b'x', b'i', b'f', 0x00, 0x00, b'I', b'I', 0x2A, 0x00,
            0x08, 0x00,
        ]);
        // Append the JFIF payload AFTER its own APP0 segment, leaving the input
        // with no APP0.
        let clean = sanitize_for_kfx(minimal_png()).unwrap();
        // The clean JPEG is [SOI, APP0(18 bytes), …]. Skip the APP0 too.
        let after_app0 = 2 + JFIF_APP0.len();
        tampered.extend_from_slice(&clean[after_app0..]);

        let stripped = strip_jpeg_metadata(&tampered).expect("APP1 stripped + APP0 injected");
        // Result starts SOI + APP0(JFIF).
        assert_eq!(&stripped[..4], &[0xFF, 0xD8, 0xFF, 0xE0]);
        assert_eq!(
            &stripped[4..11],
            &[0x00, 0x10, b'J', b'F', b'I', b'F', 0x00]
        );
        // APP1 is gone.
        assert!(!contains_subseq(&stripped, &[0xFF, 0xE1]));
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
        let out = sanitize_for_kfx(gif).expect("GIF decode succeeds");
        assert!(
            out.starts_with(&[0xFF, 0xD8]),
            "output starts with JPEG SOI"
        );
        assert!(
            out.len() > 100,
            "JPEG should be at least a few hundred bytes"
        );
    }

    #[test]
    fn minimal_png_transcodes_to_jpeg() {
        let out = sanitize_for_kfx(minimal_png()).expect("PNG decode succeeds");
        assert!(
            out.starts_with(&[0xFF, 0xD8]),
            "output starts with JPEG SOI"
        );
    }

    fn minimal_png() -> &'static [u8] {
        // The tiniest valid PNG: 1×1 transparent pixel. Sourced from
        // the canonical "smallest PNG" reference.
        &[
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
        ]
    }

    fn contains_subseq(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
