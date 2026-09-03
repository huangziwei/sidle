//! The Location scale a KFX bokai writes defines: `location_map $550` and
//! `yj.location_pid_map $621`, which a device reads together.

use bokai::Book;
use bokai::formats::kfx::loader;
use bokai::formats::kfx::position::PositionFragments;
use bokai::model::Format;

const EPUB: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

fn export_kfx(path: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    let mut book = Book::from_bytes(&bytes, Format::Epub).expect("import the fixture");
    let mut sink = std::io::Cursor::new(Vec::new());
    book.export(Format::Kfx, &mut sink).expect("export to KFX");
    Some(sink.into_inner())
}

/// §10.3: the two location fragments state the same boundaries — one as
/// `{id, offset}` coordinates, one as pids — so they must be parallel entry
/// for entry, and the pids must not go backwards.
#[test]
fn both_location_maps_are_written_and_parallel() {
    let Some(kfx) = export_kfx(EPUB) else {
        return; // fixture not present in this checkout
    };
    let book = loader::load(&kfx).expect("re-load the exported KFX");
    let fragments = PositionFragments::from_book(&book);

    let anchors = fragments.location_anchors();
    let pids = fragments.location_pids();
    assert!(!anchors.is_empty(), "the export states location boundaries");
    assert_eq!(
        anchors.len(),
        pids.len(),
        "location_map and yj.location_pid_map must be parallel"
    );
    assert!(
        pids.windows(2).all(|w| w[0] <= w[1]),
        "boundary pids must be non-decreasing"
    );
}

/// A Location is a place in the text, not a paragraph number. Amazon puts 87%
/// of its boundaries at a non-zero offset; a generator that can only start one
/// at a block boundary reports zero here and its Location numbers line up with
/// nothing.
#[test]
fn location_boundaries_sit_inside_the_text() {
    let Some(kfx) = export_kfx(EPUB) else {
        return;
    };
    let book = loader::load(&kfx).expect("re-load the exported KFX");
    let anchors = PositionFragments::from_book(&book).location_anchors();
    let inside = anchors.iter().filter(|(_, off)| *off != 0).count();
    assert!(
        inside * 2 > anchors.len(),
        "most boundaries should sit inside a paragraph, got {inside} of {}",
        anchors.len()
    );
}

/// Every boundary names an element the position scale actually places, at an
/// offset within that element — otherwise the device cannot turn a Location
/// into a reading position.
#[test]
fn every_boundary_resolves_on_the_position_axis() {
    let Some(kfx) = export_kfx(EPUB) else {
        return;
    };
    let mut exported = Book::from_bytes(&kfx, Format::Kfx).expect("re-import the exported KFX");
    let scale = exported.position_map().expect("the export carries a scale");

    let book = loader::load(&kfx).expect("re-load the exported KFX");
    let fragments = PositionFragments::from_book(&book);
    let anchors = fragments.location_anchors();
    let pids = fragments.location_pids();

    for (i, &(eid, offset)) in anchors.iter().enumerate() {
        let resolved = scale.position(eid, offset);
        assert_eq!(
            resolved,
            Some(pids[i]),
            "boundary {i} ({eid}, {offset}) should resolve to the pid the pid map states"
        );
    }
}
