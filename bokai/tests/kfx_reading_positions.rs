//! The reading-position scale a KFX defines, read through the public API.
//!
//! A Kindle addresses text by element id and displays progress as "Loc N of
//! M". Both halves of that chain — the element→coordinate map and the location
//! boundaries — come out of the container, and a device writes the element ids
//! into every annotation it syncs. So the numbers this test pins are not
//! cosmetic: a highlight made on hardware only lands on the right words if the
//! scale read here matches the one the device used.

mod common;

use bokai::Book;
use bokai::model::Format;

const REFLOWABLE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";
const SHORT: &str = "tests/fixtures/[太宰 治] 人間失格.kfx";
const EPUB: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

/// The whole chain as one line per positioned element: `eid → pid → Location`.
fn chain(kfx: &[u8]) -> (usize, i64, u64) {
    let mut book = Book::from_bytes(kfx, Format::Kfx).expect("import the fixture");
    let positions = book.position_map().expect("fixture carries a position map");
    let locations = positions.element_locations();
    assert!(!locations.is_empty(), "fixture carries positioned elements");
    let lines = locations.iter().map(|(eid, loc)| {
        let pid = positions.positions().get(eid).copied().unwrap_or(-1);
        format!("{eid}\t{pid}\t{loc}")
    });
    (
        locations.len(),
        positions.location_count(),
        common::digest_lines(lines),
    )
}

#[test]
fn reflowable_kfx_positions_are_pinned() {
    let Ok(kfx) = std::fs::read(REFLOWABLE) else {
        return; // fixture not present in this checkout
    };
    let (elements, locations, digest) = chain(&kfx);
    assert_eq!((elements, locations), (1918, 8117), "scale shape");
    assert_eq!(digest, 0xc9a7_9b52_4a0d_b8e0, "eid → pid → Location moved");
}

#[test]
fn short_kfx_positions_are_pinned() {
    let Ok(kfx) = std::fs::read(SHORT) else {
        return;
    };
    let (elements, locations, digest) = chain(&kfx);
    assert_eq!((elements, locations), (881, 1703), "scale shape");
    assert_eq!(digest, 0x8e79_38a3_8a9e_e15f, "eid → pid → Location moved");
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
