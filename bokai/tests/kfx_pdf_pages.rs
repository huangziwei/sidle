//! `formats::kfx::pdf_pages` must read a PDF-backed KFX exactly as the frozen
//! `kfx_to_epub` port does.
//!
//! The port is the shipping reference for the fixed-layout reader's text
//! overlay and for the ink-anchor page geometry, so the copy that replaced it
//! has to agree field for field. This test exists to prove that equality and
//! retires with the port.
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
            },
            PdfPage {
                width: 595.0,
                height: 842.0,
            },
            PdfPage {
                width: 612.0,
                height: 792.0,
            },
            PdfPage {
                width: 421.0,
                height: 595.0,
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

#[test]
fn page_reader_matches_the_port_field_for_field() {
    let kfx = pdf_backed_kfx();
    let port = bokai::kfx_to_epub::pdf_reader_data(&kfx).expect("port pdf_reader_data");
    let copy = pdf_pages::read_pages(&kfx).expect("pdf_pages::read_pages");

    assert_eq!(copy.pages.len(), port.pages.len(), "page count");
    // Non-vacuity: an equality assertion over empty pages proves nothing.
    assert!(!copy.pages.is_empty(), "fixture produced no pages");
    assert!(
        copy.pages.iter().any(|p| !p.eids.is_empty()),
        "fixture registered no eids, so the eid comparison is vacuous"
    );

    for (i, (p, c)) in port.pages.iter().zip(copy.pages.iter()).enumerate() {
        assert_eq!((c.box_w, c.box_h), (p.box_w, p.box_h), "page {i} box");
        assert_eq!(c.eids, p.eids, "page {i} registered eids");
        assert_eq!(c.runs.len(), p.words.len(), "page {i} run count");
        for (j, (w, r)) in p.words.iter().zip(c.runs.iter()).enumerate() {
            assert_eq!(
                (r.eid, r.left, r.top, r.width, r.height, r.text.as_str()),
                (w.eid, w.left, w.top, w.width, w.height, w.text.as_str()),
                "page {i} run {j}"
            );
        }
    }

    assert_eq!(copy.page_labels, port.page_labels, "page labels");

    let (mut want, mut got) = (Vec::new(), Vec::new());
    flat(&port.outline, 0, &mut want);
    flat(&copy.outline, 0, &mut got);
    assert_eq!(got, want, "outline tree");
}

/// The layer-only entry must agree with the port's too — it is what the ink
/// anchor cache calls, and it takes a different path into the same walk.
#[test]
fn text_layer_matches_the_port() {
    let kfx = pdf_backed_kfx();
    let port = bokai::kfx_to_epub::pdf_text_layer(&kfx).expect("port pdf_text_layer");
    let copy = pdf_pages::page_text_layer(&kfx).expect("pdf_pages::page_text_layer");

    assert_eq!(copy.len(), port.len(), "page count");
    for (i, (p, c)) in port.iter().zip(copy.iter()).enumerate() {
        assert_eq!((c.box_w, c.box_h), (p.box_w, p.box_h), "page {i} box");
        assert_eq!(c.eids, p.eids, "page {i} eids");
        assert_eq!(
            c.runs.iter().map(|r| r.eid).collect::<Vec<_>>(),
            p.words.iter().map(|w| w.eid).collect::<Vec<_>>(),
            "page {i} run eids"
        );
    }
}
