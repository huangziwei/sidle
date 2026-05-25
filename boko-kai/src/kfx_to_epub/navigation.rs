//! book_navigation → NCX.
//!
//! Mechanical port of `yj_to_epub_navigation.py`. Walks the
//! `book_navigation` ($389) fragment to extract TOC entries
//! (nav_type=`toc`) and writes the NCX `<navMap>` for the OPF.
//! Also extracts the `$266 anchors` table for use by
//! `content::process_position` to set `id="..."` on storyline
//! elements whose `(location_id, offset)` matches a registered anchor.

use std::collections::HashMap;

use crate::kfx::container::get_field;
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;

use super::loader::BookData;

/// Per-anchor data extracted from `$266 anchor` entities. Mirrors
/// calibre's `process_anchors` (yj_to_epub_navigation.py:40).
#[derive(Debug, Default, Clone)]
pub struct AnchorTable {
    /// External-URI anchors: anchor_name → uri. Set when `$186 uri` is
    /// present. These are dereferenced by `<a href>` link_to → uri
    /// resolution.
    pub anchor_uri: HashMap<String, String>,

    /// Internal-position anchors, keyed by `(location_id, offset)`. Each
    /// position can have multiple anchor names (calibre stores a list).
    /// `content::process_position` looks up `(eid, offset)` here and, if
    /// found, sets the HTML element's `id="..."` attribute to a unique
    /// id derived from the first anchor name.
    pub position_anchors: HashMap<i64, HashMap<i64, Vec<String>>>,
}

impl AnchorTable {
    /// Resolve an html id for the given anchor name. The id is unique
    /// across the table; calibre's `make_unique_name` over a set —
    /// here we just sanitize the anchor name with a `anchor-` prefix
    /// fallback for purely numeric names (HTML ids can't start with a
    /// digit in some validators).
    pub fn anchor_id(&self, anchor_name: &str) -> String {
        fix_html_id(anchor_name)
    }

    /// Look up the html id for `(eid, offset)`. Returns the id derived
    /// from the FIRST anchor at that position (matches calibre's
    /// `process_position` behavior).
    pub fn id_at(&self, eid: i64, offset: i64) -> Option<String> {
        let names = self.position_anchors.get(&eid)?.get(&offset)?;
        let name = names.first()?;
        Some(self.anchor_id(name))
    }

    /// Resolve an `anchor_name` to its final EPUB URI for `<a href>`
    /// emission. Calibre's `get_anchor_uri` — looks at three sources
    /// in order: external `anchor_uri` (a real http URI), then the
    /// `(eid, offset)` mapping plus the caller-supplied
    /// `element_id_to_filename` to produce `{chapter}#{html_id}`.
    /// Returns `None` if the anchor is not registered (caller should
    /// treat as a dangling link and either drop the href or log).
    pub fn resolve_uri(
        &self,
        anchor_name: &str,
        element_id_to_filename: &HashMap<i64, String>,
    ) -> Option<String> {
        // Already-resolved external URI?
        if let Some(uri) = self.anchor_uri.get(anchor_name) {
            return Some(uri.clone());
        }
        // Internal position anchor.
        for (eid, offsets) in &self.position_anchors {
            for (offset, names) in offsets {
                if names.iter().any(|n| n == anchor_name) {
                    let file = element_id_to_filename.get(eid)?;
                    let frag = self.anchor_id(anchor_name);
                    // `(eid, 0)` and no visible content before the
                    // element — calibre drops the fragment in that
                    // case; we keep it for simplicity (validator
                    // doesn't penalize the extra `#id`).
                    let _ = offset;
                    return Some(format!("{}#{}", file, frag));
                }
            }
        }
        None
    }
}

/// Build the anchor table by iterating every `$266 anchor` entity.
///
/// Each anchor has either:
///   - `$186 uri` — external link target (kept as-is)
///   - `$183 position` struct with `$155 id` (location_id) and
///     `$143 offset` — registers the anchor against `(id, offset)`.
pub fn extract_anchors(book: &BookData) -> AnchorTable {
    let mut table = AnchorTable::default();
    let Some(anchors) = book.by_type.get(&(KfxSymbol::Anchor as u64)) else {
        return table;
    };
    for (name, value) in anchors {
        let inner = value.unwrap_annotated();
        let Some(fields) = inner.as_struct() else {
            continue;
        };
        // External URI anchor.
        if let Some(uri_val) = get_field(fields, KfxSymbol::Uri as u64)
            && let IonValue::String(uri) = uri_val.unwrap_annotated()
        {
            // Calibre normalises bare scheme placeholders to empty string.
            let clean = if uri == "http://" || uri == "https://" {
                String::new()
            } else {
                uri.clone()
            };
            table.anchor_uri.insert(name.clone(), clean);
            continue;
        }
        // Internal position anchor.
        if let Some(pos_val) = get_field(fields, KfxSymbol::Position as u64) {
            let pos = pos_val.unwrap_annotated();
            let Some(pos_fields) = pos.as_struct() else {
                continue;
            };
            let Some(eid) = get_field(pos_fields, KfxSymbol::Id as u64)
                .and_then(|v| v.as_int())
            else {
                continue;
            };
            // `$143 offset` defaults to 0 when missing.
            let offset = get_field(pos_fields, KfxSymbol::Offset as u64)
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            table
                .position_anchors
                .entry(eid)
                .or_default()
                .entry(offset)
                .or_default()
                .push(name.clone());
        }
    }
    table
}

/// Sanitise an anchor name into a valid HTML id (alphanumerics + `_` /
/// `-`; prefix with `anchor-` if it would start with a digit). Matches
/// the safe-name policy `fix_html_id` in calibre's misc helpers.
fn fix_html_id(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let starts_with_alpha = out
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !starts_with_alpha {
        out = format!("anchor-{}", out);
    }
    out
}

/// One NCX nav point.
#[derive(Debug, Clone)]
pub struct NavPoint {
    pub label: String,
    /// `chapter.xhtml#fragment` or `chapter.xhtml` href, relative to OEBPS/.
    pub href: String,
    pub children: Vec<NavPoint>,
}

/// One OPF `<guide><reference>` entry. Mirrors calibre's
/// `add_guide_entry` (yj_to_epub_metadata.py). The `guide_type` value is
/// the EPUB 2.0 guide reference type string ("cover", "text", "toc", ...).
#[derive(Debug, Clone)]
pub struct GuideRef {
    pub guide_type: String,
    pub label: String,
    pub href: String,
}

/// Extract `<guide>` entries from KFX `nav_type=landmarks` containers.
///
/// Calibre's path is `yj_to_epub_navigation.py:140`: iterate every
/// `nav_container` whose `$235 nav_type` is `$236` ("landmarks"), and
/// for each `$393 nav_unit` inside take `$238 landmark_type` → guide
/// type via `GUIDE_TYPE_OF_LANDMARK_TYPE`. The href is resolved like
/// a TOC entry: `(eid, offset)` → chapter file [+ `#anchor`].
pub fn extract_landmarks(
    book: &BookData,
    element_id_to_filename: &HashMap<i64, String>,
    anchors: &AnchorTable,
) -> Vec<GuideRef> {
    let Some(nav) = book.by_type.get(&(KfxSymbol::BookNavigation as u64)) else {
        return Vec::new();
    };
    let mut out: Vec<GuideRef> = Vec::new();
    for value in nav.values() {
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
                if nav_type != "landmarks" {
                    continue;
                }
                let entries: Vec<IonValue> = get_field(cfields, KfxSymbol::Entries as u64)
                    .and_then(|v| v.as_list())
                    .map(|s| s.to_vec())
                    .unwrap_or_default();
                for entry in &entries {
                    if let Some(g) =
                        nav_unit_to_guide(entry, book, element_id_to_filename, anchors)
                    {
                        out.push(g);
                    }
                }
            }
        }
    }
    out
}

fn nav_unit_to_guide(
    entry: &IonValue,
    book: &BookData,
    element_id_to_filename: &HashMap<i64, String>,
    anchors: &AnchorTable,
) -> Option<GuideRef> {
    let inner = entry.unwrap_annotated();
    let fields = inner.as_struct()?;

    // Map `$238 landmark_type` symbol → guide type string. Mirrors
    // calibre's `GUIDE_TYPE_OF_LANDMARK_TYPE`.
    let landmark_type_id = get_field(fields, KfxSymbol::LandmarkType as u64)?.as_symbol()?;
    let landmark_sym_value = IonValue::Symbol(landmark_type_id);
    let guide_type = match landmark_type_id {
        x if x == KfxSymbol::CoverPage as u64 => "cover".to_string(),
        x if x == KfxSymbol::Srl as u64 => "text".to_string(),
        x if x == KfxSymbol::Text as u64 => "text".to_string(),
        x if x == KfxSymbol::Toc as u64 => "toc".to_string(),
        // Defer the rest to the symbol's textual name; OPF 2.0 readers
        // recognise these directly ("preface", "glossary", "loi", ...).
        _ => book.symbols.text_of(&landmark_sym_value)?.to_string(),
    };

    let label = get_field(fields, KfxSymbol::Representation as u64)
        .and_then(|v| v.as_struct())
        .and_then(|s| get_field(s, KfxSymbol::Label as u64))
        .and_then(|v| v.as_string())
        .map(|s| {
            // Calibre's `add_guide_entry` strips the "cover-nav-unit"
            // placeholder; mirror that so we don't emit a literal
            // "cover-nav-unit" label in OPF.
            if s == "cover-nav-unit" { "" } else { s }
        })
        .unwrap_or("")
        .to_string();

    let href = get_field(fields, KfxSymbol::TargetPosition as u64)
        .and_then(|v| v.as_struct())
        .and_then(|pos| {
            let id = get_field(pos, KfxSymbol::Id as u64)?.as_int()?;
            let offset = get_field(pos, KfxSymbol::Offset as u64)
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            let file = element_id_to_filename.get(&id)?.clone();
            Some(match anchors.id_at(id, offset) {
                Some(frag) => format!("{}#{}", file, frag),
                None => file,
            })
        })
        .unwrap_or_default();

    if href.is_empty() {
        return None;
    }

    Some(GuideRef {
        guide_type,
        label,
        href,
    })
}

/// Walk the `book_navigation` fragment and return the TOC tree.
///
/// `element_id_to_filename` is the chapter resolution map built by
/// `content.rs::process_section` — each entry maps an element id (the value
/// stored on storyline elements via `$155 id`) to the chapter `.xhtml` file
/// the element ended up in. We use it to point each nav_unit's
/// `target_position.id` at the right chapter file.
pub fn extract_toc(
    book: &BookData,
    element_id_to_filename: &HashMap<i64, String>,
    anchors: &AnchorTable,
) -> Vec<NavPoint> {
    let Some(nav) = book.by_type.get(&(KfxSymbol::BookNavigation as u64)) else {
        return Vec::new();
    };
    let mut toc: Vec<NavPoint> = Vec::new();

    // book_navigation is typically a list of reading orders, each with
    // nav_containers. The container with nav_type=$214 (= "toc") holds the
    // TOC entries.
    for value in nav.values() {
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
                    if let Some(np) =
                        nav_unit_to_navpoint(entry, element_id_to_filename, anchors)
                    {
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
    element_id_to_filename: &HashMap<i64, String>,
    anchors: &AnchorTable,
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
        .and_then(|pos| {
            let id = get_field(pos, KfxSymbol::Id as u64)?.as_int()?;
            let offset = get_field(pos, KfxSymbol::Offset as u64)
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            let file = element_id_to_filename.get(&id)?.clone();
            // If there's a registered anchor at this (id, offset),
            // append `#anchor-id` so the reader scrolls to the right
            // paragraph instead of the top of the chapter.
            Some(match anchors.id_at(id, offset) {
                Some(frag) => format!("{}#{}", file, frag),
                None => file,
            })
        })
        .unwrap_or_default();

    // Children — recursive.
    let mut children = Vec::new();
    if let Some(child_entries) =
        get_field(fields, KfxSymbol::Entries as u64).and_then(|v| v.as_list())
    {
        for child in child_entries {
            if let Some(np) =
                nav_unit_to_navpoint(child, element_id_to_filename, anchors)
            {
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

/// Render the EPUB 3 nav doc TOC body — `<ol><li><a href="...">title</a></li></ol>`.
/// Used inside `<nav epub:type="toc">` (mandatory in EPUB 3 per W3C spec).
pub fn render_nav_ol(points: &[NavPoint]) -> String {
    let mut s = String::new();
    write_nav_ol(&mut s, points, 4);
    s
}

fn write_nav_ol(s: &mut String, points: &[NavPoint], indent: usize) {
    let pad = "  ".repeat(indent);
    s.push_str(&pad);
    s.push_str("<ol>\n");
    for p in points {
        s.push_str(&pad);
        s.push_str(&format!(
            "  <li><a href=\"{}\">{}</a>",
            xml_escape(&p.href),
            xml_escape(&p.label)
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
