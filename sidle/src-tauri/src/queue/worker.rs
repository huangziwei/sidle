//! Conversion worker: runs boko-kai on a blocking thread.
//!
//! Both directions share the worker shape — claim job → mark `converting` →
//! do the heavy work → write the output to disk → record paths on the book
//! row → mark `done`/`error`. The direction (`epub_to_kfx` or `kfx_to_epub`)
//! is stored on the `conversion_jobs` row at import time; the worker just
//! dispatches on it.

use std::fs::File;
use std::path::{Path, PathBuf};

use tauri::AppHandle;

use crate::cover_fetch;
use crate::library::LibraryPaths;
use crate::library::db::{self, BookRow};
use crate::library::epub_cover;
use crate::library::import::{
    extract_cover_from_epub, extract_cover_from_kfx_bytes, sha256_of_bytes, sha256_of_file,
    write_bytes_atomic,
};
use crate::library::kfx_cover;
use crate::library::pdf_geom;
use crate::library::paths::format_basename;
use crate::queue::emit_status;
use crate::state::DbHandle;

/// Run a single conversion job: mark `converting`, run boko, write file,
/// update `done` or `error`. Errors are recorded in the DB; never propagated
/// to the caller (this is a fire-and-forget worker).
///
/// `reconvert` = a forced re-run of the format conversion (the "Force
/// re-convert" button), as opposed to a first import. It does **only**
/// source→target: the import-time cover-enrichment tail-step (which re-fetches
/// the Amazon cover and rewrites the *source* KFX, re-stamping `kfx_sha256`) is
/// skipped — re-stamping would change the on-device filename infix and break
/// annotation-sync matching for a book already pushed to the Kindle. The EPUB
/// still gets the right cover: it's extracted from the (already-enriched) KFX.
pub async fn run_job(
    app: &AppHandle,
    db: &DbHandle,
    paths: &LibraryPaths,
    book_id: i64,
    reconvert: bool,
) {
    let Some(book) = lookup_book(db, book_id).await else {
        eprintln!("[sidle/queue] book {book_id} vanished before conversion");
        return;
    };
    let Some(kind) = book.kind.as_deref() else {
        eprintln!("[sidle/queue] book {book_id} has no job kind; skipping");
        return;
    };

    eprintln!("[sidle/queue] book {book_id} converting ({kind})");
    mark_status(db, app, book_id, "converting", None).await;

    let paths_owned = paths.clone();
    let book_owned = book.clone();
    let kind_owned = kind.to_string();
    let started = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        run_direction(&paths_owned, &book_owned, &kind_owned)
    })
    .await;

    let mut produced = match result {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            let msg = format!("{e:#}");
            eprintln!("[sidle/queue] book {book_id} error: {msg}");
            mark_status(db, app, book_id, "error", Some(&msg)).await;
            return;
        }
        Err(join_err) => {
            let msg = format!("worker panicked: {join_err}");
            eprintln!("[sidle/queue] book {book_id} PANIC: {msg}");
            mark_status(db, app, book_id, "error", Some(&msg)).await;
            return;
        }
    };

    // Tail step (kfx_to_epub, first import only — skipped on a forced
    // re-convert): if the KFX came from Amazon's monochrome-device build (KOA2 +
    // friends), its embedded cover is grayscale-baked and there's no way to
    // recover color from the file itself — refetch from the product page by
    // ASIN. KFXes boko-kai produced from a color EPUB already have the original
    // color cover; the ASIN is fabricated there, so cover_fetch skips and we
    // just keep what's in the KFX. Skipped on `reconvert` because it rewrites
    // the source KFX (re-stamping `kfx_sha256`), which a force re-convert must
    // not do — see `run_job`.
    if kind == "kfx_to_epub" && !reconvert {
        match book.asin.as_deref() {
            None => eprintln!(
                "[sidle/queue] book {book_id} color cover: no ASIN on row \
                 (KFX metadata likely missing `book_id`)"
            ),
            Some(asin) => {
                eprintln!(
                    "[sidle/queue] book {book_id} color cover: fetching ASIN={asin} \
                     language={:?}",
                    book.language
                );
                match cover_fetch::fetch_color_cover(asin, &book.language).await {
                    Some(bytes) => {
                        let out = paths.cover(&book.sha256, "jpg");
                        if let Err(e) = std::fs::write(&out, &bytes) {
                            eprintln!(
                                "[sidle/queue] book {book_id} color cover write failed: {e}"
                            );
                        } else {
                            // If the worker had just written a grayscale
                            // `cover.<otherext>` (rare — typically the
                            // JXR→JPG transcode lands on .jpg too), remove
                            // the stale file so we don't leave both on disk.
                            if let Some(old) = &produced.cover_path
                                && old != &out
                                && let Err(e) = std::fs::remove_file(old)
                            {
                                eprintln!(
                                    "[sidle/queue] book {book_id} couldn't remove stale {}: {e}",
                                    old.display()
                                );
                            }
                            eprintln!(
                                "[sidle/queue] book {book_id} color cover written -> {}",
                                out.display()
                            );
                            produced.cover_path = Some(out);

                            // Also swap the cover inside the just-produced
                            // EPUB so external readers see color too. Best-
                            // effort — log and continue on failure rather
                            // than failing the whole job over a cosmetic
                            // EPUB tweak.
                            if let Some(epub) = &produced.epub_path {
                                match epub_cover::replace_cover(epub, &bytes, "jpg") {
                                    Ok(()) => eprintln!(
                                        "[sidle/queue] book {book_id} color cover \
                                         swapped inside epub"
                                    ),
                                    Err(e) => eprintln!(
                                        "[sidle/queue] book {book_id} epub cover \
                                         swap failed: {e:#}"
                                    ),
                                }
                            }

                            // Swap the cover inside the imported KFX too — that's
                            // the copy we push to the Kindle, and its embedded
                            // cover is what the home tile / sleep-screen renders.
                            // A store KFX can ship a publisher placeholder there
                            // (e.g. a house-logo) instead of the real art. The
                            // rewrite changes the file's bytes, so we re-stamp
                            // `kfx_sha256` (the on-device filename infix); a prior
                            // push keyed on the old hash is now stale and a
                            // re-push lands a fresh file, same as any reconvert.
                            if let Some(kfx) = book.kfx_path.as_deref() {
                                match kfx_cover::replace_cover(Path::new(kfx), &bytes) {
                                    Ok(new_sha) => {
                                        let conn = db.lock().await;
                                        let _ = db::set_kfx_path_and_sha(
                                            &conn, book_id, kfx, &new_sha,
                                        );
                                        drop(conn);
                                        eprintln!(
                                            "[sidle/queue] book {book_id} color cover \
                                             swapped inside kfx (kfx_sha256 re-stamped)"
                                        );
                                    }
                                    Err(e) => eprintln!(
                                        "[sidle/queue] book {book_id} kfx cover \
                                         swap failed: {e:#}"
                                    ),
                                }
                            }
                        }
                    }
                    None => {
                        eprintln!(
                            "[sidle/queue] book {book_id} color cover: fetch returned None; \
                             keeping the cover embedded in the KFX"
                        );
                    }
                }
            }
        }
    }

    {
        let conn = db.lock().await;
        if let Some(epub) = &produced.epub_path {
            let _ = db::set_epub_path(&conn, book_id, &epub.to_string_lossy());
        }
        if let Some(kfx) = &produced.kfx_path {
            // Hash the freshly-written KFX so push can stamp the on-device
            // filename with its `<sha8>` infix. Without this the on-device
            // file's identity drifts from the local row's `kfx_sha256`
            // (still None) and re-import wouldn't link back.
            match sha256_of_file(kfx) {
                Ok(sha) => {
                    let _ = db::set_kfx_path_and_sha(
                        &conn,
                        book_id,
                        &kfx.to_string_lossy(),
                        &sha,
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[sidle/queue] book {book_id}: hashing produced KFX failed: {e}; \
                         row will be unsendable until reconvert"
                    );
                }
            }
        }
        if let Some(pdf) = &produced.pdf_path {
            let _ = db::set_pdf_path(&conn, book_id, &pdf.to_string_lossy());
        }
        if let Some(cover) = &produced.cover_path {
            let _ = db::set_cover_path(&conn, book_id, &cover.to_string_lossy());
            // Refresh the picker thumbnail to match the produced cover.
            // Best-effort (see library::thumbnail).
            let _ = crate::library::thumbnail::ensure_thumbnail(paths, &book.sha256, cover);
        }
        if let Some(asin) = &produced.asin {
            let _ = db::set_asin(&conn, book_id, asin);
        }
    }
    eprintln!(
        "[sidle/queue] book {book_id} done in {:.2}s",
        started.elapsed().as_secs_f32()
    );
    mark_status(db, app, book_id, "done", None).await;
}

async fn lookup_book(db: &DbHandle, book_id: i64) -> Option<BookRow> {
    let conn = db.lock().await;
    db::get_book(&conn, book_id).ok().flatten()
}

async fn mark_status(
    db: &DbHandle,
    app: &AppHandle,
    book_id: i64,
    status: &str,
    error: Option<&str>,
) {
    {
        let conn = db.lock().await;
        let _ = db::set_job_status(&conn, book_id, status, error);
    }
    emit_status(app, book_id, status, error);
}

/// Outputs the worker produced — populated columns get written back to the
/// book row on success.
#[derive(Default)]
struct Produced {
    epub_path: Option<PathBuf>,
    kfx_path: Option<PathBuf>,
    pdf_path: Option<PathBuf>,
    cover_path: Option<PathBuf>,
    /// ASIN boko stamped into the produced KFX. For EPUB→KFX this is the
    /// fabricated 32-char value (unless the source EPUB carried a real
    /// one); the device-delete path keys catalog-style `.sdr/` cleanup on
    /// it. None for the kfx→epub direction (no KFX produced).
    asin: Option<String>,
}

fn run_direction(paths: &LibraryPaths, book: &BookRow, kind: &str) -> anyhow::Result<Produced> {
    match kind {
        "epub_to_kfx" => convert_epub_to_kfx(paths, book),
        "kfx_to_epub" => convert_kfx_to_epub(paths, book),
        "pdf_to_kfx" => convert_pdf_to_kfx(paths, book),
        "kfx_to_pdf" => convert_kfx_to_pdf(paths, book),
        other => Err(anyhow::anyhow!("unknown job kind: {other}")),
    }
}

fn convert_epub_to_kfx(paths: &LibraryPaths, book: &BookRow) -> anyhow::Result<Produced> {
    let source = book
        .epub_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("epub_to_kfx job has no epub_path"))?;
    let source_path = Path::new(source);

    paths.ensure_sha(&book.sha256)?;
    let dir = paths.book_dir(&book.sha256);
    let base = derived_basename(book, source_path);
    let out_path = dir.join(format!("{base}.kfx"));
    let tmp_path = dir.join(format!("{base}.kfx.partial"));

    let mut handle = boko::Book::open(source_path)?;
    // Bake the library's (possibly user-edited) metadata into the KFX without
    // rewriting the source EPUB: clone the parsed metadata, overlay the DB
    // fields, and install it as an override the KFX exporter reads. A first
    // import is a no-op (DB == source); only edits diverge. See
    // `book_metadata_override`.
    handle.set_metadata_override(book_metadata_override(handle.metadata(), book));
    let mut writer = File::create(&tmp_path)?;
    handle.export(boko::Format::Kfx, &mut writer)?;
    writer.sync_all().ok();
    drop(writer);
    std::fs::rename(&tmp_path, &out_path)?;

    // The KFX export stamps an ASIN — either the source's (if real) or a
    // fabricated 32-char value derived from the publication identifier.
    // The DB row started life from the EPUB metadata which usually has no
    // ASIN at all; we need the stamped value on the row so device-delete
    // can wipe Kindle's `<title>_<ASIN>.sdr/` catalog sidecar.
    let asin = boko::kfx::metadata::resolve_export_asin(handle.metadata());

    Ok(Produced {
        kfx_path: Some(out_path),
        asin,
        ..Default::default()
    })
}

fn convert_kfx_to_epub(paths: &LibraryPaths, book: &BookRow) -> anyhow::Result<Produced> {
    let source = book
        .kfx_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("kfx_to_epub job has no kfx_path"))?;
    let source_path = Path::new(source);

    paths.ensure_sha(&book.sha256)?;
    let dir = paths.book_dir(&book.sha256);
    let base = derived_basename(book, source_path);
    let out_path = dir.join(format!("{base}.epub"));

    // Mechanical port: KFX bytes → EPUB bytes. The intermediate `.kfx` is
    // already persisted (import wrote it before enqueueing), so we read it
    // back here rather than threading the bytes through the queue.
    let kfx_bytes = std::fs::read(source_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", source_path.display()))?;
    let epub_bytes = boko::kfx_to_epub::convert_to_epub(&kfx_bytes)
        .map_err(|e| anyhow::anyhow!("boko kfx→epub: {e}"))?;
    write_bytes_atomic(&out_path, &epub_bytes)?;

    // Cover sidecar — `kfx_to_epub` already transcoded any JXR to JPG and
    // marked the manifest entry as `cover-image`, so this is a plain copy
    // out of the produced zip.
    let cover_path = extract_cover_from_epub(&epub_bytes).and_then(|(bytes, ext)| {
        let out = paths.cover(&book.sha256, ext);
        std::fs::write(&out, &bytes).ok().map(|_| out)
    });

    Ok(Produced {
        epub_path: Some(out_path),
        cover_path,
        ..Default::default()
    })
}

/// PDF → KFX: wrap the PDF verbatim into a fixed-layout PDOC KFX for the Scribe.
/// The book's PDF side is the source; the produced KFX is what gets pushed to
/// the device. See .claude/plans/pdf-to-kfx.md.
fn convert_pdf_to_kfx(paths: &LibraryPaths, book: &BookRow) -> anyhow::Result<Produced> {
    let source = book
        .pdf_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("pdf_to_kfx job has no pdf_path"))?;
    let source_path = Path::new(source);

    paths.ensure_sha(&book.sha256)?;
    let dir = paths.book_dir(&book.sha256);
    let base = derived_basename(book, source_path);
    let out_path = dir.join(format!("{base}.kfx"));

    let bytes = std::fs::read(source_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", source_path.display()))?;
    let doc = boko::import::probe_pdf(bytes).map_err(|e| anyhow::anyhow!("probe pdf: {e}"))?;
    let meta = boko::export::PdfKfxMeta {
        title: book.title.clone(),
        author: (!book.author.trim().is_empty()).then(|| book.author.clone()),
        language: if book.language.is_empty() {
            "en".to_string()
        } else {
            book.language.clone()
        },
        // Edited publication year/publisher reach the KFX book_metadata too —
        // not just the renamed filename — so the device's book details match.
        date: book.published_at.clone(),
        publisher: book.publisher.clone(),
    };
    // Cover precedence: an existing sidecar — a cover the user fixed via
    // "Change cover…", or the page-1 render from a prior convert — wins over a
    // fresh page-1 render, so a force-reconvert never clobbers a corrected
    // cover. (The original page-1 of a coverless PDF is just a title page; the
    // whole point of the edit is to replace it.) Whatever we use is normalized
    // to a sleep-screen-safe JFIF JPEG via `sanitize_for_kfx`, which also
    // transcodes a PNG/WebP sidecar. Only when there's no usable sidecar (first
    // import, or an unreadable file) do we render page 1. The PDF engine
    // (PDFKit) is optional — on render failure log and proceed cover-less.
    let existing_cover = book
        .cover_path
        .as_deref()
        .map(Path::new)
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|raw| {
            let jpeg = boko::kfx::image_transcode::sanitize_for_kfx(&raw).unwrap_or(raw);
            is_jpeg(&jpeg).then_some(jpeg)
        });
    let cover_jpeg = match existing_cover {
        Some(jpeg) => Some(jpeg),
        None => match boko::render::render_pdf_page_jpeg(
            &doc.bytes,
            0,
            boko::render::COVER_TARGET_WIDTH_PX,
            boko::render::COVER_JPEG_QUALITY,
        ) {
            Ok(jpeg) => Some(jpeg),
            Err(e) => {
                eprintln!("[sidle/queue] book {} pdf cover: skipped ({e})", book.id);
                None
            }
        },
    };

    // Selectable text layer (PDFKit) — optional, like the cover; on failure the
    // book is converted visual-only.
    let text = boko::render::extract_pdf_text(&doc.bytes).ok();
    match &text {
        Some(pages) => {
            let runs: usize = pages.iter().map(|p| p.runs.len()).sum();
            eprintln!(
                "[sidle/queue] book {} pdf text: {runs} runs / {} pages",
                book.id,
                pages.len()
            );
        }
        None => eprintln!("[sidle/queue] book {} pdf text: skipped", book.id),
    }

    let kfx = boko::export::pdf_to_kfx(&doc, &meta, cover_jpeg.as_deref(), text.as_deref());
    write_bytes_atomic(&out_path, &kfx)?;

    // Warm the ink-anchor geometry cache (eid→page map + page boxes) for this
    // KFX, keyed by the same kfx_sha256 the row will carry. Without this, the
    // first ink sync after a (re)convert re-parses the whole KFX per book
    // (~0.5 s release / ~1.4 s debug each) under the DB lock — what made
    // "sync annotations" slow. Computed from the in-memory bytes (no re-read of
    // the 15 MB file); best-effort — a miss just falls back to the lazy parse.
    let kfx_sha = sha256_of_bytes(&kfx);
    let geom = pdf_geom::compute(&kfx);
    if let Err(e) = pdf_geom::write_sidecar(paths, &book.sha256, &kfx_sha, &geom) {
        eprintln!("[sidle/queue] book {} pdf geom cache: skipped ({e:#})", book.id);
    } else {
        eprintln!(
            "[sidle/queue] book {} pdf geom cache: {} pages",
            book.id,
            geom.len()
        );
    }

    // Capture the content_id boko baked into the KFX — the device names its
    // per-book `.sdr` / `.notebooks/<id>!!PDOC!!` dir after it, so `books.asin`
    // MUST equal it (annotation + ink sync and device-delete all match on it).
    // Read it back from the produced file rather than recomputing, so the row
    // always reflects what's actually in the KFX (a recompute via
    // `resolve_export_asin` used a different input and never matched). `None` on a
    // read failure just defers to the bootstrap backfill.
    let asin = boko::Book::open(&out_path)
        .ok()
        .and_then(|b| b.metadata().asin.clone());

    // Sidecar for the library tile. Reuse an existing sidecar verbatim (it's the
    // user's chosen image, possibly png/webp — keep its bytes for the gallery
    // even though the KFX got a JPEG-normalized copy); only write a fresh
    // `cover.jpg` when we rendered page 1.
    let cover_path = match book
        .cover_path
        .as_deref()
        .map(Path::new)
        .filter(|p| p.exists())
    {
        Some(existing) => Some(existing.to_path_buf()),
        None => cover_jpeg.and_then(|jpeg| {
            let out = paths.cover(&book.sha256, "jpg");
            match write_bytes_atomic(&out, &jpeg) {
                Ok(()) => Some(out),
                Err(e) => {
                    eprintln!("[sidle/queue] book {} pdf cover write failed: {e}", book.id);
                    None
                }
            }
        }),
    };

    Ok(Produced {
        kfx_path: Some(out_path),
        asin,
        cover_path,
        ..Default::default()
    })
}

/// KFX → PDF: extract the verbatim embedded PDF from a PDF-backed container KFX
/// (a synced-back PDF book, or the return side of a `.kfx` import). The PDF is
/// byte-identical to what was embedded.
fn convert_kfx_to_pdf(paths: &LibraryPaths, book: &BookRow) -> anyhow::Result<Produced> {
    let source = book
        .kfx_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("kfx_to_pdf job has no kfx_path"))?;
    let source_path = Path::new(source);

    paths.ensure_sha(&book.sha256)?;
    let dir = paths.book_dir(&book.sha256);
    let base = derived_basename(book, source_path);
    let out_path = dir.join(format!("{base}.pdf"));

    let kfx_bytes = std::fs::read(source_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", source_path.display()))?;
    let pdf = boko::kfx::pdf_container::kfx_extract_pdf(&kfx_bytes)
        .map_err(|e| anyhow::anyhow!("kfx→pdf extract: {e}"))?;
    write_bytes_atomic(&out_path, &pdf)?;

    // Cover sidecar. Import already writes one (it pulls the cover the KFX
    // carries built-in), so on the normal path the row has it and the worker
    // leaves it alone; this is the self-sufficient fallback (force-reconvert, or
    // a row that somehow lacks one). An existing sidecar — including a user's
    // "Change cover…" — wins; only when there's none do we extract from the KFX.
    // The PDF-backed container embeds its cover the same way a reflowable one
    // does, so this recovers it without re-rendering page 1 (which would discard
    // a custom cover). Best-effort: a coverless KFX just yields no sidecar.
    let cover_path = match book
        .cover_path
        .as_deref()
        .map(Path::new)
        .filter(|p| p.exists())
    {
        Some(existing) => Some(existing.to_path_buf()),
        None => extract_cover_from_kfx_bytes(&kfx_bytes).and_then(|(bytes, ext)| {
            let out = paths.cover(&book.sha256, ext);
            std::fs::write(&out, &bytes).ok().map(|_| out)
        }),
    };

    Ok(Produced {
        pdf_path: Some(out_path),
        cover_path,
        ..Default::default()
    })
}

/// JPEG magic (`FF D8 FF`). `pdf_to_kfx` embeds the cover as a `format:jpg`
/// resource and reads its JPEG dimensions, so a non-JPEG cover would corrupt it
/// — gate on this and fall back to a page-1 render when a sidecar isn't (and
/// couldn't be normalized to) a JPEG.
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= 3 && bytes[0..3] == [0xFF, 0xD8, 0xFF]
}

/// Overlay the library row's metadata onto the source's parsed metadata,
/// producing the [`boko::Metadata`] the KFX export should write. Cloned from
/// `source` so fields the DB doesn't track (identifier, ASIN, cover_image,
/// description, writing-mode…) survive untouched; the tracked fields take the
/// edited DB value. For an unedited book the DB still equals what import read
/// from the source, so this is a no-op.
fn book_metadata_override(source: &boko::Metadata, book: &BookRow) -> boko::Metadata {
    let mut m = source.clone();
    m.title = book.title.clone();
    m.authors = crate::library::authors::split_display(&book.author);
    if !book.language.trim().is_empty() {
        m.language = book.language.clone();
    }
    m.publisher = book.publisher.clone();
    m.date = book.published_at.clone();
    m.collection = book.series_name.clone().map(|name| boko::model::CollectionInfo {
        name,
        collection_type: Some("series".to_string()),
        position: book.series_index,
    });
    m
}

/// Reproduce the import-time basename. The source file's stem already encodes
/// `[Author] Title (Year)` (import writes it that way), so we fall back to
/// the stem and only re-derive from metadata if the stem is missing or empty.
fn derived_basename(book: &BookRow, source: &Path) -> String {
    if let Some(stem) = source.file_stem().and_then(|s| s.to_str())
        && !stem.is_empty() {
            return stem.to_string();
        }
    let authors = crate::library::authors::split_display(&book.author);
    format_basename(&authors, &book.title, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str, author: &str) -> BookRow {
        BookRow {
            id: 1,
            sha256: "sha".into(),
            title: title.into(),
            author: author.into(),
            language: "en".into(),
            ppd: None,
            epub_path: None,
            cover_path: None,
            kfx_path: None,
            kfx_sha256: None,
            pdf_path: None,
            file_size: 0,
            imported_at: "2026-01-01".into(),
            status: "done".into(),
            error: None,
            kind: Some("epub_to_kfx".into()),
            asin: None,
            publisher: None,
            published_at: None,
            series_name: None,
            series_index: None,
            tags: vec![],
        }
    }

    #[test]
    fn override_applies_db_fields_but_preserves_untracked() {
        let mut src = boko::Metadata {
            title: "Old Title".into(),
            authors: vec!["Old Author".into()],
            language: "ja".into(),
            ..Default::default()
        };
        // Untracked-by-DB fields that must survive the overlay.
        src.identifier = "urn:isbn:9999".into();
        src.asin = Some("B00REALASIN".into());
        src.cover_image = Some("OEBPS/cover.xhtml".into());

        let mut r = row("New Title", "Ann Author & Bob Writer");
        r.publisher = Some("New Press".into());
        r.published_at = Some("2021".into());
        r.series_name = Some("Saga".into());
        r.series_index = Some(2.0);

        let m = book_metadata_override(&src, &r);

        // Edited fields take the DB value.
        assert_eq!(m.title, "New Title");
        assert_eq!(m.authors, vec!["Ann Author".to_string(), "Bob Writer".to_string()]);
        assert_eq!(m.language, "en");
        assert_eq!(m.publisher.as_deref(), Some("New Press"));
        assert_eq!(m.date.as_deref(), Some("2021"));
        assert_eq!(m.collection.as_ref().map(|c| c.name.as_str()), Some("Saga"));
        assert_eq!(m.collection.as_ref().and_then(|c| c.position), Some(2.0));
        // Untracked identity fields are untouched, so the KFX keeps its identity.
        assert_eq!(m.identifier, "urn:isbn:9999");
        assert_eq!(m.asin.as_deref(), Some("B00REALASIN"));
        assert_eq!(m.cover_image.as_deref(), Some("OEBPS/cover.xhtml"));
    }

    #[test]
    fn override_keeps_source_language_when_db_blank() {
        let src = boko::Metadata {
            language: "ja".into(),
            ..Default::default()
        };
        let mut r = row("T", "A");
        r.language = String::new();
        assert_eq!(book_metadata_override(&src, &r).language, "ja");
    }

    #[test]
    fn is_jpeg_gates_on_magic() {
        assert!(is_jpeg(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(!is_jpeg(&[0x89, 0x50, 0x4E, 0x47])); // PNG
        assert!(!is_jpeg(&[0xFF, 0xD8])); // truncated
        assert!(!is_jpeg(&[]));
    }
}

