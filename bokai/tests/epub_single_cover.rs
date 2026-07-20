//! Regression: a converted EPUB must contain exactly ONE cover page.
//!
//! The raw passthrough route synthesizes an SVG `cover.xhtml` from the cover
//! image whenever metadata names one. A calibre-lineage source already ships
//! that exact page as its own `titlepage.xhtml`, so synthesizing on top of it
//! put two cover pages in the reading flow — both `<title>Cover</title>`, both
//! `<meta name="calibre:cover">`, both rendering the same image, both in the
//! spine. Apple Books and Kindle then open on a cover, and the reader pages
//! forward into an identical one.
//!
//! The two fixtures cover both branches: the EPUB ships its own cover page (so
//! synthesis must be skipped), the AZW3 does not (so synthesis must still
//! happen). A fix that silently stopped emitting covers would pass the first
//! assertion alone.

use std::io::Cursor;

use bokai::Exporter as _;

/// Convert `fixture` through the default (passthrough) route and return the
/// EPUB bytes.
fn convert(fixture: &str) -> Vec<u8> {
    let mut book = bokai::Book::open(fixture).expect("open fixture");
    let mut buf = Cursor::new(Vec::<u8>::new());
    bokai::EpubExporter::new()
        .export(&mut book, &mut buf)
        .expect("export to epub");
    buf.into_inner()
}

/// Every XHTML document in the package that declares itself a cover page.
fn cover_pages(epub: &[u8]) -> Vec<String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(epub)).expect("read epub zip");
    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    let mut found = Vec::new();
    for name in names {
        if !(name.ends_with(".xhtml") || name.ends_with(".html")) {
            continue;
        }
        let mut file = zip.by_name(&name).expect("open zip entry");
        let mut text = String::new();
        std::io::Read::read_to_string(&mut file, &mut text).ok();
        if text.contains(r#"name="calibre:cover""#) {
            found.push(name);
        }
    }
    found.sort();
    found
}

#[test]
fn source_with_its_own_cover_page_gets_no_second_one() {
    // This fixture's `OEBPS/titlepage.xhtml` is already the SVG cover wrapper.
    let epub = convert("tests/fixtures/[太宰 治] 人間失格.epub");
    let covers = cover_pages(&epub);
    assert_eq!(
        covers.len(),
        1,
        "expected exactly one cover page, got {covers:?}"
    );
    assert!(
        covers[0].ends_with("titlepage.xhtml"),
        "the source's own cover page should survive, not a synthesized one: {covers:?}"
    );
}

#[test]
fn source_without_a_cover_page_still_gets_one_synthesized() {
    // This fixture opens on a text title page, not a cover image, so nothing
    // in the spine is a cover page and one must be built from the cover image.
    let epub = convert("tests/fixtures/[太宰 治] 人間失格.azw3");
    let covers = cover_pages(&epub);
    assert_eq!(
        covers.len(),
        1,
        "expected exactly one cover page, got {covers:?}"
    );
    assert!(
        covers[0].ends_with("cover.xhtml"),
        "a source with no cover page should get a synthesized one: {covers:?}"
    );
}
