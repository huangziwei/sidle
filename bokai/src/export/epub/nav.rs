//! Shared `nav.xhtml` (EPUB 3 navigation document) and `toc.ncx` emitters.
//!
//! Like [`super::opf`], every EPUB writer in the crate — the exporter's raw
//! and normalized paths alike — feeds a [`NavDoc`] / [`NcxDoc`] through
//! [`emit_nav`] / [`emit_ncx`], so the two navigation artifacts have one
//! shape by construction.
//!
//! Escaping is deliberately two-tiered: the TOC/page-list `<ol>` bodies and
//! the NCX navMap leave apostrophes raw (valid XML in text and in
//! double-quoted attributes), while the document shell and the landmarks nav
//! escape the full five. Collapsing the two would churn the bytes of every
//! book already published for zero validity gain.

use std::collections::HashMap;

use super::opf::OpfGuideRef;
use crate::formats::epub::parser::epub_type_to_landmark;
use crate::model::LandmarkType;

/// One navigation entry: a TOC node (nested) or a page-list entry (flat).
/// `href` is relative to `OEBPS/` (`chapter.xhtml`, `chapter.xhtml#frag`),
/// or an empty string when the target never resolved — the TOC keeps such
/// entries (label-only), the page list drops them at build time.
#[derive(Debug, Clone)]
pub struct NavPoint {
    pub label: String,
    pub href: String,
    pub children: Vec<NavPoint>,
}

/// Stable-sort each level of the TOC tree by the reading-order rank of each
/// entry's target file. EPUB 3 requires the `toc` nav to be in reading order
/// (epubcheck warns NAV-011 otherwise); some publisher KFX TOCs list front
/// matter out of reading order (e.g. the 目次 entry before はじめに when はじめに
/// physically reads first — verified against the KFX reading_order). Ties (same
/// file, or a target file not in the spine) keep their original order, so a TOC
/// already in reading order is left byte-identical.
pub fn sort_toc_reading_order(toc: &mut [NavPoint], file_rank: &HashMap<String, usize>) {
    fn rank(np: &NavPoint, fr: &HashMap<String, usize>) -> usize {
        let file = np.href.split('#').next().unwrap_or(&np.href);
        fr.get(file).copied().unwrap_or(usize::MAX)
    }
    toc.sort_by_key(|np| rank(np, file_rank));
    for np in toc.iter_mut() {
        sort_toc_reading_order(&mut np.children, file_rank);
    }
}

/// Everything `nav.xhtml` needs.
pub struct NavDoc<'a> {
    /// Book title (`<title>` and the empty-TOC fallback label). Empty →
    /// `"Untitled"`.
    pub title: &'a str,
    /// `xml:lang` / `lang`. Empty → `"en"`.
    pub language: &'a str,
    /// TOC tree, already sorted to reading order by the caller (see
    /// [`sort_toc_reading_order`]).
    pub toc: &'a [NavPoint],
    /// Target for the single-entry TOC emitted when `toc` is empty: the first
    /// spine document (titlepage when one was synthesized). `None` (no spine
    /// at all) emits an empty `<ol></ol>`.
    pub toc_fallback_href: Option<&'a str>,
    /// Physical page list, flat and in page order; empty omits the nav.
    pub page_list: &'a [NavPoint],
    /// Landmarks, shared with the OPF `<guide>` (EPUB 2 guide-type strings —
    /// mapped to the EPUB 3 vocabulary at emission).
    pub landmarks: &'a [OpfGuideRef],
}

/// Serialize the EPUB 3 navigation document.
///
/// The W3C EPUB 3.3 spec requires every Publication to include exactly one
/// nav doc, and conformant readers (Apple Books) reject EPUB 3 packages
/// without it. NCX no longer satisfies the requirement on its own — it's
/// strictly legacy.
///
/// Body shape (mirrors calibre's EPUB 3 nav output):
/// `<nav epub:type="toc"><ol><li><a href=…>…</a></li></ol></nav>`, an
/// optional hidden `<nav epub:type="page-list">`, and an optional hidden
/// `<nav epub:type="landmarks">` derived from the same guide entries the
/// OPF `<guide>` block uses.
pub fn emit_nav(doc: &NavDoc) -> String {
    let title = if doc.title.is_empty() {
        "Untitled"
    } else {
        doc.title
    };
    let lang = if doc.language.is_empty() {
        "en"
    } else {
        doc.language
    };

    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<!DOCTYPE html>\n");
    s.push_str(&format!(
        "<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" xml:lang=\"{lang}\" lang=\"{lang}\">\n",
        lang = xml_escape(lang),
    ));
    s.push_str("<head>\n");
    s.push_str("  <meta charset=\"utf-8\"/>\n");
    s.push_str(&format!("  <title>{}</title>\n", xml_escape(title)));
    s.push_str("</head>\n<body>\n");

    // TOC nav.
    s.push_str("  <nav epub:type=\"toc\" id=\"toc\">\n");
    s.push_str("    <h1>Table of Contents</h1>\n");
    if !doc.toc.is_empty() {
        write_nav_ol(&mut s, doc.toc, 4);
    } else if let Some(href) = doc.toc_fallback_href {
        // Fallback: single entry pointing at the first spine document
        // (mirrors what `emit_ncx` does when the TOC is empty).
        s.push_str(&format!(
            "    <ol>\n      <li><a href=\"{}\">{}</a></li>\n    </ol>\n",
            xml_escape(href),
            xml_escape(title),
        ));
    } else {
        s.push_str("    <ol></ol>\n");
    }
    s.push_str("  </nav>\n");

    // Page-list nav (`<nav epub:type="page-list">`) — printed page numbers →
    // positions, round-tripped from the source's page-list container.
    // Emitted only when present; `hidden` like the landmarks nav so it
    // drives "go to page N" without cluttering the visible TOC.
    if !doc.page_list.is_empty() {
        s.push_str("  <nav epub:type=\"page-list\" id=\"page-list\" hidden=\"\">\n");
        s.push_str("    <h2>List of Pages</h2>\n");
        write_nav_ol(&mut s, doc.page_list, 4);
        s.push_str("  </nav>\n");
    }

    // Landmarks nav, derived from the same guide entries the EPUB-2
    // `<guide>` block uses. EPUB-3 vocabulary differs from EPUB-2 guide
    // types in a few names (start → bodymatter, acknowledgements vs
    // acknowledgments); map at emit time so the guide struct stays a single
    // source of truth.
    if !doc.landmarks.is_empty() {
        s.push_str("  <nav epub:type=\"landmarks\" id=\"landmarks\" hidden=\"\">\n");
        s.push_str("    <h2>Landmarks</h2>\n");
        s.push_str("    <ol>\n");
        // EPUB 3 forbids two landmarks that share an epub:type AND reference
        // the same resource (epubcheck RSC-005). bokai's own EPUB→KFX emits
        // both an `srl` (start-reading) and a `bodymatter` landmark for the
        // book's opening — which map to the same `bodymatter` + href here —
        // so keep the first of any (type, href) pair and drop later repeats.
        let mut seen_landmarks: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for g in doc.landmarks {
            let epub_type = guide_type_to_epub3(&g.guide_type);
            if !seen_landmarks.insert((epub_type.to_string(), g.href.clone())) {
                continue;
            }
            // EPUB 3 requires every `<nav>` anchor to carry text (RSC-005);
            // KFX landmark containers sometimes yield an empty label (the
            // bodymatter/cover start marker), so fall back to a default.
            let label = if g.title.trim().is_empty() {
                landmark_default_label(epub_type)
            } else {
                g.title.as_str()
            };
            s.push_str(&format!(
                "      <li><a epub:type=\"{}\" href=\"{}\">{}</a></li>\n",
                epub_type,
                xml_escape(&g.href),
                xml_escape(label),
            ));
        }
        s.push_str("    </ol>\n");
        s.push_str("  </nav>\n");
    }

    s.push_str("</body>\n</html>\n");
    s
}

/// Everything `toc.ncx` needs.
pub struct NcxDoc<'a> {
    /// `<docTitle>`. Empty → `"Untitled"`.
    pub title: &'a str,
    /// `dtb:uid` — must match the OPF unique identifier. Empty → the same
    /// nil-UUID URN [`super::opf::emit_opf`] falls back to.
    pub identifier: &'a str,
    /// TOC tree (same one `nav.xhtml` renders).
    pub toc: &'a [NavPoint],
    /// Target for the single-navPoint fallback when `toc` is empty; `None`
    /// leaves the navMap empty.
    pub toc_fallback_href: Option<&'a str>,
}

/// Serialize the legacy NCX (kept alongside the nav doc for EPUB 2 readers).
pub fn emit_ncx(doc: &NcxDoc) -> String {
    let title = if doc.title.is_empty() {
        "Untitled"
    } else {
        doc.title
    };
    let id = if doc.identifier.is_empty() {
        "urn:uuid:00000000-0000-0000-0000-000000000000"
    } else {
        doc.identifier
    };
    let mut s = String::new();
    s.push_str(&format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE ncx PUBLIC "-//NISO//DTD ncx 2005-1//EN" "http://www.daisy.org/z3986/2005/ncx-2005-1.dtd">
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head>
    <meta name="dtb:uid" content="{id}"/>
    <meta name="dtb:depth" content="1"/>
    <meta name="dtb:totalPageCount" content="0"/>
    <meta name="dtb:maxPageNumber" content="0"/>
  </head>
  <docTitle><text>{title}</text></docTitle>
  <navMap>
"#,
        id = xml_escape(id),
        title = xml_escape(title)
    ));

    if !doc.toc.is_empty() {
        let mut ctx = NavmapCtx {
            next_id: 1,
            next_play_order: 1,
            play_order_by_target: HashMap::new(),
        };
        write_nav_points(&mut s, doc.toc, &mut ctx, 2);
    } else if let Some(href) = doc.toc_fallback_href {
        s.push_str(&format!(
            "    <navPoint id=\"navPoint-1\" playOrder=\"1\">\n      <navLabel><text>{}</text></navLabel>\n      <content src=\"{}\"/>\n    </navPoint>\n",
            xml_escape(title),
            xml_escape(href)
        ));
    }

    s.push_str("  </navMap>\n</ncx>\n");
    s
}

/// Recursively write the `<ol><li><a href="…">…</a></li></ol>` body used by
/// both the TOC nav and the page-list nav.
fn write_nav_ol(s: &mut String, points: &[NavPoint], indent: usize) {
    let pad = "  ".repeat(indent);
    s.push_str(&pad);
    s.push_str("<ol>\n");
    for p in points {
        s.push_str(&pad);
        s.push_str(&format!(
            "  <li><a href=\"{}\">{}</a>",
            xml_escape_noapos(&p.href),
            xml_escape_noapos(&p.label)
        ));
        if !p.children.is_empty() {
            s.push('\n');
            write_nav_ol(s, &p.children, indent + 2);
            s.push_str(&pad);
            s.push_str("  </li>\n");
        } else {
            s.push_str("</li>\n");
        }
    }
    s.push_str(&pad);
    s.push_str("</ol>\n");
}

/// Numbering state for the NCX navMap. `id` is always unique (one per
/// navPoint); `playOrder` is assigned per unique content target so that two
/// navPoints referencing the same target share a playOrder — the NCX rule
/// epubcheck enforces (RSC-005 "different playOrder values … that refer to the
/// same target"). First-occurrence order gives reading-order playOrder.
struct NavmapCtx {
    next_id: usize,
    next_play_order: usize,
    play_order_by_target: HashMap<String, usize>,
}

fn write_nav_points(s: &mut String, points: &[NavPoint], ctx: &mut NavmapCtx, indent: usize) {
    let prefix = "  ".repeat(indent);
    for p in points {
        let id = ctx.next_id;
        ctx.next_id += 1;
        // Same content target ⇒ same playOrder (assigned in first-occurrence
        // order); the `id` stays unique per navPoint.
        let po = if let Some(&po) = ctx.play_order_by_target.get(&p.href) {
            po
        } else {
            let v = ctx.next_play_order;
            ctx.next_play_order += 1;
            ctx.play_order_by_target.insert(p.href.clone(), v);
            v
        };
        s.push_str(&format!(
            "{prefix}<navPoint id=\"navPoint-{id}\" playOrder=\"{po}\">\n"
        ));
        s.push_str(&format!(
            "{}  <navLabel><text>{}</text></navLabel>\n",
            prefix,
            xml_escape_noapos(&p.label)
        ));
        s.push_str(&format!(
            "{}  <content src=\"{}\"/>\n",
            prefix,
            xml_escape_noapos(&p.href)
        ));
        if !p.children.is_empty() {
            write_nav_points(s, &p.children, ctx, indent + 1);
        }
        s.push_str(&format!("{}</navPoint>\n", prefix));
    }
}

/// Map an EPUB 2.0 `<guide>` reference type to the EPUB 3 nav-doc
/// `epub:type` vocabulary. Most types are identical; a few names differ
/// (`start` → `bodymatter`, `acknowledgements` → `acknowledgments`).
/// Unknown types pass through verbatim — readers ignore unknown values.
fn guide_type_to_epub3(guide_type: &str) -> &str {
    match guide_type {
        "start" | "text" => "bodymatter",
        "acknowledgements" => "acknowledgments",
        other => other,
    }
}

/// Human-readable fallback label for a landmark whose source carried no
/// text. EPUB 3 rejects an empty `<nav>` anchor (RSC-005 "Anchors within nav
/// elements must contain text"), so every landmark link needs a label.
///
/// The names come from [`LandmarkType::default_label`], so this nav doc, the
/// guide, and a TOC completed from a book's landmarks all call the same place by
/// the same name. An `epub:type` outside the landmark vocabulary falls back to
/// the reading-start marker's name.
fn landmark_default_label(epub_type: &str) -> &'static str {
    epub_type_to_landmark(epub_type)
        .unwrap_or(LandmarkType::BodyMatter)
        .default_label()
}

/// Full five-entity escape, for the document shell and landmark entries.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Four-entity escape (no `&apos;`), for the TOC/page-list bodies and the
/// NCX navMap — see the module docs for why the two tiers stay distinct.
fn xml_escape_noapos(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(label: &str, href: &str, children: Vec<NavPoint>) -> NavPoint {
        NavPoint {
            label: label.to_string(),
            href: href.to_string(),
            children,
        }
    }

    #[test]
    fn nav_toc_page_list_and_landmarks() {
        let toc = vec![np(
            "Chapter 1",
            "c0.xhtml",
            vec![np("Scene", "c0.xhtml#s1", Vec::new())],
        )];
        let pages = vec![np("1", "c0.xhtml#page-1-0", Vec::new())];
        let landmarks = vec![
            OpfGuideRef {
                guide_type: "cover".to_string(),
                title: String::new(),
                href: "cover.xhtml".to_string(),
            },
            OpfGuideRef {
                guide_type: "text".to_string(),
                title: String::new(),
                href: "c0.xhtml".to_string(),
            },
            // Duplicate (type, href) after EPUB-3 mapping — must be dropped.
            OpfGuideRef {
                guide_type: "start".to_string(),
                title: "Start".to_string(),
                href: "c0.xhtml".to_string(),
            },
        ];
        let s = emit_nav(&NavDoc {
            title: "Book",
            language: "ja",
            toc: &toc,
            toc_fallback_href: Some("c0.xhtml"),
            page_list: &pages,
            landmarks: &landmarks,
        });
        assert!(s.contains("<title>Book</title>"));
        assert!(s.contains("xml:lang=\"ja\" lang=\"ja\""));
        assert!(s.contains("<li><a href=\"c0.xhtml#s1\">Scene</a></li>"));
        assert!(s.contains("<nav epub:type=\"page-list\" id=\"page-list\" hidden=\"\">"));
        assert!(s.contains("<li><a href=\"c0.xhtml#page-1-0\">1</a></li>"));
        // Empty labels fall back; the duplicated bodymatter target is deduped.
        assert!(s.contains("<a epub:type=\"cover\" href=\"cover.xhtml\">Cover</a>"));
        assert!(s.contains("<a epub:type=\"bodymatter\" href=\"c0.xhtml\">Start of Content</a>"));
        assert!(!s.contains(">Start</a>"));
    }

    #[test]
    fn nav_empty_toc_falls_back_to_first_spine_doc() {
        let s = emit_nav(&NavDoc {
            title: "",
            language: "",
            toc: &[],
            toc_fallback_href: Some("c0.xhtml"),
            page_list: &[],
            landmarks: &[],
        });
        assert!(s.contains("<title>Untitled</title>"));
        assert!(s.contains("xml:lang=\"en\" lang=\"en\""));
        assert!(s.contains("<li><a href=\"c0.xhtml\">Untitled</a></li>"));
        assert!(!s.contains("page-list"));
        assert!(!s.contains("landmarks"));

        let empty = emit_nav(&NavDoc {
            title: "",
            language: "",
            toc: &[],
            toc_fallback_href: None,
            page_list: &[],
            landmarks: &[],
        });
        assert!(empty.contains("<ol></ol>"));
    }

    #[test]
    fn ncx_playorder_shared_per_target_ids_unique() {
        // Two entries at the same target: distinct navPoint ids, one playOrder.
        let toc = vec![
            np("A", "c0.xhtml", Vec::new()),
            np("A again", "c0.xhtml", Vec::new()),
            np("B", "c1.xhtml", Vec::new()),
        ];
        let s = emit_ncx(&NcxDoc {
            title: "Book",
            identifier: "urn:x:1",
            toc: &toc,
            toc_fallback_href: None,
        });
        assert!(s.contains("<navPoint id=\"navPoint-1\" playOrder=\"1\">"));
        assert!(s.contains("<navPoint id=\"navPoint-2\" playOrder=\"1\">"));
        assert!(s.contains("<navPoint id=\"navPoint-3\" playOrder=\"2\">"));
        assert!(s.contains("<meta name=\"dtb:uid\" content=\"urn:x:1\"/>"));
        assert!(s.contains("<docTitle><text>Book</text></docTitle>"));
    }

    #[test]
    fn ncx_fallbacks() {
        let s = emit_ncx(&NcxDoc {
            title: "",
            identifier: "",
            toc: &[],
            toc_fallback_href: Some("c0.xhtml"),
        });
        assert!(s.contains("urn:uuid:00000000-0000-0000-0000-000000000000"));
        assert!(s.contains("<navLabel><text>Untitled</text></navLabel>"));
        assert!(s.contains("<content src=\"c0.xhtml\"/>"));

        let empty = emit_ncx(&NcxDoc {
            title: "T",
            identifier: "i",
            toc: &[],
            toc_fallback_href: None,
        });
        assert!(empty.contains("<navMap>\n  </navMap>"));
    }

    #[test]
    fn escape_tiers_match_the_extracted_emitters() {
        // Apostrophes stay raw in ol/navMap bodies, escape in the shell.
        let toc = vec![np("It's", "c0.xhtml", Vec::new())];
        let nav = emit_nav(&NavDoc {
            title: "It's",
            language: "en",
            toc: &toc,
            toc_fallback_href: None,
            page_list: &[],
            landmarks: &[],
        });
        assert!(nav.contains("<title>It&apos;s</title>"));
        assert!(nav.contains("<a href=\"c0.xhtml\">It's</a>"));
        let ncx = emit_ncx(&NcxDoc {
            title: "It's",
            identifier: "i",
            toc: &toc,
            toc_fallback_href: None,
        });
        assert!(ncx.contains("<docTitle><text>It&apos;s</text></docTitle>"));
        assert!(ncx.contains("<navLabel><text>It's</text></navLabel>"));
    }

    #[test]
    fn sort_toc_reading_order_stable_on_ties() {
        let mut fr = HashMap::new();
        fr.insert("a.xhtml".to_string(), 0);
        fr.insert("b.xhtml".to_string(), 1);
        let mut toc = vec![
            np("B", "b.xhtml", Vec::new()),
            np("A1", "a.xhtml#x", Vec::new()),
            np("A2", "a.xhtml#y", Vec::new()),
            np("?", "", Vec::new()),
        ];
        sort_toc_reading_order(&mut toc, &fr);
        let labels: Vec<&str> = toc.iter().map(|p| p.label.as_str()).collect();
        // Unranked ("" → not in spine) sorts last; a.xhtml ties keep order.
        assert_eq!(labels, ["A1", "A2", "B", "?"]);
    }
}
