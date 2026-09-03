//! `epub`: EPUB container parsers and in-place source-edit primitives.

pub mod class_rename;
pub mod edit;
pub mod image_extract;
pub mod manifest;
pub(crate) mod markup;
pub mod metadata_edit;
pub(crate) mod nav_doc;
pub(crate) mod opf_meta;
pub(crate) mod page_shape;
pub(crate) mod parser;
pub mod pretty;
pub mod spine_repair;
pub mod split;
pub mod split_doc;
pub(crate) mod structure;
pub mod toc_repair;
pub mod unflatten_styles;
pub mod unused_css;
pub mod upgrade;
mod zip_repair;

pub use class_rename::rename_class;
pub use edit::{Changes, EpubPackage};
pub use image_extract::{ExtractedImage, epub_extract_images};
pub use manifest::{Member, MemberRole, add_manifest_item, members, remove_manifest_item};
pub use metadata_edit::{MetadataPatch, edit_metadata};
pub use parser::{
    OpfData, parse_container_xml, parse_nav_landmarks, parse_nav_page_list, parse_nav_toc,
    parse_ncx, parse_opf, parse_opf_guide, strip_bom,
};
pub use pretty::{beautify, pretty_css, pretty_xhtml};
pub use split_doc::{merge_with_next, split_document};
pub use unflatten_styles::{
    FlattenedStyles, Restored, StyleDiff, flattened_styles, restore_styles,
};
pub use unused_css::{UnusedRule, remove_unused_css, unused_css};
pub use upgrade::upgrade_to_epub3;
pub use zip_repair::neutralize_spurious_zip64;
