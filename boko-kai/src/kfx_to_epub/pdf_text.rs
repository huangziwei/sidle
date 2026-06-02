//! Per-page text-layer geometry for the fixed-layout (PDF-backed) reader.
//!
//! A pdf-backed KFX renders each page as an image (the embedded PDF, rasterized
//! on demand) with an invisible KFX text storyline pinned over it: `type:text`
//! runs at fixed `top`/`left`/`width`/`height`. The Scribe — and Amazon's
//! Send-to-Kindle output — make that overlay live (select / search / highlight);
//! the desktop reader does the same by laying each run out as a transparent,
//! absolutely-positioned `<span data-eid>` over the page image, reusing the
//! reflowable reader's eid-anchored select/highlight/search machinery.
//!
//! [`pdf_text_layer`] walks the document's reading order → section →
//! page_template → storyline → text-overlay storyline, the same chain
//! `content::process_reading_order` walks, but collects run geometry instead of
//! building a DOM. It follows the *structure*, not boko's fragment names, so it
//! works on Amazon-authored KFX (the synced-back device corpus) as well as
//! boko's own output.

use std::collections::HashMap;

use super::ConvertError;
use super::content::{collect_element_ids, extract_reading_orders, lookup_fragment};
use super::loader::{self, BookData};
use crate::import::pdf::PdfOutlineItem;
use crate::kfx::container::get_field;
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;

/// One page's selectable text layer plus the eids anchored on it.
#[derive(Debug, Clone, Default)]
pub struct PdfPageText {
    /// Text runs to overlay, in document order. Geometry is a fraction in
    /// `[0, 1]` of the page box, top-left origin, Y down — the same space as the
    /// rendered page image, so a run drops straight onto the image as a
    /// percentage-positioned span.
    pub words: Vec<PdfWord>,
    /// Every eid registered on this page: the run eids *and* the page-structural
    /// eids (image, container, page_template). The reader maps an annotation or
    /// last-read eid to its page through this set — essential for an image-only
    /// page, where a bookmark anchors to a page eid that has no text run.
    pub eids: Vec<i64>,
    /// The page box (`fixed_width`/`fixed_height`) in **points** — the box the
    /// `words` fractions are relative to. The reader renders the page image to
    /// span exactly this box so the spans line up with the glyphs.
    pub box_w: f32,
    pub box_h: f32,
}

/// One text run positioned over the page image. Geometry is a page fraction.
#[derive(Debug, Clone)]
pub struct PdfWord {
    pub eid: i64,
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub text: String,
}

/// Extract the per-page text layer from a fixed-layout (PDF-backed) KFX, in
/// reading order (one entry per page). A page with no text storyline
/// (image-only / scanned) yields empty `words`; its `eids` still carry the page
/// anchor so bookmarks/last-read on that page resolve.
pub fn pdf_text_layer(kfx_bytes: &[u8]) -> Result<Vec<PdfPageText>, ConvertError> {
    let book = loader::load(kfx_bytes)?;
    Ok(pdf_text_layer_from_book(&book))
}

/// As [`pdf_text_layer`], from an already-loaded container (shares one `load`
/// with the rest of the reader open path).
pub fn pdf_text_layer_from_book(book: &BookData) -> Vec<PdfPageText> {
    let orders = extract_reading_orders(book);
    let Some(order) = orders.first() else {
        return Vec::new();
    };
    order.iter().map(|name| page_text(book, name)).collect()
}

/// Everything the fixed-layout reader needs from a PDF-backed KFX, from a
/// **single** container load: the per-page text layer (words + eids + box
/// size), the document outline, and the per-page labels.
///
/// The reader used to derive the outline / page labels / page sizes from a
/// separate [`crate::import::probe_pdf`] — a full lopdf parse of the *embedded*
/// PDF, which on a large PDF cost seconds (a 64 MB scanned book measured ~6.7 s)
/// and ran on every open. But `pdf_to_kfx` bakes the `page_list` (per-page
/// labels) and `toc` into the KFX `book_navigation`, and the text layer already
/// carries each page's box size, so the probe is redundant here. Page count is
/// `pages.len()`.
#[derive(Debug, Default)]
pub struct PdfReaderData {
    /// Per-page text layer, in reading order (see [`PdfPageText`]).
    pub pages: Vec<PdfPageText>,
    /// Document outline (bookmarks) → page indices. Empty if the KFX carries no
    /// `toc` nav container (a PDF without bookmarks).
    pub outline: Vec<PdfOutlineItem>,
    /// One display label per page (`page_labels[i]` for page `i`): the PDF's own
    /// labels (roman front-matter, `Cover`, …) when present, else `"1".."N"`.
    pub page_labels: Vec<String>,
}

/// As [`pdf_reader_data_from_book`], loading the container from raw KFX bytes.
pub fn pdf_reader_data(kfx_bytes: &[u8]) -> Result<PdfReaderData, ConvertError> {
    let book = loader::load(kfx_bytes)?;
    Ok(pdf_reader_data_from_book(&book))
}

/// Extract the reader's PDF view from an already-loaded container (one `load`
/// shared with the rest of the open path).
pub fn pdf_reader_data_from_book(book: &BookData) -> PdfReaderData {
    let pages = pdf_text_layer_from_book(book);
    // eid → 0-based page index, from each page's registered eids (which include
    // the page-image eid that nav `target_position`s point at) and its run eids.
    // First page wins on the (rare) collision.
    let mut eid_to_page: HashMap<i64, usize> = HashMap::new();
    for (i, p) in pages.iter().enumerate() {
        for &eid in &p.eids {
            eid_to_page.entry(eid).or_insert(i);
        }
        for w in &p.words {
            eid_to_page.entry(w.eid).or_insert(i);
        }
    }
    let (outline, page_labels) = extract_pdf_nav(book, &eid_to_page, pages.len());
    PdfReaderData {
        pages,
        outline,
        page_labels,
    }
}

/// Read the PDF `book_navigation`: the `page_list` container → per-page label
/// strings, and the `toc` container → nested outline (each `target_position.id`
/// mapped to a 0-based page index via `eid_to_page`). `pdf_to_kfx` writes the
/// containers as separate `nav_container` entities referenced by symbol (the
/// fixed-layout shape — "the device rejects an inline nav_container"), so the
/// list items are resolved through `lookup_fragment`; an inline struct (the
/// reflowable shape) is handled too. Falls back to sequential `"1".."N"` labels
/// and an empty outline when the nav is absent.
fn extract_pdf_nav(
    book: &BookData,
    eid_to_page: &HashMap<i64, usize>,
    n_pages: usize,
) -> (Vec<PdfOutlineItem>, Vec<String>) {
    let mut labels: Vec<String> = (1..=n_pages).map(|n| n.to_string()).collect();
    let mut outline: Vec<PdfOutlineItem> = Vec::new();

    let Some(nav) = book.by_type.get(&(KfxSymbol::BookNavigation as u64)) else {
        return (outline, labels);
    };
    for value in nav.values() {
        let unwrapped = value.unwrap_annotated();
        let reading_orders: Vec<IonValue> = match unwrapped {
            IonValue::List(items) => items.clone(),
            IonValue::Struct(_) => vec![unwrapped.clone()],
            _ => Vec::new(),
        };
        for ro in reading_orders {
            let Some(ro_fields) = ro.as_struct() else {
                continue;
            };
            let Some(containers) =
                get_field(ro_fields, KfxSymbol::NavContainers as u64).and_then(|v| v.as_list())
            else {
                continue;
            };
            for item in containers {
                // The list item is either a symbol referencing a separate
                // nav_container entity (PDF) or an inline container struct
                // (reflowable). Resolve to the container value either way.
                let inner = item.unwrap_annotated();
                let container = match inner {
                    IonValue::Struct(_) => inner.clone(),
                    _ => match book
                        .symbols
                        .text_of(inner)
                        .and_then(|name| lookup_fragment(book, KfxSymbol::NavContainer, name))
                    {
                        Some(c) => c,
                        None => continue,
                    },
                };
                let container = container.unwrap_annotated();
                let Some(cf) = container.as_struct() else {
                    continue;
                };
                let nav_type = get_field(cf, KfxSymbol::NavType as u64)
                    .and_then(|v| book.symbols.text_of(v))
                    .unwrap_or("");
                let entries: Vec<IonValue> = get_field(cf, KfxSymbol::Entries as u64)
                    .and_then(|v| v.as_list())
                    .map(|s| s.to_vec())
                    .unwrap_or_default();
                match nav_type {
                    "page_list" => fill_page_labels(&entries, eid_to_page, &mut labels),
                    "toc" => {
                        for e in &entries {
                            if let Some(it) = pdf_nav_item(e, eid_to_page) {
                                outline.push(it);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
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
/// `pdf_to_kfx` registers every toc target in the position map).
fn pdf_nav_item(entry: &IonValue, eid_to_page: &HashMap<i64, usize>) -> Option<PdfOutlineItem> {
    let fields = entry.unwrap_annotated().as_struct()?;
    let title = entry_label(fields).unwrap_or_default();
    let page_index = entry_target_eid(fields)
        .and_then(|eid| eid_to_page.get(&eid).copied())
        .unwrap_or(0);
    let children = get_field(fields, KfxSymbol::Entries as u64)
        .and_then(|v| v.as_list())
        .map(|list| {
            list.iter()
                .filter_map(|c| pdf_nav_item(c, eid_to_page))
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
/// `label`. Returns `None` only when neither is present.
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

    // Geometry: follow the (last, calibre's "main") page_template's storyline
    // chain, picking up the page box and every text run. Portrait + landscape
    // reference the same storyline, so either resolves to the same runs.
    if let Some(tpl) = templates.last() {
        let mut page_box: Option<(f32, f32)> = None;
        let mut runs: Vec<PdfWord> = Vec::new();
        let mut visited: Vec<String> = Vec::new();
        walk(tpl, book, &mut visited, &mut page_box, &mut runs);
        if let Some((bw, bh)) = page_box
            && bw > 0.0
            && bh > 0.0
        {
            out.box_w = bw / 100.0; // pt×100 → pt
            out.box_h = bh / 100.0;
            for mut w in runs {
                w.left /= bw;
                w.top /= bh;
                w.width /= bw;
                w.height /= bh;
                out.words.push(w);
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
    runs: &mut Vec<PdfWord>,
) {
    let inner = value.unwrap_annotated();
    if let Some(fields) = inner.as_struct() {
        // Page box — the page container. Both dims must be plain ints (pt×100):
        // the landscape page_template carries `fixed_width: 100%` (a {value,unit}
        // struct, no height), which `as_int()` rejects, so it is never mistaken
        // for the box.
        if page_box.is_none()
            && let Some(w) = get_field(fields, KfxSymbol::FixedWidth as u64).and_then(|v| v.as_int())
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
            runs.push(PdfWord {
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
                    walk(&story, book, visited, page_box, runs);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{PdfKfxMeta, pdf_to_kfx};
    use crate::import::pdf::{PdfDoc, PdfOutlineItem, PdfPage};

    fn fake_pdf() -> Vec<u8> {
        let mut v = b"%PDF-1.4\n% reader-data fixture\n".to_vec();
        v.extend_from_slice(b"\n%%EOF\n");
        v
    }

    fn flat(items: &[PdfOutlineItem], depth: usize, out: &mut Vec<(usize, String, usize)>) {
        for it in items {
            out.push((depth, it.title.clone(), it.page_index));
            flat(&it.children, depth + 1, out);
        }
    }

    /// `pdf_reader_data` must recover the outline + page labels + page sizes that
    /// `pdf_to_kfx` baked into `book_navigation` — i.e. exactly what `probe_pdf`
    /// used to hand the reader, but from the KFX itself (no embedded-PDF parse).
    #[test]
    fn reader_data_round_trips_nav_from_pdf_to_kfx() {
        let doc = PdfDoc {
            bytes: fake_pdf(),
            pages: vec![
                PdfPage { width: 612.0, height: 792.0 },
                PdfPage { width: 595.0, height: 842.0 },
                PdfPage { width: 612.0, height: 792.0 },
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
        };
        let meta = PdfKfxMeta {
            title: "Nav Round Trip".to_string(),
            author: None,
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };

        let kfx = pdf_to_kfx(&doc, &meta, None, None);
        let rd = pdf_reader_data(&kfx).expect("pdf_reader_data");

        assert_eq!(rd.pages.len(), 3, "one entry per page");
        assert_eq!(rd.page_labels, vec!["Cover", "i", "1"], "page labels from page_list");

        let (mut want, mut got) = (Vec::new(), Vec::new());
        flat(&doc.outline, 0, &mut want);
        flat(&rd.outline, 0, &mut got);
        assert_eq!(got, want, "outline (title + page index, nested) must round-trip");

        // Page box sizes come back from the KFX page_template (≈ MediaBox pt).
        for (i, p) in doc.pages.iter().enumerate() {
            assert!((rd.pages[i].box_w - p.width).abs() < 0.5, "page {i} width");
            assert!((rd.pages[i].box_h - p.height).abs() < 0.5, "page {i} height");
        }
    }
}
