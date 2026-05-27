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
    // HUFF/CDIC records — the call that panicked before the fix.
    let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.azw3")
        .expect("HUFF/CDIC AZW3 must import without panicking");
    let _ = book.resolve_links();

    // The TOC titles are decompressed from the HUFF text. Correct kanji proves
    // the decode is right, not merely non-panicking — a broken decoder yields
    // mojibake.
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
