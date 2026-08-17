//! What `formats::kfx::pdf_pages` recovers from a PDF-backed KFX.
//!
//! This feeds the fixed-layout reader's text overlay and the ink-anchor page
//! geometry, so a page box or an eid set that drifts moves annotations on the
//! page.
//!
//! What a generated fixture can cover is page boxes, the eid sets a bookmark
//! resolves through, the outline, and the page labels: `pdf_to_kfx` embeds the
//! PDF and renders each page as an image, so it writes no text overlay and the
//! run comparison below is vacuous on this input (it stays because it is the
//! property under test, and would bite if that ever changed). Run geometry is
//! pinned instead by `pdf_pages`'s own unit tests, against a hand-built
//! overlay.

use bokai::export::{PdfKfxMeta, pdf_to_kfx};
use bokai::formats::kfx::pdf_pages;
use bokai::formats::pdf::{PdfDoc, PdfOutlineItem, PdfPage};

fn flat(items: &[PdfOutlineItem], depth: usize, out: &mut Vec<(usize, String, usize)>) {
    for it in items {
        out.push((depth, it.title.clone(), it.page_index));
        flat(&it.children, depth + 1, out);
    }
}

/// A PDF-backed KFX with mixed page sizes, a nested outline, and non-sequential
/// page labels — the three things the two readers could disagree about.
fn pdf_backed_kfx() -> Vec<u8> {
    let doc = PdfDoc {
        bytes: {
            let mut v = b"%PDF-1.4\n% parity fixture\n".to_vec();
            v.extend_from_slice(b"\n%%EOF\n");
            v
        },
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
            PdfPage {
                width: 421.0,
                height: 595.0,
                rotation: 0,
            },
        ],
        title: Some("Parity".to_string()),
        author: Some("A. Author".to_string()),
        outline: vec![
            PdfOutlineItem {
                title: "Front".to_string(),
                page_index: 0,
                children: vec![PdfOutlineItem {
                    title: "Preface".to_string(),
                    page_index: 1,
                    children: Vec::new(),
                }],
            },
            PdfOutlineItem {
                title: "Body".to_string(),
                page_index: 2,
                children: Vec::new(),
            },
        ],
        page_labels: vec![
            "Cover".to_string(),
            "i".to_string(),
            "1".to_string(),
            "2".to_string(),
        ],
    };
    let meta = PdfKfxMeta {
        title: "Parity".to_string(),
        author: Some("A. Author".to_string()),
        language: "en".to_string(),
        date: None,
        publisher: None,
        page_progression_direction: None,
    };
    pdf_to_kfx(&doc, &meta, None, None)
}

/// The fixture declares its own page boxes, so they are asserted literally
/// rather than against another reader: these are the numbers a viewer sizes
/// each page to.
#[test]
fn the_page_reader_recovers_what_the_fixture_declares() {
    let kfx = pdf_backed_kfx();
    let read = pdf_pages::read_pages(&kfx).expect("pdf_pages::read_pages");

    let boxes: Vec<(f32, f32)> = read.pages.iter().map(|p| (p.box_w, p.box_h)).collect();
    assert_eq!(
        boxes,
        vec![
            (612.0, 792.0),
            (595.0, 842.0),
            (612.0, 792.0),
            (421.0, 595.0)
        ],
        "page boxes, in order, including the mixed sizes"
    );
    assert!(
        read.pages.iter().any(|p| !p.eids.is_empty()),
        "fixture registered no eids"
    );

    assert_eq!(
        read.page_labels,
        vec!["Cover", "i", "1", "2"],
        "non-sequential page labels survive"
    );

    let mut outline = Vec::new();
    flat(&read.outline, 0, &mut outline);
    assert_eq!(
        outline,
        vec![
            (0, "Front".to_string(), 0),
            (1, "Preface".to_string(), 1),
            (0, "Body".to_string(), 2),
        ],
        "outline nesting and page targets"
    );
}

/// The layer-only entry takes a different path into the same walk — it is what
/// the ink anchor cache calls, so it has to agree with the full read.
#[test]
fn the_text_layer_agrees_with_the_full_page_read() {
    let kfx = pdf_backed_kfx();
    let full = pdf_pages::read_pages(&kfx).expect("pdf_pages::read_pages");
    let layer = pdf_pages::page_text_layer(&kfx).expect("pdf_pages::page_text_layer");

    assert_eq!(layer.len(), full.pages.len(), "page count");
    for (i, (a, b)) in full.pages.iter().zip(layer.iter()).enumerate() {
        assert_eq!((b.box_w, b.box_h), (a.box_w, a.box_h), "page {i} box");
        assert_eq!(b.eids, a.eids, "page {i} eids");
        assert_eq!(
            b.runs.iter().map(|r| r.eid).collect::<Vec<_>>(),
            a.runs.iter().map(|r| r.eid).collect::<Vec<_>>(),
            "page {i} run eids"
        );
    }
}
