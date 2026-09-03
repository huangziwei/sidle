//! KFX (KF10) format reader and writer.
//!
//! KFX is Amazon's latest Kindle format, successor to KF8/AZW3.

pub mod anchor_table;
pub mod auxiliary;
pub mod container;
pub mod container_edit;
pub mod context;
pub mod cover;
pub mod cover_extract;
pub mod cover_replace;
pub mod diff;
pub mod error;
pub mod fragment;
pub mod fxl;
pub mod image_extract;
pub mod ion;
pub mod jxr;
pub mod loader;
pub mod merge;
pub mod metadata;
pub mod metadata_edit;
pub mod navigation;
pub mod package;
pub mod pdf_container;
pub mod pdf_pages;
pub mod position;
pub mod resource_index;
pub mod schema;
pub mod serialization;
pub mod storyline;
pub mod structure;
pub mod style_registry;
pub mod style_schema;
pub mod symbols;
pub mod toc_repair;
pub mod tokens;
pub mod transforms;
pub mod writing_mode;
pub mod yj_properties;

/// Does this KFX still open and convert to EPUB?
#[cfg(test)]
pub(crate) fn converts_to_epub(kfx: &[u8]) -> bool {
    let Ok(mut book) = crate::model::Book::from_bytes(kfx, crate::model::Format::Kfx) else {
        return false;
    };
    let mut sink = std::io::Cursor::new(Vec::new());
    book.export(crate::model::Format::Epub, &mut sink).is_ok()
}
