//! Pull from `<kindle>/dedrm/`.

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;
use serde::Serialize;

use crate::library::device::DeviceInfo;
use crate::library::import::{self, ImportOutcome, sha256_of_file};
use crate::library::paths::LibraryPaths;

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PullResult {
    Imported {
        book_id: i64,
        sha256: String,
        path: String,
    },
    Duplicate {
        book_id: i64,
        sha256: String,
        path: String,
    },
    Failed {
        path: String,
        error: String,
    },
}

/// Phase 1 of the autopull scan: list every decrypted book in `/dedrm` and hash
pub fn hash_dedrm_candidates(device: &DeviceInfo) -> Vec<(PathBuf, String)> {
    let Some(mount) = device.mass_storage_mount() else {
        return Vec::new();
    };
    let dir = mount.join("dedrm");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_dedrm_output(&path) {
            continue;
        }
        match sha256_of_file(&path) {
            Ok(sha) => out.push((path, sha)),
            Err(e) => {
                eprintln!("[sidle/dedrm] skip {}: hash failed: {e}", path.display())
            }
        }
    }
    out
}

/// Phase 2: the hashed candidates whose sha256 the `books` table lacks. Holds
/// the DB lock for one `SELECT` per candidate — tens of microseconds total,
/// fine to keep inside the autopull's lock-acquiring `spawn_blocking`.
pub fn filter_new_candidates(
    conn: &rusqlite::Connection,
    candidates: Vec<(PathBuf, String)>,
) -> Vec<PathBuf> {
    candidates
        .into_iter()
        .filter(|(_, sha)| {
            conn.query_row(
                "SELECT 1 FROM books WHERE sha256 = ?1",
                rusqlite::params![sha],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_none()
        })
        .map(|(path, _)| path)
        .collect()
}

/// Import a single dedrm file into the library. Returns the import outcome plus
/// the `book_id` to enqueue when the row needs a background conversion, which a
/// fresh KFX pull always does: the worker produces its EPUB, not `import_file`.
pub fn pull_one(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    _device: &DeviceInfo,
    path: &Path,
) -> (PullResult, Option<i64>) {
    match import::import_file(conn, paths, path) {
        Ok(ImportOutcome::Imported {
            book,
            needs_enqueue,
        }) => {
            let book_id = book.id;
            let result = PullResult::Imported {
                book_id,
                sha256: book.sha256,
                path: path.to_string_lossy().into_owned(),
            };
            (result, needs_enqueue.then_some(book_id))
        }
        Ok(ImportOutcome::Duplicate(book)) => (
            PullResult::Duplicate {
                book_id: book.id,
                sha256: book.sha256,
                path: path.to_string_lossy().into_owned(),
            },
            None,
        ),
        Err(e) => (
            PullResult::Failed {
                path: path.to_string_lossy().into_owned(),
                error: format!("{e:#}"),
            },
            None,
        ),
    }
}

/// What the Kindle-side tool writes into `/dedrm`: Amazon's KFX in either
/// container shape, and the MOBI family under the names it decrypts them to.
const DEDRM_EXTENSIONS: [&str; 5] = ["kfx", "kfx-zip", "azw3", "azw4", "mobi"];

fn is_dedrm_output(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .is_some_and(|ext| DEDRM_EXTENSIONS.contains(&ext.as_str()))
        && path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::device::TransportKind;
    use std::fs;

    fn touch(path: &Path, content: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn fake_device(mount: &Path) -> DeviceInfo {
        DeviceInfo {
            model: None,
            firmware: None,
            serial: "test".into(),
            free_bytes: None,
            total_bytes: None,
            transport: TransportKind::MassStorage {
                mount: mount.to_string_lossy().into_owned(),
            },
        }
    }

    fn fresh_books_table() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE books (sha256 TEXT PRIMARY KEY)",
            rusqlite::params![],
        )
        .unwrap();
        conn
    }

    #[test]
    fn hash_filters_by_extension_and_returns_64char_shas() {
        let tmp = tempfile::tempdir().unwrap();
        let dedrm = tmp.path().join("dedrm");
        for (i, ext) in DEDRM_EXTENSIONS.iter().enumerate() {
            touch(
                &dedrm.join(format!("book{i}.{ext}")),
                format!("b{i}").as_bytes(),
            );
        }
        // A FAT round-trip through a desktop can upper-case an extension.
        touch(&dedrm.join("shouty.AZW3"), b"loud");
        touch(&dedrm.join("ignore.txt"), b"x");
        touch(&dedrm.join("notes.md"), b"y");
        touch(&dedrm.join("sideload.epub"), b"z");

        let candidates = hash_dedrm_candidates(&fake_device(tmp.path()));
        let names: Vec<_> = candidates
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(candidates.len(), DEDRM_EXTENSIONS.len() + 1, "{names:?}");
        for (i, ext) in DEDRM_EXTENSIONS.iter().enumerate() {
            assert!(names.contains(&format!("book{i}.{ext}")), "{names:?}");
        }
        assert!(names.contains(&"shouty.AZW3".to_string()), "{names:?}");
        for (_, sha) in &candidates {
            assert_eq!(sha.len(), 64);
        }
    }

    #[test]
    fn filter_drops_already_imported() {
        let tmp = tempfile::tempdir().unwrap();
        let dedrm = tmp.path().join("dedrm");
        let known = dedrm.join("known.kfx-zip");
        let fresh = dedrm.join("fresh.kfx");
        touch(&known, b"known-bytes");
        touch(&fresh, b"fresh-bytes");

        let known_sha = sha256_of_file(&known).unwrap();
        let conn = fresh_books_table();
        conn.execute(
            "INSERT INTO books (sha256) VALUES (?1)",
            rusqlite::params![known_sha],
        )
        .unwrap();

        let candidates = hash_dedrm_candidates(&fake_device(tmp.path()));
        assert_eq!(candidates.len(), 2);

        let to_pull = filter_new_candidates(&conn, candidates);
        assert_eq!(to_pull.len(), 1);
        assert_eq!(to_pull[0].file_name().unwrap(), "fresh.kfx");
    }

    #[test]
    fn hash_returns_empty_when_dedrm_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // No /dedrm subdir created — hash_dedrm_candidates should return [].
        let candidates = hash_dedrm_candidates(&fake_device(tmp.path()));
        assert!(candidates.is_empty());
    }
}
