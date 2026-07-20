//! Regression: EPUB 3 nav TOC handling in EPUB→KFX and in the KFX→DOM read
//! path a host-side reader renders from.
//!
//! Two failure modes seen on real retail EPUBs:
//!
//!  1. A book ships BOTH a full EPUB 3 nav doc and a stub EPUB 2 NCX (or vice
//!     versa). The importer must validate against — and convert — the *richer*
//!     of the two. Preferring the NCX unless it was empty made a 3-entry stub
//!     NCX shadow a 7-entry nav, so every chapter vanished from the device TOC.
//!
//!  2. A nested nav points at intra-chapter headings, but the content carries no
//!     `<a href>` cross-references to those positions — so bokai's e2k KFX emits
//!     no internal `$266` anchor for them. The KFX→DOM read path must still
//!     produce one distinct, resolvable `#fragment` per entry; otherwise every
//!     nested entry collapses to the top of its chapter file.

use std::io::{Cursor, Write};

use bokai::{Book, Format};

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

/// Flatten a TOC tree to its titles, in document order.
fn titles(entries: &[bokai::TocEntry], out: &mut Vec<String>) {
    for e in entries {
        out.push(e.title.clone());
        titles(&e.children, out);
    }
}

#[test]
fn nav_doc_wins_over_degenerate_ncx() {
    // OPF 3.0 declaring a `nav` (properties="nav") AND an NCX. The NCX lists only
    // cover + colophon; the nav lists cover + three chapters + colophon. The
    // importer must keep the richer nav.
    let opf = br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Both TOCs</dc:title>
    <dc:identifier id="bookid">urn:uuid:11111111-0000-0000-0000-000000000000</dc:identifier>
    <dc:language>ja</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="cov" href="cover.xhtml" media-type="application/xhtml+xml"/>
    <item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/>
    <item id="c3" href="c3.xhtml" media-type="application/xhtml+xml"/>
    <item id="col" href="colophon.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="cov"/>
    <itemref idref="c1"/>
    <itemref idref="c2"/>
    <itemref idref="c3"/>
    <itemref idref="col"/>
  </spine>
</package>"#;

    // Degenerate NCX: cover + colophon only, no chapters.
    let ncx = br#"<?xml version="1.0"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="n1" playOrder="1"><navLabel><text>Cover</text></navLabel><content src="cover.xhtml"/></navPoint>
    <navPoint id="n2" playOrder="2"><navLabel><text>Colophon</text></navLabel><content src="colophon.xhtml"/></navPoint>
  </navMap>
</ncx>"#;

    // Full nav: cover + three chapters + colophon.
    let nav = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Nav</title></head>
<body>
<nav epub:type="toc"><ol>
  <li><a href="cover.xhtml">Cover</a></li>
  <li><a href="c1.xhtml">Chapter One</a></li>
  <li><a href="c2.xhtml">Chapter Two</a></li>
  <li><a href="c3.xhtml">Chapter Three</a></li>
  <li><a href="colophon.xhtml">Colophon</a></li>
</ol></nav>
</body></html>"#;

    let page = |body: &str| {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>p</title></head><body>{body}</body></html>"
        )
        .into_bytes()
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let epub = zip_epub(
        dir.path(),
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf),
            ("OEBPS/toc.ncx", ncx),
            ("OEBPS/nav.xhtml", nav),
            ("OEBPS/cover.xhtml", &page("<p>cover</p>")),
            ("OEBPS/c1.xhtml", &page("<h2>Chapter One</h2><p>a</p>")),
            ("OEBPS/c2.xhtml", &page("<h2>Chapter Two</h2><p>b</p>")),
            ("OEBPS/c3.xhtml", &page("<h2>Chapter Three</h2><p>c</p>")),
            ("OEBPS/colophon.xhtml", &page("<p>colophon</p>")),
        ],
    );

    let book = Book::open(&epub).expect("open both-TOCs epub");
    let mut names = Vec::new();
    titles(book.toc(), &mut names);

    assert_eq!(
        book.toc().len(),
        5,
        "the 5-entry nav must win over the 2-entry NCX, got {names:?}"
    );
    for chap in ["Chapter One", "Chapter Two", "Chapter Three"] {
        assert!(
            names.iter().any(|t| t == chap),
            "{chap} (nav-only) must be present; the stub NCX would have dropped it — got {names:?}"
        );
    }
}

#[test]
fn reader_toc_resolves_intra_chapter_anchors_without_kfx_anchor_table() {
    // A single chapter with three scene headings, and a nested nav that targets
    // each. No content `<a href>` points at the scenes, so bokai's e2k KFX emits
    // no internal `$266` anchor for them — the exact case where the reader used
    // to drop the fragment and collapse all three onto the chapter top.
    let opf = br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Scenes</dc:title>
    <dc:identifier id="bookid">urn:uuid:22222222-0000-0000-0000-000000000000</dc:identifier>
    <dc:language>ja</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c1"/>
  </spine>
</package>"#;

    let nav = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Nav</title></head>
<body>
<nav epub:type="toc"><ol>
  <li><a href="c1.xhtml#s1">Scene One</a></li>
  <li><a href="c1.xhtml#s2">Scene Two</a></li>
  <li><a href="c1.xhtml#s3">Scene Three</a></li>
</ol></nav>
</body></html>"#;

    // One file, three headings, each with a lot of filler so the scenes sit at
    // clearly distinct positions.
    let filler = "<p>".to_string() + &"the quick brown fox. ".repeat(40) + "</p>";
    let chapter = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>c1</title></head><body>\
<h3 id=\"s1\">Scene One</h3>{filler}\
<h3 id=\"s2\">Scene Two</h3>{filler}\
<h3 id=\"s3\">Scene Three</h3>{filler}\
</body></html>"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let epub = zip_epub(
        dir.path(),
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf),
            ("OEBPS/nav.xhtml", nav),
            ("OEBPS/c1.xhtml", chapter.as_bytes()),
        ],
    );

    // EPUB → KFX (bokai's own e2k output — ships no internal `$266` anchors).
    let mut book = Book::open(&epub).expect("open scenes epub");
    let mut kfx = Cursor::new(Vec::new());
    book.export(Format::Kfx, &mut kfx).expect("export kfx");
    let kfx = kfx.into_inner();

    // KFX → reader.
    let reader = bokai::kfx_to_epub::kfx_to_reader_book(&kfx).expect("reader book");

    // Collect the three scene entries (skip any synthesized cover).
    let mut flat = Vec::new();
    fn walk(pts: &[bokai::kfx_to_epub::navigation::NavPoint], out: &mut Vec<(String, String)>) {
        for p in pts {
            out.push((p.label.clone(), p.href.clone()));
            walk(&p.children, out);
        }
    }
    walk(&reader.toc, &mut flat);
    let scenes: Vec<&(String, String)> = flat
        .iter()
        .filter(|(l, _)| l.starts_with("Scene"))
        .collect();
    assert_eq!(
        scenes.len(),
        3,
        "three scene entries expected, got {flat:?}"
    );

    // Each entry: a fragment that resolves via getElementById in its section.
    let mut frags = Vec::new();
    for (label, href) in &scenes {
        let (sec, frag) = href
            .split_once('#')
            .unwrap_or_else(|| panic!("{label} lost its #fragment: {href}"));
        let html = reader
            .sections
            .iter()
            .find(|s| s.href == sec)
            .unwrap_or_else(|| panic!("{label} section {sec} missing"));
        assert!(
            html.html.contains(&format!("id=\"{frag}\"")),
            "{label} fragment #{frag} has no matching id in {sec} (would not scroll)"
        );
        frags.push(frag.to_string());
    }

    // The three must be DISTINCT — the bug made them collapse to the same href.
    frags.sort();
    frags.dedup();
    assert_eq!(
        frags.len(),
        3,
        "the three scenes must map to distinct anchors"
    );
}
