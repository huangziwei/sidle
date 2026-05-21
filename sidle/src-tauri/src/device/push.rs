//! Push a KFX from the local library to the device's documents directory,
//! and remove what we've sent.
//!
//! State model: there is no on-device manifest. The `documents/Sidle/`
//! directory is the source of truth — every file in it is something we
//! pushed, and the sha8 infix in each filename (`<basename>.<sha8>.kfx`)
//! links it back to a `books.sha256` in the local library DB. To detect
//! "already pushed" we list `Sidle/` and look for any `*.<sha8>.kfx`; to
//! delete we scan for that same pattern and remove both the `.kfx` and the
//! Kindle-created `<basename>.<sha8>.sdr/` sidecar next to it (reading
//! progress, annotations, highlights — invisible to sidle but it
//! accumulates if we don't clean it up).
//!
//! Routes through [`Transport`] so the same code handles mass-storage (KOA2
//! family) and MTP (Scribe, 2024+): `copy_in_atomic` does the right thing
//! for each (`.partial` + `rename` on a real filesystem, `SendObjectInfo`
//! / `SendObject` over MTP), and `delete_dir` handles the `.sdr/` recursion
//! on MTP where `DeleteObject` doesn't loop.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::device::DeviceInfo;
use crate::device::transport::{TPath, Transport};
use crate::library::db::{self, BookRow};

/// Length of the sha256 prefix we put in filenames. 8 hex chars = 32 bits;
/// collision-free for any realistic personal library (50% chance at ~93k
/// books per the birthday bound), and short enough to stay readable.
const SHA_INFIX_LEN: usize = 8;

/// Where we write KFX. The `Sidle` subdir keeps our pushes namespaced so
/// the Kindle's `/documents` root stays whatever the user had before and
/// our deletes can't ever touch unrelated files.
fn documents_dir() -> TPath {
    TPath::parse("documents").join("Sidle")
}

fn sha_infix(sha256: &str) -> &str {
    &sha256[..SHA_INFIX_LEN]
}

/// The `<basename>.<sha8>.kfx` suffix that identifies an on-device file as
/// belonging to a particular library row. Matched against on-device
/// filenames in both push (already-present check) and delete (find-and-
/// remove).
fn kfx_suffix(sha256: &str) -> String {
    format!(".{}.kfx", sha_infix(sha256))
}

fn sdr_suffix(sha256: &str) -> String {
    format!(".{}.sdr", sha_infix(sha256))
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PushResult {
    Pushed { book_id: i64, filename: String },
    AlreadyPresent { book_id: i64, filename: String },
    Skipped { book_id: i64, reason: String },
    Failed { book_id: i64, error: String },
}

pub fn push_one(
    _device: &DeviceInfo,
    transport: &dyn Transport,
    conn: &rusqlite::Connection,
    book: &BookRow,
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

    let dest_dir = documents_dir();

    // Already-pushed check: any file in Sidle/ ending in our sha8 infix is
    // ours, regardless of basename (a re-import after a metadata edit can
    // leave the on-device file with the old basename — that's fine, sha is
    // the stable identity).
    if let Some(existing) = find_by_sha(transport, &dest_dir, &book.sha256)? {
        return Ok(PushResult::AlreadyPresent {
            book_id: book.id,
            filename: existing,
        });
    }

    let base = Path::new(kfx_src)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("book-{}", sha_infix(&book.sha256)));
    let filename = format!("{base}.{}.kfx", sha_infix(&book.sha256));
    let dest = dest_dir.join(&filename);

    transport
        .copy_in_atomic(Path::new(kfx_src), &dest)
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
    if db::job_in_flight(conn, book.id)? {
        return Ok(Some("conversion in flight".to_string()));
    }
    Ok(None)
}

/// Scan `documents/Sidle/` for a file whose name contains our sha8 infix.
/// Returns the actual filename if present.
fn find_by_sha(
    transport: &dyn Transport,
    dir: &TPath,
    sha256: &str,
) -> Result<Option<String>> {
    let suffix = kfx_suffix(sha256);
    for entry in transport.list(dir)? {
        if entry.is_dir {
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
    /// state). The matching `.sdr/` sidecar, if any, was wiped as well —
    /// `sdr_existed` records whether it was present at delete time.
    Removed {
        sha256: String,
        filename: Option<String>,
        file_existed: bool,
        sdr_existed: bool,
    },
    Failed { sha256: String, error: String },
}

/// Remove an on-device file (and its `.sdr/`) by sha. The directory scan
/// keeps deletes inherently namespaced — we only touch things matching our
/// sha8 infix, so we can never nuke unrelated user files even if the local
/// library DB is wiped.
pub fn delete_one(
    _device: &DeviceInfo,
    transport: &dyn Transport,
    sha256: &str,
) -> Result<DeleteResult> {
    let dir = documents_dir();
    let filename = find_by_sha(transport, &dir, sha256)?;

    // Always attempt the `.sdr/` cleanup, even if the `.kfx` is missing —
    // the sidecar is the Kindle's, not ours, and could outlive a manual
    // on-device delete. We need the basename to address it; derive it from
    // the .kfx filename when we have one, otherwise scan for any matching
    // `*.<sha8>.sdr` in the directory.
    let sdr_path = if let Some(ref name) = filename {
        // Strip the `.kfx` to get the `<basename>.<sha8>` stem.
        let stem = name.strip_suffix(".kfx").unwrap_or(name.as_str());
        Some(dir.join(&format!("{stem}.sdr")))
    } else {
        find_sdr_by_sha(transport, &dir, sha256)?.map(|name| dir.join(&name))
    };

    let file_existed = if let Some(ref name) = filename {
        transport
            .delete(&dir.join(name))
            .with_context(|| format!("delete {name}"))?
    } else {
        false
    };

    let sdr_existed = if let Some(ref p) = sdr_path {
        // Best-effort: log to stderr but don't fail the delete — the .kfx
        // is already gone, leaving a residual .sdr/ behind is annoying but
        // not corrupting.
        match transport.delete_dir(p) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "[sidle/delete] sdr cleanup failed for {sha256}: {e:#}"
                );
                false
            }
        }
    } else {
        false
    };

    Ok(DeleteResult::Removed {
        sha256: sha256.to_string(),
        filename,
        file_existed,
        sdr_existed,
    })
}

fn find_sdr_by_sha(
    transport: &dyn Transport,
    dir: &TPath,
    sha256: &str,
) -> Result<Option<String>> {
    let suffix = sdr_suffix(sha256);
    for entry in transport.list(dir)? {
        if !entry.is_dir {
            continue;
        }
        if entry.name.ends_with(&suffix) {
            return Ok(Some(entry.name));
        }
    }
    Ok(None)
}
