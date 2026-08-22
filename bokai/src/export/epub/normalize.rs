//! Normalized export pipeline.
//!
//! This module provides functionality for transforming ebooks through the IR layer
//! to produce clean, consistent output. It merges styles from all chapters into a
//! unified stylesheet and synthesizes normalized XHTML.
//!
//! # Two-Pass Export Flow
//!
//! 1. **Pass 1**: Load all chapters as IR, merge styles into GlobalStylePool
//! 2. **Pass 2**: Generate unified CSS, synthesize XHTML per chapter with remapped styles
//!
//! # Example
//!
//! ```no_run
//! use bokai::Book;
//! use bokai::export::normalize_book;
//!
//! let mut book = Book::open("input.epub")?;
//! let content = normalize_book(&mut book)?;
//!
//! // content.css contains the unified stylesheet
//! // content.chapters contains synthesized XHTML documents
//! // content.assets contains all referenced asset paths
//! # Ok::<(), std::io::Error>(())
//! ```

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;

use crate::import::ChapterId;
use crate::model::{AnchorTarget, Book, Chapter, NodeId, Role};
use crate::style::{CssDecl, StyleId, StylePool, parse_inline_decl};

use super::synth::{generate_css, synthesize_xhtml_document_with_links};

/// Collects styles from all chapters into a unified pool.
///
/// When merging styles from multiple chapters, identical styles are deduplicated
/// and assigned the same global StyleId. Each chapter's local StyleIds are remapped
/// to global IDs for consistent class names across the book.
#[derive(Debug)]
pub struct GlobalStylePool {
    /// The unified style pool containing all unique styles.
    pool: StylePool,
    /// Maps (chapter_idx, local_StyleId) -> global_StyleId
    remaps: Vec<HashMap<StyleId, StyleId>>,
}

impl Default for GlobalStylePool {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalStylePool {
    /// Create a new empty global style pool.
    pub fn new() -> Self {
        Self {
            pool: StylePool::new(),
            remaps: Vec::new(),
        }
    }

    /// Merge styles from a chapter into the global pool.
    ///
    /// This method:
    /// 1. Iterates over all styles in the chapter's pool
    /// 2. Interns each style into the global pool (deduplicating identical styles)
    /// 3. Records the mapping from local to global StyleId
    ///
    /// # Arguments
    ///
    /// * `chapter_idx` - Index of the chapter (used for remap lookups)
    /// * `chapter` - The IR chapter containing styles to merge
    pub fn merge(&mut self, chapter_idx: usize, chapter: &Chapter) {
        // Ensure remaps vec is large enough
        while self.remaps.len() <= chapter_idx {
            self.remaps.push(HashMap::new());
        }

        let remap = &mut self.remaps[chapter_idx];

        // Merge each style from the chapter's pool
        for (local_id, style) in chapter.styles.iter() {
            let global_id = self.pool.intern(style.clone());
            remap.insert(local_id, global_id);
        }
    }

    /// Remap a local StyleId to its global equivalent.
    ///
    /// # Arguments
    ///
    /// * `chapter_idx` - Index of the chapter the style belongs to
    /// * `local_id` - The local StyleId from that chapter
    ///
    /// # Returns
    ///
    /// The global StyleId, or the default style if not found.
    pub fn remap(&self, chapter_idx: usize, local_id: StyleId) -> StyleId {
        self.remaps
            .get(chapter_idx)
            .and_then(|m| m.get(&local_id))
            .copied()
            .unwrap_or(StyleId::DEFAULT)
    }

    /// Get a reference to the unified style pool.
    pub fn pool(&self) -> &StylePool {
        &self.pool
    }

    /// Get all used style IDs across all chapters.
    pub fn used_styles(&self) -> Vec<StyleId> {
        let mut set = HashSet::new();
        for map in &self.remaps {
            set.extend(map.values().copied());
        }
        let mut styles: Vec<StyleId> = set.into_iter().collect();
        styles.sort_by_key(|s| s.0);
        styles
    }
}

/// Whether emitted documents carry the source element ids their nodes came
/// from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceElements {
    /// Omit them — the shape a container ships. A reading device's element
    /// ids are an addressing scheme for the source, not content, and an EPUB
    /// that carried them would leak a foreign format's internals into a
    /// published file.
    Omit,
    /// Stamp `data-eid="<id>"` on the element each source id maps to, so a
    /// renderer can resolve an `(element, offset)` handle to a DOM range.
    Mark,
}

/// Content for a single normalized chapter.
#[derive(Debug, Clone)]
pub struct ChapterContent {
    /// Chapter identifier.
    pub id: ChapterId,
    /// Original source path within the ebook.
    pub source_path: String,
    /// Complete synthesized XHTML document.
    pub document: String,
}

/// Result of normalizing all chapters in a book.
#[derive(Debug)]
pub struct NormalizedContent {
    /// The global style pool with merged styles.
    pub styles: GlobalStylePool,
    /// Normalized chapters with synthesized XHTML.
    pub chapters: Vec<ChapterContent>,
    /// All asset paths referenced across chapters.
    pub assets: HashSet<String>,
    /// The unified CSS stylesheet.
    pub css: String,
    /// The document CSS writing mode the stylesheet was built for
    /// (`horizontal-tb` when the source declared none).
    pub writing_mode: String,
}

/// What a synthesis-time link resolver decided for one `href` value.
#[derive(Clone)]
pub enum LinkOutcome {
    /// Emit the href unchanged.
    Keep,
    /// Emit this replacement href.
    Rewrite(String),
    /// Emit the element without any href.
    DropHref,
}

/// What to emit for one raw `semantics.style` inline-declaration string.
pub enum InlineStyleEmit {
    /// The declaration repeats across the book and was promoted to a
    /// generated class — emit `class="<name>"`.
    Class(String),
    /// Emit as a `style="…"` attribute, pruned of spec defaults.
    Style(String),
    /// Every declaration was a spec default — emit nothing.
    Drop,
}

/// Source-style resolution for normalized synthesis when the importer
/// declares a style program: maps each node's `semantics.class` to the
/// emitted class attribute and its `semantics.style` to a promoted class
/// or an inline `style` attribute. When present, the computed-style class
/// list is ignored entirely.
pub struct SourceStyles<'a> {
    /// Raw source style name → sanitized class name (`None` = the style
    /// produced no declarations, so no class attribute).
    pub named: &'a HashMap<String, Option<String>>,
    /// Raw inline-declaration string → what to emit for it.
    pub inline: &'a HashMap<String, InlineStyleEmit>,
}

/// Normalize all chapters in a book through the IR pipeline.
///
/// The entry point for normalized export, in five passes: each chapter to IR,
/// every style into a global pool, a unified CSS stylesheet, per-chapter XHTML
/// with remapped styles, and the asset references those name.
pub fn normalize_book(book: &mut Book) -> io::Result<NormalizedContent> {
    normalize_book_with(book, SourceElements::Omit)
}

/// [`normalize_book`], choosing whether emitted documents carry their source
/// element ids (see [`SourceElements`]).
pub fn normalize_book_with(
    book: &mut Book,
    source_elements: SourceElements,
) -> io::Result<NormalizedContent> {
    let spine: Vec<_> = book.spine().to_vec();
    let max_workers = book.max_workers();

    // =========================================================================
    // Pass 1: Load all chapters and merge styles
    // =========================================================================

    let mut global_styles = GlobalStylePool::new();
    let mut ir_chapters: Vec<(ChapterId, String, Arc<Chapter>)> = Vec::with_capacity(spine.len());

    // One bulk load: importers with thread-safe internals build the
    // chapters across cores (`Book::load_chapters_cached`). Style merging
    // is order-dependent, so it stays a serial pass over the results.
    let ids: Vec<ChapterId> = spine.iter().map(|e| e.id).collect();
    let loaded = book.load_chapters_cached(&ids)?;
    for (idx, (entry, chapter)) in spine.iter().zip(loaded).enumerate() {
        let source_path = book
            .source_id(entry.id)
            .unwrap_or("unknown.xhtml")
            .to_string();

        // Merge styles into global pool
        global_styles.merge(idx, &chapter);

        ir_chapters.push((entry.id, source_path, chapter));
    }

    // =========================================================================
    // Generate unified CSS
    // =========================================================================
    //
    // Two regimes, keyed on the importer's declared capability. A source
    // whose importer supplies a style program (today only the KFX importer
    // does): the stylesheet is the source's own styles, converted by the
    // `dom_synth` machinery — rules keyed by the raw style names each node
    // carries in `semantics.class`, matching calibre's CSS. All other
    // normalized sources: classes are interned computed
    // styles (`.c<N>`) from the global pool.
    let css_program = book.stylesheet_program();
    let (css_text, source_style_maps, css_artifact) = match &css_program {
        Some(program) => {
            // One walk collects both style channels: used named classes and
            // per-node inline declarations. Class attributes reference every
            // used style whose conversion yields any declaration;
            // stylesheet rules additionally drop spec-default declarations
            // (a rule pruned to empty vanishes from the sheet while its
            // class attribute stays — both render identically). Inline
            // declarations prune the same way, then values repeating across
            // the book promote to shared `g<N>` classes.
            let mut used: HashSet<String> = HashSet::new();
            let mut pruned_of_raw: HashMap<String, Option<String>> = HashMap::new();
            let mut inline_occurrences: Vec<String> = Vec::new();
            for (_, _, ch) in &ir_chapters {
                for node in ch.iter_dfs() {
                    if let Some(name) = ch.semantics.class(node) {
                        used.insert(name.to_string());
                    }
                    if let Some(raw) = ch.semantics.style(node) {
                        let pruned = pruned_of_raw.entry(raw.to_string()).or_insert_with(|| {
                            let mut decl = parse_inline_decl(raw);
                            super::dom_synth::prune_default_decls(&mut decl);
                            (!decl.is_empty()).then(|| decl.to_inline())
                        });
                        if let Some(p) = pruned {
                            inline_occurrences.push(p.clone());
                        }
                    }
                }
            }
            let mut named_rules: Vec<(String, CssDecl)> = used
                .iter()
                .filter_map(|name| {
                    program
                        .named
                        .get(name)
                        .map(|decl| (name.clone(), decl.clone()))
                })
                .collect();
            let named_attr: HashMap<String, Option<String>> = used
                .iter()
                .map(|name| {
                    // A style carrying only state rules keeps its class on the
                    // element: the `:link` rule needs something to select.
                    let has_decl = program.named.get(name).is_some_and(|d| !d.is_empty())
                        || program
                            .pseudo
                            .get(name)
                            .is_some_and(|r| r.iter().any(|(_, d)| !d.is_empty()));
                    (
                        name.clone(),
                        has_decl.then(|| super::dom_synth::safe_class_name(name)),
                    )
                })
                .collect();
            for (_, decl) in &mut named_rules {
                super::dom_synth::prune_default_decls(decl);
            }
            // State-conditional rules for the same used names. These prune
            // like any other rule, but a state rule that prunes to empty is
            // simply absent — it never suppresses the base rule.
            let mut pseudo_rules: Vec<(String, String, CssDecl)> = used
                .iter()
                .filter_map(|name| program.pseudo.get(name).map(|rules| (name, rules)))
                .flat_map(|(name, rules)| {
                    rules
                        .iter()
                        .map(move |(pseudo, decl)| (name.clone(), pseudo.clone(), decl.clone()))
                })
                .collect();
            for (_, _, decl) in &mut pseudo_rules {
                super::dom_synth::prune_default_decls(decl);
            }
            let mut generated_classes = Vec::new();
            let promoted = super::dom_synth::promote_repeated_inline_styles(
                inline_occurrences,
                &mut generated_classes,
            );
            let inline_attr: HashMap<String, InlineStyleEmit> = pruned_of_raw
                .into_iter()
                .map(|(raw, pruned)| {
                    let emit = match pruned {
                        None => InlineStyleEmit::Drop,
                        Some(p) => match promoted.get(&p) {
                            Some(class) => InlineStyleEmit::Class(class.clone()),
                            None => InlineStyleEmit::Style(p),
                        },
                    };
                    (raw, emit)
                })
                .collect();
            let doc = super::dom_synth::StylesheetDoc {
                fixed_layout: program.fixed_layout,
                writing_mode: program.writing_mode.clone(),
                named_rules,
                pseudo_rules,
                generated_classes,
            };
            (doc.emit(), Some((named_attr, inline_attr)), None)
        }
        None => {
            let used_styles = global_styles.used_styles();
            let artifact = generate_css(global_styles.pool(), &used_styles);
            (artifact.stylesheet.clone(), None, Some(artifact))
        }
    };
    let source_styles = source_style_maps
        .as_ref()
        .map(|(named, inline)| SourceStyles { named, inline });

    // =========================================================================
    // Link resolution (normalized-only sources)
    // =========================================================================
    //
    // KFX chapters carry `link_to` targets as `#anchor-name` placeholders;
    // resolve each to the chapter file its stamped id actually landed in
    // (`chapter.xhtml#id`), sanitize external URLs, and drop the href — the
    // `<a>` stays as a non-linking element — when the target was never
    // stamped. The same rules calibre applies in
    // `resolve_link_placeholders`. Passthrough-capable sources keep their
    // hrefs verbatim.
    let resolve_links = book.requires_normalized_export();
    if resolve_links {
        let anchor_chapters: Vec<(ChapterId, Arc<Chapter>)> = ir_chapters
            .iter()
            .map(|(id, _, ch)| (*id, Arc::clone(ch)))
            .collect();
        book.index_anchors(&anchor_chapters);
    }
    let chapter_files = super::chapter_filenames(ir_chapters.iter().map(|(_, sp, _)| sp.as_str()));
    let chapter_pos: HashMap<ChapterId, usize> = ir_chapters
        .iter()
        .enumerate()
        .map(|(i, (id, _, _))| (*id, i))
        .collect();
    let chapters_by_id: HashMap<ChapterId, Arc<Chapter>> = ir_chapters
        .iter()
        .map(|(id, _, ch)| (*id, Arc::clone(ch)))
        .collect();
    let href_resolver = |href: &str| -> LinkOutcome {
        match book.resolve_href(ChapterId(0), href) {
            Some(AnchorTarget::External(url)) => match crate::util::sanitize_href(&url) {
                Some(clean) if clean == href => LinkOutcome::Keep,
                Some(clean) => LinkOutcome::Rewrite(clean),
                None => LinkOutcome::DropHref,
            },
            Some(AnchorTarget::Internal(target)) => {
                // Only a target whose id was actually stamped resolves; the
                // chapter-start fallback (ROOT) has no id and drops, exactly
                // like calibre's unstamped-anchor rule.
                let frag = chapters_by_id
                    .get(&target.chapter)
                    .and_then(|ch| ch.semantics.id(target.node));
                match (chapter_pos.get(&target.chapter), frag) {
                    (Some(&idx), Some(frag)) => {
                        LinkOutcome::Rewrite(format!("{}#{}", chapter_files[idx], frag))
                    }
                    _ => LinkOutcome::DropHref,
                }
            }
            Some(AnchorTarget::Chapter(cid)) => match chapter_pos.get(&cid) {
                Some(&idx) => LinkOutcome::Rewrite(chapter_files[idx].clone()),
                None => LinkOutcome::DropHref,
            },
            None => LinkOutcome::DropHref,
        }
    };

    // =========================================================================
    // Pass 2: Synthesize XHTML with remapped styles
    // =========================================================================

    let mut chapters = Vec::with_capacity(ir_chapters.len());
    let mut all_assets = HashSet::new();
    let language = book.metadata().language.clone();

    // What the source actually holds, so an `<img>` naming something absent
    // degrades instead of shipping a dangling reference (see
    // `dom_synth::Builder::emit_image`). `bundled_assets` is the importer's
    // authoritative list where it has one; otherwise the container inventory.
    let available_assets: HashSet<String> = match book.bundled_assets() {
        Some(paths) => paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        None => book
            .list_assets()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    };

    // Declared style program: build each chapter through the shared XHTML
    // DOM + consolidation passes — the same code calibre serializes with, so
    // chapter files keep calibre's byte shape. The title comes from
    // `chapter_title` (a fixed-layout page is titled by its owning section, not
    // its per-page name), matching calibre's `push_book_part`; the
    // viewport is the FXL page's pixel box.
    //
    // Link resolution reads the importer through `book`, which cannot
    // cross threads — so every href the emit walk will consult is resolved
    // up front into a plain map, and the per-chapter synthesis (a pure
    // function of the IR) runs across cores. `parallel_map` preserves
    // input order and the asset set is order-insensitive, so the output
    // bytes are exactly the serial loop's.
    if let Some(src) = &source_styles {
        let mut link_map: HashMap<String, LinkOutcome> = HashMap::new();
        for (_, _, ch) in &ir_chapters {
            for node in ch.iter_dfs() {
                if let Some(href) = ch.semantics.href(node)
                    && !link_map.contains_key(href)
                {
                    link_map.insert(href.to_string(), href_resolver(href));
                }
            }
        }
        let titles: Vec<String> = ir_chapters
            .iter()
            .map(|(id, sp, _)| book.chapter_title(*id).unwrap_or(sp).to_string())
            .collect();
        let jobs: Vec<usize> = (0..ir_chapters.len()).collect();
        let emitted = crate::util::parallel_map(&jobs, max_workers, |&idx| {
            let (_, _, ir) = &ir_chapters[idx];
            let resolver = |href: &str| -> LinkOutcome {
                link_map.get(href).cloned().unwrap_or(LinkOutcome::DropHref)
            };
            let opts = super::dom_synth::ChapterEmit {
                title: &titles[idx],
                language: &language,
                source_styles: src,
                href_resolver: &resolver,
                viewport: spine.get(idx).and_then(|e| e.viewport),
                source_elements,
                available_assets: Some(&available_assets),
            };
            let mut assets = HashSet::new();
            let document = super::dom_synth::emit_chapter(ir, &opts, &mut assets);
            (document, assets)
        });
        for (idx, (document, assets)) in emitted.into_iter().enumerate() {
            let (chapter_id, source_path, _) = &ir_chapters[idx];
            all_assets.extend(assets);
            chapters.push(ChapterContent {
                id: *chapter_id,
                source_path: source_path.clone(),
                document,
            });
        }
    } else {
        for (idx, (chapter_id, source_path, ir)) in ir_chapters.iter().enumerate() {
            // Computed-style `.c<N>` regime (no declared style program):
            // string synthesis with the interned class list.
            let mut remapped_class_list: Vec<Option<&str>> = Vec::new();
            if let Some(artifact) = &css_artifact {
                remapped_class_list = vec![None; ir.styles.len()];
                for (local_id, _) in ir.styles.iter() {
                    let global_id = global_styles.remap(idx, local_id);
                    if let Some(class_name) = artifact.class_name_fast(global_id) {
                        let slot = remapped_class_list
                            .get_mut(local_id.0 as usize)
                            .expect("style id out of bounds");
                        *slot = Some(class_name);
                    }
                }
            }

            // Extract title from first heading or use source path
            let title = extract_chapter_title(ir).unwrap_or_else(|| source_path.clone());

            // Synthesize XHTML document
            let result = synthesize_xhtml_document_with_links(
                ir,
                &remapped_class_list,
                &title,
                Some("style.css"),
                resolve_links.then_some(&href_resolver as &dyn Fn(&str) -> LinkOutcome),
                None,
            );

            // Collect assets
            all_assets.extend(result.assets);

            chapters.push(ChapterContent {
                id: *chapter_id,
                source_path: source_path.clone(),
                document: result.body,
            });
        }
    }

    Ok(NormalizedContent {
        styles: global_styles,
        chapters,
        assets: all_assets,
        css: css_text,
        writing_mode: css_program
            .as_ref()
            .map(|p| p.writing_mode.clone())
            .unwrap_or_else(|| "horizontal-tb".to_string()),
    })
}

/// Extract a title from the first heading in a chapter.
fn extract_chapter_title(ir: &Chapter) -> Option<String> {
    for node_id in ir.iter_dfs() {
        if let Some(node) = ir.node(node_id)
            && matches!(node.role, Role::Heading(_))
        {
            // Collect text from heading's children
            let mut title = String::new();
            collect_text_recursive(ir, node_id, &mut title);
            if !title.is_empty() {
                return Some(crate::util::trim_markup_space(&title).to_string());
            }
        }
    }
    None
}

/// Recursively collect text content from a node and its descendants.
fn collect_text_recursive(ir: &Chapter, node_id: NodeId, buf: &mut String) {
    if let Some(node) = ir.node(node_id)
        && node.role == Role::Text
    {
        buf.push_str(ir.text(node.text));
    }

    for child_id in ir.children(node_id) {
        collect_text_recursive(ir, child_id, buf);
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::model::Node;
    use crate::style::{ComputedStyle, FontWeight};

    #[test]
    fn test_global_style_pool_new() {
        let pool = GlobalStylePool::new();
        assert_eq!(pool.pool().len(), 1); // Default style
        assert!(pool.remaps.is_empty());
    }

    #[test]
    fn test_global_style_pool_merge() {
        let mut global = GlobalStylePool::new();

        // Create first chapter with a bold style
        let mut chapter1 = Chapter::new();
        let mut bold = ComputedStyle::default();
        bold.font_weight = FontWeight::BOLD;
        let bold_id = chapter1.styles.intern(bold.clone());

        // Create second chapter with the same bold style
        let mut chapter2 = Chapter::new();
        let bold_id2 = chapter2.styles.intern(bold);

        // Merge both chapters
        global.merge(0, &chapter1);
        global.merge(1, &chapter2);

        // Both should map to the same global ID
        let global_id1 = global.remap(0, bold_id);
        let global_id2 = global.remap(1, bold_id2);
        assert_eq!(global_id1, global_id2);

        // Global pool should have 2 styles (default + bold)
        assert_eq!(global.pool().len(), 2);
    }

    #[test]
    fn test_global_style_pool_remap_unknown() {
        let global = GlobalStylePool::new();

        // Unknown chapter/style should return default
        let result = global.remap(999, StyleId(999));
        assert_eq!(result, StyleId::DEFAULT);
    }

    #[test]
    fn test_global_style_pool_used_styles() {
        let mut global = GlobalStylePool::new();

        let mut chapter = Chapter::new();
        let mut bold = ComputedStyle::default();
        bold.font_weight = FontWeight::BOLD;
        chapter.styles.intern(bold);

        global.merge(0, &chapter);

        let used = global.used_styles();
        assert!(!used.is_empty());
    }

    #[test]
    fn test_extract_chapter_title() {
        let mut chapter = Chapter::new();

        // Add a heading with text
        let h1 = chapter.alloc_node(Node::new(Role::Heading(1)));
        chapter.append_child(NodeId::ROOT, h1);

        let text_range = chapter.append_text("Chapter One");
        let mut text_node = Node::new(Role::Text);
        text_node.text = text_range;
        let text_id = chapter.alloc_node(text_node);
        chapter.append_child(h1, text_id);

        let title = extract_chapter_title(&chapter);
        assert_eq!(title, Some("Chapter One".to_string()));
    }

    #[test]
    fn test_extract_chapter_title_no_heading() {
        let chapter = Chapter::new();
        let title = extract_chapter_title(&chapter);
        assert_eq!(title, None);
    }
}
