//! Handwritten-ink ingest: decode a sideloaded doc's ink notebook (`nbk`) and
//! store its per-page renders against the host book.
//!
//! This is the **third concern** in the annotation system (see
//! `.claude/plans/scribe-handwritten-annotations.md`): the pen strokes the user
//! drew *on top of* a book Sidle pushed. It reuses both existing subsystems —
//! **A** (`yjr`/`anchor`) supplies the host-page anchor + the per-page link, and
//! **B** (`boko::kfx::nbk`) decodes the strokes to SVG — and adds only the join
//! and the book-keyed storage.
//!
//! The verified mechanism (device truth):
//!   - the host book's `.yjr` carries one `handwritten_note` per drawn page; each
//!     record's anchor handle resolves to a host PDF page (via the same
//!     `eid → page` map the reader builds), and its inline *body* is the ink
//!     notebook's page-container `kfx_id` — the explicit per-page link;
//!   - the ink notebook is `.notebooks/<asin>!!PDOC!!notebook/nbk`, where `asin`
//!     is the book's baked content_id; `boko::kfx::nbk::open` decodes every page
//!     (deltas included), each carrying its `container_id`;
//!   - so each ink page joins to its host anchor by `page.container_id == note
//!     body`, and the pages are display-sorted by the note's `linear` (NOT the
//!     nbk's page/creation order).
//!
//! This module owns the join + caching; it does NOT pull off the device (the
//! caller hands it the already-collected `nbk` bytes + the parsed
//! `handwritten_note` records — exactly as the text path hands `import_yjr` the
//! parsed annotations).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::library::LibraryPaths;
use crate::library::db::{self, NewBookInk};
use crate::library::pdf_geom::{self, PageGeom};
use crate::library::yjr::Annotation;

/// Outcome of one ink import.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct InkImportStats {
    /// Ink pages written or refreshed (one per drawn page in the nbk).
    pub pages: usize,
    /// Of those, pages whose host anchor resolved to a PDF page (the rest are
    /// stored page-unanchored — gallery-only — until a KFX / matching `.yjr`
    /// note appears).
    pub anchored: usize,
}

/// Decode one sideloaded doc's ink notebook and store its pages against the host
/// book, joining each ink page to its host-page anchor.
///
/// - `notes` are the book's `.yjr` `handwritten_note` records ([`crate::library::yjr::Kind::Handwritten`]);
///   each names its ink page via its inline body (`== container_id`) and carries
///   the host-page anchor `(eid, linear)`.
/// - `kfx_path` is the host KFX, read for the `eid → page` map + page boxes;
///   `kfx_sha` keys the cached geometry sidecar ([`crate::library::pdf_geom`]) so
///   a warm sync is a few-KB JSON read, not a full KFX re-parse. `None`/
///   unreadable → pages store page-unanchored.
/// - the raw `nbk` is backed up under `books/<sha>/ink/<asin>/` (so the ink
///   survives a device wipe) and each page is rendered to a transparent overlay
///   SVG (for the reader) plus a white-bg plain SVG (for the gallery), cached
///   beside it ([[feedback_derived_assets_at_import]]).
/// - idempotent on `(asin, container_id)`; orphan-capable (`book_id` may be
///   `None`); records device presence when `device_serial` is set, but never
///   deletes a backup page (mirrors [`crate::library::ingest::import_yjr`]).
#[allow(clippy::too_many_arguments)]
pub fn import_ink(
    conn: &Connection,
    paths: &LibraryPaths,
    book_id: Option<i64>,
    book_sha: &str,
    asin: &str,
    kfx_path: Option<&str>,
    kfx_sha: Option<&str>,
    nbk_bytes: &[u8],
    notes: &[Annotation],
    device_serial: Option<&str>,
    now: &str,
) -> Result<InkImportStats> {
    let nbk_sha = sha256_hex(nbk_bytes);

    // The nbk must be on disk to decode (a KDF notebook IS a SQLite file). Write
    // the backup first — it's required regardless (the ink must outlive a device
    // wipe) — then decode from it.
    paths.ensure_book_ink(book_sha, asin).context("create ink dir")?;
    let nbk_path = paths.book_ink_nbk(book_sha, asin);
    std::fs::write(&nbk_path, nbk_bytes)
        .with_context(|| format!("write nbk backup {}", nbk_path.display()))?;

    let nb = boko::kfx::nbk::open(&nbk_path)
        .map_err(|e| anyhow::anyhow!("decode ink notebook for {asin}: {e:?}"))?;

    // The host KFX's per-page anchor geometry: the eid → host-page map (the very
    // map the reader uses to place annotations) AND each page's box size, to
    // align the ink overlay to the page. Served from the geometry sidecar (cached
    // by kfx_sha) so this is a few-KB JSON read on a warm cache, not the ~0.5–
    // 1.4 s full-KFX parse that made every sync slow. Best-effort: no/unreadable
    // KFX → empty → pages stay page-unanchored.
    let geom: Vec<PageGeom> = match (kfx_path, kfx_sha) {
        (Some(p), Some(sha)) => pdf_geom::ensure(paths, book_sha, Path::new(p), sha),
        (Some(p), None) => pdf_geom::compute_from_file(Path::new(p)),
        (None, _) => Vec::new(),
    };
    let eid_page = eid_page_map(&geom);

    // container id (a note's inline body) → the host anchor that names it.
    let note_by_container: HashMap<&str, &Annotation> = notes
        .iter()
        .filter_map(|n| n.note_body.as_deref().map(|body| (body, n)))
        .collect();

    let mut stats = InkImportStats::default();
    let mut current_containers: Vec<String> = Vec::with_capacity(nb.pages.len());

    for (i, page) in nb.pages.iter().enumerate() {
        let cid = page.container_id.as_str();
        current_containers.push(cid.to_string());
        // Honor a Sidle-side deletion: don't re-add an ink page the user removed
        // in Sidle (Restore from device clears the record). Presence above keeps
        // provenance accurate — the device still holds the page.
        if db::is_deleted(conn, db::DELETION_INK, &db::ink_deletion_key(asin, cid))
            .context("check ink deletion record")?
        {
            continue;
        }
        let anchor = note_by_container.get(cid).and_then(|n| n.start());
        let host_eid = anchor.map(|h| h.eid as i64);
        let host_linear = anchor.map(|h| h.linear as i64);
        let host_page = host_eid.and_then(|e| eid_page.get(&e).copied()).map(|p| p as i64);

        // Cache both renders: the transparent overlay (reader) and the white-bg
        // plain page (gallery / standalone view).
        if let Some(svg) = nb.page_overlay_svg(i) {
            // Align the overlay to its host page: the Scribe shows the page
            // fit-to-screen (centered), so the page is a sub-rectangle of the ink
            // canvas. Crop the overlay's viewBox to that sub-rect so the ink lands
            // on the page without the horizontal squish a flat canvas→page stretch
            // causes (it pushed margin ink into the text).
            let svg = match host_page.and_then(|hp| geom.get(hp as usize)) {
                Some(pg) if pg.box_w > 0.0 && pg.box_h > 0.0 => {
                    crop_overlay_to_page(&svg, page.canvas_width, page.canvas_height, pg.box_w, pg.box_h)
                }
                _ => svg,
            };
            std::fs::write(paths.book_ink_overlay_svg(book_sha, asin, cid), svg)
                .context("write ink overlay svg")?;
        }
        if let Some(svg) = nb.page_svg(i) {
            std::fs::write(paths.book_ink_plain_svg(book_sha, asin, cid), svg)
                .context("write ink plain svg")?;
        }

        db::upsert_book_ink(
            conn,
            &NewBookInk {
                book_id,
                asin,
                container_id: cid,
                host_page,
                host_eid,
                host_linear,
                nbk_sha256: Some(&nbk_sha),
                imported_at: now,
            },
        )
        .context("upsert book_ink row")?;

        stats.pages += 1;
        if host_page.is_some() {
            stats.anchored += 1;
        }
    }

    // Record this device's asserted set (provenance only — never deletes a backup
    // page; Sidle is the durable backup). A deviceless import carries no identity.
    if let Some(serial) = device_serial {
        db::record_ink_device_presence(conn, serial, asin, book_id, &current_containers, now)
            .context("record ink device presence")?;
    }

    Ok(stats)
}

/// Re-link orphan ink (`book_id IS NULL`) whose `asin` now matches a library
/// book. Run after a book is added/edited (the safety net for ink that landed
/// before its host book existed, or got unlinked by `ON DELETE SET NULL`).
/// Returns the number of rows linked. Mirrors [`crate::library::ingest::relink_unmatched`].
pub fn relink_ink(conn: &Connection) -> rusqlite::Result<usize> {
    let orphans = db::list_unlinked_book_ink(conn)?;
    let mut linked = 0;
    for o in orphans {
        if let Some(book_id) = db::book_id_by_asin(conn, &o.asin)? {
            db::set_book_ink_book_id(conn, o.id, book_id)?;
            linked += 1;
        }
    }
    Ok(linked)
}

/// Delete one ink page from the library: its `book_ink` row + device-presence +
/// a deletion record (via [`db::delete_book_ink`]), plus the cached overlay/plain
/// SVGs. The book sha for the cache path comes from the row's `book_id`. A no-op
/// if the id is already gone. Used by the reader's annotations-panel delete.
pub fn delete_ink_page(conn: &Connection, paths: &LibraryPaths, id: i64) -> Result<()> {
    let Some(row) = db::get_book_ink(conn, id).context("read ink row")? else {
        return Ok(());
    };
    db::delete_book_ink(conn, id).context("delete ink row")?;
    if let Some(book_id) = row.book_id
        && let Some(book) = db::get_book(conn, book_id).context("read host book")?
    {
        let _ =
            std::fs::remove_file(paths.book_ink_overlay_svg(&book.sha256, &row.asin, &row.container_id));
        let _ =
            std::fs::remove_file(paths.book_ink_plain_svg(&book.sha256, &row.asin, &row.container_id));
    }
    Ok(())
}

/// eid → 0-based host page. The cached [`PageGeom::eids`] is already the union of
/// each page's text-run eids and structural eids (image / container /
/// page_template) — the same set the reader's `buildPdfEidIndex` registers — so
/// the first page that lists an eid wins, matching the reader.
fn eid_page_map(geom: &[PageGeom]) -> HashMap<i64, usize> {
    let mut map = HashMap::new();
    for (page_index, pg) in geom.iter().enumerate() {
        for &eid in &pg.eids {
            map.entry(eid).or_insert(page_index);
        }
    }
    map
}

/// Crop an overlay SVG's viewBox from the full ink canvas to the sub-rectangle
/// the host page occupies on the Scribe screen. The device renders the page
/// fit-to-screen (uniform scale, centered): a page narrower than the screen sits
/// in a horizontally-centered band of the canvas (letterboxed left/right), a
/// wider one in a vertically-centered band. The cropped band's aspect equals the
/// page box's, so the overlay — placed to fill the page box — aligns with the
/// page instead of being squished by a flat canvas→page stretch (which shoved
/// margin ink inward). Ink that fell in the screen letterbox margins (off-page)
/// lands outside the viewBox and is clipped by the reader's `overflow:hidden`.
fn crop_overlay_to_page(svg: &str, canvas_w: i64, canvas_h: i64, box_w: f32, box_h: f32) -> String {
    if canvas_w <= 0 || canvas_h <= 0 || box_w <= 0.0 || box_h <= 0.0 {
        return svg.to_string();
    }
    let (cw, ch) = (canvas_w as f64, canvas_h as f64);
    let page_aspect = box_w as f64 / box_h as f64;
    let canvas_aspect = cw / ch;
    let (x0, y0, pw, ph) = if page_aspect < canvas_aspect {
        // Page narrower than screen → fit by height; letterbox left/right.
        let pw = ch * page_aspect;
        ((cw - pw) / 2.0, 0.0, pw, ch)
    } else {
        // Page wider (or equal) → fit by width; letterbox top/bottom.
        let ph = cw / page_aspect;
        (0.0, (ch - ph) / 2.0, cw, ph)
    };
    let old = format!("viewBox=\"0 0 {canvas_w} {canvas_h}\"");
    let new = format!("viewBox=\"{x0:.0} {y0:.0} {pw:.0} {ph:.0}\"");
    svg.replacen(&old, &new, 1)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    // `import_ink` itself decodes a real KDF `nbk` (a SQLite file) + a real KFX,
    // so its end-to-end correctness is verified by the gitignored device-data
    // harness in `artifacts/` (per the no-gitignored-test-data rule), not here.
    // These cover the pure DB-side logic: orphan-then-relink by asin.
    use super::*;
    use crate::library::db::{self, NewBook, NewBookInk};
    use std::path::Path;

    fn mem_db() -> Connection {
        db::open(Path::new(":memory:")).unwrap()
    }

    fn add_book(conn: &Connection, sha: &str, asin: &str) -> i64 {
        db::insert_book(
            conn,
            &NewBook {
                sha256: sha,
                title: "Linear Models with R",
                author: "",
                language: "",
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: None,
                kfx_sha256: None,
                pdf_path: None,
                file_size: 0,
                imported_at: "t0",
                asin: Some(asin),
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
                title_romaji: "",
                author_romaji: "",
            },
        )
        .unwrap()
    }

    #[test]
    fn crop_overlay_letterboxes_a_narrower_page() {
        // The real Linear numbers: canvas 15624×20832 (0.75), page box 442×663
        // (0.667). The page is narrower → fit by height → a centered horizontal
        // band x0=868, width=13888 (= 20832·442/663), full height.
        let svg = "<svg xmlns=\"x\" viewBox=\"0 0 15624 20832\"><image/></svg>";
        let out = crop_overlay_to_page(svg, 15624, 20832, 442.0, 663.0);
        assert!(out.contains("viewBox=\"868 0 13888 20832\""), "got: {out}");
        assert!(out.contains("<image/>"), "ink content untouched");
    }

    #[test]
    fn crop_overlay_is_noop_when_aspects_match() {
        // Page aspect == canvas aspect → the page fills the canvas; no crop.
        let svg = "<svg viewBox=\"0 0 1000 2000\"/>";
        let out = crop_overlay_to_page(svg, 1000, 2000, 500.0, 1000.0);
        assert!(out.contains("viewBox=\"0 0 1000 2000\""), "got: {out}");
    }

    #[test]
    fn crop_overlay_letterboxes_a_wider_page() {
        // A page WIDER than the screen → fit by width → vertical letterbox.
        // canvas 2000×2000 (1.0), page 1000×500 (2.0): ph = 2000/2 = 1000,
        // y0 = (2000-1000)/2 = 500.
        let svg = "<svg viewBox=\"0 0 2000 2000\"/>";
        let out = crop_overlay_to_page(svg, 2000, 2000, 1000.0, 500.0);
        assert!(out.contains("viewBox=\"0 500 2000 1000\""), "got: {out}");
    }

    #[test]
    fn relink_ink_links_orphans_to_their_book_by_asin() {
        let conn = mem_db();
        // Ink imported as an orphan (host book not in the library yet).
        db::upsert_book_ink(
            &conn,
            &NewBookInk {
                book_id: None,
                asin: "AS1",
                container_id: "c0",
                host_page: None,
                host_eid: None,
                host_linear: Some(1),
                nbk_sha256: Some("s"),
                imported_at: "t0",
            },
        )
        .unwrap();
        // No book yet → relink is a no-op.
        assert_eq!(relink_ink(&conn).unwrap(), 0);
        assert_eq!(db::list_unlinked_book_ink(&conn).unwrap().len(), 1);

        // Book appears with that asin → relink attaches the ink.
        let book = add_book(&conn, "sha", "AS1");
        assert_eq!(relink_ink(&conn).unwrap(), 1);
        assert!(db::list_unlinked_book_ink(&conn).unwrap().is_empty());
        assert_eq!(db::list_book_ink(&conn, book).unwrap().len(), 1);
    }
}
