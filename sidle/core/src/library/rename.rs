//! Keep a book's on-disk filenames in sync with its (edited) metadata.
//!
//! Import names every file `[Author] Title (Year).<ext>` via
//! [`format_basename`]. When the user later edits the title/author/year in the
//! metadata modal, the DB row moves but the files keep their old names — so the
//! library folder drifts from what the gallery shows, and a forced re-convert
//! derives the *old* basename from the stale source stem. This module renames
//! the `epub`/`kfx`/`pdf` files to match the current metadata.
//!
//! Covers and thumbnails are sha-named (`cover.jpg`), never basename-named, so
//! they never move. A rename doesn't touch bytes, so `kfx_sha256` is unchanged
//! and the on-device filename infix (and annotation-sync matching) stays valid.

use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

use crate::library::authors;
use crate::library::db::{self, BookRow};
use crate::library::paths::{LibraryPaths, format_basename};

/// Rename `book_id`'s files so their basename is `[Author] Title (Year)` for the
/// row's *current* metadata, updating the path columns to match.
///
/// Best-effort per file: a rename that can't happen (file already gone, target
/// occupied, OS error) is logged and skipped — the others still move and the
/// command that called this never fails over a cosmetic rename. Returns the
/// refreshed row.
pub fn rename_book_files(
    conn: &Connection,
    paths: &LibraryPaths,
    book_id: i64,
) -> Result<Option<BookRow>> {
    let Some(book) = db::get_book(conn, book_id)? else {
        return Ok(None);
    };

    let author_list = authors::split_display(&book.author);
    let basename = format_basename(&author_list, &book.title, book.published_at.as_deref());
    let dir = paths.book_dir(&book.sha256);

    // (current stored path, extension, kfx?) for each side that's on disk.
    let sides = [
        (book.epub_path.as_deref(), "epub"),
        (book.pdf_path.as_deref(), "pdf"),
        (book.kfx_path.as_deref(), "kfx"),
    ];

    for (current, ext) in sides {
        let Some(current) = current else { continue };
        let cur_path = Path::new(current);
        let new_path = dir.join(format!("{basename}.{ext}"));
        if new_path == cur_path {
            continue;
        }
        if !cur_path.exists() {
            eprintln!("[sidle/rename] book {book_id} {ext}: source {current} missing; skip");
            continue;
        }
        if new_path.exists() {
            // Same basename for every ext in one dir, so this only triggers if a
            // stray file already squats the target — don't clobber it.
            eprintln!(
                "[sidle/rename] book {book_id} {ext}: target {} occupied; skip",
                new_path.display()
            );
            continue;
        }
        if let Err(e) = std::fs::rename(cur_path, &new_path) {
            eprintln!("[sidle/rename] book {book_id} {ext}: rename failed: {e}");
            continue;
        }
        let new_str = new_path.to_string_lossy();
        let res = match ext {
            "epub" => db::set_epub_path(conn, book_id, &new_str),
            "pdf" => db::set_pdf_path(conn, book_id, &new_str),
            // A rename preserves bytes, so re-record the existing hash unchanged.
            // (kfx_path is `Some` iff kfx_sha256 is `Some`; the `?` default keeps
            // a pre-hash legacy row from having its sha overwritten with "".)
            "kfx" => match book.kfx_sha256.as_deref() {
                Some(sha) => db::set_kfx_path_and_sha(conn, book_id, &new_str, sha),
                None => {
                    eprintln!("[sidle/rename] book {book_id} kfx: no kfx_sha256; leaving path");
                    Ok(())
                }
            },
            _ => Ok(()),
        };
        if let Err(e) = res {
            eprintln!("[sidle/rename] book {book_id} {ext}: db path update failed: {e}");
        }
    }

    Ok(db::get_book(conn, book_id)?)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::library::db::{self, MetadataPatch};
    use crate::library::import::import_file;
    use crate::library::paths::LibraryPaths;

    use super::rename_book_files;

    /// Editing title/author/year renames every on-disk side to the new
    /// `[Author] Title (Year)` and repoints the DB — while preserving the KFX
    /// byte-hash (a rename touches no bytes, so the on-device infix is stable).
    #[test]
    fn rename_follows_edited_metadata_and_keeps_kfx_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let conn = db::open(&paths.db()).unwrap();

        // A real PDF import gives us a book row + a persisted PDF side.
        let outcome =
            import_file(&conn, &paths, Path::new("tests/fixtures/minimal.pdf")).unwrap();
        let crate::library::import::ImportOutcome::Imported { book, .. } = outcome else {
            panic!("expected fresh import");
        };
        let id = book.id;

        // Fabricate the KFX side the worker would have produced, so the rename
        // exercises the hash-preserving KFX branch too.
        let dir = paths.book_dir(&book.sha256);
        let old_stem = Path::new(book.pdf_path.as_ref().unwrap())
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let old_kfx = dir.join(format!("{old_stem}.kfx"));
        std::fs::write(&old_kfx, b"fake-kfx-bytes").unwrap();
        db::set_kfx_path_and_sha(&conn, id, &old_kfx.to_string_lossy(), "cafef00d").unwrap();

        // Edit the metadata (full-replacement patch, mirroring the command).
        db::update_metadata(
            &conn,
            id,
            &MetadataPatch {
                title: "Brand New Title".into(),
                author: "Jane Q. Author".into(),
                language: "en".into(),
                published_at: Some("2021-03-15".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let updated = rename_book_files(&conn, &paths, id).unwrap().unwrap();

        let want = "[Jane Q. Author] Brand New Title (2021)";
        let pdf = updated.pdf_path.unwrap();
        let kfx = updated.kfx_path.unwrap();
        assert!(
            pdf.ends_with(&format!("{want}.pdf")),
            "pdf renamed: {pdf}"
        );
        assert!(
            kfx.ends_with(&format!("{want}.kfx")),
            "kfx renamed: {kfx}"
        );
        assert!(Path::new(&pdf).exists(), "renamed pdf on disk");
        assert!(Path::new(&kfx).exists(), "renamed kfx on disk");
        assert!(!old_kfx.exists(), "old kfx name gone");
        // The rename moved bytes, not content — the hash must be untouched.
        assert_eq!(updated.kfx_sha256.as_deref(), Some("cafef00d"));
    }

    /// No metadata change ⇒ no rename, no error (idempotent).
    #[test]
    fn rename_is_noop_when_basename_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let conn = db::open(&paths.db()).unwrap();

        let outcome =
            import_file(&conn, &paths, Path::new("tests/fixtures/minimal.pdf")).unwrap();
        let crate::library::import::ImportOutcome::Imported { book, .. } = outcome else {
            panic!("expected fresh import");
        };
        let before = book.pdf_path.clone().unwrap();

        let after = rename_book_files(&conn, &paths, book.id)
            .unwrap()
            .unwrap()
            .pdf_path
            .unwrap();

        assert_eq!(before, after);
        assert!(Path::new(&after).exists());
    }
}
