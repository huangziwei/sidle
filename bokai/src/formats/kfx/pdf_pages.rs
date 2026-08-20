//! The page-level view of a PDF-backed KFX: per-page text geometry, the
//! anchors registered on each page, the outline, and the page labels.
//!
//! A PDF-backed KFX carries the source PDF verbatim (see
//! [`pdf_container`](super::pdf_container)) and renders each page from it, with
//! a `type:text` storyline pinned over the page at fixed `top`/`left`/`width`/
//! `height`. That storyline is what makes the page's words addressable —
//! selectable, searchable, and anchorable by eid — without the glyphs
//! themselves ever leaving the PDF. This module reads it.
//!
//! [`page_text_layer`] follows the same chain a conversion walks (reading order
//! → section → page_template → storyline → overlay storyline) but collects run
//! geometry instead of building content. It keys on *structure*, not on
//! fragment names, so it reads Amazon's Send-to-Kindle output as well as
//! [`pdf_to_kfx`](crate::export::pdf_to_kfx)'s.
//!
//! [`read_pages`] adds what the container's `book_navigation` already knows:
//! the outline and the per-page labels. Both are baked in at authoring time, so
//! a caller that has the KFX never has to parse the embedded PDF to learn its
//! structure — the page box sizes come from the page templates here too.

use std::collections::HashMap;

use super::error::KfxError;
use super::ion::IonValue;
use super::loader::{self, BookData};
use super::navigation::for_each_nav_container;
use super::structure::{collect_element_ids, lookup_fragment, reading_orders};
use super::symbols::KfxSymbol;
use crate::formats::kfx::container::get_field;
use crate::formats::pdf::structure::PdfOutlineItem;

/// One page's text layer plus the eids anchored on it.
#[derive(Debug, Clone, Default)]
pub struct PdfPageText {
    /// Text runs to overlay, in document order. Geometry is a fraction in
    /// `[0, 1]` of the page box, top-left origin, Y down — the same space as the
    /// rendered page, so a run maps onto it as a percentage without knowing the
    /// render scale.
    pub runs: Vec<PdfTextRun>,
    /// Every eid registered on this page: the run eids *and* the page-structural
    /// eids (image, container, page_template). An eid-addressed position — a
    /// bookmark, an annotation, a nav target — resolves to its page through this
    /// set, which is what makes an image-only page (no runs at all) addressable.
    pub eids: Vec<i64>,
    /// The page box (`fixed_width`/`fixed_height`) in **points** — the box the
    /// `runs` fractions are relative to, and the page's aspect ratio.
    pub box_w: f32,
    pub box_h: f32,
}

/// One text run positioned over the page. Geometry is a page fraction.
///
/// A run is whatever unit the authoring tool emitted — usually a word, but the
/// format guarantees nothing finer than "a positioned string".
#[derive(Debug, Clone)]
pub struct PdfTextRun {
    pub eid: i64,
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub text: String,
}

/// A PDF-backed KFX's pages, outline, and page labels — everything the
/// container states about its own page structure.
#[derive(Debug, Default)]
pub struct PdfPages {
    /// Per-page text layer, in reading order (see [`PdfPageText`]).
    pub pages: Vec<PdfPageText>,
    /// Document outline (bookmarks) → page indices. Empty when the KFX carries
    /// no `toc` nav container (a PDF without bookmarks).
    pub outline: Vec<PdfOutlineItem>,
    /// One display label per page (`page_labels[i]` for page `i`): the PDF's own
    /// labels (roman front matter, `Cover`, …) when present, else `"1".."N"`.
    pub page_labels: Vec<String>,
}

/// Extract the per-page text layer from a PDF-backed KFX, in reading order (one
/// entry per page). A page with no text storyline (image-only / scanned) yields
/// empty `runs`; its `eids` still carry the page anchor.
pub fn page_text_layer(kfx_bytes: &[u8]) -> Result<Vec<PdfPageText>, KfxError> {
    let book = loader::load(kfx_bytes)?;
    Ok(page_text_layer_from_book(&book))
}

/// As [`page_text_layer`], from an already-loaded container.
pub fn page_text_layer_from_book(book: &BookData) -> Vec<PdfPageText> {
    let orders = reading_orders(book);
    let Some(order) = orders.first() else {
        return Vec::new();
    };
    order.iter().map(|name| page_text(book, name)).collect()
}

/// Read a PDF-backed KFX's whole page structure from one container load.
pub fn read_pages(kfx_bytes: &[u8]) -> Result<PdfPages, KfxError> {
    let book = loader::load(kfx_bytes)?;
    Ok(read_pages_from_book(&book))
}

/// As [`read_pages`], from an already-loaded container.
pub fn read_pages_from_book(book: &BookData) -> PdfPages {
    let pages = page_text_layer_from_book(book);
    // eid → 0-based page index, from each page's registered eids (which include
    // the page-image eid that nav `target_position`s point at) and its run eids.
    // First page wins on the (rare) collision.
    let mut eid_to_page: HashMap<i64, usize> = HashMap::new();
    for (i, p) in pages.iter().enumerate() {
        for &eid in &p.eids {
            eid_to_page.entry(eid).or_insert(i);
        }
        for r in &p.runs {
            eid_to_page.entry(r.eid).or_insert(i);
        }
    }
    let (outline, page_labels) = read_nav(book, &eid_to_page, pages.len());
    PdfPages {
        pages,
        outline,
        page_labels,
    }
}

/// Read the `book_navigation`: the `page_list` container → per-page label
/// strings, and the `toc` container → nested outline (each `target_position.id`
/// mapped to a 0-based page index via `eid_to_page`). Falls back to sequential
/// `"1".."N"` labels and an empty outline when the nav is absent.
fn read_nav(
    book: &BookData,
    eid_to_page: &HashMap<i64, usize>,
    n_pages: usize,
) -> (Vec<PdfOutlineItem>, Vec<String>) {
    let mut labels: Vec<String> = (1..=n_pages).map(|n| n.to_string()).collect();
    let mut outline: Vec<PdfOutlineItem> = Vec::new();
    for_each_nav_container(book, |nav_type, entries| match nav_type {
        "page_list" => fill_page_labels(entries, eid_to_page, &mut labels),
        "toc" => outline.extend(entries.iter().filter_map(|e| nav_item(e, eid_to_page))),
        _ => {}
    });
    (outline, labels)
}

/// Overwrite `labels[page]` from each `page_list` entry's
/// `representation.label`, placing it at the page its `target_position.id`
/// resolves to (the entries are emitted in page order, so the ordinal is the
/// fallback when the eid isn't mapped).
fn fill_page_labels(
    entries: &[IonValue],
    eid_to_page: &HashMap<i64, usize>,
    labels: &mut [String],
) {
    for (ordinal, entry) in entries.iter().enumerate() {
        let Some(fields) = entry.unwrap_annotated().as_struct() else {
            continue;
        };
        let Some(label) = entry_label(fields) else {
            continue;
        };
        let page = entry_target_eid(fields)
            .and_then(|eid| eid_to_page.get(&eid).copied())
            .unwrap_or(ordinal);
        if let Some(slot) = labels.get_mut(page) {
            *slot = label;
        }
    }
}

/// One `toc` entry → [`PdfOutlineItem`], recursing into nested `entries`. The
/// target eid maps to a page index (0 when unresolved — a defensive default;
/// an authoring tool registers every toc target in the position map).
fn nav_item(entry: &IonValue, eid_to_page: &HashMap<i64, usize>) -> Option<PdfOutlineItem> {
    let fields = entry.unwrap_annotated().as_struct()?;
    let title = entry_label(fields).unwrap_or_default();
    let page_index = entry_target_eid(fields)
        .and_then(|eid| eid_to_page.get(&eid).copied())
        .unwrap_or(0);
    let children = get_field(fields, KfxSymbol::Entries as u64)
        .and_then(|v| v.as_list())
        .map(|list| {
            list.iter()
                .filter_map(|c| nav_item(c, eid_to_page))
                .collect()
        })
        .unwrap_or_default();
    Some(PdfOutlineItem {
        title,
        page_index,
        children,
    })
}

/// A nav entry's label: `representation.label`, falling back to a direct
/// `label`. `None` only when neither is present.
fn entry_label(fields: &[(u64, IonValue)]) -> Option<String> {
    get_field(fields, KfxSymbol::Representation as u64)
        .and_then(|v| v.as_struct())
        .and_then(|s| get_field(s, KfxSymbol::Label as u64))
        .and_then(|v| v.as_string())
        .or_else(|| get_field(fields, KfxSymbol::Label as u64).and_then(|v| v.as_string()))
        .map(|s| s.to_string())
}

/// A nav entry's `target_position.id` (the target page's element eid).
fn entry_target_eid(fields: &[(u64, IonValue)]) -> Option<i64> {
    get_field(fields, KfxSymbol::TargetPosition as u64)
        .and_then(|v| v.as_struct())
        .and_then(|pos| get_field(pos, KfxSymbol::Id as u64))
        .and_then(|v| v.as_int())
}

/// The text layer for one section (page).
fn page_text(book: &BookData, section_name: &str) -> PdfPageText {
    let mut out = PdfPageText::default();
    let Some(section) = lookup_fragment(book, KfxSymbol::Section, section_name) else {
        return out;
    };
    let Some(fields) = section.unwrap_annotated().as_struct() else {
        return out;
    };
    let Some(templates) =
        get_field(fields, KfxSymbol::PageTemplates as u64).and_then(|v| v.as_list())
    else {
        return out;
    };

    // The page's anchor set: every eid reachable from its templates (the
    // page_template ids plus the storyline ids, following `story_name` refs).
    for tpl in templates {
        collect_element_ids(tpl, book, &mut out.eids);
    }

    // Geometry: follow the (last, "main") page_template's storyline chain,
    // picking up the page box and every text run. Portrait and landscape
    // reference the same storyline, so either resolves to the same runs.
    if let Some(tpl) = templates.last() {
        let mut page_box: Option<(f32, f32)> = None;
        let mut runs: Vec<PdfTextRun> = Vec::new();
        let mut visited: Vec<String> = Vec::new();
        walk(tpl, book, &mut visited, &mut page_box, &mut runs);
        if let Some((bw, bh)) = page_box
            && bw > 0.0
            && bh > 0.0
        {
            out.box_w = bw / 100.0; // pt×100 → pt
            out.box_h = bh / 100.0;
            for mut r in runs {
                r.left /= bw;
                r.top /= bh;
                r.width /= bw;
                r.height /= bh;
                out.runs.push(r);
            }
        }
    }
    out
}

/// Symbol id of a value, if it is (or annotates) a bare symbol.
fn sym(v: &IonValue) -> Option<u64> {
    match v.unwrap_annotated() {
        IonValue::Symbol(s) => Some(*s),
        _ => None,
    }
}

/// Walk a page_template's storyline tree, collecting the page box (the first
/// container with integer `fixed_width` *and* `fixed_height`, in pt×100) and
/// every `type:text` run's geometry (also pt×100; normalized to a page fraction
/// by the caller). `story_name` references are followed (cycle-guarded by
/// `visited`), which is how the page image storyline pulls in its invisible
/// text-overlay storyline.
fn walk(
    value: &IonValue,
    book: &BookData,
    visited: &mut Vec<String>,
    page_box: &mut Option<(f32, f32)>,
    runs: &mut Vec<PdfTextRun>,
) {
    let inner = value.unwrap_annotated();
    if let Some(fields) = inner.as_struct() {
        // Page box — the page container. Both dims must be plain ints (pt×100):
        // the landscape page_template carries `fixed_width: 100%` (a {value,unit}
        // struct, no height), which `as_int()` rejects, so it is never mistaken
        // for the box.
        if page_box.is_none()
            && let Some(w) =
                get_field(fields, KfxSymbol::FixedWidth as u64).and_then(|v| v.as_int())
            && let Some(h) =
                get_field(fields, KfxSymbol::FixedHeight as u64).and_then(|v| v.as_int())
        {
            *page_box = Some((w as f32, h as f32));
        }

        // A text run: `type: text` with an id, geometry, and content.
        if get_field(fields, KfxSymbol::Type as u64).and_then(sym) == Some(KfxSymbol::Text as u64)
            && let Some(eid) = get_field(fields, KfxSymbol::Id as u64).and_then(|v| v.as_int())
        {
            let g = |s: KfxSymbol| {
                get_field(fields, s as u64)
                    .and_then(|v| v.as_int())
                    .unwrap_or(0) as f32
            };
            runs.push(PdfTextRun {
                eid,
                left: g(KfxSymbol::Left),
                top: g(KfxSymbol::Top),
                width: g(KfxSymbol::Width),
                height: g(KfxSymbol::Height),
                text: get_field(fields, KfxSymbol::Content as u64)
                    .and_then(|v| v.as_string())
                    .unwrap_or("")
                    .to_string(),
            });
        }

        // Follow a `story_name` reference (page image storyline → text overlay).
        if let Some(sn) = get_field(fields, KfxSymbol::StoryName as u64)
            && let Some(name) = book.symbols.text_of(sn)
        {
            let name = name.to_string();
            if !visited.contains(&name) {
                visited.push(name.clone());
                if let Some(story) = lookup_fragment(book, KfxSymbol::Storyline, &name) {
                    walk(story, book, visited, page_box, runs);
                }
            }
        }

        if let Some(list) =
            get_field(fields, KfxSymbol::ContentList as u64).and_then(|v| v.as_list())
        {
            for child in list {
                walk(child, book, visited, page_box, runs);
            }
        }
    } else if let Some(list) = inner.as_list() {
        for item in list {
            walk(item, book, visited, page_box, runs);
        }
    }
}

#[cfg(all(test, feature = "pdf"))]
mod tests {
    use super::*;
    use crate::export::{PdfKfxMeta, pdf_to_kfx};
    use crate::formats::kfx::container::SymbolTable;
    use crate::formats::kfx::loader::BookMetadata;
    use crate::formats::pdf::structure::{PdfDoc, PdfPage};

    fn field(sym: KfxSymbol, v: IonValue) -> (u64, IonValue) {
        (sym as u64, v)
    }

    /// A one-page book whose page template carries the box and whose referenced
    /// storyline carries two `type:text` runs — the shape Amazon's overlay has.
    ///
    /// Built as a [`BookData`] rather than a serialized container because
    /// `pdf_to_kfx` writes no overlay (it embeds the PDF and renders pages as
    /// images), so a generated fixture leaves the run path unexercised.
    fn book_with_overlay() -> BookData {
        // Doc symbols land above the base, so ids are base + index.
        let base = 1000u64;
        let symbols = SymbolTable::new(base, vec!["sec1".to_string(), "story1".to_string()]);
        let (sec_sym, story_sym) = (IonValue::Symbol(base), IonValue::Symbol(base + 1));

        let run = |eid: i64, left: i64, top: i64, w: i64, h: i64, text: &str| {
            IonValue::Struct(vec![
                field(KfxSymbol::Id, IonValue::Int(eid)),
                field(KfxSymbol::Type, IonValue::Symbol(KfxSymbol::Text as u64)),
                field(KfxSymbol::Left, IonValue::Int(left)),
                field(KfxSymbol::Top, IonValue::Int(top)),
                field(KfxSymbol::Width, IonValue::Int(w)),
                field(KfxSymbol::Height, IonValue::Int(h)),
                field(KfxSymbol::Content, IonValue::String(text.to_string())),
            ])
        };

        let mut by_type: HashMap<u64, HashMap<String, IonValue>> = HashMap::new();
        by_type.insert(
            KfxSymbol::DocumentData as u64,
            HashMap::from([(
                "doc".to_string(),
                IonValue::Struct(vec![field(
                    KfxSymbol::ReadingOrders,
                    IonValue::List(vec![IonValue::Struct(vec![field(
                        KfxSymbol::Sections,
                        IonValue::List(vec![sec_sym]),
                    )])]),
                )]),
            )]),
        );
        by_type.insert(
            KfxSymbol::Section as u64,
            HashMap::from([(
                "sec1".to_string(),
                IonValue::Struct(vec![field(
                    KfxSymbol::PageTemplates,
                    // Page box in pt×100: 612 × 792 pt.
                    IonValue::List(vec![IonValue::Struct(vec![
                        field(KfxSymbol::Id, IonValue::Int(10)),
                        field(KfxSymbol::FixedWidth, IonValue::Int(61200)),
                        field(KfxSymbol::FixedHeight, IonValue::Int(79200)),
                        field(KfxSymbol::StoryName, story_sym),
                    ])]),
                )]),
            )]),
        );
        by_type.insert(
            KfxSymbol::Storyline as u64,
            HashMap::from([(
                "story1".to_string(),
                IonValue::Struct(vec![field(
                    KfxSymbol::ContentList,
                    IonValue::List(vec![
                        // A quarter in from the left, an eighth down, half wide.
                        run(20, 15300, 9900, 30600, 3960, "Hello"),
                        run(21, 15300, 13860, 30600, 3960, "world"),
                    ]),
                )]),
            )]),
        );

        BookData {
            by_type,
            raw_media: HashMap::new(),
            symbols,
            metadata: BookMetadata::default(),
        }
    }

    /// Run geometry is reported as a fraction of the page box, and the box
    /// itself in points — the contract a caller positions an overlay against.
    #[test]
    fn run_geometry_is_a_fraction_of_the_page_box() {
        let pages = page_text_layer_from_book(&book_with_overlay());
        assert_eq!(pages.len(), 1, "one section, one page");
        let page = &pages[0];

        assert_eq!((page.box_w, page.box_h), (612.0, 792.0), "box in points");
        assert_eq!(page.runs.len(), 2, "both overlay runs");

        let first = &page.runs[0];
        assert_eq!(first.eid, 20);
        assert_eq!(first.text, "Hello");
        assert_eq!(first.left, 0.25, "15300 / 61200");
        assert_eq!(first.top, 0.125, "9900 / 79200");
        assert_eq!(first.width, 0.5, "30600 / 61200");
        assert_eq!(first.height, 0.05, "3960 / 79200");

        assert_eq!(page.runs[1].eid, 21);
        assert_eq!(page.runs[1].top, 0.175, "second line sits below the first");

        // The page's anchor set spans the template and both runs, so any of the
        // three eids resolves to this page.
        assert!(
            [10, 20, 21].iter().all(|e| page.eids.contains(e)),
            "eids {:?} must cover template + runs",
            page.eids
        );
    }

    /// A page whose box never resolves reports no runs rather than fractions
    /// divided by a guessed box — a wrong overlay is worse than none.
    #[test]
    fn a_page_without_a_box_reports_no_runs() {
        let mut book = book_with_overlay();
        let section = book
            .by_type
            .get_mut(&(KfxSymbol::Section as u64))
            .and_then(|m| m.get_mut("sec1"))
            .unwrap();
        let IonValue::Struct(fields) = section else {
            unreachable!()
        };
        let IonValue::List(templates) = &mut fields[0].1 else {
            unreachable!()
        };
        let IonValue::Struct(tpl) = &mut templates[0] else {
            unreachable!()
        };
        tpl.retain(|(id, _)| *id != KfxSymbol::FixedWidth as u64);

        let pages = page_text_layer_from_book(&book);
        assert!(pages[0].runs.is_empty(), "no box ⇒ no positioned runs");
        assert!(
            pages[0].eids.contains(&20),
            "the eids still register, so the page stays addressable"
        );
    }

    fn fake_pdf() -> Vec<u8> {
        let mut v = b"%PDF-1.4\n% page-structure fixture\n".to_vec();
        v.extend_from_slice(b"\n%%EOF\n");
        v
    }

    fn flat(items: &[PdfOutlineItem], depth: usize, out: &mut Vec<(usize, String, usize)>) {
        for it in items {
            out.push((depth, it.title.clone(), it.page_index));
            flat(&it.children, depth + 1, out);
        }
    }

    fn nav_fixture() -> PdfDoc {
        PdfDoc {
            bytes: fake_pdf(),
            pages: vec![
                PdfPage {
                    width: 612.0,
                    height: 792.0,
                    rotation: 0,
                },
                PdfPage {
                    width: 595.0,
                    height: 842.0,
                    rotation: 0,
                },
                PdfPage {
                    width: 612.0,
                    height: 792.0,
                    rotation: 0,
                },
            ],
            title: Some("Nav Round Trip".to_string()),
            author: None,
            outline: vec![
                PdfOutlineItem {
                    title: "Chapter 1".to_string(),
                    page_index: 0,
                    children: vec![PdfOutlineItem {
                        title: "Section 1.1".to_string(),
                        page_index: 1,
                        children: Vec::new(),
                    }],
                },
                PdfOutlineItem {
                    title: "Chapter 2".to_string(),
                    page_index: 2,
                    children: Vec::new(),
                },
            ],
            page_labels: vec!["Cover".to_string(), "i".to_string(), "1".to_string()],
        }
    }

    fn kfx_of(doc: &PdfDoc) -> Vec<u8> {
        let meta = PdfKfxMeta {
            title: "Nav Round Trip".to_string(),
            author: None,
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };
        pdf_to_kfx(doc, &meta, None, None)
    }

    /// `read_pages` must recover the outline + page labels + page sizes that
    /// `pdf_to_kfx` baked into `book_navigation` — the structure a consumer
    /// would otherwise have to parse the embedded PDF to learn.
    #[test]
    fn page_structure_round_trips_from_pdf_to_kfx() {
        let doc = nav_fixture();
        let kfx = kfx_of(&doc);
        let read = read_pages(&kfx).expect("read_pages");

        assert_eq!(read.pages.len(), 3, "one entry per page");
        assert_eq!(
            read.page_labels,
            vec!["Cover", "i", "1"],
            "page labels from page_list"
        );

        let (mut want, mut got) = (Vec::new(), Vec::new());
        flat(&doc.outline, 0, &mut want);
        flat(&read.outline, 0, &mut got);
        assert_eq!(
            got, want,
            "outline (title + page index, nested) must round-trip"
        );

        // Page box sizes come back from the KFX page_template (≈ MediaBox pt).
        for (i, p) in doc.pages.iter().enumerate() {
            assert!(
                (read.pages[i].box_w - p.width).abs() < 0.5,
                "page {i} width"
            );
            assert!(
                (read.pages[i].box_h - p.height).abs() < 0.5,
                "page {i} height"
            );
        }
    }

    /// Every eid a page registers must resolve back to that page, and to no
    /// other — the property an eid-addressed position (bookmark, annotation,
    /// nav target) depends on.
    #[test]
    fn each_page_owns_its_eids() {
        let kfx = kfx_of(&nav_fixture());
        let read = read_pages(&kfx).expect("read_pages");

        let mut seen: HashMap<i64, usize> = HashMap::new();
        for (i, page) in read.pages.iter().enumerate() {
            assert!(!page.eids.is_empty(), "page {i} registers no eid");
            for &eid in &page.eids {
                if let Some(prev) = seen.insert(eid, i) {
                    assert_eq!(prev, i, "eid {eid} is registered on pages {prev} and {i}");
                }
            }
        }
    }
}
