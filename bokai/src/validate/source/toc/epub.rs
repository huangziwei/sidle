//! EPUB-native TOC evidence for [`super::validate`].
//!
//! Builds the same format-neutral [`TocEvidence`](super::TocEvidence) the KFX
//! extractor produces, but from an EPUB's own structures: the declared TOC (EPUB
//! 3 nav doc or EPUB 2 NCX, via `crate::epub`'s parsers), the spine XHTML (`<hN>`
//! headings, chapter-marker section starts), and an in-book Contents page (a
//! spine doc that links to many other spine docs). Reads only the EPUB — never a
//! converted KFX. Structural EPUB conformance is a separate concern owned by
//! [`crate::validate::source::epub`]; this is a TOC-*completeness* check.

use std::collections::HashSet;
use std::io::{Cursor, Read, Seek};

use zip::ZipArchive;

use super::TocEvidence;
use crate::formats::epub::{
    parse_container_xml, parse_nav_landmarks, parse_nav_toc, parse_ncx, parse_opf, parse_opf_guide,
};
use crate::model::{Landmark, LandmarkType, TocEntry};
use crate::util::{decode_text, extract_xml_encoding};

pub(super) fn evidence(epub_bytes: &[u8]) -> Result<TocEvidence, String> {
    let mut archive =
        ZipArchive::new(Cursor::new(epub_bytes)).map_err(|e| format!("not a zip: {e}"))?;

    let container =
        read_entry(&mut archive, "META-INF/container.xml").ok_or("missing container.xml")?;
    let opf_path = parse_container_xml(&container).map_err(|e| format!("container: {e:?}"))?;
    let opf_base = opf_path
        .rfind('/')
        .map(|i| opf_path[..=i].to_string())
        .unwrap_or_default();

    let opf_bytes = read_entry(&mut archive, &opf_path).ok_or("missing opf")?;
    let opf_str = decode_text(&opf_bytes, extract_xml_encoding(&opf_bytes));
    let opf = parse_opf(&opf_str).map_err(|e| format!("opf: {e:?}"))?;

    // Declared TOC labels — the richer of NCX vs EPUB-3 nav (a retail EPUB often
    // pairs a full nav with a stub NCX, or vice versa).
    let ncx = opf
        .ncx_href
        .as_ref()
        .and_then(|h| load_toc(&mut archive, &opf_base, h, parse_ncx));
    let nav = opf
        .nav_href
        .as_ref()
        .and_then(|h| load_toc(&mut archive, &opf_base, h, parse_nav_toc));
    let toc = match (ncx, nav) {
        (Some(a), Some(b)) => {
            if flat_len(&a) >= flat_len(&b) {
                a
            } else {
                b
            }
        }
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => Vec::new(),
    };
    // Spine files (basename → present), and the toc-landmark target if any.
    let spine_files: HashSet<String> = opf
        .spine_ids
        .iter()
        .filter_map(|id| opf.manifest.get(id).map(|(h, _)| basename(h)))
        .collect();
    let toc_landmark = toc_landmark_href(&mut archive, &opf, &opf_base, &opf_str);
    let has_toc_landmark = toc_landmark.is_some();

    // Walk the spine: headings, chapter-marker section starts, and the densest
    // in-book Contents-page link cluster.
    let mut headings = 0usize;
    let mut section_heads = 0usize;
    let mut best_cluster = 0usize;
    let mut best_sample: Vec<String> = Vec::new();
    let mut landmark_cluster: Option<(usize, Vec<String>)> = None;

    for id in &opf.spine_ids {
        let Some((href, _)) = opf.manifest.get(id) else {
            continue;
        };
        let Some(bytes) = read_entry(&mut archive, &format!("{opf_base}{href}")) else {
            continue;
        };
        let xhtml = decode_text(&bytes, extract_xml_encoding(&bytes));

        headings += count_hn(&xhtml);
        if first_text(&xhtml).is_some_and(|t| super::is_chapter_marker(&t)) {
            section_heads += 1;
        }

        // Links to OTHER spine docs = a Contents page's chapter links.
        let this = basename(href);
        let (targets, labels) = internal_link_targets(&xhtml, &spine_files, &this);
        let n = targets.len();
        if n > best_cluster {
            best_cluster = n;
            best_sample = labels.clone();
        }
        if toc_landmark.as_deref() == Some(this.as_str()) && n > 0 {
            landmark_cluster = Some((n, labels));
        }
    }
    let (contents_links, contents_sample) = landmark_cluster.unwrap_or((best_cluster, best_sample));

    Ok(TocEvidence {
        nav_tree: toc,
        contents_links,
        contents_sample,
        headings,
        section_heads,
        has_toc_landmark,
        // Whether the declared TOC flattened a multi-work book's levels. The
        // repair module owns the rule (and answers zero straight away for a
        // book that declares its structure), so a diagnosis here and the fix
        // there can never disagree.
        flattened: crate::formats::epub::toc_repair::declared_toc_flattening(epub_bytes)
            .unwrap_or_default(),
    })
}

fn read_entry<R: Read + Seek>(a: &mut ZipArchive<R>, name: &str) -> Option<Vec<u8>> {
    let mut f = a.by_name(name).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn load_toc<R: Read + Seek>(
    a: &mut ZipArchive<R>,
    base: &str,
    href: &str,
    f: fn(&str) -> std::io::Result<Vec<TocEntry>>,
) -> Option<Vec<TocEntry>> {
    let bytes = read_entry(a, &format!("{base}{href}"))?;
    let entries = f(&decode_text(&bytes, extract_xml_encoding(&bytes))).ok()?;
    (!entries.is_empty()).then_some(entries)
}

fn flat_len(entries: &[TocEntry]) -> usize {
    entries.iter().map(|e| 1 + flat_len(&e.children)).sum()
}

/// The spine-doc basename a `toc`-type landmark points at (EPUB 3 nav landmarks
/// preferred, EPUB 2 OPF guide from the already-parsed OPF as fallback).
fn toc_landmark_href<R: Read + Seek>(
    a: &mut ZipArchive<R>,
    opf: &crate::formats::epub::OpfData,
    opf_base: &str,
    opf_str: &str,
) -> Option<String> {
    let find = |lms: &[Landmark]| {
        lms.iter()
            .find(|l| l.landmark_type == LandmarkType::Toc)
            .map(|l| basename(&l.href))
    };
    if let Some(nav_href) = &opf.nav_href
        && let Some(bytes) = read_entry(a, &format!("{opf_base}{nav_href}"))
        && let Ok(lms) = parse_nav_landmarks(&decode_text(&bytes, extract_xml_encoding(&bytes)))
        && let Some(h) = find(&lms)
    {
        return Some(h);
    }
    parse_opf_guide(opf_str).ok().as_deref().and_then(find)
}

/// `<h1>`..`<h6>` open-tag count.
fn count_hn(xhtml: &str) -> usize {
    let b = xhtml.as_bytes();
    let mut n = 0;
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == b'<' && (b[i + 1] == b'h' || b[i + 1] == b'H') {
            let d = b[i + 2];
            if (b'1'..=b'6').contains(&d) {
                let after = b.get(i + 3).copied().unwrap_or(b' ');
                if after == b'>' || after == b' ' || after == b'\t' || after == b'\n' {
                    n += 1;
                }
            }
        }
        i += 1;
    }
    n
}

/// First visible text after `<body>`, tags stripped.
fn first_text(xhtml: &str) -> Option<String> {
    let body = xhtml.split_once("<body").map(|(_, b)| b).unwrap_or(xhtml);
    let mut out = String::new();
    let mut in_tag = false;
    for c in body.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => {
                if c.is_whitespace() {
                    if !out.is_empty() {
                        break;
                    }
                } else {
                    out.push(c);
                }
            }
            _ => {}
        }
        if out.chars().count() > 24 {
            break;
        }
    }
    let t = out.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Distinct spine-doc basenames this doc links to (excluding itself), plus a few
/// link labels — a Contents page links to many chapters.
fn internal_link_targets(
    xhtml: &str,
    spine_files: &HashSet<String>,
    self_file: &str,
) -> (HashSet<String>, Vec<String>) {
    let mut targets = HashSet::new();
    let mut labels = Vec::new();
    let mut rest = xhtml;
    while let Some(p) = rest.find("<a ") {
        rest = &rest[p + 3..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..end];
        if let Some(href) = attr(tag, "href") {
            let file = basename(&href);
            if file != self_file && spine_files.contains(&file) {
                let is_new = targets.insert(file);
                if is_new && labels.len() < 6 {
                    // link text = up to the closing </a>
                    if let Some(close) = rest[end..].find("</a>") {
                        let text = crate::util::strip_tags(&rest[end + 1..end + close])
                            .trim()
                            .to_string();
                        if !text.is_empty() {
                            labels.push(text);
                        }
                    }
                }
            }
        }
        rest = &rest[end..];
    }
    (targets, labels)
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=");
    let pos = tag.find(&key)?;
    let after = &tag[pos + key.len()..];
    let quote = after.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = after[1..].find(quote)?;
        Some(after[1..1 + end].to_string())
    } else {
        Some(after.split_whitespace().next()?.to_string())
    }
}

/// File component of an href: drop `#fragment` / `?query`, take the last path
/// segment, and undo the one percent-encoding (`%20`) common in chapter hrefs.
fn basename(href: &str) -> String {
    let no_frag = href.split(['#', '?']).next().unwrap_or(href);
    let file = no_frag.rsplit('/').next().unwrap_or(no_frag);
    file.replace("%20", " ").to_ascii_lowercase()
}
