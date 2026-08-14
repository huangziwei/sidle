//! Pull Scribe handwritten notebooks off the connected Kindle through the
//! [`Transport`] abstraction — the device-side counterpart to the manual
//! `.notebooks/` folder import (`commands::notebook::import_dir`), and the
//! default path now that the toolbar's Import button targets the device.
//!
//! Mirrors [`crate::library::device::annotations`]: the slow USB scan + reads run BEFORE
//! the DB lock is taken, then each notebook is imported under it.
//!
//! On the device, notebooks live at `.notebooks/<uuid>/nbk`, with cover
//! thumbnails in `.notebooks/thumbnails/<uuid>.png`. Only standalone
//! (dashed-uuid) notebooks are imported; the `!!PDOC!!` / `!!EBOK!!` annotation
//! notebooks are skipped, matching the folder import. Whether the device exposes
//! `.notebooks/` over MTP at all is up to its responder — an empty result means
//! it doesn't, or there are none.

use anyhow::{Context, Result};

use crate::library::device::{TPath, Transport};
use crate::library::notebook::{self, ImportSummary, NotebookOutcome};
use crate::library::{LibraryPaths, db};

/// One notebook pulled off the device, ready to import.
struct Pulled {
    uuid: String,
    nbk: Vec<u8>,
    cover: Option<Vec<u8>>,
    /// The `nbk`'s on-device "Date Modified" (naive ISO) — the notebook's
    /// `updated_at`. Falls back to the import time if the device omitted it.
    updated_at: String,
}

/// The standalone notebook directories under `.notebooks/`, identified by their
/// dashed-UUID name. This excludes the device's bookkeeping dirs (`thumbnails/`,
/// `page_cache/`, `clipboard/`, `.backups/`, `.tmp/`) and the `…!!PDOC!!` /
/// `…!!EBOK!!` annotation notebooks WITHOUT a `list` round-trip into each — a
/// single `list` of `.notebooks/` is enough. (The old "list every child to look
/// for an `nbk`" approach listed `page_cache/` too, which is large and slow over
/// USB; that needless work is what made every import drag.)
fn list_candidates(transport: &dyn Transport, root: &TPath) -> Result<Vec<String>> {
    Ok(transport
        .list(root)?
        .into_iter()
        .filter(|e| e.is_dir && is_notebook_uuid(&e.name))
        .map(|e| e.name)
        .collect())
}

/// A standalone Scribe notebook dir is named by a dashed UUID (`8-4-4-4-12` hex),
/// e.g. `da85e6f7-9672-2e2b-ef94-e57fc3502e45`. None of the device's bookkeeping
/// dirs or annotation-notebook dirs match this shape.
fn is_notebook_uuid(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Pull one candidate's `nbk` bytes + cover + on-device Date Modified.
/// `Ok(None)` when the dir holds no `nbk` (not actually a notebook). Kept
/// separate from the decode/import so it's unit-testable over mass-storage.
fn pull_one(
    transport: &dyn Transport,
    root: &TPath,
    thumbs: &TPath,
    name: &str,
) -> Result<Option<Pulled>> {
    let dir = root.join(name);
    let listing = transport.list(&dir).unwrap_or_default();
    let Some(nbk_entry) = listing.iter().find(|e| e.name == "nbk") else {
        return Ok(None); // thumbnails/, .backups/, page_cache/, … — not a notebook
    };
    let nbk = transport
        .read(&dir.join("nbk"))
        .with_context(|| format!("read .notebooks/{name}/nbk"))?;
    let updated_at = nbk_entry.modified.clone().unwrap_or_else(db::now_iso);
    // Cover thumbnail is best-effort: absent → the viewer falls back to page 0.
    let cover = transport.read(&thumbs.join(&format!("{name}.png"))).ok();
    Ok(Some(Pulled {
        uuid: name.to_string(),
        nbk,
        cover,
        updated_at,
    }))
}

/// Pull + import every standalone notebook on the device, calling `on_progress`
/// with `(done, total)` after each candidate so the UI can show "Importing N/M…".
/// Blocking (USB + DB IO); call on the blocking pool.
///
/// Pull (USB) and import (decode + DB) are INTERLEAVED per notebook — the DB lock
/// is taken per import and released across the slow USB reads, so the frontend's
/// DB queries aren't stalled for the whole import.
pub fn import_device_notebooks(
    transport: &dyn Transport,
    paths: &LibraryPaths,
    db: &impl db::Access,
    on_progress: &dyn Fn(usize, usize),
) -> Result<ImportSummary> {
    let root = TPath::parse(".notebooks");
    let thumbs = TPath::parse(".notebooks/thumbnails");
    let candidates = list_candidates(transport, &root)?;
    let total = candidates.len();

    let mut summary = ImportSummary {
        imported: 0,
        unchanged: 0,
        failed: Vec::new(),
    };
    for (i, name) in candidates.iter().enumerate() {
        on_progress(i, total);
        match pull_one(transport, &root, &thumbs, name) {
            Ok(Some(p)) => {
                // The connection is taken per import and released across the
                // slow USB read of the next notebook.
                let imported = db.with(|conn| {
                    notebook::import_notebook_bytes(
                        conn,
                        paths,
                        &p.uuid,
                        &p.nbk,
                        p.cover.as_deref(),
                        &p.updated_at,
                    )
                });
                match imported {
                    Ok(NotebookOutcome::Imported(_)) => summary.imported += 1,
                    Ok(NotebookOutcome::Unchanged(_)) => summary.unchanged += 1,
                    Ok(NotebookOutcome::Suppressed) => {} // deleted in Sidle — don't resurrect
                    Err(e) => summary.failed.push(format!("{name}: {e:#}")),
                }
            }
            Ok(None) => {} // not a notebook dir — skip silently
            Err(e) => summary.failed.push(format!("{name}: {e:#}")),
        }
    }
    on_progress(total, total);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::device::mass_storage::transport::MassStorageTransport;

    // Drives the device scan/pull through the mass-storage transport (MTP can't
    // be unit-tested without a device). Confirms `list_candidates` excludes the
    // `!!EBOK!!` annotation notebook, and `pull_one` takes the standalone
    // notebook's bytes + cover (`thumbnails/<uuid>.png`) + file mtime as
    // `updated_at`, while returning `None` for an `nbk`-less cache dir.
    #[test]
    fn scans_and_pulls_standalone_notebooks() {
        let uuid = "da85e6f7-9672-2e2b-ef94-e57fc3502e45";
        let tmp = tempfile::tempdir().unwrap();
        let nb = tmp.path().join(".notebooks");

        let standalone = nb.join(uuid);
        std::fs::create_dir_all(&standalone).unwrap();
        std::fs::write(standalone.join("nbk"), b"NBK-BYTES").unwrap();

        std::fs::create_dir_all(nb.join("thumbnails")).unwrap();
        std::fs::write(nb.join("thumbnails").join(format!("{uuid}.png")), b"PNG").unwrap();

        // Annotation notebook (PDOC/EBOK) — has an `nbk` but must be excluded.
        let annot = nb.join("B009KA3Y6I!!EBOK!!notebook");
        std::fs::create_dir_all(&annot).unwrap();
        std::fs::write(annot.join("nbk"), b"X").unwrap();

        // A non-notebook dir (no `nbk`).
        std::fs::create_dir_all(nb.join("page_cache")).unwrap();

        let transport = MassStorageTransport::new(tmp.path().to_path_buf());
        let root = TPath::parse(".notebooks");
        let thumbs = TPath::parse(".notebooks/thumbnails");

        let candidates = list_candidates(&transport, &root).unwrap();
        assert_eq!(
            candidates,
            vec![uuid.to_string()],
            "only the dashed-UUID notebook is a candidate — thumbnails/, page_cache/, \
             and the !!EBOK!! annotation notebook are all excluded with no per-dir list"
        );

        let p = pull_one(&transport, &root, &thumbs, uuid)
            .unwrap()
            .expect("standalone pulls");
        assert_eq!(p.uuid, uuid);
        assert_eq!(p.nbk, b"NBK-BYTES");
        assert_eq!(
            p.cover.as_deref(),
            Some(&b"PNG"[..]),
            "cover from thumbnails/"
        );
        // `updated_at` came from the file mtime (a real wall-clock year), not the
        // import-time fallback.
        assert!(
            p.updated_at.starts_with("20") && p.updated_at.contains('T'),
            "updated_at should be the nbk's naive-ISO mtime, got {:?}",
            p.updated_at
        );

        // A dir without an `nbk` is not a notebook.
        assert!(
            pull_one(&transport, &root, &thumbs, "page_cache")
                .unwrap()
                .is_none(),
            "page_cache/ has no nbk → not pulled"
        );
    }

    #[test]
    fn uuid_shape_distinguishes_notebooks_from_bookkeeping_dirs() {
        assert!(is_notebook_uuid("da85e6f7-9672-2e2b-ef94-e57fc3502e45"));
        assert!(is_notebook_uuid("7507C10C-D7EB-A652-C030-2090B7BB1660")); // uppercase ok
        for junk in [
            "thumbnails",
            "page_cache",
            "clipboard",
            ".backups",
            ".tmp",
            "B009KA3Y6I!!EBOK!!notebook",
            "da85e6f7-9672-2e2b-ef94-e57fc3502e4",  // 35 chars
            "da85e6f7_9672_2e2b_ef94_e57fc3502e45", // underscores, not dashes
        ] {
            assert!(
                !is_notebook_uuid(junk),
                "{junk} must not look like a notebook uuid"
            );
        }
    }

    #[test]
    fn empty_when_no_notebooks_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let transport = MassStorageTransport::new(tmp.path().to_path_buf());
        assert!(
            list_candidates(&transport, &TPath::parse(".notebooks"))
                .unwrap()
                .is_empty()
        );
    }
}
