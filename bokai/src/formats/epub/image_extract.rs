//! Pull every embedded image out of an EPUB, in memory.
//!
//! The EPUB analog of [`crate::formats::kfx::image_extract`]: where the KFX walk resolves
//! `external_resource` → `bcRawMedia` bytes, this walks the OPF manifest for
//! image media-types and reads each backing zip member through the shared
//! [`EpubPackage`] harness, flagging the book's declared cover. It serves the
//! editor's "extract one or two images" use case.
//!
//! EPUB images are already display-ready (JPEG/PNG/GIF/WebP/SVG), so — unlike
//! the KFX path, which transcodes JPEG-XR — every image passes through verbatim.
//! Pixel dimensions are sniffed from the header for the common raster formats
//! (the OPF manifest, unlike KFX resources, does not declare them).

use crate::formats::epub::edit::EpubPackage;
use crate::formats::epub::parse_opf;
use crate::util::percent_decode;

/// One image recovered from an EPUB.
#[derive(Debug)]
pub struct ExtractedImage {
    /// The full zip member path, e.g. `"OEBPS/images/fig1.jpg"` — a stable
    /// identifier for this image within the EPUB.
    pub path: String,
    /// The OPF manifest `id` that declares this image.
    pub manifest_id: Option<String>,
    /// The OPF-declared `media-type`, if non-empty (e.g. `"image/jpeg"`).
    pub media_type: Option<String>,
    /// Image bytes, verbatim from the zip — ready to write.
    pub bytes: Vec<u8>,
    /// Extension for `bytes`, no dot: `"jpg"`/`"png"`/`"gif"`/`"webp"`/`"svg"`/
    /// `"bmp"`/`"tiff"`.
    pub ext: &'static str,
    /// Pixel width, sniffed from the image header when the format allows.
    pub width: Option<u32>,
    /// Pixel height, sniffed from the image header when the format allows.
    pub height: Option<u32>,
    /// True if this is the book's declared cover image.
    pub is_cover: bool,
}

/// Extract every embedded image from an in-memory EPUB.
///
/// Returns the images sorted by member `path` for a stable order. An image is
/// any OPF manifest item with an `image/*` media-type (or, when the media-type
/// is missing/unrecognized, whose bytes sniff as a known raster format);
/// non-image members are skipped. Errors only when the bytes aren't a readable
/// EPUB zip.
pub fn epub_extract_images(epub_bytes: &[u8]) -> std::io::Result<Vec<ExtractedImage>> {
    let pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf_base = opf_dir_base(&opf_path);
    let opf_raw = pkg.opf_bytes()?;
    let hint = crate::util::extract_xml_encoding(opf_raw);
    let opf_str = crate::util::decode_text(opf_raw, hint);
    let opf = parse_opf(&opf_str)?;

    // The declared cover resolved to an absolute (zip-relative) member path, so
    // `is_cover` is matched by path. `parse_opf` leaves `cover_image` as the
    // manifest href relative to the OPF's directory (see the importer).
    let cover_path = opf
        .metadata
        .cover_image
        .as_deref()
        .filter(|h| !h.is_empty())
        .map(|href| format!("{opf_base}{}", percent_decode(href)));

    let mut out: Vec<ExtractedImage> = Vec::new();
    for (id, (href, media_type)) in &opf.manifest {
        let path = format!("{opf_base}{}", percent_decode(href));
        let Some(bytes) = pkg.get(&path) else {
            continue; // manifest references a member the zip doesn't contain
        };
        // `ext_for` returns Some only for recognized images (by media-type or
        // magic bytes), so it doubles as the "is this an image?" filter.
        let Some(ext) = ext_for(media_type, bytes) else {
            continue;
        };
        let (width, height) = match image_dimensions(bytes) {
            Some((w, h)) => (Some(w), Some(h)),
            None => (None, None),
        };
        out.push(ExtractedImage {
            is_cover: cover_path.as_deref() == Some(path.as_str()),
            path,
            manifest_id: Some(id.clone()),
            media_type: (!media_type.is_empty()).then(|| media_type.clone()),
            bytes: bytes.to_vec(),
            ext,
            width,
            height,
        });
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Directory portion of a zip member path, with a trailing `/` (empty at the
/// archive root). Member names are always `/`-delimited. Mirrors the importer's
/// `archive_dir_base`, used to resolve manifest hrefs against the OPF's dir.
fn opf_dir_base(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/"),
        None => String::new(),
    }
}

/// The export extension for a manifest item, or `None` when it isn't an image.
/// Prefers the declared media-type; falls back to sniffing the bytes when the
/// type is missing or unrecognized (some manifests mislabel resources).
fn ext_for(media_type: &str, bytes: &[u8]) -> Option<&'static str> {
    match media_type.trim() {
        "image/jpeg" | "image/jpg" | "image/pjpeg" => return Some("jpg"),
        "image/png" => return Some("png"),
        "image/gif" => return Some("gif"),
        "image/webp" => return Some("webp"),
        "image/svg+xml" => return Some("svg"),
        "image/bmp" | "image/x-ms-bmp" | "image/x-bmp" => return Some("bmp"),
        "image/tiff" | "image/x-tiff" => return Some("tiff"),
        _ => {}
    }
    sniff_raster(bytes)
}

/// Recognize a raster image by its magic bytes, returning its extension. An
/// unrecognized payload (XHTML, CSS, a font) returns `None` and is skipped.
fn sniff_raster(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 3 && bytes[..3] == [0xFF, 0xD8, 0xFF] {
        return Some("jpg");
    }
    if bytes.len() >= 8 && bytes[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("png");
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some("gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    if bytes.len() >= 2 && &bytes[..2] == b"BM" {
        return Some("bmp");
    }
    None
}

/// Sniff pixel dimensions from a raster header (PNG, GIF, JPEG). `None` for
/// formats without a trivially-parsed intrinsic size (SVG, WebP, BMP, TIFF).
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    png_dimensions(bytes)
        .or_else(|| gif_dimensions(bytes))
        .or_else(|| jpeg_dimensions(bytes))
}

fn png_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    // 8-byte signature, then the IHDR chunk: 4-byte length, `IHDR`, then the
    // width and height as big-endian u32.
    const SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    if b.len() < 24 || b[..8] != SIG || &b[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
    let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
    Some((w, h))
}

fn gif_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    // `GIF87a`/`GIF89a`, then the logical-screen width/height as little-endian u16.
    if b.len() < 10 || (&b[..6] != b"GIF87a" && &b[..6] != b"GIF89a") {
        return None;
    }
    let w = u16::from_le_bytes([b[6], b[7]]) as u32;
    let h = u16::from_le_bytes([b[8], b[9]]) as u32;
    Some((w, h))
}

fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    // Walk marker segments from the SOI (FFD8) to the first Start-Of-Frame
    // (SOFn), whose header carries the frame height then width (big-endian u16).
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 1 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        // Collapse fill runs of 0xFF to land on the marker code byte.
        let mut marker = b[i + 1];
        i += 1;
        while marker == 0xFF && i + 1 < b.len() {
            i += 1;
            marker = b[i];
        }
        match marker {
            // Standalone markers carry no length/payload.
            0x01 | 0xD0..=0xD7 | 0xD8 | 0xD9 => i += 1,
            // Start-of-scan: entropy data begins, no SOF was found.
            0xDA => return None,
            _ => {
                if i + 2 >= b.len() {
                    return None;
                }
                let len = u16::from_be_bytes([b[i + 1], b[i + 2]]) as usize;
                if len < 2 {
                    return None;
                }
                // SOF0..SOF15 hold the frame size, excluding DHT (C4)/JPG
                // (C8)/DAC (CC), which share the 0xCn range.
                if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                    if i + 8 > b.len() {
                        return None;
                    }
                    let h = u16::from_be_bytes([b[i + 4], b[i + 5]]) as u32;
                    let w = u16::from_be_bytes([b[i + 6], b[i + 7]]) as u32;
                    return Some((w, h));
                }
                i += 1 + len; // marker byte + its [length + payload]
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

    /// Extraction on the real fixture: its one image is `OEBPS/cover.jpeg`,
    /// flagged as the cover, with a valid extension, non-empty bytes, and
    /// header-sniffed dimensions.
    #[test]
    fn extracts_cover_from_fixture() {
        let epub = std::fs::read(FIXTURE).expect("read fixture");
        let images = epub_extract_images(&epub).expect("valid EPUB");

        assert_eq!(images.len(), 1, "fixture has exactly one image");
        let img = &images[0];
        assert_eq!(img.path, "OEBPS/cover.jpeg");
        assert_eq!(img.ext, "jpg");
        assert_eq!(img.media_type.as_deref(), Some("image/jpeg"));
        assert!(img.is_cover, "the one image is the declared cover");
        assert!(!img.bytes.is_empty(), "cover has bytes");
        assert_eq!(
            &img.bytes[..3],
            &[0xFF, 0xD8, 0xFF],
            "bytes are a real JPEG, verbatim"
        );
        let (w, h) = (img.width.expect("width"), img.height.expect("height"));
        assert!(w > 0 && h > 0, "JPEG dimensions sniffed: {w}x{h}");
    }

    #[test]
    fn extracted_cover_bytes_match_the_zip_member() {
        let epub = std::fs::read(FIXTURE).expect("read fixture");
        let pkg = EpubPackage::parse(&epub).expect("parse");
        let member = pkg.get("OEBPS/cover.jpeg").expect("cover member").to_vec();
        let images = epub_extract_images(&epub).expect("valid EPUB");
        assert_eq!(images[0].bytes, member, "cover extracted byte-for-byte");
    }

    #[test]
    fn non_epub_bytes_error() {
        assert!(epub_extract_images(b"not an epub").is_err());
    }

    #[test]
    fn sniff_raster_recognizes_and_rejects() {
        assert_eq!(sniff_raster(&[0xFF, 0xD8, 0xFF, 0x00]), Some("jpg"));
        assert_eq!(
            sniff_raster(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("png")
        );
        assert_eq!(sniff_raster(b"GIF89a....."), Some("gif"));
        assert_eq!(sniff_raster(b"RIFF____WEBP...."), Some("webp"));
        assert_eq!(sniff_raster(b"BM......"), Some("bmp"));
        assert_eq!(sniff_raster(b"<?xml version"), None);
        assert_eq!(sniff_raster(b"\x00\x01\x00\x00ttf"), None);
    }

    #[test]
    fn ext_for_prefers_declared_media_type() {
        assert_eq!(ext_for("image/png", &[0xFF, 0xD8, 0xFF]), Some("png"));
        assert_eq!(ext_for("image/svg+xml", b"<svg>"), Some("svg"));
        // Missing/odd type → sniff the bytes.
        assert_eq!(ext_for("", &[0xFF, 0xD8, 0xFF]), Some("jpg"));
        // Non-image members are rejected.
        assert_eq!(ext_for("application/xhtml+xml", b"<html>"), None);
        assert_eq!(ext_for("text/css", b"body{}"), None);
    }

    #[test]
    fn dimension_sniffers_read_synthetic_headers() {
        // PNG: signature + IHDR length + "IHDR" + 800x600 (big-endian).
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&800u32.to_be_bytes());
        png.extend_from_slice(&600u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((800, 600)));

        // GIF: header + 320x240 (little-endian).
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&240u16.to_le_bytes());
        assert_eq!(gif_dimensions(&gif), Some((320, 240)));

        // JPEG: SOI, an APP0 segment to skip, then SOF0 with 128x256.
        let mut jpg = vec![0xFF, 0xD8];
        jpg.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]); // APP0 len=4
        jpg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]); // SOF0, len, precision
        jpg.extend_from_slice(&256u16.to_be_bytes()); // height
        jpg.extend_from_slice(&128u16.to_be_bytes()); // width
        assert_eq!(jpeg_dimensions(&jpg), Some((128, 256)));
    }
}
