//! A built package reports its cover overlap instead of resolving it.

use std::io::Cursor;

use bokai::Book;
use bokai::export::{Exporter, PackageOptions, build_package};
use bokai::model::Format;

const REFLOWABLE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";

fn zip_entries(epub: &[u8]) -> Vec<String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(epub)).expect("read the exported container");
    (0..zip.len())
        .map(|i| zip.by_index(i).expect("zip entry").name().to_string())
        .collect()
}

#[test]
fn the_container_drops_the_cover_page_the_package_keeps() {
    let Ok(kfx) = std::fs::read(REFLOWABLE) else {
        return; // fixture not present in this checkout
    };

    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("import the fixture");
    let package = build_package(&mut book, PackageOptions::container(), &|_, _, _, _| {})
        .expect("build the package");

    let idx = package
        .redundant_cover
        .expect("this fixture ships a cover section and a titlepage");
    assert!(
        package.titlepage.is_some(),
        "a redundant cover implies a titlepage renders the same image"
    );

    // The package keeps it — that is the whole point of reporting rather than
    // resolving.
    let suppressed = &package.documents[idx];
    assert!(
        suppressed.xhtml.contains("<img"),
        "the source's cover page is an image document, got: {}",
        &suppressed.xhtml[..suppressed.xhtml.len().min(200)]
    );

    // ...and it is the page carrying source elements, which is why a renderer
    // wants this one and not the titlepage.
    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("re-import the fixture");
    let spine_id = book.spine()[idx].id;
    let elements = book
        .load_chapter_cached(spine_id)
        .expect("load the cover chapter")
        .source_elements();
    assert!(
        !elements.is_empty(),
        "the suppressed cover page carries no source elements — a renderer \
         keeping it would gain nothing over the titlepage"
    );

    // The container is where the drop happens.
    let mut out = Cursor::new(Vec::new());
    bokai::export::EpubExporter::new()
        .export(&mut book, &mut out)
        .expect("export the fixture");
    let entries = zip_entries(out.get_ref());

    let dropped = format!("OEBPS/{}", suppressed.href);
    assert!(
        !entries.contains(&dropped),
        "the shipped container still holds the source cover page {dropped}"
    );
    assert!(
        entries.iter().any(|e| e == "OEBPS/cover.xhtml"),
        "the shipped container has no cover page at all: {entries:?}"
    );
}

/// Every TOC entry must name a document the package holds.
#[test]
fn every_toc_entry_names_a_document_the_package_holds() {
    let Ok(kfx) = std::fs::read(REFLOWABLE) else {
        return; // fixture not present in this checkout
    };

    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("import the fixture");
    let package = build_package(&mut book, PackageOptions::rendered(), &|_, _, _, _| {})
        .expect("build the package");

    // Non-vacuity: this fixture is one of the books whose cover section the
    // container suppresses, so it exercises the remap.
    assert!(
        package.redundant_cover.is_some(),
        "fixture no longer has a cover overlap — the assertion below is vacuous"
    );

    let rendered: Vec<&str> = package.documents.iter().map(|d| d.href.as_str()).collect();
    let mut entries = Vec::new();
    fn walk(points: &[bokai::export::nav::NavPoint], out: &mut Vec<(String, String)>) {
        for p in points {
            out.push((p.label.clone(), p.href.clone()));
            walk(&p.children, out);
        }
    }
    walk(&package.toc, &mut entries);
    assert!(!entries.is_empty(), "fixture has no TOC to check");

    for (label, href) in &entries {
        let file = href.split('#').next().unwrap_or("");
        assert!(
            file.is_empty() || rendered.contains(&file),
            "TOC entry {label:?} targets {href}, which is not one of {rendered:?}"
        );
    }
}

/// A renderer's chapter list reaches the cover exactly once.
#[test]
fn a_renderers_toc_reaches_the_cover_exactly_once() {
    let Ok(kfx) = std::fs::read(REFLOWABLE) else {
        return; // fixture not present in this checkout
    };

    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("import the fixture");
    let package = build_package(&mut book, PackageOptions::rendered(), &|_, _, _, _| {})
        .expect("build the package");
    let cover = package
        .documents
        .get(
            package
                .redundant_cover
                .expect("fixture no longer has a cover page to reach"),
        )
        .expect("the package keeps the cover document")
        .href
        .clone();

    let rows = package
        .toc
        .iter()
        .filter(|p| p.href.split('#').next() == Some(cover.as_str()))
        .count();
    assert_eq!(rows, 1, "the cover page needs exactly one row: {rows}");
    assert_eq!(
        package.toc.first().map(|p| p.href.as_str()),
        Some(cover.as_str()),
        "the cover row opens the list"
    );
    assert!(
        !package.toc[0].label.trim().is_empty(),
        "the cover row needs a label a reader can read"
    );
}
