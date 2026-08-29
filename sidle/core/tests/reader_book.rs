//! The reader's view of a book, assembled from the IR.
//!
//! Every field here is one a paginator or an annotation handle depends on, so
//! the whole shape is pinned: a section list that gains an entry, a character
//! count that drifts, a location map that renumbers, all move where a stored
//! highlight lands.
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

/// FNV-1a, 64-bit — stable across toolchains, unlike `DefaultHasher`, which
/// matters because the value is checked in.
fn digest(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

#[test]
fn the_reader_books_shape_is_pinned() {
    let Some(kfx) = fixture() else { return };

    let (book, _store) = ReaderBook::open(&kfx).expect("open the fixture");
    assert!(!book.sections.is_empty(), "no sections — a vacuous pass");

    let mut lines = Vec::new();
    for (i, s) in book.sections.iter().enumerate() {
        let elements: Vec<String> = s.elements.iter().map(|e| e.to_string()).collect();
        lines.push(format!(
            "{i}\t{}\t{}\t{}\t{}\t{}\t{:?}\t{:?}",
            s.href,
            s.chars,
            s.image_only,
            s.image_hrefs.join(","),
            elements.join(","),
            s.viewport,
            s.spread,
        ));
    }
    let locations: Vec<String> = book
        .locations
        .iter()
        .map(|(a, b)| format!("{a}:{b}"))
        .collect();
    lines.push(format!(
        "BOOK\t{}\t{}\t{}\t{:?}\t{:?}\t{}\t{}",
        book.max_location,
        book.fixed_layout,
        locations.join(","),
        book.writing_mode,
        book.page_progression_direction,
        book.title,
        book.language,
    ));

    assert_eq!(book.sections.len(), 9, "section count");
    assert_eq!(
        digest(&lines.join("\n")),
        0x22cd_ce7f_ef9c_de62,
        "the reader book's shape moved"
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
    // the delivered one is read off the bytes. A reader that trusted the
    // prediction would hand a webview a payload it can't decode under a type
    // it can.
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
