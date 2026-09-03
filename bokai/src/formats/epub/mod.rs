//! `epub`: EPUB container parsers and in-place source editing.

pub mod edit;
pub mod image_extract;
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

pub use edit::{
    Changes, EpubPackage, Member, MemberRole, UnusedRule, add_manifest_item, beautify, members,
    merge_with_next, pretty_css, pretty_xhtml, remove_manifest_item, remove_unused_css,
    rename_class, split_document, unused_css, upgrade_to_epub3,
};
pub use image_extract::{ExtractedImage, epub_extract_images};
pub use metadata_edit::{MetadataPatch, edit_metadata};
pub use parser::{
    OpfData, parse_container_xml, parse_nav_landmarks, parse_nav_page_list, parse_nav_toc,
    parse_ncx, parse_opf, parse_opf_guide, strip_bom,
};
pub use unflatten_styles::{
    FlattenedStyles, Restored, StyleDiff, flattened_styles, restore_styles,
};
pub use zip_repair::neutralize_spurious_zip64;
