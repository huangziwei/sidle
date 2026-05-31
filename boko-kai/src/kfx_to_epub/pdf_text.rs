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

use super::ConvertError;
use super::content::{collect_element_ids, extract_reading_orders, lookup_fragment};
use super::loader::{self, BookData};
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
