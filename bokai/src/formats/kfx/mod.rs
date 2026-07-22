//! KFX (KF10) format reader and writer.
//!
//! KFX is Amazon's latest Kindle format, successor to KF8/AZW3.
//! It uses Amazon's Ion binary format for structured data.
//!
//! ## Module structure
//!
//! - `ion` - Amazon Ion binary format parser and writer
//! - `symbols` - KFX symbol table and enum
//! - `container` - KFX container format parsing (pure functions)
//! - `schema` - Bidirectional KFX ↔ IR mapping rules
//! - `tokens` - Token stream for import/export
//! - `storyline` - Storyline tokenization and IR building
//! - `transforms` - Attribute value transformers for bidirectional conversion
//! - `metadata` - Metadata schema for book metadata mapping
//! - `fragment` - KFX fragment representation
//! - `fxl` - Fixed-layout signal derivation (content_features, page pixel sizes)
//! - `serialization` - Binary container format serialization
//! - `context` - Export context for central state management
//! - `style_schema` - Declarative style property mapping
//! - `style_registry` - Style deduplication and ID assignment
//! - `cover` - Cover section detection and generation
//! - `auxiliary` - Auxiliary data generation for navigation targets
//! - `loader` - Container → `BookData`: every fragment parsed and grouped by type
//! - `structure` - Queries over the fragment graph, incl. `eid → base text`
//! - `navigation` - `book_navigation` walks: nav containers, anchors, TOC
//! - `error` - `KfxError`, the format-side failure type
//! - `position` - `eid → pid → device Location` maps ($265, $550/$621)
//! - `pdf_container` - PDF-backed KFX: detecting one, extracting its PDF
//! - `pdf_pages` - PDF-backed KFX: page text geometry, eids, outline, labels

pub mod anchor_table;
pub mod auxiliary;
pub mod container;
pub mod container_edit;
pub mod context;
pub mod cover;
pub mod cover_extract;
pub mod cover_replace;
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
///
/// The editing passes here rewrite a container in place — a metadata field, a
/// TOC, a cover resource, one spliced entity — and the failure they all risk
/// is the same: bytes that parse well enough for the edit's own assertions
/// while no longer loading as a book. Converting is the cheapest end-to-end
/// statement that the container survived.
///
/// Test-only. The format layer does not depend on `import`/`export` in
/// shipping code; test modules are exempt from that rule.
#[cfg(test)]
pub(crate) fn converts_to_epub(kfx: &[u8]) -> bool {
    let Ok(mut book) = crate::model::Book::from_bytes(kfx, crate::model::Format::Kfx) else {
        return false;
    };
    let mut sink = std::io::Cursor::new(Vec::new());
    book.export(crate::model::Format::Epub, &mut sink).is_ok()
}
