//! Notes an EPUB marks by linking open over the page on a Kindle.
//!
//! An EPUB 2 has no `epub:type`, so nothing in it *names* a note. It marks one
//! structurally instead, as a reciprocal pair of links: the marker points into
//! the note's text and the note's text points back into the passage.
//! Recognising that pair is what puts `yj.classification` on the body and
//! `yj.display: yj.note` on the marker, the two fields the device reads to open
//! a note over the page instead of navigating to it.

use std::io::Write;

use bokai::Book;
use bokai::model::NoteRole;

/// A two-document EPUB 2 in the shape retail books use: a chapter whose
/// superscript marker links into a back-of-book notes file, and notes that
/// each open with a link back to their marker. No `epub:type` anywhere.
fn build_linked_notes_epub() -> tempfile::TempDir {
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
    <dc:title>Linked Notes</dc:title>
    <dc:identifier id="bookid">urn:uuid:11111111-2222-3333-4444-555555555555</dc:identifier>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="ch1" href="ch1.html" media-type="application/xhtml+xml"/>
    <item id="fn1" href="ch1fn.html" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="ch1"/>
    <itemref idref="fn1"/>
  </spine>
</package>"#,
    );
    add(
        &mut zip,
        "OEBPS/toc.ncx",
        br#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="n1" playOrder="1"><navLabel><text>Chapter One</text></navLabel><content src="ch1.html"/></navPoint>
    <navPoint id="n2" playOrder="2"><navLabel><text>Notes</text></navLabel><content src="ch1fn.html"/></navPoint>
  </navMap>
</ncx>"#,
    );
    // Two markers, plus a plain cross-reference to the notes file that nothing
    // links back to — a link the detection has to leave alone.
    add(
        &mut zip,
        "OEBPS/ch1.html",
        br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>Chapter One</title></head>
<body>
  <h1>Chapter One</h1>
  <p>The harbour lay under a flat grey sky.<sup><a id="ch1fn1"/><a href="ch1fn.html#ch1fn-1">1</a></sup></p>
  <p>Three boats worked the channel that season.<sup><a id="ch1fn2"/><a href="ch1fn.html#ch1fn-2">2</a></sup></p>
  <p>The notes are collected <a href="ch1fn.html">at the back</a>.</p>
</body></html>"#,
    );
    add(
        &mut zip,
        "OEBPS/ch1fn.html",
        br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>Notes</title></head>
<body>
  <h2>Notes</h2>
  <p class="fn"><a id="ch1fn-1"/><a href="ch1.html#ch1fn1">1</a>. The harbour is a composite of two on the same coast.</p>
  <p class="fn"><a id="ch1fn-2"/><a href="ch1.html#ch1fn2">2</a>. Port records name four.</p>
</body></html>"#,
    );

    zip.finish().expect("finish zip");
    dir
}

#[test]
fn reciprocal_links_are_read_as_notes() {
    let dir = build_linked_notes_epub();
    let mut book = Book::open(dir.path().join("book.epub")).expect("open epub");
    let resolved = book.resolve_links().expect("resolve links");

    let mut references = 0;
    let mut bodies = 0;
    for role in resolved.note_roles().values() {
        match role {
            NoteRole::Reference => references += 1,
            NoteRole::Body => bodies += 1,
        }
    }
    assert_eq!(bodies, 2, "both notes are bodies");
    assert_eq!(
        references, 2,
        "both markers are references, and the plain cross-reference is not"
    );
}

#[test]
fn a_linked_note_reaches_kfx_with_both_popup_markers() {
    let dir = build_linked_notes_epub();
    let mut book = Book::open(dir.path().join("book.epub")).expect("open epub");

    let mut buf = std::io::Cursor::new(Vec::new());
    book.export(bokai::Format::Kfx, &mut buf)
        .expect("export kfx");
    let kfx = buf.into_inner();

    // Read the markers back out of the produced KFX: the note body's
    // `yj.classification` arrives as `epub:type="footnote"`, the marker's
    // `yj.display: yj.note` as `epub:type="noteref"`.
    let mut produced = Book::from_bytes(&kfx, bokai::Format::Kfx).expect("open produced kfx");
    let spine: Vec<_> = produced.spine().iter().map(|e| e.id).collect();

    let mut footnotes = 0;
    let mut noterefs = 0;
    for id in spine {
        let chapter = produced.load_chapter(id).expect("load chapter");
        for node_id in chapter.iter_dfs() {
            let Some(epub_type) = chapter.semantics.epub_type(node_id) else {
                continue;
            };
            for token in epub_type.split_whitespace() {
                match token {
                    "footnote" => footnotes += 1,
                    "noteref" => noterefs += 1,
                    _ => {}
                }
            }
        }
    }

    assert_eq!(footnotes, 2, "both note bodies are classified");
    assert_eq!(noterefs, 2, "both markers open their note over the page");
}
