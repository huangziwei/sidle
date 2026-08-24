//! The base text a KFX stores per element, read through the public API.
//!
//! A Kindle's annotation file records only `(element, offset)` endpoints — it
//! carries none of the highlighted words. Those are recovered by slicing this
//! text, which makes every assertion here one about stored user data: change
//! the characters an element contributes or the order elements are visited in,
//! and the same highlight covers different words.

mod common;

use bokai::Book;
use bokai::model::Format;

const REFLOWABLE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";
const SHORT: &str = "tests/fixtures/[太宰 治] 人間失格.kfx";
const EPUB: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

/// Every element's text in reading order, plus the whole book as one
/// extraction.
///
/// Two facts ride on this digest and neither is cosmetic: the characters an
/// element contributes, and the order elements are visited in. Change either
/// and the same stored `(element, offset)` pair slices different words.
///
/// The element count and the character count move independently, each worth
/// its own assertion. The walk spans every element the position scale
/// *places*, ones carrying no text of their own included — section wrappers,
/// and headings whose words sit in a child — since a device anchors a
/// highlight at whichever element holds the boundary. They contribute no
/// characters: their number is structural, the character count is the words.
fn base_text(kfx: &[u8]) -> (usize, usize, u64) {
    let mut book = Book::from_bytes(kfx, Format::Kfx).expect("import the fixture");
    let text = book.source_text().expect("source text");
    let order = text.reading_order().to_vec();
    assert!(!order.is_empty(), "nothing indexed");

    let mut lines: Vec<String> = order
        .iter()
        .map(|&e| format!("{e}\t{}", text.text_of(e).unwrap_or_default()))
        .collect();
    // The whole book as one range — this pins the reading-order walk itself,
    // beyond the per-element text.
    let (first, last) = (order[0], order[order.len() - 1]);
    let whole = text
        .extract(first, 0, last, usize::MAX)
        .expect("the whole book is a valid range");
    lines.push(format!("WHOLE\t{whole}"));

    (
        text.len(),
        whole.chars().count(),
        common::digest_lines(lines),
    )
}

#[test]
fn reflowable_kfx_base_text_is_pinned() {
    let Ok(kfx) = std::fs::read(REFLOWABLE) else {
        return; // fixture not present in this checkout
    };
    let (elements, chars, digest) = base_text(&kfx);
    assert_eq!(chars, 288763, "the words a highlight can cover moved");
    assert_eq!(elements, 1918, "placed elements in the walk");
    assert_eq!(
        digest, 0x2aac_1005_afd5_5a97,
        "base text or reading order moved"
    );
}

#[test]
fn short_kfx_base_text_is_pinned() {
    let Ok(kfx) = std::fs::read(SHORT) else {
        return;
    };
    let (elements, chars, digest) = base_text(&kfx);
    assert_eq!(chars, 74025, "the words a highlight can cover moved");
    assert_eq!(elements, 881, "placed elements in the walk");
    assert_eq!(
        digest, 0x5c7c_70ad_a5f0_50ec,
        "base text or reading order moved"
    );
}

/// An out-of-range handle is best-effort, never a panic: a device can sync an
/// annotation whose offsets do not fit the book it was made against.
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
/// element-id namespace to key this on, and its importer reports none.
#[test]
fn epub_has_no_source_text_index() {
    let Ok(epub) = std::fs::read(EPUB) else {
        return;
    };
    let mut book = Book::from_bytes(&epub, Format::Epub).expect("import the fixture");
    assert!(book.source_text().is_none());
}
