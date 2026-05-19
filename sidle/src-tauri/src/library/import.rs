//! Book import pipeline.
//!
//! One symmetric flow for both supported input formats — EPUB and KFX (the
//! latter possibly arriving as the multi-container `.kfx-zip` bundle Kindle
//! DeDRM produces). Whichever format comes in, the other side is filled in
//! later by the background queue:
//!
//!  - EPUB → library; pending `epub_to_kfx` job.
//!  - KFX  → library (merging `.kfx-zip` first if needed); pending
//!           `kfx_to_epub` job.
//!
//! Steps, identical for both inputs:
//!
//!  1. Stream-hash the source file and check the dedupe index.
//!  2. Normalize to canonical bytes (`.kfx-zip` → merged `.kfx`; other
//!     inputs are loaded verbatim).
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
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::library::db::{self, BookRow, NewBook};
use crate::library::paths::{LibraryPaths, cover_ext_from, format_basename};

pub fn import_file(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    src: &Path,
) -> Result<ImportOutcome> {
    let src_kind = SourceKind::detect(src);
    if matches!(src_kind, SourceKind::Unknown) {
        bail!(
            "unsupported file type: {} (expected .epub, .kfx, or .kfx-zip)",
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
            _ => Self::Unknown,
        }
    }

    fn canonical(self) -> Canonical {
        match self {
            Self::Epub => Canonical::Epub,
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
    let book_id = insert_row(
        conn,
        &sha,
        &meta,
        file_size,
        match canonical {
            Canonical::Epub => Some(dest_epub.as_path()),
            Canonical::Kfx => other_ready.then_some(dest_epub.as_path()),
        },
        cover_path.as_deref(),
        match canonical {
            Canonical::Kfx => Some(dest_kfx.as_path()),
            Canonical::Epub => other_ready.then_some(dest_kfx.as_path()),
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
    fs::write(&out, &bytes).ok().map(|_| out)
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
    }
}

fn insert_row(
    conn: &rusqlite::Connection,
    sha: &str,
    meta: &BookMeta,
    file_size: i64,
    epub_path: Option<&Path>,
    cover_path: Option<&Path>,
    kfx_path: Option<&Path>,
) -> Result<i64> {
    let epub_path_str = epub_path.map(|p| p.to_string_lossy().to_string());
    let cover_path_str = cover_path.map(|p| p.to_string_lossy().to_string());
    let kfx_path_str = kfx_path.map(|p| p.to_string_lossy().to_string());
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
            file_size,
            imported_at: &now,
            asin: meta.asin.as_deref(),
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
