//! PDF format support — the shared document core plus the surgical source-edit
//! primitives.
//!
//! [`structure`] is the format's plain-data vocabulary — [`PdfPage`],
//! [`PdfOutlineItem`], [`PdfDoc`] — with no parser behind it.
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
//! `probe_pdf` in [`crate::import::pdf`] fills those types from a real file,
//! feeding the PDF→KFX path off this module's core.

#[cfg(feature = "pdf")]
pub mod assemble;
#[cfg(feature = "pdf")]
pub mod cover;
#[cfg(feature = "pdf")]
pub mod doc;
#[cfg(feature = "pdf")]
pub mod edit;
#[cfg(feature = "pdf")]
pub mod metadata_edit;
#[cfg(feature = "pdf")]
pub mod render;
pub mod structure;
#[cfg(feature = "pdf")]
pub mod toc_repair;

#[cfg(feature = "pdf")]
pub use assemble::svgs_to_pdf;
#[cfg(feature = "pdf")]
pub use cover::{CoverMode, set_cover_page};
#[cfg(feature = "pdf")]
pub use doc::load_pdf;
#[cfg(feature = "pdf")]
pub use edit::PdfPackage;
#[cfg(feature = "pdf")]
pub use metadata_edit::{MetadataPatch, edit_metadata};
pub use structure::{PdfDoc, PdfOutlineItem, PdfPage};
#[cfg(feature = "pdf")]
pub use toc_repair::set_toc;
