//! Pull a Scribe's handwritten ink — the pen strokes drawn *on* a sideloaded
//! doc, stored in `.notebooks/<asin>!!PDOC!!notebook/nbk` — and import it against
//! the host book, in the SAME pass as the annotation sync
//! ([`crate::library::device::annotations::import_device_annotations`]).
//!
//! Like [`crate::library::device::annotations`], only the device-side walk lives here
//! (the [`Transport`] is an app concept); the decode → join → storage is
//! [`sidle_core::library::ink`], which the LAN server drives too. Ink is a
//! Scribe feature, so this runs only on the MTP path — mass-storage Kindles have
//! no handwriting.

use std::collections::HashSet;

use anyhow::Result;

use crate::library::device::{TPath, Transport};
use crate::library::ink::CollectedInk;

/// `.notebooks/` child suffix marking a sideloaded-doc ink notebook.
const PDOC_SUFFIX: &str = "!!PDOC!!notebook";

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
/// [`collect_device_yjr`]: crate::library::device::annotations::collect_device_yjr
pub fn collect_device_ink(
    transport: &dyn Transport,
    known_asins: &HashSet<String>,
) -> Result<Vec<CollectedInk>> {
    let root = TPath::parse(".notebooks");
    // Resolve `.notebooks` ONCE and pull every OUR `nbk` by handle in one
    // session (see [`Transport::read_files_in_children`]) — not a path-based
    // `read()` per file, which re-walks the whole directory each call (the reason
    // ink sync stayed slow even after pruning orphans down to ~100 entries).
    let pulled = transport.read_files_in_children(
        &root,
        &|name| pdoc_asin(name).is_some_and(|id| known_asins.contains(&id)),
        &|file| file == "nbk",
    )?;
    Ok(pulled
        .into_iter()
        .filter_map(|(dir_name, mut files)| {
            let asin = pdoc_asin(&dir_name)?;
            let nbk_bytes = files.pop().map(|(_, bytes)| bytes)?; // the single "nbk"
            Some(CollectedInk { asin, nbk_bytes })
        })
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::device::mass_storage::transport::MassStorageTransport;

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
        assert_eq!(
            got.len(),
            2,
            "both library books pulled; cloud + uuid skipped"
        );
        assert_eq!(got[0].asin, "97870D063206CBA0CDD733367F356508");
        assert_eq!(got[0].nbk_bytes, b"HEX");
        assert_eq!(got[1].asin, "LXOGKNCCHUP7BXFVEMCJPWBQHRP6HXOP");
    }
}
