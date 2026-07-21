//! PDF format support — the shared document core plus the surgical source-edit
//! primitives.
//!
//! `doc` is the shared core both sides need: [`load_pdf`] (the only correct
//! way to open a PDF here) and the PDF text-string codec; [`render`] is the
//! PDFKit page rasterizer + text-layer extractor. [`edit`] adds the
//! incremental-update harness ([`PdfPackage`]) the source editor writes through;
//! [`metadata_edit`] (`/Info`), [`toc_repair`] (`/Outlines`) and [`cover`] (the
//! first page) are its consumers.
//!
//! There is deliberately no image-extract primitive here, unlike the KFX and
//! EPUB families. A PDF page *is* an image, so pulling images out of a PDF means
//! rendering its pages — [`render`], not an object-model walk.
//!
//! [`assemble`] is the write side: per-page vector art in, a fresh multi-page
//! PDF out. It serves fixed-layout sources, where a page is one self-contained
//! image and there is no text flow to typeset.
//!
//! Reading a PDF's *structure* — page sizes, outline, page labels — lives in
//! [`crate::import::pdf`] (`probe_pdf`), which feeds the PDF→KFX path and is
//! built on this module's core.

pub mod assemble;
pub mod cover;
pub mod doc;
pub mod edit;
pub mod metadata_edit;
pub mod render;
pub mod toc_repair;

pub use assemble::svgs_to_pdf;
pub use cover::{CoverMode, set_cover_page};
pub use doc::{PdfDoc, PdfOutlineItem, PdfPage, load_pdf};
pub use edit::PdfPackage;
pub use metadata_edit::{MetadataPatch, edit_metadata};
pub use toc_repair::set_toc;
