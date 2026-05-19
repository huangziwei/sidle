//! Push a KFX from the local library to the device's documents directory.
//!
//! Routes through [`Transport`] so the same code handles mass-storage (KOA2
//! family) and MTP (Scribe, 2024+) — `copy_in_atomic` does the right thing for
//! each: `.partial` + `rename` on a real filesystem, `SendObjectInfo` /
//! `SendObject` over MTP. Either way, an unplug mid-copy leaves no visible
//! half-written `.kfx` for the device's indexer to choke on.
//!
//! Updates the on-device manifest (`<device>/.sidle/sent.json`) and the local
//! `device_history` table.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::device::DeviceInfo;
use crate::device::manifest::{self, Manifest, SentEntry};
use crate::device::transport::{TPath, Transport};
use crate::library::db::{self, BookRow};

/// Where we write KFX. The `Sidle` subdir keeps our pushes namespaced so the
/// Kindle's `/documents` root stays whatever the user had before, and our
/// deletes can't ever touch unrelated files.
fn documents_dir() -> TPath {
    TPath::parse("documents").join("Sidle")
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
    conn: &rusqlite::Connection,
    device: &DeviceInfo,
    transport: &dyn Transport,
    manifest: &mut Manifest,
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

    // If we've sent it before AND the file still exists, no-op.
    if let Some(entry) = manifest.sent.get(&book.sha256).cloned() {
        let dest = dest_dir.join(&entry.filename);
        if transport.exists(&dest).unwrap_or(false) {
            return Ok(PushResult::AlreadyPresent {
                book_id: book.id,
                filename: entry.filename,
            });
        }
        // Manifest says we sent it but the file is gone (user deleted on-device).
        // Re-push under the same name.
        transport
            .copy_in_atomic(Path::new(kfx_src), &dest)
            .with_context(|| format!("copy {} -> {}", kfx_src, transport.display_path(&dest)))?;
        db::record_device_action(
            conn,
            &device.serial,
            &book.sha256,
            "push",
            &transport.display_path(&dest),
        )?;
        return Ok(PushResult::Pushed {
            book_id: book.id,
            filename: entry.filename,
        });
    }

    // Both files share the same basename (import writes them as
    // `<basename>.kfx` / `<basename>.epub`), so deriving from the KFX path
    // we just bound above is equivalent to the old `epub_path` derivation —
    // and avoids depending on a field that may be `None` for a KFX-imported
    // book whose EPUB hasn't finished converting yet.
    let base = Path::new(kfx_src)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("book-{}", &book.sha256[..8]));

    let filename = unique_filename(transport, &dest_dir, &base, manifest, &book.sha256);
    let dest = dest_dir.join(&filename);
    transport
        .copy_in_atomic(Path::new(kfx_src), &dest)
        .with_context(|| format!("copy {} -> {}", kfx_src, transport.display_path(&dest)))?;

    let now = db::now_iso();
    manifest.sent.insert(
        book.sha256.clone(),
        SentEntry {
            title: book.title.clone(),
            author: book.author.clone(),
            filename: filename.clone(),
            sent_at: now,
        },
    );
    manifest::save(transport, manifest)?;

    db::record_device_action(
        conn,
        &device.serial,
        &book.sha256,
        "push",
        &transport.display_path(&dest),
    )?;

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

/// Find a non-colliding filename: `<base>.kfx`, then `<base> (2).kfx`, …
fn unique_filename(
    transport: &dyn Transport,
    dir: &TPath,
    base: &str,
    manifest: &Manifest,
    self_sha: &str,
) -> String {
    let candidate = format!("{base}.kfx");
    if !filename_taken(transport, dir, &candidate, manifest, self_sha) {
        return candidate;
    }
    for n in 2..1000 {
        let candidate = format!("{base} ({n}).kfx");
        if !filename_taken(transport, dir, &candidate, manifest, self_sha) {
            return candidate;
        }
    }
    // Pathological fallback — essentially never reached.
    format!("{base}-{}.kfx", &self_sha[..8])
}

fn filename_taken(
    transport: &dyn Transport,
    dir: &TPath,
    name: &str,
    manifest: &Manifest,
    self_sha: &str,
) -> bool {
    if transport.exists(&dir.join(name)).unwrap_or(false) {
        return true;
    }
    manifest
        .sent
        .iter()
        .any(|(sha, e)| sha != self_sha && e.filename == name)
}

// ----------------------------------------------------------------------------
// Delete
// ----------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeleteResult {
    /// Removed from /documents and from sent.json.
    Removed {
        sha256: String,
        filename: String,
        /// Whether the file was still on the device at the moment of removal.
        /// `false` means the user had already deleted it on-device.
        file_existed: bool,
    },
    /// Sha wasn't in our manifest — refused to touch the file.
    NotOurs { sha256: String },
    Failed { sha256: String, error: String },
}

/// Remove a book we previously sent from the Kindle.
///
/// **Safety:** only books in `manifest.sent` are deletable. Anything else is
/// invisible to this function — explicit guard against nuking unrelated files
/// the user has on their device.
pub fn delete_one(
    conn: &rusqlite::Connection,
    device: &DeviceInfo,
    transport: &dyn Transport,
    manifest: &mut Manifest,
    sha256: &str,
) -> Result<DeleteResult> {
    let Some(entry) = manifest.sent.remove(sha256) else {
        return Ok(DeleteResult::NotOurs {
            sha256: sha256.to_string(),
        });
    };

    let dest = documents_dir().join(&entry.filename);
    let file_existed = match transport.delete(&dest) {
        Ok(b) => b,
        Err(e) => {
            // Roll back the manifest mutation so we don't lose the record of
            // a file we actually couldn't remove.
            manifest.sent.insert(sha256.to_string(), entry);
            return Err(e);
        }
    };

    manifest::save(transport, manifest)?;
    db::record_device_action(
        conn,
        &device.serial,
        sha256,
        "delete",
        &transport.display_path(&dest),
    )?;

    Ok(DeleteResult::Removed {
        sha256: sha256.to_string(),
        filename: entry.filename,
        file_existed,
    })
}
