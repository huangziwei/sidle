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
    let kfx_sha = book
        .kfx_sha256
        .as_deref()
        .expect("preflight guarantees kfx_sha256 is Some alongside kfx_path");

    let dest_dir = documents_dir();

    // Already-pushed check: any file in Sidle/ ending in the KFX sha8
    // infix is ours, regardless of basename (a metadata edit can leave
    // the on-device file with the old basename — that's fine, sha is
    // the stable identity).
    if let Some(existing) = find_by_sha(transport, &dest_dir, kfx_sha)? {
        return Ok(PushResult::AlreadyPresent {
            book_id: book.id,
            filename: existing,
        });
    }

    let base = Path::new(kfx_src)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("book-{}", sha_infix(kfx_sha)));
    let filename = format!("{base}.{}.kfx", sha_infix(kfx_sha));
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
/// Returns the actual filename if present. Skips `._*` AppleDouble
/// companions — macOS drops them next to the real file on FAT volumes,
/// and matching one of those would have us return (and later delete) the
/// metadata file instead of the KFX itself.
fn find_by_sha(
    transport: &dyn Transport,
    dir: &TPath,
    sha256: &str,
) -> Result<Option<String>> {
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
    /// state). All matching `.sdr/` sidecars were wiped as well — both the
    /// filename-style (`<basename>.<sha8>.sdr`) and any catalog-style
    /// (`<title>_<ASIN>.sdr`) Kindle invented next to the file.
    /// `sdr_existed` records whether at least one was present at delete.
    Removed {
        filename: String,
        file_existed: bool,
        sdr_existed: bool,
    },
    /// Filename didn't look like one of ours (no `.<sha8>.kfx` suffix), or
    /// it tried to climb out of `Sidle/` via path separators. Treated as a
    /// hard refusal — better to surface than to silently target the wrong
    /// thing.
    NotOurs { filename: String },
    Failed { filename: String, error: String },
}

/// Remove an on-device file (and any `.sdr/` it spawned) by filename. The
/// popup always has the exact filename, so we don't need to scan + match
/// for the .kfx itself — we just verify the filename has our `.<sha8>.kfx`
/// shape (defense against a stale UI passing in arbitrary user files) and
/// delete by name. The `asin` argument enables a second .sdr cleanup pass
/// keyed on the catalog-style name Kindle invents (see below).
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
    let Some(stem) = filename.strip_suffix(".kfx") else {
        return Ok(DeleteResult::NotOurs {
            filename: filename.to_string(),
        });
    };
    let Some((_, sha)) = stem.rsplit_once('.') else {
        return Ok(DeleteResult::NotOurs {
            filename: filename.to_string(),
        });
    };
    if sha.len() != SHA_INFIX_LEN || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(DeleteResult::NotOurs {
            filename: filename.to_string(),
        });
    }

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
    // the file (mirrors how Amazon-fetched books are tracked by ASIN
    // server-side, even for PDOC sideloads). The title segment is
    // Kindle-normalized — different from our `<basename>` — so we can't
    // predict the exact name, but the `_<ASIN>.sdr` suffix is unique
    // per book (the ASIN is boko-kai's content-derived fabricated value,
    // or a real catalog one if it came from kfxlib). Scan `Sidle/` and
    // wipe any `.sdr` whose name ends with `_<ASIN>.sdr`.
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
/// Errors are logged, never propagated — the kfx is gone either way.
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
