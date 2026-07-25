//! Where an EPUB's parts sit, as absolute zip paths.
//!
//! An EPUB writes every reference relative to whichever document makes it — the
//! OPF's manifest, a nav doc's `<a href>`, a chapter's cross-link — while
//! anything reasoning about the book as a whole needs one vocabulary in which
//! two references to the same file compare equal. That vocabulary is the zip
//! entry name, and this module is the translation into and out of it, plus the
//! two reads that need it: the spine in reading order, and the links a document
//! makes into it.

use std::collections::HashSet;

use crate::formats::epub::OpfData;
use crate::formats::epub::edit::attr_value;
use crate::util::{percent_decode, strip_tags};

/// The spine in reading order, as `(absolute zip path, lowercase filename)`.
///
/// The filename is what a link is matched against — an href written from
/// another directory reaches the same document by a different relative path,
/// but never by a different name.
pub(crate) fn spine_documents(opf: &OpfData, opf_base: &str) -> Vec<(String, String)> {
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

/// `(label, absolute href)` for every link this document makes to *another*
/// spine document — what a Contents page's chapter links look like. Hrefs are
/// resolved against `doc_dir`, fragment preserved.
pub(crate) fn internal_links(
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

/// The directory portion of a zip path, with a trailing `/` (empty at root).
pub(crate) fn dir_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/"),
        None => String::new(),
    }
}

/// Lowercased filename of an href: `#fragment`/`?query` dropped, last path
/// segment, `%20`→space. The comparable form for spine-membership tests.
pub(crate) fn basename(href: &str) -> String {
    let no_frag = href.split(['#', '?']).next().unwrap_or(href);
    let file = no_frag.rsplit('/').next().unwrap_or(no_frag);
    file.replace("%20", " ").to_ascii_lowercase()
}

/// Split an href into `(path, fragment)` where the fragment keeps its leading
/// `#` (empty when there is none).
pub(crate) fn split_fragment(href: &str) -> (&str, &str) {
    match href.find('#') {
        Some(i) => (&href[..i], &href[i..]),
        None => (href, ""),
    }
}

/// The href without its `#fragment` — the document it opens.
pub(crate) fn strip_fragment(href: &str) -> &str {
    href.split_once('#').map(|(p, _)| p).unwrap_or(href)
}

/// Resolve `href` (relative to `base_dir`) to an absolute zip path, collapsing
/// `.`/`..` and percent-decoding. A pure-fragment href resolves to itself.
pub(crate) fn resolve_href(base_dir: &str, href: &str) -> String {
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
pub(crate) fn relativize(from_dir: &str, abs_target: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relativize_and_resolve_are_inverse() {
        assert_eq!(relativize("OEBPS/", "OEBPS/c1.xhtml#h1"), "c1.xhtml#h1");
        assert_eq!(relativize("", "OEBPS/c1.xhtml"), "OEBPS/c1.xhtml");
        assert_eq!(relativize("OEBPS/xhtml/", "OEBPS/c1.xhtml"), "../c1.xhtml");
        assert_eq!(resolve_href("OEBPS/", "c1.xhtml#h1"), "OEBPS/c1.xhtml#h1");
        assert_eq!(resolve_href("OEBPS/text/", "../c1.xhtml"), "OEBPS/c1.xhtml");
    }

    #[test]
    fn a_link_is_matched_to_the_spine_by_filename_however_it_was_written() {
        // Three ways of writing the same target from three directories, plus one
        // link out of the spine and one back to the linking document itself.
        let xhtml = r#"<a href="../Text/c1.xhtml#h1">One</a>
                       <a href="c1.xhtml">Again</a>
                       <a href="notes.xhtml">Not in the spine</a>
                       <a href="toc.xhtml">Itself</a>"#;
        let spine: HashSet<&str> = ["c1.xhtml", "toc.xhtml"].into_iter().collect();
        let links = internal_links(xhtml, "OEBPS/Text/", &spine, "toc.xhtml");
        assert_eq!(
            links,
            [
                ("One".to_string(), "OEBPS/Text/c1.xhtml#h1".to_string()),
                ("Again".to_string(), "OEBPS/Text/c1.xhtml".to_string()),
            ]
        );
    }
}
