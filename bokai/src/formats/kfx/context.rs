//! KFX export context: `ExportContext` holds symbol tables, fragment ids, anchors.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::model::ChapterId;
use crate::model::{GlobalNodeId, LandmarkType, NodeId, TocEntry};
use crate::style::StyleId;

use super::style_registry::StyleRegistry;
use super::symbols::{KFX_SYMBOL_TABLE_SIZE, KfxSymbol};
use super::transforms::encode_base32;

/// KFX font family naming the reader's chosen font; any other value pins one.
const READER_DEFAULT_FONT: &str = "default";

/// Put the reader's font at the head of a stack, the source's own faces behind
/// it: `booksming, serif` becomes `default,booksming,serif`, Amazon's shape. A
/// stack headed by `default` is returned unchanged.
fn with_reader_font_first(stack: &str) -> String {
    let compact = crate::style::compact_font_stack(stack);
    if crate::style::preferred_font_face(&compact).eq_ignore_ascii_case(READER_DEFAULT_FONT) {
        return compact;
    }
    format!("{READER_DEFAULT_FONT},{compact}")
}

/// Symbol table for KFX export: string ↔ symbol ID for the exported file.
/// Local symbols start after the shared YJ_symbols table.
pub struct SymbolTable {
    /// Local symbols (book-specific IDs)
    local_symbols: Vec<String>,
    /// Map from symbol name to ID
    symbol_map: HashMap<String, u64>,
    /// Next local symbol ID (starts after YJ_symbols max_id)
    next_id: u64,
}

impl SymbolTable {
    /// Local symbol IDs start here (after YJ_symbols shared table).
    pub const LOCAL_MIN_ID: u64 = KFX_SYMBOL_TABLE_SIZE as u64;

    /// Create a new empty symbol table.
    pub fn new() -> Self {
        Self {
            local_symbols: Vec::new(),
            symbol_map: HashMap::new(),
            next_id: Self::LOCAL_MIN_ID,
        }
    }

    /// Get or create a symbol ID for a name. A `$`-and-number name is a shared
    /// symbol reference; its number is returned directly.
    pub fn get_or_intern(&mut self, name: &str) -> u64 {
        // Check if it's a shared symbol reference (starts with $)
        if let Some(id_str) = name.strip_prefix('$')
            && let Ok(id) = id_str.parse::<u64>()
        {
            return id;
        }

        // An interned name keeps its id
        if let Some(&id) = self.symbol_map.get(name) {
            return id;
        }

        // Create new local symbol
        let id = self.next_id;
        self.next_id += 1;
        self.local_symbols.push(name.to_string());
        self.symbol_map.insert(name.to_string(), id);
        id
    }

    /// Get symbol ID without interning (returns None if not found).
    pub fn get(&self, name: &str) -> Option<u64> {
        if let Some(id_str) = name.strip_prefix('$')
            && let Ok(id) = id_str.parse::<u64>()
        {
            return Some(id);
        }
        self.symbol_map.get(name).copied()
    }

    /// Get local symbols for $ion_symbol_table fragment.
    pub fn local_symbols(&self) -> &[String] {
        &self.local_symbols
    }

    /// Get the number of local symbols.
    pub fn len(&self) -> usize {
        self.local_symbols.len()
    }

    /// Check if the symbol table is empty.
    pub fn is_empty(&self) -> bool {
        self.local_symbols.is_empty()
    }
}

impl Default for SymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Fragment ID generator, starting at 200. IDs 0-199 are reserved for system
/// fragments.
pub struct IdGenerator {
    next_id: u64,
}

impl IdGenerator {
    /// Fragment IDs start here (matching reference KFX format).
    pub const FRAGMENT_MIN_ID: u64 = 866;

    /// Create a new ID generator.
    pub fn new() -> Self {
        Self {
            next_id: Self::FRAGMENT_MIN_ID,
        }
    }

    /// Generate the next unique ID.
    pub fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Get the current next ID without incrementing.
    pub fn peek(&self) -> u64 {
        self.next_id
    }
}

impl Default for IdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Resource registry for tracking resources (images, fonts, etc.).
#[derive(Debug)]
pub struct ResourceRegistry {
    /// href → resource symbol ID
    resources: HashMap<String, u64>,
    /// href → short resource name (e.g., "e0", "e1")
    resource_names: HashMap<String, String>,
    /// Counter for generating unique names
    next_resource_id: usize,
}

impl ResourceRegistry {
    /// Create a new empty resource registry.
    pub fn new() -> Self {
        Self {
            resources: HashMap::new(),
            resource_names: HashMap::new(),
            next_resource_id: 0,
        }
    }

    /// Register a resource and get its symbol ID.
    pub fn register(&mut self, href: &str, symbols: &mut SymbolTable) -> u64 {
        if let Some(&id) = self.resources.get(href) {
            return id;
        }

        let symbol_name = format!("resource:{}", href);
        let id = symbols.get_or_intern(&symbol_name);
        self.resources.insert(href.to_string(), id);
        id
    }

    /// Get or generate a short resource name (e.g., "e0", "e1").
    ///
    /// Returns the same name for the same href on subsequent calls.
    pub fn get_or_create_name(&mut self, href: &str) -> String {
        if let Some(name) = self.resource_names.get(href) {
            return name.clone();
        }

        let name = format!("e{:X}", self.next_resource_id);
        self.next_resource_id += 1;
        self.resource_names.insert(href.to_string(), name.clone());
        name
    }

    /// Bind `href` to a caller-chosen resource name, outside the generated
    /// `e{N}` sequence.
    pub fn assign_name(&mut self, href: &str, name: &str) {
        self.resource_names
            .insert(href.to_string(), name.to_string());
    }

    /// Get the symbol ID for a resource (if registered).
    pub fn get(&self, href: &str) -> Option<u64> {
        self.resources.get(href).copied()
    }

    /// Get the short name for a resource (if assigned).
    pub fn get_name(&self, href: &str) -> Option<&str> {
        self.resource_names.get(href).map(|s| s.as_str())
    }

    /// Iterate over all registered resources.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &u64)> {
        self.resources.iter()
    }

    /// Get the number of resources registered.
    pub fn len(&self) -> usize {
        self.resource_names.len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.resource_names.is_empty()
    }
}

impl Default for ResourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Text accumulator for content entities: the text seen during export and the
/// offsets a position map needs.
#[derive(Default)]
pub struct TextAccumulator {
    /// Accumulated text segments
    segments: Vec<String>,
    /// Total accumulated length
    total_len: usize,
}

impl TextAccumulator {
    /// Create a new empty text accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push text and return the segment index.
    pub fn push(&mut self, text: &str) -> usize {
        let index = self.segments.len();
        self.total_len += text.len();
        self.segments.push(text.to_string());
        index
    }

    /// Get the total accumulated length.
    pub fn len(&self) -> usize {
        self.total_len
    }

    /// Check if the accumulator is empty.
    pub fn is_empty(&self) -> bool {
        self.total_len == 0
    }

    /// Get all accumulated text segments.
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Clear the accumulator and return the segments.
    pub fn drain(&mut self) -> Vec<String> {
        self.total_len = 0;
        std::mem::take(&mut self.segments)
    }
}

/// Position entry for a node: (fragment_id, byte_offset).
#[derive(Debug, Clone, Copy)]
pub struct Position {
    /// Fragment ID where this node lives.
    pub fragment_id: u64,
    /// Byte offset within the fragment's text content.
    pub offset: usize,
}

/// Resolved anchor with position information (for internal links).
#[derive(Debug, Clone)]
pub struct AnchorPosition {
    /// The anchor symbol name (e.g., "a0", "a1")
    pub symbol: String,
    /// Content fragment ID where this anchor points (for anchor.position.id)
    pub fragment_id: u64,
    /// Section's page_template ID (for position_map grouping)
    pub section_id: u64,
    /// Byte offset within the fragment (0 if at start)
    pub offset: usize,
}

/// External anchor with URI (for external links like http/https URLs).
#[derive(Debug, Clone)]
pub struct ExternalAnchor {
    /// The anchor symbol name (e.g., "a0", "a1")
    pub symbol: String,
    /// The external URI (e.g., `https://standardebooks.org/`)
    pub uri: String,
}

/// Anchor registry for link resolution: KFX links point to anchor symbols and
/// anchor entities ($266) resolve them. One symbol per target, reachable by
/// `GlobalNodeId` (internal targets) and by href string (`link_to` lookups).
#[derive(Debug, Default)]
pub struct AnchorRegistry {
    /// GlobalNodeId → anchor symbol name (e.g., "a0", "a1")
    node_to_symbol: HashMap<GlobalNodeId, String>,

    /// ChapterId → anchor symbol (for chapter-level targets)
    chapter_to_symbol: HashMap<ChapterId, String>,

    /// href string → anchor symbol, filled alongside `node_to_symbol`
    href_to_symbol: HashMap<String, String>,

    /// Symbols that have been resolved to positions (for deduplication)
    resolved_symbols: HashSet<String>,

    /// Resolved internal anchors ready for entity emission
    resolved: Vec<AnchorPosition>,

    /// External anchors ready for entity emission
    external_anchors: Vec<ExternalAnchor>,

    /// Counter for generating unique anchor symbols
    next_anchor_id: usize,

    /// Node positions for TOC lookup: GlobalNodeId → (content_fragment_id, offset)
    node_positions: HashMap<GlobalNodeId, (u64, usize)>,

    /// Chapter positions for TOC lookup: ChapterId → content_fragment_id
    chapter_positions: HashMap<ChapterId, u64>,
}

impl AnchorRegistry {
    /// Create a new empty anchor registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an internal link target and its href. Returns the anchor symbol
    /// a `link_to` style event carries.
    pub fn register_internal_target(&mut self, target: GlobalNodeId, href: &str) -> String {
        if let Some(symbol) = self.node_to_symbol.get(&target) {
            // Ensure href is also mapped
            self.href_to_symbol.insert(href.to_string(), symbol.clone());
            return symbol.clone();
        }

        let symbol = format!("a{:X}", self.next_anchor_id);
        self.next_anchor_id += 1;
        self.node_to_symbol.insert(target, symbol.clone());
        self.href_to_symbol.insert(href.to_string(), symbol.clone());
        symbol
    }

    /// Register a chapter-level link target and its href. Returns the anchor
    /// symbol a `link_to` style event carries.
    pub fn register_chapter_target(&mut self, chapter: ChapterId, href: &str) -> String {
        if let Some(symbol) = self.chapter_to_symbol.get(&chapter) {
            self.href_to_symbol.insert(href.to_string(), symbol.clone());
            return symbol.clone();
        }

        let symbol = format!("a{:X}", self.next_anchor_id);
        self.next_anchor_id += 1;
        self.chapter_to_symbol.insert(chapter, symbol.clone());
        self.href_to_symbol.insert(href.to_string(), symbol.clone());
        symbol
    }

    /// Register an external link target (http/https URL).
    ///
    /// Returns the anchor symbol for use in `link_to` style events.
    pub fn register_external(&mut self, url: &str) -> String {
        if let Some(symbol) = self.href_to_symbol.get(url) {
            return symbol.clone();
        }

        let symbol = format!("a{:X}", self.next_anchor_id);
        self.next_anchor_id += 1;

        self.href_to_symbol.insert(url.to_string(), symbol.clone());
        self.external_anchors.push(ExternalAnchor {
            symbol: symbol.clone(),
            uri: url.to_string(),
        });

        symbol
    }

    /// The anchor symbol a `link_to` carries for `href`: a registered target's
    /// symbol, or a fresh one for an external URL. `None` for an href reaching
    /// nothing the book holds, whose `link_to` is dropped.
    pub fn link_symbol(&mut self, href: &str) -> Option<String> {
        if let Some(symbol) = self.href_to_symbol.get(href) {
            return Some(symbol.clone());
        }
        if href.starts_with("http://") || href.starts_with("https://") {
            return Some(self.register_external(href));
        }
        None
    }

    /// Get the anchor symbol for a node target (if registered).
    pub fn get_symbol(&self, target: GlobalNodeId) -> Option<&str> {
        self.node_to_symbol.get(&target).map(|s| s.as_str())
    }

    /// Get the anchor symbol for a chapter target (if registered).
    pub fn get_chapter_symbol(&self, chapter: ChapterId) -> Option<&str> {
        self.chapter_to_symbol.get(&chapter).map(|s| s.as_str())
    }

    /// Get the anchor symbol for an href (if registered).
    pub fn get_href_symbol(&self, href: &str) -> Option<&str> {
        self.href_to_symbol.get(href).map(|s| s.as_str())
    }

    /// Check if a node is a registered internal target.
    pub fn is_internal_target(&self, target: GlobalNodeId) -> bool {
        self.node_to_symbol.contains_key(&target)
    }

    /// Check if a chapter is a registered target.
    pub fn is_chapter_target(&self, chapter: ChapterId) -> bool {
        self.chapter_to_symbol.contains_key(&chapter)
    }

    /// Create an anchor entity for a node target, during Pass 2. Returns the
    /// symbol for a created anchor, `None` for a resolved one.
    pub fn create_anchor(
        &mut self,
        target: GlobalNodeId,
        content_fragment_id: u64,
        section_id: u64,
        offset: usize,
    ) -> Option<String> {
        let symbol = self.node_to_symbol.get(&target)?.clone();

        if self.resolved_symbols.contains(&symbol) {
            return None;
        }

        self.resolved_symbols.insert(symbol.clone());
        self.resolved.push(AnchorPosition {
            symbol: symbol.clone(),
            fragment_id: content_fragment_id,
            section_id,
            offset,
        });

        // Record position for TOC lookup
        self.node_positions
            .insert(target, (content_fragment_id, offset));

        Some(symbol)
    }

    /// Create an anchor entity for a chapter-level target, during Pass 2 as the
    /// chapter's first content is generated.
    pub fn create_chapter_anchor(
        &mut self,
        chapter: ChapterId,
        content_fragment_id: u64,
        section_id: u64,
    ) -> Option<String> {
        let symbol = self.chapter_to_symbol.get(&chapter)?.clone();

        if self.resolved_symbols.contains(&symbol) {
            return None;
        }

        self.resolved_symbols.insert(symbol.clone());
        self.resolved.push(AnchorPosition {
            symbol: symbol.clone(),
            fragment_id: content_fragment_id,
            section_id,
            offset: 0,
        });

        // Record position for TOC lookup
        self.chapter_positions.insert(chapter, content_fragment_id);

        Some(symbol)
    }

    /// Record the position of a node (for TOC/navigation lookup).
    ///
    /// This stores the position without creating an anchor entity.
    pub fn record_node_position(&mut self, target: GlobalNodeId, fragment_id: u64, offset: usize) {
        self.node_positions
            .entry(target)
            .or_insert((fragment_id, offset));
    }

    /// Record the position of a chapter start.
    pub fn record_chapter_position(&mut self, chapter: ChapterId, fragment_id: u64) {
        self.chapter_positions.entry(chapter).or_insert(fragment_id);
    }

    /// Get the content position for a node (for TOC resolution).
    pub fn get_node_position(&self, target: GlobalNodeId) -> Option<(u64, usize)> {
        self.node_positions.get(&target).copied()
    }

    /// Get the content position for a chapter (for TOC resolution).
    pub fn get_chapter_position(&self, chapter: ChapterId) -> Option<u64> {
        self.chapter_positions.get(&chapter).copied()
    }

    /// Node targets carrying a symbol that Pass 2 placed nowhere — an id on a
    /// node the export flattens into its parent, whose `link_to` names an
    /// anchor no fragment defines.
    pub fn stranded_targets(&self) -> Vec<(GlobalNodeId, String)> {
        self.node_to_symbol
            .iter()
            .filter(|(_, symbol)| !self.resolved_symbols.contains(*symbol))
            .map(|(target, symbol)| (*target, symbol.clone()))
            .collect()
    }

    /// Place `symbol` at `fragment_id` within `section_id`, offset 0.
    pub fn place_anchor(&mut self, symbol: String, fragment_id: u64, section_id: u64) {
        if self.resolved_symbols.insert(symbol.clone()) {
            self.resolved.push(AnchorPosition {
                symbol,
                fragment_id,
                section_id,
                offset: 0,
            });
        }
    }

    /// Drain all resolved internal anchors for entity emission.
    pub fn drain_anchors(&mut self) -> Vec<AnchorPosition> {
        std::mem::take(&mut self.resolved)
    }

    /// Drain all external anchors for entity emission.
    pub fn drain_external_anchors(&mut self) -> Vec<ExternalAnchor> {
        std::mem::take(&mut self.external_anchors)
    }

    /// Get the number of registered targets.
    pub fn len(&self) -> usize {
        self.node_to_symbol.len() + self.chapter_to_symbol.len() + self.external_anchors.len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.node_to_symbol.is_empty()
            && self.chapter_to_symbol.is_empty()
            && self.external_anchors.is_empty()
    }
}

/// Central context for KFX export: symbols, fragment ids, resources, section
/// ids, position map. `tokens_to_ion` captures text into `text_accumulator`,
/// which the assembler packages into Content entities.
pub struct ExportContext {
    /// Global symbol table - strings → symbol IDs.
    pub symbols: SymbolTable,

    /// Fragment ID generator (starts at 200).
    pub fragment_ids: IdGenerator,

    /// Resource tracking: href → resource symbol.
    pub resource_registry: ResourceRegistry,

    /// Section IDs in spine order (for reading order).
    pub section_ids: Vec<u64>,

    /// Text accumulator for the current content entity
    text_accumulator: TextAccumulator,

    /// Current content entity name (symbol ID), set before `tokens_to_ion`
    pub current_content_name: u64,

    /// (ChapterId, NodeId) → Position, filled in the Pass 1 survey
    pub position_map: HashMap<(ChapterId, NodeId), Position>,

    /// Chapter → fragment ID, filled in Pass 1 for section references
    pub chapter_fragments: HashMap<ChapterId, u64>,

    /// Current chapter being processed.
    current_chapter: Option<ChapterId>,

    /// Current fragment ID being built.
    current_fragment_id: u64,

    /// Current text offset within the fragment.
    current_text_offset: usize,

    /// Source file path (`chapter1.xhtml`) → fragment ID
    pub path_to_fragment: HashMap<String, u64>,

    /// Default style symbol ID, referenced by every storyline element
    /// The symbol for `"s0"`. Read it through [`Self::cite_default_style`],
    /// which is what puts the fragment in the container.
    pub default_style_symbol: u64,

    /// Style registry for deduplicating and tracking KFX styles.
    pub style_registry: StyleRegistry,

    /// Memo for the `register_style_id` family, keyed by `(StyleId, class hint,
    /// is_link)`. Cleared per chapter: a StyleId names one chapter's
    /// `StylePool`.
    ir_style_memo: HashMap<
        (StyleId, Option<String>, bool),
        crate::formats::kfx::style_registry::ComputedStyle,
    >,

    /// Anchor registry for link target resolution.
    pub anchor_registry: AnchorRegistry,

    /// LandmarkType → (fragment ID, offset, label), filled during the survey
    pub landmark_fragments: HashMap<LandmarkType, LandmarkTarget>,

    /// Nav container name symbols (registered during Pass 1).
    pub nav_container_symbols: NavContainerSymbols,

    /// Heading positions tracked during survey for headings navigation.
    pub heading_positions: Vec<HeadingPosition>,

    /// Fragment ID for standalone cover section (if EPUB has cover image not in spine).
    pub cover_fragment_id: Option<u64>,

    /// Content fragment ID for standalone cover.
    pub cover_content_id: Option<u64>,

    /// Set once an in-spine chapter takes the cover storyline path
    pub inline_cover_emitted: bool,

    /// Cover pixel size from Pass 1: the cover `page_template`'s fixed box
    pub cover_dimensions: Option<(u32, u32)>,

    /// Fixed-layout image book, keying `kindle_capability_metadata`.
    pub fixed_layout_book: bool,

    /// Any section holds a facing pair — the `yj_double_page_spread` key.
    pub double_page_spread: bool,

    /// The book states author-drawn comic panels — `yj_publisher_panels`.
    pub publisher_panels: bool,

    /// Pixel box a spine page declares (`<meta name="viewport">`)
    pub page_viewports: HashMap<ChapterId, (u32, u32)>,

    /// Chapters that need chapter-start anchors.
    chapters_needing_anchor: HashSet<ChapterId>,

    /// Current pending chapter-start anchor.
    pending_chapter_anchor: Option<ChapterId>,

    /// First content fragment ID for each chapter.
    pub first_content_ids: HashMap<ChapterId, u64>,

    /// All content fragment IDs for each chapter.
    pub content_ids_by_chapter: HashMap<ChapterId, Vec<u64>>,

    /// Text length for each content fragment ID.
    pub content_id_lengths: HashMap<u64, usize>,

    /// section_name → its resource short names, for `container_entity_map`
    pub section_resource_deps: BTreeMap<String, BTreeSet<String>>,

    /// Ruby annotations, drained after the storylines into `ruby_content`
    pub ruby_registry: RubyContentRegistry,

    /// `document_data` writing mode, from the style registry. `HorizontalTb`.
    pub document_writing_mode: KfxSymbol,

    /// `document_data` `direction`; a `-rl` writing mode overrides it. `Ltr`.
    pub document_direction: KfxSymbol,

    /// The SOURCE's dominant mode, which a per-style override is compared against
    pub style_writing_mode_baseline: KfxSymbol,

    /// Whether the book ships typefaces of its own — `override_kindle_font`
    pub has_publisher_fonts: bool,

    /// The body-text `font-family` stack; `None` leaves every family untouched
    pub reader_font_family: Option<String>,

    /// Content language stamped on every reflowable `$style` (`zh-tw`)
    pub content_language: String,
}

/// Ruby annotation string → `(ruby_name, ruby_id)`. Annotations group into
/// fragments of `ENTRIES_PER_FRAGMENT`, one Ion entity each; a base text's
/// style_event names the fragment kfx_id and the 1-indexed id within it.
#[derive(Debug, Clone, Default)]
pub struct RubyContentRegistry {
    /// Annotations in the order they were registered.
    pub annotations: Vec<String>,
    /// Dedup index: annotation text → position in `annotations`.
    by_text: HashMap<String, usize>,
}

impl RubyContentRegistry {
    /// Max content_list entries per ruby_content fragment.
    pub const ENTRIES_PER_FRAGMENT: usize = 250;

    pub fn new() -> Self {
        Self::default()
    }

    /// Register an annotation string; returns (fragment_index, ruby_id).
    /// ruby_id is 1-indexed within the fragment.
    pub fn register(&mut self, annotation: &str) -> (usize, u64) {
        let idx = if let Some(&existing) = self.by_text.get(annotation) {
            existing
        } else {
            let new_idx = self.annotations.len();
            self.annotations.push(annotation.to_string());
            self.by_text.insert(annotation.to_string(), new_idx);
            new_idx
        };
        let frag_idx = idx / Self::ENTRIES_PER_FRAGMENT;
        let ruby_id = (idx % Self::ENTRIES_PER_FRAGMENT) as u64 + 1;
        (frag_idx, ruby_id)
    }

    /// Total fragment count needed to hold all registered annotations.
    pub fn fragment_count(&self) -> usize {
        if self.annotations.is_empty() {
            0
        } else {
            self.annotations.len().div_ceil(Self::ENTRIES_PER_FRAGMENT)
        }
    }

    /// Annotation entries for the given fragment.
    pub fn fragment_entries(&self, frag_idx: usize) -> &[String] {
        let start = frag_idx * Self::ENTRIES_PER_FRAGMENT;
        let end = (start + Self::ENTRIES_PER_FRAGMENT).min(self.annotations.len());
        &self.annotations[start..end]
    }
}

/// Position of a heading element for navigation.
#[derive(Debug, Clone)]
pub struct HeadingPosition {
    /// Heading level (1-6).
    pub level: u8,
    /// Fragment ID containing the heading.
    pub fragment_id: u64,
    /// Byte offset within the fragment.
    pub offset: usize,
}

/// Target position for a landmark.
#[derive(Debug, Clone)]
pub struct LandmarkTarget {
    /// Fragment ID containing the landmark target.
    pub fragment_id: u64,
    /// Byte offset within the fragment (0 for chapter start).
    pub offset: u64,
    /// Display label for the landmark.
    pub label: String,
}

/// Pre-registered symbol IDs for nav container names.
#[derive(Debug, Clone, Default)]
pub struct NavContainerSymbols {
    pub toc: u64,
    pub headings: u64,
    pub landmarks: u64,
    pub page_list: u64,
}

impl ExportContext {
    /// Create a new export context.
    pub fn new() -> Self {
        let mut symbols = SymbolTable::new();
        let default_style_symbol = symbols.get_or_intern("s0");

        Self {
            symbols,
            fragment_ids: IdGenerator::new(),
            resource_registry: ResourceRegistry::new(),
            section_ids: Vec::new(),
            text_accumulator: TextAccumulator::new(),
            current_content_name: 0,
            position_map: HashMap::new(),
            chapter_fragments: HashMap::new(),
            current_chapter: None,
            current_fragment_id: 0,
            current_text_offset: 0,
            path_to_fragment: HashMap::new(),
            default_style_symbol,
            style_registry: StyleRegistry::new(default_style_symbol),
            ir_style_memo: HashMap::new(),
            anchor_registry: AnchorRegistry::new(),
            landmark_fragments: HashMap::new(),
            nav_container_symbols: NavContainerSymbols::default(),
            heading_positions: Vec::new(),
            cover_fragment_id: None,
            cover_content_id: None,
            inline_cover_emitted: false,
            cover_dimensions: None,
            fixed_layout_book: false,
            double_page_spread: false,
            publisher_panels: false,
            page_viewports: HashMap::new(),
            chapters_needing_anchor: HashSet::new(),
            pending_chapter_anchor: None,
            first_content_ids: HashMap::new(),
            content_ids_by_chapter: HashMap::new(),
            content_id_lengths: HashMap::new(),
            section_resource_deps: BTreeMap::new(),
            ruby_registry: RubyContentRegistry::new(),
            document_writing_mode: KfxSymbol::HorizontalTb,
            style_writing_mode_baseline: KfxSymbol::HorizontalTb,
            has_publisher_fonts: false,
            reader_font_family: None,
            document_direction: KfxSymbol::Ltr,
            content_language: String::new(),
        }
    }

    /// Record that a section references a given image resource (by short name).
    pub fn record_section_image_ref(&mut self, section_name: &str, short_name: &str) {
        self.section_resource_deps
            .entry(section_name.to_string())
            .or_default()
            .insert(short_name.to_string());
    }

    /// Prepare context for processing a new chapter.
    pub fn begin_chapter(&mut self, content_name: &str) -> u64 {
        self.text_accumulator = TextAccumulator::new();
        // StyleIds are chapter-local; `ir_style_memo` never crosses a chapter.
        self.ir_style_memo.clear();
        self.current_content_name = self.symbols.get_or_intern(content_name);
        self.current_content_name
    }

    /// Begin Pass 2 export for a chapter.
    pub fn begin_chapter_export(&mut self, chapter_id: ChapterId) {
        self.ir_style_memo.clear();
        self.current_chapter = Some(chapter_id);

        // Check if this chapter needs a chapter-start anchor
        if self.chapters_needing_anchor.contains(&chapter_id) {
            self.pending_chapter_anchor = Some(chapter_id);
        } else {
            self.pending_chapter_anchor = None;
        }
    }

    /// Intern a string into the symbol table, returning its ID.
    pub fn intern(&mut self, s: &str) -> u64 {
        self.symbols.get_or_intern(s)
    }

    /// Track text and return (segment_index, offset).
    pub fn append_text(&mut self, text: &str) -> (usize, usize) {
        let offset = self.text_accumulator.len();
        let index = self.text_accumulator.push(text);
        (index, offset)
    }

    /// Get the text accumulator.
    pub fn text_accumulator(&self) -> &TextAccumulator {
        &self.text_accumulator
    }

    /// Drain the text accumulator.
    pub fn drain_text(&mut self) -> Vec<String> {
        self.text_accumulator.drain()
    }

    /// Generate a new unique fragment ID.
    pub fn next_fragment_id(&mut self) -> u64 {
        self.fragment_ids.next_id()
    }

    /// Register a section and return its symbol ID.
    pub fn register_section(&mut self, name: &str) -> u64 {
        let id = self.intern(name);
        self.section_ids.push(id);
        id
    }

    /// Register an IR style and return its KFX style symbol.
    pub fn register_ir_style(&mut self, ir_style: &crate::style::ComputedStyle) -> u64 {
        self.register_ir_style_with_hint(ir_style, None)
    }

    /// `self.style_writing_mode_baseline` as the IR `WritingMode` enum — the
    /// baseline a style's `writing_mode` override is compared against. Set by
    /// the export entry point ahead of any IR style registration.
    pub fn ir_style_baseline_writing_mode(&self) -> crate::style::WritingMode {
        kfx_symbol_to_ir_writing_mode(self.style_writing_mode_baseline)
    }

    /// The document-effective (possibly user-forced) writing mode as an IR
    /// enum. This is the axis text actually renders in, whereas
    /// [`Self::ir_style_baseline_writing_mode`] is the source's own authored axis.
    fn ir_document_writing_mode(&self) -> crate::style::WritingMode {
        kfx_symbol_to_ir_writing_mode(self.document_writing_mode)
    }

    /// Whether text renders along a vertical axis (縦書き), i.e. the document
    /// writing mode is `vertical-rl`/`vertical-lr`. Gates vertical-only
    /// typography such as tate-chu-yoko.
    pub fn is_vertical_document(&self) -> bool {
        matches!(
            self.document_writing_mode,
            KfxSymbol::VerticalRl | KfxSymbol::VerticalLr
        )
    }

    /// Whether the book carries vertical text anywhere: the document default or
    /// the source's authored axis. `primary-writing-mode` forcing a horizontal
    /// document is what parts this from [`Self::is_vertical_document`].
    pub fn has_vertical_content(&self) -> bool {
        self.is_vertical_document()
            || matches!(
                self.style_writing_mode_baseline,
                KfxSymbol::VerticalRl | KfxSymbol::VerticalLr
            )
    }

    /// Rotate a style's physical margins/padding into the document's writing
    /// axis. A horizontal source forced to vertical-rl moves its block-flow
    /// `margin-top`/`-bottom` onto `margin-right`/`-left`, Amazon's shape.
    fn box_transposed_ir_style<'a>(
        &self,
        ir_style: &'a crate::style::ComputedStyle,
    ) -> std::borrow::Cow<'a, crate::style::ComputedStyle> {
        use crate::style::WritingMode;
        let is_vertical =
            |m: WritingMode| matches!(m, WritingMode::VerticalRl | WritingMode::VerticalLr);
        let source = ir_style.writing_mode;
        let target = self.ir_document_writing_mode();
        if is_vertical(source) == is_vertical(target) {
            return std::borrow::Cow::Borrowed(ir_style);
        }
        let mut s = ir_style.clone();
        rotate_box_model(&mut s, source, target);
        std::borrow::Cow::Owned(s)
    }

    /// The IR style as the KFX writer sees it: box model transposed for the
    /// document's axis, body font handed back to the reader. See
    /// [`Self::reader_font_family`].
    fn prepared_ir_style<'a>(
        &self,
        ir_style: &'a crate::style::ComputedStyle,
    ) -> std::borrow::Cow<'a, crate::style::ComputedStyle> {
        let mut out = self.box_transposed_ir_style(ir_style);
        if let Some(stack) = out.font_family.as_deref()
            && self.reader_font_family.as_deref() == Some(stack)
        {
            let deferred = with_reader_font_first(stack);
            out.to_mut().font_family = Some(deferred);
        }
        out
    }

    /// The `layout` symbol for a `type: container` — its children's
    /// block-progression axis, which the document's writing mode governs:
    /// `horizontal` for vertical writing (縦書き), `vertical` for horizontal-tb.
    pub fn container_layout_symbol(&self) -> KfxSymbol {
        match self.document_writing_mode {
            KfxSymbol::VerticalRl | KfxSymbol::VerticalLr => KfxSymbol::Horizontal,
            _ => KfxSymbol::Vertical,
        }
    }

    /// Register an IR style with an optional source-class hint — the
    /// originating element's `class`, taken as the KFX style symbol when it is a
    /// single valid unclaimed identifier. See `StyleRegistry::register_with_hint`.
    pub fn register_ir_style_with_hint(
        &mut self,
        ir_style: &crate::style::ComputedStyle,
        class_hint: Option<&str>,
    ) -> u64 {
        let kfx_style = self.build_ir_style(ir_style);
        self.style_registry
            .register_with_hint(kfx_style, class_hint, &mut self.symbols)
    }

    /// The KFX properties an IR style states, for a caller outside a
    /// `$157 style` fragment — a table's `column_format` states its geometry
    /// inline.
    pub fn kfx_properties(
        &self,
        ir_style: &crate::style::ComputedStyle,
    ) -> Vec<(KfxSymbol, crate::formats::kfx::style_schema::KfxValue)> {
        self.build_ir_style(ir_style).iter().cloned().collect()
    }

    fn build_ir_style(
        &self,
        ir_style: &crate::style::ComputedStyle,
    ) -> crate::formats::kfx::style_registry::ComputedStyle {
        let schema = crate::formats::kfx::style_schema::StyleSchema::standard();
        let mut builder = crate::formats::kfx::style_registry::StyleBuilder::new(schema);
        let ir_style = self.prepared_ir_style(ir_style);
        builder.ingest_ir_style(&ir_style, self.ir_style_baseline_writing_mode());
        let mut kfx_style = builder.build();
        finalize_tatechuyoko(&mut kfx_style);
        self.apply_background_image(&ir_style, &mut kfx_style);
        kfx_style
    }

    /// Attach the style's background picture. KFX names it with a symbol
    /// pointing at an `external_resource`, minted in Pass 1; a name the book
    /// does not ship resolves to nothing and drops.
    fn apply_background_image(
        &self,
        ir_style: &crate::style::ComputedStyle,
        kfx_style: &mut crate::formats::kfx::style_registry::ComputedStyle,
    ) {
        use crate::formats::kfx::style_schema::KfxValue;
        if let Some(src) = ir_style.background_image.as_deref()
            && let Some(name) = self.resource_registry.get_name(src)
            && let Some(symbol) = self.symbols.get(name)
        {
            kfx_style.set(KfxSymbol::BackgroundImage, KfxValue::SymbolId(symbol));
        }
    }

    /// Register a Link-element style: `register_ir_style_with_hint` plus an
    /// explicit `underline` (`solid` or `none`) where the cascade set none.
    /// Kindle's renderer defaults `<a>` to underlined.
    pub fn register_link_ir_style_with_hint(
        &mut self,
        ir_style: &crate::style::ComputedStyle,
        class_hint: Option<&str>,
    ) -> u64 {
        let kfx_style = self.build_link_ir_style(ir_style);
        self.style_registry
            .register_with_hint(kfx_style, class_hint, &mut self.symbols)
    }

    /// Build the KFX computed style for a Link-element IR style — like
    /// [`Self::build_ir_style`] but forcing an explicit `underline` field. Split
    /// out for the same per-chapter memoization (see `register_link_style_id_with_hint`).
    fn build_link_ir_style(
        &self,
        ir_style: &crate::style::ComputedStyle,
    ) -> crate::formats::kfx::style_registry::ComputedStyle {
        use crate::formats::kfx::style_schema::KfxValue;
        let schema = crate::formats::kfx::style_schema::StyleSchema::standard();
        let mut builder = crate::formats::kfx::style_registry::StyleBuilder::new(schema);
        let ir_style = self.prepared_ir_style(ir_style);
        builder.ingest_ir_style(&ir_style, self.ir_style_baseline_writing_mode());
        let mut kfx_style = builder.build();
        if kfx_style.get(KfxSymbol::Underline).is_none() {
            kfx_style.set(
                KfxSymbol::Underline,
                KfxValue::Symbol(if ir_style.text_decoration_underline {
                    KfxSymbol::Solid
                } else {
                    KfxSymbol::None
                }),
            );
        }
        finalize_tatechuyoko(&mut kfx_style);
        self.apply_background_image(&ir_style, &mut kfx_style);
        kfx_style
    }

    /// The default style's symbol, recorded as cited. Every element resolving
    /// to `s0` takes it from here, and `drain_to_ion` emits the fragment for a
    /// book that has one.
    pub fn cite_default_style(&mut self) -> u64 {
        self.style_registry.cite_default();
        self.default_style_symbol
    }

    /// Register an IR style by StyleId.
    pub fn register_style_id(
        &mut self,
        style_id: StyleId,
        style_pool: &crate::style::StylePool,
    ) -> u64 {
        self.register_style_id_with_hint(style_id, style_pool, None)
    }

    /// Register an IR style by StyleId with an optional source-class hint.
    pub fn register_style_id_with_hint(
        &mut self,
        style_id: StyleId,
        style_pool: &crate::style::StylePool,
        class_hint: Option<&str>,
    ) -> u64 {
        if style_id == StyleId::DEFAULT {
            return self.cite_default_style();
        }

        // Memoize the built KFX style per (StyleId, hint, non-link); a repeated
        // lookup within a chapter skips the schema ingest/build. The style flows
        // through `register_with_hint`, which owns dedup and usage counts.
        let key = (style_id, class_hint.map(str::to_string), false);
        let kfx_style = if let Some(cached) = self.ir_style_memo.get(&key) {
            cached.clone()
        } else if let Some(ir_style) = style_pool.get(style_id) {
            let built = self.build_ir_style(ir_style);
            self.ir_style_memo.insert(key, built.clone());
            built
        } else {
            return self.cite_default_style();
        };
        self.style_registry
            .register_with_hint(kfx_style, class_hint, &mut self.symbols)
    }

    /// Register an IR style with extra KFX properties the IR style vocabulary
    /// has no room for. Amazon writes a cell's `table_column_span` /
    /// `table_row_span` into its `$157 style`, part of the style's identity.
    pub fn register_style_id_with_extras(
        &mut self,
        style_id: StyleId,
        style_pool: &crate::style::StylePool,
        class_hint: Option<&str>,
        extras: &[(KfxSymbol, crate::formats::kfx::style_schema::KfxValue)],
    ) -> u64 {
        if extras.is_empty() {
            return self.register_style_id_with_hint(style_id, style_pool, class_hint);
        }
        let mut kfx_style = match style_pool.get(style_id) {
            Some(ir_style) => self.build_ir_style(ir_style),
            None => crate::formats::kfx::style_registry::ComputedStyle::new(),
        };
        for (symbol, value) in extras {
            kfx_style.set(*symbol, value.clone());
        }
        self.style_registry
            .register_with_hint(kfx_style, class_hint, &mut self.symbols)
    }

    /// Register the bare tate-chu-yoko (縦中横) style and return its symbol. It
    /// carries `text_combine_upright: all`; `finalize_tatechuyoko` completes it.
    /// Applied as an INLINE span, Amazon's `render: inline`.
    pub fn register_tatechuyoko_style(&mut self) -> u64 {
        let s = crate::style::ComputedStyle {
            text_combine_upright: crate::style::TextCombineUpright::All,
            ..Default::default()
        };
        self.register_ir_style_with_hint(&s, None)
    }

    /// Register a Link element's style by StyleId. See
    /// `register_link_ir_style_with_hint` for the underline-forcing rationale.
    pub fn register_link_style_id_with_hint(
        &mut self,
        style_id: StyleId,
        style_pool: &crate::style::StylePool,
        class_hint: Option<&str>,
    ) -> u64 {
        if style_id == StyleId::DEFAULT {
            return self.cite_default_style();
        }

        // Same per-chapter memo as `register_style_id_with_hint`, keyed on the
        // link variant (is_link = true) since the link path forces an underline.
        let key = (style_id, class_hint.map(str::to_string), true);
        let kfx_style = if let Some(cached) = self.ir_style_memo.get(&key) {
            cached.clone()
        } else if let Some(ir_style) = style_pool.get(style_id) {
            let built = self.build_link_ir_style(ir_style);
            self.ir_style_memo.insert(key, built.clone());
            built
        } else {
            return self.cite_default_style();
        };
        self.style_registry
            .register_with_hint(kfx_style, class_hint, &mut self.symbols)
    }

    // Pass 1: Survey / Position Tracking

    /// Begin surveying a chapter.
    pub fn begin_chapter_survey(&mut self, chapter_id: ChapterId, path: &str) -> u64 {
        let fragment_id = self.fragment_ids.next_id();
        self.chapter_fragments.insert(chapter_id, fragment_id);
        self.path_to_fragment.insert(path.to_string(), fragment_id);
        self.current_chapter = Some(chapter_id);
        self.current_fragment_id = fragment_id;
        self.current_text_offset = 0;

        // Mark chapter-start anchor if this chapter is a target
        if self.anchor_registry.is_chapter_target(chapter_id) {
            self.chapters_needing_anchor.insert(chapter_id);
        }

        fragment_id
    }

    /// End surveying a chapter.
    pub fn end_chapter_survey(&mut self) {
        self.current_chapter = None;
    }

    /// Get the fragment ID for a given source path.
    pub fn get_fragment_for_path(&self, path: &str) -> Option<u64> {
        self.path_to_fragment.get(path).copied()
    }

    /// Record position for a node during Pass 1.
    pub fn record_position(&mut self, node_id: NodeId) {
        if let Some(chapter_id) = self.current_chapter {
            self.position_map.insert(
                (chapter_id, node_id),
                Position {
                    fragment_id: self.current_fragment_id,
                    offset: self.current_text_offset,
                },
            );
        }
    }

    /// Record a heading position for headings navigation.
    pub fn record_heading(&mut self, level: u8) {
        self.heading_positions.push(HeadingPosition {
            level,
            fragment_id: self.current_fragment_id,
            offset: self.current_text_offset,
        });
    }

    /// Record heading position during Pass 2 with actual content fragment ID.
    pub fn record_heading_with_id(&mut self, level: u8, fragment_id: u64) {
        self.heading_positions.push(HeadingPosition {
            level,
            fragment_id,
            offset: 0,
        });
    }

    /// Create the pending chapter-start anchor with the first content fragment ID.
    pub fn resolve_pending_chapter_anchor(&mut self, first_content_id: u64) {
        // Record first content ID for this chapter
        if let Some(chapter_id) = self.current_chapter {
            self.first_content_ids
                .entry(chapter_id)
                .or_insert(first_content_id);

            // Record chapter position for TOC lookup
            self.anchor_registry
                .record_chapter_position(chapter_id, first_content_id);
        }

        // Get section ID for position_map grouping
        let section_id = self
            .current_chapter
            .and_then(|ch| self.chapter_fragments.get(&ch).copied())
            .unwrap_or(first_content_id);

        // Create chapter-start anchor if pending
        if let Some(chapter_id) = self.pending_chapter_anchor.take()
            && let Some(symbol) =
                self.anchor_registry
                    .create_chapter_anchor(chapter_id, first_content_id, section_id)
        {
            self.symbols.get_or_intern(&symbol);
        }
    }

    /// Process a node during storyline building.
    ///
    /// If the node is a link target, creates an anchor entity.
    pub fn create_anchor_if_needed(&mut self, node_id: NodeId, content_id: u64, offset: usize) {
        let Some(chapter_id) = self.current_chapter else {
            return;
        };

        let gid = GlobalNodeId::new(chapter_id, node_id);

        // Get section ID for position_map grouping
        let section_id = self
            .chapter_fragments
            .get(&chapter_id)
            .copied()
            .unwrap_or(content_id);

        // Always record position for TOC/navigation lookup
        self.anchor_registry
            .record_node_position(gid, content_id, offset);

        // Only create anchor entity if this is a link target
        if let Some(symbol) = self
            .anchor_registry
            .create_anchor(gid, content_id, section_id, offset)
        {
            self.symbols.get_or_intern(&symbol);
        }
    }

    /// Record a content fragment ID for the current chapter.
    pub fn record_content_id(&mut self, content_id: u64) {
        if let Some(chapter_id) = self.current_chapter {
            self.content_ids_by_chapter
                .entry(chapter_id)
                .or_default()
                .push(content_id);
        }
    }

    /// Record text length for a content fragment ID.
    pub fn record_content_length(&mut self, content_id: u64, text_len: usize) {
        self.content_id_lengths.insert(content_id, text_len);
    }

    /// Advance the text offset during survey (Pass 1).
    pub fn advance_text_offset(&mut self, text_len: usize) {
        self.current_text_offset += text_len;
    }

    /// Get the current fragment ID being surveyed.
    pub fn current_fragment_id(&self) -> u64 {
        self.current_fragment_id
    }

    /// Get the current text offset during survey.
    pub fn current_text_offset(&self) -> usize {
        self.current_text_offset
    }

    // Pass 2: Position Lookup

    /// Look up position for a node.
    pub fn get_position(&self, chapter_id: ChapterId, node_id: NodeId) -> Option<Position> {
        self.position_map.get(&(chapter_id, node_id)).copied()
    }

    /// Get fragment ID for a chapter.
    pub fn get_chapter_fragment(&self, chapter_id: ChapterId) -> Option<u64> {
        self.chapter_fragments.get(&chapter_id).copied()
    }

    /// Get the maximum EID used.
    pub fn max_eid(&self) -> u64 {
        if self.fragment_ids.peek() > IdGenerator::FRAGMENT_MIN_ID {
            self.fragment_ids.peek() - 1
        } else {
            0
        }
    }

    /// Format a position as a Kindle position string.
    pub fn format_kindle_pos(fragment_id: u64, offset: usize) -> String {
        let fid_encoded = encode_base32(fragment_id as u32, 4);
        let off_encoded = encode_base32(offset as u32, 10);
        format!("kindle:pos:fid:{}:off:{}", fid_encoded, off_encoded)
    }

    // TOC Anchor Management

    /// Register TOC entries to mark their targets for anchor creation.
    ///
    /// Uses the pre-resolved `target` field from `ResolvedLinks`.
    pub fn register_toc_targets(&mut self, entries: &[TocEntry]) {
        for entry in entries {
            // `resolve_links` owns target resolution; this registers the entry.

            // Recurse into children
            if !entry.children.is_empty() {
                self.register_toc_targets(&entry.children);
            }
        }
    }

    /// Update landmark fragment IDs to use storyline content IDs.
    pub fn fix_landmark_content_ids(&mut self) {
        for (landmark_type, target) in self.landmark_fragments.iter_mut() {
            // The cover landmark stays at the section's page-template id, the
            // target a real Amazon KFX gives `cover_page`.
            if *landmark_type == LandmarkType::Cover {
                continue;
            }
            // Try to find which chapter this fragment_id belongs to
            let mut found_chapter = None;
            for (cid, &fid) in &self.chapter_fragments {
                if fid == target.fragment_id {
                    found_chapter = Some(*cid);
                    break;
                }
            }

            // A located chapter yields its first content ID
            if let Some(chapter_id) = found_chapter
                && let Some(&content_id) = self.first_content_ids.get(&chapter_id)
            {
                target.fragment_id = content_id;
            }
        }
    }

    /// Get the current chapter ID.
    pub fn current_chapter(&self) -> Option<ChapterId> {
        self.current_chapter
    }

    /// Check if a node is a registered link/TOC target.
    pub fn is_registered_target(&self, node_id: NodeId) -> bool {
        let Some(chapter_id) = self.current_chapter else {
            return false;
        };
        let gid = GlobalNodeId::new(chapter_id, node_id);
        self.anchor_registry.is_internal_target(gid)
    }
}

/// Complete a tate-chu-yoko (縦中横) style: Amazon pairs `text_combine: all`
/// with `writing_mode: horizontal_tb` + `character_width: auto`. The schema
/// suppresses the first and has no rule for the second; both are set here.
fn finalize_tatechuyoko(kfx_style: &mut crate::formats::kfx::style_registry::ComputedStyle) {
    use crate::formats::kfx::style_schema::KfxValue;
    if matches!(
        kfx_style.get(KfxSymbol::TextCombine),
        Some(KfxValue::Symbol(KfxSymbol::All))
    ) {
        kfx_style.set(
            KfxSymbol::WritingMode,
            KfxValue::Symbol(KfxSymbol::HorizontalTb),
        );
        kfx_style.set(KfxSymbol::CharacterWidth, KfxValue::Symbol(KfxSymbol::Auto));
    }
}

/// Map a KFX writing-mode symbol to the IR `WritingMode` enum. Non-writing-mode
/// symbols (and the horizontal default) collapse to `HorizontalTb`.
fn kfx_symbol_to_ir_writing_mode(sym: KfxSymbol) -> crate::style::WritingMode {
    use crate::style::WritingMode;
    match sym {
        KfxSymbol::VerticalRl => WritingMode::VerticalRl,
        KfxSymbol::VerticalLr => WritingMode::VerticalLr,
        _ => WritingMode::HorizontalTb,
    }
}

/// Rotate a style's physical margins and padding from the `from` writing mode's
/// axes into the `to` mode's, preserving logical sides. See
/// [`ExportContext::box_transposed_ir_style`].
fn rotate_box_model(
    s: &mut crate::style::ComputedStyle,
    from: crate::style::WritingMode,
    to: crate::style::WritingMode,
) {
    let (t, r, b, l) = rotate_sides(
        s.margin_top,
        s.margin_right,
        s.margin_bottom,
        s.margin_left,
        from,
        to,
    );
    s.margin_top = t;
    s.margin_right = r;
    s.margin_bottom = b;
    s.margin_left = l;

    let (t, r, b, l) = rotate_sides(
        s.padding_top,
        s.padding_right,
        s.padding_bottom,
        s.padding_left,
        from,
        to,
    );
    s.padding_top = t;
    s.padding_right = r;
    s.padding_bottom = b;
    s.padding_left = l;
}

/// `(top, right, bottom, left)` given in `from`'s axes, re-expressed in `to`'s
/// with the same logical sides. `horizontal-tb` takes ltr inline direction.
fn rotate_sides<T: Copy>(
    top: T,
    right: T,
    bottom: T,
    left: T,
    from: crate::style::WritingMode,
    to: crate::style::WritingMode,
) -> (T, T, T, T) {
    use crate::style::WritingMode::{HorizontalTb, VerticalLr, VerticalRl};
    // physical -> logical: (block_start, block_end, inline_start, inline_end)
    let (bs, be, is_, ie) = match from {
        HorizontalTb => (top, bottom, left, right),
        VerticalRl => (right, left, top, bottom),
        VerticalLr => (left, right, top, bottom),
    };
    // logical -> physical: (top, right, bottom, left)
    match to {
        HorizontalTb => (bs, ie, be, is_),
        VerticalRl => (is_, bs, ie, be),
        VerticalLr => (is_, be, ie, bs),
    }
}

impl Default for ExportContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_table_shared_symbols() {
        let mut symtab = SymbolTable::new();
        assert_eq!(symtab.get_or_intern("$260"), 260);
        assert_eq!(symtab.get_or_intern("$145"), 145);
    }

    #[test]
    fn test_rotate_sides_horizontal_to_vertical_rl() {
        use crate::style::WritingMode::{HorizontalTb, VerticalRl};
        // Physical (top, right, bottom, left) = (T, R, B, L). vertical-rl puts
        // the block axis on right/left and the inline axis on top/bottom.
        let (t, r, b, l) = rotate_sides("T", "R", "B", "L", HorizontalTb, VerticalRl);
        assert_eq!((t, r, b, l), ("L", "T", "R", "B"));
        // margin-top (block-start) → margin-right; margin-bottom → margin-left.
        assert_eq!(r, "T");
        assert_eq!(l, "B");
    }

    #[test]
    fn test_rotate_sides_roundtrips_and_no_op_same_mode() {
        use crate::style::WritingMode::{HorizontalTb, VerticalRl};
        // Same mode in and out is the identity.
        assert_eq!(
            rotate_sides(1, 2, 3, 4, HorizontalTb, HorizontalTb),
            (1, 2, 3, 4)
        );
        // Rotating there and back preserves every side.
        let fwd = rotate_sides(1, 2, 3, 4, HorizontalTb, VerticalRl);
        let back = rotate_sides(fwd.0, fwd.1, fwd.2, fwd.3, VerticalRl, HorizontalTb);
        assert_eq!(back, (1, 2, 3, 4));
    }

    #[test]
    fn test_symbol_table_local_symbols() {
        let mut symtab = SymbolTable::new();
        let id1 = symtab.get_or_intern("section-1");
        let id2 = symtab.get_or_intern("section-2");
        assert!(id1 >= SymbolTable::LOCAL_MIN_ID);
        assert_eq!(id2, id1 + 1);
        assert_eq!(symtab.get_or_intern("section-1"), id1);
    }

    #[test]
    fn test_id_generator() {
        let mut id_gen = IdGenerator::new();
        assert_eq!(id_gen.next_id(), 866);
        assert_eq!(id_gen.next_id(), 867);
        assert_eq!(id_gen.next_id(), 868);
    }

    #[test]
    fn test_resource_registry() {
        let mut symbols = SymbolTable::new();
        let mut registry = ResourceRegistry::new();

        let id1 = registry.register("images/cover.jpg", &mut symbols);
        let id2 = registry.register("images/cover.jpg", &mut symbols);
        let id3 = registry.register("images/other.jpg", &mut symbols);

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_anchor_registry_internal() {
        let mut registry = AnchorRegistry::new();

        let target = GlobalNodeId::new(ChapterId(1), NodeId(42));
        let symbol = registry.register_internal_target(target, "chapter.xhtml#id42");

        assert_eq!(symbol, "a0");
        assert!(registry.is_internal_target(target));
        assert_eq!(registry.get_symbol(target), Some("a0"));
        // Also accessible by href
        assert_eq!(registry.get_href_symbol("chapter.xhtml#id42"), Some("a0"));
    }

    #[test]
    fn test_anchor_registry_chapter() {
        let mut registry = AnchorRegistry::new();

        let chapter = ChapterId(5);
        let symbol = registry.register_chapter_target(chapter, "chapter5.xhtml");

        assert_eq!(symbol, "a0");
        assert!(registry.is_chapter_target(chapter));
        assert_eq!(registry.get_chapter_symbol(chapter), Some("a0"));
        // Also accessible by href
        assert_eq!(registry.get_href_symbol("chapter5.xhtml"), Some("a0"));
    }

    #[test]
    fn test_anchor_registry_external() {
        let mut registry = AnchorRegistry::new();

        let url = "https://example.com/";
        let symbol = registry.register_external(url);

        assert_eq!(symbol, "a0");
        assert_eq!(registry.get_href_symbol(url), Some("a0"));

        let externals = registry.drain_external_anchors();
        assert_eq!(externals.len(), 1);
        assert_eq!(externals[0].uri, url);
    }

    /// An href reaches a symbol when it names a registered target or an
    /// external URL, and none when it names an internal target the registry
    /// has no entry for.
    #[test]
    fn link_symbol_only_names_a_target_that_exists() {
        let mut registry = AnchorRegistry::new();
        let target = GlobalNodeId::new(ChapterId(1), NodeId(42));
        registry.register_internal_target(target, "chapter.xhtml#id42");

        assert_eq!(
            registry.link_symbol("chapter.xhtml#id42"),
            Some("a0".to_string())
        );
        assert_eq!(registry.link_symbol("chapter.xhtml#gone"), None);
        assert_eq!(
            registry.link_symbol("https://example.com/"),
            Some("a1".to_string())
        );
    }

    #[test]
    fn test_anchor_registry_create_anchor() {
        let mut registry = AnchorRegistry::new();

        let target = GlobalNodeId::new(ChapterId(1), NodeId(42));
        registry.register_internal_target(target, "chapter.xhtml#id42");

        // Create anchor
        let symbol = registry.create_anchor(target, 100, 200, 50);
        assert_eq!(symbol, Some("a0".to_string()));

        // A resolved anchor returns None on a second call
        let symbol2 = registry.create_anchor(target, 100, 200, 50);
        assert_eq!(symbol2, None);

        assert_eq!(registry.get_node_position(target), Some((100, 50)));
    }

    #[test]
    fn test_export_context() {
        let mut ctx = ExportContext::new();

        let id1 = ctx.intern("section-1");
        let id2 = ctx.intern("section-1");
        assert_eq!(id1, id2);

        let fid1 = ctx.next_fragment_id();
        let fid2 = ctx.next_fragment_id();
        assert_eq!(fid1, 866);
        assert_eq!(fid2, 867);
    }
}
