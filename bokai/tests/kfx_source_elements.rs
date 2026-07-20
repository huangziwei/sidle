//! Source element ids carried on IR nodes, per chapter.
//!
//! A reading device addresses text by element id, and a renderer has to mark up
//! the element a stored `(element, offset)` handle names. The mechanical port
//! recovers those ids by re-scanning `data-eid` out of markup it just
//! serialized; the IR carries them structurally instead. This pins the two
//! against each other, per chapter, so the structural list can replace the
//! scan.

use bokai::Book;
use bokai::model::Format;

const REFLOWABLE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";
const SHORT: &str = "tests/fixtures/[太宰 治] 人間失格.kfx";

fn assert_matches_port(path: &str) {
    let Ok(kfx) = std::fs::read(path) else {
        return; // fixture not present in this checkout
    };

    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("import the fixture");
    let ids: Vec<_> = book.spine().iter().map(|s| s.id).collect();
    let mine: Vec<Vec<i64>> = ids
        .iter()
        .map(|&id| {
            book.load_chapter_cached(id)
                .expect("load chapter")
                .source_elements()
        })
        .collect();

    let port = bokai::kfx_to_epub::kfx_to_reader_book(&kfx).expect("port reader book");
    let theirs: Vec<Vec<i64>> = port.sections.iter().map(|s| s.eids.clone()).collect();

    assert_eq!(
        mine.len(),
        theirs.len(),
        "{path}: chapter count {} vs port section count {}",
        mine.len(),
        theirs.len()
    );
    for (i, (m, t)) in mine.iter().zip(&theirs).enumerate() {
        assert_eq!(
            m,
            t,
            "{path}: chapter {i} ({}) element ids diverged — {} vs {} ids",
            port.sections[i].href,
            m.len(),
            t.len()
        );
    }
}

#[test]
fn reflowable_kfx_chapter_elements_match_the_mechanical_scan() {
    assert_matches_port(REFLOWABLE);
}

#[test]
fn short_kfx_chapter_elements_match_the_mechanical_scan() {
    assert_matches_port(SHORT);
}
