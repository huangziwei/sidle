//! Phase 1 step 1: image + raw media extraction + cover declaration.
//!
//! Port of (the image-handling parts of) calibre's `yj_to_epub_resources.py`.
//! Walks every `external_resource` ($164) entity, locates the matching
//! `bcRawMedia` ($417) bytes, transcodes JPEG-XR to JPEG when needed, and
//! adds the result to the EPUB manifest under `OEBPS/<filename>`. Marks the
//! cover image as such on the way out.
//!
//! Fonts (`process_fonts` in calibre) are intentionally deferred to phase 1
//! step 6 (properties / CSS) where they're consumed.

use std::collections::HashMap;

use crate::kfx::container::get_field;
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;

use super::jxr;
use super::loader::{BookData, SymbolTable};
use super::output::EpubOutput;
use super::ConvertError;

/// Image format symbol values KFX may set on `external_resource.format`.
/// Calibre's `SYMBOL_FORMATS` mapping for the image side.
const FORMAT_JPG: &str = "jpg";
const FORMAT_PNG: &str = "png";
const FORMAT_GIF: &str = "gif";
const FORMAT_WEBP: &str = "webp";
const FORMAT_BMP: &str = "bmp";
const FORMAT_SVG: &str = "svg";
const FORMAT_JXR: &str = "jxr";

/// One bundled image, exposed so later steps (content emission, cover wiring)
/// know what filename / resource_name to reference.
#[derive(Debug, Clone)]
pub struct ProcessedImage {
    /// Original KFX `external_resource.resource_name` (e.g. "content_30",
    /// "eF"). This is what storyline image elements reference.
    pub resource_name: String,
    /// File path under `OEBPS/` (e.g. "image_content_30.jpg").
    pub filename: String,
    /// EPUB manifest id assigned by `EpubOutput`.
    pub manifest_id: String,
    /// Final MIME type (after any transcode).
    pub mime: String,
    /// Pixel dimensions when known.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// State carried across the resources step — accessible to later steps that
/// need to resolve `resource_name` → bundled filename.
#[derive(Default)]
pub struct ResourceIndex {
    pub by_name: HashMap<String, ProcessedImage>,
}

/// Iterate every image-format `external_resource` in `book` and bundle it
/// into `out`. Also marks the cover image declared by `book_metadata`.
pub fn process(book: &BookData, out: &mut EpubOutput) -> Result<ResourceIndex, ConvertError> {
    let mut index = ResourceIndex::default();

    let Some(resources) = book.by_type.get(&(KfxSymbol::ExternalResource as u64)) else {
        return Ok(index);
    };

    // Sort for deterministic output (HashMap iteration order is random).
    let mut keys: Vec<&String> = resources.keys().collect();
    keys.sort();

    for key in keys {
        let raw = &resources[key];
        if let Some(img) = process_one_resource(key, raw, book, out)? {
            index.by_name.insert(img.resource_name.clone(), img);
        }
    }

    // Cover wiring: book_metadata names a resource_name; mark its manifest
    // entry as cover-image so EPUB readers (and the validator) see it.
    if let Some(cover_name) = &book.metadata.cover_resource_name
        && let Some(img) = index.by_name.get(cover_name)
    {
        out.mark_cover(&img.manifest_id);
    }

    Ok(index)
}

fn process_one_resource(
    fid: &str,
    raw: &IonValue,
    book: &BookData,
    out: &mut EpubOutput,
) -> Result<Option<ProcessedImage>, ConvertError> {
    let inner = raw.unwrap_annotated();
    let Some(fields) = inner.as_struct() else {
        return Ok(None);
    };

    // Pull the fields we care about. We use symbol-id lookups rather than
    // string keys for speed and to match calibre's $-symbol style.
    let resource_name = get_field(fields, KfxSymbol::ResourceName as u64)
        .and_then(|v| book.symbols.text_of(v))
        .map(|s| s.to_string())
        .unwrap_or_else(|| fid.to_string());

    let format_raw = get_field(fields, KfxSymbol::Format as u64)
        .and_then(|v| book.symbols.text_of(v))
        .map(|s| s.to_string());

    let mime_raw = get_field(fields, KfxSymbol::Mime as u64)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());

    let location = get_field(fields, KfxSymbol::Location as u64)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());

    let width = get_field(fields, KfxSymbol::ResourceWidth as u64)
        .and_then(|v| v.as_int())
        .map(|n| n as u32);
    let height = get_field(fields, KfxSymbol::ResourceHeight as u64)
        .and_then(|v| v.as_int())
        .map(|n| n as u32);

    // Skip non-image formats here (fonts come in via the fonts step).
    let format_str = format_raw.as_deref().unwrap_or("");
    if !is_image_format_symbol(format_str, mime_raw.as_deref()) {
        return Ok(None);
    }

    // Locate raw media bytes.
    let Some(location) = location else {
        return Ok(None);
    };
    let Some(raw_bytes) = book.raw_media.get(&location) else {
        // Calibre logs "Missing bcRawMedia" here and returns None.
        eprintln!("kfx_to_epub: missing bcRawMedia at {location:?}");
        return Ok(None);
    };

    // Transcode JXR → JPEG/PNG; pass everything else through. We also sniff
    // file magic here so a mislabelled format doesn't bundle the wrong mime.
    let (bytes, final_format) = if format_str == FORMAT_JXR
        || sniff_format(raw_bytes).as_deref() == Some(FORMAT_JXR)
    {
        jxr::transcode(raw_bytes, &resource_name)?
    } else {
        let sniffed = sniff_format(raw_bytes).unwrap_or_else(|| format_str.to_string());
        (raw_bytes.clone(), sniffed)
    };
    let final_mime = format_to_mime(&final_format);

    let filename = build_image_filename(&location, &final_format, out);

    let manifest_id = out.add_resource(
        &filename,
        bytes,
        &final_mime,
        width,
        height,
    );

    Ok(Some(ProcessedImage {
        resource_name,
        filename,
        manifest_id,
        mime: final_mime,
        width,
        height,
    }))
}

fn is_image_format_symbol(format: &str, mime: Option<&str>) -> bool {
    matches!(
        format,
        FORMAT_JPG | FORMAT_PNG | FORMAT_GIF | FORMAT_WEBP | FORMAT_BMP | FORMAT_SVG | FORMAT_JXR
    ) || mime.is_some_and(|m| m.starts_with("image/"))
}

/// Detect image format from leading bytes. Used as a sanity check and as a
/// fallback when the `format` field is missing.
fn sniff_format(bytes: &[u8]) -> Option<String> {
    if bytes.len() >= 3 && bytes[..3] == [0xFF, 0xD8, 0xFF] {
        return Some(FORMAT_JPG.into());
    }
    if bytes.len() >= 8 && bytes[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some(FORMAT_PNG.into());
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some(FORMAT_GIF.into());
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(FORMAT_WEBP.into());
    }
    if bytes.len() >= 2 && &bytes[..2] == b"BM" {
        return Some(FORMAT_BMP.into());
    }
    // JPEG-XR / WMP container: II-BC magic.
    if bytes.len() >= 3 && bytes[..3] == [0x49, 0x49, 0xBC] {
        return Some(FORMAT_JXR.into());
    }
    None
}

fn format_to_mime(format: &str) -> String {
    match format {
        FORMAT_JPG => "image/jpeg".into(),
        FORMAT_PNG => "image/png".into(),
        FORMAT_GIF => "image/gif".into(),
        FORMAT_WEBP => "image/webp".into(),
        FORMAT_BMP => "image/bmp".into(),
        FORMAT_SVG => "image/svg+xml".into(),
        FORMAT_JXR => "image/jxr".into(),
        _ => "application/octet-stream".into(),
    }
}

fn format_to_ext(format: &str) -> &'static str {
    match format {
        FORMAT_JPG => ".jpg",
        FORMAT_PNG => ".png",
        FORMAT_GIF => ".gif",
        FORMAT_WEBP => ".webp",
        FORMAT_BMP => ".bmp",
        FORMAT_SVG => ".svg",
        FORMAT_JXR => ".jxr",
        _ => ".bin",
    }
}

/// Mirror calibre's `resource_location_filename`: take the external_resource
/// `location` (e.g. `"resource/rsrc562"`), strip the `resource/` prefix to
/// the unique part, prepend the resource-type prefix (`"image"` for image
/// formats), and apply the extension. Result: `"image_rsrc562.jpg"`.
fn build_image_filename(location: &str, format: &str, out: &EpubOutput) -> String {
    let ext = format_to_ext(format);
    // Sanitise: only `[A-Za-z0-9_/.-]` survives; everything else → `_`.
    let safe: String = location
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '/' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Split path / name; pull the basename's root (no extension).
    let name = safe.rsplit('/').next().unwrap_or(&safe);
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    // Unique part: strip the `resource/`-style prefix that the on-disk
    // location commonly uses. For SHORT-form symbols (which is what
    // horror's KFX uses), calibre's `unique_part_of_local_symbol` just
    // strips `^resource/`. We do the same.
    let unique = stem.strip_prefix("rsrc").map(|r| format!("rsrc{r}")).unwrap_or_else(|| stem.to_string());

    // Resource-type prefix. Mirrors calibre's RESOURCE_TYPE_OF_EXT mapping
    // for image extensions: image → "image_<unique>". When the unique part
    // already starts with a letter the prefix joins with `_`.
    let prefixed = if unique.is_empty() {
        "image".to_string()
    } else {
        format!("image_{unique}")
    };

    let mut candidate = format!("{prefixed}{ext}");
    let mut n = 0;
    while out.has_file(&candidate) {
        candidate = format!("{prefixed}-{n}{ext}");
        n += 1;
    }
    candidate
}

/// Scaffolding for phase 1 step 1's validator gate. Emits one minimal XHTML
/// chapter per bundled image so the validator sees an `<img src>` for each.
/// Replaced by the real content pipeline in step 4.
pub fn emit_image_scaffold_chapters(out: &mut EpubOutput) {
    // We need to discover which resources we just added. Re-walk the OEBPS
    // files: anything with an image/* mime is fair game.
    let image_files: Vec<(String, String)> = (0..out_manifest_len(out))
        .filter_map(|idx| {
            let m = out_manifest_get(out, idx)?;
            if m.media_type.starts_with("image/") {
                Some((m.href.clone(), m.id.clone()))
            } else {
                None
            }
        })
        .collect();

    for (i, (filename, _id)) in image_files.iter().enumerate() {
        let chapter_name = format!("chapter_{:04}.xhtml", i);
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Page {i}</title></head>
<body><div><img src="{src}" alt=""/></div></body>
</html>
"#,
            i = i + 1,
            src = xml_attr_escape(filename),
        );
        out.add_spine_chapter(&chapter_name, body);
    }
}

fn xml_attr_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

// --- Accessor helpers ---
// EpubOutput keeps its manifest private; we expose just enough to scan for
// image entries here without leaking the whole structure.
fn out_manifest_len(out: &EpubOutput) -> usize {
    out.manifest_view().len()
}
fn out_manifest_get<'a>(
    out: &'a EpubOutput,
    idx: usize,
) -> Option<&'a super::output::ManifestEntry> {
    out.manifest_view().get(idx)
}

// Suppress unused-warning until later steps consume.
#[allow(dead_code)]
fn _unused_symbol_table_marker(_: &SymbolTable) {}
