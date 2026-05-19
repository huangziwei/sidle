//! Pull from `<kindle>/dedrm/`.
//!
//! The `dedrm` directory is populated by a Kindle-side jailbreak tool — files
//! there are stripped of DRM but still in Amazon's native container (`.kfx`
//! single-container, or `.kfx-zip` multi-container bundle). We hash each,
//! skip what's already in the local library by sha256, and run the rest
//! through the standard import pipeline — which synthesizes an EPUB via boko
//! and enqueues the canonical EPUB→KFX conversion just like a drag-drop.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;

use crate::device::detect::DeviceInfo;
use crate::library::db;
use crate::library::import::{self, ImportOutcome, sha256_of_file};
use crate::library::paths::LibraryPaths;

#[derive(Debug, Clone, Serialize)]
pub struct DedrmRow {
    /// Path on the Kindle, e.g. `/Volumes/Kindle/dedrm/foo.kfx-zip`.
    pub path: String,
    /// File basename (no directory).
    pub filename: String,
    /// SHA-256 of the file contents.
    pub sha256: String,
    /// True if a row with this sha already exists locally.
    pub already_imported: bool,
    pub size: u64,
}

/// List dedrm files on the device with their import status.
///
/// Scans `<kindle>/dedrm/` for `.kfx` and `.kfx-zip` files (case-insensitive).
/// Hashes each so the UI can show "X already imported / Y new" before the
/// user commits to a pull.
pub fn scan(conn: &rusqlite::Connection, device: &DeviceInfo) -> Result<Vec<DedrmRow>> {
    let dir = device.mount_path().join("dedrm");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_kfx_input(&path) {
            continue;
        }
        let sha = match sha256_of_file(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[sidle/dedrm] skip {}: hash failed: {e}", path.display());
                continue;
            }
        };
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let already: bool = conn
            .query_row(
                "SELECT 1 FROM books WHERE sha256 = ?1",
                rusqlite::params![sha],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        out.push(DedrmRow {
            path: path.to_string_lossy().to_string(),
            filename: path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            sha256: sha,
            already_imported: already,
            size,
        });
    }
    // Stable order: filename ascending.
    out.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(out)
}

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

/// Scan + pull every not-yet-imported dedrm file in one shot. Used by the
/// auto-pull-on-connect path so a freshly plugged-in Kindle drains its
/// `/dedrm` folder into the library without any UI interaction.
///
/// Returns `(PullResult, Option<book_id_to_enqueue>)` for each attempted
/// file. The caller (async context) does the actual queue enqueues.
pub fn pull_new(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    device: &DeviceInfo,
) -> Vec<(PullResult, Option<i64>)> {
    let rows = match scan(conn, device) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[sidle/dedrm] auto-scan failed: {e:#}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for r in rows {
        if r.already_imported {
            continue;
        }
        out.push(pull_one(conn, paths, device, Path::new(&r.path)));
    }
    out
}

/// Import a single dedrm file into the library. Records a `pull` row in
/// `device_history`. Returns the import outcome plus, when the row needs a
/// background conversion (now true for every fresh KFX/KFX-zip pull — the
/// EPUB is produced by the worker, not import_file), the `book_id` to
/// enqueue. Caller does the enqueue from async context.
pub fn pull_one(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    device: &DeviceInfo,
    path: &Path,
) -> (PullResult, Option<i64>) {
    match import::import_file(conn, paths, path) {
        Ok(ImportOutcome::Imported { book, needs_enqueue }) => {
            let _ = db::record_device_action(
                conn,
                &device.serial,
                &book.sha256,
                "pull",
                &path.to_string_lossy(),
            );
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
    use std::fs;

    fn touch(path: &Path, content: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn fake_device(mount: &Path) -> DeviceInfo {
        DeviceInfo {
            mount: mount.to_string_lossy().into_owned(),
            model: None,
            serial: "test".into(),
            free_bytes: None,
            total_bytes: None,
        }
    }

    #[test]
    fn scan_filters_by_extension_and_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let dedrm = tmp.path().join("dedrm");
        touch(&dedrm.join("a.kfx"), b"hello-kfx");
        touch(&dedrm.join("b.kfx-zip"), b"hello-bundle");
        touch(&dedrm.join("ignore.txt"), b"x");
        touch(&dedrm.join("notes.md"), b"y");

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE books (sha256 TEXT PRIMARY KEY)",
            rusqlite::params![],
        )
        .unwrap();

        let rows = scan(&conn, &fake_device(tmp.path())).unwrap();
        assert_eq!(rows.len(), 2);
        let names: Vec<_> = rows.iter().map(|r| r.filename.as_str()).collect();
        assert!(names.contains(&"a.kfx"));
        assert!(names.contains(&"b.kfx-zip"));
        for r in &rows {
            assert_eq!(r.sha256.len(), 64);
            assert!(!r.already_imported);
        }
    }

    #[test]
    fn scan_marks_already_imported() {
        let tmp = tempfile::tempdir().unwrap();
        let dedrm = tmp.path().join("dedrm");
        let f = dedrm.join("known.kfx-zip");
        touch(&f, b"known-bytes");

        let known_sha = sha256_of_file(&f).unwrap();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE books (sha256 TEXT PRIMARY KEY)",
            rusqlite::params![],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO books (sha256) VALUES (?1)",
            rusqlite::params![known_sha],
        )
        .unwrap();

        let rows = scan(&conn, &fake_device(tmp.path())).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].already_imported);
    }

    #[test]
    fn scan_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE books (sha256 TEXT PRIMARY KEY)",
            rusqlite::params![],
        )
        .unwrap();
        // No /dedrm subdir created — should return [].
        let rows = scan(&conn, &fake_device(tmp.path())).unwrap();
        assert!(rows.is_empty());
    }
}
