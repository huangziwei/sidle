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
    // Rewrite `<a href="anchor:NAME">` placeholders (emitted by
    // `$179 link_to`) to `chapter.xhtml#anchor-id`. Must run after
    // `process_reading_order` so `element_id_to_filename` is complete.
    content::resolve_link_placeholders(&mut content_state);
    // Calibre's div→p promotion (yj_to_epub_properties.py:1921). Must run
    // before `finalize_chapter_attrs` so the renamed `<p>` carries the same
    // `class=` / `style=` the original `<div>` accumulated.
    content::consolidate_html(&mut content_state);
    // Drop declarations that match their CSS spec default (calibre's
    // `simplify_styles` — minimal port). Has to run before
    // `fixup_styles_and_classes` so the dedupe counts identical
    // "post-pruning" style strings.
    content::simplify_styles(&mut content_state);
    // Inline-style → class promotion. Runs before `finalize_chapter_attrs`
    // so we can rewrite the in-memory `element_styles` / `element_classes`
    // maps directly instead of mutating already-serialized attributes.
    content::fixup_styles_and_classes(&mut content_state);
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

    // Cover titlepage wrapper: matches calibre's `titlepage.xhtml` — an SVG
    // viewBox sized to the cover image so readers (Apple Books, Kindle, etc.)
    // render the cover at the right aspect ratio. Inserted at the FRONT of
    // the spine so it's what opens when a reader picks up the book.
    if let Some(titlepage) = build_titlepage(&out) {
        out.prepend_spine_chapter("titlepage.xhtml", titlepage);
    }

    // Phase 1 step 2 — navigation. Build NCX from book_navigation, using the
    // element-id → chapter-filename map populated by `process_section` to
    // resolve `nav_unit.target_position.id` to a real chapter file.
    let toc = navigation::extract_toc(
        &book,
        &content_state.element_id_to_filename,
        &content_state.anchors,
    );
    if !toc.is_empty() {
        out.ncx_navmap = Some(navigation::render_navmap(&toc));
    }

    // Page-progression-direction comes from the document_data extractor in
    // content.rs (calibre's `yj_to_epub_metadata.py:108+131`). Propagate to
    // the OPF spine; `EpubOutput::generate_opf` suppresses the attribute when
    // the value is `ltr`.
    out.page_progression_direction = Some(content_state.page_progression_direction.clone());

    // Book-level writing mode drives the `<meta name="primary-writing-mode">`
    // OPF hint for vertical books (calibre's `epub_output.py:955`).
    out.writing_mode = Some(content_state.writing_mode.clone());

    out.finalize(&book.metadata).map_err(ConvertError::Io)
}

/// Build calibre-style `titlepage.xhtml`: an SVG viewBox sized to the
/// cover image's pixel dimensions, with the JPEG referenced via
/// `xlink:href`. Returns `None` if no cover image was bundled. The
/// `<meta name="calibre:cover" content="true"/>` marker matches calibre's
/// output so cover-aware readers identify the page as a title page rather
/// than the first content page.
fn build_titlepage(out: &EpubOutput) -> Option<String> {
    let (href, width, height) = out.cover_image_info()?;
    let w = width.unwrap_or(0);
    let h = height.unwrap_or(0);
    if w == 0 || h == 0 {
        // Without dimensions the viewBox would collapse; fall back to a
        // bare image wrapper rather than emit a zero-size SVG.
        return Some(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE html>\n\
             <html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"en\">\n\
             <head>\n\
             <meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"/>\n\
             <meta name=\"calibre:cover\" content=\"true\"/>\n\
             <title>Cover</title>\n\
             </head>\n\
             <body><div><img src=\"{href}\" alt=\"\"/></div></body>\n\
             </html>\n"
        ));
    }
    Some(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE html>\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"en\">\n\
         <head>\n\
         <meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"/>\n\
         <meta name=\"calibre:cover\" content=\"true\"/>\n\
         <title>Cover</title>\n\
         <style type=\"text/css\" title=\"override_css\">\n\
         @page {{padding: 0pt; margin:0pt}}\n\
         body {{ text-align: center; padding:0pt; margin: 0pt; }}\n\
         </style>\n\
         </head>\n\
         <body>\n\
         <div>\n\
         <svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" version=\"1.1\" width=\"100%\" height=\"100%\" viewBox=\"0 0 {w} {h}\" preserveAspectRatio=\"none\">\n\
         <image width=\"{w}\" height=\"{h}\" xlink:href=\"{href}\"/>\n\
         </svg>\n\
         </div>\n\
         </body>\n\
         </html>\n"
    ))
}
