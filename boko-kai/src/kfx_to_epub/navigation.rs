//! Phase 1 step 2: book_navigation → NCX.
//!
//! Mechanical port of `yj_to_epub_navigation.py` (the parts needed to
//! emit NCX). Walks the `book_navigation` ($389) fragment to extract TOC
//! entries (nav_type=`toc`) and writes the NCX `<navMap>` for the OPF.
//!
//! Calibre's NCX entries reference per-element anchor ids; we currently
//! resolve nav_units only to their target section's chapter file. Phase
//! 1.5 (when content emission emits per-position anchor ids) will wire
//! `#fragment-id` references that drop into the right paragraph.

use std::collections::HashMap;

use crate::kfx::container::get_field;
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;

use super::loader::BookData;

/// One NCX nav point.
#[derive(Debug, Clone)]
pub struct NavPoint {
    pub label: String,
    /// `chapter.xhtml#fragment` or `chapter.xhtml` href, relative to OEBPS/.
    pub href: String,
    pub children: Vec<NavPoint>,
}

/// Walk the `book_navigation` fragment and return the TOC tree.
///
/// `element_id_to_filename` is the chapter resolution map built by
/// `content.rs::process_section` — each entry maps an element id (the value
/// stored on storyline elements via `$155 id`) to the chapter `.xhtml` file
/// the element ended up in. We use it to point each nav_unit's
/// `target_position.id` at the right chapter file.
pub fn extract_toc(book: &BookData, element_id_to_filename: &HashMap<i64, String>) -> Vec<NavPoint> {
    let Some(nav) = book.by_type.get(&(KfxSymbol::BookNavigation as u64)) else {
        return Vec::new();
    };
    let mut toc: Vec<NavPoint> = Vec::new();

    // book_navigation is typically a list of reading orders, each with
    // nav_containers. The container with nav_type=$214 (= "toc") holds the
    // TOC entries.
    for (_, value) in nav {
        let unwrapped = value.unwrap_annotated();
        let candidates: Vec<IonValue> = match unwrapped {
            IonValue::List(items) => items.clone(),
            IonValue::Struct(_) => vec![unwrapped.clone()],
            _ => Vec::new(),
        };
        for reading_order in candidates {
            let Some(ro_fields) = reading_order.as_struct() else {
                continue;
            };
            let Some(containers) =
                get_field(ro_fields, KfxSymbol::NavContainers as u64).and_then(|v| v.as_list())
            else {
                continue;
            };
            for container in containers {
                let inner = container.unwrap_annotated();
                let Some(cfields) = inner.as_struct() else {
                    continue;
                };
                let nav_type = get_field(cfields, KfxSymbol::NavType as u64)
                    .and_then(|v| book.symbols.text_of(v))
                    .unwrap_or("");
                if nav_type != "toc" {
                    continue;
                }
                let entries: Vec<IonValue> = get_field(cfields, KfxSymbol::Entries as u64)
                    .and_then(|v| v.as_list())
                    .map(|s| s.to_vec())
                    .unwrap_or_default();
                for entry in &entries {
                    if let Some(np) = nav_unit_to_navpoint(entry, book, element_id_to_filename) {
                        toc.push(np);
                    }
                }
            }
        }
    }
    toc
}

fn nav_unit_to_navpoint(
    entry: &IonValue,
    book: &BookData,
    element_id_to_filename: &HashMap<i64, String>,
) -> Option<NavPoint> {
    let inner = entry.unwrap_annotated();
    let fields = inner.as_struct()?;

    // Label: prefer representation.label, then direct label.
    let label = get_field(fields, KfxSymbol::Representation as u64)
        .and_then(|v| v.as_struct())
        .and_then(|s| get_field(s, KfxSymbol::Label as u64))
        .and_then(|v| v.as_string())
        .or_else(|| {
            get_field(fields, KfxSymbol::Label as u64).and_then(|v| v.as_string())
        })
        .unwrap_or("Untitled")
        .to_string();
    if label.is_empty() || label == "heading-nav-unit" {
        // Calibre drops these.
        return None;
    }

    // Target position: gives us (id, offset). Look the id up in the chapter
    // map populated by `content::process_section`. Fragment ids (`#paragraph`)
    // are a separate phase-1.5 follow-up — calibre's `process_position`
    // emits `id="..."` attributes on storyline elements at matching offsets;
    // for now we land on the chapter file only, which is enough for "the TOC
    // takes me to the right place" UX.
    let href = get_field(fields, KfxSymbol::TargetPosition as u64)
        .and_then(|v| v.as_struct())
        .and_then(|pos| get_field(pos, KfxSymbol::Id as u64))
        .and_then(|v| v.as_int())
        .and_then(|id| element_id_to_filename.get(&id).cloned())
        .unwrap_or_default();

    // Children — recursive.
    let mut children = Vec::new();
    if let Some(child_entries) =
        get_field(fields, KfxSymbol::Entries as u64).and_then(|v| v.as_list())
    {
        for child in child_entries {
            if let Some(np) = nav_unit_to_navpoint(child, book, element_id_to_filename) {
                children.push(np);
            }
        }
    }

    Some(NavPoint {
        label,
        href,
        children,
    })
}

/// Render the NCX navMap from a list of nav points.
pub fn render_navmap(points: &[NavPoint]) -> String {
    let mut s = String::new();
    let mut play_order = 1usize;
    write_points(&mut s, points, &mut play_order, 2);
    s
}

fn write_points(s: &mut String, points: &[NavPoint], play_order: &mut usize, indent: usize) {
    let prefix = "  ".repeat(indent);
    for p in points {
        s.push_str(&format!(
            "{}<navPoint id=\"navPoint-{po}\" playOrder=\"{po}\">\n",
            prefix,
            po = *play_order
        ));
        s.push_str(&format!(
            "{}  <navLabel><text>{}</text></navLabel>\n",
            prefix,
            xml_escape(&p.label)
        ));
        s.push_str(&format!(
            "{}  <content src=\"{}\"/>\n",
            prefix,
            xml_escape(&p.href)
        ));
        *play_order += 1;
        if !p.children.is_empty() {
            write_points(s, &p.children, play_order, indent + 1);
        }
        s.push_str(&format!("{}</navPoint>\n", prefix));
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Total nav-point count (including nested children) — useful for logging.
pub fn count_points(points: &[NavPoint]) -> usize {
    let mut n = 0;
    for p in points {
        n += 1 + count_points(&p.children);
    }
    n
}
