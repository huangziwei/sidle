//! A table cell's `colspan` / `rowspan` survives EPUB → KFX → EPUB.

use std::io::{Cursor, Write};

use bokai::{Book, Format};

fn zip_epub(dir: &std::path::Path, entries: &[(&str, &[u8])]) -> std::path::PathBuf {
    let path = dir.join("book.epub");
    let file = std::fs::File::create(&path).expect("create epub");
    let mut zip = zip::ZipWriter::new(file);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in entries {
        zip.start_file(*name, stored).expect("start_file");
        zip.write_all(bytes).expect("write entry");
    }
    zip.finish().expect("finish zip");
    path
}

fn unzip(bytes: &[u8]) -> Vec<(String, String)> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).expect("read zip");
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).expect("entry");
        let name = f.name().to_string();
        let mut body = String::new();
        if std::io::Read::read_to_string(&mut f, &mut body).is_ok() {
            out.push((name, body));
        }
    }
    out
}

const CONTAINER: &[u8] = br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

const OPF: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Spanning Cells</dc:title>
    <dc:identifier id="bookid">urn:uuid:22222222-0000-0000-0000-000000000000</dc:identifier>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="c1"/></spine>
</package>"#;

const NAV: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Contents</title></head>
<body><nav epub:type="toc"><ol><li><a href="c1.xhtml">Table</a></li></ol></nav></body>
</html>"#;

/// A 3-column grid whose first row is one cell spanning all three, and whose
/// first body cell spans two rows. Both spans must come back — as must the
/// column widths, which are the only thing that makes the first column narrow.
const CHAPTER: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Table</title></head>
<body>
  <h1>Table</h1>
  <table>
    <colgroup>
      <col style="width: 20%"/>
      <col style="width: 40%"/>
      <col style="width: 40%"/>
    </colgroup>
    <tr><td colspan="3">Quarterly totals</td></tr>
    <tr><td rowspan="2">Region</td><td>Q1</td><td>Q2</td></tr>
    <tr><td>101</td><td>202</td></tr>
  </table>
</body></html>"#;

/// Every `colspan=`/`rowspan=` value in the document, in document order.
fn spans(doc: &str, attr: &str) -> Vec<String> {
    let needle = format!("{attr}=\"");
    doc.match_indices(&needle)
        .filter_map(|(at, _)| {
            doc[at + needle.len()..]
                .split_once('"')
                .map(|(v, _)| v.to_string())
        })
        .collect()
}

/// EPUB → KFX → EPUB, returning the document holding the table and the
/// stylesheet.
fn round_trip() -> (String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let epub = zip_epub(
        dir.path(),
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", OPF),
            ("OEBPS/nav.xhtml", NAV),
            ("OEBPS/c1.xhtml", CHAPTER),
        ],
    );

    let mut book = Book::open(&epub).expect("open spanning-cells epub");
    let mut kfx = Cursor::new(Vec::new());
    book.export(Format::Kfx, &mut kfx).expect("export kfx");
    let kfx = kfx.into_inner();

    let mut out = Cursor::new(Vec::new());
    Book::from_bytes(&kfx, Format::Kfx)
        .expect("import the exported kfx")
        .export(Format::Epub, &mut out)
        .expect("export epub");

    let files = unzip(&out.into_inner());
    let doc = files
        .iter()
        .find(|(name, body)| name.ends_with(".xhtml") && body.contains("<table"))
        .map(|(_, body)| body.clone())
        .expect("round-tripped epub has the table");
    let css = files
        .iter()
        .find(|(name, _)| name.ends_with(".css"))
        .map(|(_, body)| body.clone())
        .expect("round-tripped epub has a stylesheet");
    (doc, css)
}

#[test]
fn cell_spans_survive_the_epub_kfx_epub_round_trip() {
    let (doc, _) = round_trip();
    assert_eq!(
        spans(&doc, "colspan"),
        vec!["3".to_string()],
        "the header cell must still span three columns:\n{doc}"
    );
    assert_eq!(
        spans(&doc, "rowspan"),
        vec!["2".to_string()],
        "the region cell must still span two rows:\n{doc}"
    );
}

/// A table's `column_format` is the only place KFX states column proportions.
#[test]
fn column_widths_survive_the_epub_kfx_epub_round_trip() {
    let (doc, css) = round_trip();
    // `<col>` tags only — not the `<colgroup>` that wraps them.
    let col_tags: Vec<usize> = doc
        .match_indices("<col")
        .map(|(at, _)| at)
        .filter(|at| !doc[*at..].starts_with("<colgroup"))
        .collect();
    assert_eq!(col_tags.len(), 3, "one `<col>` per source column:\n{doc}");

    // The widths land either inline or in a class the sheet defines; either
    // way all three must be findable from the document.
    let mut widths: Vec<String> = Vec::new();
    for at in col_tags {
        let tag = &doc[at..at + doc[at..].find('>').expect("closed tag")];
        if let Some(inline) = tag
            .split_once("style=\"")
            .and_then(|(_, r)| r.split_once('"'))
        {
            widths.push(inline.0.to_string());
        } else if let Some(class) = tag
            .split_once("class=\"")
            .and_then(|(_, r)| r.split_once('"'))
        {
            let selector = format!(".{} {{ ", class.0);
            let rule = css
                .split_once(&selector)
                .and_then(|(_, r)| r.split_once(" }"))
                .unwrap_or_else(|| panic!("stylesheet defines {}:\n{css}", class.0));
            widths.push(rule.0.to_string());
        }
    }
    assert_eq!(widths.len(), 3, "every column states its width:\n{doc}");
    assert!(
        widths[0].contains("20%"),
        "the first column keeps its 20%, got {:?}",
        widths[0]
    );
    assert!(
        widths[1].contains("40%") && widths[2].contains("40%"),
        "the remaining columns keep their 40%, got {:?}",
        widths
    );
}
