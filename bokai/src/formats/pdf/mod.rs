//! PDF format support — the shared document core plus the surgical source-edit
//! primitives.

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
