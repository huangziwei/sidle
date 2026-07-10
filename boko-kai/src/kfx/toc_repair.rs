//! Surgical TOC repair for a KFX container: overwrite a deficient/front-matter
//! `nav_container` toc with a real chapter list, in place.
//!
//! This is the *repair* half of the KFX side of "add/repair a TOC" — the write
//! that the [`boko validate toc`](crate::validate::source::toc) detector's
//! SUSPECT verdict feeds. It rebuilds the toc `nav_container`'s `entries` from a
//! caller-supplied chapter list via the container edit harness
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
//! v1 scope: the book must already carry a toc `nav_container` — the shape every
//! SUSPECT book with a front-matter-only or chapterless TOC has. Synthesizing a
//! container from scratch (which *does* need a new name doc-symbol) is deferred.

use crate::kfx::container::get_field;
use crate::kfx::container_edit::{EntityEdit, edit_container};
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;
use crate::kfx_to_epub::ConvertError;
use crate::kfx_to_epub::loader::{self, BookData};
use crate::kfx_to_epub::navigation::resolve_nav_container;

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

/// Overwrite the KFX's toc `nav_container` entries with `entries`, in place.
///
/// Errors (via [`ConvertError::InvalidKfx`]) if `entries` is empty, if the bytes
/// aren't a KFX container, or if the book carries no toc `nav_container` to
/// replace.
pub fn set_toc(kfx_bytes: &[u8], entries: &[TocEntry]) -> Result<Vec<u8>, ConvertError> {
    if entries.is_empty() {
        return Err(ConvertError::InvalidKfx(
            "refusing to write an empty TOC".into(),
        ));
    }
    let book = loader::load(kfx_bytes)?;
    let mode = detect_toc_mode(&book).ok_or_else(|| {
        ConvertError::InvalidKfx(
            "KFX has no toc nav_container to replace (synthesizing one is not yet supported)"
                .into(),
        )
    })?;

    let new_entries: Vec<IonValue> = entries.iter().map(build_nav_unit).collect();

    edit_container(kfx_bytes, |e| {
        match &mode {
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
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kfx_to_epub::navigation::{AnchorTable, extract_toc};
    use std::collections::HashMap;

    const FIXTURE: &str = "tests/fixtures/[太宰 治] 人間失格.kfx";

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
}
