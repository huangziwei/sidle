//! KRDS sidecars read off real Kindles, held to a byte-exact round trip.
//!
//! Writing a sidecar back to a device means handing firmware a file that also
//! carries records this crate has no opinion about, so "re-encodes unchanged"
//! is the property that makes writing safe at all. The fixtures are real device
//! output; they contain anchors, timestamps and font preferences only — a
//! sidecar never stores the text an annotation covers.

use bokai::formats::krds::{Anchor, Annotation, Kind, Store};

const COLORED: &[u8] = include_bytes!("fixtures/krds/colored_highlights.yjr");
const MONOCHROME: &[u8] = include_bytes!("fixtures/krds/monochrome_highlights.yjr");
const NO_ANNOTATIONS: &[u8] = include_bytes!("fixtures/krds/no_annotations.yjr");
const READING_STATE: &[u8] = include_bytes!("fixtures/krds/reading_state.yjf");

#[test]
fn every_device_sidecar_re_encodes_byte_for_byte() {
    for (name, bytes) in [
        ("colored_highlights.yjr", COLORED),
        ("monochrome_highlights.yjr", MONOCHROME),
        ("no_annotations.yjr", NO_ANNOTATIONS),
        ("reading_state.yjf", READING_STATE),
    ] {
        let store = Store::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(store.encode(), bytes, "{name} did not survive a round trip",);
    }
}

#[test]
fn a_colorsoft_sidecar_yields_its_highlight_colours() {
    let anns = Store::parse(COLORED).unwrap().annotations();
    assert_eq!(anns.len(), 4);
    assert!(anns.iter().all(|a| a.kind == Kind::Highlight));
    // In the order the device stored them.
    let colors: Vec<Option<&str>> = anns.iter().map(|a| a.color.as_deref()).collect();
    assert_eq!(
        colors,
        vec![Some("pink"), Some("yellow"), Some("blue"), Some("orange")],
    );
    // The colour is a colour, not a note — the two share a shape in the file.
    assert!(
        anns.iter().all(|a| a.body.is_none()),
        "a colour must not be mistaken for a note body",
    );
    assert_eq!(anns[0].start().unwrap().eid, 980);
    assert_eq!(anns[0].start().unwrap().position, 438);
}

#[test]
fn a_monochrome_sidecar_names_no_colour_at_all() {
    let anns = Store::parse(MONOCHROME).unwrap().annotations();
    assert!(!anns.is_empty());
    assert!(
        anns.iter().all(|a| a.color.is_none()),
        "absence means the device had nothing to say, not that it meant yellow",
    );
}

#[test]
fn a_read_but_unmarked_book_has_no_annotations() {
    let store = Store::parse(NO_ANNOTATIONS).unwrap();
    assert!(store.annotations().is_empty());
    // `annotation.cache.object` must not be mistaken for an annotation record.
    assert!(store.root("annotation.cache.object").is_some());
}

#[test]
fn the_reading_state_sidecar_carries_a_last_read_position() {
    let store = Store::parse(READING_STATE).unwrap();
    let lpr = store.position("lpr").expect("a last-read position");
    assert!(lpr.eid > 0 && lpr.position > 0);
    assert!(store.position("fpr").is_some());
    assert!(store.position("no.such.record").is_none());
}

/// The push path's core promise: adding a highlight changes the annotation
/// cache and nothing else in the file.
#[test]
fn merging_a_highlight_leaves_every_other_record_untouched() {
    let mut store = Store::parse(COLORED).unwrap();
    let before: Vec<String> = store.roots.iter().map(|o| o.name.clone()).collect();
    let untouched = store.root("font.prefs").cloned();

    let added = store.merge_annotations(&[Annotation::highlight(
        Anchor::new(1200, 0, 5000),
        Anchor::new(1200, 40, 5040),
        1_786_188_500_000,
        Some("orange"),
    )]);
    assert_eq!(added, 1);

    let anns = store.annotations();
    assert_eq!(anns.len(), 5);
    assert_eq!(
        anns.last().unwrap().color.as_deref(),
        Some("orange"),
        "our colour rides along",
    );
    assert_eq!(
        store
            .roots
            .iter()
            .map(|o| o.name.clone())
            .collect::<Vec<_>>(),
        before,
        "no record added or dropped at the top level",
    );
    assert_eq!(store.root("font.prefs").cloned(), untouched);
    // And the result is still a well-formed sidecar.
    let bytes = store.encode();
    assert_eq!(Store::parse(&bytes).unwrap(), store);
}

/// Re-pushing what the device already has must be a no-op, or every sync would
/// grow the file.
#[test]
fn merging_what_is_already_there_adds_nothing() {
    let mut store = Store::parse(COLORED).unwrap();
    let existing = store.annotations();
    let before = store.encode();
    let added = store.merge_annotations(&existing);
    assert_eq!(added, 0);
    assert_eq!(
        store.encode(),
        before,
        "an idempotent merge rewrites nothing"
    );
}
