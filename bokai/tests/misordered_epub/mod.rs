//! A publisher EPUB whose spine contradicts its own navigation.
//!
//! Modelled on 金庸『雪山飛狐（全集）』(遠流 2014), where the spine is the manifest
//! in lexicographic order: volume one's colophon (`v1-9`) sorts ahead of volume
//! two's first file and lands mid-book, and the chapter whose filename carries
//! no `-N` (`v2`) sorts behind all of its own siblings and lands last. The
//! navigation is correct, because a human wrote it.
//!
//! Shared by the tests that read this defect from either end: what a renderer
//! should show for such a book, and what repairing it should do.

// Each test binary compiles the whole module and uses a different part of it.
#![allow(dead_code)]

use std::io::Write;

type Zip = zip::ZipWriter<std::fs::File>;

const CONTAINER: &[u8] = br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

const OPF: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Collection</dc:title>
    <dc:identifier id="bookid">urn:uuid:22222222-0000-0000-0000-000000000000</dc:identifier>
    <dc:language>zh</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="v1-1" href="v1-1.xhtml" media-type="application/xhtml+xml"/>
    <item id="v1-2" href="v1-2.xhtml" media-type="application/xhtml+xml"/>
    <item id="v1-9" href="v1-9.xhtml" media-type="application/xhtml+xml"/>
    <item id="v2-1" href="v2-1.xhtml" media-type="application/xhtml+xml"/>
    <item id="v2" href="v2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="v1-1"/>
    <itemref idref="v1-2"/>
    <itemref idref="v1-9"/>
    <itemref idref="v2-1"/>
    <itemref idref="v2"/>
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

/// The order the spine reads in — the filename-sorted accident, and what a sort
/// by reading order produces.
pub const SPINE_ORDER: [&str; 5] = ["One", "Two", "Colophon", "Four", "Three"];
/// The order the book declares, which is what every reader of it shows and what
/// a repair should leave the spine in.
pub const DECLARED_ORDER: [&str; 5] = ["One", "Two", "Three", "Four", "Colophon"];
/// The same, as the manifest ids a spine write names.
pub const DECLARED_IDS: [&str; 5] = ["v1-1", "v1-2", "v2", "v2-1", "v1-9"];

/// The fixture's bytes. The `TempDir` owns the file and must outlive the read,
/// so it is handed back rather than dropped here.
pub fn build() -> (tempfile::TempDir, Vec<u8>) {
    let page = |title: &str| {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>p</title></head><body><h2>{title}</h2><p>{title} text.</p></body></html>"
        )
        .into_bytes()
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("book.epub");
    let file = std::fs::File::create(&path).expect("create epub");
    let mut zip: Zip = zip::ZipWriter::new(file);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in [
        ("mimetype", b"application/epub+zip".as_slice()),
        ("META-INF/container.xml", CONTAINER),
        ("OEBPS/content.opf", OPF),
        ("OEBPS/nav.xhtml", NAV),
        ("OEBPS/v1-1.xhtml", &page("One")),
        ("OEBPS/v1-2.xhtml", &page("Two")),
        ("OEBPS/v1-9.xhtml", &page("Colophon")),
        ("OEBPS/v2-1.xhtml", &page("Four")),
        ("OEBPS/v2.xhtml", &page("Three")),
    ] {
        zip.start_file(name, stored).expect("start_file");
        zip.write_all(bytes).expect("write entry");
    }
    zip.finish().expect("finish zip");
    let bytes = std::fs::read(&path).expect("read back");
    (dir, bytes)
}
