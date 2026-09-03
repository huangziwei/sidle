//! Repair flat EPUB TOCs that dropped their `#fragment`.

use std::io::Write;

use bokai::Book;
use bokai::model::AnchorTarget;

/// Build a 3-entry EPUB whose NCX points two chapters at one file and a third at
/// its own. Bodies carry the ids the TOC omitted; one heading adds a U+3000.
fn build_flat_toc_epub() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let file = std::fs::File::create(dir.path().join("book.epub")).expect("create epub");
    let mut zip = zip::ZipWriter::new(file);
    let stored: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let add = |zip: &mut zip::ZipWriter<std::fs::File>, name: &str, bytes: &[u8]| {
        zip.start_file(name, stored).expect("start_file");
        zip.write_all(bytes).expect("write entry");
    };

    add(&mut zip, "mimetype", b"application/epub+zip");
    add(
        &mut zip,
        "META-INF/container.xml",
        br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
    );
    add(
        &mut zip,
        "OEBPS/content.opf",
        br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Flat TOC</dc:title>
    <dc:identifier id="bookid">urn:uuid:99999999-0000-0000-0000-000000000000</dc:identifier>
    <dc:language>ja</dc:language>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="p0" href="part0.html" media-type="application/xhtml+xml"/>
    <item id="p1" href="part1.html" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="p0"/>
    <itemref idref="p1"/>
  </spine>
</package>"#,
    );
    // NCX: both 第一話 and 第二話 point to part0.html with NO fragment.
    add(
        &mut zip,
        "OEBPS/toc.ncx",
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<ncx xmlns=\"http://www.daisy.org/z3986/2005/ncx/\" version=\"2005-1\">
  <navMap>
    <navPoint id=\"n1\" playOrder=\"1\"><navLabel><text>\u{7b2c}\u{4e00}\u{8a71}\u{300c}A\u{300d}</text></navLabel><content src=\"part0.html\"/></navPoint>
    <navPoint id=\"n2\" playOrder=\"2\"><navLabel><text>\u{7b2c}\u{4e8c}\u{8a71}\u{300c}B\u{300d}</text></navLabel><content src=\"part0.html\"/></navPoint>
    <navPoint id=\"n3\" playOrder=\"3\"><navLabel><text>\u{7b2c}\u{4e09}\u{8a71}\u{300c}C\u{300d}</text></navLabel><content src=\"part1.html\"/></navPoint>
  </navMap>
</ncx>".as_bytes(),
    );
    // part0.html: two episodes. Second heading inserts a full-width space the
    // TOC label does not have (第二話　「B」 vs 第二話「B」).
    add(
        &mut zip,
        "OEBPS/part0.html",
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>
<!DOCTYPE html>
<html xmlns=\"http://www.w3.org/1999/xhtml\" lang=\"ja\"><head><title>p0</title></head>
<body>
  <p class=\"bold\" id=\"h-a\">\u{7b2c}\u{4e00}\u{8a71}\u{300c}A\u{300d}</p>
  <p>\u{672c}\u{6587}\u{4e00}\u{3002}</p>
  <p class=\"bold\" id=\"h-b\">\u{7b2c}\u{4e8c}\u{8a71}\u{3000}\u{300c}B\u{300d}</p>
  <p>\u{672c}\u{6587}\u{4e8c}\u{3002}</p>
</body></html>"
            .as_bytes(),
    );
    add(
        &mut zip,
        "OEBPS/part1.html",
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>
<!DOCTYPE html>
<html xmlns=\"http://www.w3.org/1999/xhtml\" lang=\"ja\"><head><title>p1</title></head>
<body>
  <p class=\"bold\" id=\"h-c\">\u{7b2c}\u{4e09}\u{8a71}\u{300c}C\u{300d}</p>
  <p>\u{672c}\u{6587}\u{4e09}\u{3002}</p>
</body></html>"
            .as_bytes(),
    );

    zip.finish().expect("finish zip");
    dir
}

fn internal_gid(target: &Option<AnchorTarget>) -> Option<bokai::model::GlobalNodeId> {
    match target {
        Some(AnchorTarget::Internal(gid)) => Some(*gid),
        _ => None,
    }
}

#[test]
fn flat_toc_pair_resolves_to_distinct_targets() {
    let dir = build_flat_toc_epub();
    let mut book = Book::open(dir.path().join("book.epub")).expect("open flat-toc epub");
    book.resolve_links().expect("resolve links");

    let toc = book.toc();
    assert_eq!(toc.len(), 3, "three TOC entries expected");

    // Both fragment-less hrefs that pointed at part0.html were repaired to the
    // matching element ids — including the second via whitespace-insensitive
    // matching (heading has a full-width space the label lacks).
    assert!(
        toc[0].href.ends_with("part0.html#h-a"),
        "first entry should gain #h-a, got {:?}",
        toc[0].href
    );
    assert!(
        toc[1].href.ends_with("part0.html#h-b"),
        "second entry should gain #h-b despite full-width space, got {:?}",
        toc[1].href
    );

    // The two entries that shared one file now resolve to different nodes.
    let g0 = internal_gid(&toc[0].target).expect("entry 0 resolves to a node");
    let g1 = internal_gid(&toc[1].target).expect("entry 1 resolves to a node");
    assert_ne!(
        g0, g1,
        "the collapsed pair must resolve to distinct positions, not the file top"
    );

    // The single-chapter file is repaired too.
    assert!(
        toc[2].href.ends_with("part1.html#h-c"),
        "third entry should gain #h-c, got {:?}",
        toc[2].href
    );
}

#[test]
fn flat_toc_repair_carries_into_kfx_nav() {
    let dir = build_flat_toc_epub();
    let mut book = Book::open(dir.path().join("book.epub")).expect("open flat-toc epub");

    let mut buf = std::io::Cursor::new(Vec::new());
    book.export(bokai::Format::Kfx, &mut buf)
        .expect("export kfx");
    let kfx = buf.into_inner();
    assert!(!kfx.is_empty(), "KFX export should produce bytes");

    // Re-open the produced KFX: the two episodes that shared one EPUB file must
    // now land on different nav target positions.
    let produced = Book::from_bytes(&kfx, bokai::Format::Kfx).expect("open produced kfx");
    let toc = produced.toc();
    let pos = |needle: char| -> Option<String> {
        toc.iter()
            .find(|e| e.title.contains(needle))
            .map(|e| e.href.clone())
    };
    let a = pos('A').expect("episode A present in KFX TOC");
    let b = pos('B').expect("episode B present in KFX TOC");
    assert_ne!(
        a, b,
        "episodes A and B must point to different KFX positions, got both = {a:?}"
    );
}
