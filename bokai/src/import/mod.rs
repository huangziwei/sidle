//! Format importers for reading ebook files.
//!
//! The `Importer` trait defines a two-track interface:

mod azw3;
mod epub;
mod kfx;
mod mobi;
#[cfg(feature = "pdf")]
pub mod pdf;

pub use azw3::Azw3Importer;
pub use epub::EpubImporter;
pub use kfx::KfxImporter;
pub use mobi::MobiImporter;
#[cfg(feature = "pdf")]
pub use pdf::{PdfDoc, PdfOutlineItem, PdfPage, probe_pdf};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::html::{Origin, Stylesheet, compile_dom, extract_stylesheets_from_dom, parse_dom};
use crate::model::{
    AnchorTarget, Chapter, FontFace, GlobalNodeId, Landmark, Metadata, PositionMap, SourceText,
    TocEntry,
};
use crate::style::CssDecl;

// Chapter identity is IR vocabulary: the format layer and the exporters name
// it too. It lives in `model` and is re-exported here.
pub use crate::model::ChapterId;

/// Entry in the reading order (spine).
#[derive(Debug, Clone)]
pub struct SpineEntry {
    /// Unique identifier for this chapter.
    pub id: ChapterId,
    /// Estimated size in bytes (for progress indication).
    pub size_estimate: usize,
    /// For a fixed-layout book, which half of the two-page spread this page
    /// occupies (source's declared `page-spread-*`). `None` for reflowable
    /// books or FXL pages with no declared side. See [`crate::model::PageSpread`].
    pub page_spread: Option<crate::model::PageSpread>,
    /// For a fixed-layout page, its pixel viewport `(width, height)` — the
    /// `<meta name="viewport">` box the page is authored to. `None` for
    /// reflowable documents.
    pub viewport: Option<(u32, u32)>,
    /// The author-drawn comic panels on this page, in `ordinal` order. Empty
    /// for a page the source gave none. See [`crate::model::Panel`].
    pub panels: Vec<crate::model::Panel>,
}

/// One asset described without loading it.
#[derive(Debug, Clone)]
pub struct AssetInfo {
    /// Path as [`Importer::load_asset`] takes it.
    pub path: PathBuf,
    /// *Predicted* media type. The loaded bytes are the authority — a
    /// transcode that fails passes the source type through.
    pub media_type: String,
    /// Declared pixel size, when the source states one. `None` means unknown,
    /// not absent: the bytes may decode to an image.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// A source format's contribution to the normalized stylesheet: every named
#[derive(Debug, Default)]
pub struct CssProgram {
    /// Raw source style name → converted declarations. A node whose
    /// `semantics.class` names an entry with a non-empty declaration gets a
    /// sanitized class attribute in synthesized XHTML.
    pub named: HashMap<String, CssDecl>,
    /// Raw source style name → the state-conditional rules that style carries,
    pub pseudo: HashMap<String, Vec<(String, CssDecl)>>,
    /// Doc-level CSS writing mode (`horizontal-tb` emits no body rule).
    pub writing_mode: String,
    /// Image-based fixed-layout book (viewport-fit reset header).
    pub fixed_layout: bool,
}

/// Polymorphic interface for format-specific backends.
///
/// Implementors provide access to book content via two tracks:
pub trait Importer: Send + Sync {
    // --- Lifecycle ---

    /// Open a file and parse structure (metadata, TOC, spine).
    fn open(path: &Path) -> std::io::Result<Self>
    where
        Self: Sized;

    /// Book metadata (title, authors, etc.).
    fn metadata(&self) -> &Metadata;

    /// Table of contents.
    fn toc(&self) -> &[TocEntry];

    /// Landmarks (structural navigation points like cover, start reading location).
    fn landmarks(&self) -> &[Landmark];

    /// Reading order (spine).
    fn spine(&self) -> &[SpineEntry];

    // --- Track 1: Normalization ---

    /// Load a chapter as normalized IR.
    ///
    /// The default implementation:
    fn load_chapter(&mut self, id: ChapterId) -> std::io::Result<Chapter> {
        // Load raw HTML
        let html_bytes = self.load_raw(id)?;
        let hint_encoding = crate::util::extract_xml_encoding(&html_bytes);
        let html_str = crate::util::decode_text(&html_bytes, hint_encoding);

        // Parse the chapter DOM once; it is shared by stylesheet discovery and
        // IR compilation below (avoids a second decode + full parse).
        let dom = parse_dom(&html_str);

        // Extract stylesheet references from the parsed DOM
        let (linked, inline) = extract_stylesheets_from_dom(&dom);

        // Build stylesheets list
        let mut stylesheets = Vec::new();

        // Load linked stylesheets
        for href in linked {
            // Resolve relative path based on chapter's source path
            let css_path = if let Some(chapter_path) = self.source_id(id) {
                resolve_relative_path(chapter_path, &href)
            } else {
                PathBuf::from(crate::util::percent_decode(&href))
            };

            if let Some(mut sheet) = self.load_stylesheet(&css_path) {
                // A linked sheet's `url()`s are relative to the sheet itself.
                let base = css_path.to_string_lossy().replace('\\', "/");
                sheet.resolve_asset_urls(|src| resolve_asset_url(&base, src));
                stylesheets.push((sheet, Origin::Author));
            }
        }

        // Inline styles resolve relative to the document.
        let inline_base = self.source_id(id).map(|p| p.to_string());
        for css in inline {
            let mut sheet = Stylesheet::parse(&css);
            if let Some(base) = &inline_base {
                sheet.resolve_asset_urls(|src| resolve_asset_url(base, src));
            }
            stylesheets.push((sheet, Origin::Author));
        }

        // Compile the parsed DOM to IR
        let mut chapter = compile_dom(&dom, &stylesheets);

        // Post-process: Resolve relative paths in semantic attributes (src, href)
        if let Some(base_path) = self.source_id(id) {
            resolve_semantic_paths(&mut chapter, base_path);
        }

        Ok(chapter)
    }

    /// Load several chapters, one result per input id. Implementations
    fn load_chapters(&mut self, ids: &[ChapterId]) -> Vec<std::io::Result<Chapter>> {
        ids.iter().map(|id| self.load_chapter(*id)).collect()
    }

    /// Cap the worker threads this importer's parallel stages may run at
    /// once, `0` for the platform's reported parallelism. The default ignores
    /// it: an importer with no parallel stage has nothing to bound.
    fn set_max_workers(&mut self, _workers: usize) {}

    // --- Track 2: Raw Access (The Converter) ---

    /// Returns the internal source path for a chapter (e.g., "OEBPS/text/ch01.xhtml").
    fn source_id(&self, id: ChapterId) -> Option<&str>;

    /// The document `<title>` for a spine chapter. Defaults to the source id;
    fn chapter_title(&self, id: ChapterId) -> Option<&str> {
        self.source_id(id)
    }

    /// Returns the raw bytes of a chapter.
    fn load_raw(&mut self, id: ChapterId) -> std::io::Result<Vec<u8>>;

    // --- Assets ---

    /// List all assets (images, fonts, CSS, etc.).
    fn list_assets(&self) -> &[PathBuf];

    /// Load an asset by path.
    fn load_asset(&mut self, path: &Path) -> std::io::Result<Vec<u8>>;

    /// Load several assets, one result per input path.
    fn load_assets(&mut self, paths: &[PathBuf]) -> Vec<std::io::Result<Vec<u8>>> {
        paths.iter().map(|p| self.load_asset(p)).collect()
    }

    /// The same assets as the source stores them, each with its declared
    /// format. [`Self::load_assets`] re-encodes what it stores in a format
    /// no common decoder reads.
    fn load_assets_stored(
        &mut self,
        paths: &[PathBuf],
    ) -> Vec<std::io::Result<(Vec<u8>, Option<String>)>> {
        self.load_assets(paths)
            .into_iter()
            .map(|bytes| bytes.map(|bytes| (bytes, None)))
            .collect()
    }

    /// The authoritative asset list for a normalized EPUB export, in
    fn bundled_assets(&self) -> Option<Vec<PathBuf>> {
        None
    }

    /// [`Self::bundled_assets`] with each entry's predicted media type and
    fn asset_manifest(&mut self) -> Option<Vec<AssetInfo>> {
        None
    }

    /// Load and parse a stylesheet, optionally using a cache.
    ///
    /// The default implementation loads the asset bytes and parses CSS.
    fn load_stylesheet(&mut self, path: &Path) -> Option<Stylesheet> {
        if let Ok(css_bytes) = self.load_asset(path) {
            let css_str = String::from_utf8_lossy(&css_bytes);
            return Some(Stylesheet::parse(&css_str));
        }
        None
    }

    /// Collect all @font-face definitions from CSS files.
    fn font_faces(&mut self) -> Vec<FontFace> {
        let mut font_faces = Vec::new();

        // Find all CSS files
        let css_paths: Vec<_> = self
            .list_assets()
            .iter()
            .filter(|p| {
                p.extension()
                    .map(|e| e.eq_ignore_ascii_case("css"))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();

        for css_path in css_paths {
            if let Some(stylesheet) = self.load_stylesheet(&css_path) {
                // Resolve relative font paths to canonical paths
                for mut font_face in stylesheet.font_faces {
                    // Resolve the src path relative to the CSS file location
                    let resolved =
                        resolve_relative_path(css_path.to_string_lossy().as_ref(), &font_face.src);
                    // Normalize to forward slashes for archive paths.
                    font_face.src = resolved.to_string_lossy().replace('\\', "/");
                    font_faces.push(font_face);
                }
            }
        }

        font_faces
    }

    /// The book's reading-position scale, when the source defines one.
    fn position_map(&mut self) -> Option<PositionMap> {
        None
    }

    /// The source's own base text, keyed by the same element ids
    /// [`Self::position_map`] places — the substrate a physically-addressed
    /// annotation slices to recover the words it covers. See [`SourceText`].
    fn source_text(&mut self) -> Option<SourceText> {
        None
    }

    /// Whether this importer requires normalized export for HTML-based formats.
    ///
    /// Returns true for binary formats (KFX) where load_raw returns non-HTML data.
    fn requires_normalized_export(&self) -> bool {
        false
    }

    // --- Link Resolution ---

    /// Index all anchor targets after chapters are loaded.
    fn index_anchors(&mut self, _chapters: &[(ChapterId, Arc<Chapter>)]) {
        // Default: no-op. Path-based resolution in resolve_href() handles EPUB.
        // Format-specific importers override to build their anchor maps.
    }

    /// Resolve TOC href fragments after chapters are loaded.
    fn resolve_toc(&mut self) {
        // Default: no-op. EPUB and KFX have correct TOC hrefs from source.
    }

    /// Get mutable access to TOC entries for resolution.
    fn toc_mut(&mut self) -> &mut [TocEntry];

    /// Physical page-break list (EPUB 3 `<nav epub:type="page-list">`), mapping
    fn page_list(&self) -> &[TocEntry] {
        &[]
    }

    /// Mutable page-list access, for `Book::resolve_page_list_targets` to
    /// fill in each entry's `target`. Empty by default.
    fn page_list_mut(&mut self) -> &mut [TocEntry] {
        &mut []
    }

    /// Resolve an href to its target.
    ///
    /// Handles format-specific href parsing and resolution.
    fn resolve_href(&self, _from_chapter: ChapterId, href: &str) -> Option<AnchorTarget> {
        let href = href.trim();

        // External URLs
        if href.starts_with("http://")
            || href.starts_with("https://")
            || href.starts_with("mailto:")
            || href.starts_with("tel:")
        {
            return Some(AnchorTarget::External(href.to_string()));
        }

        None
    }

    /// Resolve a navigation href (TOC / page-list / landmarks) to its target.
    fn resolve_toc_href(&self, from_chapter: ChapterId, href: &str) -> Option<AnchorTarget> {
        self.resolve_href(from_chapter, href)
    }

    /// The fragment id a navigation href should carry in normalized export,
    /// plus whether that id was actually stamped into a loaded chapter.
    fn nav_fragment(&self, _href: &str) -> Option<(String, bool)> {
        None
    }

    /// The source's named-style program for normalized export: every named
    /// style converted to CSS declarations, plus the doc-level writing mode
    /// and fixed-layout flag the stylesheet header needs.
    fn stylesheet_program(&mut self) -> Option<CssProgram> {
        None
    }

    /// The axis the document states it is written along. A format carrying a
    /// writing mode per element leaves this at the initial value; one stating
    /// it once for the book, as `document_data.writing_mode` does, answers.
    fn writing_mode(&mut self) -> crate::style::WritingMode {
        crate::style::WritingMode::HorizontalTb
    }
}

/// `(width, height)` from `html`'s `<meta name="viewport" content="width=1800,
/// height=2700">`. `None` when `html` declares no viewport, or omits a number.
pub fn viewport_meta(html: &str) -> Option<(u32, u32)> {
    let cut = (0..=html.len().min(4096))
        .rev()
        .find(|&i| html.is_char_boundary(i))?;
    let head = &html[..cut];
    let at = head
        .find("name=\"viewport\"")
        .or_else(|| head.find("name='viewport'"))?;
    let (before, after) = head.split_at(at);
    // `content` may precede or follow `name` on the same element.
    let tag_start = before.rfind('<')?;
    let tag_end = at + after.find('>')?;
    let tag = &head[tag_start..tag_end];
    let content_at = tag.find("content=")? + "content=".len();
    let quoted = &tag[content_at..];
    let quote = quoted.chars().next()?;
    let body = &quoted[quote.len_utf8()..];
    let value = &body[..body.find(quote)?];
    let mut width = None;
    let mut height = None;
    for part in value.split(',') {
        let Some((key, num)) = part.split_once('=') else {
            continue;
        };
        let Ok(num) = num.trim().parse() else {
            continue;
        };
        match key.trim() {
            "width" => width = Some(num),
            "height" => height = Some(num),
            _ => {}
        }
    }
    Some((width?, height?))
}

/// Helper for path-based href resolution (used by EPUB, AZW3, MOBI).
pub fn resolve_path_based_href(
    from_path: &str,
    href: &str,
    chapter_for_path: impl Fn(&str) -> Option<ChapterId>,
    anchor: impl Fn(&str) -> Option<GlobalNodeId>,
    chapter_fallback: bool,
) -> Option<AnchorTarget> {
    let href = href.trim();

    // External URLs
    if href.starts_with("http://")
        || href.starts_with("https://")
        || href.starts_with("mailto:")
        || href.starts_with("tel:")
    {
        return Some(AnchorTarget::External(href.to_string()));
    }

    // Fragment-only link (#id) - same chapter
    if let Some(fragment) = href.strip_prefix('#') {
        let key = format!("{}#{}", from_path, fragment);
        if let Some(target) = anchor(&key) {
            return Some(AnchorTarget::Internal(target));
        }
        return None;
    }

    // Split path and fragment
    let (raw_path, fragment) = if let Some(hash_pos) = href.find('#') {
        (&href[..hash_pos], Some(&href[hash_pos + 1..]))
    } else {
        (href, None)
    };

    // Collapse `.` / `..`: a href like `OEBPS/Text/../Text/Ch01.xhtml`
    let normalized_path = normalize_components(Path::new(raw_path));
    let normalized_path = normalized_path.to_string_lossy();
    let path: &str = &normalized_path;

    // Look up target chapter
    let target_chapter = chapter_for_path(path)?;

    // If there's a fragment, resolve to specific node
    if let Some(frag) = fragment {
        let key = format!("{}#{}", path, frag);
        if let Some(target) = anchor(&key) {
            return Some(AnchorTarget::Internal(target));
        }
        // A dead fragment in a real chapter: `chapter_fallback` lands navigation
        // on the chapter start, and leaves an in-text link unresolved.
        if chapter_fallback {
            return Some(AnchorTarget::Chapter(target_chapter));
        }
        return None;
    }

    // No fragment - link to chapter start
    Some(AnchorTarget::Chapter(target_chapter))
}

/// Resolve a relative path against a base path.
fn resolve_relative_path(base: &str, relative: &str) -> PathBuf {
    // Hierarchical URLs (http://, https://, …) are not archive paths — leave
    // them untouched. Scheme-only URIs like mailto:/data: are filtered out by
    // the callers (resolve_semantic_paths) before they reach here.
    if relative.contains("://") {
        return PathBuf::from(relative);
    }

    // The href/src is a URI reference. Percent-decoding it matches the
    // archive's literal zip entry names; `base` is a decoded archive path,
    // and the joined result stays in decoded space.
    let relative = crate::util::percent_decode(relative);
    let relative = relative.as_str();

    // Handle absolute archive paths
    if relative.starts_with('/') {
        return PathBuf::from(relative);
    }

    // Handle fragment-only paths (#anchor) - resolve to base file + fragment
    if relative.starts_with('#') {
        return PathBuf::from(format!("{}{}", base, relative));
    }

    // Get the directory of the base path
    let base_path = Path::new(base);
    let base_dir = base_path.parent().unwrap_or(Path::new(""));

    // Join and collapse `..` / `.`; the result matches the canonical
    // archive entry (PathBuf::join does not normalize on its own).
    normalize_components(&base_dir.join(relative))
}

/// Collapse `.` and `..` components in a path. `PathBuf::join` appends
pub(crate) fn normalize_components(p: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in p.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::Normal(name) => {
                result.push(name);
            }
            std::path::Component::CurDir => {}
            std::path::Component::RootDir => {
                result.push("/");
            }
            std::path::Component::Prefix(prefix) => {
                result.push(prefix.as_os_str());
            }
        }
    }
    result
}

/// Resolve relative paths in a chapter's semantic attributes.
fn resolve_asset_url(base_path: &str, url: &str) -> String {
    if url.contains("://") || url.starts_with("data:") {
        return url.to_string();
    }
    resolve_relative_path(base_path, url)
        .to_string_lossy()
        .replace('\\', "/")
}

fn resolve_semantic_paths(chapter: &mut Chapter, base_path: &str) {
    chapter.semantics.resolve_paths(|path| {
        // Skip external URLs and data URIs
        if path.contains("://") || path.starts_with("data:") {
            return path.to_string();
        }

        // Resolve relative path to absolute archive path
        let resolved = resolve_relative_path(base_path, path);
        // Normalize to forward slashes (archive paths, not filesystem paths)
        resolved.to_string_lossy().replace('\\', "/")
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Landmark, Metadata, TocEntry};
    use proptest::prelude::*;
    use std::collections::HashMap;
    use std::io;

    #[test]
    fn test_resolve_fragment_only_path() {
        // Fragment-only paths should resolve to base + fragment
        let result = resolve_relative_path("f_0004.xhtml", "#FOOTNOTE-1");
        assert_eq!(result.to_string_lossy(), "f_0004.xhtml#FOOTNOTE-1");

        let result = resolve_relative_path("OEBPS/text/chapter.xhtml", "#anchor");
        assert_eq!(result.to_string_lossy(), "OEBPS/text/chapter.xhtml#anchor");
    }

    #[test]
    fn test_resolve_relative_path_with_fragment() {
        // Relative paths with fragments should resolve normally
        let result = resolve_relative_path("text/ch1.xhtml", "ch2.xhtml#section");
        // Normalize path separators for cross-platform comparison
        let normalized: String = result.to_string_lossy().replace('\\', "/");
        assert_eq!(normalized, "text/ch2.xhtml#section");
    }

    #[test]
    fn test_resolve_parent_directory() {
        let result = resolve_relative_path("OEBPS/text/ch01.xhtml", "../styles/main.css");
        // Normalize path separators for cross-platform comparison
        let normalized: String = result.to_string_lossy().replace('\\', "/");
        assert_eq!(normalized, "OEBPS/styles/main.css");
    }

    #[test]
    fn test_resolve_absolute_path_unchanged() {
        let result = resolve_relative_path("text/chapter.xhtml", "/absolute/path.css");
        assert_eq!(result.to_string_lossy(), "/absolute/path.css");
    }

    #[test]
    fn test_resolve_url_unchanged() {
        let result = resolve_relative_path("text/chapter.xhtml", "https://example.com/");
        assert_eq!(result.to_string_lossy(), "https://example.com/");
    }

    #[test]
    fn test_dead_fragment_chapter_fallback() {
        use crate::model::{AnchorTarget, GlobalNodeId, NodeId};
        // `chapter_for` names the file as a real chapter; `dead_anchor` gives
        // its fragment no target.
        let chapter_for = |p: &str| (p == "text/part0001.html").then_some(ChapterId(3));
        let dead_anchor = |_k: &str| None;

        // In-text link (strict): a dead fragment stays unresolved.
        assert_eq!(
            resolve_path_based_href(
                "text/x.html",
                "text/part0001.html#UGI0",
                chapter_for,
                dead_anchor,
                false,
            ),
            None,
        );
        // Navigation (fallback): a dead fragment lands at the chapter start.
        assert_eq!(
            resolve_path_based_href(
                "text/x.html",
                "text/part0001.html#UGI0",
                chapter_for,
                dead_anchor,
                true,
            ),
            Some(AnchorTarget::Chapter(ChapterId(3))),
        );
        // A fragment that DOES resolve is unaffected by the flag.
        let live_anchor = |k: &str| {
            (k == "text/part0001.html#printhead1")
                .then_some(GlobalNodeId::new(ChapterId(3), NodeId::ROOT))
        };
        assert_eq!(
            resolve_path_based_href(
                "text/x.html",
                "text/part0001.html#printhead1",
                chapter_for,
                live_anchor,
                false,
            ),
            Some(AnchorTarget::Internal(GlobalNodeId::new(
                ChapterId(3),
                NodeId::ROOT
            ))),
        );
    }

    #[test]
    fn test_load_chapter_stylesheet_cache() {
        struct TestImporter {
            chapters: HashMap<u32, String>,
            assets: HashMap<String, Vec<u8>>,
            asset_list: Vec<PathBuf>,
            css_cache: HashMap<String, Stylesheet>,
            css_loads: usize,
            metadata: Metadata,
            toc: Vec<TocEntry>,
            landmarks: Vec<Landmark>,
            spine: Vec<SpineEntry>,
            source_ids: Vec<String>,
        }

        impl Importer for TestImporter {
            fn open(_path: &Path) -> io::Result<Self> {
                unreachable!()
            }

            fn metadata(&self) -> &Metadata {
                &self.metadata
            }

            fn toc(&self) -> &[TocEntry] {
                &self.toc
            }

            fn toc_mut(&mut self) -> &mut [TocEntry] {
                &mut self.toc
            }

            fn landmarks(&self) -> &[Landmark] {
                &self.landmarks
            }

            fn spine(&self) -> &[SpineEntry] {
                &self.spine
            }

            fn source_id(&self, id: ChapterId) -> Option<&str> {
                self.source_ids.get(id.0 as usize).map(|s| s.as_str())
            }

            fn load_raw(&mut self, id: ChapterId) -> io::Result<Vec<u8>> {
                self.chapters
                    .get(&id.0)
                    .map(|s| s.as_bytes().to_vec())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "chapter not found"))
            }

            fn list_assets(&self) -> &[PathBuf] {
                &self.asset_list
            }

            fn load_asset(&mut self, path: &Path) -> io::Result<Vec<u8>> {
                let key = path.to_string_lossy().replace('\\', "/");
                self.assets
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "asset not found"))
            }

            fn load_stylesheet(&mut self, path: &Path) -> Option<Stylesheet> {
                let key = path.to_string_lossy().replace('\\', "/");
                if let Some(sheet) = self.css_cache.get(&key) {
                    return Some(sheet.clone());
                }
                let css_bytes = self.load_asset(path).ok()?;
                let css_str = String::from_utf8_lossy(&css_bytes);
                let sheet = Stylesheet::parse(&css_str);
                self.css_cache.insert(key, sheet.clone());
                self.css_loads += 1;
                Some(sheet)
            }
        }

        let mut importer = TestImporter {
            chapters: HashMap::from([
                (
                    0,
                    r#"<html><head><link rel="stylesheet" href="style.css"></head><body>One</body></html>"#
                        .to_string(),
                ),
                (
                    1,
                    r#"<html><head><link rel="stylesheet" href="style.css"></head><body>Two</body></html>"#
                        .to_string(),
                ),
            ]),
            assets: HashMap::from([(
                "text/style.css".to_string(),
                b"p { color: red; }".to_vec(),
            )]),
            asset_list: vec![PathBuf::from("text/style.css")],
            css_cache: HashMap::new(),
            css_loads: 0,
            metadata: Metadata::default(),
            toc: Vec::new(),
            landmarks: Vec::new(),
            spine: vec![
                SpineEntry {
                    id: ChapterId(0),
                    size_estimate: 0,
                    page_spread: None,
                    viewport: None,
                    panels: Vec::new(),
                },
                SpineEntry {
                    id: ChapterId(1),
                    size_estimate: 0,
                    page_spread: None,
                    viewport: None,
                    panels: Vec::new(),
                },
            ],
            source_ids: vec!["text/ch1.xhtml".to_string(), "text/ch2.xhtml".to_string()],
        };

        let _ = importer.load_chapter(ChapterId(0)).unwrap();
        let _ = importer.load_chapter(ChapterId(1)).unwrap();

        assert_eq!(importer.css_loads, 1);
    }

    #[test]
    fn test_font_faces_uses_load_stylesheet() {
        struct TestImporter {
            asset_list: Vec<PathBuf>,
            metadata: Metadata,
            toc: Vec<TocEntry>,
            landmarks: Vec<Landmark>,
            spine: Vec<SpineEntry>,
        }

        impl Importer for TestImporter {
            fn open(_path: &Path) -> io::Result<Self> {
                unreachable!()
            }

            fn metadata(&self) -> &Metadata {
                &self.metadata
            }

            fn toc(&self) -> &[TocEntry] {
                &self.toc
            }

            fn toc_mut(&mut self) -> &mut [TocEntry] {
                &mut self.toc
            }

            fn landmarks(&self) -> &[Landmark] {
                &self.landmarks
            }

            fn spine(&self) -> &[SpineEntry] {
                &self.spine
            }

            fn source_id(&self, _id: ChapterId) -> Option<&str> {
                None
            }

            fn load_raw(&mut self, _id: ChapterId) -> io::Result<Vec<u8>> {
                Err(io::Error::other("unused"))
            }

            fn list_assets(&self) -> &[PathBuf] {
                &self.asset_list
            }

            fn load_asset(&mut self, _path: &Path) -> io::Result<Vec<u8>> {
                Err(io::Error::other("load_asset should not be called"))
            }

            fn load_stylesheet(&mut self, _path: &Path) -> Option<Stylesheet> {
                let css = "@font-face { font-family: Test; src: url(../fonts/test.woff); }";
                Some(Stylesheet::parse(css))
            }
        }

        let mut importer = TestImporter {
            asset_list: vec![PathBuf::from("styles/main.css")],
            metadata: Metadata::default(),
            toc: Vec::new(),
            landmarks: Vec::new(),
            spine: Vec::new(),
        };

        let fonts = importer.font_faces();
        assert_eq!(fonts.len(), 1);
        assert_eq!(fonts[0].font_family, "Test");
        assert_eq!(fonts[0].src, "fonts/test.woff");
    }

    proptest! {
        #[test]
        fn prop_resolve_relative_path_preserves_fragment_and_no_backslashes(
            base_parts in prop::collection::vec("[a-z]{1,8}", 1..5),
            target_parts in prop::collection::vec("[a-z]{1,8}", 1..5),
            fragment in "[A-Za-z0-9_-]{1,12}",
            up_levels in 0usize..3
        ) {
            // Build a base like "dir/sub/chapter.xhtml"
            let mut base = base_parts.join("/");
            base.push_str("/chapter.xhtml");

            // Build a relative target like "../a/b.xhtml#frag"
            let mut target = String::new();
            for _ in 0..up_levels {
                target.push_str("../");
            }
            target.push_str(&target_parts.join("/"));
            target.push_str(".xhtml#");
            target.push_str(&fragment);

            let resolved = resolve_relative_path(&base, &target);
            let normalized = resolved.to_string_lossy().replace('\\', "/");

            // Fragment preserved.
            let expected_fragment = format!("#{}", fragment);
            prop_assert!(normalized.ends_with(&expected_fragment));
            // Archive paths should be normalized to forward slashes.
            prop_assert!(!normalized.contains('\\'));
        }

        #[test]
        fn prop_resolve_relative_path_preserves_absolute_and_urls(
            base_parts in prop::collection::vec("[a-z]{1,8}", 1..5),
            absolute in "[A-Za-z0-9/_\\-]{1,24}",
            path in "[A-Za-z0-9/_\\-]{1,24}",
        ) {
            let mut base = base_parts.join("/");
            base.push_str("/chapter.xhtml");

            let absolute_path = format!("/{}", absolute);
            let url = format!("https://example.com/{}", path);

            let resolved_abs = resolve_relative_path(&base, &absolute_path);
            prop_assert_eq!(resolved_abs.to_string_lossy(), absolute_path);

            let resolved_url = resolve_relative_path(&base, &url);
            prop_assert_eq!(resolved_url.to_string_lossy(), url);
        }

        #[test]
        fn prop_resolve_relative_path_eliminates_dotdot(
            base_parts in prop::collection::vec("[a-z]{1,8}", 2..5),
            target_parts in prop::collection::vec("[a-z]{1,8}", 1..4),
            up_levels in 0usize..2
        ) {
            let mut base = base_parts.join("/");
            base.push_str("/chapter.xhtml");

            let mut target = String::new();
            for _ in 0..up_levels {
                target.push_str("../");
            }
            target.push_str(&target_parts.join("/"));
            target.push_str(".xhtml");

            let resolved = resolve_relative_path(&base, &target);
            let normalized = resolved.to_string_lossy().replace('\\', "/");

            prop_assert!(!normalized.contains("/../"));
        }

        #[test]
        fn prop_resolve_fragment_only_appends_to_base(
            base_parts in prop::collection::vec("[a-z]{1,8}", 1..5),
            fragment in "[A-Za-z0-9_-]{1,12}"
        ) {
            let mut base = base_parts.join("/");
            base.push_str("/chapter.xhtml");

            let target = format!("#{}", fragment);
            let resolved = resolve_relative_path(&base, &target);
            let normalized = resolved.to_string_lossy().replace('\\', "/");

            let expected = format!("{}#{}", base, fragment);
            prop_assert_eq!(normalized, expected);
        }
    }
}

#[cfg(test)]
mod viewport_tests {
    use super::viewport_meta;

    /// A page states its pixel box in `<meta name="viewport">`, in either
    /// attribute order and either quoting.
    #[test]
    fn a_declared_viewport_reads_as_a_pixel_box() {
        assert_eq!(
            viewport_meta(r#"<head><meta content="width=2048, height=1456" name="viewport"/>"#),
            Some((2048, 1456))
        );
        assert_eq!(
            viewport_meta(r#"<meta name="viewport" content="width=1800, height=2700"/>"#),
            Some((1800, 2700))
        );
        assert_eq!(
            viewport_meta(r#"<meta name='viewport' content='width=900, height=1200'/>"#),
            Some((900, 1200))
        );
    }

    /// A page that states no viewport, or states one without both numbers, has
    /// no pixel box.
    #[test]
    fn an_undeclared_or_partial_viewport_is_none() {
        assert_eq!(viewport_meta("<head><title>x</title></head>"), None);
        assert_eq!(
            viewport_meta(r#"<meta name="viewport" content="width=device-width"/>"#),
            None
        );
    }

    /// The head scan stops at a byte count; a multi-byte character spanning the
    /// cut keeps the slice on a character boundary.
    #[test]
    fn a_multibyte_head_does_not_split() {
        let pad = "ã".repeat(2000);
        assert_eq!(viewport_meta(&pad), None);
        let doc = format!(r#"<meta name="viewport" content="width=10, height=20"/>{pad}"#);
        assert_eq!(viewport_meta(&doc), Some((10, 20)));
    }
}
