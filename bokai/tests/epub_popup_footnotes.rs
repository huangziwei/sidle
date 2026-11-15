//! `NoteRole` from an EPUB's reciprocal links, and the popup markers its KFX
//! export carries.

use std::io::Write;

use bokai::Book;
use bokai::model::NoteRole;

/// An EPUB 2 package holding `members` beside its `mimetype` and container.
fn build_epub(members: &[(&str, &[u8])]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let file = std::fs::File::create(dir.path().join("book.epub")).expect("create epub");
    let mut zip = zip::ZipWriter::new(file);
    let stored: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let container: &[u8] = br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;
    let head: [(&str, &[u8]); 2] = [
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", container),
    ];
    for (name, bytes) in head.iter().chain(members) {
        zip.start_file(*name, stored).expect("start_file");
        zip.write_all(bytes).expect("write entry");
    }
    zip.finish().expect("finish zip");
    dir
}

/// A two-document EPUB 2 in the shape retail books use: a chapter whose
/// superscript marker links into a back-of-book notes file, and notes that
/// each open with a link back to their marker. No `epub:type` anywhere.
fn build_linked_notes_epub() -> tempfile::TempDir {
    build_epub(&[
        (
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
        ),
        (
            "OEBPS/toc.ncx",
            br#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="n1" playOrder="1"><navLabel><text>Chapter One</text></navLabel><content src="ch1.html"/></navPoint>
    <navPoint id="n2" playOrder="2"><navLabel><text>Notes</text></navLabel><content src="ch1fn.html"/></navPoint>
  </navMap>
</ncx>"#,
        ),
        // `ch1.html`: two markers, and a link to `ch1fn.html` that nothing links back to.
        (
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
        ),
        (
            "OEBPS/ch1fn.html",
            br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>Notes</title></head>
<body>
  <h2>Notes</h2>
  <p class="fn"><a id="ch1fn-1"/><a href="ch1.html#ch1fn1">1</a>. The harbour is a composite of two on the same coast.</p>
  <p class="fn"><a id="ch1fn-2"/><a href="ch1.html#ch1fn2">2</a>. Port records name four.</p>
</body></html>"#,
        ),
    ])
}

/// `contents.html` entries and the `story1.html`/`story2.html` titles (styled
/// paragraphs) link each other, `toc.ncx` naming each title by its link's id;
/// `notes.html` holds one note, carrying the `guide`, `Notes` and page targets.
fn build_linked_contents_epub() -> tempfile::TempDir {
    build_epub(&[
        (
            "OEBPS/content.opf",
            br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Linked Contents</dc:title>
    <dc:identifier id="bookid">urn:uuid:66666666-7777-8888-9999-000000000000</dc:identifier>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="toc" href="contents.html" media-type="application/xhtml+xml"/>
    <item id="s1" href="story1.html" media-type="application/xhtml+xml"/>
    <item id="s2" href="story2.html" media-type="application/xhtml+xml"/>
    <item id="nt" href="notes.html" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="toc"/>
    <itemref idref="s1"/>
    <itemref idref="s2"/>
    <itemref idref="nt"/>
  </spine>
  <guide>
    <reference type="toc" title="Contents" href="contents.html"/>
    <reference type="notes" title="Notes" href="notes.html#n1"/>
  </guide>
</package>"#,
        ),
        (
            "OEBPS/toc.ncx",
            br#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="n0" playOrder="1"><navLabel><text>Contents</text></navLabel><content src="contents.html"/></navPoint>
    <navPoint id="n1" playOrder="2"><navLabel><text>The Tide</text></navLabel><content src="story1.html#t1"/></navPoint>
    <navPoint id="n2" playOrder="3"><navLabel><text>The Lighthouse</text></navLabel><content src="story2.html#t2"/></navPoint>
    <navPoint id="n3" playOrder="4"><navLabel><text>Notes</text></navLabel><content src="notes.html#n1"/></navPoint>
  </navMap>
  <pageList>
    <pageTarget id="pg9" type="normal" value="9"><navLabel><text>9</text></navLabel><content src="notes.html#p9"/></pageTarget>
  </pageList>
</ncx>"#,
        ),
        (
            "OEBPS/contents.html",
            br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>Contents</title></head>
<body>
  <p class="toc"><b>Contents</b></p>
  <p class="toc1"><a href="story1.html#t1" id="c1"><b>The Tide</b></a></p>
  <p class="toc1"><a href="story2.html#t2" id="c2"><b>The Lighthouse</b></a></p>
</body></html>"#,
        ),
        (
            "OEBPS/story1.html",
            br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>The Tide</title></head>
<body>
  <p class="ct"><a href="contents.html#c1" id="t1"><b>The Tide</b></a></p>
  <p>The tide turned at noon.<sup><a id="r1"/><a href="notes.html#n1">1</a></sup></p>
</body></html>"#,
        ),
        (
            "OEBPS/story2.html",
            br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>The Lighthouse</title></head>
<body>
  <p class="ct"><a href="contents.html#c2" id="t2"><b>The Lighthouse</b></a></p>
  <p>Nobody kept the light that winter.</p>
</body></html>"#,
        ),
        (
            "OEBPS/notes.html",
            br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>Notes</title></head>
<body>
  <p class="fn"><a id="n1"/><a href="story1.html#r1">1</a>. The almanac gives<span id="p9"/> half past twelve.</p>
</body></html>"#,
        ),
    ])
}

/// `contents.html`, last in the spine with `linear="no"`, links each `<li>`
/// entry to an `<h1>` story title that links back; `notes.html` holds one note.
fn build_trailing_contents_epub() -> tempfile::TempDir {
    build_epub(&[
        (
            "OEBPS/content.opf",
            br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Trailing Contents</dc:title>
    <dc:identifier id="bookid">urn:uuid:12121212-3434-5656-7878-909090909090</dc:identifier>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="s1" href="story1.html" media-type="application/xhtml+xml"/>
    <item id="s2" href="story2.html" media-type="application/xhtml+xml"/>
    <item id="nt" href="notes.html" media-type="application/xhtml+xml"/>
    <item id="toc" href="contents.html" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="s1"/>
    <itemref idref="s2"/>
    <itemref idref="nt"/>
    <itemref idref="toc" linear="no"/>
  </spine>
</package>"#,
        ),
        (
            "OEBPS/toc.ncx",
            br#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="n1" playOrder="1"><navLabel><text>The Tide</text></navLabel><content src="story1.html#t1"/></navPoint>
    <navPoint id="n2" playOrder="2"><navLabel><text>The Lighthouse</text></navLabel><content src="story2.html#t2"/></navPoint>
    <navPoint id="n3" playOrder="3"><navLabel><text>Notes</text></navLabel><content src="notes.html"/></navPoint>
    <navPoint id="n4" playOrder="4"><navLabel><text>Contents</text></navLabel><content src="contents.html"/></navPoint>
  </navMap>
</ncx>"#,
        ),
        (
            "OEBPS/story1.html",
            br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>The Tide</title></head>
<body>
  <h1 id="t1"><a href="contents.html#c1">The Tide</a></h1>
  <p>The tide turned at noon.<sup><a id="r1"/><a href="notes.html#n1">1</a></sup></p>
</body></html>"#,
        ),
        (
            "OEBPS/story2.html",
            br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>The Lighthouse</title></head>
<body>
  <h1 id="t2"><a href="contents.html#c2">The Lighthouse</a></h1>
  <p>Nobody kept the light that winter.</p>
</body></html>"#,
        ),
        (
            "OEBPS/notes.html",
            br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>Notes</title></head>
<body>
  <p class="fn"><a id="n1"/><a href="story1.html#r1">1</a>. The almanac gives half past twelve.</p>
</body></html>"#,
        ),
        (
            "OEBPS/contents.html",
            br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>Contents</title></head>
<body>
  <h1>Contents</h1>
  <ol>
    <li id="c1"><a href="story1.html#t1">The Tide</a></li>
    <li id="c2"><a href="story2.html#t2">The Lighthouse</a></li>
  </ol>
</body></html>"#,
        ),
    ])
}

/// `(references, bodies)` among the roles `resolve_links` assigns.
fn note_role_counts(book: &mut Book) -> (usize, usize) {
    let resolved = book.resolve_links().expect("resolve links");
    let mut references = 0;
    let mut bodies = 0;
    for role in resolved.note_roles().values() {
        match role {
            NoteRole::Reference => references += 1,
            NoteRole::Body => bodies += 1,
        }
    }
    (references, bodies)
}

/// `(footnotes, noterefs)`: the `"footnote"` and `"noteref"` `epub:type` tokens
/// in `book` exported to KFX and read back.
fn kfx_popup_markers(book: &mut Book) -> (usize, usize) {
    let mut buf = std::io::Cursor::new(Vec::new());
    book.export(bokai::Format::Kfx, &mut buf)
        .expect("export kfx");
    let kfx = buf.into_inner();

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
    (footnotes, noterefs)
}

#[test]
fn reciprocal_links_are_read_as_notes() {
    let dir = build_linked_notes_epub();
    let mut book = Book::open(dir.path().join("book.epub")).expect("open epub");
    let (references, bodies) = note_role_counts(&mut book);
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
    let (footnotes, noterefs) = kfx_popup_markers(&mut book);
    assert_eq!(footnotes, 2, "both note bodies are classified");
    assert_eq!(noterefs, 2, "both markers open their note over the page");
}

#[test]
fn a_contents_page_linked_both_ways_stays_navigation() {
    let dir = build_linked_contents_epub();
    let mut book = Book::open(dir.path().join("book.epub")).expect("open epub");
    assert_eq!(
        note_role_counts(&mut book),
        (1, 1),
        "the note is the only note; the contents entries and titles are none"
    );
}

#[test]
fn a_contents_entry_linked_both_ways_jumps_on_kfx() {
    let dir = build_linked_contents_epub();
    let mut book = Book::open(dir.path().join("book.epub")).expect("open epub");
    assert_eq!(
        kfx_popup_markers(&mut book),
        (1, 1),
        "only the note marker opens over the page"
    );
}

#[test]
fn a_title_linking_back_to_a_trailing_contents_page_stays_navigation() {
    let dir = build_trailing_contents_epub();
    let mut book = Book::open(dir.path().join("book.epub")).expect("open epub");
    assert_eq!(
        note_role_counts(&mut book),
        (1, 1),
        "the note is the only note; the titles and contents entries are none"
    );
}
