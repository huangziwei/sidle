//! Push a KFX from the local library to the device's documents directory,
//! and remove what we've sent.

use std::path::Path;

use crate::library::paths::{kfx_device_filename, parse_sha_infix, sha_infix};
use anyhow::{Context, Result};
use serde::Serialize;

use crate::library::db::{self, BookRow};
use crate::library::device::DeviceInfo;
use crate::library::device::transport::{TPath, Transport};

/// Where we write KFX. The `Sidle` subdir keeps our pushes namespaced so
/// the Kindle's `/documents` root stays whatever the user had before and
/// our deletes can't ever touch unrelated files.
fn documents_dir() -> TPath {
    TPath::parse("documents").join("Sidle")
}

/// The `<basename>.<sha8>.kfx` suffix that identifies an on-device file as
/// belonging to a particular library row. Matched against on-device
/// filenames in [`find_by_sha`] (already-present check).
fn kfx_suffix(sha256: &str) -> String {
    format!(".{}.kfx", sha_infix(sha256))
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PushResult {
    Pushed { book_id: i64, filename: String },
    AlreadyPresent { book_id: i64, filename: String },
    Skipped { book_id: i64, reason: String },
    Failed { book_id: i64, error: String },
}

/// Push one book's KFX to the device, streaming byte-progress for the copy via
pub fn push_one(
    _device: &DeviceInfo,
    transport: &dyn Transport,
    conn: &rusqlite::Connection,
    book: &BookRow,
    on_progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<PushResult> {
    if let Some(reason) = preflight(conn, book)? {
        return Ok(PushResult::Skipped {
            book_id: book.id,
            reason,
        });
    }
    let kfx_src = book
        .kfx_path
        .as_deref()
        .expect("preflight guarantees kfx_path is Some");
    let kfx_sha = book
        .kfx_sha256
        .as_deref()
        .expect("preflight guarantees kfx_sha256 is Some alongside kfx_path");

    let dest_dir = documents_dir();

    // Already-pushed check: any file in Sidle/ ending in the KFX sha8
    if let Some(existing) = find_by_sha(transport, &dest_dir, kfx_sha)? {
        return Ok(PushResult::AlreadyPresent {
            book_id: book.id,
            filename: existing,
        });
    }

    let filename = kfx_device_filename(kfx_src, kfx_sha);
    let dest = dest_dir.join(&filename);

    transport
        .copy_in_atomic_with_progress(Path::new(kfx_src), &dest, on_progress)
        .with_context(|| format!("copy {} -> {}", kfx_src, transport.display_path(&dest)))?;

    Ok(PushResult::Pushed {
        book_id: book.id,
        filename,
    })
}

fn preflight(conn: &rusqlite::Connection, book: &BookRow) -> Result<Option<String>> {
    if book.status != "done" {
        return Ok(Some(format!("status is {}", book.status)));
    }
    if book.kfx_path.is_none() {
        return Ok(Some("no KFX yet".to_string()));
    }
    if book.kfx_sha256.is_none() {
        // Row predates the kfx_sha256 column. Reconvert (or wait for the
        // bootstrap backfill) to populate it.
        return Ok(Some("kfx hash missing — reconvert".to_string()));
    }
    if db::job_in_flight(conn, book.id)? {
        return Ok(Some("conversion in flight".to_string()));
    }
    Ok(None)
}

/// Scan `documents/Sidle/` for a file whose name contains our sha8 infix.
fn find_by_sha(transport: &dyn Transport, dir: &TPath, sha256: &str) -> Result<Option<String>> {
    let suffix = kfx_suffix(sha256);
    for entry in transport.list(dir)? {
        if entry.is_dir || entry.name.starts_with('.') {
            continue;
        }
        if entry.name.ends_with(&suffix) {
            return Ok(Some(entry.name));
        }
    }
    Ok(None)
}

// ----------------------------------------------------------------------------
// Delete
// ----------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeleteResult {
    /// `.kfx` removed (or already absent — `file_existed` reflects the prior
    Removed {
        filename: String,
        file_existed: bool,
        sdr_existed: bool,
    },
    /// Filename didn't look like one of ours (no `.<sha8>.kfx` suffix), or
    NotOurs {
        filename: String,
    },
    Failed {
        filename: String,
        error: String,
    },
}

/// Remove an on-device file (and any `.sdr/` it spawned) by filename. The
pub fn delete_one(
    _device: &DeviceInfo,
    transport: &dyn Transport,
    filename: &str,
    asin: Option<&str>,
) -> Result<DeleteResult> {
    if filename.contains('/') || filename.contains('\\') || is_apple_double(filename) {
        return Ok(DeleteResult::NotOurs {
            filename: filename.to_string(),
        });
    }
    // Filename must match `<basename>.<sha8>.kfx`. Reject anything else so
    // a stale UI can't drive deletes of unrelated files even if the user
    // had moved random KFXes into `documents/Sidle/`.
    if parse_sha_infix(filename).is_none() {
        return Ok(DeleteResult::NotOurs {
            filename: filename.to_string(),
        });
    }
    let stem = filename
        .strip_suffix(".kfx")
        .expect("parse_sha_infix matched .kfx");

    let dir = documents_dir();
    let sdr_name = format!("{stem}.sdr");

    // Best-effort wipe of any legacy `._<name>` AppleDouble dropped by an
    // older push (before `copy_in_atomic` switched to raw read/write).
    let _ = transport.delete(&dir.join(&format!("._{filename}")));

    let file_existed = transport
        .delete(&dir.join(filename))
        .with_context(|| format!("delete {filename}"))?;

    // .sdr/ cleanup is best-effort — the .kfx is the user-visible thing;
    // leaving the sidecar behind is annoying but not corrupting.
    let filename_sdr_existed = match transport.delete_dir(&dir.join(&sdr_name)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[sidle/delete] sdr cleanup failed for {sdr_name}: {e:#}");
            false
        }
    };

    // Kindle also drops a *catalog-style* `<title>_<ASIN>.sdr/` next to
    let catalog_sdr_existed = match asin {
        Some(asin) if !asin.is_empty() => wipe_catalog_sdrs(transport, &dir, asin, &sdr_name),
        _ => false,
    };

    Ok(DeleteResult::Removed {
        filename: filename.to_string(),
        file_existed,
        sdr_existed: filename_sdr_existed || catalog_sdr_existed,
    })
}

/// Scan `documents/Sidle/` for any directory ending with `_<asin>.sdr` and
/// wipe it. Skips the filename-style `.sdr` that the caller already
/// handled. Returns true if at least one extra directory was wiped.
fn wipe_catalog_sdrs(
    transport: &dyn Transport,
    dir: &TPath,
    asin: &str,
    already_wiped: &str,
) -> bool {
    let suffix = format!("_{asin}.sdr");
    let entries = match transport.list(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[sidle/delete] catalog sdr scan failed: {e:#}");
            return false;
        }
    };
    let mut wiped_any = false;
    for entry in entries {
        if !entry.is_dir || entry.name == already_wiped {
            continue;
        }
        if !entry.name.ends_with(&suffix) {
            continue;
        }
        match transport.delete_dir(&dir.join(&entry.name)) {
            Ok(true) => wiped_any = true,
            Ok(false) => {}
            Err(e) => {
                eprintln!(
                    "[sidle/delete] catalog sdr wipe failed for {}: {e:#}",
                    entry.name
                );
            }
        }
    }
    wiped_any
}

fn is_apple_double(name: &str) -> bool {
    name.starts_with("._")
}
