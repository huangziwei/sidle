//! A rendered package keeps the book's declared TOC order; a shipped container
//! sorts it into spine order.
//!
//! Retail collections (合本 / 全集) are routinely packaged with a spine ordered by
//! filename rather than by chapter: volume one's colophon (`…513-9.xhtml`) sorts
//! ahead of volume two's first file, and a chapter file with no `-N` suffix
//! (`…520.xhtml`) sorts behind all of its own siblings. The book's navigation
//! says what the reader should see; the spine is the accident, and it survives
//! into the KFX a reader is handed.
//!
//! EPUB 3 still wants a shipped `nav.xhtml` in reading order (epubcheck
//! NAV-011), so the container sorts — but a renderer showing that sorted list
//! puts chapter nine after the appendices, which is neither what the source
//! declares nor what any other reader of the same book displays.

use std::io::{Cursor, Write};

use bokai::export::{Exporter, KfxExporter, PackageOptions, build_package};
use bokai::model::Format;
use bokai::{Book, TocEntry};

type Zip = zip::ZipWriter<std::fs::File>;

fn zip_epub(dir: &std::path::Path, entries: &[(&str, &[u8])]) -> std::path::PathBuf {
    let path = dir.join("book.epub");
    let file = std::fs::File::create(&path).expect("create epub");
    let mut zip: Zip = zip::ZipWriter::new(file);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in entries {
        zip.start_file(*name, stored).expect("start_file");
        zip.write_all(bytes).expect("write entry");
    }
    zip.finish().expect("finish zip");
    path
}

const CONTAINER: &[u8] = br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

/// Spine: One, Two, Colophon, Four, Three — the filename-sorted accident.
/// Nav:   One, Two, Three, Four, Colophon — what the book declares.
const OPF: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Collection</dc:title>
    <dc:identifier id="bookid">urn:uuid:22222222-0000-0000-0000-000000000000</dc:identifier>
    <dc:language>zh</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="c1" href="v1-1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="v1-2.xhtml" media-type="application/xhtml+xml"/>
    <item id="col" href="v1-9.xhtml" media-type="application/xhtml+xml"/>
    <item id="c4" href="v2-1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c3" href="v2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c1"/>
    <itemref idref="c2"/>
    <itemref idref="col"/>
    <itemref idref="c4"/>
    <itemref idref="c3"/>
  </spine>
</package>"#;

const NAV: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Nav</title></head>
<body>
<nav epub:type="toc"><ol>
  <li><a href="v1-1.xhtml">One</a></li>
  <li><a href="v1-2.xhtml">Two</a></li>
  <li><a href="v2.xhtml">Three</a></li>
  <li><a href="v2-1.xhtml">Four</a></li>
  <li><a href="v1-9.xhtml">Colophon</a></li>
</ol></nav>
</body></html>"#;

/// The order the spine implies, which is what a sort by reading order produces.
const SPINE_ORDER: [&str; 5] = ["One", "Two", "Colophon", "Four", "Three"];
/// The order the book declares, which is what every reader of it shows.
const DECLARED_ORDER: [&str; 5] = ["One", "Two", "Three", "Four", "Colophon"];

/// The collection as a KFX — the shape a reader is actually handed, and the
/// only source `build_package` serves (an EPUB reaches a container through the
/// passthrough route, which never builds a package).
fn collection_kfx() -> Vec<u8> {
    let page = |title: &str| {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>p</title></head><body><h2>{title}</h2><p>{title} text.</p></body></html>"
        )
        .into_bytes()
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let epub = zip_epub(
        dir.path(),
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", OPF),
            ("OEBPS/nav.xhtml", NAV),
            ("OEBPS/v1-1.xhtml", &page("One")),
            ("OEBPS/v1-2.xhtml", &page("Two")),
            ("OEBPS/v1-9.xhtml", &page("Colophon")),
            ("OEBPS/v2-1.xhtml", &page("Four")),
            ("OEBPS/v2.xhtml", &page("Three")),
        ],
    );

    let mut book = Book::open(&epub).expect("open the collection");
    let declared: Vec<&str> = book
        .toc()
        .iter()
        .map(|e: &TocEntry| e.title.as_str())
        .collect();
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
