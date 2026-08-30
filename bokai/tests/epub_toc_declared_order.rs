//! A rendered package keeps the book's declared TOC order; a shipped container
//! sorts it into spine order.

mod misordered_epub;

use std::io::Cursor;

use bokai::Book;
use bokai::export::{Exporter, KfxExporter, PackageOptions, build_package};
use bokai::model::Format;
use misordered_epub::{DECLARED_ORDER, SPINE_ORDER};

/// The collection as a KFX — the shape a reader is actually handed, and the
/// only source `build_package` serves (an EPUB reaches a container through the
/// passthrough route, which never builds a package).
fn collection_kfx() -> Vec<u8> {
    let (_dir, epub) = misordered_epub::build();
    let mut book = Book::from_bytes(&epub, Format::Epub).expect("open the collection");
    let declared: Vec<&str> = book.toc().iter().map(|e| e.title.as_str()).collect();
    assert_eq!(
        declared, DECLARED_ORDER,
        "fixture is only meaningful while its nav disagrees with its spine"
    );

    let mut out = Cursor::new(Vec::new());
    KfxExporter::new()
        .export(&mut book, &mut out)
        .expect("convert the collection to KFX");
    out.into_inner()
}

fn open_kfx(kfx: &[u8]) -> Book {
    Book::from_bytes(kfx, Format::Kfx).expect("read the converted KFX")
}

#[test]
fn rendered_package_keeps_the_declared_toc_order() {
    let kfx = collection_kfx();
    let mut book = open_kfx(&kfx);
    let package = build_package(&mut book, PackageOptions::rendered(), &|_, _, _, _| {})
        .expect("build the rendered package");

    let labels: Vec<&str> = package.toc.iter().map(|p| p.label.as_str()).collect();
    assert_eq!(
        labels, DECLARED_ORDER,
        "the reader's TOC must list chapters as the book declares them; \
         sorting by the filename-ordered spine gives {SPINE_ORDER:?}"
    );
}

#[test]
fn container_nav_stays_in_reading_order() {
    let kfx = collection_kfx();
    let mut book = open_kfx(&kfx);
    let package = build_package(&mut book, PackageOptions::container(), &|_, _, _, _| {})
        .expect("build the container package");

    // Labels in the order `nav.xhtml` lists them.
    let mut emitted = Vec::new();
    let mut rest = package.nav.as_str();
    while let Some(open) = rest.find("<a href=") {
        rest = &rest[open..];
        let Some(gt) = rest.find('>') else { break };
        let Some(end) = rest[gt..].find("</a>") else {
            break;
        };
        emitted.push(&rest[gt + 1..gt + end]);
        rest = &rest[gt + end..];
    }
    assert_eq!(
        emitted, SPINE_ORDER,
        "epubcheck NAV-011 wants a shipped nav in reading order, so the \
         container's view stays sorted"
    );
}
