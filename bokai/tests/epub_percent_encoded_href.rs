//! EPUB hrefs are URI references and may percent-encode reserved
//! bytes, while the container's ZIP entries are stored under their literal
//! decoded names.
//!
//! calibre names split chapter files like `CR!….html` but writes the manifest
//! href as `CR%21….html`, so an encoded path handed straight to the ZIP lookup
//! finds nothing. Every URI→archive boundary percent-decodes: spine paths, the
//! NCX/nav TOC, landmarks, the cover, in-HTML `<img src>` / `<a href>`, and CSS
//! `@import` URLs.

use std::io::Write;
use std::path::Path;

use bokai::Book;
use bokai::model::AnchorTarget;

/// Build a minimal EPUB 2.0 whose files carry literal `!` and space characters
/// but whose hrefs reference them percent-encoded (`%21`, `%20`). Returns the
/// tempdir holding `book.epub`.
fn build_percent_encoded_epub() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let epub_path = dir.path().join("book.epub");
    let file = std::fs::File::create(&epub_path).expect("create epub");
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

    // Manifest hrefs are percent-encoded; the matching zip entries below are
    // stored under the decoded names (`CR!…`, `img a.jpg`).
    add(
        &mut zip,
        "OEBPS/content.opf",
        br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Percent Test</dc:title>
    <dc:identifier id="bookid">urn:uuid:11111111-2222-3333-4444-555555555555</dc:identifier>
    <dc:language>en</dc:language>
    <meta name="cover" content="img1"/>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="c0" href="Text/CR%21A_split_000.html" media-type="application/xhtml+xml"/>
    <item id="c1" href="Text/CR%21A_split_001.html" media-type="application/xhtml+xml"/>
    <item id="img1" href="Images/img%20a.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="c0"/>
    <itemref idref="c1"/>
  </spine>
</package>"#,
    );

    add(
        &mut zip,
        "OEBPS/toc.ncx",
        br#"<?xml version="1.0" encoding="utf-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="np0" playOrder="1">
      <navLabel><text>Chapter One</text></navLabel>
      <content src="Text/CR%21A_split_000.html"/>
    </navPoint>
  </navMap>
</ncx>"#,
    );

    // Chapter 0 references the image (encoded space) and links to chapter 1
    // (encoded bang) — both must resolve to the decoded zip entries.
    add(
        &mut zip,
        "OEBPS/Text/CR!A_split_000.html",
        br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>c0</title></head>
<body>
  <p>Hello trauma world.</p>
  <img src="../Images/img%20a.jpg" alt="fig"/>
  <p><a href="CR%21A_split_001.html">next chapter</a></p>
</body></html>"#,
    );
    add(
        &mut zip,
        "OEBPS/Text/CR!A_split_001.html",
        br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>c1</title></head>
<body><p>Second chapter body.</p></body></html>"#,
    );

    add(
        &mut zip,
        "OEBPS/Images/img a.jpg",
        b"\xff\xd8\xff\xe0JFIF-fake-jpeg-bytes",
    );

    zip.finish().expect("finish zip");
    dir
}

/// The exact failure the user hit: dragging this EPUB in crashed conversion
/// because the spine path stayed percent-encoded and missed the ZIP entry.
/// `load_raw` walks that same path, so it stands in for the convert step.
#[test]
fn percent_encoded_spine_path_loads() {
    let dir = build_percent_encoded_epub();
    let mut book = Book::open(dir.path().join("book.epub")).expect("open percent-encoded epub");

    let spine = book.spine().to_vec();
    assert_eq!(spine.len(), 2, "both spine chapters should be present");

    let raw = book
        .load_raw(spine[0].id)
        .expect("load_raw must resolve CR%21… -> CR!… (this is what crashed)");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("Hello trauma world."),
        "decoded spine path should yield the chapter body"
    );
}

/// `<img src>` with an encoded space resolves to the decoded asset key, and the
/// asset is readable under that key.
#[test]
fn percent_encoded_image_src_resolves() {
    let dir = build_percent_encoded_epub();
    let mut book = Book::open(dir.path().join("book.epub")).expect("open percent-encoded epub");
    let spine = book.spine().to_vec();

    let chapter = book.load_chapter(spine[0].id).expect("load chapter 0");
    let resolved_src = chapter
        .iter_dfs()
        .find_map(|n| chapter.semantics.src(n).map(str::to_string))
        .expect("chapter should carry an <img src>");
    assert_eq!(
        resolved_src, "OEBPS/Images/img a.jpg",
        "img src must be percent-decoded and resolved to the archive path"
    );

    let bytes = book
        .load_asset(Path::new(&resolved_src))
        .expect("asset must be readable under the decoded key");
    assert!(
        bytes.starts_with(b"\xff\xd8\xff\xe0"),
        "load_asset should return the stored image bytes"
    );
}

/// An inter-chapter `<a href>` with `%21` resolves to the target chapter
/// instead of becoming a broken link.
#[test]
fn percent_encoded_link_resolves_not_broken() {
    let dir = build_percent_encoded_epub();
    let mut book = Book::open(dir.path().join("book.epub")).expect("open percent-encoded epub");

    let links = book.resolve_links().expect("resolve links");
    assert!(
        links.broken_links().is_empty(),
        "percent-encoded inter-chapter link should resolve, got broken: {:?}",
        links.broken_links()
    );
    assert!(
        links.iter().any(|(_, target)| matches!(
            target,
            AnchorTarget::Chapter(_) | AnchorTarget::Internal(_)
        )),
        "expected the next-chapter link to resolve to a chapter target"
    );
}

/// The NCX TOC href is percent-decoded so it matches the decoded chapter path.
#[test]
fn percent_encoded_ncx_toc_href_decoded() {
    let dir = build_percent_encoded_epub();
    let book = Book::open(dir.path().join("book.epub")).expect("open percent-encoded epub");

    let toc = book.toc();
    assert_eq!(toc.len(), 1, "one navPoint expected");
    assert_eq!(
        toc[0].href, "OEBPS/Text/CR!A_split_000.html",
        "NCX href should be base-prefixed and percent-decoded"
    );
}
