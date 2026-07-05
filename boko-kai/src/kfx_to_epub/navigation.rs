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

/// Resolve one `book_navigation.nav_containers` entry to its `nav_container`
/// ($391) struct. Two forms occur: the reflowable path inlines the container
/// struct directly, while the fixed-layout / PDOC path (which the device
/// requires) lists a **symbol** naming a separate `nav_container` entity. This
/// handles both — inline structs pass through; a symbol is looked up in
/// `by_type[$391]` by its resolved name — so the reader (and Sidle) sees the
/// TOC / landmarks of a manga KFX, not just reflowable books. Returns an owned
/// value so the caller can borrow its fields through the loop body.
fn resolve_nav_container(book: &BookData, container: &IonValue) -> Option<IonValue> {
    let inner = container.unwrap_annotated();
    if inner.as_struct().is_some() {
        return Some(inner.clone());
    }
    // Referenced form: a symbol naming a separate nav_container entity.
    let name = book.symbols.text_of(inner)?;
    book.by_type
        .get(&(KfxSymbol::NavContainer as u64))
        .and_then(|m| m.get(name))
        .cloned()
}

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

    /// Reverse index `anchor_name → (location_id, offset)`, built alongside
    /// `position_anchors`. `resolve_uri` used to scan the whole
    /// `position_anchors` map for every `<a href="anchor:…">` — an
    /// O(hrefs × anchors) quadratic that cost ~300 ms on a link-dense book.
    /// This makes resolution O(1). The OFFSET is retained (not just the eid)
    /// so `resolve_uri` can resolve a link to ANY anchor at a position to the
    /// SAME element id the *first* anchor there stamped (calibre's
    /// `get_anchor_uri` semantics — `id_at` returns the first anchor's id, so a
    /// link naming a non-first co-located anchor must still point at that id or
    /// it dangles). First registration wins, matching the prior "first match".
    pub name_to_position: HashMap<String, (i64, i64)>,

    /// Heading level (1..=6) registered at a `(location_id, offset)` by the
    /// `$798 headings` nav container. boko's eager equivalent of calibre's
    /// `anchor_heading_level` (name-keyed) + `position_anchors` lookup: since
    /// these synthesized heading anchors are never link targets (only the
    /// element's TAG matters), we key the level directly by position.
    /// `process_position` reads this and stamps `-kfx-heading-level` (here: the
    /// element's `layout_hints` level), which `consolidate_html` promotes to
    /// `<hN>` when the element also carries the `"heading"` layout hint.
    pub heading_level: HashMap<(i64, i64), u8>,
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

    /// Heading level (1..=6) registered at `(eid, offset)` by the `$798`
    /// headings nav, or `None`. Read by `process_position`.
    pub fn heading_level_at(&self, eid: i64, offset: i64) -> Option<u8> {
        self.heading_level.get(&(eid, offset)).copied()
    }

    /// Resolve an anchor to its final `<a href>` using the AUTHORITATIVE
    /// `html-id → file` map built from the emitted DOM, guaranteeing
    /// referential integrity. Unlike `resolve_uri` (which trusts the
    /// structural `element_id_to_filename`, wrong for content that emits in a
    /// different section than the one that structurally claims its eid — e.g.
    /// footnotes — and blind to positions that never got stamped), this
    /// resolves an internal anchor ONLY when its target id was actually stamped
    /// somewhere, and to the file it really landed in. Returns `None` when the
    /// anchor is unresolvable (never stamped, or a blank external URI); the
    /// caller drops the dangling link so no `<a href="…#missing">` is emitted
    /// (epubcheck RSC-012). Matches calibre's behavior of dropping anchors it
    /// can't place.
    pub fn resolve_uri_stamped(
        &self,
        anchor_name: &str,
        stamped_id_to_file: &HashMap<String, String>,
    ) -> Option<String> {
        // External URI (real http link); a blank placeholder is unresolvable.
        if let Some(uri) = self.anchor_uri.get(anchor_name) {
            return if uri.is_empty() {
                None
            } else {
                Some(uri.clone())
            };
        }
        // Internal position anchor — resolve to the file the stamped id is
        // actually in (not the structural guess).
        let &(eid, offset) = self.name_to_position.get(anchor_name)?;
        let frag = self.id_at(eid, offset)?;
        let file = stamped_id_to_file.get(&frag)?;
        Some(format!("{file}#{frag}"))
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
            let Some(eid) = get_field(pos_fields, KfxSymbol::Id as u64).and_then(|v| v.as_int())
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
            // Reverse index for O(1) resolve_uri; first registration wins.
            table
                .name_to_position
                .entry(name.clone())
                .or_insert((eid, offset));
        }
    }
    table
}

/// Register heading levels from the `$798 headings` nav container into the
/// anchor table, BEFORE content emission. Mirrors the subset of calibre's
/// `process_nav_unit` (`yj_to_epub_navigation.py:199-258`) that derives
/// `heading_level` from a `$798` container: each top-level unit's
/// `landmark_type` (`$h2`..`$h6`) sets the level for its nested entries, and
/// every nested unit's `target_position` `(eid, offset)` is recorded at that
/// level. `content::process_position` later stamps the level onto the matching
/// element so `consolidate_html` promotes it to `<hN>` (the element supplies
/// the `"heading"` layout hint via its `$style`).
///
/// TOC target positions are registered separately by [`register_toc_anchors`]
/// (headings carry a *level*; TOC entries carry a jump *target*). page_list /
/// landmark anchors are still resolved eagerly via `id_at` + the chapter-file
/// map, which is enough for those.
pub fn register_heading_anchors(book: &BookData, table: &mut AnchorTable) {
    let Some(nav) = book.by_type.get(&(KfxSymbol::BookNavigation as u64)) else {
        return;
    };
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
                let Some(resolved) = resolve_nav_container(book, container) else {
                    continue;
                };
                let Some(cfields) = resolved.as_struct() else {
                    continue;
                };
                let nav_type = get_field(cfields, KfxSymbol::NavType as u64)
                    .and_then(|v| book.symbols.text_of(v))
                    .unwrap_or("");
                if nav_type != "headings" {
                    continue;
                }
                let Some(level_units) =
                    get_field(cfields, KfxSymbol::Entries as u64).and_then(|v| v.as_list())
                else {
                    continue;
                };
                for level_unit in level_units {
                    register_heading_level_unit(level_unit, book, table);
                }
            }
        }
    }
}

/// Register a synthetic id-anchor at every TOC target position, so intra-chapter
/// nav fragments resolve even for a KFX that ships no internal `$266` anchors.
///
/// boko's own e2k KFX is exactly that case: its `$266` table holds only
/// external-URL anchors, so `id_at(eid, offset)` returns `None` for every TOC
/// target and [`nav_unit_to_navpoint`] / [`AnchorTable::resolve_uri`] drop the
/// `#fragment` — every nested entry then collapses to the top of its chapter
/// file. Unusable in the Sidle reader and the EPUB export; the *device* is
/// unaffected because it jumps via `book_navigation` positions directly, which
/// is why it stayed hidden (and why the k2e port originally skipped this).
///
/// Anchors are added only at positions that have none, so a real Amazon KFX —
/// whose `$266` table already anchors these positions — is byte-for-byte
/// unchanged: `id_at` still returns its real first-anchor id. The synthetic name
/// (`toc-<eid>-<offset>`) is unique per position and stamped onto the target
/// element by `content::process_position`.
pub fn register_toc_anchors(book: &BookData, table: &mut AnchorTable) {
    let Some(nav) = book.by_type.get(&(KfxSymbol::BookNavigation as u64)) else {
        return;
    };
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
                let Some(resolved) = resolve_nav_container(book, container) else {
                    continue;
                };
                let Some(cfields) = resolved.as_struct() else {
                    continue;
                };
                let nav_type = get_field(cfields, KfxSymbol::NavType as u64)
                    .and_then(|v| book.symbols.text_of(v))
                    .unwrap_or("");
                if nav_type != "toc" {
                    continue;
                }
                let Some(entries) =
                    get_field(cfields, KfxSymbol::Entries as u64).and_then(|v| v.as_list())
                else {
                    continue;
                };
                for entry in entries {
                    register_toc_entry_anchor(entry, table);
                }
            }
        }
    }
}

/// Register one TOC entry's target (if the position is not already anchored),
/// then recurse into its nested entries.
fn register_toc_entry_anchor(entry: &IonValue, table: &mut AnchorTable) {
    if let Some((eid, offset)) = nav_target_position(entry) {
        let occupied = table
            .position_anchors
            .get(&eid)
            .and_then(|m| m.get(&offset))
            .is_some_and(|names| !names.is_empty());
        if !occupied {
            let name = format!("toc-{eid}-{offset}");
            table
                .position_anchors
                .entry(eid)
                .or_default()
                .entry(offset)
                .or_default()
                .push(name.clone());
            table.name_to_position.entry(name).or_insert((eid, offset));
        }
    }
    if let Some(children) = entry
        .unwrap_annotated()
        .as_struct()
        .and_then(|f| get_field(f, KfxSymbol::Entries as u64))
        .and_then(|v| v.as_list())
    {
        for child in children {
            register_toc_entry_anchor(child, table);
        }
    }
}

fn register_heading_level_unit(value: &IonValue, book: &BookData, table: &mut AnchorTable) {
    let inner = value.unwrap_annotated();
    let Some(fields) = inner.as_struct() else {
        return;
    };
    let Some(level) = get_field(fields, KfxSymbol::LandmarkType as u64)
        .and_then(|v| book.symbols.text_of(v))
        .and_then(level_of_landmark)
    else {
        return;
    };
    let Some(nested) = get_field(fields, KfxSymbol::Entries as u64).and_then(|v| v.as_list())
    else {
        return;
    };
    for unit in nested {
        if let Some((eid, offset)) = nav_target_position(unit) {
            table.heading_level.entry((eid, offset)).or_insert(level);
        }
    }
}

/// Kindle headings `landmark_type` symbol → heading level. h1 is included for
/// completeness even though Kindle omits it from the headings nav.
fn level_of_landmark(name: &str) -> Option<u8> {
    match name {
        "h1" | "$h1" => Some(1),
        "h2" | "$h2" => Some(2),
        "h3" | "$h3" => Some(3),
        "h4" | "$h4" => Some(4),
        "h5" | "$h5" => Some(5),
        "h6" | "$h6" => Some(6),
        _ => None,
    }
}

/// The `(location_id, offset)` a nav_unit's `$246 target_position` points at.
/// Shared by heading-level and TOC-anchor registration (both key off the same
/// target).
fn nav_target_position(unit: &IonValue) -> Option<(i64, i64)> {
    let fields = unit.unwrap_annotated().as_struct()?;
    get_field(fields, KfxSymbol::TargetPosition as u64)
        .and_then(|v| v.as_struct())
        .and_then(|pos| {
            let id = get_field(pos, KfxSymbol::Id as u64)?.as_int()?;
            let offset = get_field(pos, KfxSymbol::Offset as u64)
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            Some((id, offset))
        })
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
                let Some(resolved) = resolve_nav_container(book, container) else {
                    continue;
                };
                let Some(cfields) = resolved.as_struct() else {
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
                    if let Some(g) = nav_unit_to_guide(entry, book, element_id_to_filename, anchors)
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
                let Some(resolved) = resolve_nav_container(book, container) else {
                    continue;
                };
                let Some(cfields) = resolved.as_struct() else {
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
                    if let Some(np) = nav_unit_to_navpoint(entry, element_id_to_filename, anchors) {
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
        .or_else(|| get_field(fields, KfxSymbol::Label as u64).and_then(|v| v.as_string()))
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
            if let Some(np) = nav_unit_to_navpoint(child, element_id_to_filename, anchors) {
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
    let mut ctx = NavmapCtx {
        next_id: 1,
        next_play_order: 1,
        play_order_by_target: HashMap::new(),
    };
    write_points(&mut s, points, &mut ctx, 2);
    s
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

fn write_points(s: &mut String, points: &[NavPoint], ctx: &mut NavmapCtx, indent: usize) {
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
            xml_escape(&p.label)
        ));
        s.push_str(&format!(
            "{}  <content src=\"{}\"/>\n",
            prefix,
            xml_escape(&p.href)
        ));
        if !p.children.is_empty() {
            write_points(s, &p.children, ctx, indent + 1);
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
