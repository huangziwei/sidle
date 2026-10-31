//! Surgical in-place metadata edit for an EPUB's OPF package document.

use std::collections::HashSet;

use crate::formats::epub::edit::{EpubPackage, attr_value, attributes, escape_text, set_attr};
use crate::formats::epub::parser::{publication_date_rank, resolve_entity};
use crate::model::sort_key_survives;

/// Which OPF metadata fields to set. `None` leaves a field untouched; an empty
/// value removes the element.
#[derive(Debug, Clone, Default)]
pub struct MetadataPatch {
    pub title: Option<String>,
    /// The full ordered author list, written creator by creator.
    pub authors: Option<Vec<String>>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    /// Publication date (`<dc:date>`).
    pub date: Option<String>,
    pub asin: Option<String>,
}

impl MetadataPatch {
    /// True if the patch sets no field — [`edit_metadata`] then returns the input
    /// unchanged.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.authors.is_none()
            && self.language.is_none()
            && self.publisher.is_none()
            && self.date.is_none()
            && self.asin.is_none()
    }
}

/// Apply `patch` to the OPF of `epub_bytes`, passing every other member through.
/// Returns `epub_bytes` itself when the OPF states every patched value, and errors
/// on an unreadable EPUB or a non-UTF-8 OPF.
pub fn edit_metadata(epub_bytes: &[u8], patch: &MetadataPatch) -> std::io::Result<Vec<u8>> {
    if patch.is_empty() {
        return Ok(epub_bytes.to_vec());
    }
    let mut pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf = std::str::from_utf8(pkg.opf_bytes()?).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("OPF not UTF-8: {e}"),
        )
    })?;
    let new_opf = rewrite_metadata(opf, patch);
    if new_opf == opf {
        return Ok(epub_bytes.to_vec());
    }
    pkg.replace(&opf_path, new_opf.into_bytes());
    pkg.into_bytes()
}

/// Which of several same-named elements carries a field, as the parser reads it.
#[derive(Clone, Copy)]
enum Pick {
    /// The first with text (`dc:title`).
    First,
    /// The last (`dc:language`, `dc:publisher`).
    Last,
    /// The best-ranked by [`publication_date_rank`] (`dc:date`).
    Publication,
}

/// Apply every set field of `patch` to the OPF text.
fn rewrite_metadata(opf: &str, patch: &MetadataPatch) -> String {
    let mut s = opf.to_string();
    if let Some(t) = &patch.title {
        s = set_single(&s, "title", t, Pick::First);
    }
    if let Some(l) = &patch.language {
        s = set_single(&s, "language", l, Pick::Last);
    }
    if let Some(p) = &patch.publisher {
        s = set_single(&s, "publisher", p, Pick::Last);
    }
    if let Some(d) = &patch.date {
        s = set_single(&s, "date", d, Pick::Publication);
    }
    if let Some(a) = &patch.asin {
        s = set_asin(&s, a);
    }
    if let Some(authors) = &patch.authors {
        s = set_creators(&s, authors);
    }
    s
}

/// One `<dc:…>text</dc:…>` element: where it starts, where its text runs, and
/// where it ends.
struct Element {
    start: usize,
    text: (usize, usize),
    end: usize,
}

impl Element {
    fn start_tag<'a>(&self, opf: &'a str) -> &'a str {
        &opf[self.start..self.text.0]
    }

    /// The text as the parser reads it: references resolved, ends trimmed.
    fn text(&self, opf: &str) -> String {
        decode(&opf[self.text.0..self.text.1])
    }
}

/// Every `<dc:{tag}>` element that has a closing tag, in document order.
fn dc_elements(opf: &str, tag: &str) -> Vec<Element> {
    let open = format!("<dc:{tag}");
    let close = format!("</dc:{tag}>");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = opf[from..].find(&open) {
        let start = from + rel;
        from = start + open.len();
        let after = opf.as_bytes().get(from).copied().unwrap_or(b'>');
        if !(after == b'>' || after.is_ascii_whitespace()) {
            continue;
        }
        let Some(gt) = opf[start..].find('>').map(|p| start + p) else {
            break;
        };
        if opf.as_bytes()[gt - 1] == b'/' {
            from = gt + 1;
            continue;
        }
        let Some(text_end) = opf[gt + 1..].find(&close).map(|p| gt + 1 + p) else {
            break;
        };
        from = text_end + close.len();
        out.push(Element {
            start,
            text: (gt + 1, text_end),
            end: from,
        });
    }
    out
}

/// The element of `elements` that carries the field.
fn pick<'a>(opf: &str, elements: &'a [Element], which: Pick) -> Option<&'a Element> {
    match which {
        Pick::First => elements
            .iter()
            .find(|e| !e.text(opf).is_empty())
            .or(elements.first()),
        Pick::Last => elements.last(),
        Pick::Publication => {
            let mut best: Option<(u8, &Element)> = None;
            for e in elements {
                let event = local_attr(e.start_tag(opf), "event");
                if let Some(rank) = publication_date_rank(event.as_deref())
                    && !e.text(opf).is_empty()
                    && best.is_none_or(|(kept, _)| rank < kept)
                {
                    best = Some((rank, e));
                }
            }
            best.map(|(_, e)| e)
        }
    }
}

/// Set the field `<dc:{tag}>` carries, keeping its attributes: an empty `value`
/// removes the element, and a `tag` with no element gains one.
fn set_single(opf: &str, tag: &str, value: &str, which: Pick) -> String {
    let elements = dc_elements(opf, tag);
    match pick(opf, &elements, which) {
        Some(e) if value.is_empty() => remove_element(opf, e),
        Some(e) if e.text(opf) == value => opf.to_string(),
        Some(e) => format!(
            "{}{}{}",
            &opf[..e.text.0],
            escape_text(value),
            &opf[e.text.1..]
        ),
        None if value.is_empty() => opf.to_string(),
        None => inject_into_metadata(opf, &format!("<dc:{tag}>{}</dc:{tag}>", escape_text(value))),
    }
}

/// Rewrite the `<dc:creator>` run to `authors`, position by position: a renamed
/// creator keeps its element, and its `file-as` only when [`sort_key_survives`];
/// surplus creators go with their refinements, and extra `authors` follow the last.
fn set_creators(opf: &str, authors: &[String]) -> String {
    let creators = dc_elements(opf, "creator");
    let mut taken = ids_in(opf);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    // (creator id, whether every refinement of it goes or only the name-derived ones)
    let mut stale: Vec<(String, bool)> = Vec::new();

    for (i, c) in creators.iter().enumerate() {
        let start_tag = c.start_tag(opf);
        let id = attr_value(start_tag, "id");
        let Some(new) = authors.get(i) else {
            let (s, e) = line_span(opf, c.start, c.end);
            edits.push((s, e, String::new()));
            stale.extend(id.map(|id| (id, true)));
            continue;
        };
        let old = c.text(opf);
        if old == *new {
            continue;
        }
        let keeps_sort_key = sort_key_survives(&old, new);
        let tag = if keeps_sort_key {
            start_tag.to_string()
        } else {
            without_file_as(start_tag)
        };
        edits.push((c.start, c.text.1, format!("{tag}{}", escape_text(new))));
        if !keeps_sort_key {
            stale.extend(id.map(|id| (id, false)));
        }
    }

    let extra = authors.get(creators.len()..).unwrap_or_default();
    if let Some(last) = creators.last()
        && !extra.is_empty()
    {
        let indent = indent_of(opf, last.start);
        let added: String = extra
            .iter()
            .map(|a| {
                format!(
                    "\n{indent}<dc:creator id=\"{}\">{}</dc:creator>",
                    free_id(&mut taken),
                    escape_text(a)
                )
            })
            .collect();
        edits.push((last.end, last.end, added));
    }

    edits.sort_by_key(|&(start, _, _)| std::cmp::Reverse(start));
    let mut out = opf.to_string();
    for (start, end, text) in edits {
        out.replace_range(start..end, &text);
    }
    let mut out = strip_refines(&out, |id, property| {
        stale
            .iter()
            .any(|(s, every)| s == id && (*every || name_derived(property)))
    });
    if creators.is_empty() {
        for a in extra {
            let element = format!(
                "<dc:creator id=\"{}\">{}</dc:creator>",
                free_id(&mut taken),
                escape_text(a)
            );
            out = inject_into_metadata(&out, &element);
        }
    }
    out
}

/// Whether `property` spells the name it refines (`file-as`, `alternate-script`).
fn name_derived(property: &str) -> bool {
    matches!(
        property.rsplit(':').next(),
        Some("file-as" | "alternate-script")
    )
}

/// `tag` without its `file-as` attribute, whatever its prefix.
fn without_file_as(tag: &str) -> String {
    attributes(tag)
        .iter()
        .filter(|a| a.name.rsplit(':').next() == Some("file-as"))
        .fold(tag.to_string(), |t, a| set_attr(&t, &a.name, None))
}

/// The value of the attribute whose local name is `local`, any prefix.
fn local_attr(tag: &str, local: &str) -> Option<String> {
    attributes(tag)
        .into_iter()
        .find(|a| a.name.rsplit(':').next() == Some(local))
        .map(|a| a.value)
}

/// Every `id` attribute value in `opf`.
fn ids_in(opf: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut from = 0;
    while let Some(rel) = opf[from..].find("id=") {
        let pos = from + rel;
        from = pos + 3;
        if pos == 0 || !opf.as_bytes()[pos - 1].is_ascii_whitespace() {
            continue;
        }
        let rest = &opf[from..];
        if let Some(q) = rest.chars().next().filter(|q| *q == '"' || *q == '\'')
            && let Some(end) = rest[1..].find(q)
        {
            out.insert(rest[1..1 + end].to_string());
        }
    }
    out
}

/// The first `creatorN` not in `taken`, claimed.
fn free_id(taken: &mut HashSet<String>) -> String {
    let id = (1..)
        .map(|n| format!("creator{n}"))
        .find(|id| !taken.contains(id))
        .expect("an unbounded range finds a free id");
    taken.insert(id.clone());
    id
}

/// The indentation of the line `pos` starts on, when only whitespace precedes it.
fn indent_of(opf: &str, pos: usize) -> &str {
    let line_start = opf[..pos].rfind('\n').map_or(0, |p| p + 1);
    let lead = &opf[line_start..pos];
    if lead.trim().is_empty() { lead } else { "    " }
}

/// `[start, end)` widened to its whole line, newline included, when the rest of
/// that line is blank.
fn line_span(opf: &str, start: usize, end: usize) -> (usize, usize) {
    let line_start = opf[..start].rfind('\n').map_or(0, |p| p + 1);
    let line_end = opf[end..].find('\n').map_or(opf.len(), |p| end + p + 1);
    if opf[line_start..start].trim().is_empty() && opf[end..line_end].trim().is_empty() {
        (line_start, line_end)
    } else {
        (start, end)
    }
}

/// `opf` without `e`, and without the refinements that pointed at it.
fn remove_element(opf: &str, e: &Element) -> String {
    let id = attr_value(e.start_tag(opf), "id");
    let (start, end) = line_span(opf, e.start, e.end);
    let out = format!("{}{}", &opf[..start], &opf[end..]);
    match id {
        Some(id) => strip_refines(&out, |target, _| target == id),
        None => out,
    }
}

/// Remove every `<meta refines="#id" property="…">` (and self-closing form) for
/// which `drop(id, property)` holds, each with its line when it stood alone.
fn strip_refines(opf: &str, drop: impl Fn(&str, &str) -> bool) -> String {
    let mut out = String::with_capacity(opf.len());
    let mut copied = 0;
    let mut scan = 0;
    while let Some(rel) = opf[scan..].find("<meta") {
        let mstart = scan + rel;
        // Tag-boundary guard: skip `<metadata`, match only the `<meta` element.
        let after = opf.as_bytes().get(mstart + 5).copied().unwrap_or(b'>');
        if !(after == b'>' || after == b'/' || after.is_ascii_whitespace()) {
            scan = mstart + 5;
            continue;
        }
        let Some(gt) = opf[mstart..].find('>').map(|p| mstart + p) else {
            break;
        };
        let elem_end = if opf.as_bytes()[gt - 1] == b'/' {
            gt + 1
        } else {
            match opf[gt + 1..].find("</meta>") {
                Some(c) => gt + 1 + c + "</meta>".len(),
                None => gt + 1,
            }
        };
        let tag = &opf[mstart..=gt];
        let property = attr_value(tag, "property").unwrap_or_default();
        let hit = attr_value(tag, "refines")
            .as_deref()
            .and_then(|r| r.strip_prefix('#'))
            .is_some_and(|id| drop(id, &property));
        if hit {
            let (start, end) = line_span(opf, mstart, elem_end);
            out.push_str(&opf[copied..start]);
            copied = end;
            scan = end;
        } else {
            scan = elem_end;
        }
    }
    out.push_str(&opf[copied..]);
    out
}

/// XML text with its entity and character references resolved and its ends
/// trimmed, as the parser reads an element's text.
fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp + 1..];
        match tail
            .find(';')
            .and_then(|semi| resolve_entity(&tail[..semi]).map(|r| (semi, r)))
        {
            Some((semi, resolved)) => {
                out.push_str(&resolved);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    crate::util::trim_markup_space(&out).to_string()
}

/// Set the ASIN identifier's text: replace the `scheme="ASIN"` (or `id="asin"`)
/// `<dc:identifier>`, or append one.
fn set_asin(opf: &str, asin: &str) -> String {
    let mut from = 0;
    while let Some(rel) = opf[from..].find("<dc:identifier") {
        let start = from + rel;
        let Some(gtrel) = opf[start..].find('>') else {
            break;
        };
        let gt = start + gtrel;
        let tag = opf[start..gt].to_ascii_lowercase();
        let is_asin = tag.contains("scheme=\"asin\"")
            || tag.contains("scheme='asin'")
            || tag.contains("scheme=\"mobi-asin\"")
            || tag.contains("scheme='mobi-asin'")
            || tag.contains("id=\"asin\"")
            || tag.contains("id='asin'");
        if is_asin && opf.as_bytes()[gt.saturating_sub(1)] != b'/' {
            let content_start = gt + 1;
            if let Some(cerel) = opf[content_start..].find("</dc:identifier>") {
                let ce = content_start + cerel;
                return format!(
                    "{}{}{}",
                    &opf[..content_start],
                    escape_text(asin),
                    &opf[ce..]
                );
            }
        }
        from = gt + 1;
    }
    inject_into_metadata(
        opf,
        &format!(
            "<dc:identifier opf:scheme=\"ASIN\">{}</dc:identifier>",
            escape_text(asin)
        ),
    )
}

/// Insert `element` just before `</metadata>`, indented; an `opf` without
/// `</metadata>` comes back unchanged.
fn inject_into_metadata(opf: &str, element: &str) -> String {
    match opf.rfind("</metadata>") {
        Some(pos) => format!("{}  {element}\n{}", &opf[..pos], &opf[pos..]),
        None => opf.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::Book;
    use crate::model::Format;

    const EPUB2: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>Old Title</dc:title>
    <dc:creator opf:role="aut" opf:file-as="Doe, Jane">Doe, Jane</dc:creator>
    <dc:identifier id="uid" opf:scheme="ISBN">9780000000000</dc:identifier>
    <dc:date>2020-02-01</dc:date>
    <dc:date opf:event="modification">2020-01-07</dc:date>
    <dc:publisher>Old Press</dc:publisher>
    <dc:language>en</dc:language>
    <meta name="cover" content="cov"/>
  </metadata>
  <manifest>
    <item href="ch1.xhtml" id="ch1" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
  </spine>
</package>
"#;

    const EPUB3_CREATORS: &str = "<metadata>\n    \
        <dc:creator id=\"creator1\">Doe, Jane</dc:creator>\n    \
        <meta refines=\"#creator1\" property=\"role\" scheme=\"marc:relators\">aut</meta>\n    \
        <meta refines=\"#creator1\" property=\"file-as\">Doe, Jane</meta>\n    \
        <dc:creator id=\"creator2\">Roe, John</dc:creator>\n    \
        <meta refines=\"#creator2\" property=\"file-as\">Roe, John</meta>\n    \
        <dc:language>en</dc:language>\n</metadata>";

    fn epub_with(opf: &str) -> Vec<u8> {
        use zip::write::SimpleFileOptions;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default();
        let mut put = |name: &str, body: &[u8], opts: SimpleFileOptions| {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body).unwrap();
        };
        put("mimetype", b"application/epub+zip", stored);
        put(
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            deflated,
        );
        put("OEBPS/content.opf", opf.as_bytes(), deflated);
        put(
            "OEBPS/ch1.xhtml",
            br#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>c</title></head><body><p>Text.</p></body></html>"#,
            deflated,
        );
        zip.finish().unwrap().into_inner()
    }

    fn reopen(bytes: &[u8]) -> Book {
        Book::from_bytes(bytes, Format::Epub).expect("edited EPUB opens")
    }

    fn opf_of(bytes: &[u8]) -> String {
        let pkg = EpubPackage::parse(bytes).unwrap();
        String::from_utf8(pkg.opf_bytes().unwrap().to_vec()).unwrap()
    }

    #[test]
    fn a_flipped_author_changes_only_the_creator_text() {
        let epub = epub_with(EPUB2);
        let patch = MetadataPatch {
            authors: Some(vec!["Jane Doe".into()]),
            ..Default::default()
        };
        let out = edit_metadata(&epub, &patch).unwrap();
        assert_eq!(
            opf_of(&out),
            EPUB2.replace(">Doe, Jane</dc:creator>", ">Jane Doe</dc:creator>"),
            "the role and the sort key stay, every other line is untouched"
        );
        let after = reopen(&out);
        assert_eq!(after.metadata().authors, vec!["Jane Doe"]);
        assert_eq!(after.metadata().author_sorts, vec!["Doe, Jane"]);
    }

    #[test]
    fn a_different_author_loses_the_old_sort_key_but_keeps_the_role() {
        let out = set_creators(EPUB2, &["Ann Writer".to_string()]);
        assert!(
            out.contains("<dc:creator opf:role=\"aut\">Ann Writer</dc:creator>"),
            "{out}"
        );
        assert!(!out.contains("Doe, Jane"));
    }

    #[test]
    fn epub3_refinements_follow_the_same_rule() {
        let out = set_creators(
            EPUB3_CREATORS,
            &["Jane Doe".to_string(), "Ann Writer".to_string()],
        );
        assert!(out.contains("<dc:creator id=\"creator1\">Jane Doe</dc:creator>"));
        assert!(
            out.contains("<meta refines=\"#creator1\" property=\"file-as\">Doe, Jane</meta>"),
            "a reordered name keeps its sort key"
        );
        assert!(out.contains("<dc:creator id=\"creator2\">Ann Writer</dc:creator>"));
        assert!(
            !out.contains("Roe, John"),
            "a replaced author's sort key goes"
        );
        assert!(!out.contains("\n\n"), "no blank line left behind: {out:?}");
    }

    #[test]
    fn surplus_creators_go_with_every_refinement() {
        let out = set_creators(EPUB3_CREATORS, &["Doe, Jane".to_string()]);
        assert_eq!(out.matches("<dc:creator").count(), 1);
        assert!(!out.contains("#creator2"), "{out}");
        assert!(
            out.contains("<meta refines=\"#creator1\" property=\"role\""),
            "the kept creator keeps its role"
        );
        assert!(!out.contains("\n\n"), "no blank line left behind: {out:?}");
    }

    #[test]
    fn extra_authors_follow_the_last_creator_under_fresh_ids() {
        let out = set_creators(
            EPUB3_CREATORS,
            &[
                "Doe, Jane".to_string(),
                "Roe, John".to_string(),
                "Ann Writer".to_string(),
            ],
        );
        assert!(
            out.contains(
                "<dc:creator id=\"creator2\">Roe, John</dc:creator>\n    <dc:creator id=\"creator3\">Ann Writer</dc:creator>"
            ),
            "{out}"
        );
        assert_eq!(
            reopen_opf(&out).metadata.authors,
            ["Doe, Jane", "Roe, John", "Ann Writer"]
        );
    }

    #[test]
    fn a_book_without_creators_gains_them() {
        let opf = "<metadata>\n  <dc:title>T</dc:title>\n</metadata>";
        let out = set_creators(opf, &["Ann Writer".to_string()]);
        assert!(out.contains("<dc:creator id=\"creator1\">Ann Writer</dc:creator>"));
    }

    #[test]
    fn clearing_the_authors_removes_every_creator() {
        let out = set_creators(EPUB3_CREATORS, &[]);
        assert!(!out.contains("<dc:creator"));
        assert!(!out.contains("refines"), "{out}");
        assert!(out.contains("<dc:language>en</dc:language>"));
    }

    #[test]
    fn the_publication_date_is_written_where_the_parser_reads_it() {
        let out = set_single(EPUB2, "date", "2019-05-05", Pick::Publication);
        assert!(out.contains("<dc:date>2019-05-05</dc:date>"));
        assert!(
            out.contains("<dc:date opf:event=\"modification\">2020-01-07</dc:date>"),
            "the modification stamp is not the publication date"
        );
        assert_eq!(
            reopen_opf(&out).metadata.date.as_deref(),
            Some("2019-05-05")
        );
    }

    #[test]
    fn an_empty_value_removes_the_element() {
        let out = set_single(EPUB2, "publisher", "", Pick::Last);
        assert!(!out.contains("dc:publisher"));
        assert!(!out.contains("\n\n"), "the line goes with it: {out:?}");
        let out = set_single(EPUB2, "date", "", Pick::Publication);
        assert!(!out.contains("2020-02-01"));
        assert!(out.contains("2020-01-07"));
        assert_eq!(reopen_opf(&out).metadata.date, None);
    }

    #[test]
    fn the_last_language_is_the_one_replaced() {
        let opf = "<metadata>\n<dc:language>ja</dc:language>\n<dc:language>zh-TW</dc:language>\n</metadata>";
        let out = set_single(opf, "language", "zh-Hant", Pick::Last);
        assert!(out.contains("<dc:language>ja</dc:language>"));
        assert!(out.contains("<dc:language>zh-Hant</dc:language>"));
    }

    #[test]
    fn a_patch_the_file_already_carries_returns_the_input() {
        let epub = epub_with(EPUB2);
        let patch = MetadataPatch {
            title: Some("Old Title".into()),
            authors: Some(vec!["Doe, Jane".into()]),
            language: Some("en".into()),
            publisher: Some("Old Press".into()),
            date: Some("2020-02-01".into()),
            asin: None,
        };
        assert_eq!(edit_metadata(&epub, &patch).unwrap(), epub);
    }

    #[test]
    fn an_edit_reopens_with_the_new_values() {
        let epub = epub_with(EPUB2);
        let patch = MetadataPatch {
            title: Some("New Title".into()),
            publisher: Some("New Press".into()),
            ..Default::default()
        };
        let after = reopen(&edit_metadata(&epub, &patch).unwrap());
        assert_eq!(after.metadata().title, "New Title");
        assert_eq!(after.metadata().publisher.as_deref(), Some("New Press"));
        assert_eq!(after.metadata().authors, vec!["Doe, Jane"], "untouched");
        assert_eq!(after.metadata().date.as_deref(), Some("2020-02-01"));
    }

    #[test]
    fn edit_asin_updates_existing_identifier() {
        let opf = EPUB2.replace(
            "<dc:language>",
            "<dc:identifier opf:scheme=\"ASIN\">B000000000</dc:identifier>\n    <dc:language>",
        );
        let patch = MetadataPatch {
            asin: Some("B0TESTASIN".into()),
            ..Default::default()
        };
        let after = reopen(&edit_metadata(&epub_with(&opf), &patch).unwrap());
        assert_eq!(after.metadata().asin.as_deref(), Some("B0TESTASIN"));
    }

    #[test]
    fn empty_patch_returns_input_unchanged() {
        let epub = epub_with(EPUB2);
        let out = edit_metadata(&epub, &MetadataPatch::default()).unwrap();
        assert_eq!(out, epub, "an empty patch is a no-op");
    }

    #[test]
    fn non_epub_bytes_error() {
        let patch = MetadataPatch {
            title: Some("x".into()),
            ..Default::default()
        };
        assert!(edit_metadata(b"not an epub", &patch).is_err());
    }

    #[test]
    fn escapes_xml_metacharacters() {
        let patch = MetadataPatch {
            title: Some("Q&A:<Draft>".into()),
            authors: Some(vec!["Smith & Jones".into()]),
            ..Default::default()
        };
        let out = edit_metadata(&epub_with(EPUB2), &patch).unwrap();
        let opf = opf_of(&out);
        assert!(
            opf.contains("Q&amp;A:&lt;Draft&gt;"),
            "value escaped in the OPF"
        );
        assert!(!opf.contains("Q&A:<Draft>"), "no raw metacharacters");
        assert_eq!(reopen(&out).metadata().title, "Q&A:<Draft>");
        assert_eq!(reopen(&out).metadata().authors, vec!["Smith & Jones"]);
    }

    #[test]
    fn an_escaped_name_reads_as_its_text() {
        let opf = "<metadata>\n<dc:creator>Smith &amp; Jones</dc:creator>\n</metadata>";
        assert_eq!(
            set_creators(opf, &["Smith & Jones".to_string()]),
            opf,
            "an unchanged name is left byte for byte"
        );
    }

    fn reopen_opf(opf: &str) -> crate::formats::epub::OpfData {
        let wrapped = if opf.contains("<package") {
            opf.to_string()
        } else {
            format!(
                "<?xml version=\"1.0\"?>\n<package xmlns=\"http://www.idpf.org/2007/opf\" xmlns:opf=\"http://www.idpf.org/2007/opf\" version=\"3.0\">\n{}\n<manifest/><spine/></package>",
                opf.replace(
                    "<metadata>",
                    "<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">"
                )
            )
        };
        crate::formats::epub::parse_opf(&wrapped).unwrap()
    }
}
