//! PDF format support — the shared document core plus the surgical source-edit
//! primitives.
//!
//! [`doc`] is the shared core both sides need: [`load_pdf`] (the only correct
//! way to open a PDF here) and the PDF text-string codec. [`edit`] adds the
//! incremental-update harness ([`PdfPackage`]) the source editor writes through;
//! [`metadata_edit`] (`/Info`) and [`toc_repair`] (`/Outlines`) are its
//! consumers.
//!
//! Reading a PDF's *structure* — page sizes, outline, page labels — lives in
//! [`crate::import::pdf`] (`probe_pdf`), which feeds the PDF→KFX path and is
//! built on this module's core.

pub(crate) mod doc;
pub mod edit;
pub mod metadata_edit;
pub mod toc_repair;

pub use doc::load_pdf;
pub use edit::PdfPackage;
pub use metadata_edit::{MetadataPatch, edit_metadata};
pub use toc_repair::set_toc;
