//! Source element ids carried on IR nodes, per chapter.
//!
//! A reading device addresses text by element id, and a renderer has to mark up
//! the element a stored `(element, offset)` handle names. The IR carries those
//! ids structurally. If a chapter's list changes — different ids, different
//! order, a different chapter holding an id — then "which page is this
//! highlight on" changes with it, so the per-chapter lists are pinned.

mod common;

use bokai::Book;
use bokai::model::Format;

const REFLOWABLE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";
const SHORT: &str = "tests/fixtures/[太宰 治] 人間失格.kfx";

/// One line per chapter: index, id count, and the ids themselves.
fn elements_digest(kfx: &[u8]) -> (usize, u64) {
    let mut book = Book::from_bytes(kfx, Format::Kfx).expect("import the fixture");
    let ids: Vec<_> = book.spine().iter().map(|s| s.id).collect();
    let lines: Vec<String> = ids
        .iter()
        .enumerate()
        .map(|(i, &id)| {
            let eids = book
                .load_chapter_cached(id)
                .expect("load chapter")
                .source_elements();
            let joined: Vec<String> = eids.iter().map(|e| e.to_string()).collect();
            format!("{i}\t{}\t{}", eids.len(), joined.join(","))
        })
        .collect();
    (lines.len(), common::digest_lines(lines))
}

#[test]
fn reflowable_kfx_chapter_elements_are_pinned() {
    let Ok(kfx) = std::fs::read(REFLOWABLE) else {
        return; // fixture not present in this checkout
    };
    let (chapters, digest) = elements_digest(&kfx);
    assert_eq!(chapters, 22, "chapter count");
    assert_eq!(
        digest, 0xe636_88f5_dc1c_155f,
        "per-chapter source element ids moved"
    );
}

#[test]
fn short_kfx_chapter_elements_are_pinned() {
    let Ok(kfx) = std::fs::read(SHORT) else {
        return;
    };
    let (chapters, digest) = elements_digest(&kfx);
    assert_eq!(chapters, 9, "chapter count");
    assert_eq!(
        digest, 0x31bf_28e0_9fd4_e00b,
        "per-chapter source element ids moved"
    );
}
