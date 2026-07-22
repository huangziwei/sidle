//! Surgical TOC repair for an EPUB: derive a chapter list from the book's own
//! in-book Contents page (or its headings) and write it into the EPUB 3 nav doc
//! **and** the EPUB 2 NCX, in place — synthesizing either when the book has none.
//!
//! The EPUB analog of [`crate::formats::kfx::toc_repair`]. Where the KFX side rewrites
//! `nav_container` Ion, this edits the two XML documents a reader consults: it
//! **splices** a fresh `<nav epub:type="toc">` into an existing nav doc (leaving
//! its landmarks / page-list untouched) or **synthesizes** a nav doc, and the
//! same for the NCX `<navMap>`; a synthesized doc is registered in the OPF
//! manifest (and the NCX in the spine `toc`). Because EPUB hrefs *are* the nav
//! targets, no eid resolution is needed — the proposer pairs each Contents-page
//! link's text with its href directly.
//!
//! [`propose_toc`] reads the chapter list; [`set_toc`] writes a caller-supplied
//! one; [`repair_toc`] composes them. Public hrefs are **absolute zip paths**
//! (e.g. `"OEBPS/c1.xhtml#ch1"`); [`set_toc`] rebases each to the nav/NCX
//! document it writes. The importer re-derives the KFX nav from whichever of the
//! two is richer, so one repair fixes both the EPUB's own nav and the KFX
//! derived from it.

use std::collections::HashSet;
use std::io;

use crate::formats::epub::edit::{EpubPackage, attr_value, escape_attr, escape_text};
use crate::formats::epub::{OpfData, parse_nav_landmarks, parse_opf, parse_opf_guide};
use crate::model::{LandmarkType, TocEntry};
use crate::util::{decode_text, extract_xml_encoding, percent_decode};

/// Minimum distinct chapter links for a page to count as a real Contents page (or
/// headings for the fallback). Below this, a stray cross-reference or two is just
/// noise. A shade lower than the validator's evidence gate because repair is
/// opt-in — the user asked for a TOC and reviews the proposal.
const MIN_CHAPTER_LINKS: usize = 3;

// ---------------------------------------------------------------------------
// Proposer
// ---------------------------------------------------------------------------

/// Derive a chapter list from the EPUB's own structure, for [`set_toc`]. Uses the
/// richest of: the book's own declared TOC (NCX / nav doc, when it lists real
/// chapters) and the densest in-body Contents-page link cluster; falls back to
/// the spine's text headings. Each entry's `href` is an absolute zip path. Empty
/// when nothing usable is found.
pub fn propose_toc(epub_bytes: &[u8]) -> io::Result<Vec<TocEntry>> {
    let pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_str = decode_text(pkg.opf_bytes()?, extract_xml_encoding(pkg.opf_bytes()?));
    let opf = parse_opf(&opf_str).map_err(io::Error::other)?;
    Ok(propose_from_pkg(&pkg, &opf, &opf_base, &opf_str))
}

/// The spine documents, as `(absolute zip path, lowercase basename)` in reading
/// order.
fn spine_docs(opf: &OpfData, opf_base: &str) -> Vec<(String, String)> {
    opf.spine_ids
        .iter()
        .filter_map(|id| opf.manifest.get(id))
        .map(|(href, _)| {
            let abs = format!("{opf_base}{}", percent_decode(href));
            let base = basename(&abs);
            (abs, base)
        })
        .collect()
}

fn propose_from_pkg(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_base: &str,
    opf_str: &str,
) -> Vec<TocEntry> {
    // 1. The book's own authored TOC (NCX / nav doc). When it lists real
    //    chapters it's the most reliable source — and often the *only* one:
    //    image-heavy books (JP light novels) render both the 目次 and the chapter
    //    headings as images, so there are no links or heading text to derive from,
    //    yet the NCX carries the full chapter list. A deficient (chapterless)
    //    declared TOC has too few real chapters to qualify and is skipped.
    let declared = existing_declared_toc(pkg, opf, opf_base);
    let declared_ok = count_chapters(&declared) >= MIN_CHAPTER_LINKS;

    // 2. The densest in-body Contents-page link cluster.
    let contents = contents_page_links(pkg, opf, opf_base, opf_str);
    let contents_ok = contents.len() >= MIN_CHAPTER_LINKS;

    // Prefer whichever qualifying source is richer.
    match (declared_ok, contents_ok) {
        (true, true) => {
            if flat_count(&declared) >= contents.len() {
                declared
            } else {
                contents
            }
        }
        (true, false) => declared,
        (false, true) => contents,
        // 3. Last resort: one entry per spine doc that opens with a text heading.
        (false, false) => {
            let headings = propose_from_headings(pkg, &spine_docs(opf, opf_base));
            if headings.len() >= MIN_CHAPTER_LINKS {
                headings
            } else {
                Vec::new()
            }
        }
    }
}

/// The densest cluster of internal chapter links across the spine — a Contents
/// page. Prefers the page a `toc` landmark marks (guards against a link-dense
/// chapter out-linking the real Contents page). Empty if none carries links.
fn contents_page_links(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_base: &str,
    opf_str: &str,
) -> Vec<TocEntry> {
    let spine = spine_docs(opf, opf_base);
    let spine_files: HashSet<&str> = spine.iter().map(|(_, b)| b.as_str()).collect();
    let landmark_file = toc_landmark_basename(pkg, opf, opf_base, opf_str);

    let mut best: Vec<TocEntry> = Vec::new();
    let mut landmark_hit: Option<Vec<TocEntry>> = None;
    for (abs, base) in &spine {
        let Some(bytes) = pkg.get(abs) else { continue };
        let xhtml = decode_text(bytes, extract_xml_encoding(bytes));
        let doc_dir = dir_of(abs);
        let links = collect_internal_links(&xhtml, &doc_dir, &spine_files, base);
        let entries = dedup_entries(links);
        if entries.len() > best.len() {
            best = entries.clone();
        }
        if landmark_hit.is_none()
            && landmark_file.as_deref() == Some(base.as_str())
            && entries.len() >= MIN_CHAPTER_LINKS
        {
            landmark_hit = Some(entries);
        }
    }
    landmark_hit.unwrap_or(best)
}

/// The book's declared TOC (the richer of its NCX and EPUB-3 nav doc), with every
/// href resolved to an absolute zip path so it can be re-emitted by [`set_toc`].
fn existing_declared_toc(pkg: &EpubPackage, opf: &OpfData, opf_base: &str) -> Vec<TocEntry> {
    let read = |href: &str, parse: fn(&str) -> io::Result<Vec<TocEntry>>| -> Vec<TocEntry> {
        let abs = format!("{opf_base}{}", percent_decode(href));
        let Some(bytes) = pkg.get(&abs) else {
            return Vec::new();
        };
        let text = decode_text(bytes, extract_xml_encoding(bytes));
        let entries = parse(&text).unwrap_or_default();
        // NCX/nav hrefs are relative to that document's own directory.
        rebase_to_absolute(&entries, &dir_of(&abs))
    };
    let ncx = opf
        .ncx_href
        .as_deref()
        .map(|h| read(h, crate::formats::epub::parse_ncx))
        .unwrap_or_default();
    let nav = opf
        .nav_href
        .as_deref()
        .map(|h| read(h, crate::formats::epub::parse_nav_toc))
        .unwrap_or_default();
    if flat_count(&ncx) >= flat_count(&nav) {
        ncx
    } else {
        nav
    }
}

/// Resolve every entry's (doc-relative) href to an absolute zip path, recursively.
fn rebase_to_absolute(entries: &[TocEntry], base_dir: &str) -> Vec<TocEntry> {
    entries
        .iter()
        .map(|e| {
            let href = if e.href.starts_with('#') || e.href.is_empty() {
                e.href.clone()
            } else {
                resolve_href(base_dir, &e.href)
            };
            let mut t = TocEntry::new(e.title.clone(), href);
            t.children = rebase_to_absolute(&e.children, base_dir);
            t
        })
        .collect()
}

/// Count entries whose label is a real chapter (not front-matter boilerplate),
/// recursively — the gate for reusing an existing declared TOC.
fn count_chapters(entries: &[TocEntry]) -> usize {
    entries
        .iter()
        .map(|e| {
            let this = usize::from(!crate::validate::source::toc::is_front_matter(&e.title));
            this + count_chapters(&e.children)
        })
        .sum()
}

/// Total entries in a tree, counting nested children.
fn flat_count(entries: &[TocEntry]) -> usize {
    entries.iter().map(|e| 1 + flat_count(&e.children)).sum()
}

/// `(label, absolute-href)` for every link this doc makes to *another* spine doc
/// — a Contents page's chapter links. Hrefs are resolved against `doc_dir` to an
/// absolute zip path (fragment preserved).
fn collect_internal_links(
    xhtml: &str,
    doc_dir: &str,
    spine_files: &HashSet<&str>,
    self_file: &str,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = xhtml;
    while let Some(p) = rest.find("<a ") {
        rest = &rest[p + 3..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..end];
        if let Some(href) = attr_value(tag, "href") {
            let file = basename(&href);
            if file != self_file && spine_files.contains(file.as_str()) {
                let label = rest[end + 1..]
                    .find("</a>")
                    .map(|c| strip_tags(&rest[end + 1..end + 1 + c]))
                    .unwrap_or_default();
                out.push((label, resolve_href(doc_dir, &href)));
            }
        }
        rest = &rest[end..];
    }
    out
}

/// One entry per spine doc whose first heading (`<h1>`..`<h6>`) carries text: the
/// heading text as label, the doc (plus the heading's `id`, if any) as target.
fn propose_from_headings(pkg: &EpubPackage, spine: &[(String, String)]) -> Vec<TocEntry> {
    let mut out = Vec::new();
    for (abs, _) in spine {
        let Some(bytes) = pkg.get(abs) else { continue };
        let xhtml = decode_text(bytes, extract_xml_encoding(bytes));
        if let Some((label, id)) = first_heading(&xhtml) {
            let href = match id {
                Some(id) => format!("{abs}#{id}"),
                None => abs.clone(),
            };
            out.push(TocEntry::new(label, href));
        }
    }
    out
}

/// Collapse `(label, href)` links to one [`TocEntry`] per distinct target,
/// keeping document order and the first non-empty label seen. A link whose label
/// is blank still anchors an entry (labeled by its filename as a last resort).
fn dedup_entries(links: Vec<(String, String)>) -> Vec<TocEntry> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (label, href) in links {
        if !seen.insert(href.clone()) {
            continue;
        }
        let label = clean_label(&label);
        let label = if label.is_empty() {
            basename(&href)
        } else {
            label
        };
        out.push(TocEntry::new(label, href));
    }
    out
}

/// The spine-doc basename a `toc`-type landmark points at (EPUB 3 nav landmarks
/// preferred, EPUB 2 OPF `<guide>` fallback).
fn toc_landmark_basename(
    pkg: &EpubPackage,
    opf: &OpfData,
    opf_base: &str,
    opf_str: &str,
) -> Option<String> {
    let find = |lms: &[crate::model::Landmark]| {
        lms.iter()
            .find(|l| l.landmark_type == LandmarkType::Toc)
            .map(|l| basename(&l.href))
    };
    if let Some(nav_href) = &opf.nav_href
        && let Some(bytes) = pkg.get(&format!("{opf_base}{}", percent_decode(nav_href)))
    {
        let nav = decode_text(bytes, extract_xml_encoding(bytes));
        if let Ok(lms) = parse_nav_landmarks(&nav)
            && let Some(h) = find(&lms)
        {
            return Some(h);
        }
    }
    parse_opf_guide(opf_str).ok().as_deref().and_then(find)
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Write `entries` into the EPUB's nav doc and NCX, in place — splicing over an
/// existing toc / navMap, or synthesizing (and registering in the OPF) when the
/// book has none. `entries` hrefs are absolute zip paths.
///
/// Errors if `entries` is empty or the bytes aren't a readable EPUB.
pub fn set_toc(epub_bytes: &[u8], entries: &[TocEntry]) -> io::Result<Vec<u8>> {
    if entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to write an empty TOC",
        ));
    }
    let mut pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_raw =
        decode_text(pkg.opf_bytes()?, extract_xml_encoding(pkg.opf_bytes()?)).into_owned();
    let opf = parse_opf(&opf_raw).map_err(io::Error::other)?;

    let title = if opf.metadata.title.is_empty() {
        "Contents"
    } else {
        &opf.metadata.title
    };
    let lang = if opf.metadata.language.is_empty() {
        "en"
    } else {
        &opf.metadata.language
    };
    let uid = if opf.metadata.identifier.is_empty() {
        "urn:uuid:bokai-repaired-toc"
    } else {
        &opf.metadata.identifier
    };

    let mut opf_str = opf_raw;
    let mut opf_dirty = false;

    // --- nav doc (EPUB 3) ---
    let nav_abs = opf
        .nav_href
        .as_deref()
        .map(|h| format!("{opf_base}{}", percent_decode(h)))
        .unwrap_or_else(|| format!("{opf_base}nav.xhtml"));
    let nav_dir = dir_of(&nav_abs);
    let toc_nav = render_toc_nav(entries, &nav_dir);
    let new_nav = match pkg.get(&nav_abs) {
        Some(bytes) => splice_toc_nav(&decode_text(bytes, extract_xml_encoding(bytes)), &toc_nav),
        None => {
            // Synthesize and register in the OPF manifest.
            let id = free_id(&opf, "nav");
            let rel = relativize(&opf_base, &nav_abs);
            opf_str = add_manifest_item(&opf_str, &id, &rel, "application/xhtml+xml", Some("nav"));
            opf_dirty = true;
            render_nav_doc(&toc_nav, lang, title)
        }
    };
    pkg.set(&nav_abs, new_nav.into_bytes());

    // --- NCX (EPUB 2) ---
    let ncx_abs = opf
        .ncx_href
        .as_deref()
        .map(|h| format!("{opf_base}{}", percent_decode(h)))
        .unwrap_or_else(|| format!("{opf_base}toc.ncx"));
    let ncx_dir = dir_of(&ncx_abs);
    let navmap = render_navmap(entries, &ncx_dir);
    let new_ncx = match pkg.get(&ncx_abs) {
        Some(bytes) => splice_navmap(&decode_text(bytes, extract_xml_encoding(bytes)), &navmap),
        None => {
            let id = free_id(&opf, "ncx");
            let rel = relativize(&opf_base, &ncx_abs);
            opf_str = add_manifest_item(&opf_str, &id, &rel, "application/x-dtbncx+xml", None);
            opf_str = ensure_spine_toc(&opf_str, &id);
            opf_dirty = true;
            render_ncx(&navmap, uid, title, depth(entries))
        }
    };
    pkg.set(&ncx_abs, new_ncx.into_bytes());

    if opf_dirty {
        pkg.replace(&opf_path, opf_str.into_bytes());
    }
    pkg.into_bytes()
}

/// One-call repair: [`propose_toc`] then [`set_toc`]. Errors if no chapter list
/// can be derived, or the bytes aren't a readable EPUB.
pub fn repair_toc(epub_bytes: &[u8]) -> io::Result<Vec<u8>> {
    let entries = propose_toc(epub_bytes)?;
    if entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no in-book chapter list found to rebuild the TOC from",
        ));
    }
    set_toc(epub_bytes, &entries)
}

// ---------------------------------------------------------------------------
// XML rendering
// ---------------------------------------------------------------------------

/// Render `<nav epub:type="toc">…</nav>`, hrefs rebased relative to `base_dir`.
fn render_toc_nav(entries: &[TocEntry], base_dir: &str) -> String {
    let mut s =
        String::from("<nav epub:type=\"toc\" role=\"doc-toc\" id=\"toc\">\n<h1>Contents</h1>\n");
    render_ol(entries, base_dir, &mut s);
    s.push_str("</nav>");
    s
}

fn render_ol(entries: &[TocEntry], base_dir: &str, out: &mut String) {
    out.push_str("<ol>\n");
    for e in entries {
        let href = relativize(base_dir, &e.href);
        out.push_str(&format!(
            "<li><a href=\"{}\">{}</a>",
            escape_attr(&href),
            escape_text(crate::util::trim_markup_space(&e.title))
        ));
        if e.children.is_empty() {
            out.push_str("</li>\n");
        } else {
            out.push('\n');
            render_ol(&e.children, base_dir, out);
            out.push_str("</li>\n");
        }
    }
    out.push_str("</ol>\n");
}

/// A minimal EPUB 3 nav document wrapping `toc_nav`.
fn render_nav_doc(toc_nav: &str, lang: &str, title: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<!DOCTYPE html>\n\
<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" lang=\"{lang}\" xml:lang=\"{lang}\">\n\
<head>\n<meta charset=\"utf-8\"/>\n<title>{}</title>\n</head>\n\
<body>\n{toc_nav}\n</body>\n</html>\n",
        escape_text(title)
    )
}

/// Render `<navMap>…</navMap>` with sequential `playOrder`, hrefs rebased to
/// `base_dir`.
fn render_navmap(entries: &[TocEntry], base_dir: &str) -> String {
    let mut s = String::from("<navMap>\n");
    let mut order = 0usize;
    render_navpoints(entries, base_dir, &mut order, &mut s);
    s.push_str("</navMap>");
    s
}

fn render_navpoints(entries: &[TocEntry], base_dir: &str, order: &mut usize, out: &mut String) {
    for e in entries {
        *order += 1;
        let n = *order;
        let src = relativize(base_dir, &e.href);
        out.push_str(&format!(
            "<navPoint id=\"navPoint-{n}\" playOrder=\"{n}\">\n\
<navLabel><text>{}</text></navLabel>\n\
<content src=\"{}\"/>\n",
            escape_text(crate::util::trim_markup_space(&e.title)),
            escape_attr(&src)
        ));
        if !e.children.is_empty() {
            render_navpoints(&e.children, base_dir, order, out);
        }
        out.push_str("</navPoint>\n");
    }
}

/// A minimal NCX wrapping `navmap`.
fn render_ncx(navmap: &str, uid: &str, title: &str, depth: usize) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<!DOCTYPE ncx PUBLIC \"-//NISO//DTD ncx 2005-1//EN\" \"http://www.daisy.org/z3986/2005/ncx-2005-1.dtd\">\n\
<ncx xmlns=\"http://www.daisy.org/z3986/2005/ncx/\" version=\"2005-1\">\n\
<head>\n\
<meta name=\"dtb:uid\" content=\"{}\"/>\n\
<meta name=\"dtb:depth\" content=\"{depth}\"/>\n\
<meta name=\"dtb:totalPageCount\" content=\"0\"/>\n\
<meta name=\"dtb:maxPageNumber\" content=\"0\"/>\n\
</head>\n\
<docTitle><text>{}</text></docTitle>\n\
{navmap}\n</ncx>\n",
        escape_attr(uid),
        escape_text(title)
    )
}

// ---------------------------------------------------------------------------
// XML splicing (preserve everything but the one element we rewrite)
// ---------------------------------------------------------------------------

/// Replace the existing `<nav epub:type="toc">…</nav>` with `new_nav`, or insert
/// `new_nav` before `</body>` if the doc has no toc nav yet.
fn splice_toc_nav(nav_xhtml: &str, new_nav: &str) -> String {
    if let Some((start, end)) = find_toc_nav_span(nav_xhtml) {
        format!("{}{new_nav}{}", &nav_xhtml[..start], &nav_xhtml[end..])
    } else {
        insert_before(nav_xhtml, "</body>", &format!("{new_nav}\n"))
    }
}

/// Replace the existing `<navMap>…</navMap>` with `new_navmap`, or insert it
/// before `</ncx>` if the NCX has none.
fn splice_navmap(ncx_xml: &str, new_navmap: &str) -> String {
    if let Some((start, end)) = find_element_span(ncx_xml, "navMap") {
        format!("{}{new_navmap}{}", &ncx_xml[..start], &ncx_xml[end..])
    } else {
        insert_before(ncx_xml, "</ncx>", &format!("{new_navmap}\n"))
    }
}

/// Byte range of the whole `<nav …epub:type/​type contains toc…>…</nav>`.
fn find_toc_nav_span(xhtml: &str) -> Option<(usize, usize)> {
    let bytes = xhtml.as_bytes();
    let mut from = 0;
    while let Some(rel) = xhtml[from..].find("<nav") {
        let tag_start = from + rel;
        let after = bytes.get(tag_start + 4).copied().unwrap_or(b' ');
        if !(after == b'>' || after.is_ascii_whitespace()) {
            from = tag_start + 4;
            continue;
        }
        let Some(gt) = xhtml[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + gt + 1;
        if nav_tag_is_toc(&xhtml[tag_start..tag_end]) {
            let close = xhtml[tag_end..].find("</nav>")?;
            return Some((tag_start, tag_end + close + "</nav>".len()));
        }
        from = tag_end;
    }
    None
}

/// Byte range of the first `<name …>…</name>` element (no nesting assumed).
fn find_element_span(xml: &str, name: &str) -> Option<(usize, usize)> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let start = xml.find(&open)?;
    // Confirm a real tag boundary (`<navMap>` or `<navMap …>`), not a prefix.
    let after = xml
        .as_bytes()
        .get(start + open.len())
        .copied()
        .unwrap_or(b' ');
    if !(after == b'>' || after.is_ascii_whitespace()) {
        return None;
    }
    let end_rel = xml[start..].find(&close)?;
    Some((start, start + end_rel + close.len()))
}

fn nav_tag_is_toc(start_tag: &str) -> bool {
    ["epub:type", "type"].iter().any(|key| {
        attr_value(start_tag, key).is_some_and(|v| v.split_whitespace().any(|t| t == "toc"))
    })
}

/// Insert `insertion` immediately before the last occurrence of `anchor`
/// (case-sensitive); append it if the anchor is absent.
fn insert_before(haystack: &str, anchor: &str, insertion: &str) -> String {
    match haystack.rfind(anchor) {
        Some(pos) => format!("{}{insertion}{}", &haystack[..pos], &haystack[pos..]),
        None => format!("{haystack}{insertion}"),
    }
}

// ---------------------------------------------------------------------------
// OPF editing (register a synthesized nav doc / NCX)
// ---------------------------------------------------------------------------

/// A manifest id not already in use, from `preferred` (`preferred`, then
/// `preferred-2`, …).
fn free_id(opf: &OpfData, preferred: &str) -> String {
    if !opf.manifest.contains_key(preferred) {
        return preferred.to_string();
    }
    (2..)
        .map(|n| format!("{preferred}-{n}"))
        .find(|id| !opf.manifest.contains_key(id))
        .unwrap_or_else(|| format!("{preferred}-bokai"))
}

/// Insert an `<item>` into the OPF `<manifest>`, before `</manifest>`.
fn add_manifest_item(
    opf: &str,
    id: &str,
    href: &str,
    media_type: &str,
    properties: Option<&str>,
) -> String {
    let props = properties
        .map(|p| format!(" properties=\"{}\"", escape_attr(p)))
        .unwrap_or_default();
    let item = format!(
        "  <item id=\"{}\" href=\"{}\" media-type=\"{}\"{}/>\n",
        escape_attr(id),
        escape_attr(href),
        escape_attr(media_type),
        props
    );
    insert_before(opf, "</manifest>", &item)
}

/// Ensure the OPF `<spine>` carries `toc="{ncx_id}"` (add it if absent; leave any
/// existing `toc` reference alone).
fn ensure_spine_toc(opf: &str, ncx_id: &str) -> String {
    let Some(start) = opf.find("<spine") else {
        return opf.to_string();
    };
    let Some(gt) = opf[start..].find('>') else {
        return opf.to_string();
    };
    let tag_end = start + gt;
    let tag = &opf[start..tag_end];
    if attr_value(tag, "toc").is_some() {
        return opf.to_string(); // already references an NCX
    }
    format!(
        "{}<spine toc=\"{}\"{}",
        &opf[..start],
        escape_attr(ncx_id),
        &opf[start + "<spine".len()..]
    )
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// The directory portion of a zip path, with a trailing `/` (empty at root).
fn dir_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/"),
        None => String::new(),
    }
}

/// Lowercased filename of an href: `#fragment`/`?query` dropped, last path
/// segment, `%20`→space. Matches the TOC detector, for spine-membership tests.
fn basename(href: &str) -> String {
    let no_frag = href.split(['#', '?']).next().unwrap_or(href);
    let file = no_frag.rsplit('/').next().unwrap_or(no_frag);
    file.replace("%20", " ").to_ascii_lowercase()
}

/// Split an href into `(path, fragment)` where the fragment keeps its leading
/// `#` (empty when there is none).
fn split_fragment(href: &str) -> (&str, &str) {
    match href.find('#') {
        Some(i) => (&href[..i], &href[i..]),
        None => (href, ""),
    }
}

/// Resolve `href` (relative to `base_dir`) to an absolute zip path, collapsing
/// `.`/`..` and percent-decoding. A pure-fragment href resolves to itself.
fn resolve_href(base_dir: &str, href: &str) -> String {
    let (path, frag) = split_fragment(href);
    if path.is_empty() {
        return href.to_string();
    }
    let path = percent_decode(path);
    let mut stack: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    format!("{}{frag}", stack.join("/"))
}

/// Rewrite an absolute zip path as an href relative to `from_dir`.
fn relativize(from_dir: &str, abs_target: &str) -> String {
    let (path, frag) = split_fragment(abs_target);
    let from: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (name, to_dirs) = match to.split_last() {
        Some((n, d)) => (*n, d),
        None => return format!("{path}{frag}"),
    };
    let mut i = 0;
    while i < from.len() && i < to_dirs.len() && from[i] == to_dirs[i] {
        i += 1;
    }
    let mut parts: Vec<&str> = std::iter::repeat_n("..", from.len() - i).collect();
    parts.extend_from_slice(&to_dirs[i..]);
    parts.push(name);
    format!("{}{frag}", parts.join("/"))
}

/// Collapse a raw link text to a nav label: ASCII whitespace runs → one space
/// (full-width spacing in JP titles preserved), then trim.
fn clean_label(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len());
    let mut prev_space = false;
    for c in raw.chars() {
        if matches!(c, ' ' | '\n' | '\r' | '\t') {
            if !prev_space {
                s.push(' ');
                prev_space = true;
            }
        } else {
            s.push(c);
            prev_space = false;
        }
    }
    // Trim the same ASCII set the collapse above uses. `str::trim` would drop a
    // leading or trailing U+3000, contradicting the preservation this function
    // exists to provide.
    crate::util::trim_markup_space(&s).to_string()
}

/// First `<h1>`..`<h6>`'s `(text, id)` — the heading's stripped text and its
/// `id` attribute, if any. `None` when the doc has no heading with text.
fn first_heading(xhtml: &str) -> Option<(String, Option<String>)> {
    let body = xhtml.split_once("<body").map(|(_, b)| b).unwrap_or(xhtml);
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'<'
            && (bytes[i + 1] == b'h' || bytes[i + 1] == b'H')
            && (b'1'..=b'6').contains(&bytes[i + 2])
        {
            let after = bytes.get(i + 3).copied().unwrap_or(b' ');
            if after == b'>' || after.is_ascii_whitespace() {
                let gt = body[i..].find('>')?;
                let tag = &body[i..i + gt];
                let id = attr_value(tag, "id");
                let close_lower = format!("</h{}>", (bytes[i + 2] as char));
                let content_start = i + gt + 1;
                let close = body[content_start..]
                    .to_ascii_lowercase()
                    .find(&close_lower)?;
                let text = clean_label(&strip_tags(&body[content_start..content_start + close]));
                if !text.is_empty() {
                    return Some((text, id));
                }
            }
        }
        i += 1;
    }
    None
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
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

/// Max nesting depth of the tree (a flat list is depth 1).
fn depth(entries: &[TocEntry]) -> usize {
    entries
        .iter()
        .map(|e| 1 + depth(&e.children))
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Book;
    use crate::model::Format;
    use std::io::Write;

    const FIXTURE: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

    /// Build a no-TOC EPUB: a Contents page links to 6 chapters, but the OPF
    /// declares no nav doc and no NCX. This is the "no toc epub" repair case.
    fn no_toc_epub() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut add = |name: &str, body: &str| {
                zip.start_file(name, stored).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            };
            add("mimetype", "application/epub+zip");
            add(
                "META-INF/container.xml",
                r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            );
            let mut manifest = String::from(
                r#"<item id="toc-page" href="toc.xhtml" media-type="application/xhtml+xml"/>"#,
            );
            let mut spine = String::from(r#"<itemref idref="toc-page"/>"#);
            let mut toc_links = String::new();
            for n in 1..=6 {
                manifest.push_str(&format!(
                    r#"<item id="c{n}" href="c{n}.xhtml" media-type="application/xhtml+xml"/>"#
                ));
                spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
                toc_links.push_str(&format!(r#"<li><a href="c{n}.xhtml">Chapter {n}</a></li>"#));
                add(
                    &format!("OEBPS/c{n}.xhtml"),
                    &format!(
                        r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>C{n}</title></head><body><h1 id="h{n}">Chapter {n}</h1><p>body {n}</p></body></html>"#
                    ),
                );
            }
            add(
                "OEBPS/content.opf",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>No TOC Book</dc:title><dc:language>en</dc:language><dc:identifier id="uid">urn:uuid:test-book</dc:identifier></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
                ),
            );
            add(
                "OEBPS/toc.xhtml",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>Contents</title></head><body><h1>Contents</h1><ul>{toc_links}</ul></body></html>"#
                ),
            );
            zip.finish().unwrap();
        }
        buf
    }

    /// The headline case: a no-TOC EPUB is repaired. `propose_toc` finds the 6
    /// chapters, `set_toc` synthesizes a nav doc + NCX (registered in the OPF),
    /// and the result opens with a real 6-entry TOC that validates OK.
    #[test]
    fn repairs_a_no_toc_epub() {
        let epub = no_toc_epub();

        // Originally deficient: a chapterless declared TOC with in-book chapters.
        let before = crate::validate::source::toc::validate(&epub).expect("validate");
        assert_eq!(before.nav_count, 0, "starts with no declared TOC");

        let proposed = propose_toc(&epub).expect("propose");
        let labels: Vec<&str> = proposed.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Chapter 1",
                "Chapter 2",
                "Chapter 3",
                "Chapter 4",
                "Chapter 5",
                "Chapter 6"
            ]
        );
        assert_eq!(proposed[0].href, "OEBPS/c1.xhtml");

        let out = repair_toc(&epub).expect("repair");

        // Reopens as a Book with the 6-chapter TOC.
        let book = Book::from_bytes(&out, Format::Epub).expect("repaired EPUB opens");
        let toc_titles: Vec<&str> = book.toc().iter().map(|e| e.title.as_str()).collect();
        assert_eq!(
            toc_titles,
            [
                "Chapter 1",
                "Chapter 2",
                "Chapter 3",
                "Chapter 4",
                "Chapter 5",
                "Chapter 6"
            ]
        );

        // And the validator now passes.
        let after = crate::validate::source::toc::validate(&out).expect("validate");
        assert_eq!(after.verdict, crate::validate::source::toc::Verdict::Ok);
    }

    /// The synthesized OPF registers both docs so a reader finds them.
    #[test]
    fn synthesized_docs_are_registered_in_opf() {
        let out = repair_toc(&no_toc_epub()).expect("repair");
        let pkg = EpubPackage::parse(&out).expect("parse");
        assert!(pkg.contains("OEBPS/nav.xhtml"), "nav doc created");
        assert!(pkg.contains("OEBPS/toc.ncx"), "NCX created");
        let opf = String::from_utf8(pkg.opf_bytes().unwrap().to_vec()).unwrap();
        assert!(opf.contains("properties=\"nav\""), "nav registered");
        assert!(opf.contains("application/x-dtbncx+xml"), "NCX registered");
        assert!(opf.contains("<spine toc=\""), "spine points at the NCX");
    }

    /// On a book that already has a nav doc + NCX (the fixture), `set_toc`
    /// splices over them in place — the result reopens with exactly our entries.
    #[test]
    fn set_toc_splices_existing_docs() {
        let epub = std::fs::read(FIXTURE).expect("read fixture");
        let entries = vec![
            TocEntry::new("第一章", "OEBPS/c16.xhtml"),
            TocEntry::new("第二章", "OEBPS/c2A.xhtml"),
            TocEntry::new("第三章", "OEBPS/c5S.xhtml"),
        ];
        let out = set_toc(&epub, &entries).expect("set_toc");

        let book = Book::from_bytes(&out, Format::Epub).expect("opens");
        let titles: Vec<&str> = book.toc().iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["第一章", "第二章", "第三章"]);
        // No duplicate nav docs were created — the existing ones were reused.
        let pkg = EpubPackage::parse(&out).expect("parse");
        assert!(pkg.contains("OEBPS/nav.xhtml") && pkg.contains("OEBPS/toc.ncx"));
    }

    /// Nesting round-trips: a Part with child chapters comes back nested.
    #[test]
    fn set_toc_preserves_nesting() {
        let epub = std::fs::read(FIXTURE).expect("read fixture");
        let entries = vec![TocEntry {
            title: "第一部".into(),
            href: "OEBPS/c16.xhtml".into(),
            children: vec![
                TocEntry::new("第一章", "OEBPS/c2A.xhtml"),
                TocEntry::new("第二章", "OEBPS/c5S.xhtml"),
            ],
            play_order: None,
            target: None,
        }];
        let out = set_toc(&epub, &entries).expect("set_toc");
        let book = Book::from_bytes(&out, Format::Epub).expect("opens");
        let toc = book.toc();
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].title, "第一部");
        let kids: Vec<&str> = toc[0].children.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(kids, ["第一章", "第二章"]);
    }

    /// Build an EPUB whose 目次 and chapter headings are images (no links, no
    /// heading text) but which ships a real NCX listing the chapters — the shape
    /// of image-heavy JP light novels.
    fn image_toc_and_ncx_epub() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut add = |name: &str, body: &str| {
                zip.start_file(name, stored).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            };
            add("mimetype", "application/epub+zip");
            add(
                "META-INF/container.xml",
                r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            );
            // 目次 page: an image, no links.
            add(
                "OEBPS/Text/mokuji.xhtml",
                r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>目次</title></head><body class="contents"><div><p><img src="../Images/toc.jpg" alt=""/></p></div></body></html>"#,
            );
            let mut manifest = String::from(
                r#"<item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/><item id="mokuji" href="Text/mokuji.xhtml" media-type="application/xhtml+xml"/>"#,
            );
            let mut spine = String::from(r#"<itemref idref="mokuji"/>"#);
            let mut navpoints = String::from(
                r#"<navPoint id="np0" playOrder="1"><navLabel><text>目次</text></navLabel><content src="Text/mokuji.xhtml"/></navPoint>"#,
            );
            let titles = [
                "残酷童話　うつくし姫",
                "第零話　あせろら",
                "第零話　かれん",
                "第零話　つばさ",
                "あとがき",
            ];
            for (i, title) in titles.iter().enumerate() {
                let n = i + 1;
                manifest.push_str(&format!(
                    r#"<item id="c{n}" href="Text/c{n}.xhtml" media-type="application/xhtml+xml"/>"#
                ));
                spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
                navpoints.push_str(&format!(
                    r#"<navPoint id="np{n}" playOrder="{}"><navLabel><text>{title}</text></navLabel><content src="Text/c{n}.xhtml#link{n}"/></navPoint>"#,
                    n + 1
                ));
                // Chapter heading is an image — no text to derive a label from.
                add(
                    &format!("OEBPS/Text/c{n}.xhtml"),
                    &format!(
                        r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>c{n}</title></head><body class="image-page"><div class="main"><h2 id="link{n}"><img src="../Images/p{n}.jpg" alt=""/></h2></div></body></html>"#
                    ),
                );
            }
            add(
                "OEBPS/content.opf",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>業物語</dc:title><dc:language>ja</dc:language><dc:identifier id="uid">urn:uuid:image-toc</dc:identifier></metadata><manifest>{manifest}</manifest><spine toc="ncx">{spine}</spine><guide><reference type="toc" title="目次" href="Text/mokuji.xhtml"/></guide></package>"#
                ),
            );
            add(
                "OEBPS/toc.ncx",
                &format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1"><head><meta name="dtb:uid" content="urn:uuid:image-toc"/></head><docTitle><text>業物語</text></docTitle><navMap>{navpoints}</navMap></ncx>"#
                ),
            );
            zip.finish().unwrap();
        }
        buf
    }

    /// The regression: a book with an image 目次 and image chapter headings (no
    /// links, no heading text) still proposes its chapters — from the declared
    /// NCX — instead of coming up empty. Repair then writes them so it reopens
    /// with a real TOC.
    #[test]
    fn proposes_from_declared_ncx_when_body_is_images() {
        let epub = image_toc_and_ncx_epub();

        // The Contents-page / headings paths find nothing here.
        assert!(
            contents_page_links(
                &EpubPackage::parse(&epub).unwrap(),
                &parse_opf(&decode_text(
                    EpubPackage::parse(&epub).unwrap().opf_bytes().unwrap(),
                    None
                ))
                .unwrap(),
                "OEBPS/",
                ""
            )
            .is_empty(),
            "image 目次 yields no link cluster"
        );

        // …but the proposer recovers the 5 chapters from the NCX.
        let proposed = propose_toc(&epub).expect("propose");
        let labels: Vec<&str> = proposed.iter().map(|e| e.title.as_str()).collect();
        assert!(labels.contains(&"残酷童話　うつくし姫"), "got {labels:?}");
        assert!(labels.contains(&"あとがき"), "got {labels:?}");
        assert!(
            proposed
                .iter()
                .any(|e| e.href == "OEBPS/Text/c1.xhtml#link1"),
            "href resolved to an absolute zip path: {:?}",
            proposed.iter().map(|e| &e.href).collect::<Vec<_>>()
        );

        // Repair writes it back; the book reopens with the chapter list.
        let out = repair_toc(&epub).expect("repair");
        let book = Book::from_bytes(&out, Format::Epub).expect("opens");
        assert!(
            book.toc().iter().any(|e| e.title.contains("うつくし")),
            "repaired TOC lists the chapters"
        );
    }

    #[test]
    fn empty_toc_is_rejected() {
        let epub = no_toc_epub();
        assert!(set_toc(&epub, &[]).is_err());
    }

    #[test]
    fn non_epub_bytes_error() {
        assert!(propose_toc(b"not an epub").is_err());
        assert!(repair_toc(b"not an epub").is_err());
    }

    #[test]
    fn relativize_and_resolve_are_inverse() {
        assert_eq!(relativize("OEBPS/", "OEBPS/c1.xhtml#h1"), "c1.xhtml#h1");
        assert_eq!(relativize("", "OEBPS/c1.xhtml"), "OEBPS/c1.xhtml");
        assert_eq!(relativize("OEBPS/xhtml/", "OEBPS/c1.xhtml"), "../c1.xhtml");
        assert_eq!(resolve_href("OEBPS/", "c1.xhtml#h1"), "OEBPS/c1.xhtml#h1");
        assert_eq!(resolve_href("OEBPS/text/", "../c1.xhtml"), "OEBPS/c1.xhtml");
    }

    #[test]
    fn nav_tag_toc_detection_respects_boundaries() {
        assert!(nav_tag_is_toc(r#"<nav epub:type="toc""#));
        assert!(nav_tag_is_toc(r#"<nav type="toc""#));
        assert!(nav_tag_is_toc(r#"<nav epub:type="landmarks toc""#));
        assert!(!nav_tag_is_toc(r#"<nav epub:type="landmarks""#));
        assert!(!nav_tag_is_toc(r#"<nav epub:type="page-list""#));
    }

    #[test]
    fn splice_replaces_only_the_toc_nav() {
        let doc = "<body>\n<nav epub:type=\"toc\"><ol><li>old</li></ol></nav>\n<nav epub:type=\"landmarks\"><ol><li>keep</li></ol></nav>\n</body>";
        let out = splice_toc_nav(doc, "<nav epub:type=\"toc\">NEW</nav>");
        assert!(out.contains("<nav epub:type=\"toc\">NEW</nav>"));
        assert!(out.contains("landmarks"), "landmarks nav preserved");
        assert!(!out.contains("old"), "old toc nav gone");
    }
}
