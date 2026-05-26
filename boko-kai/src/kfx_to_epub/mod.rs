//! Mechanical port of calibre's KFX → EPUB pipeline.
//!
//! Parallel path from boko's generic `KfxImporter` + `EpubExporter` IR
//! pipeline. Mirrors `ref/calibre-kfx-input/kfxlib/yj_to_epub_*.py` as
//! closely as Rust syntax allows.

pub mod content;
pub mod dom;
pub mod jxr;
pub mod loader;
pub mod navigation;
pub mod output;
pub mod properties;
pub mod resources;
pub mod text_index;

use std::io;

pub use loader::BookData;
pub use output::EpubOutput;
pub use text_index::TextIndex;

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
    let trace = crate::trace::Trace::new("kfx2epub", "BOKO_KFX2EPUB_TRACE");
    let book = loader::load(kfx_bytes)?;
    trace.mark("loader::load");
    let mut out = EpubOutput::new();

    // Resources (images, cover).
    let resources = resources::process(&book, &mut out)?;
    trace.mark("resources::process (JXR → JPEG)");

    // Content (storyline → XHTML).
    let mut content_state = content::ContentState::new(&book, &resources);
    content_state.process_reading_order()?;
    trace.mark("content::process_reading_order");
    // Rewrite `<a href="anchor:NAME">` placeholders (emitted by
    // `$179 link_to`) to `chapter.xhtml#anchor-id`. Must run after
    // `process_reading_order` so `element_id_to_filename` is complete.
    content::resolve_link_placeholders(&mut content_state);
    trace.mark("content::resolve_link_placeholders");
    // Calibre's div→p promotion (yj_to_epub_properties.py:1921). Must run
    // before `finalize_chapter_attrs` so the renamed `<p>` carries the same
    // `class=` / `style=` the original `<div>` accumulated.
    content::consolidate_html(&mut content_state);
    trace.mark("content::consolidate_html");
    // EOL → `<br/>` (calibre `yj_to_epub_content.py:1720`). Must run AFTER
    // `consolidate_html` so the div→p promotion sees the original text
    // shape and isn't fooled by inserted `<br/>` block-children. KFX
    // encodes forced line breaks as raw `\n` inside text segments; without
    // this pass HTML whitespace collapse silently eats them.
    content::replace_eol_with_br(&mut content_state);
    trace.mark("content::replace_eol_with_br");
    // Drop declarations that match their CSS spec default (calibre's
    // `simplify_styles` — minimal port). Has to run before
    // `fixup_styles_and_classes` so the dedupe counts identical
    // "post-pruning" style strings.
    content::simplify_styles(&mut content_state);
    trace.mark("content::simplify_styles");
    // Inline-style → class promotion. Runs before `finalize_chapter_attrs`
    // so we can rewrite the in-memory `element_styles` / `element_classes`
    // maps directly instead of mutating already-serialized attributes.
    content::fixup_styles_and_classes(&mut content_state);
    trace.mark("content::fixup_styles_and_classes");
    content::finalize_chapter_attrs(&mut content_state);
    trace.mark("content::finalize_chapter_attrs");

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
    trace.mark("content::emit_stylesheet");
    for part in &content_state.book_parts {
        let xhtml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n{}",
            part.dom.serialize(part.dom.root)
        );
        out.add_spine_chapter(&part.filename, xhtml);
    }
    trace.mark("dom serialize + add spine chapters");

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

    // Navigation. Build NCX from book_navigation, using the
    // element-id → chapter-filename map populated by `process_section` to
    // resolve `nav_unit.target_position.id` to a real chapter file.
    let toc = navigation::extract_toc(
        &book,
        &content_state.element_id_to_filename,
        &content_state.anchors,
    );
    if !toc.is_empty() {
        out.ncx_navmap = Some(navigation::render_navmap(&toc));
        out.nav_ol_html = Some(navigation::render_nav_ol(&toc));
    }
    trace.mark("navigation::extract_toc");

    // OPF `<guide>` from `nav_type=landmarks` containers (calibre's
    // `add_guide_entry` path). EPUB 2.0 readers (Apple Books, Kindle)
    // surface these as Cover / Table of Contents / Start Reading shortcuts.
    out.guide = navigation::extract_landmarks(
        &book,
        &content_state.element_id_to_filename,
        &content_state.anchors,
    );
    // When we synthesized a `titlepage.xhtml` wrapper above, repoint the
    // cover guide reference at it (calibre's convention; see
    // `ref/calibre-mobi-output/transforms/...`). KFX's CoverPage landmark
    // targets the first content chapter, but the cover the reader actually
    // *sees* is our titlepage SVG wrapper. Apple Books reads the guide
    // `type="cover"` href to render the cover page; without this rewrite
    // it ends up rendering c0.xhtml's first paragraph instead of the
    // cover image.
    if out.has_file("titlepage.xhtml") {
        if let Some(cover_ref) = out.guide.iter_mut().find(|g| g.guide_type == "cover") {
            cover_ref.href = "titlepage.xhtml".to_string();
            if cover_ref.label.is_empty() {
                cover_ref.label = "Cover".to_string();
            }
        } else {
            out.guide.insert(
                0,
                navigation::GuideRef {
                    guide_type: "cover".to_string(),
                    label: "Cover".to_string(),
                    href: "titlepage.xhtml".to_string(),
                },
            );
        }
    }
    trace.mark("navigation::extract_landmarks");

    // Page-progression-direction comes from the document_data extractor in
    // content.rs (calibre's `yj_to_epub_metadata.py:108+131`). Propagate to
    // the OPF spine; `EpubOutput::generate_opf` suppresses the attribute when
    // the value is `ltr`.
    out.page_progression_direction = Some(content_state.page_progression_direction.clone());

    // Book-level writing mode drives the `<meta name="primary-writing-mode">`
    // OPF hint for vertical books (calibre's `epub_output.py:955`).
    out.writing_mode = Some(content_state.writing_mode.clone());

    let bytes = out.finalize(&book.metadata).map_err(ConvertError::Io)?;
    trace.mark("output::finalize (zip)");
    Ok(bytes)
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
