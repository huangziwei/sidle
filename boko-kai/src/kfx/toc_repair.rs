//! Surgical TOC repair for a KFX container: derive a chapter list from the
//! book's own in-book Contents page and overwrite a deficient/front-matter
//! `nav_container` toc with it, in place.
//!
//! [`propose_toc`] reads the chapter list off the book's Contents page (the same
//! page [`boko validate toc`](crate::validate::source::toc) reads as ground
//! truth); [`set_toc`] writes a caller-supplied list into the toc container;
//! [`repair_toc`] is the two composed. The write rebuilds the toc
//! `nav_container`'s `entries` via the container edit harness
//! ([`edit_container`]): the toc container keeps its `nav_type` and
//! `nav_container_name` symbol and every other fragment passes through verbatim,
//! so **no doc-symbol growth is needed** — the one primitive the harness lacks.
//!
//! Because the KFX *is* the reader/device file, this fixes the Sidle reader
//! sidebar directly; re-ingest then re-derives the EPUB nav from the corrected
//! source. One source edit, both surfaces.
//!
//! ⚠️ Device strictness: the offline reader is more permissive than a Kindle on
//! nav. A repaired container must still clear the offline entity differ and a
//! device round-trip before it's trusted on-device.
//! The nav-unit shape here mirrors boko's proven `pdf_to_kfx` toc export exactly
//! (`representation:{label}` + `target_position:{id, offset:0}`).
//!
//! v1 scope: the book must already carry a `book_navigation` fragment (every
//! reflowable KFX does). A deficient toc container is overwritten in place; when
//! none exists, a fresh inline toc container is added, reusing the system `toc`
//! symbol ($212) for its name — so no doc-symbol growth is needed. A book with no
//! `book_navigation` at all, and the fixed-layout referenced-container shape, are
//! not yet handled.

use std::collections::HashSet;

use crate::kfx::container::get_field;
use crate::kfx::container_edit::{EntityEdit, edit_container};
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;
use crate::kfx_to_epub::ConvertError;
use crate::kfx_to_epub::content::resolve_content_text;
use crate::kfx_to_epub::loader::{self, BookData};
use crate::kfx_to_epub::navigation::{AnchorTable, extract_anchors, resolve_nav_container};

/// One chapter in an edited TOC. `eid` is the target element's `$155 id`; the
/// target offset is always written as 0 — the Kindle TOC convention (a jump
/// lands on the chapter-start element, and a non-zero offset can wedge the
/// firmware). Nesting is supported for sub-chapters.
#[derive(Debug, Clone)]
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

/// Overwrite the KFX's toc `nav_container` entries with `entries` — or, when the
/// book declares no toc container, synthesize a fresh inline one in its
/// `book_navigation`. In place.
///
/// Errors (via [`ConvertError::InvalidKfx`]) if `entries` is empty, if the bytes
/// aren't a KFX container, or if the book has no `book_navigation` at all to
/// attach a toc to (creating one from scratch is not yet supported).
pub fn set_toc(kfx_bytes: &[u8], entries: &[TocEntry]) -> Result<Vec<u8>, ConvertError> {
    if entries.is_empty() {
        return Err(ConvertError::InvalidKfx(
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
) -> Result<Vec<u8>, ConvertError> {
    let has_nav = book
        .by_type
        .get(&(KfxSymbol::BookNavigation as u64))
        .is_some_and(|m| !m.is_empty());
    if !has_nav {
        return Err(ConvertError::InvalidKfx(
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
        for ro in reading_orders(value) {
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
fn reading_orders(value: &IonValue) -> Vec<IonValue> {
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
/// target_position:{id, offset:0}}` plus nested `entries` — matching boko's
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

/// Minimum distinct chapter links for a storyline to count as a real Contents
/// page (below this, a stray forward link or two is just noise). Mirrors the TOC
/// validator's evidence gate, so the proposer only fires on the pages the
/// detector reads as ground truth.
const MIN_CONTENTS_LINKS: usize = 5;

/// Derive a chapter list for [`set_toc`] from the book's own in-book Contents
/// page — the densest cluster of internal chapter links (the page a `toc`
/// landmark marks, when present). Each `link_to` run's display text becomes a
/// [`TocEntry`] label and its resolved anchor's element id the target eid;
/// entries come back in document order, deduped by target. Empty when no page
/// carries enough links to trust (the caller then has nothing to write).
pub fn propose_toc(kfx_bytes: &[u8]) -> Result<Vec<TocEntry>, ConvertError> {
    let book = loader::load(kfx_bytes)?;
    Ok(propose_from_book(&book))
}

/// One-call TOC repair: derive the chapter list from the in-book Contents page
/// ([`propose_toc`]) and write it with [`set_toc`] (overwriting a deficient toc
/// container, or synthesizing one when the book has none). Errors if no usable
/// Contents page is found, or if the book has no `book_navigation` to attach to.
pub fn repair_toc(kfx_bytes: &[u8]) -> Result<Vec<u8>, ConvertError> {
    let book = loader::load(kfx_bytes)?;
    let entries = propose_from_book(&book);
    if entries.is_empty() {
        return Err(ConvertError::InvalidKfx(
            "no in-book Contents page found to rebuild the TOC from".into(),
        ));
    }
    set_toc(kfx_bytes, &entries)
}

/// Pick the book's Contents storyline and return its chapter list. Chooses the
/// storyline with the most distinct chapter-link targets, preferring the one a
/// `toc` landmark points into (guards against a footnote-dense chapter that
/// out-links the real Contents page). Returns empty when even the best page has
/// fewer than [`MIN_CONTENTS_LINKS`] links.
fn propose_from_book(book: &BookData) -> Vec<TocEntry> {
    let anchors = extract_anchors(book);
    let Some(storylines) = book.by_type.get(&(KfxSymbol::Storyline as u64)) else {
        return Vec::new();
    };
    let landmark_eid = toc_landmark_eid(book);

    let mut best: Vec<TocEntry> = Vec::new();
    let mut landmark_hit: Option<Vec<TocEntry>> = None;

    for storyline in storylines.values() {
        let mut links: Vec<(String, i64)> = Vec::new();
        collect_chapter_links(storyline, book, &anchors, &mut links);
        let entries = dedup_entries(links);
        if entries.len() > best.len() {
            best = entries.clone();
        }
        // Prefer the storyline the toc landmark points into (only one storyline
        // holds a given eid, so this fixes on the true Contents page even if a
        // link-dense chapter has more raw links).
        if landmark_hit.is_none()
            && let Some(le) = landmark_eid
            && entries.len() >= MIN_CONTENTS_LINKS
        {
            let mut ids = HashSet::new();
            collect_ids(storyline, &mut ids);
            if ids.contains(&le) {
                landmark_hit = Some(entries);
            }
        }
    }

    let chosen = landmark_hit.unwrap_or(best);
    if chosen.len() < MIN_CONTENTS_LINKS {
        return Vec::new();
    }
    chosen
}

/// Walk a storyline tree in document order, emitting `(display_text, target_eid)`
/// for every internal `link_to`: both element-level links (`$179` on the element)
/// and inline style-event runs (`$142` events each carrying `$179`). A run's text
/// is the char-slice `[offset, offset+length)` of the element's `$145` text; an
/// element-level link uses the element's full text. Links whose anchor resolves
/// to no internal position (external URIs) are skipped.
fn collect_chapter_links(
    value: &IonValue,
    book: &BookData,
    anchors: &AnchorTable,
    out: &mut Vec<(String, i64)>,
) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            let direct_text = get_field(fields, KfxSymbol::Content as u64)
                .map(|c| resolve_content_text(c, book))
                .unwrap_or_default();

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
                    push_link(out, anchors, name, &label);
                }
            }

            // Element-level link: the whole element is the link (no inline runs).
            if !handled_inline
                && let Some(name) = get_field(fields, KfxSymbol::LinkTo as u64)
                    .and_then(|v| book.symbols.text_of(v))
            {
                let label = element_text(fields, book);
                push_link(out, anchors, name, &label);
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
    s.trim().to_string()
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
    let nav = book.by_type.get(&(KfxSymbol::BookNavigation as u64))?;
    for value in nav.values() {
        for ro in reading_orders(value) {
            let Some(containers) = ro
                .as_struct()
                .and_then(|f| get_field(f, KfxSymbol::NavContainers as u64))
                .and_then(|v| v.as_list())
            else {
                continue;
            };
            for container in containers {
                let Some(resolved) = resolve_nav_container(book, container) else {
                    continue;
                };
                if nav_type_of(&resolved, book) != Some("landmarks") {
                    continue;
                }
                let Some(entries) = resolved
                    .as_struct()
                    .and_then(|f| get_field(f, KfxSymbol::Entries as u64))
                    .and_then(|v| v.as_list())
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
    use crate::kfx_to_epub::navigation::extract_toc;
    use std::collections::HashMap;

    const FIXTURE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";

    /// A real target eid from the fixture's existing toc — any valid nav target,
    /// so the write test points at a position the container already trusts.
    fn a_real_toc_eid(book: &BookData) -> Option<i64> {
        let nav = book.by_type.get(&(KfxSymbol::BookNavigation as u64))?;
        for value in nav.values() {
            for ro in reading_orders(value) {
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
        let labels: Vec<&str> = toc.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["第一章", "第二章", "第三章"]);

        // The TOC validator now passes (3 real chapter entries > the gate).
        let audit = crate::validate::source::toc::validate(&out).expect("validate");
        assert_eq!(audit.verdict, crate::validate::source::toc::Verdict::Ok);

        assert!(
            crate::kfx_to_epub::convert_to_epub(&out).is_ok(),
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
        assert_eq!(toc[0].label, "第一部");
        let kids: Vec<&str> = toc[0].children.iter().map(|p| p.label.as_str()).collect();
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
            assert_eq!(e.label, e.label.trim(), "untrimmed label: {:?}", e.label);
            assert!(seen.insert(e.eid), "duplicate target eid {}", e.eid);
        }
    }

    /// End-to-end when the fixture carries an in-book Contents page: `repair_toc`
    /// derives it, and the rewritten container is no longer SUSPECT and still
    /// converts to EPUB. A no-op if this fixture ships no Contents page.
    #[test]
    fn repair_roundtrips_when_contents_page_present() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        if propose_toc(&kfx).expect("propose").is_empty() {
            return; // no in-book Contents page to rebuild from
        }
        let out = repair_toc(&kfx).expect("repair");
        loader::load(&out).expect("repaired container must re-load");
        let audit = crate::validate::source::toc::validate(&out).expect("validate");
        assert_ne!(
            audit.verdict,
            crate::validate::source::toc::Verdict::Suspect,
            "repaired TOC must not remain SUSPECT"
        );
        assert!(
            crate::kfx_to_epub::convert_to_epub(&out).is_ok(),
            "repaired KFX must still convert to EPUB"
        );
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
        let labels: Vec<&str> = toc.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["第一章", "第二章"]);
        assert!(
            crate::kfx_to_epub::convert_to_epub(&out).is_ok(),
            "synthesized KFX must still convert to EPUB"
        );
    }

    /// Test helper: drop every inline toc `nav_container` from a KFX's
    /// `book_navigation`, yielding a book with no declared toc (to drive the
    /// synthesize path against real bytes).
    fn strip_toc_containers(kfx: &[u8], book: &BookData) -> Result<Vec<u8>, ConvertError> {
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
