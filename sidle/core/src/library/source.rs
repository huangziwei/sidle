//! The file a book's edits are made to, and how it is replaced safely.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::library::db::{self, BookRow};
use crate::library::import::sha256_of_file;

/// The editable source a book carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Kfx,
    Epub,
    Pdf,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Kfx => "kfx",
            Source::Epub => "epub",
            Source::Pdf => "pdf",
        }
    }
}

/// The format a book was imported *from*, read off its conversion `kind`
/// (`"<source>_to_<target>"`).
pub fn format_of(kind: Option<&str>) -> &str {
    kind.unwrap_or("epub_to_kfx")
        .split("_to_")
        .next()
        .unwrap_or("epub")
}

/// A book's editable source: its format and its path on disk. An error names
/// what is missing — an unrecognized source format, or a file not yet produced.
pub fn of(row: &BookRow) -> Result<(Source, String)> {
    match format_of(row.kind.as_deref()) {
        "kfx" => row
            .kfx_path
            .clone()
            .map(|p| (Source::Kfx, p))
            .context("this book has no KFX file yet"),
        "epub" => row
            .epub_path
            .clone()
            .map(|p| (Source::Epub, p))
            .context("this book has no EPUB file yet"),
        "pdf" => row
            .pdf_path
            .clone()
            .map(|p| (Source::Pdf, p))
            .context("this book has no PDF file yet"),
        other => anyhow::bail!(
            "edits are written to a KFX, EPUB or PDF source; this is a {other}-source book"
        ),
    }
}

/// Write `new_bytes` over a book's source file, keeping its device identity.
pub fn commit(
    conn: &Connection,
    book_id: i64,
    source: Source,
    path: &str,
    new_bytes: &[u8],
) -> Result<()> {
    replace_file(Path::new(path), new_bytes)?;
    if source == Source::Kfx {
        let sha = sha256_of_file(Path::new(path))?;
        db::set_kfx_path_and_sha(conn, book_id, path, &sha)?;
    }
    Ok(())
}

/// Replace a file's contents atomically, restoring the original if the swap
/// fails after the write.
pub fn replace_file(target: &Path, new_bytes: &[u8]) -> Result<()> {
    let backup = sibling(target, "editbak");
    let temp = sibling(target, "editing");

    // The backup is what makes a failed replace recoverable, so a failure here
    // aborts before the live file is touched at all.
    std::fs::copy(target, &backup).with_context(|| format!("back up {}", target.display()))?;
    if let Err(e) = std::fs::write(&temp, new_bytes) {
        let _ = std::fs::remove_file(&backup);
        return Err(e).with_context(|| format!("write {}", temp.display()));
    }
    if let Err(e) = std::fs::rename(&temp, target) {
        let _ = std::fs::remove_file(&temp);
        let _ = std::fs::copy(&backup, target); // best-effort restore
        let _ = std::fs::remove_file(&backup);
        return Err(e).with_context(|| format!("replace {}", target.display()));
    }
    let _ = std::fs::remove_file(&backup); // settled — tidy the backup
    Ok(())
}

/// `path` with an extra dot-suffix, e.g. `book.kfx` + `"editbak"` →
/// `book.kfx.editbak`. Kept in the same directory as `path` so the temp→target
/// rename is atomic (same filesystem).
pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_follows_the_conversion_direction() {
        assert_eq!(format_of(Some("kfx_to_epub")), "kfx");
        assert_eq!(format_of(Some("epub_to_kfx")), "epub");
        assert_eq!(format_of(Some("pdf_to_kfx")), "pdf");
        // A row with no direction recorded predates the column; EPUB-source is
        // what those books are.
        assert_eq!(format_of(None), "epub");
    }

    #[test]
    fn sibling_stays_in_the_same_directory() {
        let p = Path::new("/books/abc/book.kfx");
        assert_eq!(
            sibling(p, "editbak"),
            Path::new("/books/abc/book.kfx.editbak")
        );
        assert_eq!(
            sibling(p, "editing"),
            Path::new("/books/abc/book.kfx.editing")
        );
    }

    #[test]
    fn a_replace_leaves_no_temporaries_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("book.epub");
        std::fs::write(&target, b"old").unwrap();

        replace_file(&target, b"new").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let left: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, vec!["book.epub".to_string()]);
    }

    #[test]
    fn a_missing_source_is_refused_before_anything_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("gone.epub");
        assert!(replace_file(&target, b"new").is_err());
        assert!(!target.exists(), "nothing is created out of a failed edit");
    }
}
