//! The reader's view of a book, assembled from the IR.

use sidle_core::reader::ReaderBook;

/// bokai's KFX fixture, relative to this crate. Absent in a stand-alone
/// checkout of sidle-core, in which case these tests skip.
const FIXTURE: &str = "../../bokai/tests/fixtures/[太宰 治] 人間失格.kfx";

fn fixture() -> Option<Vec<u8>> {
    std::fs::read(FIXTURE).ok()
}

#[test]
fn the_reader_books_shape_holds_together() {
    let Some(kfx) = fixture() else { return };

    let (book, _store) = ReaderBook::open(&kfx).expect("open the fixture");
    assert!(!book.sections.is_empty(), "no sections — a vacuous pass");

    // Book-level facts the source states outright.
    assert_eq!(book.sections.len(), 9, "section count");
    assert_eq!(book.title, "人間失格");
    assert_eq!(book.language, "ja");
    assert!(!book.fixed_layout, "a novel is reflowable");
    assert_eq!(format!("{:?}", book.writing_mode), "\"vertical-rl\"");
    assert_eq!(
        format!("{:?}", book.page_progression_direction),
        "\"rtl\"",
        "a vertical-rl book turns pages right to left"
    );

    // The cover leads and carries a picture instead of text; every other
    // section carries text. A section with neither is a section that renders
    // blank.
    let cover = &book.sections[0];
    assert!(cover.image_only, "the first section is the cover");
    assert_eq!(cover.chars, 0, "an image-only section has no text");
    assert!(
        !cover.image_hrefs.is_empty(),
        "an image-only section must name its picture"
    );
    for (i, s) in book.sections.iter().enumerate().skip(1) {
        assert!(!s.href.is_empty(), "section {i} has no href");
        assert!(!s.image_only, "only the cover is image-only, not {i}");
        assert!(s.chars > 0, "section {i} renders no text");
        assert!(
            !s.elements.is_empty(),
            "section {i} carries no addressable element"
        );
    }

    // Section hrefs are distinct — two sections at one href would collide in
    // the reader's own addressing.
    let mut hrefs: Vec<&str> = book.sections.iter().map(|s| s.href.as_str()).collect();
    hrefs.sort_unstable();
    let count = hrefs.len();
    hrefs.dedup();
    assert_eq!(hrefs.len(), count, "two sections share an href");

    // No element is listed twice: an element belongs to exactly one section,
    // which is what `section_of_element` can only report if it is true.
    let mut elements: Vec<i64> = book
        .sections
        .iter()
        .flat_map(|s| s.elements.iter().copied())
        .collect();
    let total = elements.len();
    elements.sort_unstable();
    elements.dedup();
    assert_eq!(
        elements.len(),
        total,
        "an element is listed by two sections"
    );

    // The Location scale, keyed by element in ascending element order — that
    // ordering is what lets the reader binary-search an element's Location.
    assert!(!book.locations.is_empty(), "no locations — a vacuous pass");
    assert!(book.max_location > 0, "the book ends at Location 0");
    let mut previous: Option<i64> = None;
    for &(element, location) in &book.locations {
        if let Some(prev) = previous {
            assert!(
                element > prev,
                "the scale lists element {element} after {prev}, out of order"
            );
        }
        assert!(
            location >= 1,
            "element {element} sits at Location {location}"
        );
        assert!(
            location <= book.max_location,
            "element {element} sits at Location {location}, past the book's {}",
            book.max_location
        );
        assert!(
            elements.binary_search(&element).is_ok(),
            "the scale places element {element}, which no section lists"
        );
        previous = Some(element);
    }
    // The scale covers the book: the last element starts inside the final
    // stretch of it. `max_location` is the axis end, one past the last
    // addressable position, so it sits a little beyond where any element
    // starts — but not a chapter beyond.
    let highest = book.locations.iter().map(|&(_, l)| l).max().unwrap_or(0);
    assert!(
        highest <= book.max_location && highest * 100 >= book.max_location * 99,
        "the last element starts at Location {highest} of {}",
        book.max_location
    );

    // And it places most of the book's elements, not a handful.
    assert!(
        book.locations.len() * 2 >= elements.len(),
        "the scale places {} of {} elements",
        book.locations.len(),
        elements.len()
    );
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
    let got = store.fetch(&href).expect("fetch the first image");
    assert!(!got.bytes.is_empty(), "{href} fetched as empty");

    let batch = store.fetch_many(std::slice::from_ref(&href));
    assert_eq!(batch.len(), 1, "batch fetch dropped a known href");
    assert_eq!(batch[0].bytes, got.bytes, "batch and single fetch disagree");
    assert_eq!(batch[0].mime, got.mime, "batch and single fetch mime");
}

#[test]
fn a_delivered_image_is_labelled_with_what_it_actually_is() {
    let Some(kfx) = fixture() else { return };

    let (book, store) = ReaderBook::open(&kfx).expect("open the fixture");
    if book.images.is_empty() {
        return; // this fixture ships no images
    }

    // The manifest's media type is a prediction made before the bytes exist;
    let hrefs: Vec<String> = book.images.iter().map(|i| i.href.clone()).collect();
    for got in store.fetch_many(&hrefs) {
        assert_eq!(
            got.mime,
            bokai::image::media_type_of(&got.bytes),
            "{} delivered as {} but its bytes are not",
            got.href,
            got.mime
        );
        assert!(
            got.mime.starts_with("image/"),
            "{} delivered as {}",
            got.href,
            got.mime
        );
    }
}
