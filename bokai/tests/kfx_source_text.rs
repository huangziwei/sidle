//! The base text a KFX stores per element, read through the public API.
//!
//! A Kindle's annotation file records only `(element, offset)` endpoints — it
//! carries none of the highlighted words. Those are recovered by slicing this
//! text. So every assertion here is really about stored user data: if the text
//! an element contributes changes, or the elements fall in a different reading
//! order, then the same highlight covers different words.

use bokai::Book;
use bokai::model::Format;

const REFLOWABLE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";
const SHORT: &str = "tests/fixtures/[太宰 治] 人間失格.kfx";
const EPUB: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

/// The IR path walks storyline entities out of the container's index and
/// resolves content references one entity at a time; the mechanical port
/// parses the book's entire fragment graph first. Both must recover the same
/// characters for the same element, and place those elements in the same
/// reading order — the two inputs a highlight's text is sliced from.
fn assert_matches_port(path: &str) {
    let Ok(kfx) = std::fs::read(path) else {
        return; // fixture not present in this checkout
    };
    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("import the fixture");
    let positions = book.position_map().expect("position map");
    let text = book.source_text().expect("source text");
    let port = bokai::kfx_to_epub::TextIndex::from_kfx(&kfx).expect("port index");

    assert_eq!(text.len(), port.len(), "{path}: indexed element count");

    // Both directions across the positioned set: an element the IR path
    // *omits* would silently blank a highlight, which one direction misses.
    for &element in text.reading_order() {
        assert_eq!(
            text.text_of(element),
            port.text_of(element),
            "{path}: base text diverged for element {element}"
        );
    }
    for &element in positions.positions().keys() {
        assert_eq!(
            text.text_of(element),
            port.text_of(element),
            "{path}: base text diverged for positioned element {element}"
        );
    }

    // Ranges exercise the reading-order walk itself, not just per-element
    // text: a divergent order shows up here and nowhere above.
    let order = text.reading_order();
    assert!(!order.is_empty(), "{path}: nothing indexed");
    let mut ranges = 0usize;
    for pair in order.windows(2).step_by(97) {
        let (a, b) = (pair[0], pair[1]);
        assert_eq!(
            text.extract(a, 0, b, 3),
            port.extract(a, 0, b, 3),
            "{path}: adjacent range {a}..{b} diverged"
        );
        ranges += 1;
    }
    for i in (0..order.len().saturating_sub(50)).step_by(211) {
        let (a, b) = (order[i], order[i + 50]);
        assert_eq!(
            text.extract(a, 2, b, 4),
            port.extract(a, 2, b, 4),
            "{path}: 50-element range {a}..{b} diverged"
        );
        ranges += 1;
    }
    // The whole book as one range — the strongest single comparison there is.
    let (first, last) = (order[0], order[order.len() - 1]);
    assert_eq!(
        text.extract(first, 0, last, usize::MAX),
        port.extract(first, 0, last, usize::MAX),
        "{path}: whole-book extraction diverged"
    );
    assert!(ranges > 0, "{path}: no ranges compared");
}

#[test]
fn reflowable_kfx_base_text_matches_the_mechanical_index() {
    assert_matches_port(REFLOWABLE);
}

#[test]
fn short_kfx_base_text_matches_the_mechanical_index() {
    assert_matches_port(SHORT);
}

/// An out-of-range handle is best-effort, never a panic: a device can sync an
/// annotation whose offsets no longer fit the book it was made against.
#[test]
fn a_malformed_handle_clamps_instead_of_panicking() {
    let Ok(kfx) = std::fs::read(SHORT) else {
        return;
    };
    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("import the fixture");
    let text = book.source_text().expect("source text");
    let first = text.reading_order()[0];

    assert_eq!(
        text.extract(first, usize::MAX, first, usize::MAX)
            .as_deref(),
        Some("")
    );
    assert_eq!(text.extract(i64::MAX, 0, first, 1), None);
    assert_eq!(text.extract(first, 0, i64::MAX, 1), None);
}

/// EPUB carries its text in the content documents themselves and has no
/// element-id namespace to key this on, so its importer reports none.
#[test]
fn epub_has_no_source_text_index() {
    let Ok(epub) = std::fs::read(EPUB) else {
        return;
    };
    let mut book = Book::from_bytes(&epub, Format::Epub).expect("import the fixture");
    assert!(book.source_text().is_none());
}
