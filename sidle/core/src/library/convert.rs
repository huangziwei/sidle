//! The conversion pipeline: one book, source format → target format.
//!
//! Both directions share the shape — read the source the row points at, run
//! bokai, write the output beside it, record the produced paths on the row, and
//! bring everything derived from the file's shape (cover, position axis,
//! annotation anchors) back into agreement with the new bytes. The direction
//! (`epub_to_kfx`, `kfx_to_epub`, `pdf_to_kfx`, `kfx_to_pdf`) is stored on the
//! `conversion_jobs` row at import time; [`convert_book`] dispatches on it.
//!
//! Synchronous from end to end, the cover fetch included. Minutes of CPU per
//! book: the desktop queue calls it on a blocking task, the CLI on a worker
//! thread.

use std::fs::File;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::library::db::{self, BookRow};
use crate::library::import::{
    extract_cover_from_epub_file, extract_cover_from_kfx_bytes, sha256_of_bytes, sha256_of_file,
    write_bytes_atomic,
};
use crate::library::paths::{LibraryPaths, format_basename};
use crate::library::{cover_fetch, epub_cover, extent, kfx_cover, pdf_geom, thumbnail};

/// Whether a conversion may write to the source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// First conversion after an import. Runs the cover-enrichment tail, which
    /// re-fetches the colour cover from the catalogue, writes it into the
    /// source KFX, and mints `kfx_sha256`.
    Import,
    /// A forced re-run of the format conversion, source→target **only**. The
    /// cover-enrichment tail is skipped, leaving the source KFX and
    /// `kfx_sha256` as they stand.
    Reconvert,
}

impl Mode {
    fn is_reconvert(self) -> bool {
        matches!(self, Mode::Reconvert)
    }
}

/// Conversion progress as bokai reports it: `(phase, current, total, human
/// label)`. [`crate::library::progress::fraction`] maps it to a bar.
pub type OnProgress<'a> = &'a dyn Fn(&str, usize, usize, &str);

/// What one conversion changed.
#[derive(Debug, Default)]
pub struct Converted {
    pub epub_path: Option<PathBuf>,
    pub kfx_path: Option<PathBuf>,
    pub pdf_path: Option<PathBuf>,
    pub cover_path: Option<PathBuf>,
    /// The book's position axis as the produced file measures it. `None` for a
    /// file that carries no position map.
    pub max_position: Option<i64>,
    /// Annotations whose anchors moved to follow their text through the
    /// rebuild.
    pub reanchored: usize,
    /// Annotations whose text was found nowhere, or in several places, in the
    /// rebuilt book. Their anchors are unchanged.
    pub stranded: usize,
}

/// Convert one book, and bring the library row and everything derived from the
/// file back into agreement with the result.
///
/// Marks the job `converting` on entry and `done`/`error` on the way out. An
/// `Err` is both recorded and returned.
///
/// Holds `conn` for the whole conversion. [`run`] + [`record`] is the same work
/// split, with the minutes-long half taking no database.
pub fn convert_book(
    conn: &Connection,
    paths: &LibraryPaths,
    book_id: i64,
    mode: Mode,
    on_progress: OnProgress<'_>,
) -> anyhow::Result<Converted> {
    let book = db::get_book(conn, book_id)?
        .ok_or_else(|| anyhow::anyhow!("book {book_id} vanished before conversion"))?;
    let kind = book
        .kind
        .clone()
        .ok_or_else(|| anyhow::anyhow!("book {book_id} has no job kind"))?;

    db::set_job_status(conn, book_id, "converting", None)?;
    match run(paths, &book, &kind, mode, on_progress).and_then(|p| record(conn, &book, p)) {
        Ok(converted) => {
            db::set_job_status(conn, book_id, "done", None)?;
            Ok(converted)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let _ = db::set_job_status(conn, book_id, "error", Some(&msg));
            Err(e)
        }
    }
}

/// The half of a conversion that touches no database: run bokai, write the
/// produced file, fetch and embed the cover, parse the rebuilt book's position
/// axis and text index.
///
/// Minutes of CPU on a large book. [`record`] takes the result.
pub fn run(
    paths: &LibraryPaths,
    book: &BookRow,
    kind: &str,
    mode: Mode,
    on_progress: OnProgress<'_>,
) -> anyhow::Result<Produced> {
    let mut produced = run_direction(paths, book, kind, on_progress)?;

    // A KFX from Amazon's monochrome-device build carries a grayscale-baked
    // cover; the colour one comes from the product page, keyed by the catalogue
    // ASIN. A KFX bokai produced from a colour EPUB carries no such ASIN, and
    // [`enrich_cover`] leaves its embedded cover alone.
    if kind == "kfx_to_epub" && !mode.is_reconvert() {
        enrich_cover(paths, book, &mut produced);
    }

    // The picker's thumbnail is derived from `produced.cover_path`.
    // Best-effort; see `library::thumbnail`.
    if let Some(cover) = &produced.cover_path {
        let _ = thumbnail::ensure_thumbnail(paths, &book.sha256, cover);
    }

    // The KFX the row points at: `produced.kfx_path` for epub→kfx, else the
    // imported source `book.kfx_path`.
    let indexed = produced
        .kfx_path
        .clone()
        .or_else(|| book.kfx_path.as_deref().map(PathBuf::from));
    if let Some(kfx) = indexed {
        // A Kindle session log names a position, not a book. `max_position` is
        // the axis the Reading Log matches those positions against.
        produced.max_position = extent::of_file(&kfx);
        if produced.max_position.is_none() {
            eprintln!(
                "[sidle/convert] book {}: no position map in the produced KFX; \
                 reading time on this book cannot be attributed",
                book.id
            );
        }
        // An annotation's element ids belong to the build it was made against;
        // its text survives the rebuild. [`record`] re-finds that text here.
        produced.index = std::fs::read(&kfx)
            .ok()
            .and_then(|bytes| crate::library::anchor::BookIndex::from_kfx(&bytes));
        produced.indexed_kfx = Some(kfx);
    }

    Ok(produced)
}

/// The half of a conversion that touches only the database: record the produced
/// paths on the book row, store the position axis, and move the book's
/// annotations onto the rebuilt text.
///
/// Fast: [`run`] owns every expensive parse.
pub fn record(conn: &Connection, book: &BookRow, produced: Produced) -> anyhow::Result<Converted> {
    let book_id = book.id;
    if let Some(epub) = &produced.epub_path {
        db::set_epub_path(conn, book_id, &epub.to_string_lossy())?;
    }
    if let Some(kfx) = &produced.kfx_path {
        let kfx_str = kfx.to_string_lossy();
        match book.kfx_sha256.as_deref() {
            // `kfx_sha256` is a frozen identity: the on-device filename embeds
            // its `<sha8>` and each `.sdr` is bound to that exact name. The
            // existing value is passed back, which also skips the re-hash.
            Some(sha) => db::set_kfx_path_and_sha(conn, book_id, &kfx_str, sha)?,
            // No identity yet: mint one from the fresh KFX bytes.
            None => {
                let sha = sha256_of_file(kfx)?;
                db::set_kfx_path_and_sha(conn, book_id, &kfx_str, &sha)?;
            }
        }
    }
    // [`enrich_cover`] rewrote the source KFX in place. `set_kfx_path_and_sha`
    // COALESCEs the hash, leaving an existing `kfx_sha256` as it stands.
    if let (Some(kfx), Some(sha)) = (book.kfx_path.as_deref(), &produced.enriched_kfx_sha) {
        db::set_kfx_path_and_sha(conn, book_id, kfx, sha)?;
    }
    if let Some(pdf) = &produced.pdf_path {
        db::set_pdf_path(conn, book_id, &pdf.to_string_lossy())?;
    }
    if let Some(cover) = &produced.cover_path {
        db::set_cover_path(conn, book_id, &cover.to_string_lossy())?;
    }
    if let Some(asin) = &produced.asin {
        db::set_asin(conn, book_id, asin)?;
    }

    let mut converted = Converted {
        epub_path: produced.epub_path,
        kfx_path: produced.kfx_path,
        pdf_path: produced.pdf_path,
        cover_path: produced.cover_path,
        max_position: produced.max_position,
        ..Default::default()
    };

    if let Some(index) = &produced.index {
        match crate::library::reanchor::book(conn, book_id, index) {
            Ok(done) => {
                converted.reanchored = done.moved;
                converted.stranded = done.stranded;
            }
            Err(e) => eprintln!("[sidle/convert] book {book_id}: re-anchor failed: {e}"),
        }
    }
    if produced.indexed_kfx.is_some() {
        // `None` stores as 0, "this file has no axis", overwriting any extent
        // on the row.
        db::set_max_position(conn, book_id, produced.max_position)?;
        // Reading sessions stored against a position that matched no book get
        // another pass against this axis.
        if produced.max_position.is_some() {
            db::resolve_reading_sessions(conn)?;
        }
    }
    Ok(converted)
}

/// Re-fetch the catalogue colour cover and put it everywhere this book keeps
/// one: the sidecar, the produced EPUB, and the source KFX.
///
/// Best-effort throughout: every failure here leaves the conversion's result
/// intact.
fn enrich_cover(paths: &LibraryPaths, book: &BookRow, produced: &mut Produced) {
    let book_id = book.id;
    let Some(asin) = book.amazon_asin.as_deref() else {
        eprintln!("[sidle/convert] book {book_id} colour cover: no catalogue ASIN on row");
        return;
    };
    let Some(bytes) = cover_fetch::fetch_color_cover(asin, &book.language) else {
        eprintln!(
            "[sidle/convert] book {book_id} colour cover: nothing returned; \
             keeping the cover embedded in the KFX"
        );
        return;
    };

    let out = paths.cover(&book.sha256, "jpg");
    if let Err(e) = std::fs::write(&out, &bytes) {
        eprintln!("[sidle/convert] book {book_id} colour cover write failed: {e}");
        return;
    }
    // A cover the conversion wrote under a different extension is removed,
    // leaving one cover file on disk.
    if let Some(old) = &produced.cover_path
        && old != &out
    {
        let _ = std::fs::remove_file(old);
    }
    produced.cover_path = Some(out);

    // The same colour cover goes into the produced EPUB. This tail runs for
    // `kfx_to_epub` alone, where the EPUB is the derived side of the KFX.
    if let Some(epub) = &produced.epub_path {
        let kfx = book.kfx_path.as_deref().map(Path::new);
        if let Err(e) = epub_cover::ensure_cover(epub, kfx, &bytes, "jpg", true) {
            eprintln!("[sidle/convert] book {book_id} epub cover swap failed: {e:#}");
        }
    }

    // And into the imported KFX, the copy pushed to the Kindle, whose embedded
    // cover the home tile and sleep screen render. `enriched_kfx_sha` carries
    // the rewritten file's hash to [`record`].
    if let Some(kfx) = book.kfx_path.as_deref() {
        match kfx_cover::replace_cover(Path::new(kfx), &bytes) {
            Ok(new_sha) => produced.enriched_kfx_sha = Some(new_sha),
            Err(e) => eprintln!("[sidle/convert] book {book_id} kfx cover swap failed: {e:#}"),
        }
    }
}

/// What [`run`] produced, for [`record`] to write down. Not `Send`: `index`
/// holds a parse that stays on the converting thread.
#[derive(Default)]
pub struct Produced {
    pub epub_path: Option<PathBuf>,
    pub kfx_path: Option<PathBuf>,
    pub pdf_path: Option<PathBuf>,
    pub cover_path: Option<PathBuf>,
    /// ASIN bokai stamped into the produced KFX: the source EPUB's real value,
    /// else a fabricated 32-char one. `None` for kfx→epub, which produces no
    /// KFX. Device-delete keys `.sdr/` cleanup on it.
    pub asin: Option<String>,
    /// The source KFX's hash, as [`enrich_cover`] left it.
    pub enriched_kfx_sha: Option<String>,
    pub max_position: Option<i64>,
    /// The KFX `max_position` and `index` were read from. `None` for a book
    /// with no KFX at all, separating that from a KFX with no position map.
    pub indexed_kfx: Option<PathBuf>,
    /// The rebuilt book's text index, which annotations are re-anchored onto.
    pub index: Option<crate::library::anchor::BookIndex>,
}

fn run_direction(
    paths: &LibraryPaths,
    book: &BookRow,
    kind: &str,
    on_progress: OnProgress<'_>,
) -> anyhow::Result<Produced> {
    match kind {
        "epub_to_kfx" => convert_epub_to_kfx(paths, book, on_progress),
        "kfx_to_epub" => convert_kfx_to_epub(paths, book, on_progress),
        "pdf_to_kfx" => convert_pdf_to_kfx(paths, book, on_progress),
        "kfx_to_pdf" => convert_kfx_to_pdf(paths, book, on_progress),
        other => Err(anyhow::anyhow!("unknown job kind: {other}")),
    }
}

fn convert_epub_to_kfx(
    paths: &LibraryPaths,
    book: &BookRow,
    on_progress: OnProgress<'_>,
) -> anyhow::Result<Produced> {
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

    // Every step names itself and the file it read, and the failure report
    // shows that message.
    let mut handle = bokai::Book::open(source_path)
        .map_err(|e| anyhow::anyhow!("bokai epub→kfx (load {}): {e}", source_path.display()))?;
    // The row's metadata reaches the KFX as an override the exporter reads,
    // leaving the source EPUB untouched. See [`book_metadata_override`].
    handle.set_metadata_override(book_metadata_override(handle.metadata(), book));
    // Interior plates are full-colour JXR. A grayscale source page collapses to
    // `8bppGray` inside the encoder. The cover is JPEG under either mode.
    handle.set_image_color_mode(bokai::jxr::ColorMode::Color);
    let mut writer = File::create(&tmp_path)
        .map_err(|e| anyhow::anyhow!("create {}: {e}", tmp_path.display()))?;
    let exported = handle.export_with_progress(bokai::Format::Kfx, &mut writer, on_progress);
    writer.sync_all().ok();
    drop(writer);
    if let Err(e) = exported {
        // The half-written `.partial` is removed.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow::anyhow!("bokai epub→kfx: {e}"));
    }
    std::fs::rename(&tmp_path, &out_path).map_err(|e| {
        anyhow::anyhow!(
            "rename {} -> {}: {e}",
            tmp_path.display(),
            out_path.display()
        )
    })?;

    // The ASIN the export stamped: the source's real value, else one fabricated
    // from the publication identifier. Device-delete matches Kindle's
    // `<title>_<ASIN>.sdr/` against the row's copy.
    let asin = bokai::formats::kfx::metadata::resolve_export_asin(handle.metadata());

    Ok(Produced {
        kfx_path: Some(out_path),
        asin,
        ..Default::default()
    })
}

fn convert_kfx_to_epub(
    paths: &LibraryPaths,
    book: &BookRow,
    on_progress: OnProgress<'_>,
) -> anyhow::Result<Produced> {
    let source = book
        .kfx_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("kfx_to_epub job has no kfx_path"))?;
    let source_path = Path::new(source);

    paths.ensure_sha(&book.sha256)?;
    let dir = paths.book_dir(&book.sha256);
    let base = derived_basename(book, source_path);
    let out_path = dir.join(format!("{base}.epub"));
    let tmp_path = dir.join(format!("{base}.epub.partial"));

    // IR route: the `.kfx` import wrote → Book → EPUB. `open_format` is the
    // container parse (the `load` phase), reading each entity's payload off the
    // file as the export asks for it; `export_with_progress` emits
    // content/resources/nav/finalize straight into the output file.
    on_progress("load", 0, 1, "Reading KFX");
    let mut handle = bokai::Book::open_format(source_path, bokai::Format::Kfx)
        .map_err(|e| anyhow::anyhow!("bokai kfx→epub (load {}): {e}", source_path.display()))?;
    let mut writer = File::create(&tmp_path)
        .map_err(|e| anyhow::anyhow!("create {}: {e}", tmp_path.display()))?;
    let exported = handle.export_with_progress(bokai::Format::Epub, &mut writer, on_progress);
    writer.sync_all().ok();
    drop(writer);
    if let Err(e) = exported {
        // The half-written `.partial` is removed.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow::anyhow!("bokai kfx→epub: {e}"));
    }
    std::fs::rename(&tmp_path, &out_path).map_err(|e| {
        anyhow::anyhow!(
            "rename {} -> {}: {e}",
            tmp_path.display(),
            out_path.display()
        )
    })?;

    // Cover sidecar: a plain copy out of the produced zip, whose manifest entry
    // the exporter marked `cover-image` and whose JXR it transcoded to JPG.
    let cover_path = extract_cover_from_epub_file(&out_path).and_then(|(bytes, ext)| {
        let out = paths.cover(&book.sha256, ext);
        std::fs::write(&out, &bytes).ok().map(|_| out)
    });

    Ok(Produced {
        epub_path: Some(out_path),
        cover_path,
        ..Default::default()
    })
}

/// PDF → KFX: the book's PDF wrapped verbatim into a fixed-layout PDOC KFX,
/// the copy pushed to a Scribe.
fn convert_pdf_to_kfx(
    paths: &LibraryPaths,
    book: &BookRow,
    on_progress: OnProgress<'_>,
) -> anyhow::Result<Produced> {
    let source = book
        .pdf_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("pdf_to_kfx job has no pdf_path"))?;
    let source_path = Path::new(source);

    paths.ensure_sha(&book.sha256)?;
    let dir = paths.book_dir(&book.sha256);
    let base = derived_basename(book, source_path);
    let out_path = dir.join(format!("{base}.kfx"));

    on_progress("probe", 0, 1, "Reading PDF");
    let bytes = std::fs::read(source_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", source_path.display()))?;
    let doc = bokai::import::probe_pdf(bytes).map_err(|e| anyhow::anyhow!("probe pdf: {e}"))?;
    let meta = bokai::export::PdfKfxMeta {
        title: book.title.clone(),
        author: (!book.author.trim().is_empty()).then(|| book.author.clone()),
        language: if book.language.is_empty() {
            "en".to_string()
        } else {
            book.language.clone()
        },
        // The row's year and publisher reach KFX `book_metadata`, alongside the
        // filename derived from them.
        date: book.published_at.clone(),
        publisher: book.publisher.clone(),
        // A PDF states no reading direction. `ppd` on the row is the only
        // source of one; unset, the book reads ltr.
        page_progression_direction: book.ppd.clone(),
    };
    // Cover precedence: the sidecar on the row wins over a page-1 render, and
    // `sanitize_for_kfx` normalizes it to a sleep-screen-safe JFIF JPEG,
    // transcoding a PNG or WebP one. A failed render leaves the KFX coverless.
    on_progress("cover", 0, 1, "Rendering cover");
    let existing_cover = book
        .cover_path
        .as_deref()
        .map(Path::new)
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|raw| {
            let jpeg = bokai::image::jpeg::sanitize_for_kfx(&raw).unwrap_or(raw);
            is_jpeg(&jpeg).then_some(jpeg)
        });
    let cover_jpeg = match existing_cover {
        Some(jpeg) => Some(jpeg),
        None => match bokai::formats::pdf::render::render_pdf_page_jpeg(
            &doc.bytes,
            0,
            bokai::formats::pdf::render::COVER_TARGET_WIDTH_PX,
            bokai::formats::pdf::render::COVER_JPEG_QUALITY,
        ) {
            Ok(jpeg) => Some(jpeg),
            Err(e) => {
                eprintln!("[sidle/convert] book {} pdf cover: skipped ({e})", book.id);
                None
            }
        },
    };

    // Selectable text layer, optional like the cover: a failure converts the
    // book visual-only. The slowest step, and the widest progress band.
    on_progress("text", 0, 1, "Extracting text");
    let text = bokai::formats::pdf::render::extract_pdf_text(&doc.bytes).ok();
    match &text {
        Some(pages) => {
            let runs: usize = pages.iter().map(|p| p.runs.len()).sum();
            eprintln!(
                "[sidle/convert] book {} pdf text: {runs} runs / {} pages",
                book.id,
                pages.len()
            );
        }
        None => eprintln!("[sidle/convert] book {} pdf text: skipped", book.id),
    }

    on_progress("build", 0, 1, "Building KFX");
    let kfx = bokai::export::pdf_to_kfx(&doc, &meta, cover_jpeg.as_deref(), text.as_deref());
    write_bytes_atomic(&out_path, &kfx)?;

    // The ink-anchor geometry cache (eid→page map + page boxes), keyed by the
    // `kfx_sha256` the row carries and computed from the in-memory bytes. An
    // absent sidecar leaves ink sync on its lazy parse.
    on_progress("geom", 0, 1, "Caching geometry");
    let kfx_sha = sha256_of_bytes(&kfx);
    let geom = pdf_geom::compute(&kfx);
    if let Err(e) = pdf_geom::write_sidecar(paths, &book.sha256, &kfx_sha, &geom) {
        eprintln!(
            "[sidle/convert] book {} pdf geom cache: skipped ({e:#})",
            book.id
        );
    } else {
        eprintln!(
            "[sidle/convert] book {} pdf geom cache: {} pages",
            book.id,
            geom.len()
        );
    }

    // The content_id baked into `out_path`, read back from it. The device names
    // its `.sdr` and `.notebooks/<id>!!PDOC!!` dirs after this value, and
    // `books.asin` equals it. `None` defers to the bootstrap backfill.
    let asin = bokai::Book::open(&out_path)
        .ok()
        .and_then(|b| b.metadata().asin.clone());

    // Sidecar for the library tile. An existing one is kept verbatim, png or
    // webp included, alongside the KFX's JPEG-normalized copy. A fresh
    // `cover.jpg` is written for a page-1 render alone.
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
                    eprintln!(
                        "[sidle/convert] book {} pdf cover write failed: {e}",
                        book.id
                    );
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

/// KFX → PDF: the embedded PDF of a PDF-backed container KFX, byte-identical to
/// what was embedded.
fn convert_kfx_to_pdf(
    paths: &LibraryPaths,
    book: &BookRow,
    on_progress: OnProgress<'_>,
) -> anyhow::Result<Produced> {
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
    on_progress("extract", 0, 1, "Extracting PDF");
    let pdf = bokai::formats::kfx::pdf_container::kfx_extract_pdf(&kfx_bytes)
        .map_err(|e| anyhow::anyhow!("kfx→pdf extract: {e}"))?;
    write_bytes_atomic(&out_path, &pdf)?;

    // Cover sidecar: the one on the row wins, and a row without one falls back
    // to the cover embedded in `kfx_bytes`. A PDF-backed container embeds it
    // the same way a reflowable one does. A coverless KFX yields no sidecar.
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

/// JPEG magic (`FF D8 FF`). `pdf_to_kfx` embeds its cover as a `format:jpg`
/// resource and reads JPEG dimensions off those bytes.
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= 3 && bytes[0..3] == [0xFF, 0xD8, 0xFF]
}

/// The [`bokai::Metadata`] a KFX export writes: a clone of `source` with the
/// fields `book` tracks overlaid onto it. Fields outside the row — identifier,
/// ASIN, cover_image, description — keep the `source` value.
pub fn book_metadata_override(source: &bokai::Metadata, book: &BookRow) -> bokai::Metadata {
    let mut m = source.clone();
    m.title = book.title.clone();
    m.authors = crate::library::authors::split_display(&book.author);
    if !book.language.trim().is_empty() {
        m.language = book.language.clone();
    }
    // `None` leaves the source's own `page_progression_direction`.
    if let Some(ppd) = &book.ppd {
        m.page_progression_direction = Some(ppd.clone());
    }
    // The KFX exporter takes its vertical/horizontal text axis from
    // `primary_writing_mode`, and the page-turn from `ppd` above. `None` leaves
    // the source's own writing mode.
    if let Some(wm) = &book.writing_mode {
        m.primary_writing_mode = Some(wm.clone());
    }
    m.publisher = book.publisher.clone();
    m.date = book.published_at.clone();
    m.collection = book
        .series_name
        .clone()
        .map(|name| bokai::model::CollectionInfo {
            name,
            collection_type: Some("series".to_string()),
            position: book.series_index,
        });
    m
}

/// The import-time basename: the stem of `source`, which import wrote as
/// `[Author] Title (Year)`. An empty stem is re-derived from `book`.
fn derived_basename(book: &BookRow, source: &Path) -> String {
    if let Some(stem) = source.file_stem().and_then(|s| s.to_str())
        && !stem.is_empty()
    {
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
            title_romaji: String::new(),
            author_romaji: String::new(),
            search_key: String::new(),
            language: "en".into(),
            ppd: None,
            writing_mode: None,
            epub_path: None,
            cover_path: None,
            cover_thumb_path: None,
            cover_rev: 0,
            kfx_path: None,
            kfx_sha256: None,
            pdf_path: None,
            file_size: 0,
            imported_at: "2026-01-01".into(),
            status: "done".into(),
            error: None,
            kind: Some("epub_to_kfx".into()),
            asin: None,
            amazon_asin: None,
            publisher: None,
            published_at: None,
            series_name: None,
            series_index: None,
            tags: vec![],
            updated_at: "2026-01-01".into(),
        }
    }

    #[test]
    fn override_applies_db_fields_but_preserves_untracked() {
        let mut src = bokai::Metadata {
            title: "Old Title".into(),
            authors: vec!["Old Author".into()],
            language: "ja".into(),
            ..Default::default()
        };
        // Fields outside the row, carried through the overlay.
        src.identifier = "urn:isbn:9999".into();
        src.asin = Some("B00REALASIN".into());
        src.cover_image = Some("OEBPS/cover.xhtml".into());

        let mut r = row("New Title", "Ann Author & Bob Writer");
        r.publisher = Some("New Press".into());
        r.published_at = Some("2021".into());
        r.series_name = Some("Saga".into());
        r.series_index = Some(2.0);

        let m = book_metadata_override(&src, &r);

        // Tracked fields take the row's value.
        assert_eq!(m.title, "New Title");
        assert_eq!(
            m.authors,
            vec!["Ann Author".to_string(), "Bob Writer".to_string()]
        );
        assert_eq!(m.language, "en");
        assert_eq!(m.publisher.as_deref(), Some("New Press"));
        assert_eq!(m.date.as_deref(), Some("2021"));
        assert_eq!(m.collection.as_ref().map(|c| c.name.as_str()), Some("Saga"));
        assert_eq!(m.collection.as_ref().and_then(|c| c.position), Some(2.0));
        // Identity fields outside the row hold their `source` value.
        assert_eq!(m.identifier, "urn:isbn:9999");
        assert_eq!(m.asin.as_deref(), Some("B00REALASIN"));
        assert_eq!(m.cover_image.as_deref(), Some("OEBPS/cover.xhtml"));
    }

    #[test]
    fn override_keeps_source_language_when_db_blank() {
        let src = bokai::Metadata {
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
