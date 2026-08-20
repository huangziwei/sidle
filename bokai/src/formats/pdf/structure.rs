//! What a PDF says about itself, as plain data: page geometry, the document
//! outline, and per-page labels. [`super::doc`] parses a PDF into these;
//! [`crate::formats::kfx::pdf_pages`] fills them from a PDF-backed KFX.

/// One PDF page's display geometry.
#[derive(Debug, Clone, Copy)]
pub struct PdfPage {
    /// Displayed width in PDF points (1/72 inch), `/Rotate` applied.
    pub width: f32,
    /// Displayed height in PDF points, `/Rotate` applied.
    pub height: f32,
    /// `/Rotate` as quarter turns clockwise (0..=3). `width`/`height` are the
    /// post-rotation extents, so this says how the page's own coordinate space
    /// is oriented inside them, not that they need swapping.
    pub rotation: u8,
}

/// One entry in the PDF document outline (bookmarks), resolved to a page. The
/// tree shape (`children`) mirrors the PDF's nesting so a TOC built from it
/// nests too.
#[derive(Debug, Clone)]
pub struct PdfOutlineItem {
    pub title: String,
    /// 0-based index of the page this bookmark jumps to.
    pub page_index: usize,
    pub children: Vec<PdfOutlineItem>,
}

/// A probed PDF: the verbatim bytes plus the structural facts a writer needs.
/// `bytes` is the unmodified input — embed it as-is.
#[derive(Debug, Clone)]
pub struct PdfDoc {
    pub bytes: Vec<u8>,
    pub pages: Vec<PdfPage>,
    pub title: Option<String>,
    pub author: Option<String>,
    /// Document outline (bookmarks). Empty if the PDF has none.
    pub outline: Vec<PdfOutlineItem>,
    /// Per-page display label (`page_labels[i]` for page `i`), from the catalog
    /// `/PageLabels` number tree. Always one per page: honors the PDF's labels
    /// (roman front-matter, prefixes like `Cover`) and falls back to sequential
    /// `"1".."N"` when the PDF declares none.
    pub page_labels: Vec<String>,
}
