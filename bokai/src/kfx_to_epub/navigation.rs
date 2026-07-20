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

// The emit-side machinery (NavPoint tree, reading-order sort, nav.xhtml/NCX
// serialization) is shared with the IR exporter — this module only *extracts*
// navigation from KFX and hands the shared types to `export::nav`. The anchor
// table and its registration rules are likewise shared (`kfx::anchor_table`);
// this module contributes only the BookData-shaped walks.
pub use crate::export::nav::{NavPoint, sort_toc_reading_order};
use crate::export::opf::OpfGuideRef;
pub use crate::kfx::anchor_table::AnchorTable;
use crate::kfx::anchor_table::{register_heading_levels, register_nav_synthetics};

/// Resolve one `book_navigation.nav_containers` entry to its `nav_container`
/// ($391) struct. Two forms occur: the reflowable path inlines the container
/// struct directly, while the fixed-layout / PDOC path (which the device
/// requires) lists a **symbol** naming a separate `nav_container` entity. This
/// handles both — inline structs pass through; a symbol is looked up in
/// `by_type[$391]` by its resolved name — so the reader (and Sidle) sees the
/// TOC / landmarks of a manga KFX, not just reflowable books. Returns an owned
/// value so the caller can borrow its fields through the loop body.
pub(crate) fn resolve_nav_container(book: &BookData, container: &IonValue) -> Option<IonValue> {
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

/// The `book_navigation` entity values in sorted-name order. Books ship a
/// single entity, but the map walk must still be deterministic — anchor
/// registration order decides co-located first-wins.
fn nav_values_sorted(book: &BookData) -> Vec<&IonValue> {
    let Some(nav) = book.by_type.get(&(KfxSymbol::BookNavigation as u64)) else {
        return Vec::new();
    };
    let mut pairs: Vec<(&String, &IonValue)> = nav.iter().collect();
    pairs.sort_by_key(|(name, _)| *name);
    pairs.into_iter().map(|(_, v)| v).collect()
}

/// Build the anchor table by iterating every `$266 anchor` entity, in
/// sorted-name order (the backing map is unordered; a hash walk would pick a
/// different co-located first anchor run to run).
pub fn extract_anchors(book: &BookData) -> AnchorTable {
    let mut table = AnchorTable::default();
    let Some(anchors) = book.by_type.get(&(KfxSymbol::Anchor as u64)) else {
        return table;
    };
    let mut pairs: Vec<(&String, &IonValue)> = anchors.iter().collect();
    pairs.sort_by_key(|(name, _)| *name);
    for (name, value) in pairs {
        let inner = value.unwrap_annotated();
        let Some(fields) = inner.as_struct() else {
            continue;
        };
        table.register_anchor_fields(name, fields);
    }
    table
}

/// Register heading levels from the `$798 headings` nav container into the
/// anchor table, BEFORE content emission. Each top-level unit's
/// `landmark_type` (`$h2`..`$h6`) sets the level for its nested entries;
/// `content::process_position` later stamps the level onto the matching
/// element so `consolidate_html` promotes it to `<hN>` (the element supplies
/// the `"heading"` layout hint via its `$style`).
///
/// TOC target positions are registered separately by [`register_toc_anchors`]
/// (headings carry a *level*; TOC entries carry a jump *target*).
pub fn register_heading_anchors(book: &BookData, table: &mut AnchorTable) {
    register_heading_levels(
        table,
        nav_values_sorted(book).into_iter(),
        |c| resolve_nav_container(book, c),
        &book.symbols,
    );
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
    register_nav_synthetics(
        table,
        nav_values_sorted(book).into_iter(),
        |c| resolve_nav_container(book, c),
        &book.symbols,
        "toc",
        "toc",
    );
}

/// Register a synthetic id-anchor at every `page_list` target position, so a
/// physical page break that falls mid-chapter (`…#page_N`) resolves to the exact
/// paragraph in the exported EPUB rather than the top of the chapter file.
///
/// Runs *after* [`register_toc_anchors`], so a page whose position coincides
/// with a TOC entry (or a real `$266` anchor) reuses that existing anchor —
/// only page-only positions get a fresh `page-<eid>-<offset>` anchor. Without
/// this, [`extract_page_list`] would drop every `#fragment` and collapse the
/// page list to a run of chapter-top links.
pub fn register_page_list_anchors(book: &BookData, table: &mut AnchorTable) {
    register_nav_synthetics(
        table,
        nav_values_sorted(book).into_iter(),
        |c| resolve_nav_container(book, c),
        &book.symbols,
        "page_list",
        "page",
    );
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
) -> Vec<OpfGuideRef> {
    let Some(nav) = book.by_type.get(&(KfxSymbol::BookNavigation as u64)) else {
        return Vec::new();
    };
    let mut out: Vec<OpfGuideRef> = Vec::new();
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
) -> Option<OpfGuideRef> {
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

    Some(OpfGuideRef {
        guide_type,
        title: label,
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

/// Walk `book_navigation` and return the flat physical page list (EPUB
/// `<nav epub:type="page-list">`), mapping each printed page number to its
/// content location. Same `nav_unit` shape as the TOC — reuse
/// [`nav_unit_to_navpoint`] — but the list is flat and stays in page order (no
/// reading-order sort). Entries with no usable label (Amazon ships a synthetic
/// unlabelled book-start entry) or no resolvable target file are dropped so the
/// emitted nav stays epubcheck-clean.
pub fn extract_page_list(
    book: &BookData,
    element_id_to_filename: &HashMap<i64, String>,
    anchors: &AnchorTable,
    stamped_id_to_file: &HashMap<String, String>,
) -> Vec<NavPoint> {
    let Some(nav) = book.by_type.get(&(KfxSymbol::BookNavigation as u64)) else {
        return Vec::new();
    };
    let mut pages: Vec<NavPoint> = Vec::new();
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
                if nav_type != "page_list" {
                    continue;
                }
                let entries: Vec<IonValue> = get_field(cfields, KfxSymbol::Entries as u64)
                    .and_then(|v| v.as_list())
                    .map(|s| s.to_vec())
                    .unwrap_or_default();
                for entry in &entries {
                    if let Some(mut np) =
                        nav_unit_to_navpoint(entry, element_id_to_filename, anchors)
                    {
                        // Drop the unlabelled book-start sentinel Amazon ships.
                        if np.label == "Untitled" {
                            continue;
                        }
                        // A page break that lands on an already-anchored chapter
                        // start registered a `page-<eid>-0` name that
                        // `process_position` never stamped (the element already
                        // carries its chapter anchor). `id_at` still returns that
                        // name, so strip the fragment when it isn't a real stamped
                        // id — the chapter-file link is where the page starts
                        // anyway, and this avoids a dangling `#page-…` (RSC-012).
                        if let Some(hash) = np.href.find('#') {
                            let stamped = stamped_id_to_file.contains_key(&np.href[hash + 1..]);
                            if !stamped {
                                np.href.truncate(hash);
                            }
                        }
                        // A page whose target didn't resolve to any chapter file.
                        if np.href.is_empty() {
                            continue;
                        }
                        pages.push(np);
                    }
                }
            }
        }
    }
    pages
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
