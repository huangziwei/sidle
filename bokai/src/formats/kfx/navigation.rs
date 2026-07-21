//! `book_navigation` ($389) walks: nav-container resolution, anchor-table
//! registration, and TOC extraction.
//!
//! Reads navigation straight out of the fragment graph and hands back IR
//! vocabulary ([`TocEntry`], [`AnchorTable`]) — no emit-side types, so both
//! directions and the validators can use it.

use crate::formats::kfx::anchor_table::AnchorTable;
use crate::formats::kfx::container::get_field;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::BookData;
use crate::formats::kfx::symbols::KfxSymbol;
use crate::model::TocEntry;

/// Resolve one `book_navigation.nav_containers` entry to its `nav_container`
/// ($391) struct. Two forms occur: the reflowable path inlines the container
/// struct directly, while the fixed-layout / PDOC path (which the device
/// requires) lists a **symbol** naming a separate `nav_container` entity. This
/// handles both — inline structs pass through; a symbol is looked up in
/// `by_type[$391]` by its resolved name. Returns an owned value so the caller
/// can borrow its fields through the loop body.
pub fn resolve_nav_container(book: &BookData, container: &IonValue) -> Option<IonValue> {
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

/// Visit every nav container in the book's `$389 book_navigation`, resolved to
/// its `nav_type` and `$247 entries`.
///
/// `book_navigation` holds one entry per reading order (or a bare struct when
/// there is only one), each listing its containers; the containers themselves
/// come in both the inline and the referenced form (see
/// [`resolve_nav_container`]). This is the walk down to the entry lists —
/// what a caller does with `toc` / `page_list` / anything else is its own.
pub fn for_each_nav_container(book: &BookData, mut visit: impl FnMut(&str, &[IonValue])) {
    let Some(nav) = book.by_type.get(&(KfxSymbol::BookNavigation as u64)) else {
        return;
    };
    for value in nav.values() {
        let unwrapped = value.unwrap_annotated();
        let reading_orders: Vec<IonValue> = match unwrapped {
            IonValue::List(items) => items.clone(),
            IonValue::Struct(_) => vec![unwrapped.clone()],
            _ => Vec::new(),
        };
        for reading_order in reading_orders {
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
                let entries: Vec<IonValue> = get_field(cfields, KfxSymbol::Entries as u64)
                    .and_then(|v| v.as_list())
                    .map(|s| s.to_vec())
                    .unwrap_or_default();
                visit(nav_type, &entries);
            }
        }
    }
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

/// Extract the TOC tree from the `nav_type=toc` container.
///
/// `element_id_to_filename` maps a target position's element id to the file it
/// landed in; callers that only want the label tree (no resolved hrefs) pass an
/// empty map and every `href` comes back blank.
pub fn extract_toc(
    book: &BookData,
    element_id_to_filename: &std::collections::HashMap<i64, String>,
    anchors: &AnchorTable,
) -> Vec<TocEntry> {
    // The container with nav_type=$214 (= "toc") holds the TOC entries.
    let mut toc: Vec<TocEntry> = Vec::new();
    for_each_nav_container(book, |nav_type, entries| {
        if nav_type != "toc" {
            return;
        }
        for entry in entries {
            if let Some(e) = nav_unit_to_entry(entry, element_id_to_filename, anchors) {
                toc.push(e);
            }
        }
    });
    toc
}

/// One `$393 nav_unit` → [`TocEntry`], recursively. `None` for the entries
/// calibre drops: blank labels and the `heading-nav-unit` placeholder.
fn nav_unit_to_entry(
    entry: &IonValue,
    element_id_to_filename: &std::collections::HashMap<i64, String>,
    anchors: &AnchorTable,
) -> Option<TocEntry> {
    let inner = entry.unwrap_annotated();
    let fields = inner.as_struct()?;

    // Label: prefer representation.label, then direct label.
    let title = get_field(fields, KfxSymbol::Representation as u64)
        .and_then(|v| v.as_struct())
        .and_then(|s| get_field(s, KfxSymbol::Label as u64))
        .and_then(|v| v.as_string())
        .or_else(|| get_field(fields, KfxSymbol::Label as u64).and_then(|v| v.as_string()))
        .unwrap_or("Untitled")
        .to_string();
    if title.is_empty() || title == "heading-nav-unit" {
        return None;
    }

    // Target position gives (id, offset); the id resolves to a file through
    // `element_id_to_filename`, and a registered anchor at that exact position
    // adds the `#fragment` so the target is the paragraph, not the chapter top.
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

    let mut children = Vec::new();
    if let Some(child_entries) =
        get_field(fields, KfxSymbol::Entries as u64).and_then(|v| v.as_list())
    {
        for child in child_entries {
            if let Some(e) = nav_unit_to_entry(child, element_id_to_filename, anchors) {
                children.push(e);
            }
        }
    }

    Some(TocEntry {
        title,
        href,
        children,
        play_order: None,
        target: None,
    })
}
