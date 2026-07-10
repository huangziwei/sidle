//! Notebook library entity: Scribe handwritten notebooks backed up + rendered.
//!
//! Storage mirrors `books/<sha>/`: each notebook lives at `notebooks/<uuid>/`
//! holding the raw `nbk` backup, an optional `cover.png` (the device
//! thumbnail), and `pages/page-<n>.svg` — the page renders cached at import
//! time, so the viewer never
//! re-parses the SQLite. Metadata (title, page count, content hash) lives in
//! the `notebooks` DB table. Decode + render come from `boko::kfx::nbk`.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::library::LibraryPaths;
use crate::library::db::{self, NotebookRow};

/// Result of importing one notebook directory.
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
/// (optionally with a device cover thumbnail). `updated_at` is the on-device
/// "Date Modified" the CALLER resolved — the source file's mtime for a folder
/// import, the MTP object's DateModified for a device pull — since only the
/// caller knows where the bytes came from. Idempotent: re-importing an unchanged
/// notebook (same uuid + content hash, page cache intact) is a no-op returning
/// the existing row (backfilling a missing `updated_at`); an edited notebook
/// (same uuid, new bytes) re-extracts and replaces the page cache.
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
        // row may lack: the on-device mtime (rows imported before that column
        // existed) and the default title (rows still on the old 'Notebook'
        // sentinel — the title is the first-import datetime now). Both guard
        // internally, so this is a no-op for an already-populated row.
        db::backfill_notebook_updated_at(conn, uuid, updated_at)?;
        db::backfill_notebook_default_title(conn, uuid, updated_at)?;
        let refreshed = db::get_notebook_by_uuid(conn, uuid)?.unwrap_or(existing);
        return Ok(NotebookOutcome::Unchanged(refreshed));
    }

    // Decode + render every page up front — this is the cached derived asset.
    let notebook = boko::kfx::nbk::open(src_nbk)
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

/// Render a stored notebook to a single multi-page PDF — one page per cached
/// page SVG, read from the import-time render cache so we never re-parse the
/// `nbk`. `page_count` is the notebook row's count. See
/// [`crate::library::pdf_render`] for the SVG→PDF rasterization.
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
    crate::library::pdf_render::svgs_to_pdf(&svgs)
}

/// SHA-256 hex of a byte buffer (same digest shape as `import::sha256_of_file`).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}
