//! Converting must not add a redundant cover page.

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

/// The package's OPF text.
fn opf_text(epub: &[u8]) -> String {
    let mut zip = zip::ZipArchive::new(Cursor::new(epub)).expect("read epub zip");
    let name = zip
        .file_names()
        .find(|n| n.ends_with(".opf"))
        .expect("opf present")
        .to_string();
    let mut s = String::new();
    std::io::Read::read_to_string(&mut zip.by_name(&name).unwrap(), &mut s).unwrap();
    s
}

/// Every OPF guide `type="cover"` reference href — the page a reader opens on.
/// The dual-cover bug produced two designated covers; a correct package has one.
fn cover_reference_hrefs(epub: &[u8]) -> Vec<String> {
    opf_text(epub)
        .split("<reference")
        .skip(1)
        .map(|t| t.split_once('>').map(|(head, _)| head).unwrap_or(t))
        .filter(|t| t.contains(r#"type="cover""#))
        .filter_map(|t| {
            let at = t.find("href=\"")? + "href=\"".len();
            let end = t[at..].find('"')?;
            Some(t[at..at + end].to_string())
        })
        .collect()
}

/// Whether the package contains a zip entry ending in `suffix`.
fn has_entry(epub: &[u8], suffix: &str) -> bool {
    zip::ZipArchive::new(Cursor::new(epub))
        .expect("read epub zip")
        .file_names()
        .any(|n| n.ends_with(suffix))
}

#[test]
fn source_with_its_own_cover_page_is_reused_not_duplicated() {
    // This fixture's `titlepage.xhtml` is already the SVG cover wrapper, so
    // bokai must reuse it and NOT synthesize a second cover.xhtml on top.
    let epub = convert("tests/fixtures/[太宰 治] 人間失格.epub");
    let covers = cover_reference_hrefs(&epub);
    assert_eq!(
        covers.len(),
        1,
        "exactly one cover reference, got {covers:?}"
    );
    assert!(
        covers[0].ends_with("titlepage.xhtml"),
        "the cover reference should point at the source's own page: {covers:?}"
    );
    assert!(
        !has_entry(&epub, "/cover.xhtml"),
        "no synthesized cover.xhtml when the source already ships a cover page"
    );
}

#[test]
fn source_without_a_cover_page_gets_one_synthesized() {
    // This fixture opens on a text title page, so bokai must synthesize the
    // cover page from the cover image and point the guide at it.
    let epub = convert("tests/fixtures/[太宰 治] 人間失格.azw3");
    let covers = cover_reference_hrefs(&epub);
    assert_eq!(
        covers.len(),
        1,
        "exactly one cover reference, got {covers:?}"
    );
    assert!(
        covers[0].ends_with("cover.xhtml"),
        "the cover reference should point at the synthesized page: {covers:?}"
    );
    assert!(
        has_entry(&epub, "/cover.xhtml"),
        "a source with no cover page should get a synthesized cover.xhtml"
    );
}
