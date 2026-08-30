//! Relocate the library to a new root, or adopt one sitting

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

/// Copy the library — a `VACUUM INTO` snapshot of `src_conn` plus the
/// `src_books` tree — into `dest_root`. Returns the book count, verified equal
/// in source and copy. `dest_root` is an empty directory.
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

/// Move the library into the empty `dest_root`: snapshot and verify the DB
pub fn move_library(
    src_conn: &Connection,
    src_root: &Path,
    dest_root: &Path,
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dest_root)
        .with_context(|| format!("create {}", dest_root.display()))?;

    let dest_db = dest_root.join("library.db");
    snapshot_db(src_conn, &dest_db)?;
    let expected = count_books(src_conn).context("count source books")?;
    {
        let dest = Connection::open(&dest_db)
            .with_context(|| format!("open copied db {}", dest_db.display()))?;
        let got = count_books(&dest).context("count copied books")?;
        if got != expected {
            bail!("relocate verify failed: source has {expected} books, copy has {got}");
        }
    }

    let mut copied = Vec::new();
    for entry in
        std::fs::read_dir(src_root).with_context(|| format!("read {}", src_root.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        if is_db_or_pointer(&name) {
            continue;
        }
        let from = entry.path();
        let to = dest_root.join(&name);
        match std::fs::rename(&from, &to) {
            Ok(()) => {}
            // Cross-volume rename fails (EXDEV); copy and let finish_move remove
            // the source after the repoint.
            Err(_) => {
                copy_path(&from, &to)?;
                copied.push(from);
            }
        }
    }
    Ok(copied)
}

/// The root entries [`move_library`] relocates by other means: the live DB and
/// its WAL/SHM sidecars, which it snapshots, and `config.json`, the root
/// pointer that stays in the state dir.
fn is_db_or_pointer(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some("library.db" | "library.db-wal" | "library.db-shm" | "config.json")
    )
}

/// Delete the moved-from library's remnants once the caller has repointed:
/// `copied`, the old `library.db*`, and an emptied `src_root` that is not
/// `state_dir`. A file that resists removal is logged.
pub fn finish_move(src_root: &Path, state_dir: &Path, copied: &[PathBuf]) {
    for from in copied {
        let removed = if from.is_dir() {
            std::fs::remove_dir_all(from)
        } else {
            std::fs::remove_file(from)
        };
        if let Err(e) = removed {
            eprintln!("[sidle/relocate] left old {}: {e}", from.display());
        }
    }
    for name in ["library.db", "library.db-wal", "library.db-shm"] {
        let f = src_root.join(name);
        if f.exists()
            && let Err(e) = std::fs::remove_file(&f)
        {
            eprintln!("[sidle/relocate] left old {}: {e}", f.display());
        }
    }
    if src_root != state_dir
        && dir_is_empty(src_root)
        && let Err(e) = std::fs::remove_dir(src_root)
    {
        eprintln!("[sidle/relocate] left old root {}: {e}", src_root.display());
    }
}

/// The book count of the sidle library in `dir` — a readable `library.db`
/// carrying a `books` table. An empty or foreign folder is refused.
pub fn validate_existing(dir: &Path) -> Result<i64> {
    let db = dir.join("library.db");
    if !db.is_file() {
        bail!("no library.db in {}", dir.display());
    }
    let conn = Connection::open(&db).with_context(|| format!("open {}", db.display()))?;
    count_books(&conn).context("not a sidle library (no books table)")
}

/// `VACUUM INTO`: a transactionally consistent, WAL-free single-file copy of
/// the live DB, taking committed state under a concurrent writer.
pub(crate) fn snapshot_db(src: &Connection, dest_db: &Path) -> Result<()> {
    src.execute("VACUUM INTO ?1", [dest_db.to_string_lossy().as_ref()])
        .with_context(|| format!("VACUUM INTO {}", dest_db.display()))?;
    Ok(())
}

fn count_books(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM books", [], |r| r.get(0))
}

/// Recursively copy the contents of `src` into `dest` (created if absent).
/// `pub(crate)` so merge can copy a staged `books/<sha>/` or `notebooks/<uuid>/`
/// dir into the live root.
pub(crate) fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
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

/// Copy a single root entry — a file or a whole directory tree — for the
/// cross-volume move path, where `rename` can't cross the device boundary.
fn copy_path(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        copy_dir(from, to)
    } else {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::copy(from, to)
            .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
        Ok(())
    }
}

/// True if `dir` exists and contains no entries.
fn dir_is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_none())
        .unwrap_or(false)
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
                    pdf_path: None,
                    file_size: 0,
                    imported_at: "t",
                    asin: None,
                    amazon_asin: None,
                    publisher: None,
                    published_at: None,
                    series_name: None,
                    series_index: None,
                    tags: &[],
                    title_romaji: "",
                    author_romaji: "",
                    source_format: None,
                },
            )
            .unwrap();
        }

        let n = copy_library(&conn, &books_dir, &dest).unwrap();
        assert_eq!(n, 2);
        assert!(dest.join("library.db").is_file());
        assert_eq!(
            std::fs::read_to_string(dest.join("books/aaa/book.epub")).unwrap(),
            "epub-aaa"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("books/bbb/book.epub")).unwrap(),
            "epub-bbb"
        );
        // The copied DB reads back as a sidle library with the same count.
        assert_eq!(validate_existing(&dest).unwrap(), 2);
    }

    #[test]
    fn validate_existing_rejects_empty_or_foreign_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validate_existing(dir.path()).is_err());
    }

    #[test]
    fn move_library_renames_then_finish_removes_old_root() {
        let parent = tempfile::tempdir().unwrap();
        let src_root = parent.path().join("old");
        let dest_root = parent.path().join("new");
        std::fs::create_dir_all(&src_root).unwrap();

        let conn = db::open(&src_root.join("library.db")).unwrap();
        let books = src_root.join("books");
        for (sha, title) in [("aaa", "One"), ("bbb", "Two")] {
            std::fs::create_dir_all(books.join(sha)).unwrap();
            std::fs::write(books.join(sha).join("book.epub"), format!("e-{sha}")).unwrap();
            db::insert_book(
                &conn,
                &db::NewBook {
                    sha256: sha,
                    title,
                    author: "",
                    language: "",
                    ppd: None,
                    epub_path: None,
                    cover_path: None,
                    kfx_path: None,
                    kfx_sha256: None,
                    pdf_path: None,
                    file_size: 0,
                    imported_at: "t",
                    asin: None,
                    amazon_asin: None,
                    publisher: None,
                    published_at: None,
                    series_name: None,
                    series_index: None,
                    tags: &[],
                    title_romaji: "",
                    author_romaji: "",
                    source_format: None,
                },
            )
            .unwrap();
        }

        // Other root contents must move too: notebooks (real data!), the staged
        // device-dist bundle, and the server token. Only library.db* stays behind
        // for finish_move.
        std::fs::create_dir_all(src_root.join("notebooks/nb-1/pages")).unwrap();
        std::fs::write(src_root.join("notebooks/nb-1/pages/page-0.svg"), "<svg/>").unwrap();
        std::fs::create_dir_all(src_root.join("device-dist")).unwrap();
        std::fs::write(src_root.join("device-dist/manifest.json"), "{}").unwrap();
        std::fs::write(src_root.join(".server-token"), "tok").unwrap();

        // Same volume (same tempdir) → rename path; nothing is copied.
        let copied = move_library(&conn, &src_root, &dest_root).unwrap();
        assert!(
            copied.is_empty(),
            "same-volume move renames, nothing copied"
        );
        assert!(dest_root.join("library.db").is_file());
        assert_eq!(
            std::fs::read_to_string(dest_root.join("books/aaa/book.epub")).unwrap(),
            "e-aaa"
        );
        // notebooks / device-dist / token relocated, not stranded.
        assert_eq!(
            std::fs::read_to_string(dest_root.join("notebooks/nb-1/pages/page-0.svg")).unwrap(),
            "<svg/>",
        );
        assert!(dest_root.join("device-dist/manifest.json").is_file());
        assert!(dest_root.join(".server-token").is_file());
        assert!(
            !src_root.join("books").exists(),
            "books renamed out of source"
        );
        assert!(
            !src_root.join("notebooks").exists(),
            "notebooks renamed out of source"
        );
        assert!(
            !src_root.join("device-dist").exists(),
            "device-dist renamed out of source"
        );
        assert!(
            !src_root.join(".server-token").exists(),
            "token renamed out of source"
        );
        assert!(
            src_root.join("library.db").is_file(),
            "old db kept until finish_move"
        );
        assert_eq!(validate_existing(&dest_root).unwrap(), 2);

        // finish_move clears the old db; src_root isn't the state dir and is now
        // empty, so it's removed entirely.
        let state_dir = parent.path().join("state");
        finish_move(&src_root, &state_dir, &copied);
        assert!(!src_root.exists(), "empty old root removed");
    }

    #[test]
    fn finish_move_keeps_state_dir_and_its_config() {
        // A `src_root` equal to `state_dir`: `finish_move` drops the old
        // library.db and keeps the dir and its config.json pointer.
        let state_dir = tempfile::tempdir().unwrap();
        std::fs::write(state_dir.path().join("library.db"), b"x").unwrap();
        std::fs::write(state_dir.path().join("config.json"), b"{}").unwrap();

        finish_move(state_dir.path(), state_dir.path(), &[]);

        assert!(
            !state_dir.path().join("library.db").exists(),
            "old db dropped"
        );
        assert!(
            state_dir.path().join("config.json").is_file(),
            "pointer kept"
        );
        assert!(state_dir.path().exists(), "state dir kept");
    }

    #[test]
    fn finish_move_deletes_copied_sources_then_removes_empty_root() {
        // Cross-volume path: move_library copied these out and handed the source
        // paths back for deletion after the repoint.
        let parent = tempfile::tempdir().unwrap();
        let src_root = parent.path().join("old");
        std::fs::create_dir_all(src_root.join("books/aaa")).unwrap();
        std::fs::write(src_root.join("books/aaa/x.epub"), "e").unwrap();
        std::fs::write(src_root.join(".server-token"), "t").unwrap();
        std::fs::write(src_root.join("library.db"), "db").unwrap();

        let copied = vec![src_root.join("books"), src_root.join(".server-token")];
        let state_dir = parent.path().join("state");
        finish_move(&src_root, &state_dir, &copied);

        assert!(
            !src_root.join("books").exists(),
            "copied dir source removed"
        );
        assert!(
            !src_root.join(".server-token").exists(),
            "copied file source removed"
        );
        assert!(!src_root.exists(), "now-empty old root removed");
    }
}
