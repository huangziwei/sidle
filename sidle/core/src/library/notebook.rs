//! Notebook library entity: Scribe handwritten notebooks backed up + rendered.
//!
//! Storage mirrors `books/<sha>/`: each notebook lives at `notebooks/<uuid>/`
//! holding the raw `nbk` backup, an optional `cover.png` (the device
//! thumbnail), and `pages/page-<n>.svg` — the page renders cached at import
//! time (per [[feedback_derived_assets_at_import]]), so the viewer never
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
}

/// Import a single Scribe notebook into the library from a source `nbk` file
/// (optionally with a device cover thumbnail). Idempotent: re-importing an
/// unchanged notebook (same uuid + content hash, page cache intact) is a no-op
/// returning the existing row; an edited notebook (same uuid, new bytes)
/// re-extracts and replaces the page cache.
pub fn import_notebook(
    conn: &Connection,
    paths: &LibraryPaths,
    uuid: &str,
    src_nbk: &Path,
    src_cover: Option<&Path>,
) -> Result<NotebookOutcome> {
    let bytes =
        std::fs::read(src_nbk).with_context(|| format!("read nbk {}", src_nbk.display()))?;
    let sha = sha256_hex(&bytes);

    if let Some(existing) = db::get_notebook_by_uuid(conn, uuid)? {
        if existing.nbk_sha256.as_deref() == Some(sha.as_str())
            && paths.notebook_page_svg(uuid, 0).exists()
        {
            return Ok(NotebookOutcome::Unchanged(existing));
        }
    }

    // Decode + render every page up front — this is the cached derived asset.
    let notebook = boko::kfx::nbk::open(src_nbk)
        .map_err(|e| anyhow::anyhow!("decode notebook {uuid}: {e:?}"))?;
    let svgs = notebook.page_svgs();

    paths.ensure_notebook(uuid).with_context(|| "create notebook dir")?;
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
    let id = db::upsert_notebook(conn, uuid, svgs.len() as i64, &sha, &now)?;
    let row = db::get_notebook(conn, id)?
        .ok_or_else(|| anyhow::anyhow!("notebook row missing after upsert"))?;
    Ok(NotebookOutcome::Imported(row))
}

/// SHA-256 hex of a byte buffer (same digest shape as `import::sha256_of_file`).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}
