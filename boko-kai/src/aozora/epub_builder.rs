//! Build an EPUB byte stream from a parsed Aozora [`Document`].
//!
//! Faithful port of `buildEpub` in
//! `/Users/ziweih/projects/tools/aozora-epub.html` (lines 902-1188).
//! Output mirrors the HTML tool's package shape: mimetype-first STORE'd,
//! `META-INF/container.xml`, `OEBPS/style.css`, per-chapter XHTML split at
//! `<h2>` boundaries, EPUB-3 nav doc + NCX (for older readers), OPF with
//! `xml:lang="ja"` and `page-progression-direction="rtl"`. Cover is a
//! pre-rendered JPEG supplied by the caller.

use std::io::{self, Write};

use zip::write::{SimpleFileOptions, ZipWriter};
use zip::CompressionMethod;

use super::parser_txt::{Document, TocEntry};

/// Inputs to [`build_epub`].
pub struct EpubInput<'a> {
    pub document: &'a Document,
    /// `(filename-as-referenced-in-body, raw bytes)` pairs for every image
    /// the body references via `<img src="../images/{filename}"/>`. Order
    /// affects manifest item ids only.
    pub images: &'a [(String, Vec<u8>)],
    /// Pre-rendered cover JPEG bytes. Built by the Aozora cover module
    /// (resvg → JPEG). The EPUB always carries a cover entry.
    pub cover_jpeg: &'a [u8],
}

/// Build an EPUB byte stream. Output is a complete EPUB-3 zip (mimetype
/// first, STORE'd; rest DEFLATE'd). The bytes are suitable for
/// `EpubImporter::from_source` or for writing to a `.epub` file.
pub fn build_epub(input: EpubInput<'_>) -> io::Result<Vec<u8>> {
    let doc = input.document;
    let uuid = crate::util::uuid_v5(&format!("aozora:{}:{}", doc.title, doc.author));
    let chapters = split_into_chapters(doc);
    let id_to_file = build_id_to_file_map(&chapters);
    // EPUB publisher is always "青空文庫" (the digital publisher). The print
    // publisher that the HTML tool used to extract from the 底本 colophon
    // line is just the *source* paperback — kept inside the colophon
    // chapter text but not surfaced as `<dc:publisher>`. Pub-date is still
    // parsed from the colophon as the work's most authoritative date.
    let publisher = "青空文庫";
    let pub_date = parse_pub_date(&doc.colophon);

    let buf = Vec::with_capacity(256 * 1024);
    let cursor = io::Cursor::new(buf);
    let mut zip = ZipWriter::new(cursor);

    let stored: SimpleFileOptions =
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated: SimpleFileOptions =
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    // 1. mimetype — must be the first entry and stored uncompressed.
    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;

    // 2. META-INF/container.xml
    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(CONTAINER_XML.as_bytes())?;

    // 3. OEBPS/style.css — verbatim port of the HTML tool's stylesheet.
    zip.start_file("OEBPS/style.css", deflated)?;
    zip.write_all(STYLE_CSS.as_bytes())?;

    // 4. Cover image (always JPEG — user constraint, see plan).
    zip.start_file("OEBPS/images/cover.jpg", stored)?;
    zip.write_all(input.cover_jpeg)?;

    // 5. Cover XHTML
    zip.start_file("OEBPS/text/cover.xhtml", deflated)?;
    zip.write_all(COVER_XHTML.as_bytes())?;

    // 6. Chapter XHTML files
    for ch in &chapters {
        zip.start_file(&format!("OEBPS/text/{}", ch.file), deflated)?;
        zip.write_all(wrap_chapter_xhtml(&ch.title, &ch.body).as_bytes())?;
    }

    // 7. Asset images (referenced by the body)
    for (name, bytes) in input.images {
        zip.start_file(&format!("OEBPS/images/{}", name), stored)?;
        zip.write_all(bytes)?;
    }

    // 8. Nav doc (EPUB 3 toc)
    zip.start_file("OEBPS/toc.xhtml", deflated)?;
    zip.write_all(
        build_nav_xhtml(&doc.title, &doc.toc, &id_to_file, !doc.colophon.is_empty()).as_bytes(),
    )?;

    // 9. NCX (EPUB 2 compat)
    zip.start_file("OEBPS/toc.ncx", deflated)?;
    zip.write_all(
        build_ncx(&uuid, &doc.title, &doc.toc, &id_to_file, !doc.colophon.is_empty())
            .as_bytes(),
    )?;

    // 10. OPF (last so the rest of the package is already on the wire)
    zip.start_file("OEBPS/content.opf", deflated)?;
    zip.write_all(
        build_opf(
            &uuid,
            &doc.title,
            &doc.author,
            &publisher,
            &pub_date,
            &chapters,
            input.images,
        )
        .as_bytes(),
    )?;

    let cursor = zip.finish()?;
    Ok(cursor.into_inner())
}

// =========================================================================
// Chapter splitting
// =========================================================================

struct Chapter {
    /// Filename within `OEBPS/text/`, e.g. `"title.xhtml"`, `"ch1.xhtml"`.
    file: String,
    /// Plain-text title used for the `<title>` element + NCX label.
    title: String,
    /// Inner XHTML body content (no `<html>` wrapper yet).
    body: String,
}

/// Split the document body at `<h2>` boundaries.
///
/// First chunk = title page (`<h1>title</h1><p>author</p>` + any body
/// content before the first `<h2>`). Each subsequent `<h2>` starts a new
/// chapter. If a colophon is present it gets a trailing chapter.
fn split_into_chapters(doc: &Document) -> Vec<Chapter> {
    let body = &doc.body_xhtml;
    // JS uses `body.split(/(?=<h2[\s>])/)` — split on positions immediately
    // before `<h2 ` or `<h2>`. Rust regex has no lookahead but we can scan
    // manually for `<h2 ` / `<h2>`.
    let mut splits: Vec<&str> = Vec::new();
    let mut last = 0;
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if &bytes[i..i + 3] == b"<h2" && matches!(bytes[i + 3], b' ' | b'>' | b'\t' | b'\n') {
            if i > last {
                splits.push(&body[last..i]);
            }
            last = i;
            i += 3;
        } else {
            i += 1;
        }
    }
    splits.push(&body[last..]);

    let mut chapters = Vec::with_capacity(splits.len() + 1);

    // Title page = `<h1>title</h1><p>author</p>` + content before the first h2.
    let title_body = format!(
        "<h1>{}</h1>\n<p>{}</p>\n{}",
        escape_xml(&doc.title),
        escape_xml(&doc.author),
        splits.first().copied().unwrap_or(""),
    );
    chapters.push(Chapter {
        file: "title.xhtml".to_string(),
        title: doc.title.clone(),
        body: title_body,
    });

    for (i, body) in splits.iter().enumerate().skip(1) {
        let ch_title = extract_h2_text(body).unwrap_or_else(|| format!("Chapter {}", i));
        chapters.push(Chapter {
            file: format!("ch{}.xhtml", i),
            title: ch_title,
            body: body.to_string(),
        });
    }

    if !doc.colophon.is_empty() {
        let mut col_body = String::from(r#"<div class="colophon" id="colophon">"#);
        col_body.push('\n');
        for line in doc.colophon.lines() {
            col_body.push_str("<p>");
            col_body.push_str(&escape_xml(line));
            col_body.push_str("</p>\n");
        }
        col_body.push_str("</div>\n");
        chapters.push(Chapter {
            file: "colophon.xhtml".to_string(),
            title: "底本情報".to_string(),
            body: col_body,
        });
    }

    chapters
}

fn extract_h2_text(body: &str) -> Option<String> {
    // Find the first `<h2 ...>` / `</h2>` pair and return the inner text with
    // any nested tags stripped. JS regex: `/<h2[^>]*>(.+?)<\/h2>/`.
    let open_start = body.find("<h2")?;
    let open_end = body[open_start..].find('>')? + open_start + 1;
    let close = body[open_end..].find("</h2>")? + open_end;
    let inner = &body[open_end..close];
    Some(strip_tags(inner))
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

// =========================================================================
// TOC structure
// =========================================================================

/// Map from `<h2>/<h3>/<h4>` id (e.g. `"h3"`) → chapter file
/// (e.g. `"ch2.xhtml"`). Used to resolve TOC hrefs to the correct chapter.
fn build_id_to_file_map(chapters: &[Chapter]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for ch in chapters {
        // Scan for `id="hN"` in body.
        let mut rest = ch.body.as_str();
        while let Some(idx) = rest.find("id=\"h") {
            let after = &rest[idx + 4..]; // skip `id="`
            if let Some(end) = after.find('"') {
                let id = &after[..end];
                out.insert(id.to_string(), ch.file.clone());
                rest = &after[end..];
            } else {
                break;
            }
        }
    }
    out
}

fn toc_href(entry: &TocEntry, id_to_file: &std::collections::HashMap<String, String>) -> String {
    let file = id_to_file
        .get(&entry.id)
        .cloned()
        .unwrap_or_else(|| "title.xhtml".to_string());
    format!("text/{}#{}", file, entry.id)
}

// =========================================================================
// XHTML / NAV / NCX / OPF templates
// =========================================================================

fn wrap_chapter_xhtml(ch_title: &str, ch_body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" xml:lang="ja" lang="ja">
<head>
  <meta charset="UTF-8"/>
  <title>{}</title>
  <link rel="stylesheet" type="text/css" href="../style.css"/>
</head>
<body>
{}
</body>
</html>"#,
        escape_xml(ch_title),
        ch_body,
    )
}

fn build_nav_xhtml(
    title: &str,
    toc: &[TocEntry],
    id_to_file: &std::collections::HashMap<String, String>,
    has_colophon: bool,
) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" xml:lang="ja">
<head><meta charset="UTF-8"/><title>Navigation</title>
<link rel="stylesheet" type="text/css" href="style.css"/>
</head>
<body>
<nav epub:type="toc" id="toc">
  <h1>目次</h1>
  {}
</nav>
</body>
</html>"#,
        build_nav_ol(title, toc, id_to_file, has_colophon),
    )
}

fn build_nav_ol(
    title: &str,
    entries: &[TocEntry],
    id_to_file: &std::collections::HashMap<String, String>,
    has_colophon: bool,
) -> String {
    let mut ol = String::from("<ol>\n");
    ol.push_str(&format!(
        r#"<li><a href="text/title.xhtml">{}</a></li>
"#,
        escape_xml(title),
    ));
    if entries.is_empty() && has_colophon {
        ol.push_str(r#"<li><a href="text/colophon.xhtml">底本情報</a></li>
"#);
        ol.push_str("</ol>");
        return ol;
    }
    let mut i = 0;
    while i < entries.len() {
        let e = &entries[i];
        if e.level == 2 {
            ol.push_str(&format!(
                r#"<li><a href="{}">{}</a>"#,
                toc_href(e, id_to_file),
                escape_xml(&e.text),
            ));
            // Collect nested children (level > 2 immediately following).
            let mut children: Vec<&TocEntry> = Vec::new();
            i += 1;
            while i < entries.len() && entries[i].level > 2 {
                children.push(&entries[i]);
                i += 1;
            }
            if !children.is_empty() {
                ol.push_str("\n<ol>\n");
                for c in children {
                    ol.push_str(&format!(
                        r#"<li><a href="{}">{}</a></li>
"#,
                        toc_href(c, id_to_file),
                        escape_xml(&c.text),
                    ));
                }
                ol.push_str("</ol>\n");
            }
            ol.push_str("</li>\n");
        } else {
            // Orphan child (level > 2 with no preceding h2). Emit flat.
            ol.push_str(&format!(
                r#"<li><a href="{}">{}</a></li>
"#,
                toc_href(e, id_to_file),
                escape_xml(&e.text),
            ));
            i += 1;
        }
    }
    if has_colophon {
        ol.push_str(r#"<li><a href="text/colophon.xhtml">底本情報</a></li>
"#);
    }
    ol.push_str("</ol>");
    ol
}

fn build_ncx(
    uuid: &str,
    title: &str,
    toc: &[TocEntry],
    id_to_file: &std::collections::HashMap<String, String>,
    _has_colophon: bool,
) -> String {
    let mut points = String::new();
    points.push_str(&format!(
        r#"  <navPoint id="np0" playOrder="0">
    <navLabel><text>{}</text></navLabel>
    <content src="text/title.xhtml"/>
  </navPoint>
"#,
        escape_xml(title),
    ));
    for (t, entry) in toc.iter().enumerate() {
        points.push_str(&format!(
            r#"  <navPoint id="np{}" playOrder="{}">
    <navLabel><text>{}</text></navLabel>
    <content src="{}"/>
  </navPoint>
"#,
            t + 1,
            t + 1,
            escape_xml(&entry.text),
            toc_href(entry, id_to_file),
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
<head><meta name="dtb:uid" content="urn:uuid:{}"/></head>
<docTitle><text>{}</text></docTitle>
<navMap>
{}</navMap>
</ncx>"#,
        uuid,
        escape_xml(title),
        points,
    )
}

fn build_opf(
    uuid: &str,
    title: &str,
    author: &str,
    publisher: &str,
    pub_date: &str,
    chapters: &[Chapter],
    images: &[(String, Vec<u8>)],
) -> String {
    // Aozora publisher is hardcoded upstream; pub-date is parsed from
    // colophon. Empty pub_date suppresses the `<dc:date>` element.
    let mut chapter_manifest = String::new();
    let mut chapter_spine = String::new();
    for (i, ch) in chapters.iter().enumerate() {
        chapter_manifest.push_str(&format!(
            r#"    <item id="ch{}" href="text/{}" media-type="application/xhtml+xml"/>
"#,
            i, ch.file,
        ));
        chapter_spine.push_str(&format!(
            r#"    <itemref idref="ch{}"/>
"#,
            i,
        ));
    }
    let mut image_manifest = String::new();
    for (i, (name, _)) in images.iter().enumerate() {
        let mime = mime_for_image(name);
        image_manifest.push_str(&format!(
            r#"    <item id="img{}" href="images/{}" media-type="{}"/>
"#,
            i,
            escape_xml(name),
            mime,
        ));
    }
    let modified = utc_now_iso8601();
    let publisher_xml = format!(
        "    <dc:publisher>{}</dc:publisher>\n",
        escape_xml(publisher),
    );
    let pub_date_xml = if pub_date.is_empty() {
        String::new()
    } else {
        format!("    <dc:date>{}</dc:date>\n", escape_xml(pub_date))
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0"
         unique-identifier="uid" xml:lang="ja" prefix="rendition: http://www.idpf.org/vocab/rendition/#">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">urn:uuid:{uuid}</dc:identifier>
    <dc:title>{title}</dc:title>
    <dc:creator>{author}</dc:creator>
    <dc:language>ja</dc:language>
{publisher}{date}    <dc:source>https://www.aozora.gr.jp/</dc:source>
    <meta property="dcterms:modified">{modified}</meta>
    <meta property="rendition:layout">reflowable</meta>
  </metadata>
  <manifest>
    <item id="cover" href="text/cover.xhtml" media-type="application/xhtml+xml"/>
    <item id="cover-image" href="images/cover.jpg" media-type="image/jpeg" properties="cover-image"/>
{chapter_manifest}    <item id="style" href="style.css" media-type="text/css"/>
    <item id="nav" href="toc.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
{image_manifest}  </manifest>
  <spine page-progression-direction="rtl" toc="ncx">
    <itemref idref="cover"/>
{chapter_spine}  </spine>
</package>"#,
        uuid = uuid,
        title = escape_xml(title),
        author = escape_xml(author),
        publisher = publisher_xml,
        date = pub_date_xml,
        modified = modified,
        chapter_manifest = chapter_manifest,
        image_manifest = image_manifest,
        chapter_spine = chapter_spine,
    )
}

// =========================================================================
// Colophon parsing — publisher + pub date
// =========================================================================

fn parse_pub_date(colophon: &str) -> String {
    if colophon.is_empty() {
        return String::new();
    }
    use std::sync::LazyLock;
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(\d{4})[（(].*?[）)]\s*年\s*(\d{1,2})\s*月").unwrap()
    });
    RE.captures(colophon)
        .map(|c| {
            let year = c.get(1).unwrap().as_str();
            let month: u32 = c.get(2).unwrap().as_str().parse().unwrap_or(1);
            format!("{}-{:02}", year, month)
        })
        .unwrap_or_default()
}

// =========================================================================
// Helpers
// =========================================================================

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn mime_for_image(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}

// `uuid_v5` lives in `crate::util` — shared across the EPUB exporters.

fn utc_now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    // Convert epoch seconds to YYYY-MM-DDTHH:MM:SSZ. Simple gmtime.
    let (year, month, day, hour, min, sec) = epoch_to_ymdhms(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, min, sec
    )
}

fn epoch_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    // Plain UTC conversion — sufficient for `dcterms:modified`. No DST,
    // no timezone math needed.
    let days = (secs / 86400) as i32;
    let rem = (secs % 86400) as u32;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;

    // Days since 1970-01-01 → Y/M/D using Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i32 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hour, min, sec)
}

// =========================================================================
// Templates
// =========================================================================

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

const COVER_XHTML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" xml:lang="ja">
<head><meta charset="UTF-8"/><title>Cover</title>
<style>body { margin: 0; padding: 0; text-align: center; } img { max-width: 100%; max-height: 100%; }</style>
</head>
<body epub:type="cover"><img src="../images/cover.jpg" alt="Cover"/></body>
</html>"#;

const STYLE_CSS: &str = r#"@charset "utf-8";
body {
  writing-mode: vertical-rl;
  -webkit-writing-mode: vertical-rl;
  -epub-writing-mode: vertical-rl;
  font-family: "Hiragino Mincho ProN", "Yu Mincho", "Noto Serif JP", serif;
  line-height: 1.8;
  margin: 1.5em;
  text-align: justify;
}
p {
  text-indent: 1em;
  margin: 0;
}
h1, h2, h3, h4 { text-indent: 0; }
em.sesame {
  font-style: normal;
  -webkit-text-emphasis: filled sesame;
  text-emphasis: filled sesame;
}
rt { font-size: 0.55em; }
p.indent { margin-inline-start: 1em; text-indent: 0; }
img { max-width: 100%; max-height: 100%; }
.underline { text-decoration: underline; }
.underline-double { text-decoration: underline double; }
.underline-wavy { text-decoration: underline wavy; }
.underline-dashed { text-decoration: underline dashed; }
.underline-dotted { text-decoration: underline dotted; }
.strikethrough { text-decoration: line-through; }
.strikethrough-double { text-decoration: line-through double; }
em.open-sesame { font-style: normal; -webkit-text-emphasis: open sesame; text-emphasis: open sesame; }
em.circle { font-style: normal; -webkit-text-emphasis: filled circle; text-emphasis: filled circle; }
em.open-circle { font-style: normal; -webkit-text-emphasis: open circle; text-emphasis: open circle; }
em.triangle { font-style: normal; -webkit-text-emphasis: filled triangle; text-emphasis: filled triangle; }
em.open-triangle { font-style: normal; -webkit-text-emphasis: open triangle; text-emphasis: open triangle; }
em.double-circle { font-style: normal; -webkit-text-emphasis: filled double-circle; text-emphasis: filled double-circle; }
em.batsu { font-style: normal; -webkit-text-emphasis: "×"; text-emphasis: "×"; }
.gothic { font-family: "Hiragino Kaku Gothic ProN", "Yu Gothic", "Noto Sans JP", sans-serif; }
.italic { font-style: italic; }
.yokogumi { writing-mode: horizontal-tb; -webkit-writing-mode: horizontal-tb; -epub-writing-mode: horizontal-tb; }
.keigakomi { border: 1px solid currentColor; padding: 0.2em; }
.keigakomi-dashed { border: 1px dashed currentColor; padding: 0.2em; }
.keigakomi-double { border: 3px double currentColor; padding: 0.2em; }
.colophon { margin-top: 3em; font-size: 0.85em; }
.colophon p { text-indent: 0; margin: 0.2em 0; }
"#;

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::parser_txt::Document;

    fn tiny_jpeg() -> Vec<u8> {
        // Minimal valid JPEG: 1x1 white. The bytes were dumped from
        // `printf '\xFF\xD8\xFF\xE0\x00\x10JFIF...\xFF\xD9'` — see EPUB
        // tests for shape; we don't need to render this, just ship it.
        // Synthesized via `jpeg-encoder` would also work but this is
        // smaller and dependency-free.
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06,
            0x07, 0x06, 0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D,
            0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12, 0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D,
            0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28,
            0x37, 0x29, 0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32,
            0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01,
            0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x1F, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02,
            0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x10,
            0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00,
            0x01, 0x7D, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06,
            0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42,
            0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16,
            0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37,
            0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55,
            0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73,
            0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
            0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
            0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA,
            0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6,
            0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA,
            0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xDA, 0x00, 0x08,
            0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0xFB, 0xD0, 0xFF, 0xD9,
        ]
    }

    fn sample_document() -> Document {
        Document {
            title: "テスト".to_string(),
            author: "著者".to_string(),
            body_xhtml: r#"<p>序文</p>
<h2 id="h1">第一章</h2>
<p>本文1</p>
<h2 id="h2">第二章</h2>
<p>本文2</p>
"#
            .to_string(),
            toc: vec![
                TocEntry {
                    id: "h1".to_string(),
                    level: 2,
                    text: "第一章".to_string(),
                },
                TocEntry {
                    id: "h2".to_string(),
                    level: 2,
                    text: "第二章".to_string(),
                },
            ],
            colophon: String::new(),
            referenced_images: vec![],
        }
    }

    #[test]
    fn builds_valid_zip() {
        let doc = sample_document();
        let bytes = build_epub(EpubInput {
            document: &doc,
            images: &[],
            cover_jpeg: &tiny_jpeg(),
        })
        .expect("build epub");
        assert!(bytes.starts_with(b"PK"), "zip magic missing");

        // mimetype must be the first entry and stored uncompressed: check by
        // verifying the literal `application/epub+zip` appears at the right
        // offset (after `PK\x03\x04` + 26-byte local file header + 8-byte
        // name "mimetype"). For simplicity just confirm the string is
        // present near the start.
        let head = &bytes[..200];
        assert!(
            head.windows(20).any(|w| w == b"application/epub+zip"),
            "mimetype not in zip head"
        );
    }

    #[test]
    fn chapter_split_produces_title_then_chapters_then_colophon() {
        let mut doc = sample_document();
        doc.colophon = "底本：「テスト」テスト社\n1990年初版".to_string();
        let chapters = split_into_chapters(&doc);
        assert_eq!(chapters.len(), 4); // title + 2 h2 chapters + colophon
        assert_eq!(chapters[0].file, "title.xhtml");
        assert_eq!(chapters[1].file, "ch1.xhtml");
        assert_eq!(chapters[1].title, "第一章");
        assert_eq!(chapters[2].file, "ch2.xhtml");
        assert_eq!(chapters[2].title, "第二章");
        assert_eq!(chapters[3].file, "colophon.xhtml");
    }

    #[test]
    fn id_to_file_map_resolves_toc_targets() {
        let doc = sample_document();
        let chapters = split_into_chapters(&doc);
        let map = build_id_to_file_map(&chapters);
        assert_eq!(map.get("h1").unwrap(), "ch1.xhtml");
        assert_eq!(map.get("h2").unwrap(), "ch2.xhtml");
    }

    #[test]
    fn opf_contains_metadata_and_spine() {
        let doc = sample_document();
        let bytes = build_epub(EpubInput {
            document: &doc,
            images: &[],
            cover_jpeg: &tiny_jpeg(),
        })
        .unwrap();
        // Extract OEBPS/content.opf from the zip and check key fields.
        let opf = extract_zip_entry(&bytes, "OEBPS/content.opf");
        assert!(opf.contains("<dc:title>テスト</dc:title>"), "title missing");
        assert!(opf.contains("<dc:creator>著者</dc:creator>"), "author missing");
        assert!(opf.contains(r#"xml:lang="ja""#), "lang missing");
        assert!(
            opf.contains(r#"page-progression-direction="rtl""#),
            "ppd missing"
        );
        assert!(opf.contains(r#"properties="cover-image""#));
        // Cover spine entry must NOT be `linear="no"` without a hyperlink to
        // it elsewhere — EPUB 3.3 §5.8.2 (non-linear reachability). Apple
        // Books and downstream KFX conversion both reject the violation.
        assert!(
            !opf.contains(r#"idref="cover" linear="no""#),
            "cover must be linear; non-linear cover with no inbound hyperlink fails epubcheck"
        );
    }

    #[test]
    fn parses_pub_date_from_colophon() {
        // Print publisher is no longer surfaced — `<dc:publisher>` is
        // hardcoded to 青空文庫. Only the print date is extracted, and
        // we use it as `<dc:date>` for the work.
        let d = parse_pub_date("底本：「タイトル」テスト出版社、1990（平成2）年5月1日初版");
        assert_eq!(d, "1990-05");
    }

    #[test]
    fn opf_publisher_is_aozora_bunko() {
        let doc = sample_document();
        let bytes = build_epub(EpubInput {
            document: &doc,
            images: &[],
            cover_jpeg: &tiny_jpeg(),
        })
        .unwrap();
        let opf = extract_zip_entry(&bytes, "OEBPS/content.opf");
        assert!(
            opf.contains("<dc:publisher>青空文庫</dc:publisher>"),
            "publisher should be 青空文庫, got OPF:\n{}",
            opf
        );
    }

    fn extract_zip_entry(zip_bytes: &[u8], name: &str) -> String {
        let cursor = io::Cursor::new(zip_bytes);
        let mut zip = zip::ZipArchive::new(cursor).expect("read zip");
        let mut entry = zip.by_name(name).expect("entry");
        let mut s = String::new();
        std::io::Read::read_to_string(&mut entry, &mut s).unwrap();
        s
    }
}
