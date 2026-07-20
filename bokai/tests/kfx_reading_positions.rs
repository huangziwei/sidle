//! The reading-position scale a KFX defines, read through the public API.
//!
//! A Kindle addresses text by element id and displays progress as "Loc N of
//! M". Both halves of that chain — the element→coordinate map and the location
//! boundaries — come out of the container, and a device writes the element ids
//! into every annotation it syncs. So the numbers this test pins are not
//! cosmetic: a highlight made on hardware only lands on the right words if the
//! scale read here matches the one the device used.

use std::collections::HashMap;

use bokai::Book;
use bokai::model::Format;

const REFLOWABLE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";
const SHORT: &str = "tests/fixtures/[太宰 治] 人間失格.kfx";
const EPUB: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

/// The chain the mechanical port derives from a fully loaded book, for the
/// same KFX: `eid → pid` plus the Location number each pid falls in.
fn port_chain(kfx: &[u8]) -> (HashMap<i64, i64>, Vec<(i64, i64)>, i64) {
    use bokai::kfx_to_epub::text_index::LocationMap;
    use bokai::kfx_to_epub::{TextIndex, loader};

    let book = loader::load(kfx).expect("port loads the fixture");
    let pid_of = TextIndex::pid_map_from_book(&book);
    let max_pid = pid_of.values().copied().max().unwrap_or(0);
    let lm =
        LocationMap::from_book(&book, &pid_of).unwrap_or_else(|| LocationMap::approximate(max_pid));
    let mut locations: Vec<(i64, i64)> = pid_of
        .iter()
        .map(|(&eid, &pid)| (eid, lm.location_for_pid(pid)))
        .collect();
    locations.sort_unstable();
    (pid_of, locations, lm.count())
}

/// The importer reads only the four position fragments out of the container's
/// entity index; the port parses every fragment in the book. Those two paths
/// must produce the same scale, or a stored `(eid, offset)` annotation resolves
/// to a different place than it did before.
fn assert_matches_port(path: &str) {
    let Ok(kfx) = std::fs::read(path) else {
        return; // fixture not present in this checkout
    };
    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("import the fixture");
    let positions = book
        .position_map()
        .unwrap_or_else(|| panic!("{path} should carry a position map"));

    let (pid_of, locations, count) = port_chain(&kfx);
    assert!(
        !pid_of.is_empty(),
        "{path} should carry positioned elements"
    );
    assert_eq!(positions.positions(), &pid_of, "{path}: eid → pid diverged");
    assert_eq!(
        positions.location_count(),
        count,
        "{path}: location count diverged"
    );
    assert_eq!(
        positions.element_locations(),
        locations,
        "{path}: per-element Location diverged"
    );
}

#[test]
fn reflowable_kfx_positions_match_the_mechanical_chain() {
    assert_matches_port(REFLOWABLE);
}

#[test]
fn short_kfx_positions_match_the_mechanical_chain() {
    assert_matches_port(SHORT);
}

/// Offsets ride on top of an element's coordinate — this is what turns an
/// annotation's `(eid, offset)` handle into a point on the scale.
#[test]
fn an_offset_advances_from_the_elements_coordinate() {
    let Ok(kfx) = std::fs::read(REFLOWABLE) else {
        return;
    };
    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("import the fixture");
    let positions = book.position_map().expect("position map");

    let (&eid, &pid) = positions
        .positions()
        .iter()
        .min_by_key(|&(_, &p)| p)
        .expect("at least one positioned element");
    assert_eq!(positions.position(eid, 0), Some(pid));
    assert_eq!(positions.position(eid, 7), Some(pid + 7));
    assert_eq!(
        positions.position(i64::MAX, 0),
        None,
        "an element outside the source's addressable text has no coordinate"
    );
}

/// The scale is reported, not invented. EPUB addresses text structurally and
/// defines no linear coordinate, so its importer reports none rather than
/// synthesizing one — how a reader shows progress for such a book is the
/// consumer's policy.
#[test]
fn epub_defines_no_reading_position_scale() {
    let Ok(epub) = std::fs::read(EPUB) else {
        return;
    };
    let mut book = Book::from_bytes(&epub, Format::Epub).expect("import the fixture");
    assert!(book.position_map().is_none());
}
