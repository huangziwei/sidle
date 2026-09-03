//! Notebook library entity: Scribe handwritten notebooks backed up + rendered.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::library::LibraryPaths;
use crate::library::db::{self, NotebookRow};

/// Tally of importing a set of notebooks — a `.notebooks/` folder, or every
/// notebook on a connected device.
#[derive(Debug, Default, serde::Serialize)]
pub struct ImportSummary {
    pub imported: usize,
    pub unchanged: usize,
    pub failed: Vec<String>,
}

/// Result of importing one notebook directory.
#[derive(Debug)]
pub enum NotebookOutcome {
    /// Newly imported or re-extracted (content changed).
    Imported(NotebookRow),
    /// Same uuid + content hash already present; nothing rewritten.
    Unchanged(NotebookRow),
    /// Skipped: the user deleted this notebook in Sidle (a deletion record
    /// exists), so a device/folder re-import must not resurrect it. "Restore
    /// from device" clears the record.
    Suppressed,
}

/// Import a single Scribe notebook into the library from a source `nbk` file
pub fn import_notebook(
    conn: &Connection,
    paths: &LibraryPaths,
    uuid: &str,
    src_nbk: &Path,
    src_cover: Option<&Path>,
    updated_at: &str,
) -> Result<NotebookOutcome> {
    // Honor a Sidle-side deletion: don't resurrect a notebook the user removed
    // in Sidle (Restore from device clears the record).
    if db::is_deleted(conn, db::DELETION_NOTEBOOK, uuid)
        .context("check notebook deletion record")?
    {
        return Ok(NotebookOutcome::Suppressed);
    }
    let bytes =
        std::fs::read(src_nbk).with_context(|| format!("read nbk {}", src_nbk.display()))?;
    let sha = sha256_hex(&bytes);

    if let Some(existing) = db::get_notebook_by_uuid(conn, uuid)?
        && existing.nbk_sha256.as_deref() == Some(sha.as_str())
        && paths.notebook_page_svg(uuid, 0).exists()
    {
        // Unchanged content — no re-extraction. Still backfill metadata a legacy
        db::backfill_notebook_updated_at(conn, uuid, updated_at)?;
        db::backfill_notebook_default_title(conn, uuid, updated_at)?;
        let refreshed = db::get_notebook_by_uuid(conn, uuid)?.unwrap_or(existing);
        return Ok(NotebookOutcome::Unchanged(refreshed));
    }

    // Decode + render every page up front — this is the cached derived asset.
    let notebook = bokai::formats::nbk::open(src_nbk)
        .map_err(|e| anyhow::anyhow!("decode notebook {uuid}: {e:?}"))?;
    let svgs = notebook.page_svgs();

    paths
        .ensure_notebook(uuid)
        .with_context(|| "create notebook dir")?;
    // Raw backup so the notebook survives a device wipe.
    std::fs::write(paths.notebook_nbk(uuid), &bytes).with_context(|| "write nbk backup")?;
    if let Some(cover) = src_cover {
        // Best-effort: a missing/oddly-named thumbnail just means no cover.
        let _ = std::fs::copy(cover, paths.notebook_cover(uuid));
    }
    // Rewrite the page cache from scratch (an edited notebook may have fewer
    // pages than before, so a stale page-N.svg must not linger).
    let pages_dir = paths.notebook_pages_dir(uuid);
    let _ = std::fs::remove_dir_all(&pages_dir);
    std::fs::create_dir_all(&pages_dir).with_context(|| "create pages dir")?;
    for (i, svg) in svgs.iter().enumerate() {
        std::fs::write(paths.notebook_page_svg(uuid, i), svg)
            .with_context(|| format!("write page {i} svg"))?;
    }

    let now = db::now_iso();
    let id = db::upsert_notebook(conn, uuid, svgs.len() as i64, &sha, &now, updated_at)?;
    let row = db::get_notebook(conn, id)?
        .ok_or_else(|| anyhow::anyhow!("notebook row missing after upsert"))?;
    Ok(NotebookOutcome::Imported(row))
}

/// Import a notebook whose bytes arrived over a wire rather than as a file — an
/// MTP pull from the desktop, a LAN push from the on-device picker.
pub fn import_notebook_bytes(
    conn: &Connection,
    paths: &LibraryPaths,
    uuid: &str,
    nbk: &[u8],
    cover: Option<&[u8]>,
    updated_at: &str,
) -> Result<NotebookOutcome> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("nbk-staging-{}-{n}-{uuid}", std::process::id()));
    std::fs::create_dir_all(&tmp).context("stage notebook tempdir")?;
    let out = (|| {
        let nbk_path = tmp.join("nbk");
        std::fs::write(&nbk_path, nbk).context("stage nbk bytes")?;
        let cover_path = match cover {
            Some(bytes) => {
                let cp = tmp.join("thumbnail.png");
                std::fs::write(&cp, bytes).context("stage cover bytes")?;
                Some(cp)
            }
            None => None,
        };
        import_notebook(
            conn,
            paths,
            uuid,
            &nbk_path,
            cover_path.as_deref(),
            updated_at,
        )
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    out
}

/// Render a stored notebook to a single multi-page PDF — one page per cached
pub fn export_notebook_pdf(paths: &LibraryPaths, uuid: &str, page_count: usize) -> Result<Vec<u8>> {
    if page_count == 0 {
        anyhow::bail!("notebook has no pages");
    }
    let mut svgs = Vec::with_capacity(page_count);
    for i in 0..page_count {
        let p = paths.notebook_page_svg(uuid, i);
        let svg = std::fs::read_to_string(&p)
            .with_context(|| format!("read page {} svg: {}", i + 1, p.display()))?;
        svgs.push(svg);
    }
    bokai::formats::pdf::svgs_to_pdf(&svgs).context("assemble notebook PDF")
}

/// SHA-256 hex of a byte buffer (same digest shape as `import::sha256_of_file`).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Scan `folder` for notebook directories and import each. `folder` may itself
/// be one notebook dir (holds `nbk` directly) or a parent of many — a
/// `.notebooks/` copied off a Scribe is the latter.
pub fn import_folder(conn: &Connection, paths: &LibraryPaths, folder: &Path) -> ImportSummary {
    let mut summary = ImportSummary::default();
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    if folder.join("nbk").is_file() {
        let uuid = folder
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("notebook")
            .to_string();
        candidates.push((uuid, folder.to_path_buf()));
    } else if let Ok(entries) = std::fs::read_dir(folder) {
        for e in entries.flatten() {
            let dir = e.path();
            if dir.join("nbk").is_file()
                && let Some(uuid) = dir.file_name().and_then(|s| s.to_str())
            {
                candidates.push((uuid.to_string(), dir));
            }
        }
    }

    for (uuid, dir) in candidates {
        if uuid.contains("!!") {
            continue;
        }
        let nbk = dir.join("nbk");
        let cover = find_cover(&dir, &uuid);
        let updated_at = folder_updated_at(&nbk);
        match import_notebook(conn, paths, &uuid, &nbk, cover.as_deref(), &updated_at) {
            Ok(NotebookOutcome::Imported(_)) => summary.imported += 1,
            Ok(NotebookOutcome::Unchanged(_)) => summary.unchanged += 1,
            Ok(NotebookOutcome::Suppressed) => {} // deleted in Sidle — don't resurrect
            Err(e) => summary.failed.push(format!("{uuid}: {e:#}")),
        }
    }
    summary
}

/// Locate the device cover thumbnail for a notebook.
fn find_cover(dir: &Path, uuid: &str) -> Option<PathBuf> {
    let mut candidates = vec![dir.join("thumbnail.png")];
    if let Some(parent) = dir.parent() {
        candidates.push(parent.join("thumbnails").join(format!("{uuid}.png")));
        if let Some(grand) = parent.parent() {
            candidates.push(grand.join("thumbnails").join(format!("{uuid}.png")));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// The `nbk` file's mtime as a naive local-wall-clock ISO string, the notebook's
/// `updated_at` for a folder import. Falls back to the import time.
fn folder_updated_at(nbk: &Path) -> String {
    std::fs::metadata(nbk)
        .and_then(|m| m.modified())
        .ok()
        .map(|t| {
            chrono::DateTime::<chrono::Utc>::from(t)
                .with_timezone(&chrono::Local)
                .naive_local()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(db::now_iso)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Decoding a real `nbk` is `bokai::formats::nbk`'s job; what belongs here is
    /// that the staging around it never leaks. A failed decode is the case that
    /// would leave a temp dir behind, so that's the one to check.
    #[test]
    fn import_notebook_bytes_removes_its_staging_dir_when_the_decode_fails() {
        let root = std::env::temp_dir().join("sidle-nbk-staging-test");
        let _ = std::fs::remove_dir_all(&root);
        let paths = LibraryPaths { root };
        paths.ensure().unwrap();
        let conn = db::open(&paths.db()).unwrap();

        let uuid = "da85e6f7-9672-2e2b-ef94-e57fc3502e45";
        let err = import_notebook_bytes(&conn, &paths, uuid, b"not a KDF database", None, "t0")
            .expect_err("garbage bytes are not a notebook");
        assert!(
            format!("{err:#}").contains(uuid),
            "the error names the notebook it was decoding: {err:#}"
        );
        // The staging path is unique per call, so find any that survived by its
        // prefix rather than reconstructing the name.
        let leaked: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("nbk-staging-") && n.ends_with(uuid))
            .collect();
        assert!(
            leaked.is_empty(),
            "staging dir left behind after a failure: {leaked:?}"
        );
        // And nothing was recorded for it, so a later sync can still store it.
        assert!(db::get_notebook_by_uuid(&conn, uuid).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&paths.root);
    }
}
