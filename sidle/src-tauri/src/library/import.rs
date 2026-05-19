//! Book import pipeline.
//!
//! Two source formats are accepted:
//!
//! - **EPUB**: original P1 path. Copy as the canonical source, queue EPUB→KFX.
//! - **KFX / KFX-zip**: P2b reverse path. The chain is `.kfx-zip → .kfx → .epub`:
//!   `.kfx-zip` is first merged to a single-container `.kfx` (via boko's
//!   `kfx::merge::merge_kfx_zip`) and persisted to disk; the same in-memory
//!   KFX bytes are then handed to boko's `kfx_to_epub::convert_to_epub` to
//!   produce the EPUB without re-reading the file. The cover image is pulled
//!   from that same in-memory KFX (it lives in the KFX's resource entities,
//!   not the EPUB output), so no second `Book::open` is needed. `.kfx` inputs
//!   skip the merge step but follow the same shape. No EPUB→KFX queue work
//!   runs — we already have the KFX.
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
    match SourceKind::detect(src) {
        SourceKind::Epub => import_epub(conn, paths, src),
        SourceKind::Kfx | SourceKind::KfxZip => import_kfx(conn, paths, src),
        SourceKind::Unknown => bail!(
            "unsupported file type: {} (expected .epub, .kfx, or .kfx-zip)",
            src.display()
        ),
    }
}

#[derive(Copy, Clone, Debug)]
enum SourceKind {
    Epub,
    Kfx,
    KfxZip,
    Unknown,
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
}

// ---------------------------------------------------------------------------
// EPUB path
// ---------------------------------------------------------------------------

fn import_epub(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    src: &Path,
) -> Result<ImportOutcome> {
    let bytes = fs::read(src).with_context(|| format!("read {}", src.display()))?;
    let file_size = bytes.len() as i64;
    let sha = sha256_bytes(&bytes);

    if let Some(existing) = db::find_by_sha(conn, &sha)? {
        return Ok(ImportOutcome::Duplicate(existing));
    }

    let meta = read_book_metadata(src).with_context(|| format!("parse {}", src.display()))?;
    let basename = format_basename(&meta.authors, &meta.title, meta.date.as_deref());

    paths.ensure_sha(&sha)?;
    let dest_epub = paths.book_dir(&sha).join(format!("{basename}.epub"));
    if !dest_epub.exists() {
        fs::write(&dest_epub, &bytes)
            .with_context(|| format!("write {}", dest_epub.display()))?;
    }

    let cover_path =
        write_cover_from_source(paths, &sha, src, meta.cover_image.as_deref()).unwrap_or(None);

    let kfx_path = paths.book_dir(&sha).join(format!("{basename}.kfx"));
    let kfx_ready = kfx_path.exists();

    let book_id = insert_row(
        conn,
        &sha,
        &meta,
        file_size,
        &dest_epub,
        cover_path.as_deref(),
        kfx_ready.then_some(kfx_path.as_path()),
    )?;

    if kfx_ready {
        db::upsert_job(conn, book_id, "done", None)?;
    } else {
        db::upsert_job(conn, book_id, "pending", None)?;
    }

    let row = db::get_book(conn, book_id)?.expect("just inserted");
    Ok(ImportOutcome::Imported {
        book: row,
        reused_kfx: kfx_ready,
    })
}

// ---------------------------------------------------------------------------
// KFX / KFX-zip path
// ---------------------------------------------------------------------------

fn import_kfx(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    src: &Path,
) -> Result<ImportOutcome> {
    let kind = SourceKind::detect(src);

    // Hash the original input bytes (kfx or kfx-zip) — that's our content key,
    // so re-pulling the same `.kfx-zip` from a different Mac dedupes correctly.
    let sha = sha256_of_file(src).with_context(|| format!("hash {}", src.display()))?;
    let file_size = fs::metadata(src)
        .with_context(|| format!("stat {}", src.display()))?
        .len() as i64;

    if let Some(existing) = db::find_by_sha(conn, &sha)? {
        return Ok(ImportOutcome::Duplicate(existing));
    }

    // Step 1: `.kfx-zip` → single-container `.kfx` bytes. We call boko's
    // explicit merge function instead of `Book::open(.kfx-zip)`, because the
    // latter would do the merge in-memory inside a Book handle — there must
    // be no path anywhere that converts `.kfx-zip` directly to EPUB. The
    // intermediate `.kfx` is a real file on disk; everything downstream
    // (metadata, EPUB synthesis, cover) reads from that file.
    let kfx_bytes: Vec<u8> = match kind {
        SourceKind::KfxZip => boko::kfx::merge::merge_kfx_zip(src)
            .with_context(|| format!("merge kfx-zip {}", src.display()))?,
        SourceKind::Kfx => fs::read(src)
            .with_context(|| format!("read {}", src.display()))?,
        _ => unreachable!("import_kfx called with non-KFX source"),
    };

    // Read metadata AND pull the cover image out of the same in-memory KFX —
    // we already have all the bytes, so a second `Book::open` later would be
    // pure overhead. The cover ref lives in `Metadata.cover_image`; its bytes
    // live in a resource entity inside the KFX itself.
    let (meta, cover_data): (BookMeta, Option<(Vec<u8>, &'static str)>) = {
        let mut book = boko::Book::from_bytes(&kfx_bytes, boko::Format::Kfx)
            .with_context(|| "read kfx metadata")?;
        let meta = extract_meta(book.metadata(), src.file_stem().and_then(|s| s.to_str()));
        let cover = meta
            .cover_image
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|cref| {
                let bytes = book.load_asset(&PathBuf::from(cref)).ok()?;
                Some((bytes, cover_ext_from(cref)))
            });
        (meta, cover)
    };
    let basename = format_basename(&meta.authors, &meta.title, meta.date.as_deref());

    paths.ensure_sha(&sha)?;
    let dest_kfx = paths.book_dir(&sha).join(format!("{basename}.kfx"));
    let dest_epub = paths.book_dir(&sha).join(format!("{basename}.epub"));

    // Persist the intermediate `.kfx`.
    write_bytes_atomic(&dest_kfx, &kfx_bytes)?;

    // Step 2: KFX bytes → EPUB bytes via boko's mechanical port. The bytes
    // are byte-identical to what we just wrote to `dest_kfx`, so this matches
    // a re-open without the disk round-trip.
    if !dest_epub.exists() {
        synthesize_epub_from_kfx(&kfx_bytes, &dest_epub)
            .with_context(|| format!("export EPUB {}", dest_epub.display()))?;
    }

    let cover_path = cover_data.and_then(|(bytes, ext)| {
        let out = paths.cover(&sha, ext);
        fs::write(&out, &bytes).ok().map(|_| out)
    });

    let book_id = insert_row(
        conn,
        &sha,
        &meta,
        file_size,
        &dest_epub,
        cover_path.as_deref(),
        Some(&dest_kfx),
    )?;
    // KFX already on disk — nothing for the queue to do.
    db::upsert_job(conn, book_id, "done", None)?;

    let row = db::get_book(conn, book_id)?.expect("just inserted");
    Ok(ImportOutcome::Imported {
        book: row,
        reused_kfx: true,
    })
}

/// Convert single-container KFX bytes to an EPUB file via boko's mechanical
/// port (`kfx_to_epub::convert_to_epub`). Caller is responsible for ensuring
/// the bytes are a single-container `.kfx` — `.kfx-zip` must be merged first.
fn synthesize_epub_from_kfx(kfx_bytes: &[u8], dest_epub: &Path) -> Result<()> {
    let epub_bytes = boko::kfx_to_epub::convert_to_epub(kfx_bytes)
        .map_err(|e| anyhow::anyhow!("boko kfx→epub: {e}"))?;
    write_bytes_atomic(dest_epub, &epub_bytes)
}

fn write_bytes_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
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

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub enum ImportOutcome {
    Imported { book: BookRow, reused_kfx: bool },
    Duplicate(BookRow),
}

struct BookMeta {
    title: String,
    authors: Vec<String>,
    language: String,
    ppd: Option<String>,
    cover_image: Option<String>,
    date: Option<String>,
}

fn read_book_metadata(path: &Path) -> Result<BookMeta> {
    let book = boko::Book::open(path).with_context(|| "open with boko")?;
    Ok(extract_meta(
        book.metadata(),
        path.file_stem().and_then(|s| s.to_str()),
    ))
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
        cover_image: m.cover_image.clone(),
        date: m.date.clone(),
    }
}

/// Pull the cover image out of the source file and copy it into
/// `books/<sha>/cover.<ext>`. Works for any format boko can open.
fn write_cover_from_source(
    paths: &LibraryPaths,
    sha: &str,
    src: &Path,
    cover_ref: Option<&str>,
) -> Result<Option<PathBuf>> {
    let Some(cover_ref) = cover_ref else { return Ok(None) };
    if cover_ref.is_empty() {
        return Ok(None);
    }

    let mut book = boko::Book::open(src).with_context(|| "reopen for cover")?;
    let asset_path = PathBuf::from(cover_ref);

    let bytes = match book.load_asset(&asset_path) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };

    let ext = cover_ext_from(cover_ref);
    let out = paths.cover(sha, ext);
    fs::write(&out, bytes).with_context(|| format!("write cover {}", out.display()))?;
    Ok(Some(out))
}

fn insert_row(
    conn: &rusqlite::Connection,
    sha: &str,
    meta: &BookMeta,
    file_size: i64,
    dest_epub: &Path,
    cover_path: Option<&Path>,
    kfx_path: Option<&Path>,
) -> Result<i64> {
    let dest_epub_str = dest_epub.to_string_lossy().to_string();
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
            source_epub_path: &dest_epub_str,
            cover_path: cover_path_str.as_deref(),
            kfx_path: kfx_path_str.as_deref(),
            file_size,
            imported_at: &now,
        },
    )?;
    Ok(id)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
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

