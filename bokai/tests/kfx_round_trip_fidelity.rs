//! What a KFX → IR → KFX round trip costs, measured entity by entity.
//!
//! `validate source` asks whether a container is self-consistent; this asks
//! whether it is still the same book. The corpus run lives outside the repo,
//! so this pins the same measures on the fixture.

use bokai::Book;
use bokai::formats::kfx::diff;
use bokai::model::Format;

const FIXTURE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";

fn round_trip(kfx: &[u8]) -> Vec<u8> {
    let mut book = Book::from_bytes(kfx, Format::Kfx).expect("import the fixture");
    let mut sink = std::io::Cursor::new(Vec::new());
    book.export(Format::Kfx, &mut sink).expect("export to KFX");
    sink.into_inner()
}

#[test]
fn a_round_trip_keeps_the_prose_element_ids_and_media() {
    let Ok(kfx) = std::fs::read(FIXTURE) else {
        return; // fixture not present in this checkout
    };
    let out = round_trip(&kfx);
    let d = diff::diff("source", &kfx, "round-trip", &out).expect("diff");

    assert!(
        d.text.identical,
        "prose must survive a round trip: {:?}",
        d.text.divergence
    );
    assert_eq!(
        d.text.zwsp.a, d.text.zwsp.b,
        "no zero-width space may be injected into the prose"
    );

    // Element ids are what a device's stored annotations and reading position
    // resolve through. An id that survives must still name the same text — a
    // reused id pointing at other words sends every highlight somewhere else.
    assert_eq!(
        d.eids.surviving, d.eids.same_text,
        "every surviving element id must still name the same text"
    );
    assert!(
        d.eids.same_text * 10 >= d.eids.count.a * 9,
        "at least 90% of element ids should survive with their text, got {} of {}",
        d.eids.same_text,
        d.eids.count.a
    );

    // No picture is dropped or invented. The fixture stores JPEG, which the
    // export re-encodes into the JPEG-XR plates a device reads, so the bytes
    // are expected to move; a **JPEG-XR** source is instead copied verbatim,
    // which the corpus measures and `a_swapped_media_payload_is_reported`
    // pins.
    assert_eq!(
        d.media.count.a, d.media.count.b,
        "no media file added or dropped"
    );

    // Ruby survives essentially intact. The fixture still loses a handful of
    // readings; that residue is an open item, so this pins it from growing
    // rather than asserting zero.
    assert!(
        d.ruby.lost * 100 <= d.ruby.readings.a,
        "at most 1% of ruby readings may be lost, got {} of {}",
        d.ruby.lost,
        d.ruby.readings.a
    );
}

/// Both location maps are written, parallel, and ordered — and the word
/// segmentation the source stated survives.
#[test]
fn a_round_trip_keeps_the_position_fragments() {
    let Ok(kfx) = std::fs::read(FIXTURE) else {
        return;
    };
    let out = round_trip(&kfx);
    let d = diff::diff("source", &kfx, "round-trip", &out).expect("diff");
    let p = &d.positions;

    assert!(
        p.locations.b > 0,
        "the round trip states location boundaries"
    );
    assert_eq!(
        p.locations.b, p.location_pids.b,
        "location_map and yj.location_pid_map must be parallel"
    );
    assert!(p.pids_ordered.b, "boundary pids must be non-decreasing");
    assert!(
        p.locations_inside_text.b * 2 > p.locations.b,
        "most boundaries must sit inside the text, got {} of {}",
        p.locations_inside_text.b,
        p.locations.b
    );

    // Carrying `word_boundary_list` through is measured on the Amazon corpus,
    // where 99.98% survive. This fixture is bokai's own older output and its
    // lists sum to one less than the text they describe, so the exporter
    // correctly refuses most of them — a list that does not sum to the span
    // segments the wrong characters. What must hold here is that nothing is
    // invented for an element the source never segmented.
    assert!(
        p.word_boundaries.b <= p.word_boundaries.a,
        "no word_boundary_list may be invented: {} → {}",
        p.word_boundaries.a,
        p.word_boundaries.b
    );
}
