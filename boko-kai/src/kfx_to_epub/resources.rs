//! Image + raw media extraction + cover declaration.
//!
//! Port of (the image-handling parts of) calibre's `yj_to_epub_resources.py`.
//! Walks every `external_resource` ($164) entity, locates the matching
//! `bcRawMedia` ($417) bytes, and registers the result in the EPUB manifest
//! under `OEBPS/<filename>`. Marks the cover image as such on the way out.
//!
//! Image *bytes* are deferred: [`process_deferred`] registers every image
//! with its final filename/mime/dimensions but empty data (the content and
//! navigation passes only ever consume the metadata), and returns a
//! [`DeferredImage`] work list. The JPEG-XR→JPEG transcode — by far the most
//! expensive stage of the whole pipeline — runs later, only for the images
//! that survive fixed-layout thumbnail pruning: eagerly for the EPUB export
//! (`convert_to_epub` fills the bytes back in before zipping) and lazily,
//! per reading position, for the Sidle reader (`ReaderImageStore`).
//!
//! Fonts (`process_fonts` in calibre) are intentionally deferred to the
//! properties / CSS pass where they're consumed.

use std::collections::HashMap;

use crate::kfx::container::get_field;
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;

use super::ConvertError;
use super::loader::{BookData, SymbolTable};
use super::output::EpubOutput;
use crate::image::jxr_transcode as transcode;

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

/// One registered image whose bytes haven't been produced yet: everything a
/// later (possibly on-demand) transcode needs. `filename` is the final OEBPS
/// path — including the `cover.<ext>` rename — and `mime` is the *predicted*
/// final type (JXR predicts `image/jpeg`; the decode-failure pass-through is
/// corrected at fill/fetch time via the actual mime returned then).
#[derive(Debug, Clone)]
pub struct DeferredImage {
    /// Original KFX `external_resource.resource_name` (what storylines reference).
    pub resource_name: String,
    /// `book.raw_media` key holding the source bytes.
    pub location: String,
    /// File path under `OEBPS/` (post cover-rename).
    pub filename: String,
    /// Predicted final MIME type.
    pub mime: String,
    /// True iff the bytes are JPEG-XR (by `format` field or by file magic).
    pub is_jxr: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// The produced bytes for one [`DeferredImage`].
pub struct TranscodedImage {
    pub bytes: Vec<u8>,
    /// Actual final MIME type (`image/jxr` when a broken JXR passed through).
    pub mime: String,
    /// `Some` only when the JXR pipeline ran; `None` for pass-through formats.
    pub timing: Option<transcode::TranscodeTiming>,
}

/// Iterate every image-format `external_resource` in `book`, register it in
/// `out` (final filename, mime, dimensions — with **empty bytes**), and mark
/// the cover image declared by `book_metadata`. Returns the lookup index the
/// content pass needs plus the deferred-transcode work list, in deterministic
/// (sorted-key) order. Manifest filenames/IDs are identical to what the old
/// eager pass produced, because they never depended on the decoded bytes.
pub fn process_deferred(
    book: &BookData,
    out: &mut EpubOutput,
) -> Result<(ResourceIndex, Vec<DeferredImage>), ConvertError> {
    let mut index = ResourceIndex::default();
    let mut deferred: Vec<DeferredImage> = Vec::new();

    let Some(resources) = book.by_type.get(&(KfxSymbol::ExternalResource as u64)) else {
        return Ok((index, deferred));
    };

    // Sort for deterministic output (HashMap iteration order is random).
    let mut keys: Vec<&String> = resources.keys().collect();
    keys.sort();

    for key in keys {
        let Some(prep) = prepare_resource(key, &resources[key], book) else {
            continue;
        };
        // Predict the final format without decoding: a JXR will be transcoded
        // to JPEG; everything else passes through as sniffed (falling back to
        // the KFX `format` field) — exactly the eager pass's non-JXR logic.
        let final_format = if prep.is_jxr {
            FORMAT_JPG.to_string()
        } else {
            sniff_format(prep.raw_bytes).unwrap_or_else(|| prep.format_str.clone())
        };
        let final_mime = format_to_mime(&final_format);
        let filename = build_image_filename(&prep.location, &final_format, out);
        let manifest_id =
            out.add_resource(&filename, Vec::new(), &final_mime, prep.width, prep.height);
        index.by_name.insert(
            prep.resource_name.clone(),
            ProcessedImage {
                resource_name: prep.resource_name.clone(),
                filename: filename.clone(),
                manifest_id,
                mime: final_mime.clone(),
                width: prep.width,
                height: prep.height,
            },
        );
        deferred.push(DeferredImage {
            resource_name: prep.resource_name,
            location: prep.location,
            filename,
            mime: final_mime,
            is_jxr: prep.is_jxr,
            width: prep.width,
            height: prep.height,
        });
    }

    // Cover wiring: book_metadata names a resource_name; mark its manifest
    // entry as cover-image so EPUB readers (and the validator) see it.
    // Also rename the file from `image_rsrcXX.jpg` to `cover.<ext>` and the
    // manifest id to `cover` — matches calibre's convention and lets the
    // titlepage SVG wrapper reference a stable path.
    if let Some(cover_name) = &book.metadata.cover_resource_name
        && let Some(img) = index.by_name.get(cover_name).cloned()
    {
        let ext = std::path::Path::new(&img.filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| if e == "jpg" { "jpeg" } else { e })
            .unwrap_or("jpeg");
        let new_filename = format!("cover.{}", ext);
        if let Some(new_id) = out.rename_resource(&img.filename, &new_filename, Some("cover")) {
            out.mark_cover(&new_id);
            if let Some(slot) = index.by_name.get_mut(cover_name) {
                slot.filename = new_filename.clone();
                slot.manifest_id = new_id;
            }
            if let Some(d) = deferred.iter_mut().find(|d| d.filename == img.filename) {
                d.filename = new_filename;
            }
        } else {
            out.mark_cover(&img.manifest_id);
        }
    }

    Ok((index, deferred))
}

/// Produce the bytes for one deferred image: JXR → JPEG transcode, or a
/// pass-through copy for formats EPUB readers handle natively. Fails only
/// when the raw media vanished (corrupt container) or JPEG encoding fails;
/// a JXR that won't *decode* passes through as `image/jxr` (same policy as
/// the old eager pass).
pub fn transcode_deferred_one(
    book: &BookData,
    item: &DeferredImage,
) -> Result<TranscodedImage, ConvertError> {
    let raw = book.raw_media.get(&item.location).ok_or_else(|| {
        ConvertError::InvalidKfx(format!("missing bcRawMedia at {:?}", item.location))
    })?;
    if item.is_jxr {
        let (bytes, final_format, t) = transcode::transcode(raw, &item.resource_name)?;
        Ok(TranscodedImage {
            bytes,
            mime: format_to_mime(&final_format),
            timing: Some(t),
        })
    } else {
        Ok(TranscodedImage {
            bytes: raw.clone(),
            mime: item.mime.clone(),
            timing: None,
        })
    }
}

/// Run [`transcode_deferred_one`] over `items` **in parallel** across all
/// available CPU cores via `std::thread::scope`, preserving order. Each
/// worker owns a contiguous slice — static striping is fine because all JXR
/// images on the corpus cost ~20 ms ± 30%.
pub fn transcode_deferred(
    book: &BookData,
    items: &[DeferredImage],
) -> Vec<Result<TranscodedImage, ConvertError>> {
    if items.is_empty() {
        return Vec::new();
    }
    let n_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(items.len());
    if n_workers <= 1 {
        return items.iter().map(|i| transcode_deferred_one(book, i)).collect();
    }
    let mut out: Vec<Option<Result<TranscodedImage, ConvertError>>> =
        (0..items.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let chunk_size = items.len().div_ceil(n_workers);
        let mut handles = Vec::with_capacity(n_workers);
        for chunk in items.chunks(chunk_size) {
            handles.push(scope.spawn(move || -> Vec<Result<TranscodedImage, ConvertError>> {
                chunk.iter().map(|i| transcode_deferred_one(book, i)).collect()
            }));
        }
        let mut write_idx = 0;
        for h in handles {
            let results = h.join().expect("transcode worker panicked");
            for r in results {
                out[write_idx] = Some(r);
                write_idx += 1;
            }
        }
    });
    out.into_iter().map(|slot| slot.expect("filled")).collect()
}

/// Aggregate JXR timings from transcode results and print the
/// `BOKO_KFX2EPUB_TRACE`-gated summary lines (same format the eager pass
/// printed). No-op when the env var is unset or nothing went through the JXR
/// pipeline.
pub fn trace_jxr_totals<'a>(timings: impl Iterator<Item = &'a transcode::TranscodeTiming>) {
    if std::env::var("BOKO_KFX2EPUB_TRACE").is_err() {
        return;
    }
    let mut totals = transcode::TranscodeTiming::default();
    let mut jxr_count = 0usize;
    for t in timings {
        totals.container_parse += t.container_parse;
        totals.jxr_decode += t.jxr_decode;
        totals.jpeg_encode += t.jpeg_encode;
        totals.jxr_decode_breakdown.header += t.jxr_decode_breakdown.header;
        totals.jxr_decode_breakdown.coded_tiles += t.jxr_decode_breakdown.coded_tiles;
        totals.jxr_decode_breakdown.sample_recon += t.jxr_decode_breakdown.sample_recon;
        totals.jxr_decode_breakdown.output_fmt += t.jxr_decode_breakdown.output_fmt;
        jxr_count += 1;
    }
    if jxr_count == 0 {
        return;
    }
    let to_ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let n = jxr_count as f64;
    eprintln!(
        "[kfx2epub:jxr] {} images, totals: container_parse={:.2} ms  jxr_decode={:.2} ms  jpeg_encode={:.2} ms  (per-image: {:.2} / {:.2} / {:.2} ms; CPU-time, NOT wall)",
        jxr_count,
        to_ms(totals.container_parse),
        to_ms(totals.jxr_decode),
        to_ms(totals.jpeg_encode),
        to_ms(totals.container_parse) / n,
        to_ms(totals.jxr_decode) / n,
        to_ms(totals.jpeg_encode) / n,
    );
    let bd = totals.jxr_decode_breakdown;
    eprintln!(
        "[kfx2epub:jxr:decode]   header={:.2} ms  coded_tiles={:.2} ms  sample_recon={:.2} ms  output_fmt={:.2} ms  (per-image: {:.2} / {:.2} / {:.2} / {:.2} ms)",
        to_ms(bd.header),
        to_ms(bd.coded_tiles),
        to_ms(bd.sample_recon),
        to_ms(bd.output_fmt),
        to_ms(bd.header) / n,
        to_ms(bd.coded_tiles) / n,
        to_ms(bd.sample_recon) / n,
        to_ms(bd.output_fmt) / n,
    );
}

/// Everything `prepare_resource` extracts up-front. Borrows from
/// `book.raw_media` for the source image bytes (used here only for format
/// sniffing; the transcode re-resolves them by `location`).
struct PreparedResource<'a> {
    resource_name: String,
    location: String,
    raw_bytes: &'a [u8],
    width: Option<u32>,
    height: Option<u32>,
    /// True iff the bytes are JPEG-XR (by `format` field or by file magic).
    is_jxr: bool,
    /// `format` field as a String for the non-JXR pass-through path.
    format_str: String,
}

fn prepare_resource<'a>(
    fid: &str,
    raw: &'a IonValue,
    book: &'a BookData,
) -> Option<PreparedResource<'a>> {
    let inner = raw.unwrap_annotated();
    let fields = inner.as_struct()?;

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
        .map(|s| s.to_string())?;

    let width = get_field(fields, KfxSymbol::ResourceWidth as u64)
        .and_then(|v| v.as_int())
        .map(|n| n as u32);
    let height = get_field(fields, KfxSymbol::ResourceHeight as u64)
        .and_then(|v| v.as_int())
        .map(|n| n as u32);

    // Skip non-image formats here (fonts come in via the fonts step).
    let format_str = format_raw.as_deref().unwrap_or("");
    if !is_image_format_symbol(format_str, mime_raw.as_deref()) {
        return None;
    }

    let raw_bytes = match book.raw_media.get(&location) {
        Some(b) => b.as_slice(),
        None => {
            // Calibre logs "Missing bcRawMedia" here and skips.
            eprintln!("kfx_to_epub: missing bcRawMedia at {location:?}");
            return None;
        }
    };

    let is_jxr = format_str == FORMAT_JXR || sniff_format(raw_bytes).as_deref() == Some(FORMAT_JXR);

    Some(PreparedResource {
        resource_name,
        location,
        raw_bytes,
        width,
        height,
        is_jxr,
        format_str: format_str.to_string(),
    })
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
    let unique = stem
        .strip_prefix("rsrc")
        .map(|r| format!("rsrc{r}"))
        .unwrap_or_else(|| stem.to_string());

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

/// Fallback when the content pipeline emits no chapters: one minimal XHTML
/// chapter per bundled image so the validator still sees an `<img src>` for
/// each.
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
fn out_manifest_get(out: &EpubOutput, idx: usize) -> Option<&super::output::ManifestEntry> {
    out.manifest_view().get(idx)
}

// Suppress unused-warning until later steps consume.
#[allow(dead_code)]
fn _unused_symbol_table_marker(_: &SymbolTable) {}
