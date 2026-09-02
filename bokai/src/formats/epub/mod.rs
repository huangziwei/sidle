//! EPUB format support — parsing plus the surgical source-edit primitives.

pub mod edit;
pub mod image_extract;
pub mod manifest;
pub mod metadata_edit;
pub(crate) mod nav_doc;
pub(crate) mod opf_meta;
pub(crate) mod page_shape;
pub(crate) mod parser;
pub mod spine_repair;
pub mod split;
pub(crate) mod structure;
pub mod toc_repair;
pub mod unflatten_styles;
mod zip_repair;

pub use edit::EpubPackage;
pub use image_extract::{ExtractedImage, epub_extract_images};
pub use manifest::{Member, MemberRole, add_manifest_item, members, remove_manifest_item};
pub use metadata_edit::{MetadataPatch, edit_metadata};
pub use parser::{
    OpfData, parse_container_xml, parse_nav_landmarks, parse_nav_page_list, parse_nav_toc,
    parse_ncx, parse_opf, parse_opf_guide, strip_bom,
};
pub use unflatten_styles::{
    FlattenedStyles, Restored, StyleDiff, flattened_styles, restore_styles,
};
pub use zip_repair::neutralize_spurious_zip64;
