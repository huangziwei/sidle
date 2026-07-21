//! Per-chapter reader metadata, derived from the IR rather than re-scanned.
//!
//! A reader needs three things about a chapter before it paints anything: how
//! much reading it represents, whether it is a full-page image rather than
//! prose, and which images to fetch. The mechanical route recovers them by
//! scanning the XHTML it just serialized; the IR answers them structurally.
//! This pins the two against each other so the scan can be deleted.

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
    let mine: Vec<_> = ids
        .iter()
        .map(|&id| {
            book.load_chapter_cached(id)
                .expect("load chapter")
                .summary()
        })
        .collect();

    let (port, _store) =
        bokai::kfx_to_epub::kfx_to_reader_book_lazy(&kfx).expect("port reader book");

    assert_eq!(
        mine.len(),
        port.sections.len(),
        "{path}: chapter count {} vs port section count {}",
        mine.len(),
        port.sections.len()
    );
    assert!(
        mine.iter().any(|s| s.text_chars > 0),
        "{path}: no chapter has any text — the comparison would pass vacuously"
    );

    for (i, (m, s)) in mine.iter().zip(&port.sections).enumerate() {
        assert_eq!(
            m.text_chars, s.chars,
            "{path}: chapter {i} ({}) base-text count",
            s.href
        );
        assert_eq!(
            m.image_only, s.image_only,
            "{path}: chapter {i} ({}) image-only flag",
            s.href
        );
        assert_eq!(
            m.images, s.image_hrefs,
            "{path}: chapter {i} ({}) image list",
            s.href
        );
    }
}

#[test]
fn reflowable_kfx_chapter_summaries_match_the_mechanical_scan() {
    assert_matches_port(REFLOWABLE);
}

#[test]
fn short_kfx_chapter_summaries_match_the_mechanical_scan() {
    assert_matches_port(SHORT);
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
