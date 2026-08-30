//! Pull a Scribe's handwritten ink — the pen strokes drawn *on* a sideloaded

use std::collections::HashSet;

use anyhow::Result;

use crate::library::device::{TPath, Transport};
use crate::library::ink::CollectedInk;

/// `.notebooks/` child suffix marking a sideloaded-doc ink notebook.
const PDOC_SUFFIX: &str = "!!PDOC!!notebook";

/// Walk the device `.notebooks/` for OUR sideloaded-doc ink
pub fn collect_device_ink(
    transport: &dyn Transport,
    known_asins: &HashSet<String>,
) -> Result<Vec<CollectedInk>> {
    let root = TPath::parse(".notebooks");
    // Resolve `.notebooks` ONCE and pull every OUR `nbk` by handle in one
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
