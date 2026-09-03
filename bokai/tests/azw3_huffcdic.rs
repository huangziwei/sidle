//! HUFF/CDIC-compressed AZW3 must import and decompress correctly.

use bokai::Book;
use bokai::model::TocEntry;

fn collect_titles(entries: &[TocEntry], out: &mut Vec<String>) {
    for e in entries {
        out.push(e.title.clone());
        collect_titles(&e.children, out);
    }
}

#[test]
fn huff_azw3_imports_and_decompresses_toc_text() {
    // Opening drives Azw3Importer::extract_text → HuffCdicReader over the
    // HUFF/CDIC records, where a `maxcode` computed in 32 bits overflows.
    let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.azw3")
        .expect("HUFF/CDIC AZW3 must import without panicking");
    let _ = book.resolve_links();

    let mut titles = Vec::new();
    collect_titles(book.toc(), &mut titles);
    let joined = titles.join("\n");

    for heading in [
        "はしがき",
        "第一の手記",
        "第二の手記",
        "第三の手記",
        "あとがき",
    ] {
        assert!(
            joined.contains(heading),
            "HUFF decode lost TOC heading {heading}; got titles: {titles:?}"
        );
    }
}

/// The TOC test above can pass with a subtly broken decoder, because its short
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
