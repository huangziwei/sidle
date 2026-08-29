//! Handwritten-ink ingest: `bokai::formats::nbk` decodes an `nbk` to per-page
//! SVG, and [`import_ink`] stores the renders against the host book.
//!
//! One `handwritten_note` per drawn page sits in the host book's `.yjr`. Its
//! anchor handle resolves to a host PDF page through the `eid → page` map, and
//! its inline body is the page-container `kfx_id`.
//!
//! The `nbk` is `.notebooks/<asin>!!PDOC!!notebook/nbk`, `asin` being the baked
//! content_id. An ink page joins its `handwritten_note` by
//! `page.container_id == note body`, and display-sorts by the note's `linear`.
//!
//! [`import_ink`] takes `nbk` bytes and parsed `handwritten_note` records from
//! its caller, the shape `import_yjr` takes.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::library::db::{self, NewBookInk};
use crate::library::ingest::{CollectedYjr, DeviceImportReport};
use crate::library::pdf_geom::{self, PageGeom};
use crate::library::yjr::{self, Annotation};
use crate::library::{LibraryPaths, import};

/// Outcome of one ink import.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct InkImportStats {
    /// Ink pages written or refreshed (one per drawn page in the nbk).
    pub pages: usize,
    /// Of `pages`, those whose anchor resolved to a PDF page. The remainder are
    /// stored page-unanchored.
    pub anchored: usize,
}

/// Decode one sideloaded doc's ink notebook against its host book: `notes`
/// name each page by `container_id`, `kfx_path` gives the `eid → page` map,
/// and the raw `nbk` backs up under `books/<sha>/ink/<asin>/`.
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

    // `bokai::formats::nbk::open` takes a path: a KDF notebook is a SQLite file.
    paths
        .ensure_book_ink(book_sha, asin)
        .context("create ink dir")?;
    let nbk_path = paths.book_ink_nbk(book_sha, asin);
    std::fs::write(&nbk_path, nbk_bytes)
        .with_context(|| format!("write nbk backup {}", nbk_path.display()))?;

    let nb = bokai::formats::nbk::open(&nbk_path)
        .map_err(|e| anyhow::anyhow!("decode ink notebook for {asin}: {e:?}"))?;

    // The `eid → page` map and each page box, keyed by `kfx_sha`. An unreadable
    // `kfx_path` leaves `geom` empty.
    let geom: Vec<PageGeom> = match (kfx_path, kfx_sha) {
        (Some(p), Some(sha)) => pdf_geom::ensure(paths, book_sha, Path::new(p), sha),
        (Some(p), None) => pdf_geom::compute_from_file(Path::new(p)),
        (None, _) => Vec::new(),
    };
    let eid_page = eid_page_map(&geom);

    // container id (a note's inline body) → the host anchor that names it.
    let note_by_container: HashMap<&str, &Annotation> = notes
        .iter()
        .filter_map(|n| n.body.as_deref().map(|body| (body, n)))
        .collect();

    let mut stats = InkImportStats::default();
    let mut current_containers: Vec<String> = Vec::with_capacity(nb.pages.len());

    for (i, page) in nb.pages.iter().enumerate() {
        let cid = page.container_id.as_str();
        current_containers.push(cid.to_string());
        // `DELETION_INK` blocks the re-add of a removed page.
        if db::is_deleted(conn, db::DELETION_INK, &db::ink_deletion_key(asin, cid))
            .context("check ink deletion record")?
        {
            continue;
        }
        let anchor = note_by_container.get(cid).and_then(|n| n.start());
        let host_eid = anchor.map(|h| h.eid);
        let host_linear = anchor.map(|h| h.position);
        let host_page = host_eid
            .and_then(|e| eid_page.get(&e).copied())
            .map(|p| p as i64);

        // Both renders: the transparent overlay, and the white-background page.
        if let Some(svg) = nb.page_overlay_svg(i) {
            // A Scribe fits the page to the screen and centres it, filling a
            // sub-rectangle of the ink canvas. `crop_overlay_to_page` takes it.
            let svg = match host_page.and_then(|hp| geom.get(hp as usize)) {
                Some(pg) if pg.box_w > 0.0 && pg.box_h > 0.0 => crop_overlay_to_page(
                    &svg,
                    page.canvas_width,
                    page.canvas_height,
                    pg.box_w,
                    pg.box_h,
                ),
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

    // `device_serial`'s asserted set, provenance only. `None` carries no identity.
    if let Some(serial) = device_serial {
        db::record_ink_device_presence(conn, serial, asin, book_id, &current_containers, now)
            .context("record ink device presence")?;
    }

    Ok(stats)
}

/// One pulled ink notebook: the host book's content_id (`== books.asin`) and
/// the raw `nbk` bytes.
pub struct CollectedInk {
    pub asin: String,
    pub nbk_bytes: Vec<u8>,
}

/// Every handwritten-ink record across the collected `.yjr`s, each naming an
/// ink page by `container_id` and carrying its host-page position.
/// [`import_ink`] selects the records belonging to one `nbk`.
pub fn handwritten_notes(collected: &[CollectedYjr]) -> Vec<Annotation> {
    collected
        .iter()
        .filter_map(|c| c.yjr_bytes.as_deref())
        .flat_map(yjr::parse)
        .filter(|a| matches!(a.kind, yjr::Kind::Handwritten(_)))
        .collect()
}

/// Import a sync's worth of pulled ink, folding the counts into `report`.
/// Each notebook joins its host book by `asin == books.asin`, and an `nbk`
/// matching the `ink_sync` checkpoint skips the decode. Call under a held lock.
#[allow(clippy::too_many_arguments)]
pub fn import_collected_ink(
    conn: &Connection,
    paths: &LibraryPaths,
    device_serial: &str,
    now: &str,
    inks: &[CollectedInk],
    notes: &[Annotation],
    report: &mut DeviceImportReport,
    on_ink: &dyn Fn(usize, usize, &str),
) -> Result<()> {
    let total = inks.len();
    for (i, CollectedInk { asin, nbk_bytes }) in inks.iter().enumerate() {
        let Some(book_id) = db::book_id_by_asin(conn, asin)? else {
            continue; // host book not in the library (yet) — relink on a later sync
        };
        let Some(book) = db::get_book(conn, book_id)? else {
            continue;
        };
        on_ink(i + 1, total, &book.title);
        let nbk_sha = import::sha256_of_bytes(nbk_bytes);
        if db::get_ink_sync_sha(conn, device_serial, asin)?.as_deref() == Some(nbk_sha.as_str()) {
            report.ink_unchanged += 1;
            continue; // unchanged nbk — nothing to re-decode
        }
        let stats = import_ink(
            conn,
            paths,
            Some(book_id),
            &book.sha256,
            asin,
            book.kfx_path.as_deref(),
            book.kfx_sha256.as_deref(),
            nbk_bytes,
            notes,
            Some(device_serial),
            now,
        )?;
        db::set_ink_sync_sha(conn, device_serial, asin, &nbk_sha, now)?;
        report.ink_books += 1;
        report.ink_pages += stats.pages;
    }
    Ok(())
}

/// Re-link orphan ink (`book_id IS NULL`) whose `asin` matches a library book,
/// returning the rows linked. Pairs with
/// [`crate::library::ingest::relink_unmatched`] on a book add or edit.
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

/// Delete one ink page: its `book_ink` row, device presence and deletion
/// record through [`db::delete_book_ink`], and the cached SVGs under the sha
/// its `book_id` names. An absent id makes no change.
pub fn delete_ink_page(conn: &Connection, paths: &LibraryPaths, id: i64) -> Result<()> {
    let Some(row) = db::get_book_ink(conn, id).context("read ink row")? else {
        return Ok(());
    };
    db::delete_book_ink(conn, id).context("delete ink row")?;
    if let Some(book_id) = row.book_id
        && let Some(book) = db::get_book(conn, book_id).context("read host book")?
    {
        let _ = std::fs::remove_file(paths.book_ink_overlay_svg(
            &book.sha256,
            &row.asin,
            &row.container_id,
        ));
        let _ = std::fs::remove_file(paths.book_ink_plain_svg(
            &book.sha256,
            &row.asin,
            &row.container_id,
        ));
    }
    Ok(())
}

/// eid → 0-based host page. [`PageGeom::eids`] unions each page's text-run and
/// structural eids, and the first page listing an eid wins.
fn eid_page_map(geom: &[PageGeom]) -> HashMap<i64, usize> {
    let mut map = HashMap::new();
    for (page_index, pg) in geom.iter().enumerate() {
        for &eid in &pg.eids {
            map.entry(eid).or_insert(page_index);
        }
    }
    map
}

/// Crop an overlay SVG's viewBox from the ink canvas to the sub-rectangle the
/// host page occupies on a Scribe screen. The band's aspect equals the page
/// box's; ink in the letterbox margins falls outside the viewBox.
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
    // These fixtures carry no `nbk` and no KFX, the two `import_ink` decodes.
    // Covered here: `relink_orphan_ink` by asin, and `crop_overlay_to_page`.
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
                amazon_asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
                title_romaji: "",
                author_romaji: "",
                source_format: None,
            },
        )
        .unwrap()
    }

    /// A `.yjr` carrying one handwritten-ink record naming `container`, beside
    /// a bookmark.
    fn yjr_with_ink_note(container: &str) -> Vec<u8> {
        use crate::library::yjr::{Anchor, Kind, Store};
        let mut store = Store::empty();
        store.merge_annotations(&[
            Annotation {
                kind: Kind::Bookmark,
                anchors: vec![Anchor::new(10, 0, 9)],
                body: None,
                color: None,
                created_ms: Some(0),
                modified_ms: Some(0),
            },
            Annotation {
                kind: Kind::Handwritten("handwritten_note".into()),
                anchors: vec![Anchor::new(11, 0, 42)],
                body: Some(container.to_string()),
                color: None,
                created_ms: Some(0),
                modified_ms: Some(0),
            },
        ]);
        store.encode()
    }

    /// `collect_ink_notes` unions the ink records of every `.yjr` in the
    /// collection into one flat list across books.
    #[test]
    fn handwritten_notes_unions_the_ink_records_and_drops_the_rest() {
        let collected = vec![
            CollectedYjr {
                sdr_name: "a.deadbeef.sdr".into(),
                yjr_bytes: Some(yjr_with_ink_note("container-a")),
                yjf_bytes: None,
                yjr_name: None,
                yjf_name: None,
            },
            CollectedYjr {
                sdr_name: "b.cafef00d.sdr".into(),
                yjr_bytes: Some(yjr_with_ink_note("container-b")),
                yjf_bytes: None,
                yjr_name: None,
                yjf_name: None,
            },
            // A book with no `.yjr`.
            CollectedYjr {
                sdr_name: "c.12345678.sdr".into(),
                yjr_bytes: None,
                yjf_bytes: None,
                yjr_name: None,
                yjf_name: None,
            },
        ];
        let notes = handwritten_notes(&collected);
        assert_eq!(notes.len(), 2, "the bookmarks are not ink");
        let mut bodies: Vec<&str> = notes.iter().filter_map(|n| n.body.as_deref()).collect();
        bodies.sort_unstable();
        assert_eq!(bodies, ["container-a", "container-b"]);
        assert!(
            notes
                .iter()
                .all(|n| matches!(n.kind, yjr::Kind::Handwritten(_)))
        );
    }

    /// The join keys on `container_id`, across whichever book a note came from.
    /// A notebook whose host book is absent from `books` is skipped.
    #[test]
    fn import_collected_ink_skips_an_asin_with_no_host_book() {
        let conn = mem_db();
        add_book(&conn, "sha-a", "KNOWNASIN");
        let mut report = DeviceImportReport::default();
        import_collected_ink(
            &conn,
            &LibraryPaths {
                root: std::env::temp_dir().join("sidle-ink-no-host"),
            },
            "G000TESTSERIAL",
            "t0",
            &[CollectedInk {
                asin: "NOSUCHASIN".into(),
                nbk_bytes: b"not-a-real-nbk".to_vec(),
            }],
            &[],
            &mut report,
            &|_, _, _| {},
        )
        .expect("an unmatched asin is a skip, not an error");
        assert_eq!(report.ink_books, 0);
        assert_eq!(report.ink_pages, 0);
        // `get_ink_sync_sha` holds nothing, leaving the notebook on offer.
        assert_eq!(
            db::get_ink_sync_sha(&conn, "G000TESTSERIAL", "NOSUCHASIN").unwrap(),
            None
        );
    }

    #[test]
    fn crop_overlay_letterboxes_a_narrower_page() {
        // canvas 15624×20832 (0.75), page box 442×663 (0.667): narrower page,
        // fit by height, band x0=868, width=13888 (= 20832·442/663).
        let svg = "<svg xmlns=\"x\" viewBox=\"0 0 15624 20832\"><image/></svg>";
        let out = crop_overlay_to_page(svg, 15624, 20832, 442.0, 663.0);
        assert!(out.contains("viewBox=\"868 0 13888 20832\""), "got: {out}");
        assert!(out.contains("<image/>"), "ink content untouched");
    }

    #[test]
    fn crop_overlay_is_noop_when_aspects_match() {
        // Page aspect == canvas aspect: the page fills the canvas, no crop.
        let svg = "<svg viewBox=\"0 0 1000 2000\"/>";
        let out = crop_overlay_to_page(svg, 1000, 2000, 500.0, 1000.0);
        assert!(out.contains("viewBox=\"0 0 1000 2000\""), "got: {out}");
    }

    #[test]
    fn crop_overlay_letterboxes_a_wider_page() {
        // canvas 2000×2000 (1.0), page 1000×500 (2.0): wider page, fit by
        // width, ph = 2000/2 = 1000, y0 = (2000-1000)/2 = 500.
        let svg = "<svg viewBox=\"0 0 2000 2000\"/>";
        let out = crop_overlay_to_page(svg, 2000, 2000, 1000.0, 500.0);
        assert!(out.contains("viewBox=\"0 500 2000 1000\""), "got: {out}");
    }

    #[test]
    fn relink_ink_links_orphans_to_their_book_by_asin() {
        let conn = mem_db();
        // Ink with no matching row in `books`.
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
        // With no matching book, `relink_orphan_ink` links 0 rows.
        assert_eq!(relink_ink(&conn).unwrap(), 0);
        assert_eq!(db::list_unlinked_book_ink(&conn).unwrap().len(), 1);

        // A book with that asin: `relink_orphan_ink` attaches the ink.
        let book = add_book(&conn, "sha", "AS1");
        assert_eq!(relink_ink(&conn).unwrap(), 1);
        assert!(db::list_unlinked_book_ink(&conn).unwrap().is_empty());
        assert_eq!(db::list_book_ink(&conn, book).unwrap().len(), 1);
    }
}
