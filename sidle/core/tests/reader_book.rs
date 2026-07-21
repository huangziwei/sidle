//! The reader's view of a book, checked against the route it replaces.
//!
//! `ReaderBook::open` assembles from the IR what the frozen `kfx_to_epub`
//! reader recovers by re-scanning its own serialized XHTML. Every field a
//! paginator or an annotation handle depends on has to survive that swap, so
//! this compares them field by field on a real book.
//!
//! The fixture is bokai's — this test reads it through a path relative to that
//! crate and skips when it isn't there, so sidle-core still builds and tests
//! standing alone.

use sidle_core::reader::ReaderBook;

/// bokai's KFX fixture, relative to this crate. Absent in a stand-alone
/// checkout of sidle-core, in which case these tests skip.
const FIXTURE: &str = "../../bokai/tests/fixtures/[太宰 治] 人間失格.kfx";

fn fixture() -> Option<Vec<u8>> {
    std::fs::read(FIXTURE).ok()
}

#[test]
fn the_reader_book_matches_the_route_it_replaces() {
    let Some(kfx) = fixture() else { return };

    let (mine, _store) = ReaderBook::open(&kfx).expect("open the fixture");
    let (port, _port_store) =
        bokai::kfx_to_epub::kfx_to_reader_book_lazy(&kfx).expect("port reader book");

    assert_eq!(
        mine.sections.len(),
        port.sections.len(),
        "section count: {} vs port {}",
        mine.sections.len(),
        port.sections.len()
    );
    assert!(!mine.sections.is_empty(), "no sections — a vacuous pass");

    for (i, (m, p)) in mine.sections.iter().zip(&port.sections).enumerate() {
        assert_eq!(m.href, p.href, "section {i} href");
        assert_eq!(m.chars, p.chars, "section {i} ({}) base-text count", p.href);
        assert_eq!(
            m.image_only, p.image_only,
            "section {i} ({}) image-only flag",
            p.href
        );
        assert_eq!(
            m.image_hrefs, p.image_hrefs,
            "section {i} ({}) image list",
            p.href
        );
        assert_eq!(m.elements, p.eids, "section {i} ({}) element ids", p.href);
        assert_eq!(m.viewport, p.viewport, "section {i} ({}) viewport", p.href);
        assert_eq!(m.spread, p.spread, "section {i} ({}) spread", p.href);
    }

    assert_eq!(mine.locations, port.locations, "location map");
    assert_eq!(mine.max_location, port.max_location, "location count");
    assert_eq!(mine.fixed_layout, port.fixed_layout, "fixed-layout flag");
    assert_eq!(mine.writing_mode, port.writing_mode, "writing mode");
    assert_eq!(
        mine.page_progression_direction, port.page_progression_direction,
        "page progression direction"
    );
    assert_eq!(mine.title, port.metadata.title, "title");
    assert_eq!(mine.language, port.metadata.language, "language");
}

#[test]
fn every_addressable_element_resolves_to_a_section() {
    let Some(kfx) = fixture() else { return };

    let (book, _store) = ReaderBook::open(&kfx).expect("open the fixture");
    let index = book.section_of_element();
    assert!(!index.is_empty(), "no elements — a vacuous pass");

    // An element the reader can be asked to scroll to must name a section that
    // exists, or a jump silently goes nowhere.
    for (&element, &section) in &index {
        assert!(
            section < book.sections.len(),
            "element {element} maps to section {section}, past the end"
        );
        assert!(
            book.sections[section].elements.contains(&element),
            "element {element} maps to a section that does not list it"
        );
    }
}

#[test]
fn images_are_described_but_not_loaded_until_asked() {
    let Some(kfx) = fixture() else { return };

    let (book, store) = ReaderBook::open(&kfx).expect("open the fixture");
    if book.images.is_empty() {
        return; // this fixture ships no images
    }

    // The manifest names them; the bytes arrive only on request.
    let href = book.images[0].href.clone();
    let bytes = store.fetch(&href).expect("fetch the first image");
    assert!(!bytes.is_empty(), "{href} fetched as empty");

    let batch = store.fetch_many(std::slice::from_ref(&href));
    assert_eq!(batch.len(), 1, "batch fetch dropped a known href");
    assert_eq!(batch[0].1, bytes, "batch and single fetch disagree");
}
