//! Cached per-page anchor geometry for a PDF-backed KFX: the eid→page map and each
//! page's box size, the only things ink import needs from the host KFX.
//! session — next to which the nbk→SVG decode is ~6 ms. Running it per drawn
//! book on every connect, under the DB lock, is what makes a sync take
//! seconds. The geometry is a pure
//! function of the KFX bytes — immutable per `kfx_sha256` — so we cache it as a
//! derived-asset sidecar keyed by that
//! sha: computed once (warmed at conversion, see the worker), read as a few-KB
//! JSON on every sync thereafter.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::library::LibraryPaths;

/// One page's anchor geometry. `eids` is the union of the page's text-run eids
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageGeom {
    /// Page box width in points (the box the overlay crop is relative to).
    pub box_w: f32,
    /// Page box height in points.
    pub box_h: f32,
    /// Every eid registered on this page, sorted+deduped.
    pub eids: Vec<i64>,
}

/// On-disk sidecar shape: the geometry plus the `kfx_sha256` it was derived
/// from, so a reconversion (new KFX bytes → new sha) invalidates it.
#[derive(Serialize, Deserialize)]
struct GeomCache {
    kfx_sha256: String,
    pages: Vec<PageGeom>,
}

/// Compute the per-page geometry from raw KFX bytes (the slow path: a full
/// `page_text_layer` parse). Empty for a reflowable / unreadable KFX.
pub fn compute(kfx_bytes: &[u8]) -> Vec<PageGeom> {
    let Ok(layer) = bokai::formats::kfx::pdf_pages::page_text_layer(kfx_bytes) else {
        return Vec::new();
    };
    layer
        .into_iter()
        .map(|pt| {
            let mut eids: Vec<i64> = pt.runs.iter().map(|r| r.eid).chain(pt.eids).collect();
            eids.sort_unstable();
            eids.dedup();
            PageGeom {
                box_w: pt.box_w,
                box_h: pt.box_h,
                eids,
            }
        })
        .collect()
}

/// Compute from a KFX file, uncached. Used when no `kfx_sha256` is available to
/// key a sidecar (a legacy row the bootstrap hasn't backfilled yet).
pub fn compute_from_file(kfx_path: &Path) -> Vec<PageGeom> {
    std::fs::read(kfx_path)
        .map(|b| compute(&b))
        .unwrap_or_default()
}

/// Write/refresh the geometry sidecar for a book's KFX, stamped with `kfx_sha`.
/// Best-effort caching — a failure just means the next reader/sync recomputes.
pub fn write_sidecar(
    paths: &LibraryPaths,
    book_sha: &str,
    kfx_sha: &str,
    pages: &[PageGeom],
) -> Result<()> {
    let cache = GeomCache {
        kfx_sha256: kfx_sha.to_string(),
        pages: pages.to_vec(),
    };
    let json = serde_json::to_vec(&cache).context("serialize pdf geom")?;
    paths.ensure_sha(book_sha).ok();
    let path = paths.pdf_geom(book_sha);
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// The per-page geometry for a host KFX: the cached sidecar when it matches
/// `kfx_sha`, else parsed from the KFX and cached. Empty if the KFX is unreadable.
pub fn ensure(
    paths: &LibraryPaths,
    book_sha: &str,
    kfx_path: &Path,
    kfx_sha: &str,
) -> Vec<PageGeom> {
    if let Ok(bytes) = std::fs::read(paths.pdf_geom(book_sha))
        && let Ok(cache) = serde_json::from_slice::<GeomCache>(&bytes)
        && cache.kfx_sha256 == kfx_sha
    {
        return cache.pages;
    }
    let Ok(kfx_bytes) = std::fs::read(kfx_path) else {
        return Vec::new();
    };
    let pages = compute(&kfx_bytes);
    let _ = write_sidecar(paths, book_sha, kfx_sha, &pages);
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_round_trips_and_invalidates_on_sha_change() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        let sha = "deadbeef";
        let pages = vec![
            PageGeom {
                box_w: 442.0,
                box_h: 663.0,
                eids: vec![1, 2, 3],
            },
            PageGeom {
                box_w: 442.0,
                box_h: 663.0,
                eids: vec![10, 11],
            },
        ];
        write_sidecar(&paths, sha, "KFXSHA1", &pages).unwrap();

        // A non-existent KFX path: a valid sidecar for the matching sha is still
        // served (the cache hit never touches the KFX).
        let missing = tmp.path().join("nope.kfx");
        let got = ensure(&paths, sha, &missing, "KFXSHA1");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].eids, vec![1, 2, 3]);
        assert_eq!(got[1].box_h, 663.0);

        // A different kfx_sha (reconversion) → stale → recompute, but the KFX is
        // missing → empty (proves it did NOT serve the stale cache).
        let stale = ensure(&paths, sha, &missing, "KFXSHA2");
        assert!(stale.is_empty(), "stale sha must not serve the old cache");
    }
}
