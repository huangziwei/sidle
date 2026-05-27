//! Regression: HUFF/CDIC-compressed AZW3 must import and decompress correctly.
//!
//! `[太宰 治] 人間失格.azw3` is a real Amazon AZW3 that uses HUFF/CDIC Huffman
//! compression. Before the u64 rewrite of `mobi::huffcdic`, `Book::open` panicked
//! here (`attempt to add with overflow` in `Azw3Importer::extract_text`); the
//! prior u32 layout also truncated the code-space thresholds and would have
//! decoded garbage. This guards both ends: the book imports, and the text it
//! decompresses is real Japanese (correct kanji in the TOC, not mojibake).

use boko::Book;
use boko::model::TocEntry;

fn collect_titles(entries: &[TocEntry], out: &mut Vec<String>) {
    for e in entries {
        out.push(e.title.clone());
        collect_titles(&e.children, out);
    }
}

#[test]
fn huff_azw3_imports_and_decompresses_toc_text() {
    // Opening drives Azw3Importer::extract_text → HuffCdicReader over the
    // HUFF/CDIC records — the call that panicked before the u64 rewrite.
    let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.azw3")
        .expect("HUFF/CDIC AZW3 must import without panicking");
    let _ = book.resolve_links();

    let mut titles = Vec::new();
    collect_titles(book.toc(), &mut titles);
    let joined = titles.join("\n");

    for heading in ["はしがき", "第一の手記", "第二の手記", "第三の手記", "あとがき"] {
        assert!(
            joined.contains(heading),
            "HUFF decode lost TOC heading {heading}; got titles: {titles:?}"
        );
    }
}

/// The TOC test above passes even with a subtly broken decoder, because the
/// short TOC strings happened to land on correctly-decoded codes. The real
/// regression is in the *body*: an off-by-one in the `mincode`/`maxcode`
/// tables mis-thresholded some Huffman codes, so the dictionary returned the
/// wrong phrase intermittently — most text stayed correct, but periodic runs
/// turned to mojibake (and unrelated CSS/attribute fragments spliced in).
///
/// This decodes every chapter's reconstructed HTML and asserts that passages
/// from the previously-corrupted regions come through intact. Each is a long
/// contiguous run of plain body text (no ruby tags), so a single wrong code
/// anywhere inside it breaks the match. Verified byte-for-byte against
/// calibre's own HUFF/CDIC reader on the same fixture.
#[test]
fn huff_azw3_body_text_decodes_without_corruption() {
    let mut book =
        Book::open("tests/fixtures/[太宰 治] 人間失格.azw3").expect("HUFF/CDIC AZW3 must import");

    let ids: Vec<_> = book.spine().iter().map(|e| e.id).collect();
    let mut body = String::new();
    for id in ids {
        let raw = book.load_raw(id).expect("load chapter HTML");
        body.push_str(&String::from_utf8_lossy(&raw));
    }

    for passage in [
        "もちろん一ばん下の座でしたが、その食事の部屋は薄暗く",
        "クラスで最も貧弱な肉体をして、顔も青ぶくれで",
        "教練や体操はいつも見学という白痴に似た生徒でした",
    ] {
        assert!(
            body.contains(passage),
            "HUFF body decode corrupted; clean passage missing: {passage}"
        );
    }
}
