//! Pull a Scribe's handwritten ink — the pen strokes drawn *on* a sideloaded
//! doc, stored in `.notebooks/<asin>!!PDOC!!notebook/nbk` — and import it against
//! the host book, in the SAME pass as the annotation sync
//! ([`crate::device::annotations::import_device_annotations`]).
//!
//! Like [`crate::device::annotations`], the device-side walk lives here (the
//! [`Transport`] is an app concept); the decode → join → storage is
//! [`sidle_core::library::ink`]. Ink is a Scribe (MTP) feature, so this runs only
//! on the MTP path — mass-storage Kindles have no handwriting.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::Connection;

use crate::device::{TPath, Transport};
use crate::library::ingest::{CollectedYjr, DeviceImportReport};
use crate::library::yjr::{self, Annotation};
use crate::library::{LibraryPaths, db, import, ink};

/// `.notebooks/` child suffix marking a sideloaded-doc ink notebook.
const PDOC_SUFFIX: &str = "!!PDOC!!notebook";

/// One pulled ink notebook: the host book's content_id (`== books.asin`) + the
/// raw `nbk` bytes.
pub struct CollectedInk {
    pub asin: String,
    pub nbk_bytes: Vec<u8>,
}

/// Walk the device `.notebooks/` for OUR sideloaded-doc ink
/// (`<content_id>!!PDOC!!notebook/nbk`) and pull each `nbk`. `known_asins` is the
/// set of `books.asin` (baked content_ids) in the library; a dir is OURS iff its
/// id is in that set — the content_id's alphabet varies per book (hex *or*
/// Crockford-base32), so this is the only reliable test. Skips Amazon cloud
/// notebooks (a `!!PDOC!!` id not in our library) and standalone notebooks (uuid
/// dirs, no `!!PDOC!!`) — both without a wasted pull. The USB phase: run *before*
/// taking the DB lock (slow `GetObject`s), mirroring [`collect_device_yjr`]. A
/// device that exposes no `.notebooks/` over MTP yields nothing (harmless no-op).
///
/// [`collect_device_yjr`]: crate::device::annotations::collect_device_yjr
pub fn collect_device_ink(
    transport: &dyn Transport,
    known_asins: &HashSet<String>,
) -> Result<Vec<CollectedInk>> {
    let root = TPath::parse(".notebooks");
    let mut out = Vec::new();
    for entry in transport.list(&root).unwrap_or_default() {
        let Some(asin) = pdoc_asin(&entry.name) else {
            continue; // not a `!!PDOC!!` ink dir
        };
        if !known_asins.contains(&asin) {
            continue; // a `!!PDOC!!` dir whose content_id isn't one of our books
        }
        let nbk = root.join(&entry.name).join("nbk");
        match transport.read(&nbk) {
            Ok(bytes) if !bytes.is_empty() => out.push(CollectedInk { asin, nbk_bytes: bytes }),
            _ => {} // a PDOC dir with no readable `nbk` — skip
        }
    }
    Ok(out)
}

/// The content_id of a `<id>!!PDOC!!notebook` dir (sideloaded-doc ink), or `None`
/// for any other entry (a standalone-notebook uuid dir, etc.). Whether the id is
/// OURS is decided by the caller via the library's asin set — NOT a hex test (our
/// content_ids are hex for some books, Crockford-base32 for others).
fn pdoc_asin(dir_name: &str) -> Option<String> {
    dir_name
        .strip_suffix(PDOC_SUFFIX)
        .filter(|id| !id.is_empty())
        .map(|id| id.to_string())
}

/// Every `handwritten_note` record across the collected `.yjr`s (the union). The
/// container-id join in [`ink::import_ink`] selects the right notes per nbk, so we
/// needn't associate a note with a specific book here — a stray note from another
/// book simply won't match this nbk's page containers.
pub fn handwritten_notes(collected: &[CollectedYjr]) -> Vec<Annotation> {
    collected
        .iter()
        .filter_map(|c| c.yjr_bytes.as_deref())
        .flat_map(yjr::parse)
        .filter(|a| a.kind == yjr::Kind::Handwritten)
        .collect()
}

/// Import the pulled ink into the library (under an already-held DB lock),
/// folding the counts into `report`. Each notebook joins to its host book by
/// `asin == books.asin`; an unmatched asin is skipped (the book isn't in the
/// library — it'll sync next connect once it's added). An unchanged `nbk` (same
/// content sha) skips the decode + raster re-render via the `ink_sync`
/// checkpoint, exactly like the `.yjr` fast path.
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
        let stats = ink::import_ink(
            conn,
            paths,
            Some(book_id),
            &book.sha256,
            asin,
            book.kfx_path.as_deref(),
            nbk_bytes,
            notes,
            Some(device_serial),
            now,
        )?;
        db::set_ink_sync_sha(conn, device_serial, asin, &nbk_sha, now)?;
        report.ink_books += 1;
        report.ink_pages += stats.pages;
        report.ink_removed += stats.removed;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mass_storage::transport::MassStorageTransport;

    #[test]
    fn pdoc_asin_extracts_the_content_id_regardless_of_alphabet() {
        // Hex content_id (e.g. Linear Models).
        assert_eq!(
            pdoc_asin("97870D063206CBA0CDD733367F356508!!PDOC!!notebook").as_deref(),
            Some("97870D063206CBA0CDD733367F356508")
        );
        // Crockford-base32 content_id (most of our PDF→KFX pushes) — ALSO extracted;
        // whether it's ours is the asin-set test, not a hex check.
        assert_eq!(
            pdoc_asin("LXOGKNCCHUP7BXFVEMCJPWBQHRP6HXOP!!PDOC!!notebook").as_deref(),
            Some("LXOGKNCCHUP7BXFVEMCJPWBQHRP6HXOP")
        );
        // A standalone notebook (uuid dir, no !!PDOC!!) — not a PDOC ink dir.
        assert_eq!(pdoc_asin("a1b2c3d4-e5f6-7890-abcd-ef0123456789"), None);
    }

    #[test]
    fn collect_device_ink_pulls_only_library_asins() {
        let tmp = tempfile::tempdir().unwrap();
        let nb = tmp.path().join(".notebooks");
        let mk = |dir: &str, body: &[u8]| {
            let d = nb.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("nbk"), body).unwrap();
        };
        // Two of OUR books (one hex content_id, one base32) + a cloud doc + a
        // standalone notebook.
        mk("97870D063206CBA0CDD733367F356508!!PDOC!!notebook", b"HEX");
        mk("LXOGKNCCHUP7BXFVEMCJPWBQHRP6HXOP!!PDOC!!notebook", b"B32");
        mk("42OY5GRNMOZLFZAAJQJISFR4KHOYIPW2!!PDOC!!notebook", b"CLOUD");
        std::fs::create_dir_all(nb.join("a1b2c3d4-e5f6-7890-abcd-ef0123456789")).unwrap();

        let known: HashSet<String> = [
            "97870D063206CBA0CDD733367F356508".to_string(),
            "LXOGKNCCHUP7BXFVEMCJPWBQHRP6HXOP".to_string(),
        ]
        .into_iter()
        .collect();

        let transport = MassStorageTransport::new(tmp.path().to_path_buf());
        let mut got = collect_device_ink(&transport, &known).unwrap();
        got.sort_by(|a, b| a.asin.cmp(&b.asin));
        assert_eq!(got.len(), 2, "both library books pulled; cloud + uuid skipped");
        assert_eq!(got[0].asin, "97870D063206CBA0CDD733367F356508");
        assert_eq!(got[0].nbk_bytes, b"HEX");
        assert_eq!(got[1].asin, "LXOGKNCCHUP7BXFVEMCJPWBQHRP6HXOP");
    }
}
