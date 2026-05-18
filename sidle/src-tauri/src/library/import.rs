//! EPUB import pipeline.
//!
//! Pipeline (per file):
//! 1. sha256 the bytes
//! 2. extract metadata (title/authors/language/ppd/date/cover) with boko
//! 3. derive a filesystem-safe basename `[Author] Title (Year)`
//! 4. copy EPUB to `books/<sha>/<basename>.epub`
//! 5. save cover (if any) to `books/<sha>/cover.<ext>`
//! 6. INSERT book row (UNIQUE(sha256) — skip if already imported)
//! 7. decide job status:
//!    - if `books/<sha>/<basename>.kfx` already exists → `done` (KFX reused)
//!    - otherwise → `pending` (worker will pick it up)
//!
//! Each step touches disk; callers should run this inside `spawn_blocking`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::library::db::{self, BookRow, NewBook};
use crate::library::paths::{LibraryPaths, cover_ext_from, format_basename};

pub fn import_file(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    src: &Path,
) -> Result<ImportOutcome> {
    let bytes = fs::read(src).with_context(|| format!("read {}", src.display()))?;
    let file_size = bytes.len() as i64;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sha = format!("{:x}", hasher.finalize());

    if let Some(existing) = db::find_by_sha(conn, &sha)? {
        return Ok(ImportOutcome::Duplicate(existing));
    }

    // Parse metadata directly from the source EPUB before we copy, so we can
    // name the destination file with title/author/year.
    let meta = read_epub_metadata(src)
        .with_context(|| format!("parse {}", src.display()))?;
    let basename = format_basename(&meta.authors, &meta.title, meta.date.as_deref());

    paths.ensure_sha(&sha)?;
    let dest_epub = paths.book_dir(&sha).join(format!("{basename}.epub"));
    if !dest_epub.exists() {
        fs::write(&dest_epub, &bytes)
            .with_context(|| format!("write {}", dest_epub.display()))?;
    }

    let cover_path = write_cover(paths, &sha, &dest_epub, meta.cover_image.as_deref())
        .unwrap_or(None);

    let kfx_path = paths.book_dir(&sha).join(format!("{basename}.kfx"));
    let kfx_ready = kfx_path.exists();

    let dest_epub_str = dest_epub.to_string_lossy().to_string();
    let cover_path_str = cover_path.as_ref().map(|p| p.to_string_lossy().to_string());
    let kfx_path_str = if kfx_ready {
        Some(kfx_path.to_string_lossy().to_string())
    } else {
        None
    };
    let authors_joined = meta.authors.join(", ");
    let now = db::now_iso();
    let book_id = db::insert_book(
        conn,
        &NewBook {
            sha256: &sha,
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

pub enum ImportOutcome {
    Imported { book: BookRow, reused_kfx: bool },
    Duplicate(BookRow),
}

struct EpubMeta {
    title: String,
    authors: Vec<String>,
    language: String,
    ppd: Option<String>,
    cover_image: Option<String>,
    date: Option<String>,
}

fn read_epub_metadata(path: &Path) -> Result<EpubMeta> {
    let book = boko::Book::open(path).with_context(|| "open with boko")?;
    let m = book.metadata();
    let title = if m.title.trim().is_empty() {
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Untitled".to_string())
    } else {
        m.title.clone()
    };
    Ok(EpubMeta {
        title,
        authors: m.authors.clone(),
        language: m.language.clone(),
        ppd: m.page_progression_direction.clone(),
        cover_image: m.cover_image.clone(),
        date: m.date.clone(),
    })
}

/// Pull the cover image out of the EPUB and copy it into `books/<sha>/cover.<ext>`.
fn write_cover(
    paths: &LibraryPaths,
    sha: &str,
    epub_path: &Path,
    cover_ref: Option<&str>,
) -> Result<Option<PathBuf>> {
    let Some(cover_ref) = cover_ref else { return Ok(None) };
    if cover_ref.is_empty() {
        return Ok(None);
    }

    let mut book = boko::Book::open(epub_path).with_context(|| "reopen for cover")?;
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

#[allow(dead_code)]
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
