//! Image + raw media extraction + cover declaration.
//!
//! Port of (the image-handling parts of) calibre's `yj_to_epub_resources.py`.
//! The external_resource walk, filename convention, and format prediction
//! live in the shared [`crate::kfx::resource_index`] module (the IR route
//! uses the same code, which is what keeps both routes' image trees
//! byte-identical); this module registers the results in the EPUB manifest
//! under `OEBPS/<filename>` and marks the cover image on the way out.
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

use crate::kfx::resource_index::{build_image_index, cover_filename, format_to_mime};
use crate::kfx::symbols::KfxSymbol;

use super::ConvertError;
use super::loader::BookData;
use super::output::EpubOutput;
use crate::image::jxr_transcode as transcode;

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

    let entries = build_image_index(
        resources.iter().map(|(fid, v)| (fid.as_str(), v)).collect(),
        &book.symbols,
        |location| book.raw_media.get(location).cloned(),
    );

    for img in &entries {
        let manifest_id =
            out.add_resource(&img.filename, Vec::new(), &img.mime, img.width, img.height);
        index.by_name.insert(
            img.resource_name.clone(),
            ProcessedImage {
                resource_name: img.resource_name.clone(),
                filename: img.filename.clone(),
                manifest_id,
                mime: img.mime.clone(),
                width: img.width,
                height: img.height,
            },
        );
        deferred.push(DeferredImage {
            resource_name: img.resource_name.clone(),
            location: img.location.clone(),
            filename: img.filename.clone(),
            mime: img.mime.clone(),
            is_jxr: img.is_jxr,
            width: img.width,
            height: img.height,
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
        let new_filename = cover_filename(&img.filename);
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
/// available CPU cores, preserving order (static striping via
/// [`crate::util::parallel_map`] — fine because all JXR images on the corpus
/// cost ~20 ms ± 30%).
pub fn transcode_deferred(
    book: &BookData,
    items: &[DeferredImage],
) -> Vec<Result<TranscodedImage, ConvertError>> {
    crate::util::parallel_map(items, |i| transcode_deferred_one(book, i))
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
