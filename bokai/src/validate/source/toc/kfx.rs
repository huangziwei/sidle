//! KFX-native TOC evidence for [`super::validate`].
//!
//! Reads a loaded KFX's own structures: the `nav_container` toc (declared TOC),
//! internal `link_to` ($179) anchors clustered on the 目次/Contents page, styled
//! headings, and storylines opening with a bare chapter marker. Never a derived
//! copy.

use std::collections::{HashMap, HashSet};

use crate::formats::kfx::anchor_table::AnchorTable;
use crate::formats::kfx::container::get_field;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::BookData;
use crate::formats::kfx::navigation::{extract_anchors, extract_toc, resolve_nav_container};
use crate::formats::kfx::structure::{resolve_content_text, style_layout_hints_for};
use crate::formats::kfx::symbols::KfxSymbol;
use crate::formats::kfx::yj_properties as properties;
use crate::model::TocEntry;

use super::{MIN_EVIDENCE, TocEvidence, is_chapter_marker};

pub(super) fn evidence(book: &BookData) -> TocEvidence {
    let anchors = extract_anchors(book);
    let empty_files = HashMap::new();
    let toc = extract_toc(book, &empty_files, &AnchorTable::default());

    let mut nav_labels = Vec::new();
    flatten_labels(&toc, &mut nav_labels);

    let (contents_links, contents_sample) = in_book_contents(book, &anchors);
    TocEvidence {
        nav_labels,
        contents_links,
        contents_sample,
        headings: count_headings(book),
        section_heads: count_section_heads(book),
        has_toc_landmark: toc_landmark_eid(book).is_some(),
        // Flattened-volume detection is EPUB-only so far; a KFX 合本版 reads as
        // unflattened until the KFX proposer learns the same grouping.
        flattened: Default::default(),
    }
}

fn flatten_labels(points: &[TocEntry], out: &mut Vec<String>) {
    for p in points {
        out.push(p.title.clone());
        flatten_labels(&p.children, out);
    }
}

/// The in-book Contents page's chapter links: distinct internal `link_to`
/// destinations in the storyline that carries the most of them. When a `toc`
/// landmark names a page, the storyline containing that page's eid wins even if
/// another storyline has more links (e.g. a footnote-dense chapter). Returns the
/// count and a few source-text samples for the report.
fn in_book_contents(book: &BookData, anchors: &AnchorTable) -> (usize, Vec<String>) {
    let landmark_eid = toc_landmark_eid(book);
    let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) else {
        return (0, Vec::new());
    };

    let mut best_count = 0usize;
    let mut best_sample: Vec<String> = Vec::new();
    let mut landmark_hit: Option<(usize, Vec<String>)> = None;

    for storyline in storylines.values() {
        let mut links = Vec::new();
        collect_link_to(storyline, book, &mut links);
        let mut dests: HashSet<(i64, i64)> = HashSet::new();
        for name in &links {
            if let Some(&pos) = anchors.name_to_position.get(name) {
                dests.insert(pos);
            }
        }
        let sample: Vec<String> = links.iter().take(6).cloned().collect();

        let count = dests.len();
        if count > best_count {
            best_count = count;
            best_sample = sample.clone();
        }
        if let Some(le) = landmark_eid
            && count >= MIN_EVIDENCE
        {
            let mut ids = HashSet::new();
            collect_ids(storyline, &mut ids);
            if ids.contains(&le) {
                landmark_hit = Some((count, sample));
            }
        }
    }

    landmark_hit.unwrap_or((best_count, best_sample))
}

/// Count heading-styled elements across all storylines — bokai's `<hN>` promotion
/// criterion (a `$760 treat_as_title` style, or a `$761 layout_hints` list
/// containing "heading"). Style→heading resolution is memoised per style name.
fn count_headings(book: &BookData) -> usize {
    let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) else {
        return 0;
    };
    let mut memo: HashMap<String, bool> = HashMap::new();
    let mut n = 0usize;
    for sv in storylines.values() {
        count_headings_in(sv, book, &mut memo, &mut n);
    }
    n
}

fn count_headings_in(
    value: &IonValue,
    book: &BookData,
    memo: &mut HashMap<String, bool>,
    n: &mut usize,
) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            let mut is_heading = false;
            if let Some(name) =
                get_field(fields, KfxSymbol::Style as u64).and_then(|v| book.symbols.text_of(v))
            {
                is_heading = *memo.entry(name.to_string()).or_insert_with(|| {
                    style_layout_hints_for(name, book)
                        .0
                        .iter()
                        .any(|h| h == "heading")
                });
            }
            if !is_heading {
                let (hints, _) = properties::layout_hints_from_element_fields(fields);
                is_heading = hints.iter().any(|h| h == "heading");
            }
            if is_heading {
                *n += 1;
            }
            for (_, v) in fields {
                count_headings_in(v, book, memo, n);
            }
        }
        IonValue::List(items) => {
            for it in items {
                count_headings_in(it, book, memo, n);
            }
        }
        _ => {}
    }
}

/// Count storylines whose first text block is a bare chapter marker. Some editions
/// (e.g. Hayakawa mysteries) split each chapter into its own storyline and open it
/// with just the chapter number — no anchor, no heading style — so this is the
/// only machine-readable trace of the chapter list. A continuous novella opens
/// each storyline with prose, so this stays ~0.
fn count_section_heads(book: &BookData) -> usize {
    let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) else {
        return 0;
    };
    storylines
        .values()
        .filter(|s| first_text(s, book).is_some_and(|t| is_chapter_marker(&t)))
        .count()
}

/// The first non-empty text of a value tree (depth-first). Resolves `$145
/// content` via the same helper the emitter uses.
fn first_text(value: &IonValue, book: &BookData) -> Option<String> {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            if let Some(c) = get_field(fields, KfxSymbol::Content as u64) {
                let t = resolve_content_text(c, book);
                if !t.trim().is_empty() {
                    return Some(t);
                }
            }
            for (_, v) in fields {
                if let Some(t) = first_text(v, book) {
                    return Some(t);
                }
            }
            None
        }
        IonValue::List(items) => items.iter().find_map(|it| first_text(it, book)),
        _ => None,
    }
}

/// Recursively gather every `link_to` anchor name in a value tree.
fn collect_link_to(value: &IonValue, book: &BookData, out: &mut Vec<String>) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            if let Some(lt) = get_field(fields, KfxSymbol::LinkTo as u64)
                && let Some(name) = book.symbols.text_of(lt)
            {
                out.push(name.to_string());
            }
            for (_, v) in fields {
                collect_link_to(v, book, out);
            }
        }
        IonValue::List(items) => {
            for it in items {
                collect_link_to(it, book, out);
            }
        }
        _ => {}
    }
}

/// Every `$155 id` in a value tree — used to locate which storyline a landmark
/// eid falls in.
fn collect_ids(value: &IonValue, out: &mut HashSet<i64>) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            if let Some(id) = get_field(fields, KfxSymbol::Id as u64).and_then(|v| v.as_int()) {
                out.insert(id);
            }
            for (_, v) in fields {
                collect_ids(v, out);
            }
        }
        IonValue::List(items) => {
            for it in items {
                collect_ids(it, out);
            }
        }
        _ => {}
    }
}

/// The eid the `toc`-type landmark targets, if the book has one.
fn toc_landmark_eid(book: &BookData) -> Option<i64> {
    let nav = book.by_type.get(&(KfxSymbol::BookNavigation as u64))?;
    for value in nav.values() {
        let unwrapped = value.unwrap_annotated();
        let candidates: Vec<IonValue> = match unwrapped {
            IonValue::List(items) => items.clone(),
            IonValue::Struct(_) => vec![unwrapped.clone()],
            _ => Vec::new(),
        };
        for reading_order in candidates {
            let Some(ro) = reading_order.as_struct() else {
                continue;
            };
            let Some(containers) =
                get_field(ro, KfxSymbol::NavContainers as u64).and_then(|v| v.as_list())
            else {
                continue;
            };
            for container in containers {
                let Some(resolved) = resolve_nav_container(book, container) else {
                    continue;
                };
                let Some(cf) = resolved.as_struct() else {
                    continue;
                };
                let nav_type = get_field(cf, KfxSymbol::NavType as u64)
                    .and_then(|v| book.symbols.text_of(v))
                    .unwrap_or("");
                if nav_type != "landmarks" {
                    continue;
                }
                let Some(entries) =
                    get_field(cf, KfxSymbol::Entries as u64).and_then(|v| v.as_list())
                else {
                    continue;
                };
                for entry in entries {
                    if let Some(eid) = landmark_toc_target(entry) {
                        return Some(eid);
                    }
                }
            }
        }
    }
    None
}

/// If this landmark entry is the `toc` type, its target eid.
fn landmark_toc_target(entry: &IonValue) -> Option<i64> {
    let fields = entry.unwrap_annotated().as_struct()?;
    let lt = get_field(fields, KfxSymbol::LandmarkType as u64)?.as_symbol()?;
    if lt != KfxSymbol::Toc as u64 {
        return None;
    }
    get_field(fields, KfxSymbol::TargetPosition as u64)
        .and_then(|v| v.as_struct())
        .and_then(|pos| get_field(pos, KfxSymbol::Id as u64)?.as_int())
}
