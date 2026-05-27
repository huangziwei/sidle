//! Relocate the library to a new root, or adopt one that already lives
//! elsewhere. The copy is a consistent DB snapshot (`VACUUM INTO`, so it's
//! transactionally consistent and WAL-free even while the conversion queue /
//! device monitor write concurrently — the §3 H1 hazard) plus the `books/`
//! tree. Copy, not move, so a failure is non-destructive.
//!
//! Nothing here touches the source or the root pointer: the Tauri command layer
//! repoints via [`LibraryPaths::set_root`](crate::library::LibraryPaths::set_root)
//! and relaunches. See `.claude/plans/library-backup-and-portability.md` §6.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

/// Copy the library — a `VACUUM INTO` snapshot of `src_conn` plus the
/// `src_books` tree — into `dest_root`. Returns the book count, verified equal
/// in source and copy. `dest_root` should be empty/new (the caller checks).
pub fn copy_library(src_conn: &Connection, src_books: &Path, dest_root: &Path) -> Result<i64> {
    let dest_books = dest_root.join("books");
    std::fs::create_dir_all(&dest_books)
        .with_context(|| format!("create {}", dest_books.display()))?;

    let dest_db = dest_root.join("library.db");
    snapshot_db(src_conn, &dest_db)?;

    if src_books.is_dir() {
        copy_dir(src_books, &dest_books)?;
    }

    let expected = count_books(src_conn).context("count source books")?;
    let copied = {
        let dest = Connection::open(&dest_db)
            .with_context(|| format!("open copied db {}", dest_db.display()))?;
        count_books(&dest).context("count copied books")?
    };
    if copied != expected {
        bail!("relocate verify failed: source has {expected} books, copy has {copied}");
    }
    Ok(expected)
}

/// Validate that `dir` already holds a sidle library (a readable `library.db`
/// with a `books` table); returns its book count. Used by "Use existing" before
/// repointing, so an empty or foreign folder is rejected cleanly.
pub fn validate_existing(dir: &Path) -> Result<i64> {
    let db = dir.join("library.db");
    if !db.is_file() {
        bail!("no library.db in {}", dir.display());
    }
    let conn = Connection::open(&db).with_context(|| format!("open {}", db.display()))?;
    count_books(&conn).context("not a sidle library (no books table)")
}

/// `VACUUM INTO` — a transactionally-consistent, WAL-free single-file copy of
/// the live DB. Tolerates concurrent writers (snapshots committed state; later
/// commits simply aren't included).
fn snapshot_db(src: &Connection, dest_db: &Path) -> Result<()> {
    src.execute("VACUUM INTO ?1", [dest_db.to_string_lossy().as_ref()])
        .with_context(|| format!("VACUUM INTO {}", dest_db.display()))?;
    Ok(())
}

fn count_books(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM books", [], |r| r.get(0))
}

/// Recursively copy the contents of `src` into `dest` (created if absent).
fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::db;

    #[test]
    fn copy_library_snapshots_db_and_books_then_verifies() {
        let src_root = tempfile::tempdir().unwrap();
        let dest_parent = tempfile::tempdir().unwrap();
        // dest must be empty/new — a subdir that doesn't exist yet.
        let dest = dest_parent.path().join("moved");

        let conn = db::open(&src_root.path().join("library.db")).unwrap();
        let books_dir = src_root.path().join("books");
        for (sha, title) in [("aaa", "One"), ("bbb", "Two")] {
            let dir = books_dir.join(sha);
            std::fs::create_dir_all(&dir).unwrap();
            let epub = dir.join("book.epub");
            std::fs::write(&epub, format!("epub-{sha}")).unwrap();
            let epub_s = epub.to_string_lossy().into_owned();
            db::insert_book(
                &conn,
                &db::NewBook {
                    sha256: sha,
                    title,
                    author: "",
                    language: "",
                    ppd: None,
                    epub_path: Some(&epub_s),
                    cover_path: None,
                    kfx_path: None,
                    kfx_sha256: None,
                    file_size: 0,
                    imported_at: "t",
                    asin: None,
                    publisher: None,
                    published_at: None,
                    series_name: None,
                    series_index: None,
                    tags: &[],
                },
            )
            .unwrap();
        }

        let n = copy_library(&conn, &books_dir, &dest).unwrap();
        assert_eq!(n, 2);
        assert!(dest.join("library.db").is_file());
        assert_eq!(std::fs::read_to_string(dest.join("books/aaa/book.epub")).unwrap(), "epub-aaa");
        assert_eq!(std::fs::read_to_string(dest.join("books/bbb/book.epub")).unwrap(), "epub-bbb");
        // The copied DB reads back as a sidle library with the same count.
        assert_eq!(validate_existing(&dest).unwrap(), 2);
    }

    #[test]
    fn validate_existing_rejects_empty_or_foreign_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validate_existing(dir.path()).is_err());
    }
}
