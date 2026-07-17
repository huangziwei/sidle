//! EPUB format support — parsing plus the surgical source-edit primitives.
//!
//! [`parser`] is read-only (OPF/NCX/nav). [`edit`] adds the shared zip
//! edit harness ([`EpubPackage`]) that the source editor writes through, and
//! [`image_extract`] is its first read-only consumer.

pub mod edit;
pub mod image_extract;
pub mod metadata_edit;
pub(crate) mod parser;
pub mod toc_repair;
mod zip_repair;

pub use edit::EpubPackage;
pub use image_extract::{ExtractedImage, epub_extract_images};
pub use metadata_edit::{MetadataPatch, edit_metadata};
pub use parser::{
    OpfData, parse_container_xml, parse_nav_landmarks, parse_nav_page_list, parse_nav_toc,
    parse_ncx, parse_opf, parse_opf_guide, strip_bom,
};
pub use zip_repair::neutralize_spurious_zip64;
