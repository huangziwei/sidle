//! Mechanical port of calibre's KFX → EPUB pipeline.
//!
//! Parallel path from boko's generic `KfxImporter` + `EpubExporter` IR
//! pipeline. Mirrors `ref/calibre-kfx-input/kfxlib/yj_to_epub_*.py` as
//! closely as Rust syntax allows. See `.claude/plans/kfx-to-epub-port.md`.

pub mod content;
pub mod dom;
pub mod jxr;
pub mod loader;
pub mod navigation;
pub mod output;
pub mod properties;
pub mod resources;

use std::io;

pub use loader::BookData;
pub use output::EpubOutput;

/// Failure modes for the mechanical port.
#[derive(Debug)]
pub enum ConvertError {
    InvalidKfx(String),
    JxrDecode(String),
    JpegEncode(String),
    Io(io::Error),
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvertError::InvalidKfx(m) => write!(f, "invalid KFX: {m}"),
            ConvertError::JxrDecode(m) => write!(f, "JXR decode failed: {m}"),
            ConvertError::JpegEncode(m) => write!(f, "JPEG encode failed: {m}"),
            ConvertError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for ConvertError {}

impl From<io::Error> for ConvertError {
    fn from(e: io::Error) -> Self {
        ConvertError::Io(e)
    }
}

/// Convert a KFX container in memory to a complete EPUB byte stream.
///
/// Calibre's orchestration order (`KFX_EPUB.__init__` + `decompile_to_epub`):
/// 1. organize_fragments_by_type → `BookData`
/// 2. process_content_features, process_fonts, process_document_data,
///    process_metadata, process_anchors, process_navigation
/// 3. process_reading_order — emit XHTML per section
/// 4. process_external_resource(cover) — mark cover image
/// 5. fixup_anchors_and_hrefs, update_default_font_and_language,
///    set_html_defaults, fixup_styles_and_classes, create_css_files,
///    prepare_book_parts
/// 6. zip everything into the EPUB
///
/// We follow the same shape; missing steps are TODOs in the relevant modules.
pub fn convert_to_epub(kfx_bytes: &[u8]) -> Result<Vec<u8>, ConvertError> {
    let book = loader::load(kfx_bytes)?;
    let mut out = EpubOutput::new();

    // Phase 1 step 1 — resources (images, cover).
    let resources = resources::process(&book, &mut out)?;

    // Phase 1 step 4 — content (storyline → XHTML).
    let mut content_state = content::ContentState::new(&book, &resources);
    content_state.process_reading_order()?;
    content::finalize_chapter_attrs(&mut content_state);

    // Emit stylesheet + per-section XHTML files. The stylesheet has to be
    // the same path the chapters' <link rel="stylesheet"> point at.
    let css = content::emit_stylesheet(&content_state);
    if !css.is_empty() {
        out.add_resource(
            "style.css",
            css.into_bytes(),
            "text/css",
            None,
            None,
        );
    }
    for part in &content_state.book_parts {
        let xhtml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n{}",
            part.dom.serialize(part.dom.root)
        );
        out.add_spine_chapter(&part.filename, xhtml);
    }

    // If content emission produced nothing, fall back to image scaffolding
    // so the EPUB still has something in the spine.
    if content_state.book_parts.is_empty() {
        resources::emit_image_scaffold_chapters(&mut out);
    }

    // Phase 1 step 2 — navigation. Build NCX from book_navigation.
    let toc = navigation::extract_toc(&book);
    if !toc.is_empty() {
        out.ncx_navmap = Some(navigation::render_navmap(&toc));
    }

    // Page-progression-direction comes from the document_data extractor in
    // content.rs (calibre's `yj_to_epub_metadata.py:108+131`). Propagate to
    // the OPF spine; `EpubOutput::generate_opf` suppresses the attribute when
    // the value is `ltr`.
    out.page_progression_direction = Some(content_state.page_progression_direction.clone());

    out.finalize(&book.metadata).map_err(ConvertError::Io)
}
