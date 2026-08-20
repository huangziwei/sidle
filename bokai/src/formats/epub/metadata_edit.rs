//! Surgical in-place metadata edit for an EPUB's OPF package document.
//!
//! The EPUB analog of [`crate::formats::kfx::metadata_edit`]: sets
//! title / authors / language / publisher / date / ASIN on the OPF's Dublin Core
//! elements without touching the rest of the book. Where the KFX side rewrites
//! Ion fragments, this does targeted text edits on the OPF XML — the same
//! string-surgery approach the cover writer uses — through the shared
//! [`EpubPackage`] harness: every other member passes through untouched.
//!
//! A single-valued field (`<dc:title>` / `<dc:language>` / `<dc:publisher>` /
//! `<dc:date>`) has its text replaced in place (or the element is appended to
//! `<metadata>` when absent). Authors replace the whole run of `<dc:creator>`
//! elements. ASIN patches the `scheme="ASIN"` identifier. A `None` field is left
//! untouched (matching the KFX primitive — v1 sets, it does not clear).
//!
//! Scope (v1): targets the near-universal `dc:`-prefixed Dublin Core form real
//! EPUBs use. Replacing the author list also prunes any `<meta refines="#…">`
//! refinement that pointed at a replaced creator — leaving it would dangle the
//! `refines` fragment (epubcheck flags it, and it undoes the validity the
//! exporter's `id="creatorN"` scheme guarantees). A single-valued field's edit
//! replaces only the element's text, keeping its `id`, so a refinement on it
//! still resolves; the refinement's own value (e.g. a title `file-as` sort key)
//! is left as-is rather than re-derived.

use crate::formats::epub::edit::{EpubPackage, attr_value, escape_text};

/// Which OPF metadata fields to set. `None` leaves a field untouched.
#[derive(Debug, Clone, Default)]
pub struct MetadataPatch {
    pub title: Option<String>,
    /// Replaces the full ordered author list (`<dc:creator>` run).
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

/// Apply `patch` to the EPUB, returning the repackaged bytes. Edits only the OPF;
/// every other member passes through. Returns the input unchanged for an empty
/// patch. Errors if the bytes aren't a readable EPUB or the OPF isn't UTF-8.
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
    pkg.replace(&opf_path, new_opf.into_bytes());
    pkg.into_bytes()
}

/// Apply every set field of `patch` to the OPF text.
fn rewrite_metadata(opf: &str, patch: &MetadataPatch) -> String {
    let mut s = opf.to_string();
    if let Some(t) = &patch.title {
        s = set_single(&s, "title", t);
    }
    if let Some(l) = &patch.language {
        s = set_single(&s, "language", l);
    }
    if let Some(p) = &patch.publisher {
        s = set_single(&s, "publisher", p);
    }
    if let Some(d) = &patch.date {
        s = set_single(&s, "date", d);
    }
    if let Some(a) = &patch.asin {
        s = set_asin(&s, a);
    }
    if let Some(authors) = &patch.authors {
        s = set_creators(&s, authors);
    }
    s
}

/// Replace the text of the first `<dc:{tag}…>…</dc:{tag}>`, keeping its
/// attributes; append `<dc:{tag}>value</dc:{tag}>` to `<metadata>` if absent.
fn set_single(opf: &str, tag: &str, value: &str) -> String {
    match dc_content_span(opf, tag) {
        Some((cs, ce)) => format!("{}{}{}", &opf[..cs], escape_text(value), &opf[ce..]),
        None => inject_into_metadata(opf, &format!("<dc:{tag}>{}</dc:{tag}>", escape_text(value))),
    }
}

/// `(content_start, content_end)` of the first non-self-closing
/// `<dc:{tag}…>content</dc:{tag}>`.
fn dc_content_span(opf: &str, tag: &str) -> Option<(usize, usize)> {
    let open = format!("<dc:{tag}");
    let close = format!("</dc:{tag}>");
    let mut from = 0;
    loop {
        let start = from + opf[from..].find(&open)?;
        let after = opf
            .as_bytes()
            .get(start + open.len())
            .copied()
            .unwrap_or(b' ');
        if after == b'>' || after.is_ascii_whitespace() {
            let gt = start + opf[start..].find('>')?;
            if opf.as_bytes()[gt.saturating_sub(1)] == b'/' {
                from = gt + 1; // self-closing — no text content; keep looking
                continue;
            }
            let content_start = gt + 1;
            let ce = content_start + opf[content_start..].find(&close)?;
            return Some((content_start, ce));
        }
        from = start + open.len();
    }
}

/// Replace the run of `<dc:creator>` elements with `authors`, one element each,
/// at the position of the first existing creator (or appended to `<metadata>`).
///
/// New creators carry the exporter's `id="creatorN"` scheme, and any
/// `<meta refines="#…">` refinement (sort-key `file-as`, `role`, …) that pointed
/// at a *replaced* creator is pruned: it described the old creator, and leaving
/// it would dangle the `refines` fragment. New creators carry no refinement (a
/// bare `<dc:creator>` is authorship by default, and this primitive has no
/// sort-key input to synthesize one from).
fn set_creators(opf: &str, authors: &[String]) -> String {
    let creators = authors
        .iter()
        .enumerate()
        .map(|(i, a)| {
            format!(
                "<dc:creator id=\"creator{}\">{}</dc:creator>",
                i + 1,
                escape_text(a)
            )
        })
        .collect::<Vec<_>>()
        .join("\n    ");

    let open = "<dc:creator";
    let close = "</dc:creator>";
    let mut out = String::with_capacity(opf.len() + creators.len());
    let mut copied = 0;
    let mut first_pos: Option<usize> = None;
    let mut old_ids: Vec<String> = Vec::new();
    let mut scan = 0;
    while let Some(rel) = opf[scan..].find(open) {
        let cstart = scan + rel;
        let after = opf
            .as_bytes()
            .get(cstart + open.len())
            .copied()
            .unwrap_or(b' ');
        if !(after == b'>' || after.is_ascii_whitespace()) {
            scan = cstart + open.len();
            continue;
        }
        let Some(gtrel) = opf[cstart..].find('>') else {
            break;
        };
        let gt = cstart + gtrel;
        if let Some(id) = attr_value(&opf[cstart..gt], "id") {
            old_ids.push(id);
        }
        let elem_end = if opf.as_bytes()[gt.saturating_sub(1)] == b'/' {
            gt + 1 // self-closing creator
        } else {
            match opf[gt..].find(close) {
                Some(c) => gt + c + close.len(),
                None => gt + 1,
            }
        };
        out.push_str(&opf[copied..cstart]);
        first_pos.get_or_insert(out.len());
        copied = elem_end;
        scan = elem_end;
    }
    out.push_str(&opf[copied..]);

    let out = match first_pos {
        Some(pos) => {
            out.insert_str(pos, &creators);
            out
        }
        None => inject_into_metadata(&out, &creators),
    };
    strip_meta_refines(&out, &old_ids)
}

/// Remove every `<meta … refines="#<id>" …>…</meta>` (and self-closing form)
/// whose `refines` fragment names one of `ids`, taking the element's whole
/// indented line with it. Drops refinements orphaned when their `<dc:creator>`
/// target is replaced. No-op when `ids` is empty.
fn strip_meta_refines(opf: &str, ids: &[String]) -> String {
    if ids.is_empty() {
        return opf.to_string();
    }
    let targets: Vec<String> = ids.iter().map(|id| format!("#{id}")).collect();
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
        let Some(gtrel) = opf[mstart..].find('>') else {
            break;
        };
        let gt = mstart + gtrel;
        let elem_end = if opf.as_bytes()[gt.saturating_sub(1)] == b'/' {
            gt + 1 // self-closing <meta …/>
        } else {
            match opf[gt + 1..].find("</meta>") {
                Some(c) => gt + 1 + c + "</meta>".len(),
                None => gt + 1,
            }
        };
        let refines_match =
            attr_value(&opf[mstart..=gt], "refines").is_some_and(|v| targets.contains(&v));
        if refines_match {
            // Take the whole line: leading indentation back to the previous
            // newline (only when that gap is blank) and the trailing newline.
            let prev_nl = opf[..mstart].rfind('\n').map_or(0, |p| p + 1);
            let line_start = if opf[prev_nl..mstart].trim().is_empty() {
                prev_nl
            } else {
                mstart
            };
            let line_end = if opf[elem_end..].starts_with('\n') {
                elem_end + 1
            } else {
                elem_end
            };
            out.push_str(&opf[copied..line_start]);
            copied = line_end;
            scan = line_end;
        } else {
            scan = elem_end;
        }
    }
    out.push_str(&opf[copied..]);
    out
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

/// Insert `element` just before `</metadata>` (indented). A no-op returning the
/// input if the OPF has no `</metadata>` (malformed — nothing to edit).
fn inject_into_metadata(opf: &str, element: &str) -> String {
    match opf.rfind("</metadata>") {
        Some(pos) => format!("{}  {element}\n{}", &opf[..pos], &opf[pos..]),
        None => opf.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Book;
    use crate::model::Format;

    const FIXTURE: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

    fn reopen(bytes: &[u8]) -> Book {
        Book::from_bytes(bytes, Format::Epub).expect("edited EPUB opens")
    }

    /// Title + authors on the real fixture: the edit reopens with the new values,
    /// leaves language + the cover intact, and stays a valid EPUB.
    #[test]
    fn edit_title_and_authors_on_fixture() {
        let epub = std::fs::read(FIXTURE).expect("read fixture");
        let before = reopen(&epub);
        let before_lang = before.metadata().language.clone();
        let before_cover = before.metadata().cover_image.clone();

        let patch = MetadataPatch {
            title: Some("新しいタイトル".into()),
            authors: Some(vec!["著者 一".into(), "著者 二".into()]),
            ..Default::default()
        };
        let out = edit_metadata(&epub, &patch).expect("edit");
        let after = reopen(&out);

        assert_eq!(after.metadata().title, "新しいタイトル");
        assert_eq!(after.metadata().authors, vec!["著者 一", "著者 二"]);
        assert_eq!(after.metadata().language, before_lang, "language untouched");
        assert_eq!(
            after.metadata().cover_image,
            before_cover,
            "cover untouched"
        );
    }

    /// A single-field edit changes only that field.
    #[test]
    fn edit_single_field_leaves_rest() {
        let epub = std::fs::read(FIXTURE).expect("read fixture");
        let before = reopen(&epub);
        let patch = MetadataPatch {
            publisher: Some("新潮社".into()),
            ..Default::default()
        };
        let after = reopen(&edit_metadata(&epub, &patch).unwrap());
        assert_eq!(after.metadata().publisher.as_deref(), Some("新潮社"));
        assert_eq!(after.metadata().title, before.metadata().title);
        assert_eq!(after.metadata().authors, before.metadata().authors);
    }

    /// ASIN patches the existing `scheme="ASIN"` identifier (the fixture has one).
    #[test]
    fn edit_asin_updates_existing_identifier() {
        let epub = std::fs::read(FIXTURE).expect("read fixture");
        let patch = MetadataPatch {
            asin: Some("B0TESTASIN".into()),
            ..Default::default()
        };
        let after = reopen(&edit_metadata(&epub, &patch).unwrap());
        assert_eq!(after.metadata().asin.as_deref(), Some("B0TESTASIN"));
    }

    #[test]
    fn empty_patch_returns_input_unchanged() {
        let epub = std::fs::read(FIXTURE).expect("read fixture");
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

    /// XML metacharacters in a value are escaped so the OPF stays well-formed and
    /// the `&`/`<`/`>` survive the parse round-trip. (Whitespace adjacent to an
    /// entity is dropped by the OPF parser's `trim_text`, so the fixture value
    /// keeps the metacharacters tight against their neighbours.)
    #[test]
    fn escapes_xml_metacharacters() {
        let epub = std::fs::read(FIXTURE).expect("read fixture");
        let patch = MetadataPatch {
            title: Some("Q&A:<Draft>".into()),
            ..Default::default()
        };
        let out = edit_metadata(&epub, &patch).unwrap();
        // The written OPF is well-formed (metacharacters escaped, not raw).
        let pkg = EpubPackage::parse(&out).unwrap();
        let opf = String::from_utf8(pkg.opf_bytes().unwrap().to_vec()).unwrap();
        assert!(
            opf.contains("Q&amp;A:&lt;Draft&gt;"),
            "value escaped in the OPF"
        );
        assert!(!opf.contains("Q&A:<Draft>"), "no raw metacharacters");
        // And it round-trips through the parser.
        assert_eq!(reopen(&out).metadata().title, "Q&A:<Draft>");
    }

    /// Setting a single-valued field the OPF lacks appends it.
    #[test]
    fn set_single_injects_when_absent() {
        let opf = "<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n<dc:title>T</dc:title>\n</metadata>";
        let out = set_single(opf, "publisher", "Acme");
        assert!(out.contains("<dc:publisher>Acme</dc:publisher>"));
        assert!(out.contains("<dc:title>T</dc:title>"), "existing kept");
    }

    /// Replacing authors drops every old creator and injects the new run, each
    /// element carrying the exporter's `id="creatorN"` scheme.
    #[test]
    fn set_creators_replaces_the_whole_run() {
        let opf = "<metadata>\n<dc:creator id=\"a\">Old One</dc:creator>\n<dc:creator>Old Two</dc:creator>\n<dc:title>T</dc:title>\n</metadata>";
        let out = set_creators(opf, &["New".to_string()]);
        assert!(out.contains("<dc:creator id=\"creator1\">New</dc:creator>"));
        assert!(
            !out.contains("Old One") && !out.contains("Old Two"),
            "old creators gone"
        );
        assert!(out.contains("<dc:title>T</dc:title>"), "title preserved");
        assert_eq!(out.matches("<dc:creator").count(), 1, "exactly one creator");
    }

    /// Replacing authors prunes the `<meta refines="#creatorN">` refinements that
    /// pointed at the old creators, so no `refines` fragment is left dangling —
    /// the defect that silently made an author-edited book epubcheck-dirty. This
    /// is the exact shape the exporter emits.
    #[test]
    fn set_creators_prunes_orphaned_refines() {
        let opf = "<metadata>\n    \
            <dc:creator id=\"creator1\">Old</dc:creator>\n    \
            <meta refines=\"#creator1\" property=\"role\" scheme=\"marc:relators\">aut</meta>\n    \
            <meta refines=\"#creator1\" property=\"file-as\">Old, The</meta>\n    \
            <dc:language>en</dc:language>\n</metadata>";
        let out = set_creators(opf, &["New".to_string()]);
        assert!(
            out.contains("<dc:creator id=\"creator1\">New</dc:creator>"),
            "new creator carries the id"
        );
        assert!(
            !out.contains("refines=\"#creator1\""),
            "orphaned refines pruned: {out}"
        );
        assert!(!out.contains("Old, The"), "stale file-as gone");
        assert!(
            out.contains("<dc:language>en</dc:language>"),
            "unrelated element untouched"
        );
        // No blank line left where the two meta lines were removed.
        assert!(!out.contains("\n\n"), "no doubled newline: {out:?}");
    }
}
