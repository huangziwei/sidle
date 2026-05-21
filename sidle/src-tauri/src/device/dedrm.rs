//! Pull from `<kindle>/dedrm/`.
//!
//! The `dedrm` directory is populated by a Kindle-side jailbreak tool — files
//! there are stripped of DRM but still in Amazon's native container (`.kfx`
//! single-container, or `.kfx-zip` multi-container bundle). We hash each,
//! skip what's already in the local library by sha256, and run the rest
//! through the standard import pipeline — which synthesizes an EPUB via boko
//! and enqueues the canonical EPUB→KFX conversion just like a drag-drop.
//!
//! Mass-storage only. Non-jailbroken devices (every MTP-class Kindle) have no
//! `/dedrm` folder, and the jailbreak that creates the folder isn't available
//! for Scribe-and-later firmware anyway. `monitor.rs` gates the call here on
//! `TransportKind::MassStorage`; reaching this module via any other transport
//! is a bug.

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;
use serde::Serialize;

use crate::device::DeviceInfo;
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

/// Phase 1 of the autopull scan: list every `.kfx` / `.kfx-zip` in `/dedrm`
/// and hash it. Does no DB work, so it's safe to run outside the DB lock —
/// crucial because hashing several MB-per-file off a USB-attached Kindle
/// takes a second or two, and holding the DB lock for that long would block
/// the frontend's first `library_list` request after a cold start.
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
        if !is_kfx_input(&path) {
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

/// Phase 2: filter the hashed candidates against the library so only files
/// we haven't seen before get pulled. Holds the DB lock for one `SELECT`
/// per candidate — tens of microseconds total, fine to keep inside the
/// autopull's lock-acquiring `spawn_blocking`.
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

/// Import a single dedrm file into the library. Returns the import outcome
/// plus, when the row needs a background conversion (now true for every
/// fresh KFX/KFX-zip pull — the EPUB is produced by the worker, not
/// import_file), the `book_id` to enqueue. Caller does the enqueue from
/// async context.
pub fn pull_one(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    _device: &DeviceInfo,
    path: &Path,
) -> (PullResult, Option<i64>) {
    match import::import_file(conn, paths, path) {
        Ok(ImportOutcome::Imported { book, needs_enqueue }) => {
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

fn is_kfx_input(path: &PathBuf) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("kfx") | Some("kfx-zip")
    ) && path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::TransportKind;
    use std::fs;

    fn touch(path: &Path, content: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn fake_device(mount: &Path) -> DeviceInfo {
        DeviceInfo {
            model: None,
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
        touch(&dedrm.join("a.kfx"), b"hello-kfx");
        touch(&dedrm.join("b.kfx-zip"), b"hello-bundle");
        touch(&dedrm.join("ignore.txt"), b"x");
        touch(&dedrm.join("notes.md"), b"y");

        let candidates = hash_dedrm_candidates(&fake_device(tmp.path()));
        assert_eq!(candidates.len(), 2);
        let names: Vec<_> = candidates
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"a.kfx".to_string()));
        assert!(names.contains(&"b.kfx-zip".to_string()));
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
