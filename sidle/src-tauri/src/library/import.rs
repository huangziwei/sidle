//! EPUB import pipeline.
//!
//! Pipeline (per file):
//! 1. sha256 the bytes
//! 2. copy to `epubs/<sha>/source.epub`
//! 3. parse with `boko::Book` to pull title/authors/language/ppd/cover
//! 4. extract cover image to `cache/<sha>/cover.<ext>` if present
//! 5. INSERT book row (UNIQUE(sha256) — skip if already imported)
//! 6. decide job status:
//!    - if `cache/<sha>/book.kfx` already exists → `done` (KFX reused)
//!    - otherwise → `pending` (worker will pick it up)
//!
//! Each step that touches disk runs on the calling thread; callers should run
//! this inside `spawn_blocking` so the Tauri event loop stays responsive.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::library::db::{self, BookRow, NewBook};
use crate::library::paths::{LibraryPaths, cover_ext_from};

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

    // Short-circuit: already in DB.
    if let Some(existing) = db::find_by_sha(conn, &sha)? {
        return Ok(ImportOutcome::Duplicate(existing));
    }

    paths.ensure_sha(&sha)?;
    let dest_epub = paths.source_epub(&sha);
    if !dest_epub.exists() {
        fs::write(&dest_epub, &bytes)
            .with_context(|| format!("write {}", dest_epub.display()))?;
    }

    let meta = read_epub_metadata(&dest_epub)
        .with_context(|| format!("parse {}", dest_epub.display()))?;

    let cover_path =
        write_cover(paths, &sha, &dest_epub, meta.cover_image.as_deref()).unwrap_or(None);

    let kfx_path = paths.kfx(&sha);
    let kfx_ready = kfx_path.exists();

    let dest_epub_str = dest_epub.to_string_lossy().to_string();
    let cover_path_str = cover_path.as_ref().map(|p| p.to_string_lossy().to_string());
    let kfx_path_str = if kfx_ready {
        Some(kfx_path.to_string_lossy().to_string())
    } else {
        None
    };
    let now = db::now_iso();
    let book_id = db::insert_book(
        conn,
        &NewBook {
            sha256: &sha,
            title: &meta.title,
            author: &meta.authors_joined,
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
    authors_joined: String,
    language: String,
    ppd: Option<String>,
    cover_image: Option<String>,
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
    let authors_joined = m.authors.join(", ");
    Ok(EpubMeta {
        title,
        authors_joined,
        language: m.language.clone(),
        ppd: m.page_progression_direction.clone(),
        cover_image: m.cover_image.clone(),
    })
}

/// Pull the cover image out of the EPUB and copy it into `cache/<sha>/cover.<ext>`.
/// Returns the saved path. Silently returns `None` if no cover is referenced.
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

    let mut book =
        boko::Book::open(epub_path).with_context(|| "reopen for cover")?;
    let asset_path = PathBuf::from(cover_ref);

    // `load_asset` expects a path inside the archive. The metadata's cover_image
    // is whatever the OPF spit out — usually it's already canonical.
    let bytes = match book.load_asset(&asset_path) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };

    let ext = cover_ext_from(cover_ref);
    let out = paths.cover(sha, ext);
    fs::write(&out, bytes).with_context(|| format!("write cover {}", out.display()))?;
    Ok(Some(out))
}

/// Quick existence check used by the frontend to verify a cover path.
#[allow(dead_code)]
pub fn cover_exists(p: &Path) -> bool {
    p.is_file() && fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
}

/// Used by callers that read bytes themselves (e.g., drag-drop bridge).
/// Kept here so the hashing implementation stays in one place.
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
