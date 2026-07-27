//! Regression: EPUB 3 nav TOC handling across EPUB→KFX and back.
//!
//! Three failure modes seen on real retail EPUBs:
//!
//!  1. A book ships BOTH a full EPUB 3 nav doc and a stub EPUB 2 NCX (or vice
//!     versa). The importer must validate against — and convert — the *richer*
//!     of the two. Preferring the NCX unless it was empty made a 3-entry stub
//!     NCX shadow a 7-entry nav, so every chapter vanished from the device TOC.
//!
//!  2. A nested nav points at intra-chapter headings, but the content carries no
//!     `<a href>` cross-references to those positions — so bokai's e2k KFX emits
//!     no internal `$266` anchor for them. Reading that KFX back must still
//!     produce one distinct, resolvable `#fragment` per entry; otherwise every
//!     nested entry collapses to the top of its chapter file.
//!
//!  3. The whole chapter list hangs off one entry that points at the cover page
//!     — what a collection's own list leaves behind in each volume it is split
//!     into, and what plenty of light novels ship as their nav. The KFX export
//!     drops a cover entry the source carries itself, and taking the subtree
//!     with it left those books a one-row TOC reading "Cover".

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
fn nested_nav_anchors_survive_the_epub_kfx_epub_round_trip() {
    // A single chapter with three scene headings, and a nested nav that targets
    // each. No content `<a href>` points at the scenes, so bokai's e2k KFX emits
    // no internal `$266` anchor for them — the exact case where reading the KFX
    // back dropped the fragment and collapsed all three onto the chapter top.
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

    // KFX → EPUB, and read the round-tripped nav back out.
    let mut out = Cursor::new(Vec::new());
    Book::from_bytes(&kfx, Format::Kfx)
        .expect("import the exported kfx")
        .export(Format::Epub, &mut out)
        .expect("export epub");
    let files = unzip(&out.into_inner());

    let nav_doc = files
        .iter()
        .find(|(name, body)| name.ends_with(".xhtml") && body.contains("epub:type=\"toc\""))
        .map(|(_, body)| body.clone())
        .expect("round-tripped epub has a nav document");

    // Collect the three scene entries (skip any synthesized cover).
    let scenes: Vec<(String, String)> = nav_doc
        .match_indices("<a href=\"")
        .filter_map(|(at, _)| {
            let rest = &nav_doc[at + "<a href=\"".len()..];
            let (href, rest) = rest.split_once('"')?;
            let label = rest.split_once('>')?.1.split_once('<')?.0;
            label
                .starts_with("Scene")
                .then(|| (label.to_string(), href.to_string()))
        })
        .collect();
    assert_eq!(
        scenes.len(),
        3,
        "three scene entries expected in the nav, got {scenes:?}"
    );

    // Each entry: a fragment that resolves via getElementById in its document.
    let mut frags = Vec::new();
    for (label, href) in &scenes {
        let (doc, frag) = href
            .split_once('#')
            .unwrap_or_else(|| panic!("{label} lost its #fragment: {href}"));
        let (_, body) = files
            .iter()
            .find(|(name, _)| name.ends_with(doc))
            .unwrap_or_else(|| panic!("{label} target document {doc} missing"));
        assert!(!frag.is_empty(), "{label} has an empty fragment: {href}");
        assert!(
            body.contains(&format!("id=\"{frag}\"")),
            "{label} fragment #{frag} has no matching id in {doc} (would not scroll)"
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

/// A chapter list rooted at an entry that points at the cover page must survive
/// the KFX export.
///
/// This is the shape every volume of a split collection has: the collection
/// named the volume by its title and pointed that entry at the volume's own
/// cover, so carving the volume out leaves its whole chapter list nested under
/// one cover-targeting row. The exporter drops the source's own cover entry —
/// it synthesizes the canonical one — and dropping the subtree with it left the
/// volume with a table of contents of exactly one row, "Cover", on the device
/// and in the reader alike.
#[test]
fn a_toc_rooted_at_the_cover_page_keeps_its_chapters_through_kfx() {
    let opf = br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Volume One</dc:title>
    <dc:identifier id="bookid">urn:uuid:33333333-0000-0000-0000-000000000000</dc:identifier>
    <dc:language>en</dc:language>
    <meta name="cover" content="img"/>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="cov" href="cover.xhtml" media-type="application/xhtml+xml"/>
    <item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="cover.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine>
    <itemref idref="cov"/>
    <itemref idref="c1"/>
    <itemref idref="c2"/>
  </spine>
</package>"#;

    // The volume's title is the sole top-level row, and it targets the cover.
    let nav = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Nav</title></head>
<body>
<nav epub:type="toc"><ol>
  <li><a href="cover.xhtml">Volume One</a>
    <ol>
      <li><a href="c1.xhtml">Chapter One</a></li>
      <li><a href="c2.xhtml">Chapter Two</a></li>
    </ol>
  </li>
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
            ("OEBPS/nav.xhtml", nav),
            (
                "OEBPS/cover.xhtml",
                &page(r#"<img src="cover.jpg" alt=""/>"#),
            ),
            ("OEBPS/c1.xhtml", &page("<h2>Chapter One</h2><p>a</p>")),
            ("OEBPS/c2.xhtml", &page("<h2>Chapter Two</h2><p>b</p>")),
            ("OEBPS/cover.jpg", JPEG),
        ],
    );

    let mut book = Book::open(&epub).expect("open volume epub");
    let mut kfx = Cursor::new(Vec::new());
    book.export(Format::Kfx, &mut kfx).expect("export kfx");

    let kfx = Book::from_bytes(&kfx.into_inner(), Format::Kfx).expect("import the exported kfx");
    let mut names = Vec::new();
    titles(kfx.toc(), &mut names);

    for chap in ["Chapter One", "Chapter Two"] {
        assert!(
            names.iter().any(|t| t == chap),
            "{chap} sat under the cover-targeting root and must survive it — got {names:?}"
        );
    }
}

/// Smallest JPEG the image pipeline will accept: a 1×1 baseline grey pixel.
const JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xdb, 0x00, 0x43, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00,
    0xff, 0xc4, 0x00, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xff, 0xc4, 0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xda, 0x00, 0x08,
    0x01, 0x01, 0x00, 0x00, 0x3f, 0x00, 0x54, 0xdf, 0xff, 0xd9,
];

/// Every text entry of an EPUB, as `(name, contents)`. Binary entries decode
/// lossily — nothing here inspects them.
fn unzip(epub: &[u8]) -> Vec<(String, String)> {
    let mut zip = zip::ZipArchive::new(Cursor::new(epub)).expect("read epub zip");
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).expect("zip entry");
        let name = entry.name().to_string();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut buf).expect("read entry");
        out.push((name, String::from_utf8_lossy(&buf).into_owned()));
    }
    out
}
