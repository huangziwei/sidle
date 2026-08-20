//! Surgical TOC repair for a KFX container: derive a chapter list from the
//! book's own in-book Contents page and overwrite a deficient/front-matter
//! `nav_container` toc with it, in place.
//!
//! [`propose_toc`] reads the chapter list off the book's Contents page (the same
//! page [`bokai validate toc`](crate::validate::source::toc) reads as ground
//! truth); [`set_toc`] writes a caller-supplied list into the toc container;
//! [`repair_toc`] is the two composed. The write rebuilds the toc
//! `nav_container`'s `entries` via the container edit harness
//! ([`edit_container`]): the toc container keeps its `nav_type` and
//! `nav_container_name` symbol and every other fragment passes through verbatim,
//! so **no doc-symbol growth is needed** — the one primitive the harness lacks.
//!
//! Because the KFX *is* the reader/device file, repairing it fixes the nav any
//! app renders straight from the KFX; re-importing then re-derives the EPUB nav
//! from the corrected source. One source edit, both surfaces.
//!
//! ⚠️ Device strictness: the offline reader is more permissive than a Kindle on
//! nav. A repaired container must still clear the offline entity differ and a
//! device round-trip before it's trusted on-device.
//! The nav-unit shape here mirrors bokai's proven `pdf_to_kfx` toc export exactly
//! (`representation:{label}` + `target_position:{id, offset:0}`).
//!
//! v1 scope: the book must already carry a `book_navigation` fragment (every
//! reflowable KFX does). A deficient toc container is overwritten in place; when
//! none exists, a fresh inline toc container is added, reusing the system `toc`
//! symbol ($212) for its name — so no doc-symbol growth is needed. A book with no
//! `book_navigation` at all, and the fixed-layout referenced-container shape, are
//! not yet handled.

use std::collections::{HashMap, HashSet};

use crate::formats::kfx::anchor_table::AnchorTable;
use crate::formats::kfx::container::get_field;
use crate::formats::kfx::container_edit::{EntityEdit, edit_container};
use crate::formats::kfx::error::KfxError;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::{self, BookData};
use crate::formats::kfx::navigation;
use crate::formats::kfx::navigation::{
    extract_anchors, for_each_nav_container, nav_unit_label, resolve_nav_container,
};
use crate::formats::kfx::structure::{
    collect_element_ids, lookup_fragment, reading_orders, resolve_content_text,
};
use crate::formats::kfx::symbols::KfxSymbol;
use crate::model::LandmarkType;
use crate::model::toc_shape::{TocNode, merge_by_document_order, nest_by_label_indent};

/// One chapter in an edited TOC. `eid` is the target element's `$155 id`; the
/// target offset is always written as 0 — the Kindle TOC convention (a jump
/// lands on the chapter-start element, and a non-zero offset can wedge the
/// firmware). Nesting is supported for sub-chapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    pub label: String,
    pub eid: i64,
    pub children: Vec<TocEntry>,
}

impl TocEntry {
    /// A leaf chapter entry (no children).
    pub fn new(label: impl Into<String>, eid: i64) -> Self {
        Self {
            label: label.into(),
            eid,
            children: Vec::new(),
        }
    }
}

impl TocNode for TocEntry {
    fn label(&self) -> &str {
        &self.label
    }
    fn set_label(&mut self, label: String) {
        self.label = label;
    }
    fn children(&self) -> &[Self] {
        &self.children
    }
    fn set_children(&mut self, children: Vec<Self>) {
        self.children = children;
    }
    /// Every entry is written at offset 0, so two entries land on the same place
    /// exactly when they target the same element.
    fn target_key(&self) -> String {
        self.eid.to_string()
    }
}

/// Overwrite the KFX's toc `nav_container` entries with `entries` — or, when the
/// book declares no toc container, synthesize a fresh inline one in its
/// `book_navigation`. In place.
///
/// Errors (via [`KfxError::InvalidKfx`]) if `entries` is empty, if the bytes
/// aren't a KFX container, or if the book has no `book_navigation` at all to
/// attach a toc to (creating one from scratch is not yet supported).
pub fn set_toc(kfx_bytes: &[u8], entries: &[TocEntry]) -> Result<Vec<u8>, KfxError> {
    if entries.is_empty() {
        return Err(KfxError::InvalidKfx(
            "refusing to write an empty TOC".into(),
        ));
    }
    let book = loader::load(kfx_bytes)?;
    let new_entries: Vec<IonValue> = entries.iter().map(build_nav_unit).collect();

    match detect_toc_mode(&book) {
        // The book already declares a toc container — overwrite its entries.
        Some(mode) => edit_container(kfx_bytes, |e| match &mode {
            // Reflowable: the toc container is inlined in the book_navigation
            // ($389) fragment — rewrite that entity.
            TocMode::Inline if e.is_type(KfxSymbol::BookNavigation) => Ok(EntityEdit::Ion(
                rewrite_inline_toc(&book, &e.parse_ion()?, &new_entries),
            )),
            // Fixed-layout / PDOC: the toc container is a separate nav_container
            // ($391) entity named `name` — rewrite that entity, leaving $389.
            TocMode::Referenced(name)
                if e.is_type(KfxSymbol::NavContainer)
                    && book.symbols.resolve(e.id() as u64) == *name =>
            {
                Ok(EntityEdit::Ion(replace_container_entries(
                    &e.parse_ion()?,
                    &new_entries,
                )))
            }
            _ => Ok(EntityEdit::Keep),
        }),
        // No toc container at all — add a fresh inline one to the book_navigation.
        None => synthesize_toc(kfx_bytes, &book, &new_entries),
    }
}

/// Add a fresh inline toc `nav_container` to the book's existing
/// `book_navigation` ($389) — the no-existing-toc case that most chapterless
/// books hit. The container reuses the system `toc` symbol ($212) for both its
/// `nav_type` and `nav_container_name`, so no doc-symbol growth is needed; every
/// other fragment (and reading order) passes through untouched. Errors if the
/// book has no `book_navigation` to attach to.
fn synthesize_toc(
    kfx_bytes: &[u8],
    book: &BookData,
    new_entries: &[IonValue],
) -> Result<Vec<u8>, KfxError> {
    let has_nav = book
        .by_type
        .get(&(KfxSymbol::BookNavigation as u64))
        .is_some_and(|m| !m.is_empty());
    if !has_nav {
        return Err(KfxError::InvalidKfx(
            "KFX has no book_navigation to add a TOC to (creating one is not yet supported)".into(),
        ));
    }
    // Add to the first book_navigation entity only (there is normally one); a
    // captured flag keeps a multi-fragment book from getting duplicate tocs.
    let mut done = false;
    edit_container(kfx_bytes, |e| {
        if !done && e.is_type(KfxSymbol::BookNavigation) {
            done = true;
            Ok(EntityEdit::Ion(add_toc_container(
                &e.parse_ion()?,
                new_entries,
            )))
        } else {
            Ok(EntityEdit::Keep)
        }
    })
}

/// Build a fresh inline toc `nav_container`:
/// `nav_container::{nav_type: toc, nav_container_name: toc, entries: [...]}`.
/// Both symbols are the system `toc` ($212), so nothing new is interned.
fn build_toc_container(new_entries: &[IonValue]) -> IonValue {
    IonValue::Annotated(
        vec![KfxSymbol::NavContainer as u64],
        Box::new(IonValue::Struct(vec![
            (
                KfxSymbol::NavType as u64,
                IonValue::Symbol(KfxSymbol::Toc as u64),
            ),
            (
                KfxSymbol::NavContainerName as u64,
                IonValue::Symbol(KfxSymbol::Toc as u64),
            ),
            (
                KfxSymbol::Entries as u64,
                IonValue::List(new_entries.to_vec()),
            ),
        ])),
    )
}

/// Insert the synthesized toc container into the `book_navigation` value's first
/// reading order. Preserves annotations and every existing reading order /
/// container.
fn add_toc_container(nav: &IonValue, new_entries: &[IonValue]) -> IonValue {
    match nav {
        IonValue::Annotated(anns, inner) => IonValue::Annotated(
            anns.clone(),
            Box::new(add_toc_container(inner, new_entries)),
        ),
        // A list of reading orders — add the toc to the first only.
        IonValue::List(ros) => {
            let mut out = Vec::with_capacity(ros.len());
            let mut first = true;
            for ro in ros {
                if first {
                    out.push(add_toc_to_reading_order(ro, new_entries));
                    first = false;
                } else {
                    out.push(ro.clone());
                }
            }
            IonValue::List(out)
        }
        // A single reading-order struct.
        IonValue::Struct(_) => add_toc_to_reading_order(nav, new_entries),
        other => other.clone(),
    }
}

/// Prepend the synthesized toc container to a reading order's `nav_containers`
/// (the exporter orders toc before landmarks), adding the field if absent.
fn add_toc_to_reading_order(ro: &IonValue, new_entries: &[IonValue]) -> IonValue {
    match ro {
        IonValue::Annotated(anns, inner) => IonValue::Annotated(
            anns.clone(),
            Box::new(add_toc_to_reading_order(inner, new_entries)),
        ),
        IonValue::Struct(fields) => {
            let toc = build_toc_container(new_entries);
            let mut out: Vec<(u64, IonValue)> = Vec::with_capacity(fields.len() + 1);
            let mut added = false;
            for (k, v) in fields {
                if *k == KfxSymbol::NavContainers as u64
                    && let Some(list) = v.as_list()
                {
                    let mut containers = Vec::with_capacity(list.len() + 1);
                    containers.push(toc.clone());
                    containers.extend(list.iter().cloned());
                    out.push((*k, IonValue::List(containers)));
                    added = true;
                } else {
                    out.push((*k, v.clone()));
                }
            }
            if !added {
                out.push((KfxSymbol::NavContainers as u64, IonValue::List(vec![toc])));
            }
            IonValue::Struct(out)
        }
        other => other.clone(),
    }
}

/// Where the toc `nav_container` lives, so [`set_toc`] knows which entity to
/// rewrite.
enum TocMode {
    /// Inlined in the `book_navigation` ($389) fragment (reflowable path).
    Inline,
    /// A separate `nav_container` ($391) entity of this resolved name (the
    /// fixed-layout / PDOC path the device requires for manga/PDF).
    Referenced(String),
}

/// Locate the toc `nav_container` and classify how it's referenced. Mirrors the
/// reader's [`resolve_nav_container`] walk. `None` when the book declares no toc.
fn detect_toc_mode(book: &BookData) -> Option<TocMode> {
    let nav = book.by_type.get(&(KfxSymbol::BookNavigation as u64))?;
    for value in nav.values() {
        for ro in nav_reading_orders(value) {
            let Some(ro_fields) = ro.as_struct() else {
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
                if nav_type_of(&resolved, book) != Some("toc") {
                    continue;
                }
                return Some(match container.unwrap_annotated() {
                    IonValue::Symbol(id) => {
                        TocMode::Referenced(book.symbols.resolve(*id).to_string())
                    }
                    _ => TocMode::Inline,
                });
            }
        }
    }
    None
}

/// The `book_navigation` fragment is either a single reading-order struct or a
/// list of them. Normalize to a vec (mirrors the reader).
fn nav_reading_orders(value: &IonValue) -> Vec<IonValue> {
    match value.unwrap_annotated() {
        IonValue::List(items) => items.clone(),
        s @ IonValue::Struct(_) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// The `nav_type` text of a resolved `nav_container` struct.
fn nav_type_of<'a>(container: &'a IonValue, book: &'a BookData) -> Option<&'a str> {
    let fields = container.as_struct()?;
    book.symbols
        .text_of(get_field(fields, KfxSymbol::NavType as u64)?)
}

/// Build one `nav_unit` — `nav_unit::{representation:{label},
/// target_position:{id, offset:0}}` plus nested `entries` — matching bokai's
/// `pdf_to_kfx` toc export exactly.
fn build_nav_unit(entry: &TocEntry) -> IonValue {
    let mut fields = vec![
        (
            KfxSymbol::Representation as u64,
            IonValue::Struct(vec![(
                KfxSymbol::Label as u64,
                IonValue::String(entry.label.clone()),
            )]),
        ),
        (
            KfxSymbol::TargetPosition as u64,
            IonValue::Struct(vec![
                (KfxSymbol::Id as u64, IonValue::Int(entry.eid)),
                (KfxSymbol::Offset as u64, IonValue::Int(0)),
            ]),
        ),
    ];
    if !entry.children.is_empty() {
        let kids: Vec<IonValue> = entry.children.iter().map(build_nav_unit).collect();
        fields.push((KfxSymbol::Entries as u64, IonValue::List(kids)));
    }
    IonValue::Annotated(
        vec![KfxSymbol::NavUnit as u64],
        Box::new(IonValue::Struct(fields)),
    )
}

/// Rewrite the `book_navigation` ($389) value, replacing the inline toc
/// container's `entries`. Preserves annotations and every other reading-order /
/// container field.
fn rewrite_inline_toc(book: &BookData, nav: &IonValue, new_entries: &[IonValue]) -> IonValue {
    match nav {
        IonValue::Annotated(anns, inner) => IonValue::Annotated(
            anns.clone(),
            Box::new(rewrite_inline_toc(book, inner, new_entries)),
        ),
        IonValue::List(ros) => IonValue::List(
            ros.iter()
                .map(|ro| rewrite_reading_order(book, ro, new_entries))
                .collect(),
        ),
        IonValue::Struct(_) => rewrite_reading_order(book, nav, new_entries),
        other => other.clone(),
    }
}

fn rewrite_reading_order(book: &BookData, ro: &IonValue, new_entries: &[IonValue]) -> IonValue {
    match ro {
        IonValue::Annotated(anns, inner) => IonValue::Annotated(
            anns.clone(),
            Box::new(rewrite_reading_order(book, inner, new_entries)),
        ),
        IonValue::Struct(fields) => IonValue::Struct(
            fields
                .iter()
                .map(|(k, v)| {
                    if *k == KfxSymbol::NavContainers as u64
                        && let Some(list) = v.as_list()
                    {
                        let rebuilt = list
                            .iter()
                            .map(|c| maybe_replace_toc(book, c, new_entries))
                            .collect();
                        (*k, IonValue::List(rebuilt))
                    } else {
                        (*k, v.clone())
                    }
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// If `container` is the inline toc container, replace its `entries`; else leave
/// it untouched.
fn maybe_replace_toc(book: &BookData, container: &IonValue, new_entries: &[IonValue]) -> IonValue {
    let is_toc = container
        .unwrap_annotated()
        .as_struct()
        .and_then(|f| get_field(f, KfxSymbol::NavType as u64))
        .and_then(|v| book.symbols.text_of(v))
        == Some("toc");
    if is_toc {
        replace_container_entries(container, new_entries)
    } else {
        container.clone()
    }
}

/// Replace a `nav_container` struct's `entries` field (appending it if absent),
/// preserving the annotation wrapper and every other field (`nav_type`,
/// `nav_container_name`, …).
fn replace_container_entries(container: &IonValue, new_entries: &[IonValue]) -> IonValue {
    match container {
        IonValue::Annotated(anns, inner) => IonValue::Annotated(
            anns.clone(),
            Box::new(replace_container_entries(inner, new_entries)),
        ),
        IonValue::Struct(fields) => {
            let mut out: Vec<(u64, IonValue)> = Vec::with_capacity(fields.len() + 1);
            let mut replaced = false;
            for (k, v) in fields {
                if *k == KfxSymbol::Entries as u64 {
                    out.push((*k, IonValue::List(new_entries.to_vec())));
                    replaced = true;
                } else {
                    out.push((*k, v.clone()));
                }
            }
            if !replaced {
                out.push((
                    KfxSymbol::Entries as u64,
                    IonValue::List(new_entries.to_vec()),
                ));
            }
            IonValue::Struct(out)
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Proposer: derive the chapter list from the book's own in-book Contents page.
// ---------------------------------------------------------------------------

/// Minimum distinct chapter links for a storyline to open a Contents page
/// (below this, a stray forward link or two is just noise). Mirrors the TOC
/// validator's evidence gate, so the proposer only fires on the pages the
/// detector reads as ground truth.
const MIN_CONTENTS_LINKS: usize = 5;

/// Minimum links for a storyline *continuing* an already-found Contents page
/// ([`contents_page_entries`]). Lower than [`MIN_CONTENTS_LINKS`], which has to
/// tell a Contents page from noise across a whole book: here the run is already
/// attested by the storyline before it, and a page's last fragment holding two
/// remaining chapters is ordinary.
const MIN_CONTINUATION_LINKS: usize = 2;

/// Derive a chapter list for [`set_toc`] from the book's own structure: the TOC
/// it already declares, plus whatever its in-book Contents page knows that the
/// declaration doesn't, nested to the depth its labels evidence.
///
/// Every declared entry survives — a proposal is something the user is offered
/// in place of what they have, so it may add chapters and add structure but may
/// never drop an entry the reader can navigate by today. Empty only when the
/// book declares no TOC *and* no page carries enough links to trust.
pub fn propose_toc(kfx_bytes: &[u8]) -> Result<Vec<TocEntry>, KfxError> {
    let book = loader::load(kfx_bytes)?;
    Ok(propose_from_book(&book))
}

/// One-call TOC repair: derive the chapter list ([`propose_toc`]) and write it
/// with [`set_toc`] (overwriting a deficient toc container, or synthesizing one
/// when the book has none). Errors if nothing could be derived, or if the book
/// has no `book_navigation` to attach to.
pub fn repair_toc(kfx_bytes: &[u8]) -> Result<Vec<u8>, KfxError> {
    let book = loader::load(kfx_bytes)?;
    let entries = propose_from_book(&book);
    if entries.is_empty() {
        return Err(KfxError::InvalidKfx(
            "no declared TOC and no in-book Contents page to rebuild one from".into(),
        ));
    }
    // Since the proposal starts from the declared TOC, a book with nothing to
    // add and no structure to restore proposes exactly what it already has.
    // Writing that back is not a repair — say so, rather than report a fix that
    // changed nothing.
    if entries == declared_entries(&book) {
        return Err(KfxError::InvalidKfx(
            "the declared TOC already lists everything the book evidences".into(),
        ));
    }
    set_toc(kfx_bytes, &entries)
}

/// Build the proposal from a loaded container: merge the declared TOC with the
/// in-book Contents page in reading order, then restore the levels the labels
/// evidence.
///
/// The cover and the Contents page itself are deliberately *not* added from the
/// book's landmarks. A reader's chapter list reaches them because the renderer
/// composes the landmarks into its own view (see `bokai::export::build_package`),
/// exactly as a Kindle does — writing them into the toc container as well would
/// list them twice for every book whose publisher already put them there.
fn propose_from_book(book: &BookData) -> Vec<TocEntry> {
    let order = document_order(book);
    let declared = declared_entries(book);
    let contents = contents_page_entries(book, &order);
    let merged = merge_by_document_order(declared, contents, |e| order.at.get(&e.eid).copied());
    nest_by_label_indent(merged)
}

/// Where every element sits in the book, walked from the first reading order's
/// sections and each section's page templates (which follow `story_name` into
/// the storylines they render), so the scale covers exactly what a reader
/// scrolls through.
///
/// The first placement wins for an id reachable from more than one template —
/// portrait and landscape variants render the same storyline.
#[derive(Default)]
struct DocumentOrder {
    /// `eid →` its ordinal across the whole book.
    at: HashMap<i64, usize>,
    /// `eid →` the index, in reading order, of the section that renders it.
    section: HashMap<i64, usize>,
}

fn document_order(book: &BookData) -> DocumentOrder {
    let mut order = DocumentOrder::default();
    let Some(sections) = reading_orders(book).into_iter().next() else {
        return order;
    };
    for (index, name) in sections.iter().enumerate() {
        let Some(section) = lookup_fragment(book, KfxSymbol::Section, name) else {
            continue;
        };
        let Some(templates) = section
            .unwrap_annotated()
            .as_struct()
            .and_then(|f| get_field(f, KfxSymbol::PageTemplates as u64))
            .and_then(|v| v.as_list())
        else {
            continue;
        };
        for template in templates {
            let mut ids = Vec::new();
            collect_element_ids(template, book, &mut ids);
            for id in ids {
                let next = order.at.len();
                order.at.entry(id).or_insert(next);
                order.section.entry(id).or_insert(index);
            }
        }
    }
    order
}

/// The TOC the book declares today, as editable entries — the inverse of what
/// [`set_toc`] writes, so a round trip through the editor is lossless. Nesting
/// is preserved. Empty when the book declares no toc container.
fn declared_entries(book: &BookData) -> Vec<TocEntry> {
    let mut out = Vec::new();
    for_each_nav_container(book, |nav_type, entries| {
        if nav_type != "toc" {
            return;
        }
        out.extend(entries.iter().filter_map(declared_nav_unit));
    });
    out
}

/// One `nav_unit` → [`TocEntry`], recursively. Reads the label through the same
/// rule the reader's chapter list uses, so the declared TOC an editor offers back
/// is entry-for-entry the one the reader sees.
fn declared_nav_unit(entry: &IonValue) -> Option<TocEntry> {
    let fields = entry.unwrap_annotated().as_struct()?;
    let label = nav_unit_label(fields)?;
    let eid = get_field(fields, KfxSymbol::TargetPosition as u64)
        .and_then(|v| v.as_struct())
        .and_then(|pos| get_field(pos, KfxSymbol::Id as u64)?.as_int())
        .unwrap_or(0);
    let children = get_field(fields, KfxSymbol::Entries as u64)
        .and_then(|v| v.as_list())
        .map(|list| list.iter().filter_map(declared_nav_unit).collect())
        .unwrap_or_default();
    Some(TocEntry {
        label,
        eid,
        children,
    })
}

/// The chapter list on the book's own in-book Contents page.
///
/// A Contents page is a *run* of storylines, not one storyline: a long list is
/// split across several, and taking only the densest returns whichever fragment
/// happens to be biggest — the middle of the list, missing both ends. So the
/// densest qualifying storyline seeds the run (the one a `toc` landmark marks,
/// when present) and the run grows through its neighbours in reading order for
/// as long as they keep looking like a Contents page.
///
/// "Looking like a Contents page" is link *density* plus nearness in the book,
/// not link count. Nearly every line of a Contents page is a link, while a
/// chapter with footnotes has a handful among its prose; and the fragments of
/// one page share a section, while another work's own Contents page — which
/// looks exactly as link-dense — sits many sections away. Together they keep the
/// run from swallowing either the chapter that follows the page or the rest of
/// an anthology's per-work Contents pages, which are a level of structure this
/// has no way to place.
///
/// Entries come back in document order, deduped by target.
fn contents_page_entries(book: &BookData, order: &DocumentOrder) -> Vec<TocEntry> {
    let anchors = extract_anchors(book);
    let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) else {
        return Vec::new();
    };
    let landmark_eid = toc_landmark_eid(book);

    // Every storyline that carries links at all, in reading order.
    let mut pages: Vec<ContentsPage> = Vec::new();
    for storyline in storylines.values() {
        let mut page = ContentsPage::read(storyline, book, &anchors);
        if page.links.is_empty() {
            continue;
        }
        // Where the storyline *is*, not where it points: a Contents page links
        // forward into the chapters, so its targets' positions would sort it
        // among them and make every page in the book look adjacent to the
        // Contents page.
        let mut ids = HashSet::new();
        collect_ids(storyline, &mut ids);
        page.at = ids
            .iter()
            .filter_map(|eid| order.at.get(eid))
            .min()
            .copied();
        page.section = ids
            .iter()
            .filter_map(|eid| order.section.get(eid))
            .min()
            .copied();
        page.holds_landmark = landmark_eid.is_some_and(|le| ids.contains(&le));
        pages.push(page);
    }
    // A storyline the reading order never reaches sorts last; it can still join
    // a run, but it can't claim to start one.
    pages.sort_by_key(|p| p.at.unwrap_or(usize::MAX));

    let Some(seed) = seed_page(&pages) else {
        return Vec::new();
    };
    // Grow outwards while the neighbours still read as part of the same page.
    // Only link-carrying storylines are in `pages`, so adjacency here is not
    // adjacency in the book — each step is checked against the page it extends.
    let mut first = seed;
    while first > 0 && pages[first - 1].continues(&pages[first]) {
        first -= 1;
    }
    let mut last = seed;
    while last + 1 < pages.len() && pages[last + 1].continues(&pages[last]) {
        last += 1;
    }

    let links = pages[first..=last]
        .iter()
        .flat_map(|p| p.links.iter().cloned())
        .collect();
    dedup_entries(links)
}

/// Which storyline opens the Contents page: the one the `toc` landmark marks,
/// else the one with the most chapter links. `None` when none qualifies.
fn seed_page(pages: &[ContentsPage]) -> Option<usize> {
    let qualifying = |p: &ContentsPage| p.links.len() >= MIN_CONTENTS_LINKS && p.is_link_dense();
    if let Some(i) = pages.iter().position(|p| p.holds_landmark && qualifying(p)) {
        return Some(i);
    }
    pages
        .iter()
        .enumerate()
        .filter(|(_, p)| qualifying(p))
        .max_by_key(|(_, p)| p.links.len())
        .map(|(i, _)| i)
}

/// One storyline weighed as a candidate Contents page.
#[derive(Default)]
struct ContentsPage {
    /// `(label, target eid)` for every internal link it makes, in reading order.
    links: Vec<(String, i64)>,
    /// Elements carrying at least one internal link.
    linked: usize,
    /// Elements carrying any text at all.
    texty: usize,
    /// Where the storyline itself starts, as an ordinal in [`DocumentOrder`].
    /// `None` when the reading order never reaches it.
    at: Option<usize>,
    /// The reading-order index of the section it starts in.
    section: Option<usize>,
    /// The book's `toc` landmark points into this storyline.
    holds_landmark: bool,
}

impl ContentsPage {
    fn read(storyline: &IonValue, book: &BookData, anchors: &AnchorTable) -> Self {
        let mut page = ContentsPage::default();
        collect_chapter_links(storyline, book, anchors, &mut page);
        page
    }

    /// Most of what this storyline says is a link — a Contents page, not a
    /// chapter that happens to cross-reference a few others.
    fn is_link_dense(&self) -> bool {
        self.linked * 2 >= self.texty
    }

    /// Enough of a Contents page to continue `page`, and near enough in the book
    /// to be the same page. The fragments of one page share a section or spill
    /// into the next; a per-work Contents page deeper in an anthology is just as
    /// link-dense but sections away.
    fn continues(&self, page: &ContentsPage) -> bool {
        self.links.len() >= MIN_CONTINUATION_LINKS
            && self.is_link_dense()
            && matches!(
                (self.section, page.section),
                (Some(a), Some(b)) if a.abs_diff(b) <= 1
            )
    }
}

/// Walk a storyline tree in document order, recording `(display_text,
/// target_eid)` for every internal `link_to` — both element-level links (`$179`
/// on the element) and inline style-event runs (`$142` events each carrying
/// `$179`) — and, alongside them, how many elements carry text and how many
/// carry a link, which is what tells a Contents page from a chapter.
///
/// A run's text is the char-slice `[offset, offset+length)` of the element's
/// `$145` text; an element-level link uses the element's full text. Links whose
/// anchor resolves to no internal position (external URIs) are skipped.
fn collect_chapter_links(
    value: &IonValue,
    book: &BookData,
    anchors: &AnchorTable,
    out: &mut ContentsPage,
) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            let direct_text = get_field(fields, KfxSymbol::Content as u64)
                .map(|c| resolve_content_text(c, book))
                .unwrap_or_default();
            if !direct_text.trim().is_empty() {
                out.texty += 1;
            }
            let before = out.links.len();

            // Inline style-event links: a run of the element's text is the link.
            let mut handled_inline = false;
            if let Some(events) =
                get_field(fields, KfxSymbol::StyleEvents as u64).and_then(|v| v.as_list())
            {
                let chars: Vec<char> = direct_text.chars().collect();
                for ev in events {
                    let Some(ef) = ev.unwrap_annotated().as_struct() else {
                        continue;
                    };
                    let Some(name) = get_field(ef, KfxSymbol::LinkTo as u64)
                        .and_then(|v| book.symbols.text_of(v))
                    else {
                        continue;
                    };
                    handled_inline = true;
                    let off = get_field(ef, KfxSymbol::Offset as u64)
                        .and_then(|v| v.as_int())
                        .unwrap_or(0)
                        .max(0) as usize;
                    let len = get_field(ef, KfxSymbol::Length as u64)
                        .and_then(|v| v.as_int())
                        .unwrap_or(0)
                        .max(0) as usize;
                    let end = off.saturating_add(len).min(chars.len());
                    let label: String = if off < end {
                        chars[off..end].iter().collect()
                    } else {
                        String::new()
                    };
                    push_link(&mut out.links, anchors, name, &label);
                }
            }

            // Element-level link: the whole element is the link (no inline runs).
            if !handled_inline
                && let Some(name) = get_field(fields, KfxSymbol::LinkTo as u64)
                    .and_then(|v| book.symbols.text_of(v))
            {
                let label = element_text(fields, book);
                push_link(&mut out.links, anchors, name, &label);
            }

            if out.links.len() > before {
                out.linked += 1;
            }

            // Recurse into children; skip the style-events list (handled above —
            // its `link_to`s are runs, not sub-elements, and carry no own text).
            for (k, v) in fields {
                if *k == KfxSymbol::StyleEvents as u64 {
                    continue;
                }
                collect_chapter_links(v, book, anchors, out);
            }
        }
        IonValue::List(items) => {
            for it in items {
                collect_chapter_links(it, book, anchors, out);
            }
        }
        _ => {}
    }
}

/// The full display text of a content element: its direct `$145` text, or the
/// concatenation of its `$146` content-list children's text (what the emitter
/// would render inside the link wrapper).
fn element_text(fields: &[(u64, IonValue)], book: &BookData) -> String {
    if let Some(c) = get_field(fields, KfxSymbol::Content as u64) {
        let t = resolve_content_text(c, book);
        if !t.is_empty() {
            return t;
        }
    }
    if let Some(list) = get_field(fields, KfxSymbol::ContentList as u64).and_then(|v| v.as_list()) {
        let mut s = String::new();
        for child in list {
            if let Some(cf) = child.unwrap_annotated().as_struct() {
                s.push_str(&element_text(cf, book));
            }
        }
        return s;
    }
    String::new()
}

/// Resolve one link's anchor to a target eid and, if its label is non-empty,
/// record `(label, eid)`. External-URI anchors (absent from the position index)
/// are dropped — they're not chapters.
fn push_link(out: &mut Vec<(String, i64)>, anchors: &AnchorTable, name: &str, raw_label: &str) {
    let label = clean_label(raw_label);
    if label.is_empty() {
        return;
    }
    if let Some(&(eid, _offset)) = anchors.name_to_position.get(name) {
        out.push((label, eid));
    }
}

/// Collapse a raw link text into a nav label: hard line breaks and runs of ASCII
/// spaces become a single space (full-width spacing in JP titles is preserved),
/// then trim.
fn clean_label(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len());
    let mut prev_space = false;
    for c in raw.chars() {
        if c == ' ' || c == '\n' || c == '\r' || c == '\t' {
            if !prev_space {
                s.push(' ');
                prev_space = true;
            }
        } else {
            s.push(c);
            prev_space = false;
        }
    }
    // Trim the same ASCII set the collapse above uses. `str::trim` would drop a
    // leading or trailing U+3000, contradicting the preservation this function
    // exists to provide.
    crate::util::trim_markup_space(&s).to_string()
}

/// Collapse `(label, eid)` pairs to one [`TocEntry`] per distinct target eid,
/// keeping document order and the first label seen for each eid. `set_toc` writes
/// every target at offset 0, so two links to different offsets of one element are
/// indistinguishable in the result and fold into a single entry.
fn dedup_entries(links: Vec<(String, i64)>) -> Vec<TocEntry> {
    let mut seen: HashSet<i64> = HashSet::new();
    let mut out = Vec::new();
    for (label, eid) in links {
        if seen.insert(eid) {
            out.push(TocEntry::new(label, eid));
        }
    }
    out
}

/// The eid a `toc`-type landmark targets, if the book has one — used to prefer
/// the true Contents storyline over a merely link-dense one.
fn toc_landmark_eid(book: &BookData) -> Option<i64> {
    navigation::landmarks(book)
        .into_iter()
        .find(|l| l.landmark_type == LandmarkType::Toc)
        .and_then(|l| l.eid)
}

/// Every `$155 id` in a value tree — used to find which storyline a landmark eid
/// falls in.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::kfx::navigation::extract_toc;
    use std::collections::HashMap;

    const FIXTURE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";

    /// A real target eid from the fixture's existing toc — any valid nav target,
    /// so the write test points at a position the container already trusts.
    fn a_real_toc_eid(book: &BookData) -> Option<i64> {
        let nav = book.by_type.get(&(KfxSymbol::BookNavigation as u64))?;
        for value in nav.values() {
            for ro in nav_reading_orders(value) {
                let containers = ro
                    .as_struct()
                    .and_then(|f| get_field(f, KfxSymbol::NavContainers as u64))
                    .and_then(|v| v.as_list())?;
                for c in containers {
                    let Some(resolved) = resolve_nav_container(book, c) else {
                        continue;
                    };
                    if nav_type_of(&resolved, book) != Some("toc") {
                        continue;
                    }
                    let entries = resolved
                        .as_struct()
                        .and_then(|f| get_field(f, KfxSymbol::Entries as u64))
                        .and_then(|v| v.as_list())?;
                    for e in entries {
                        if let Some((eid, _)) = target_of(e) {
                            return Some(eid);
                        }
                    }
                }
            }
        }
        None
    }

    /// The `(eid, offset)` a nav_unit targets (test helper).
    fn target_of(unit: &IonValue) -> Option<(i64, i64)> {
        let fields = unit.unwrap_annotated().as_struct()?;
        let pos = get_field(fields, KfxSymbol::TargetPosition as u64)?.as_struct()?;
        let id = get_field(pos, KfxSymbol::Id as u64)?.as_int()?;
        let offset = get_field(pos, KfxSymbol::Offset as u64)
            .and_then(|v| v.as_int())
            .unwrap_or(0);
        Some((id, offset))
    }

    /// End-to-end: overwrite the fixture's toc with a real chapter list, then
    /// prove the rewritten container re-loads, the reader's toc extractor sees
    /// the new entries, the TOC validator now passes, and it still converts to
    /// EPUB.
    #[cfg(feature = "validate")]
    #[test]
    fn set_toc_rewrites_and_validates_ok() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let book = loader::load(&kfx).expect("load fixture");
        let eid = a_real_toc_eid(&book).expect("fixture has a toc with a target");

        let entries = vec![
            TocEntry::new("第一章", eid),
            TocEntry::new("第二章", eid),
            TocEntry::new("第三章", eid),
        ];
        let out = set_toc(&kfx, &entries).expect("set_toc");

        // Re-loads, and the reader's toc extractor sees exactly our labels.
        let after = loader::load(&out).expect("rewritten container must re-load");
        let toc = extract_toc(&after, &HashMap::new(), &AnchorTable::default());
        let labels: Vec<&str> = toc.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(labels, ["第一章", "第二章", "第三章"]);

        // The TOC validator now passes (3 real chapter entries > the gate).
        let audit = crate::validate::source::toc::validate(&out).expect("validate");
        assert_eq!(audit.verdict, crate::validate::source::toc::Verdict::Ok);

        assert!(
            crate::formats::kfx::converts_to_epub(&out),
            "repaired KFX must still convert to EPUB"
        );
    }

    /// Nested entries round-trip through the reader as parent → children.
    #[test]
    fn set_toc_preserves_nesting() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let book = loader::load(&kfx).expect("load fixture");
        let eid = a_real_toc_eid(&book).expect("fixture has a toc target");

        let entries = vec![TocEntry {
            label: "第一部".into(),
            eid,
            children: vec![TocEntry::new("第一章", eid), TocEntry::new("第二章", eid)],
        }];
        let out = set_toc(&kfx, &entries).expect("set_toc");
        let after = loader::load(&out).expect("re-load");
        let toc = extract_toc(&after, &HashMap::new(), &AnchorTable::default());

        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].title, "第一部");
        let kids: Vec<&str> = toc[0].children.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(kids, ["第一章", "第二章"]);
    }

    #[test]
    fn empty_toc_is_rejected() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        assert!(set_toc(&kfx, &[]).is_err());
    }

    #[test]
    fn non_kfx_bytes_error() {
        assert!(set_toc(b"not a kfx", &[TocEntry::new("X", 1)]).is_err());
    }

    #[test]
    fn clean_label_collapses_ascii_ws_keeps_fullwidth() {
        assert_eq!(clean_label("  Chapter\n One \t"), "Chapter One");
        // Full-width space (U+3000) is real spacing in a JP title — kept as-is.
        assert_eq!(clean_label("第一章　タイトル"), "第一章　タイトル");
        assert_eq!(clean_label("   \n\t "), "");
    }

    #[test]
    fn dedup_entries_keeps_first_label_per_eid_in_order() {
        let links = vec![
            ("One".to_string(), 10),
            ("Two".to_string(), 20),
            ("Two again".to_string(), 20), // same target eid → dropped
            ("Three".to_string(), 30),
        ];
        let got: Vec<(String, i64)> = dedup_entries(links)
            .into_iter()
            .map(|e| (e.label, e.eid))
            .collect();
        assert_eq!(
            got,
            [
                ("One".to_string(), 10),
                ("Two".to_string(), 20),
                ("Three".to_string(), 30)
            ]
        );
    }

    /// Whatever the proposer finds on the fixture, the contract every caller
    /// relies on holds: non-empty trimmed labels, unique target eids.
    #[test]
    fn propose_is_wellformed_on_fixture() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let proposed = propose_toc(&kfx).expect("propose");
        let mut seen = HashSet::new();
        for e in &proposed {
            assert!(!e.label.is_empty(), "empty label proposed");
            // ASCII-only, matching `clean_label`: a leading U+3000 is content a
            // JP publisher typed, not markup padding, and must survive.
            assert_eq!(
                e.label,
                crate::util::trim_markup_space(&e.label),
                "untrimmed label: {:?}",
                e.label
            );
            assert!(seen.insert(e.eid), "duplicate target eid {}", e.eid);
        }
    }

    /// End-to-end on a book that genuinely needs the repair: strip the declared
    /// TOC, and `repair_toc` rebuilds it from the in-book Contents page. The
    /// result re-loads, is no longer SUSPECT, and still converts to EPUB. A
    /// no-op if this fixture ships no Contents page to rebuild from.
    #[cfg(feature = "validate")]
    #[test]
    fn repair_rebuilds_a_stripped_toc_from_the_contents_page() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let book = loader::load(&kfx).expect("load");
        let stripped = strip_toc_containers(&kfx, &book).expect("strip");
        if propose_toc(&stripped).expect("propose").is_empty() {
            return; // no in-book Contents page to rebuild from
        }
        let out = repair_toc(&stripped).expect("repair");
        loader::load(&out).expect("repaired container must re-load");
        let audit = crate::validate::source::toc::validate(&out).expect("validate");
        assert_ne!(
            audit.verdict,
            crate::validate::source::toc::Verdict::Suspect,
            "repaired TOC must not remain SUSPECT"
        );
        assert!(
            crate::formats::kfx::converts_to_epub(&out),
            "repaired KFX must still convert to EPUB"
        );
    }

    /// A repair that would write back exactly what the book already declares is
    /// refused, so no caller reports a fix that changed nothing.
    #[test]
    fn repair_refuses_when_the_declared_toc_already_says_everything() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let book = loader::load(&kfx).expect("load");
        if propose_from_book(&book) != declared_entries(&book) {
            return; // this fixture has something to add; nothing to assert here
        }
        let err = repair_toc(&kfx).expect_err("a no-op repair must be refused");
        assert!(
            err.to_string().contains("already lists"),
            "unexpected error: {err}"
        );
    }

    /// The invariant every caller leans on: whatever else the proposal does, it
    /// never drops an entry the book already declares. A proposal is offered in
    /// place of what the reader has today, so losing one would make the "repair"
    /// a regression.
    #[test]
    fn a_proposal_never_loses_a_declared_entry() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let book = loader::load(&kfx).expect("load");

        let mut proposed = HashSet::new();
        collect_targets(&propose_from_book(&book), &mut proposed);
        for entry in flatten(&declared_entries(&book)) {
            assert!(
                proposed.contains(&entry.eid),
                "the proposal dropped declared entry {:?} (eid {})",
                entry.label,
                entry.eid
            );
        }
    }

    fn collect_targets(entries: &[TocEntry], out: &mut HashSet<i64>) {
        for e in entries {
            out.insert(e.eid);
            collect_targets(&e.children, out);
        }
    }

    fn flatten(entries: &[TocEntry]) -> Vec<TocEntry> {
        entries
            .iter()
            .flat_map(|e| std::iter::once(e.clone()).chain(flatten(&e.children)))
            .collect()
    }

    /// The synthesize path's core: given a `book_navigation` holding only a
    /// landmarks container, `add_toc_container` prepends an inline toc container
    /// (the system `toc` symbol for both `nav_type` and `nav_container_name`) and
    /// leaves the landmarks one in place.
    #[test]
    fn add_toc_container_prepends_inline_toc() {
        let landmarks = IonValue::Annotated(
            vec![KfxSymbol::NavContainer as u64],
            Box::new(IonValue::Struct(vec![(
                KfxSymbol::NavType as u64,
                IonValue::Symbol(KfxSymbol::Landmarks as u64),
            )])),
        );
        let ro = IonValue::Struct(vec![(
            KfxSymbol::NavContainers as u64,
            IonValue::List(vec![landmarks]),
        )]);
        let nav = IonValue::List(vec![ro]);

        let entries = vec![build_nav_unit(&TocEntry::new("第一章", 42))];
        let out = add_toc_container(&nav, &entries);

        let ros = out.as_list().expect("list of reading orders");
        let containers = ros[0]
            .as_struct()
            .and_then(|f| get_field(f, KfxSymbol::NavContainers as u64))
            .and_then(|v| v.as_list())
            .expect("nav_containers");
        assert_eq!(containers.len(), 2, "toc prepended, landmarks kept");

        let toc = containers[0]
            .unwrap_annotated()
            .as_struct()
            .expect("toc struct");
        assert_eq!(
            get_field(toc, KfxSymbol::NavType as u64).and_then(|v| v.as_symbol()),
            Some(KfxSymbol::Toc as u64)
        );
        assert_eq!(
            get_field(toc, KfxSymbol::NavContainerName as u64).and_then(|v| v.as_symbol()),
            Some(KfxSymbol::Toc as u64),
            "name reuses the system toc symbol — no doc-symbol growth"
        );
        assert_eq!(
            get_field(toc, KfxSymbol::Entries as u64)
                .and_then(|v| v.as_list())
                .map(<[_]>::len),
            Some(1)
        );
        // The landmarks container is preserved as the second entry.
        let kept = containers[1]
            .unwrap_annotated()
            .as_struct()
            .expect("landmarks struct");
        assert_eq!(
            get_field(kept, KfxSymbol::NavType as u64).and_then(|v| v.as_symbol()),
            Some(KfxSymbol::Landmarks as u64),
            "landmarks container untouched"
        );
    }

    /// End-to-end synthesize: strip the fixture's toc so it declares none, then
    /// `set_toc` must add a fresh inline container. The result re-loads, the
    /// reader sees the new chapters, and it still converts to EPUB.
    #[test]
    fn synthesize_toc_when_book_has_none() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let book = loader::load(&kfx).expect("load");
        let eid = a_real_toc_eid(&book).expect("fixture toc target");

        let stripped = strip_toc_containers(&kfx, &book).expect("strip");
        let sbook = loader::load(&stripped).expect("stripped re-loads");
        assert!(
            detect_toc_mode(&sbook).is_none(),
            "stripped KFX must declare no toc container"
        );

        let entries = vec![TocEntry::new("第一章", eid), TocEntry::new("第二章", eid)];
        let out = set_toc(&stripped, &entries).expect("synthesize");

        let after = loader::load(&out).expect("synthesized container must re-load");
        let toc = extract_toc(&after, &HashMap::new(), &AnchorTable::default());
        let labels: Vec<&str> = toc.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(labels, ["第一章", "第二章"]);
        assert!(
            crate::formats::kfx::converts_to_epub(&out),
            "synthesized KFX must still convert to EPUB"
        );
    }

    /// Test helper: drop every inline toc `nav_container` from a KFX's
    /// `book_navigation`, yielding a book with no declared toc (to drive the
    /// synthesize path against real bytes).
    fn strip_toc_containers(kfx: &[u8], book: &BookData) -> Result<Vec<u8>, KfxError> {
        edit_container(kfx, |e| {
            if e.is_type(KfxSymbol::BookNavigation) {
                Ok(EntityEdit::Ion(drop_toc(book, &e.parse_ion()?)))
            } else {
                Ok(EntityEdit::Keep)
            }
        })
    }

    fn drop_toc(book: &BookData, nav: &IonValue) -> IonValue {
        match nav {
            IonValue::Annotated(anns, inner) => {
                IonValue::Annotated(anns.clone(), Box::new(drop_toc(book, inner)))
            }
            IonValue::List(items) => {
                IonValue::List(items.iter().map(|it| drop_toc(book, it)).collect())
            }
            IonValue::Struct(fields) => IonValue::Struct(
                fields
                    .iter()
                    .map(|(k, v)| {
                        if *k == KfxSymbol::NavContainers as u64
                            && let Some(list) = v.as_list()
                        {
                            let kept: Vec<IonValue> = list
                                .iter()
                                .filter(|c| {
                                    c.unwrap_annotated()
                                        .as_struct()
                                        .and_then(|f| get_field(f, KfxSymbol::NavType as u64))
                                        .and_then(|nt| book.symbols.text_of(nt))
                                        != Some("toc")
                                })
                                .cloned()
                                .collect();
                            (*k, IonValue::List(kept))
                        } else {
                            (*k, v.clone())
                        }
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}
