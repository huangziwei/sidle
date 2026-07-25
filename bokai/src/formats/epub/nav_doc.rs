//! The two navigation documents an EPUB ships, written from a chapter list.
//!
//! An EPUB 3 nav doc and an EPUB 2 NCX say the same thing in two vocabularies,
//! and a book carries both so that old and new readers agree. Everything that
//! writes a chapter list — repairing one in place, carving one for a volume of a
//! collection — writes both, so both live here and neither route grows its own
//! copy of the `playOrder` rule.
//!
//! Entry hrefs arrive as absolute zip paths and are rebased to the directory of
//! the document being written.

use std::collections::HashMap;

use crate::formats::epub::edit::{escape_attr, escape_text};
use crate::formats::epub::structure::relativize;
use crate::model::TocEntry;
use crate::util::trim_markup_space;

/// Render `<nav epub:type="toc">…</nav>`, hrefs rebased relative to `base_dir`.
pub(crate) fn render_toc_nav(entries: &[TocEntry], base_dir: &str) -> String {
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
            escape_text(trim_markup_space(&e.title))
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
pub(crate) fn render_nav_doc(toc_nav: &str, lang: &str, title: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<!DOCTYPE html>\n\
<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" lang=\"{lang}\" xml:lang=\"{lang}\">\n\
<head>\n<meta charset=\"utf-8\"/>\n<title>{}</title>\n</head>\n\
<body>\n{toc_nav}\n</body>\n</html>\n",
        escape_text(title)
    )
}

/// Render `<navMap>…</navMap>` in reading order, hrefs rebased to `base_dir`.
pub(crate) fn render_navmap(entries: &[TocEntry], base_dir: &str) -> String {
    let mut s = String::from("<navMap>\n");
    let mut order = NavOrder::default();
    render_navpoints(entries, base_dir, &mut order, &mut s);
    s.push_str("</navMap>");
    s
}

/// Numbering state for the NCX: element ids are unique per navPoint, while
/// `playOrder` is assigned per unique content target, so two navPoints on the
/// same target share one playOrder — the NCX 2005-1 rule epubcheck enforces as
/// RSC-005 ("different playOrder values … that refer to the same target"). A
/// book that lists one piece twice (a repeated 目次 row, or a parent whose
/// first child starts at the same document) is ordinary, so this is not an edge
/// case. The EPUB exporter's own NCX emitter carries the same rule.
#[derive(Default)]
struct NavOrder {
    next_id: usize,
    next_play_order: usize,
    play_order_by_target: HashMap<String, usize>,
}

impl NavOrder {
    /// `(element id, playOrder)` for the next navPoint on `src`.
    fn next(&mut self, src: &str) -> (usize, usize) {
        self.next_id += 1;
        let play_order = match self.play_order_by_target.get(src) {
            Some(&existing) => existing,
            None => {
                self.next_play_order += 1;
                self.play_order_by_target
                    .insert(src.to_string(), self.next_play_order);
                self.next_play_order
            }
        };
        (self.next_id, play_order)
    }
}

fn render_navpoints(entries: &[TocEntry], base_dir: &str, order: &mut NavOrder, out: &mut String) {
    for e in entries {
        let src = relativize(base_dir, &e.href);
        let (id, play_order) = order.next(&src);
        out.push_str(&format!(
            "<navPoint id=\"navPoint-{id}\" playOrder=\"{play_order}\">\n\
<navLabel><text>{}</text></navLabel>\n\
<content src=\"{}\"/>\n",
            escape_text(trim_markup_space(&e.title)),
            escape_attr(&src)
        ));
        if !e.children.is_empty() {
            render_navpoints(&e.children, base_dir, order, out);
        }
        out.push_str("</navPoint>\n");
    }
}

/// A minimal NCX wrapping `navmap`.
pub(crate) fn render_ncx(navmap: &str, uid: &str, title: &str, depth: usize) -> String {
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

/// Max nesting depth of a chapter list (a flat list is depth 1) — the NCX's
/// `dtb:depth`.
pub(crate) fn depth(entries: &[TocEntry]) -> usize {
    entries
        .iter()
        .map(|e| 1 + depth(&e.children))
        .max()
        .unwrap_or(0)
}
