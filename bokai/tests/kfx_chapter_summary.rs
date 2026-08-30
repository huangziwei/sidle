//! Per-chapter reader metadata, derived from the IR rather than re-scanned.

mod common;

use bokai::Book;
use bokai::model::Format;

const REFLOWABLE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";
const SHORT: &str = "tests/fixtures/[太宰 治] 人間失格.kfx";

/// One line per chapter: characters, image-only flag, image list.
fn summaries(kfx: &[u8]) -> (usize, u64, u64) {
    let mut book = Book::from_bytes(kfx, Format::Kfx).expect("import the fixture");
    let ids: Vec<_> = book.spine().iter().map(|s| s.id).collect();
    let all: Vec<_> = ids
        .iter()
        .map(|&id| {
            book.load_chapter_cached(id)
                .expect("load chapter")
                .summary()
        })
        .collect();
    assert!(
        all.iter().any(|c| c.text_chars > 0),
        "no chapter has any text — the digest would pin nothing"
    );
    let total: u64 = all.iter().map(|c| c.text_chars).sum();
    let lines = all.iter().enumerate().map(|(i, c)| {
        format!(
            "{i}\t{}\t{}\t{}",
            c.text_chars,
            c.image_only,
            c.images.join(",")
        )
    });
    (all.len(), total, common::digest_lines(lines))
}

#[test]
fn reflowable_kfx_chapter_summaries_are_pinned() {
    let Ok(kfx) = std::fs::read(REFLOWABLE) else {
        return; // fixture not present in this checkout
    };
    let (chapters, chars, digest) = summaries(&kfx);
    assert_eq!((chapters, chars), (22, 298113), "summary shape");
    assert_eq!(digest, 0xf576_5090_94cd_5d2c, "chapter summaries moved");
}

#[test]
fn short_kfx_chapter_summaries_are_pinned() {
    let Ok(kfx) = std::fs::read(SHORT) else {
        return;
    };
    let (chapters, chars, digest) = summaries(&kfx);
    assert_eq!((chapters, chars), (9, 73273), "summary shape");
    assert_eq!(digest, 0x1acf_10ff_9431_8d83, "chapter summaries moved");
}

#[test]
fn a_line_break_is_neither_a_character_nor_a_word_boundary() {
    use bokai::model::{Chapter, Node, NodeId, Role};

    // A break renders as a `<br>`, which contributes nothing to text content —
    // so "ab\ncd" reads as four characters, not five.
    let mut chapter = Chapter::new();
    let para = chapter.alloc_node(Node::new(Role::Paragraph));
    chapter.append_child(NodeId::ROOT, para);
    let range = chapter.append_text("ab\ncd");
    let mut text = Node::new(Role::Text);
    text.text = range;
    let text_id = chapter.alloc_node(text);
    chapter.append_child(para, text_id);

    assert_eq!(chapter.summary().text_chars, 4);
}
