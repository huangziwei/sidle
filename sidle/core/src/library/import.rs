//! Book import pipeline.
//!
//! One symmetric flow for both supported input formats — EPUB and KFX (the
//! latter possibly arriving as the multi-container `.kfx-zip` bundle Kindle
//! DeDRM produces). Whichever format comes in, the other side is filled in
//! later by the background queue:
//!
//!  - EPUB → library; pending `epub_to_kfx` job.
//!  - KFX  → library (merging `.kfx-zip` first if needed); pending a
//!    `kfx_to_epub` job.
//!
//! Three formats are converted to EPUB at import time and from there look
//! identical to a regular EPUB drop:
//!
//!  - `.azw3` → boko-kai's AZW3 importer + EPUB exporter.
//!  - `.mobi` → boko-kai's MOBI importer + EPUB exporter.
//!  - `.zip`  → Aozora Bunko sniff + parse → cover → build_epub. See
//!    `convert_aozora_zip` for the sniff details.
//!
//! Steps, identical for all inputs:
//!
//!  1. Stream-hash the source file and check the dedupe index.
//!  2. Normalize to canonical bytes (`.kfx-zip` → merged `.kfx`;
//!     `.azw3`/`.mobi`/`.zip` → freshly built EPUB; other inputs are
//!     loaded verbatim).
//!  3. Read metadata from those bytes.
//!  4. Persist the canonical file into `books/<sha>/<basename>.<ext>`.
//!  5. Extract the cover sidecar if we already have a readable EPUB on
//!     hand. EPUB input always does; KFX input only does on an idempotent
//!     re-import where the EPUB already exists. The fresh KFX path leaves
//!     `cover_path` empty for the worker to fill once `convert_to_epub`
//!     produces an EPUB whose JXR cover has been transcoded to JPG.
//!  6. Insert book + conversion job. If the *other* side is already on
//!     disk (idempotent re-import) the job is marked `done`; otherwise
//!     it's `pending` and the caller enqueues it.
//!
//! Each step touches disk; callers should run this inside `spawn_blocking`.

use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::library::db::{self, BookRow, NewBook};
use crate::library::paths::{LibraryPaths, cover_ext_from, format_basename};

// ---------------------------------------------------------------------------
// Aozora Bunko .zip → EPUB
//
// Wires boko-kai's aozora pipeline into the import flow. Aozora source
// archives are unstructured .zip files: a Shift_JIS text file with
// ［＃markers］ + 底本 colophon, plus accompanying image files. We open
// the zip, sniff for those markers, parse → Document, render a
// programmatic cover JPEG (resvg), and build the EPUB. From there
// `import_one` continues with Canonical::Epub bytes, so the import is
// indistinguishable from a regular EPUB drop.
//
// "Secret feature" per user request: no UI affordance, no help text. A
// non-aozora .zip falls out as an "import failed" toast like any other
// bad input.
// ---------------------------------------------------------------------------

pub fn import_file(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    src: &Path,
) -> Result<ImportOutcome> {
    let src_kind = SourceKind::detect(src);
    if matches!(src_kind, SourceKind::Unknown) {
        bail!(
            "unsupported file type: {} (expected .epub, .kfx, .kfx-zip, .azw3, or .mobi)",
            src.display()
        );
    }
    import_one(conn, paths, src, src_kind)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SourceKind {
    Epub,
    Kfx,
    KfxZip,
    /// `.azw3` — Kindle AZW3 (decrypted). Converted to EPUB at import time
    /// via boko-kai's AZW3 importer + EPUB exporter, so downstream
    /// everything looks like a regular EPUB drop. Symmetric with the
    /// aozora `.zip` path, except the format is detected purely by
    /// extension (no content sniff).
    Azw3,
    /// `.mobi` — older Mobipocket/KF8 hybrid (decrypted). Same shape as
    /// AZW3: boko-kai's MOBI importer + EPUB exporter, detected purely by
    /// extension. Japanese MOBIs carry vertical-writing-mode and PPD in
    /// EXTH 525/527, which the boko importer propagates into the EPUB.
    Mobi,
    /// `.zip` extension — tentatively an Aozora Bunko archive. The actual
    /// aozora sniff (底本/［＃ markers) happens in `convert_aozora_zip`
    /// during canonical-bytes extraction; a non-aozora zip fails out
    /// there.
    AozoraZip,
    Unknown,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Canonical {
    Epub,
    Kfx,
}

impl SourceKind {
    fn detect(p: &Path) -> Self {
        match p
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("epub") => Self::Epub,
            Some("kfx") => Self::Kfx,
            Some("kfx-zip") => Self::KfxZip,
            Some("azw3") => Self::Azw3,
            Some("mobi") => Self::Mobi,
            Some("zip") => Self::AozoraZip,
            _ => Self::Unknown,
        }
    }

    fn canonical(self) -> Canonical {
        match self {
            Self::Epub | Self::AozoraZip | Self::Azw3 | Self::Mobi => Canonical::Epub,
            Self::Kfx | Self::KfxZip => Canonical::Kfx,
            Self::Unknown => unreachable!("filtered by import_file"),
        }
    }
}

impl Canonical {
    fn boko_format(self) -> boko::Format {
        match self {
            Self::Epub => boko::Format::Epub,
            Self::Kfx => boko::Format::Kfx,
        }
    }

    /// Direction the queue needs to run to fill the *other* side of a row
    /// imported in this canonical format.
    fn job_kind_for_other_side(self) -> &'static str {
        match self {
            Self::Epub => "epub_to_kfx",
            Self::Kfx => "kfx_to_epub",
        }
    }
}

fn import_one(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    src: &Path,
    src_kind: SourceKind,
) -> Result<ImportOutcome> {
    let canonical = src_kind.canonical();

    // 1. Hash + dedupe before doing expensive normalization (a `.kfx-zip`
    //    merge can be tens of ms; stream-hashing the source is microseconds).
    let sha = sha256_of_file(src).with_context(|| format!("hash {}", src.display()))?;
    let file_size = fs::metadata(src)
        .with_context(|| format!("stat {}", src.display()))?
        .len() as i64;

    if let Some(existing) = db::find_by_sha(conn, &sha)? {
        return Ok(ImportOutcome::Duplicate(existing));
    }

    // 2. Normalize → canonical bytes.
    let canonical_bytes: Vec<u8> = match src_kind {
        SourceKind::Epub | SourceKind::Kfx => {
            fs::read(src).with_context(|| format!("read {}", src.display()))?
        }
        SourceKind::KfxZip => boko::kfx::merge::merge_kfx_zip(src)
            .with_context(|| format!("merge kfx-zip {}", src.display()))?,
        SourceKind::Azw3 => convert_azw3(src)
            .with_context(|| format!("azw3 {}", src.display()))?,
        SourceKind::Mobi => convert_mobi(src)
            .with_context(|| format!("mobi {}", src.display()))?,
        SourceKind::AozoraZip => convert_aozora_zip(src)
            .with_context(|| format!("aozora zip {}", src.display()))?,
        SourceKind::Unknown => unreachable!("filtered above"),
    };

    // 3. Metadata from the canonical bytes (no file re-open).
    let meta = {
        let book = boko::Book::from_bytes(&canonical_bytes, canonical.boko_format())
            .with_context(|| format!("read metadata from {}", src.display()))?;
        extract_meta(book.metadata(), src.file_stem().and_then(|s| s.to_str()))
    };
    let basename = format_basename(&meta.authors, &meta.title, meta.date.as_deref());

    // 4. Persist canonical to the library slot.
    paths.ensure_sha(&sha)?;
    let dest_epub = paths.book_dir(&sha).join(format!("{basename}.epub"));
    let dest_kfx = paths.book_dir(&sha).join(format!("{basename}.kfx"));
    let own_dest: &Path = match canonical {
        Canonical::Epub => &dest_epub,
        Canonical::Kfx => &dest_kfx,
    };
    let other_dest: &Path = match canonical {
        Canonical::Epub => &dest_kfx,
        Canonical::Kfx => &dest_epub,
    };
    if !own_dest.exists() {
        write_bytes_atomic(own_dest, &canonical_bytes)?;
    }

    // 5. Cover sidecar — only when we have EPUB bytes on hand. KFX input
    //    without a pre-existing EPUB defers to the worker.
    let cover_path: Option<PathBuf> = match canonical {
        Canonical::Epub => write_cover_from_epub_bytes(paths, &sha, &canonical_bytes),
        Canonical::Kfx if other_dest.exists() => fs::read(other_dest)
            .ok()
            .and_then(|b| write_cover_from_epub_bytes(paths, &sha, &b)),
        Canonical::Kfx => None,
    };

    // 6. Insert book row + job.
    let other_ready = other_dest.exists();
    // When this import lands a KFX (either as canonical or because the
    // other side was already on disk from a prior import), record the
    // hash of *its* bytes. That hash drives the on-device filename
    // infix; without it, a re-import of the same file off the Kindle
    // can't be linked back to the local row.
    let kfx_bytes_sha: Option<String> = match canonical {
        Canonical::Kfx => Some(sha256_of_bytes(&canonical_bytes)),
        Canonical::Epub if other_ready => match fs::read(&dest_kfx) {
            Ok(bytes) => Some(sha256_of_bytes(&bytes)),
            Err(_) => None,
        },
        _ => None,
    };
    let book_id = insert_row(
        conn,
        &sha,
        &meta,
        file_size,
        &Persisted {
            epub_path: match canonical {
                Canonical::Epub => Some(dest_epub.as_path()),
                Canonical::Kfx => other_ready.then_some(dest_epub.as_path()),
            },
            cover_path: cover_path.as_deref(),
            kfx_path: match canonical {
                Canonical::Kfx => Some(dest_kfx.as_path()),
                Canonical::Epub => other_ready.then_some(dest_kfx.as_path()),
            },
            kfx_sha256: kfx_bytes_sha.as_deref(),
        },
    )?;

    let job_status = if other_ready { "done" } else { "pending" };
    db::insert_job(
        conn,
        book_id,
        job_status,
        canonical.job_kind_for_other_side(),
    )?;

    let row = db::get_book(conn, book_id)?.expect("just inserted");
    Ok(ImportOutcome::Imported {
        book: row,
        needs_enqueue: !other_ready,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Open an in-memory EPUB, read its cover-image asset, and persist it as the
/// `cover.<ext>` sidecar. Returns the path written, or `None` if the EPUB
/// has no cover or the asset can't be loaded.
fn write_cover_from_epub_bytes(
    paths: &LibraryPaths,
    sha: &str,
    epub_bytes: &[u8],
) -> Option<PathBuf> {
    let (bytes, ext) = extract_cover_from_epub(epub_bytes)?;
    let out = paths.cover(sha, ext);
    fs::write(&out, &bytes).ok()?;
    // Derive the picker thumbnail now, at import. Best-effort: a thumbnail
    // failure must not fail the import — the full-res cover still works and the
    // server falls back to it (see library::thumbnail).
    let _ = super::thumbnail::ensure_thumbnail(paths, sha, &out);
    Some(out)
}

/// Pull the cover bytes (and extension) out of an in-memory EPUB. Used both
/// for direct EPUB imports and by the worker after `kfx_to_epub` produces an
/// EPUB whose JXR cover has been transcoded to JPG.
pub fn extract_cover_from_epub(epub_bytes: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    let mut book = boko::Book::from_bytes(epub_bytes, boko::Format::Epub).ok()?;
    let cref = book.metadata().cover_image.as_deref()?.to_string();
    if cref.is_empty() {
        return None;
    }
    let bytes = book.load_asset(&PathBuf::from(&cref)).ok()?;
    Some((bytes, cover_ext_from(&cref)))
}

pub fn write_bytes_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = dest.with_file_name(format!(
        "{}.partial",
        dest.file_name()
            .expect("dest must include a filename")
            .to_string_lossy()
    ));
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, dest)
        .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
    Ok(())
}

pub enum ImportOutcome {
    Imported { book: BookRow, needs_enqueue: bool },
    Duplicate(BookRow),
}

struct BookMeta {
    title: String,
    authors: Vec<String>,
    language: String,
    ppd: Option<String>,
    date: Option<String>,
    /// Amazon catalogue id. Comes from boko's dedicated `Metadata.asin` field
    /// (populated from KFX `kindle_title_metadata.ASIN` and from EPUB
    /// `<dc:identifier opf:scheme="ASIN">`). Distinct from `boko::Metadata`'s
    /// generic `identifier`, which for KFX is the per-device internal
    /// `book_id` UUID — not the ASIN.
    asin: Option<String>,
    /// EPUB `<dc:publisher>` or KFX `publisher` field (symbol 232). Optional;
    /// many self-pub and indie books leave it blank.
    publisher: Option<String>,
}

fn extract_meta(m: &boko::Metadata, fallback_stem: Option<&str>) -> BookMeta {
    let title = if m.title.trim().is_empty() {
        fallback_stem
            .map(str::to_string)
            .unwrap_or_else(|| "Untitled".to_string())
    } else {
        m.title.clone()
    };
    BookMeta {
        title,
        authors: m.authors.clone(),
        language: m.language.clone(),
        ppd: m.page_progression_direction.clone(),
        date: m.date.clone(),
        asin: m.asin.clone().filter(|s| !s.is_empty()),
        publisher: m.publisher.clone().filter(|s| !s.is_empty()),
    }
}

/// The on-disk artifacts an import produced (or found already present): the
/// paths and KFX hash to persist on the book row. Grouped into one struct so
/// `insert_row` stays under clippy's argument-count lint.
struct Persisted<'a> {
    epub_path: Option<&'a Path>,
    cover_path: Option<&'a Path>,
    kfx_path: Option<&'a Path>,
    kfx_sha256: Option<&'a str>,
}

fn insert_row(
    conn: &rusqlite::Connection,
    sha: &str,
    meta: &BookMeta,
    file_size: i64,
    files: &Persisted<'_>,
) -> Result<i64> {
    let epub_path_str = files.epub_path.map(|p| p.to_string_lossy().to_string());
    let cover_path_str = files.cover_path.map(|p| p.to_string_lossy().to_string());
    let kfx_path_str = files.kfx_path.map(|p| p.to_string_lossy().to_string());
    let authors_joined = meta.authors.join(", ");
    let now = db::now_iso();
    let id = db::insert_book(
        conn,
        &NewBook {
            sha256: sha,
            title: &meta.title,
            author: &authors_joined,
            language: &meta.language,
            ppd: meta.ppd.as_deref(),
            epub_path: epub_path_str.as_deref(),
            cover_path: cover_path_str.as_deref(),
            kfx_path: kfx_path_str.as_deref(),
            kfx_sha256: files.kfx_sha256,
            file_size,
            imported_at: &now,
            asin: meta.asin.as_deref(),
            publisher: meta.publisher.as_deref(),
            // meta.date comes from boko's EPUB `<dc:date>` / KFX equivalent.
            // Stored verbatim — typically `2024-03-15` or `2024`. We filter
            // empties so a missing OPF date doesn't land as `Some("")`.
            published_at: meta
                .date
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            // Series and tags aren't populated from source format yet —
            // they're set via the metadata editor. Flagged as a follow-up
            // in `.claude/plans/library-navigation.md` (Phase 5+).
            series_name: None,
            series_index: None,
            tags: &[],
        },
    )?;
    Ok(id)
}

pub fn sha256_of_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn sha256_of_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Convert a decrypted `.azw3` to EPUB bytes via boko-kai's AZW3 importer +
/// EPUB exporter. Caller has already extension-detected `.azw3`; if the
/// file isn't a real AZW3 (bad PalmDOC header, etc.), boko's `Book::from_bytes`
/// returns the error and the caller's `?` surfaces it as a normal import
/// failure.
///
/// Gated on the standalone EPUB-3 validator for the same reason as the
/// aozora path: the bytes we're about to persist were freshly synthesized
/// rather than passed through, so we'd rather fail import than write a
/// broken book and then fail the downstream `epub_to_kfx` job.
fn convert_azw3(src: &Path) -> Result<Vec<u8>> {
    use boko::Exporter as _;
    let azw3_bytes = fs::read(src).with_context(|| format!("read {}", src.display()))?;
    let mut book = boko::Book::from_bytes(&azw3_bytes, boko::Format::Azw3)
        .with_context(|| format!("parse azw3 {}", src.display()))?;
    let mut buf = Cursor::new(Vec::<u8>::new());
    boko::EpubExporter::new()
        .export(&mut book, &mut buf)
        .context("azw3 -> epub export")?;
    let epub_bytes = buf.into_inner();
    let report = boko::validate::epub3::validate(&epub_bytes);
    if !report.is_clean() {
        bail!("azw3 -> epub failed validation:\n{report}");
    }
    Ok(epub_bytes)
}

/// Convert a decrypted `.mobi` to EPUB bytes via boko-kai's MOBI importer +
/// EPUB exporter. Shape matches `convert_azw3` — the EXTH parsing and
/// EPUB export are shared infrastructure; only the input format tag and
/// boko's per-importer wiring differ.
fn convert_mobi(src: &Path) -> Result<Vec<u8>> {
    use boko::Exporter as _;
    let mobi_bytes = fs::read(src).with_context(|| format!("read {}", src.display()))?;
    let mut book = boko::Book::from_bytes(&mobi_bytes, boko::Format::Mobi)
        .with_context(|| format!("parse mobi {}", src.display()))?;
    let mut buf = Cursor::new(Vec::<u8>::new());
    boko::EpubExporter::new()
        .export(&mut book, &mut buf)
        .context("mobi -> epub export")?;
    let epub_bytes = buf.into_inner();
    let report = boko::validate::epub3::validate(&epub_bytes);
    if !report.is_clean() {
        bail!("mobi -> epub failed validation:\n{report}");
    }
    Ok(epub_bytes)
}

/// Open an Aozora Bunko `.zip`, sniff for the markers, run the
/// parse → cover → build_epub pipeline, return EPUB bytes. Errors out
/// (via `bail!`) for any zip that doesn't look like aozora — the caller's
/// `?` then surfaces this as a normal import failure with no special UI.
///
/// Pipeline mirrors `aozora_dispatch` in `boko-kai/src/main.rs:1602` so
/// the CLI and the GUI produce byte-identical EPUBs from the same input.
fn convert_aozora_zip(src: &Path) -> Result<Vec<u8>> {
    let file =
        fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("not a valid zip archive")?;

    let mut txt_buf: Option<Vec<u8>> = None;
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("zip entry {i}"))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let lower = name.to_ascii_lowercase();
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf).context("read zip entry")?;
        if lower.ends_with(".txt") {
            txt_buf = Some(buf);
        } else if lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".gif")
        {
            let basename = Path::new(&name)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or(name);
            images.push((basename, buf));
        }
    }

    let Some(txt) = txt_buf else {
        bail!("zip contains no .txt entry");
    };
    let text = boko::aozora::parser_txt::decode_bytes(&txt);

    // Aozora marker sniff. Bare zips with no aozora content fail here.
    if !text.contains("底本") && !text.contains("［＃") {
        bail!("not an Aozora Bunko archive");
    }

    let doc = boko::aozora::parse_txt(&text);
    let cover = boko::aozora::render_cover_jpeg(&doc.title, &doc.author)
        .context("aozora cover render")?;
    let epub_bytes = boko::aozora::build_epub(boko::aozora::EpubInput {
        document: &doc,
        images: &images,
        cover_jpeg: &cover,
    })
    .context("aozora build_epub")?;
    // Gate the import on the standalone EPUB-3 validator. Bad output here
    // silently corrupts the downstream KFX conversion, so we fail fast
    // instead of writing a broken book into the library.
    let report = boko::validate::epub3::validate(&epub_bytes);
    if !report.is_clean() {
        bail!("aozora epub failed validation:\n{report}");
    }
    Ok(epub_bytes)
}
