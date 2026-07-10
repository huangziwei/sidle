//! Pull *every* embedded image out of a KFX container, in memory.
//!
//! The bulk generalization of [`super::cover_extract`]: where that recovers the
//! one declared cover, this walks every `external_resource` ($164) with a
//! raster-image `format`, resolves its backing `bcRawMedia` ($417) bytes through
//! the same [`kfx_to_epub::loader`] (correct dynamic doc-symbol `base_len`), and
//! returns them ready to write. This serves the editor's "extract one or two
//! images" use case.
//!
//! JPEG-XR resources are transcoded to JPEG (mirroring the reader / EPUB
//! resource pass); every other image format passes through verbatim. Non-image
//! resources (fonts, an embedded PDF page) and JPEG-XR that fails to decode are
//! skipped — a valid container with no images yields an empty list, not an error.

use std::collections::HashSet;

use crate::image::jxr_transcode as transcode;
use crate::kfx::container::get_field;
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;
use crate::kfx_to_epub::ConvertError;
use crate::kfx_to_epub::loader;

/// One image recovered from a KFX container.
#[derive(Debug)]
pub struct ExtractedImage {
    /// The KFX `resource_name` (e.g. `"eF"` or `"resource/rsrc7"`) — a stable
    /// identifier for this image within the container. Falls back to the backing
    /// location when the resource declared no name.
    pub resource_name: String,
    /// The backing `bcRawMedia` location key.
    pub location: String,
    /// Image bytes, ready to write. JPEG-XR has been transcoded to JPEG.
    pub bytes: Vec<u8>,
    /// Extension for `bytes`, no dot: `"jpg"`/`"png"`/`"gif"`/`"webp"`/`"bmp"`.
    pub ext: &'static str,
    /// Declared pixel width, if the resource carried `resource_width`.
    pub width: Option<u32>,
    /// Declared pixel height, if the resource carried `resource_height`.
    pub height: Option<u32>,
    /// True if this resource is the book's declared cover (matched by backing
    /// location, so it holds even when the cover is referenced under a second
    /// resource name).
    pub is_cover: bool,
}

/// Extract every embedded image from an in-memory KFX container.
///
/// Returns the images sorted by `resource_name` (then location) for a stable
/// order. Deduplicates by backing location, so a resource referenced twice
/// yields one image. Errors (via [`ConvertError`]) only when the bytes aren't a
/// KFX container at all.
pub fn kfx_extract_images(kfx_bytes: &[u8]) -> Result<Vec<ExtractedImage>, ConvertError> {
    let book = loader::load(kfx_bytes)?;
    let Some(resources) = book.by_type.get(&(KfxSymbol::ExternalResource as u64)) else {
        return Ok(Vec::new());
    };

    // The declared cover's backing location (if any), so `is_cover` is matched
    // by bytes rather than by name.
    let cover_location = book
        .metadata
        .cover_resource_name
        .as_deref()
        .and_then(|cn| resource_location_for_name(resources, &book.symbols, cn));

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<ExtractedImage> = Vec::new();
    for res in resources.values() {
        let Some(fields) = res.unwrap_annotated().as_struct() else {
            continue;
        };
        let Some(location) =
            get_field(fields, KfxSymbol::Location as u64).and_then(IonValue::as_string)
        else {
            continue;
        };
        let location = location.to_string();
        // Dedup by backing bytes — the same location resolves to the same image.
        if !seen.insert(location.clone()) {
            continue;
        }
        let Some(raw) = book.raw_media.get(&location) else {
            continue;
        };
        let resource_name = get_field(fields, KfxSymbol::ResourceName as u64)
            .and_then(|x| book.symbols.text_of(x))
            .map(str::to_string);
        let format = get_field(fields, KfxSymbol::Format as u64)
            .and_then(|x| book.symbols.text_of(x))
            .map(str::to_string);

        let name = resource_name.as_deref().unwrap_or(&location);
        let Some((bytes, ext)) = resolve_image(raw, format.as_deref(), name) else {
            continue; // not a raster image, or an undecodable JPEG-XR
        };

        let dim = |sym: KfxSymbol| {
            get_field(fields, sym as u64)
                .and_then(IonValue::as_int)
                .map(|n| n as u32)
        };
        out.push(ExtractedImage {
            is_cover: cover_location.as_deref() == Some(location.as_str()),
            resource_name: resource_name.unwrap_or_else(|| location.clone()),
            location,
            bytes,
            ext,
            width: dim(KfxSymbol::ResourceWidth),
            height: dim(KfxSymbol::ResourceHeight),
        });
    }

    out.sort_by(|a, b| {
        a.resource_name
            .cmp(&b.resource_name)
            .then_with(|| a.location.cmp(&b.location))
    });
    Ok(out)
}

/// The `location` of the `external_resource` whose `resource_name` field equals
/// `name` — how [`cover_extract`] resolves the cover's backing bytes.
///
/// [`cover_extract`]: super::cover_extract
fn resource_location_for_name(
    resources: &std::collections::HashMap<String, IonValue>,
    symbols: &loader::SymbolTable,
    name: &str,
) -> Option<String> {
    resources.values().find_map(|res| {
        let fields = res.unwrap_annotated().as_struct()?;
        let rn = get_field(fields, KfxSymbol::ResourceName as u64).and_then(|x| symbols.text_of(x));
        if rn != Some(name) {
            return None;
        }
        get_field(fields, KfxSymbol::Location as u64)
            .and_then(IonValue::as_string)
            .map(str::to_string)
    })
}

/// Decide the export bytes + extension for one resource's raw payload, or `None`
/// to skip it (non-image, or an undecodable JPEG-XR).
fn resolve_image(
    raw: &[u8],
    format: Option<&str>,
    name: &str,
) -> Option<(Vec<u8>, &'static str)> {
    // JPEG-XR (declared, or by the II-BC magic) → transcode to JPEG.
    if format == Some("jxr") || raw.starts_with(&[0x49, 0x49, 0xBC]) {
        let (bytes, final_format, _timing) = transcode::transcode(raw, name).ok()?;
        // `transcode` returns the input with format "jxr" on a graceful decode
        // failure; an undisplayable sidecar is no better than skipping it.
        if final_format == "jxr" {
            return None;
        }
        return Some((bytes, "jpg"));
    }
    Some((raw.to_vec(), image_ext(format, raw)?))
}

/// The export extension for a non-JXR resource: from its declared `format`, or
/// sniffed from the bytes. `None` when it isn't a recognized raster image (a
/// font or an embedded PDF page lands here and is skipped).
fn image_ext(format: Option<&str>, bytes: &[u8]) -> Option<&'static str> {
    match format {
        Some("jpg") | Some("jpeg") => Some("jpg"),
        Some("png") => Some("png"),
        Some("gif") => Some("gif"),
        Some("webp") => Some("webp"),
        Some("bmp") => Some("bmp"),
        _ => sniff_image(bytes),
    }
}

/// Recognize a raster image by its magic bytes, returning its extension. Unlike
/// a cover sniffer, an unrecognized payload returns `None` (skip), not a default.
fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
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

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "tests/fixtures/[太宰 治] 人間失格.kfx";

    /// Extraction on the real fixture: every image has non-empty bytes and a
    /// valid extension, exactly one is flagged the cover, and that cover image
    /// byte-matches the dedicated `kfx_extract_cover`.
    #[test]
    fn extracts_images_from_fixture() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let images = kfx_extract_images(&kfx).expect("valid KFX");
        assert!(!images.is_empty(), "fixture has embedded images");

        for img in &images {
            assert!(!img.bytes.is_empty(), "{} has bytes", img.resource_name);
            assert!(
                ["jpg", "png", "gif", "webp", "bmp"].contains(&img.ext),
                "{} has a known ext, got {}",
                img.resource_name,
                img.ext
            );
        }

        let covers: Vec<&ExtractedImage> = images.iter().filter(|i| i.is_cover).collect();
        assert_eq!(covers.len(), 1, "exactly one image is the declared cover");

        let (cover_bytes, cover_ext) = crate::kfx::cover_extract::kfx_extract_cover(&kfx)
            .expect("valid KFX")
            .expect("fixture has a cover");
        assert_eq!(covers[0].bytes, cover_bytes, "cover bytes agree with cover_extract");
        assert_eq!(covers[0].ext, cover_ext, "cover ext agrees with cover_extract");
    }

    /// A PDF-backed KFX declares a JPEG cover *and* a `format: pdf` resource: the
    /// extractor returns the cover image and skips the non-image PDF page.
    #[test]
    fn pdf_backed_extracts_cover_skips_pdf_resource() {
        use crate::export::{PdfKfxMeta, pdf_to_kfx};
        use crate::import::pdf::{PdfDoc, PdfPage};

        let doc = PdfDoc {
            bytes: b"%PDF-1.4\n% fixture\n%%EOF\n".to_vec(),
            pages: vec![PdfPage {
                width: 612.0,
                height: 792.0,
            }],
            title: Some("Backed".into()),
            author: None,
            outline: Vec::new(),
            page_labels: Vec::new(),
        };
        let meta = PdfKfxMeta {
            title: "Backed".into(),
            author: None,
            language: "en".into(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };
        let cover = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 0xFF, 0xD9];
        let kfx = pdf_to_kfx(&doc, &meta, Some(&cover), None);

        let images = kfx_extract_images(&kfx).expect("valid KFX");
        assert_eq!(images.len(), 1, "only the cover image, PDF resource skipped");
        assert!(images[0].is_cover);
        assert_eq!(images[0].ext, "jpg");
        assert_eq!(images[0].bytes, cover, "cover extracted verbatim");
    }

    #[test]
    fn non_kfx_bytes_error() {
        assert!(kfx_extract_images(b"not a kfx container").is_err());
    }

    #[test]
    fn sniff_image_recognizes_formats_and_rejects_others() {
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0x00]), Some("jpg"));
        assert_eq!(
            sniff_image(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("png")
        );
        assert_eq!(sniff_image(b"GIF89a....."), Some("gif"));
        assert_eq!(sniff_image(b"RIFF____WEBP...."), Some("webp"));
        assert_eq!(sniff_image(b"BM......"), Some("bmp"));
        // A font / arbitrary payload is not an image.
        assert_eq!(sniff_image(b"\x00\x01\x00\x00ttf"), None);
        assert_eq!(sniff_image(b"%PDF-1.4"), None);
        assert_eq!(sniff_image(&[]), None);
    }

    #[test]
    fn image_ext_prefers_declared_format() {
        assert_eq!(image_ext(Some("png"), &[0xFF, 0xD8, 0xFF]), Some("png"));
        assert_eq!(image_ext(Some("jpeg"), &[]), Some("jpg"));
        // No format → fall back to the bytes.
        assert_eq!(image_ext(None, &[0xFF, 0xD8, 0xFF]), Some("jpg"));
        // A PDF/font format is not a raster image.
        assert_eq!(image_ext(Some("pdf"), b"%PDF"), None);
    }
}
