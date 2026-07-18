//! KFX format importer.
//!
//! KFX is Amazon's Kindle Format 10, using Ion binary data format.
//!
//! This module handles I/O operations for reading KFX containers.
//! Pure parsing functions are in `crate::formats::kfx::container`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::formats::kfx::anchor_table::{
    AnchorTable, register_heading_levels, register_nav_synthetics,
};
use crate::formats::kfx::container::{
    ContainerError, EntityLoc, SymbolTable, get_field, parse_container_header,
    parse_container_info, parse_index_table, skip_enty_header,
};
use crate::formats::kfx::fxl;
use crate::formats::kfx::ion::{IonParser, IonValue};
use crate::formats::kfx::resource_index::{self, ImageResource};
use crate::formats::kfx::schema::schema;
use crate::formats::kfx::storyline::parse_storyline_to_ir;
use crate::formats::kfx::symbols::KfxSymbol;
use crate::import::{ChapterId, CssProgram, Importer, SpineEntry};
use crate::io::{ByteSource, FileSource};
use crate::model::Chapter;
use crate::model::{
    AnchorTarget, CollectionInfo, Contributor, GlobalNodeId, Landmark, Metadata, PageSpread,
    TocEntry,
};

/// Shorthand for getting a KfxSymbol as u32 for field lookups.
macro_rules! sym {
    ($variant:ident) => {
        KfxSymbol::$variant as u64
    };
}

/// KFX format importer.
///
/// `.kfx-zip` bundles are pre-merged into a single in-memory KFX container
/// before reaching this importer; see `Book::open_format` and `kfx::merge`.
/// So this type only ever sees a single container.
pub struct KfxImporter {
    /// Random-access byte source.
    source: Arc<dyn ByteSource>,

    /// Container header length (offset to entity data).
    #[allow(dead_code)]
    header_len: usize,

    /// Entity index: maps (type_id, entity_idx) -> EntityLoc
    entities: Vec<EntityLoc>,

    /// Precomputed asset paths (bcRawMedia entity IDs + font paths).
    asset_paths: Vec<PathBuf>,

    /// Font entity map: font path (e.g., "fonts/font_0000.otf") -> EntityLoc.
    font_entities: HashMap<String, EntityLoc>,

    /// Resolved symbol table (static base as declared by the container +
    /// doc-local symbols).
    symbols: Arc<SymbolTable>,

    /// Book metadata.
    metadata: Metadata,

    /// Table of contents.
    toc: Vec<TocEntry>,

    /// Physical page list (from the `page_list` nav_container). Flat; empty
    /// when the KFX carries no page numbers. Kept so a KFX→KFX reconvert (or the
    /// IR-based export) re-emits the page list instead of silently dropping it.
    page_list: Vec<TocEntry>,

    /// Landmarks (structural navigation points).
    landmarks: Vec<Landmark>,

    /// Reading order (spine).
    spine: Vec<SpineEntry>,

    /// Section names for spine entries.
    section_names: Vec<String>,

    /// Cache: section name -> storyline EntityLoc (lazily populated)
    section_storylines: HashMap<String, EntityLoc>,
    /// Whether section→storyline mapping has been built
    section_storylines_indexed: bool,

    /// Element ids declared on section entities themselves (page-template
    /// containers) → owning section name. These eids never appear in chapter
    /// content, so `element_id_map` can't resolve them; navigation targets
    /// pointing at them (a common shape for cover/TOC landmarks) fall back
    /// to the section's chapter start.
    section_eids: HashMap<i64, String>,

    /// Section name → the main (last) page template's own element id and
    /// `$style` name. The template becomes the chapter's root container —
    /// the same body-level `<div>` the mechanical route emits — and anchors
    /// at `(template_eid, offset)` stamp onto/inside it.
    section_templates: HashMap<String, SectionTemplate>,

    /// Fixed-layout page records, parallel to `spine`/`section_names` when
    /// the book is image-based fixed layout (empty for reflowable books).
    /// Each spine entry is one page; `section` names its owning section
    /// (chapter `<title>`, storyline lookup) and `ordinal` its leaf index in
    /// the section's spread walk.
    fxl_pages: Vec<FxlPage>,
    /// Section name → its spread-walked leaf pages (container +
    /// `page-spread-*` property), from the section's FIRST page template
    /// (fixed-layout sections split that template into per-page documents;
    /// reflowable sections use the LAST). Walked once at spine expansion;
    /// per-page loads index it by ordinal instead of re-walking the
    /// template.
    fxl_leaves: HashMap<String, Vec<(IonValue, String)>>,
    /// story_name → storyline entity location, for resolving spread pages
    /// and container story references without a section hop.
    storylines_by_name: HashMap<String, EntityLoc>,
    /// Structure ($608) entity name → location, for `page_templates` entries
    /// that are symbol references rather than inline containers.
    structures_by_name: HashMap<String, EntityLoc>,
    /// Content-walk page-progression direction: `document_data.direction`
    /// plus the vertical-writing-mode → rtl override, WITHOUT the explicit
    /// reading-order override `derive_writing_direction` applies last to the
    /// OPF value. The spread pairing (`page-spread-left` first vs `-right`
    /// first) alternates from this, matching the mechanical route.
    content_ppd: String,

    /// Resources: name -> EntityLoc (lazily populated)
    resources: HashMap<String, EntityLoc>,
    /// Whether resources have been indexed
    resources_indexed: bool,

    /// Canonical image list from the shared external_resource walk —
    /// port-identical filenames, deterministic (sorted-fid) order, cover
    /// renamed to `cover.<ext>`. Built once in `from_source`.
    images: Vec<ImageResource>,
    /// resource_name → `images` index (duplicates: last one wins).
    image_by_name: HashMap<String, usize>,
    /// Exported filename → `images` index.
    image_by_filename: HashMap<String, usize>,
    /// bcRawMedia location key (resolved entity symbol name) → payload location.
    image_media: HashMap<String, EntityLoc>,

    /// Content (`$145`) entity name → location, built once in `from_source`
    /// so text lookups resolve by direct index instead of a scan-and-parse
    /// over every content entity (first entity wins on duplicate names,
    /// like the scan did).
    content_by_name: HashMap<String, EntityLoc>,
    /// Content cache: name -> list of strings (lazily populated). Behind a
    /// lock so `lookup_content_text` works from `&self` — chapter builds
    /// run in parallel across a shared importer reference.
    content_cache: std::sync::RwLock<HashMap<String, Vec<String>>>,

    /// Anchor map: anchor_name -> uri (for external link resolution)
    anchors: Arc<HashMap<String, String>>,
    /// Whether anchors have been indexed
    anchors_indexed: bool,

    /// Style map: style_name -> KFX style properties (for style resolution)
    styles: Arc<HashMap<String, Vec<(u64, IonValue)>>>,
    /// Whether styles have been indexed
    styles_indexed: bool,

    /// Ruby map: ruby_name (e.g. "b_ruby_0") → ordered annotation texts.
    /// Style events reference entries via `ruby_name`+`ruby_id` (1-indexed).
    ruby_index: Arc<HashMap<String, Vec<String>>>,
    /// Whether ruby_content has been indexed
    ruby_indexed: bool,

    // --- Link resolution ---
    /// The shared KFX anchor table (real `$266` anchors + synthetic toc/page
    /// anchors at nav target positions) — the same rule set the mechanical
    /// route stamps ids from, so both engines name content anchors
    /// identically. Built by `index_anchor_entities`; `load_chapter` stamps
    /// `semantics.id` from it.
    anchor_table: Arc<AnchorTable>,

    /// Maps element string ID -> GlobalNodeId (built during index_anchors).
    /// Ids are the STAMPED anchor ids (`a85J`, `toc-148-0`, …), the same
    /// namespace the emitted XHTML carries.
    element_id_map: HashMap<String, GlobalNodeId>,

    /// Element id (`$155`) → owning chapter, accumulated as chapters load
    /// (every eid in the parsed storyline counts, whether or not its element
    /// survives into the IR). The structural file-resolution map for nav
    /// targets — the mechanical route's `element_id_to_filename` analog.
    eid_chapters: HashMap<i64, ChapterId>,

    /// Doc-level CSS writing mode (`horizontal-tb` / `vertical-rl` / …),
    /// resolved by `derive_writing_direction`. Feeds the normalized
    /// stylesheet's `body { writing-mode: … }` header;
    /// `metadata.primary_writing_mode` carries the OPF-vocabulary form.
    css_writing_mode: String,
}

impl From<ContainerError> for io::Error {
    fn from(e: ContainerError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e.to_string())
    }
}

impl Importer for KfxImporter {
    fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let source = Arc::new(FileSource::new(file)?);
        Self::from_source(source)
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

    fn page_list(&self) -> &[TocEntry] {
        &self.page_list
    }

    fn page_list_mut(&mut self) -> &mut [TocEntry] {
        &mut self.page_list
    }

    fn landmarks(&self) -> &[Landmark] {
        &self.landmarks
    }

    fn spine(&self) -> &[SpineEntry] {
        &self.spine
    }

    fn source_id(&self, id: ChapterId) -> Option<&str> {
        self.section_names.get(id.0 as usize).map(|s| s.as_str())
    }

    fn chapter_title(&self, id: ChapterId) -> Option<&str> {
        // A fixed-layout page is titled by its owning section — the
        // mechanical route's `push_book_part(base, section_name)`.
        if let Some(p) = self.fxl_pages.get(id.0 as usize) {
            return Some(&p.section);
        }
        self.source_id(id)
    }

    fn load_chapter(&mut self, id: ChapterId) -> io::Result<Chapter> {
        // Ensure anchors, styles, and ruby are indexed
        self.index_anchor_entities()?;
        self.index_styles()?;
        self.index_ruby_content()?;

        let (chapter, eids) = self.build_chapter(id)?;
        self.register_eids(id, &eids);
        Ok(chapter)
    }

    fn load_chapters(&mut self, ids: &[ChapterId]) -> Vec<io::Result<Chapter>> {
        // One-time indexes first — the parallel builds below share `&self`.
        for index in [
            Self::index_anchor_entities,
            Self::index_styles,
            Self::index_ruby_content,
        ] {
            if let Err(e) = index(self) {
                return ids
                    .iter()
                    .map(|_| Err(io::Error::new(e.kind(), e.to_string())))
                    .collect();
            }
        }

        // Chapter builds are pure per chapter; eid registration is
        // order-sensitive (first-wins in spine order), so it stays serial,
        // applied in `ids` order after the parallel phase — the same map
        // the one-at-a-time loads produce.
        let built = crate::util::parallel_map(ids, |id| self.build_chapter(*id));
        built
            .into_iter()
            .zip(ids)
            .map(|(res, id)| {
                res.map(|(chapter, eids)| {
                    self.register_eids(*id, &eids);
                    chapter
                })
            })
            .collect()
    }

    fn load_raw(&mut self, id: ChapterId) -> io::Result<Vec<u8>> {
        // A fixed-layout page's raw form is its owning section's storyline
        // (spread halves share one).
        let section_name = if let Some(p) = self.fxl_pages.get(id.0 as usize) {
            p.section.clone()
        } else {
            self.section_names
                .get(id.0 as usize)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Chapter not found"))?
                .clone()
        };

        // Find section entity and resolve to storyline
        let storyline_loc = self.resolve_section_to_storyline(&section_name)?;
        self.read_entity(storyline_loc)
    }

    fn list_assets(&self) -> &[PathBuf] {
        &self.asset_paths
    }

    fn load_asset(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        let name = path.to_string_lossy();

        // Handle font path lookup (e.g., "fonts/font_0000.otf")
        if let Some(loc) = self.font_entities.get(&*name) {
            return self.read_entity(*loc);
        }

        // Handle direct entity ID lookup (e.g., "#1102" from list_assets)
        if let Some(id_str) = name.strip_prefix('#') {
            if let Ok(id) = id_str.parse::<u32>() {
                // Find entity by ID
                if let Some(loc) = self.entities.iter().find(|e| e.id == id) {
                    return self.read_entity(*loc);
                }
            }
            return Err(io::Error::new(io::ErrorKind::NotFound, "Entity not found"));
        }

        // Exported image filename (what `list_assets` returns): serve the
        // final bytes — JXR sources transcode to JPEG here.
        if let Some(&idx) = self.image_by_filename.get(&*name) {
            return self.load_image_bytes(idx);
        }

        // Ensure resources are indexed for name-based lookup
        if !self.resources_indexed {
            self.index_resources()?;
        }

        let loc = self
            .resources
            .get(&*name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Asset not found"))?;

        self.read_entity(*loc)
    }

    fn bundled_assets(&self) -> Option<Vec<PathBuf>> {
        Some(
            self.images
                .iter()
                .map(|img| PathBuf::from(&img.filename))
                .collect(),
        )
    }

    fn load_assets(&mut self, paths: &[PathBuf]) -> Vec<io::Result<Vec<u8>>> {
        // Resolve raw bytes serially (cheap reads), then run the CPU-bound
        // JPEG-XR→JPEG transcodes in parallel across cores — the mechanical
        // route parallelizes the same stage the same way.
        let mut results: Vec<Option<io::Result<Vec<u8>>>> = Vec::with_capacity(paths.len());
        let mut jxr_jobs: Vec<(usize, usize, Vec<u8>)> = Vec::new(); // (slot, image idx, raw)
        for (slot, path) in paths.iter().enumerate() {
            let name = path.to_string_lossy();
            match self.image_by_filename.get(&*name).copied() {
                Some(idx) if self.images[idx].is_jxr => match self.read_image_raw(idx) {
                    Ok(raw) => {
                        jxr_jobs.push((slot, idx, raw));
                        results.push(None);
                    }
                    Err(e) => results.push(Some(Err(e))),
                },
                _ => results.push(Some(self.load_asset(path))),
            }
        }
        let transcoded = crate::util::parallel_map(&jxr_jobs, |(_, idx, raw)| {
            crate::image::jxr_transcode::transcode(raw, &self.images[*idx].resource_name)
                .map(|(bytes, _format, _timing)| bytes)
                .map_err(|e| io::Error::other(e.to_string()))
        });
        for ((slot, _, _), result) in jxr_jobs.iter().zip(transcoded) {
            results[*slot] = Some(result);
        }
        results.into_iter().map(|r| r.expect("filled")).collect()
    }

    fn requires_normalized_export(&self) -> bool {
        // KFX load_raw returns binary Ion data, not HTML
        true
    }

    fn index_anchors(&mut self, chapters: &[(ChapterId, Arc<Chapter>)]) {
        self.element_id_map.clear();

        // Build element_id → GlobalNodeId map from chapters
        for (chapter_id, chapter) in chapters {
            for node_id in chapter.iter_dfs() {
                if let Some(id) = chapter.semantics.id(node_id) {
                    self.element_id_map
                        .insert(id.to_string(), GlobalNodeId::new(*chapter_id, node_id));
                }
            }
        }
    }

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

        // Strip leading # if present for anchor/element lookups
        let anchor_name = href.strip_prefix('#').unwrap_or(href);

        // Handle #id:offset format (KFX TOC/nav format)
        let anchor_name = if let Some(colon_pos) = anchor_name.find(':') {
            &anchor_name[..colon_pos]
        } else {
            anchor_name
        };

        // Check external anchors map (anchor_name → uri)
        if let Some(uri) = self.anchors.as_ref().get(anchor_name) {
            return Some(AnchorTarget::External(uri.clone()));
        }

        // Internal anchor NAME (`link_to` targets): resolve through the
        // anchor table to the node its stamped id landed on — precise even
        // for content that emits outside the section that structurally owns
        // its eid. An unstamped position falls through to eid resolution.
        if let Some(&(pos_id, offset)) = self.anchor_table.name_to_position.get(anchor_name) {
            if let Some(target) = self
                .anchor_table
                .id_at(pos_id, offset)
                .and_then(|frag| self.element_id_map.get(&frag))
            {
                return Some(AnchorTarget::Internal(*target));
            }
            if let Some(target) = self.resolve_eid_chapter(pos_id) {
                return Some(target);
            }
        }

        // Try direct element ID lookup (stamped anchor ids: `a85J`,
        // `toc-148-0`, …).
        if let Some(target) = self.element_id_map.get(anchor_name) {
            return Some(AnchorTarget::Internal(*target));
        }

        // Numeric eid (`#911` / `#911:4` nav placeholders): the element's
        // chapter, from the structural eid walk; section-declared ids
        // (page-template containers) fall back to the owning section.
        if let Ok(numeric_id) = anchor_name.parse::<i64>() {
            return self.resolve_eid_chapter(numeric_id);
        }

        // Not found
        None
    }

    fn nav_fragment(&self, href: &str) -> Option<(String, bool)> {
        let (eid, offset) = parse_position_href(href)?;
        let frag = self.anchor_table.id_at(eid, offset)?;
        let stamped = self.element_id_map.contains_key(&frag);
        Some((frag, stamped))
    }

    fn stylesheet_program(&mut self) -> Option<CssProgram> {
        self.index_styles().ok()?;
        let named = self
            .styles
            .iter()
            .map(|(name, fields)| {
                (
                    name.clone(),
                    crate::formats::kfx::yj_properties::convert_yj_properties(
                        fields,
                        &self.symbols,
                    ),
                )
            })
            .collect();
        Some(CssProgram {
            named,
            writing_mode: self.css_writing_mode.clone(),
            fixed_layout: self.metadata.fixed_layout,
        })
    }
}

impl KfxImporter {
    /// Create an importer from a ByteSource.
    pub fn from_source(source: Arc<dyn ByteSource>) -> io::Result<Self> {
        // Read and parse container header (18 bytes)
        let header_data = source.read_at(0, 18)?;
        let header = parse_container_header(&header_data)?;

        // Read and parse container info
        let container_info_data = source.read_at(
            header.container_info_offset as u64,
            header.container_info_length,
        )?;
        let container_info = parse_container_info(&container_info_data)?;

        // Get index table location (required)
        let (index_offset, index_length) = container_info.index.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Missing index table in container",
            )
        })?;

        // Read and parse document symbols (optional). `from_fragment` reads
        // the container's declared import max_id as the doc-symbol base —
        // never assume our static table's length (older containers declare a
        // smaller base and would mis-resolve every doc-local name).
        let symbols = if let Some((offset, length)) = container_info.doc_symbols {
            if length > 0 {
                let doc_sym_data = source.read_at(offset as u64, length)?;
                SymbolTable::from_fragment(Some(&doc_sym_data))
            } else {
                SymbolTable::from_fragment(None)
            }
        } else {
            SymbolTable::from_fragment(None)
        };

        // Read and parse index table
        let index_data = source.read_at(index_offset as u64, index_length)?;
        let entities = parse_index_table(&index_data, header.header_len);

        // Build asset paths: bcRawMedia as entity IDs, bcRawFont as fonts/ paths
        let mut asset_paths: Vec<PathBuf> = entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::Bcrawmedia as u32)
            .map(|e| PathBuf::from(format!("#{}", e.id)))
            .collect();

        let mut font_entities = HashMap::new();
        for (idx, e) in entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::Bcrawfont as u32)
            .enumerate()
        {
            let font_path = format!("fonts/font_{idx:04}.otf");
            font_entities.insert(font_path.clone(), *e);
            asset_paths.push(PathBuf::from(font_path));
        }

        let mut importer = Self {
            source,
            header_len: header.header_len,
            entities,
            asset_paths,
            font_entities,
            symbols: Arc::new(symbols),
            metadata: Metadata::default(),
            toc: Vec::new(),
            page_list: Vec::new(),
            landmarks: Vec::new(),
            spine: Vec::new(),
            section_names: Vec::new(),
            section_storylines: HashMap::new(),
            section_storylines_indexed: false,
            section_eids: HashMap::new(),
            section_templates: HashMap::new(),
            fxl_pages: Vec::new(),
            fxl_leaves: HashMap::new(),
            storylines_by_name: HashMap::new(),
            structures_by_name: HashMap::new(),
            content_ppd: "ltr".to_string(),
            resources: HashMap::new(),
            resources_indexed: false,
            images: Vec::new(),
            image_by_name: HashMap::new(),
            image_by_filename: HashMap::new(),
            image_media: HashMap::new(),
            content_by_name: HashMap::new(),
            content_cache: std::sync::RwLock::new(HashMap::new()),
            anchors: Arc::new(HashMap::new()),
            anchors_indexed: false,
            styles: Arc::new(HashMap::new()),
            styles_indexed: false,
            ruby_index: Arc::new(HashMap::new()),
            ruby_indexed: false,
            anchor_table: Arc::new(AnchorTable::default()),
            element_id_map: HashMap::new(),
            eid_chapters: HashMap::new(),
            css_writing_mode: "horizontal-tb".to_string(),
        };

        importer.parse_metadata()?;
        importer.detect_fxl();
        importer.parse_navigation()?;
        importer.index_section_storylines()?;
        importer.parse_spine()?;
        // Needs the explicit reading-order direction captured by
        // `parse_spine` (it is the strongest override).
        importer.derive_writing_direction();
        // Image index: needs metadata (declared cover) and the spine /
        // section→storyline maps (first-section cover fallback) — so it runs
        // while `section_names` still holds sections, before the fixed-layout
        // expansion renames entries per page. Cheap — external_resource
        // fragments are tiny and media bytes are only peeked (≤ 64 bytes
        // each) for format sniffing.
        importer.build_image_index();
        // Fixed-layout books: split each section into per-page spine entries
        // (needs `content_ppd` from `derive_writing_direction` for the
        // spread pairing).
        importer.expand_fxl_spine()?;
        // Content name → location, so per-token text lookups during chapter
        // builds are direct reads.
        importer.index_content_entities();

        Ok(importer)
    }

    /// Read an entity's raw data (after ENTY header).
    fn read_entity(&self, loc: EntityLoc) -> io::Result<Vec<u8>> {
        let entity_data = self.source.read_at(loc.offset as u64, loc.length)?;

        // Use pure function to skip ENTY header
        let payload = skip_enty_header(&entity_data);
        if payload.len() != entity_data.len() {
            Ok(payload.to_vec())
        } else {
            Ok(entity_data)
        }
    }

    /// Parse an entity as Ion and return the parsed value.
    fn parse_entity_ion(&self, loc: EntityLoc) -> io::Result<IonValue> {
        let ion_data = self.read_entity(loc)?;
        let mut parser = IonParser::new(&ion_data);
        parser.parse()
    }

    /// Get a symbol's text from an IonValue (handles both Symbol and String).
    fn get_symbol_text<'a>(&'a self, value: &'a IonValue) -> Option<&'a str> {
        self.symbols.text_of_opt(value)
    }

    /// Parse book metadata.
    fn parse_metadata(&mut self) -> io::Result<()> {
        // Find book_metadata entity
        let loc = self
            .entities
            .iter()
            .find(|e| e.type_id == KfxSymbol::BookMetadata as u32)
            .copied();

        if let Some(loc) = loc {
            let elem = self.parse_entity_ion(loc)?;

            // Amazon's KFX wraps as `book_metadata::{...}` (annotated struct);
            // boko's own exporter emits a plain struct. Handle both.
            if let Some(fields) = elem.unwrap_annotated().as_struct()
                && let Some(list) =
                    get_field(fields, sym!(CategorisedMetadata)).and_then(|m| m.as_list())
            {
                for category_elem in list {
                    if let Some(cat_fields) = category_elem.as_struct() {
                        let category = get_field(cat_fields, sym!(Category))
                            .and_then(|v| self.get_symbol_text(v))
                            .unwrap_or("");

                        if category == "kindle_title_metadata"
                            && let Some(metadata_list) =
                                get_field(cat_fields, sym!(Metadata)).and_then(|v| v.as_list())
                        {
                            for meta in metadata_list {
                                let Some(meta_fields) = meta.as_struct() else {
                                    continue;
                                };
                                let key = get_field(meta_fields, sym!(Key))
                                    .and_then(|v| v.as_string())
                                    .unwrap_or("");
                                let value = get_field(meta_fields, sym!(Value))
                                    .and_then(|v| v.as_string())
                                    .unwrap_or("");

                                match key {
                                    // First-wins / skip-empty guards match
                                    // `kfx_to_epub::loader`'s duplicate-key
                                    // handling so both KFX readers agree on
                                    // containers that repeat a key.
                                    "title" if self.metadata.title.is_empty() => {
                                        self.metadata.title = value.to_string()
                                    }
                                    "author" => {
                                        // calibre joins multiple authors with " & " in a
                                        // single `author` field (yj_metadata.py:209) and
                                        // splits on "&" when reading back. Mirror that.
                                        for part in value.split('&') {
                                            let trimmed = part.trim();
                                            if !trimmed.is_empty() {
                                                self.metadata.authors.push(trimmed.to_string());
                                            }
                                        }
                                    }
                                    "publisher" => {
                                        self.metadata.publisher = Some(value.trim().to_string())
                                    }
                                    "language" => self.metadata.language = value.to_string(),
                                    "description" => {
                                        self.metadata.description = Some(value.to_string())
                                    }
                                    "book_id" => self.metadata.identifier = value.to_string(),
                                    "ASIN"
                                        // Amazon catalogue id. Separate from
                                        // `book_id`, which is a per-device
                                        // internal UUID — both keys appear
                                        // side-by-side in kindle_title_metadata.
                                        if !value.is_empty() && self.metadata.asin.is_none() => {
                                            self.metadata.asin = Some(value.to_string());
                                        }
                                    "issue_date"
                                        if self.metadata.date.is_none() && !value.is_empty() =>
                                    {
                                        self.metadata.date = Some(value.to_string())
                                    }
                                    "cover_image" => {
                                        let value_elem = get_field(meta_fields, sym!(Value));
                                        if let Some(cover) = self.resolve_cover_value(value_elem) {
                                            self.metadata.cover_image = Some(cover);
                                        }
                                    }
                                    "modified_date" => {
                                        self.metadata.modified_date = Some(value.to_string())
                                    }
                                    "translator" => self.metadata.contributors.push(Contributor {
                                        name: value.to_string(),
                                        file_as: None,
                                        role: Some("trl".to_string()),
                                    }),
                                    "title_pronunciation" if !value.is_empty() => {
                                        self.metadata.title_sort = Some(value.to_string())
                                    }
                                    "author_pronunciation" if !value.is_empty() => {
                                        self.metadata.author_sort = Some(value.to_string())
                                    }
                                    "series_name" => {
                                        if let Some(ref mut coll) = self.metadata.collection {
                                            coll.name = value.to_string();
                                        } else {
                                            self.metadata.collection = Some(CollectionInfo {
                                                name: value.to_string(),
                                                collection_type: Some("series".to_string()),
                                                position: None,
                                            });
                                        }
                                    }
                                    "series_position" => {
                                        if let Ok(pos) = value.parse::<f64>() {
                                            if let Some(ref mut coll) = self.metadata.collection {
                                                coll.position = Some(pos);
                                            } else {
                                                self.metadata.collection = Some(CollectionInfo {
                                                    name: String::new(),
                                                    collection_type: Some("series".to_string()),
                                                    position: Some(pos),
                                                });
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }

        // Fallback: the flat `$258 metadata` entity carries the same fields as
        // direct symbol keys (`title`, `language`, `publisher`, `author`,
        // `ASIN`, `cover_image`). Amazon KFX often ships `language` (and
        // sometimes others) ONLY here, not in `kindle_title_metadata`; the
        // port's loader reads both and fills empties from this struct, so
        // mirror it — else the `<html lang>` and other fields silently drop
        // on the IR route.
        self.parse_flat_metadata_fallback()?;

        Ok(())
    }

    /// Fill still-empty metadata from the flat `$258 metadata` entity's direct
    /// fields (the mechanical `loader.rs::parse_metadata_struct` fallback).
    /// Every write is empty-guarded so `kindle_title_metadata` wins.
    fn parse_flat_metadata_fallback(&mut self) -> io::Result<()> {
        let loc = self
            .entities
            .iter()
            .find(|e| e.type_id == KfxSymbol::Metadata as u32)
            .copied();
        let Some(loc) = loc else {
            return Ok(());
        };
        let elem = self.parse_entity_ion(loc)?;
        let Some(fields) = elem.unwrap_annotated().as_struct().map(<[_]>::to_vec) else {
            return Ok(());
        };
        let text = |sym: KfxSymbol| {
            get_field(&fields, sym as u64)
                .and_then(IonValue::as_string)
                .map(str::to_string)
        };

        if self.metadata.cover_image.is_none() {
            let value = get_field(&fields, KfxSymbol::CoverImage as u64);
            if let Some(cover) = self.resolve_cover_value(value) {
                self.metadata.cover_image = Some(cover);
            }
        }
        if self.metadata.title.is_empty()
            && let Some(t) = text(KfxSymbol::Title)
        {
            self.metadata.title = t;
        }
        if self.metadata.authors.is_empty() {
            match get_field(&fields, KfxSymbol::Author as u64) {
                Some(IonValue::String(s)) if !s.is_empty() => self.metadata.authors.push(s.clone()),
                Some(IonValue::List(items)) => {
                    for it in items {
                        if let Some(s) = it.as_string().filter(|s| !s.is_empty()) {
                            self.metadata.authors.push(s.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        if self.metadata.language.is_empty()
            && let Some(l) = text(KfxSymbol::Language)
        {
            self.metadata.language = l;
        }
        if self.metadata.publisher.is_none()
            && let Some(p) = text(KfxSymbol::Publisher)
        {
            self.metadata.publisher = Some(p.trim().to_string());
        }
        if self.metadata.asin.is_none()
            && let Some(a) = text(KfxSymbol::Asin).filter(|s| !s.is_empty())
        {
            self.metadata.asin = Some(a);
        }
        Ok(())
    }

    /// Resolve one `book_navigation.nav_containers` entry to its nav_container
    /// struct. Inline structs pass through; a bare symbol is the referenced form
    /// (fixed-layout / PDOC books, which the device requires) — look up the
    /// separate nav_container ($391) entity whose id matches and parse it. The
    /// reference symbol id equals the target entity's id, so no text lookup is
    /// needed. `None` when it can't be resolved.
    fn resolve_nav_container(&self, container: &IonValue) -> Option<IonValue> {
        let inner = container.unwrap_annotated();
        if inner.as_struct().is_some() {
            return Some(inner.clone());
        }
        let IonValue::Symbol(sym) = inner else {
            return None;
        };
        let loc = self
            .entities
            .iter()
            .find(|e| e.type_id == KfxSymbol::NavContainer as u32 && e.id as u64 == *sym)
            .copied()?;
        self.parse_entity_ion(loc).ok()
    }

    /// Parse book navigation (TOC and landmarks).
    fn parse_navigation(&mut self) -> io::Result<()> {
        // Find book_navigation entity
        let loc = self
            .entities
            .iter()
            .find(|e| e.type_id == KfxSymbol::BookNavigation as u32)
            .copied();

        if let Some(loc) = loc {
            let elem = self.parse_entity_ion(loc)?;

            // book_navigation is a list of reading orders
            if let Some(list) = elem.as_list() {
                for reading_order in list {
                    if let Some(ro_fields) = reading_order.as_struct() {
                        // Look for nav_containers
                        if let Some(containers) =
                            get_field(ro_fields, sym!(NavContainers)).and_then(|v| v.as_list())
                        {
                            for container in containers {
                                // Inline struct, or a symbol referencing a
                                // separate nav_container entity (the fixed-layout
                                // / PDOC shape) — resolve both.
                                let Some(resolved) = self.resolve_nav_container(container) else {
                                    continue;
                                };
                                if let Some(container_fields) = resolved.as_struct() {
                                    // Check nav_type
                                    let nav_type = get_field(container_fields, sym!(NavType))
                                        .and_then(|v| self.get_symbol_text(v));

                                    // Append: a book can carry several
                                    // containers of one nav_type (one per
                                    // reading order) — calibre and the
                                    // mechanical route collect them all;
                                    // last-wins would drop entries.
                                    match nav_type {
                                        Some("toc") => {
                                            let entries = self.parse_nav_entries(container_fields);
                                            self.toc.extend(entries);
                                        }
                                        Some("page_list") => {
                                            let entries = self.parse_nav_entries(container_fields);
                                            self.page_list.extend(entries);
                                        }
                                        Some("landmarks") => {
                                            let entries =
                                                self.parse_landmark_entries(container_fields);
                                            self.landmarks.extend(entries);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Parse landmark entries from a landmarks nav_container.
    fn parse_landmark_entries(&self, container: &[(u64, IonValue)]) -> Vec<Landmark> {
        let mut landmarks = Vec::new();

        if let Some(entry_list) = get_field(container, sym!(Entries)).and_then(|v| v.as_list()) {
            for entry in entry_list {
                // Unwrap annotation if present
                let inner = entry.unwrap_annotated();
                if let Some(entry_fields) = inner.as_struct() {
                    // Get landmark_type symbol and convert via schema
                    let landmark_type =
                        get_field(entry_fields, sym!(LandmarkType)).and_then(|v| match v {
                            IonValue::Symbol(id) => schema().landmark_from_kfx(*id),
                            _ => None,
                        });

                    // Skip unknown landmark types
                    let Some(landmark_type) = landmark_type else {
                        continue;
                    };

                    // Get label from representation.label. "cover-nav-unit"
                    // is a placeholder, not a display label (calibre's
                    // `add_guide_entry` strips it too).
                    let label = get_field(entry_fields, sym!(Representation))
                        .and_then(|v| v.as_struct())
                        .and_then(|s| get_field(s, sym!(Label)))
                        .and_then(|v| v.as_string())
                        .filter(|s| *s != "cover-nav-unit")
                        .unwrap_or("")
                        .to_string();

                    // Get target position (id and offset)
                    let target_pos =
                        get_field(entry_fields, sym!(TargetPosition)).and_then(|v| v.as_struct());
                    let href = if let Some(pos) = target_pos {
                        let id = get_field(pos, sym!(Id)).and_then(|v| v.as_int());
                        let offset = get_field(pos, sym!(Offset)).and_then(|v| v.as_int());
                        match (id, offset) {
                            (Some(id), Some(off)) if off > 0 => format!("#{}:{}", id, off),
                            (Some(id), _) => format!("#{}", id),
                            _ => String::new(),
                        }
                    } else {
                        String::new()
                    };

                    landmarks.push(Landmark {
                        landmark_type,
                        href,
                        label,
                    });
                }
            }
        }

        landmarks
    }

    /// Recursively parse nav entries into a tree structure.
    fn parse_nav_entries(&self, container: &[(u64, IonValue)]) -> Vec<TocEntry> {
        let mut entries = Vec::new();

        if let Some(entry_list) = get_field(container, sym!(Entries)).and_then(|v| v.as_list()) {
            for entry in entry_list {
                // Unwrap annotation if present (nav_unit::...)
                let inner = entry.unwrap_annotated();
                if let Some(entry_fields) = inner.as_struct() {
                    // Get label (try representation.label first, then direct label).
                    // A MISSING label falls back to "Untitled" and the entry is
                    // kept — Amazon ships unlabeled nav_units (e.g. the page-list
                    // book-start sentinel) and the mechanical route keeps them in
                    // the TOC; page-list consumers drop the sentinel at emission.
                    // A PRESENT-but-empty label and the "heading-nav-unit"
                    // placeholder are dropped, matching calibre.
                    let label = get_field(entry_fields, sym!(Representation))
                        .and_then(|v| v.as_struct())
                        .and_then(|s| get_field(s, sym!(Label)))
                        .and_then(|v| v.as_string())
                        .or_else(|| {
                            get_field(entry_fields, sym!(Label)).and_then(|v| v.as_string())
                        })
                        .unwrap_or("Untitled");
                    if label.is_empty() || label == "heading-nav-unit" {
                        continue;
                    }

                    // Get target position (includes id and offset for within-section navigation)
                    let target_pos =
                        get_field(entry_fields, sym!(TargetPosition)).and_then(|v| v.as_struct());
                    let href = if let Some(pos) = target_pos {
                        let id = get_field(pos, sym!(Id)).and_then(|v| v.as_int());
                        let offset = get_field(pos, sym!(Offset)).and_then(|v| v.as_int());
                        match (id, offset) {
                            (Some(id), Some(off)) if off > 0 => format!("#{}:{}", id, off),
                            (Some(id), _) => format!("#{}", id),
                            _ => String::new(),
                        }
                    } else {
                        String::new()
                    };

                    // Recursively parse children
                    let children = self.parse_nav_entries(entry_fields);

                    entries.push(TocEntry {
                        title: label.to_string(),
                        href,
                        children,
                        play_order: None,
                        target: None,
                    });
                }
            }
        }

        entries
    }

    /// Parse spine from reading_orders.
    ///
    /// Uses the section→storyline cache to get size estimates. Also captures
    /// `page_progression_direction` from the selected reading order onto
    /// `metadata.page_progression_direction` so the EPUB exporter can re-emit
    /// it on `<spine>` (vertical-RTL Japanese books rely on this).
    fn parse_spine(&mut self) -> io::Result<()> {
        let (section_names, ppd) = self.get_reading_order_sections()?;

        if let Some(ppd) = ppd {
            self.metadata.page_progression_direction = Some(ppd);
        }

        for (idx, name) in section_names.into_iter().enumerate() {
            // Get size from cached storyline location
            let size_estimate = self
                .section_storylines
                .get(&name)
                .map(|loc| loc.length)
                .unwrap_or(0);

            self.section_names.push(name);
            self.spine.push(SpineEntry {
                id: ChapterId(idx as u32),
                size_estimate,
                page_spread: None,
                viewport: None,
            });
        }

        Ok(())
    }

    /// Read the `content_features` entities for the book-level fixed-layout
    /// signals: `yj_*fixed_layout` switches spine construction to the
    /// per-page expansion; `yj_double_page_spread` marks a spread comic
    /// (`book-type` OPF hint).
    fn detect_fxl(&mut self) {
        let locs: Vec<EntityLoc> = self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::ContentFeatures as u32)
            .copied()
            .collect();
        let mut acc = fxl::FxlFeatures::default();
        for loc in locs {
            if let Ok(elem) = self.parse_entity_ion(loc) {
                fxl::scan_content_features(&elem, &self.symbols, &mut acc);
            }
        }
        self.metadata.fixed_layout = acc.fixed_layout;
        if acc.double_page_spread {
            self.metadata.book_type = Some("comic".to_string());
        }
    }

    /// Fixed-layout spine expansion: replace the one-entry-per-section spine
    /// with one entry per page. Each section's FIRST page template (the
    /// reflowable path uses the LAST) is either a `page_spread`/`facing_page`
    /// container — its storyline's content_list holds the per-page
    /// containers, paired `page-spread-left`/`-right` alternating from
    /// `content_ppd` (RTL books read the right page first) — or itself a
    /// single leaf page (e.g. the cover). Page names follow the mechanical
    /// route: `{section}` for a spreadless page, `{section}-{left|right}`
    /// for spread halves; export-side filename dedup adds `-N` on collision.
    fn expand_fxl_spine(&mut self) -> io::Result<()> {
        if !self.metadata.fixed_layout || self.section_names.is_empty() {
            return Ok(());
        }

        // Structure ($608) entity name → location, for `page_templates`
        // entries that are symbol references rather than inline containers.
        for e in &self.entities {
            if e.type_id == KfxSymbol::Structure as u32 {
                let name = self.symbols.resolve(e.id as u64).to_string();
                self.structures_by_name.entry(name).or_insert(*e);
            }
        }

        // Section name → FIRST page template, one pass over the section
        // entities. Only needed for the leaf walk below.
        let mut templates: HashMap<String, IonValue> = HashMap::new();
        let sec_locs: Vec<EntityLoc> = self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::Section as u32)
            .copied()
            .collect();
        for loc in sec_locs {
            let Ok(elem) = self.parse_entity_ion(loc) else {
                continue;
            };
            let Some(fields) = elem.as_struct() else {
                continue;
            };
            let Some(name) = get_field(fields, sym!(SectionName))
                .and_then(|v| self.get_symbol_text(v))
                .map(|s| s.to_string())
            else {
                continue;
            };
            let Some(t0) = get_field(fields, sym!(PageTemplates))
                .and_then(|v| v.as_list())
                .and_then(|l| l.first())
                .cloned()
            else {
                continue;
            };
            templates.entry(name).or_insert(t0);
        }

        let old_sections = std::mem::take(&mut self.section_names);
        self.spine.clear();
        let mut names: Vec<String> = Vec::new();
        let mut fxl_pages: Vec<FxlPage> = Vec::new();
        let mut spine: Vec<SpineEntry> = Vec::new();
        for sec in &old_sections {
            let Some(template) = templates.get(sec) else {
                continue;
            };
            let size_estimate = self
                .section_storylines
                .get(sec)
                .map(|l| l.length)
                .unwrap_or(0);
            let leaves = self.collect_spread_leaves(template);
            for (ordinal, (leaf, prop)) in leaves.iter().enumerate() {
                let spread_type = prop.rsplit('-').next().unwrap_or("");
                let name = if spread_type.is_empty() {
                    sec.clone()
                } else {
                    format!("{sec}-{spread_type}")
                };
                let viewport = leaf.unwrap_annotated().as_struct().and_then(|f| {
                    match (
                        fxl::read_px(f, KfxSymbol::FixedWidth),
                        fxl::read_px(f, KfxSymbol::FixedHeight),
                    ) {
                        (Some(w), Some(h)) => Some((w, h)),
                        _ => None,
                    }
                });
                let page_spread = match prop.as_str() {
                    "page-spread-left" => Some(PageSpread::Left),
                    "page-spread-right" => Some(PageSpread::Right),
                    _ => None,
                };
                spine.push(SpineEntry {
                    id: ChapterId(names.len() as u32),
                    size_estimate,
                    page_spread,
                    viewport,
                });
                names.push(name);
                fxl_pages.push(FxlPage {
                    section: sec.clone(),
                    ordinal,
                });
            }
            self.fxl_leaves.insert(sec.clone(), leaves);
        }
        self.section_names = names;
        self.fxl_pages = fxl_pages;
        self.spine = spine;
        Ok(())
    }

    /// Collect a page template's leaf pages in emission order, each with its
    /// `page-spread-*` property (empty for a spreadless page). Mirrors the
    /// mechanical route's `process_spread_template` walk: spread containers
    /// recurse into their storyline's containers with alternating sides;
    /// anything else is a leaf.
    fn collect_spread_leaves(&self, template: &IonValue) -> Vec<(IonValue, String)> {
        let mut out = Vec::new();
        self.walk_spread(template, "", &mut out);
        out
    }

    fn walk_spread(
        &self,
        template: &IonValue,
        spread_property: &str,
        out: &mut Vec<(IonValue, String)>,
    ) {
        // Resolve a `$608 structure` symbol reference to the real struct.
        let resolved;
        let template = match template.unwrap_annotated() {
            IonValue::Symbol(id) => {
                let name = self.symbols.resolve(*id).to_string();
                let Some(loc) = self.structures_by_name.get(&name) else {
                    return;
                };
                let Ok(elem) = self.parse_entity_ion(*loc) else {
                    return;
                };
                resolved = elem;
                &resolved
            }
            _ => template,
        };
        let inner = template.unwrap_annotated();
        let Some(fields) = inner.as_struct() else {
            return;
        };
        let layout = get_field(fields, sym!(Layout))
            .and_then(|v| self.symbols.text_of(v))
            .unwrap_or("");
        if fxl::is_spread_layout(layout) {
            // The story holds the per-page containers; RTL spreads read the
            // right-hand page first.
            let Some(story) = get_field(fields, sym!(StoryName))
                .and_then(|v| self.symbols.text_of(v))
                .map(|s| s.to_string())
            else {
                return;
            };
            let (_, pages) = self.storyline_parts(&story);
            let mut prop = if self.content_ppd == "ltr" {
                "page-spread-left"
            } else {
                "page-spread-right"
            };
            for page in &pages {
                self.walk_spread(page, prop, out);
                prop = if prop == "page-spread-left" {
                    "page-spread-right"
                } else {
                    "page-spread-left"
                };
            }
            return;
        }
        out.push((template.clone(), spread_property.to_string()));
    }

    /// Parse a storyline by story name into its root element id (when the
    /// storyline struct declares one) and its content_list items.
    fn storyline_parts(&self, story: &str) -> (Option<i64>, Vec<IonValue>) {
        let Some(loc) = self.storylines_by_name.get(story) else {
            return (None, Vec::new());
        };
        let Ok(elem) = self.parse_entity_ion(*loc) else {
            return (None, Vec::new());
        };
        let inner = elem.unwrap_annotated();
        let Some(fields) = inner.as_struct() else {
            return (None, Vec::new());
        };
        let root_eid = get_field(fields, sym!(Id)).and_then(|v| v.as_int());
        let items = get_field(fields, sym!(ContentList))
            .and_then(|v| v.as_list())
            .map(|l| l.to_vec())
            .unwrap_or_default();
        (root_eid, items)
    }

    /// Recursively inline `story_name` references: a struct that carries a
    /// story but no inline content_list gets the story's content_list
    /// spliced in (the tokenizer does not follow story references itself —
    /// the mechanical route's `process_content` resolves them at walk time).
    /// The stack guards against reference cycles; a story legitimately
    /// referenced from two siblings inlines twice, like the walk would.
    fn inline_story_refs(&self, value: IonValue, stack: &mut Vec<String>) -> IonValue {
        match value {
            IonValue::Annotated(ann, inner) => {
                IonValue::Annotated(ann, Box::new(self.inline_story_refs(*inner, stack)))
            }
            IonValue::List(items) => IonValue::List(
                items
                    .into_iter()
                    .map(|v| self.inline_story_refs(v, stack))
                    .collect(),
            ),
            IonValue::Struct(fields) => {
                let has_content = fields.iter().any(|(k, _)| *k == sym!(ContentList));
                let story = if has_content {
                    None
                } else {
                    fields
                        .iter()
                        .find(|(k, _)| *k == sym!(StoryName))
                        .and_then(|(_, v)| self.symbols.text_of(v))
                        .map(|s| s.to_string())
                        .filter(|s| !stack.contains(s))
                };
                let mut fields: Vec<(u64, IonValue)> = fields
                    .into_iter()
                    .map(|(k, v)| (k, self.inline_story_refs(v, stack)))
                    .collect();
                if let Some(story) = story {
                    stack.push(story.clone());
                    let (_, items) = self.storyline_parts(&story);
                    let items = items
                        .into_iter()
                        .map(|v| self.inline_story_refs(v, stack))
                        .collect();
                    stack.pop();
                    fields.push((sym!(ContentList), IonValue::List(items)));
                }
                IonValue::Struct(fields)
            }
            other => other,
        }
    }

    /// Build one chapter's IR without touching importer state — with the
    /// one-time indexes in place, safe to call across threads over a shared
    /// `&self`. Returns the chapter and every element id it declares, in
    /// registration order; the caller stamps those into `eid_chapters`
    /// (first-wins, spine order — the mechanical route's structural
    /// `element_id_to_filename` walk).
    fn build_chapter(&self, id: ChapterId) -> io::Result<(Chapter, Vec<i64>)> {
        // Fixed-layout books build per-page chapters.
        if !self.fxl_pages.is_empty() {
            return self.build_fxl_page(id);
        }

        let section_name = self
            .section_names
            .get(id.0 as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Chapter not found"))?
            .clone();

        // Get storyline location
        let storyline_loc = self.resolve_section_to_storyline(&section_name)?;

        // Parse storyline entity
        let storyline_ion = self.parse_entity_ion(storyline_loc)?;

        // Every eid the storyline declares counts — whether or not the
        // element survives into the IR — so nav targets resolve to the
        // right file.
        let mut declared_eids = Vec::new();
        collect_declared_eids(&storyline_ion, &mut declared_eids);

        let symbols = Arc::clone(&self.symbols);
        let anchors = Arc::clone(&self.anchors);
        let styles = Arc::clone(&self.styles);
        let ruby_index = Arc::clone(&self.ruby_index);
        let anchor_table = Arc::clone(&self.anchor_table);

        // Parse storyline and build IR using schema-driven tokenization
        let mut chapter = parse_storyline_to_ir(
            &storyline_ion,
            symbols.as_ref(),
            Some(anchors.as_ref()),
            Some(styles.as_ref()),
            Some(ruby_index.as_ref()),
            Some(anchor_table.as_ref()),
            |name, index| self.lookup_content_text(name, index),
        );

        // Re-root under the section's main page-template container — the
        // mechanical route's body-level `<div>` — so anchors targeting the
        // template or the storyline root (a common page-list/TOC shape)
        // stamp onto a real element.
        let template = self
            .section_templates
            .get(&section_name)
            .cloned()
            .unwrap_or_default();
        if let Some(eid) = template.eid {
            declared_eids.push(eid);
        }
        let story_eid = storyline_ion
            .as_struct()
            .and_then(|f| get_field(f, sym!(Id)))
            .and_then(|v| v.as_int());
        crate::formats::kfx::storyline::apply_section_template(
            &mut chapter,
            template.eid,
            template.style.as_deref(),
            &template.inline_style,
            story_eid,
            Some(styles.as_ref()),
            Some(anchor_table.as_ref()),
        );

        // No generic `html::optimize` pass here: the KFX token→IR builder
        // already produces a tree that mirrors the mechanical route's
        // pre-consolidation DOM, and the shared
        // `export::epub::dom::consolidate_part` (run on both KFX→EPUB
        // routes) does the port-faithful cleanup.
        // Generic HTML-cleanup passes (list fusion, empty-node prune, span
        // merge) only DIVERGE from the reference — e.g. `fuse_lists` merged
        // two adjacent single-item `<ul>`s the port keeps separate — so they
        // must not run on the KFX path. `optimize` still serves the
        // HTML-sourced importers via `compile_html`.

        self.rewrite_image_srcs(&mut chapter);

        Ok((chapter, declared_eids))
    }

    /// Stamp a chapter's declared element ids into `eid_chapters`,
    /// first-registration-wins.
    fn register_eids(&mut self, id: ChapterId, eids: &[i64]) {
        for &eid in eids {
            self.eid_chapters.entry(eid).or_insert(id);
        }
    }

    /// Build one fixed-layout page as a chapter: take the page's leaf
    /// container from the cached spread walk, synthesize a storyline from
    /// the container's children (inline `content_list` wins over its story —
    /// the mechanical route's `process_content` order), and run the shared
    /// token→IR build with the container itself as the chapter's root — the
    /// same body-level `<div>` that route emits per page.
    fn build_fxl_page(&self, id: ChapterId) -> io::Result<(Chapter, Vec<i64>)> {
        let page = self
            .fxl_pages
            .get(id.0 as usize)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Chapter not found"))?;
        let (container, _) = self
            .fxl_leaves
            .get(&page.section)
            .and_then(|leaves| leaves.get(page.ordinal))
            .cloned()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Fixed-layout page not found")
            })?;

        let inner = container.unwrap_annotated();
        let Some(cfields) = inner.as_struct() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fixed-layout page container is not a struct",
            ));
        };

        // Children: inline content_list wins; else the container's story.
        let mut story_eid: Option<i64> = None;
        let children: Vec<IonValue> =
            if let Some(list) = get_field(cfields, sym!(ContentList)).and_then(|v| v.as_list()) {
                list.to_vec()
            } else if let Some(story) = get_field(cfields, sym!(StoryName))
                .and_then(|v| self.symbols.text_of(v))
                .map(|s| s.to_string())
            {
                let (root_eid, items) = self.storyline_parts(&story);
                story_eid = root_eid;
                items
            } else {
                Vec::new()
            };
        let children: Vec<IonValue> = children
            .into_iter()
            .map(|c| self.inline_story_refs(c, &mut Vec::new()))
            .collect();
        let synthetic = IonValue::Struct(vec![(sym!(ContentList), IonValue::List(children))]);

        // Every eid the page subtree declares — and the container's own —
        // counts for nav-target file resolution (the mechanical route's
        // per-page `collect_element_ids`); the caller registers them
        // first-wins across pages, matching its emission order.
        let container_eid = get_field(cfields, sym!(Id)).and_then(|v| v.as_int());
        let mut declared = Vec::new();
        collect_declared_eids(&synthetic, &mut declared);
        if let Some(eid) = container_eid {
            declared.push(eid);
        }

        let style_name = get_field(cfields, sym!(Style))
            .and_then(|v| self.symbols.text_of(v))
            .map(|s| s.to_string());
        let inline_style =
            crate::formats::kfx::yj_properties::convert_yj_properties(cfields, &self.symbols).items;

        let symbols = Arc::clone(&self.symbols);
        let anchors = Arc::clone(&self.anchors);
        let styles = Arc::clone(&self.styles);
        let ruby_index = Arc::clone(&self.ruby_index);
        let anchor_table = Arc::clone(&self.anchor_table);
        let mut chapter = parse_storyline_to_ir(
            &synthetic,
            symbols.as_ref(),
            Some(anchors.as_ref()),
            Some(styles.as_ref()),
            Some(ruby_index.as_ref()),
            Some(anchor_table.as_ref()),
            |name, index| self.lookup_content_text(name, index),
        );

        // Root the chapter at the page container — the reflowable
        // template's role: class from its `$157` style, inline style from
        // its converted outer fields, `(eid, 0)` anchors stamping onto the
        // root.
        crate::formats::kfx::storyline::apply_section_template(
            &mut chapter,
            container_eid,
            style_name.as_deref(),
            &inline_style,
            story_eid,
            Some(styles.as_ref()),
            Some(anchor_table.as_ref()),
        );

        self.rewrite_image_srcs(&mut chapter);
        Ok((chapter, declared))
    }

    /// Rewrite image references from KFX resource names ("eF") to the
    /// exported asset filenames ("image_rsrc7.jpg" / "cover.jpeg") so the IR
    /// speaks file paths, exactly like an EPUB-sourced book.
    fn rewrite_image_srcs(&self, chapter: &mut Chapter) {
        let node_ids: Vec<_> = chapter.iter_dfs().collect();
        for node_id in node_ids {
            let Some(filename) = chapter
                .semantics
                .src(node_id)
                .and_then(|src| self.image_by_name.get(src))
                .map(|&i| self.images[i].filename.clone())
            else {
                continue;
            };
            chapter.semantics.set_src(node_id, &filename);
        }
    }

    /// Derive the book-level writing mode and page-progression direction
    /// onto `metadata.{primary_writing_mode, page_progression_direction}`.
    ///
    /// `document_data.writing_mode` is only a default (see
    /// `kfx::writing_mode`): when it reads `horizontal_tb`, the style pool's
    /// majority vertical mode corrects it. Any `-rl` writing mode forces an
    /// RTL page turn — the common case for CJK vertical books, whose
    /// `direction` field literally says `ltr` — while an explicit
    /// `reading_orders[*].page_progression_direction` (captured by
    /// `parse_spine`) outranks both heuristics.
    fn derive_writing_direction(&mut self) {
        let mut writing_mode = "horizontal-tb".to_string();
        let mut ppd = "ltr".to_string();

        if let Some(loc) = self
            .entities
            .iter()
            .find(|e| e.type_id == KfxSymbol::DocumentData as u32)
            .copied()
            && let Ok(elem) = self.parse_entity_ion(loc)
            && let Some(fields) = elem.unwrap_annotated().as_struct()
        {
            if let Some(wm) =
                get_field(fields, sym!(WritingMode)).and_then(|v| self.symbols.text_of(v))
            {
                writing_mode =
                    crate::formats::kfx::writing_mode::normalize_writing_mode(wm).to_string();
            }
            if let Some(dir) =
                get_field(fields, sym!(Direction)).and_then(|v| self.symbols.text_of(v))
            {
                ppd = dir.to_string();
            }
        }

        if writing_mode == "horizontal-tb" {
            let styles: Vec<IonValue> = self
                .entities
                .iter()
                .filter(|e| e.type_id == KfxSymbol::Style as u32)
                .filter_map(|loc| self.parse_entity_ion(*loc).ok())
                .collect();
            if let Some(vertical) = crate::formats::kfx::writing_mode::majority_vertical_mode(
                styles.iter(),
                &self.symbols,
            ) {
                writing_mode = vertical;
            }
        }
        if writing_mode.ends_with("-rl") {
            ppd = "rtl".to_string();
        }
        // The content walk (spread pairing) alternates from the value BEFORE
        // the explicit reading-order override — the mechanical route's
        // `extract_doc_data` chain ends here.
        self.content_ppd = ppd.clone();
        // `$default` defers to the reader, i.e. to the heuristics above —
        // only a concrete direction overrides them.
        if let Some(explicit) = self
            .metadata
            .page_progression_direction
            .as_deref()
            .filter(|d| matches!(*d, "rtl" | "ltr"))
        {
            ppd = explicit.to_string();
        }

        self.metadata.primary_writing_mode =
            crate::formats::epub::opf_meta::primary_writing_mode(Some(&writing_mode), Some(&ppd));
        self.metadata.page_progression_direction = Some(ppd);
        self.css_writing_mode = writing_mode;
    }

    /// Resolve an eid to its owning chapter's start: the structural
    /// `eid_chapters` walk first (storyline-declared ids, accumulated as
    /// chapters load), then the section-declared ids (page-template
    /// containers, known from construction).
    fn resolve_eid_chapter(&self, eid: i64) -> Option<AnchorTarget> {
        if let Some(&chapter) = self.eid_chapters.get(&eid) {
            return Some(AnchorTarget::Internal(GlobalNodeId::new(
                chapter,
                crate::model::NodeId::ROOT,
            )));
        }
        let section = self.section_eids.get(&eid)?;
        // Per-page fixed-layout spine: a section-declared eid resolves to
        // the section's first page.
        let idx = if self.fxl_pages.is_empty() {
            self.section_names.iter().position(|s| s == section)?
        } else {
            self.fxl_pages.iter().position(|p| &p.section == section)?
        };
        Some(AnchorTarget::Internal(GlobalNodeId::new(
            ChapterId(idx as u32),
            crate::model::NodeId::ROOT,
        )))
    }

    /// Resolve a section name to its storyline entity location.
    fn resolve_section_to_storyline(&self, section_name: &str) -> io::Result<EntityLoc> {
        self.section_storylines
            .get(section_name)
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Could not resolve section: {}", section_name),
                )
            })
    }

    /// Build the section name → storyline location cache.
    fn index_section_storylines(&mut self) -> io::Result<()> {
        if self.section_storylines_indexed {
            return Ok(());
        }

        // First, build a map of story_name → storyline EntityLoc
        let mut storyline_map: HashMap<String, EntityLoc> = HashMap::new();
        for loc in &self.entities {
            if loc.type_id == KfxSymbol::Storyline as u32
                && let Ok(elem) = self.parse_entity_ion(*loc)
                && let Some(fields) = elem.as_struct()
                && let Some(name) =
                    get_field(fields, sym!(StoryName)).and_then(|v| self.get_symbol_text(v))
            {
                storyline_map.insert(name.to_string(), *loc);
            }
        }

        // Then, map each section to its storyline, and record the element
        // ids the section struct itself declares (page-template containers)
        // so navigation targets pointing at them resolve to the section.
        // The MAIN template is the LAST entry in `page_templates` (calibre's
        // rule; earlier entries are conditional templates); its own id and
        // style ride onto the chapter's root container.
        let mut section_eids: Vec<(i64, String)> = Vec::new();
        let mut section_templates: Vec<(String, SectionTemplate)> = Vec::new();
        for loc in &self.entities {
            if loc.type_id == KfxSymbol::Section as u32
                && let Ok(elem) = self.parse_entity_ion(*loc)
                && let Some(fields) = elem.as_struct()
            {
                let section_name =
                    get_field(fields, sym!(SectionName)).and_then(|v| self.get_symbol_text(v));

                let main_template = get_field(fields, sym!(PageTemplates))
                    .and_then(|v| v.as_list())
                    .and_then(|templates| templates.last())
                    .and_then(|t| t.as_struct());
                let story_name = main_template
                    .and_then(|f| get_field(f, sym!(StoryName)))
                    .and_then(|v| self.get_symbol_text(v));

                if let Some(sec_name) = section_name {
                    let mut eids = Vec::new();
                    collect_declared_eids(&elem, &mut eids);
                    for eid in eids {
                        section_eids.push((eid, sec_name.to_string()));
                    }
                    if let Some(tf) = main_template {
                        let template = SectionTemplate {
                            eid: get_field(tf, sym!(Id)).and_then(|v| v.as_int()),
                            style: get_field(tf, sym!(Style))
                                .and_then(|v| self.get_symbol_text(v))
                                .map(|s| s.to_string()),
                            inline_style:
                                crate::formats::kfx::yj_properties::convert_yj_properties(
                                    tf,
                                    &self.symbols,
                                )
                                .items,
                        };
                        section_templates.push((sec_name.to_string(), template));
                    }
                }

                if let (Some(sec_name), Some(story_name)) = (section_name, story_name)
                    && let Some(storyline_loc) = storyline_map.get(story_name)
                {
                    self.section_storylines
                        .insert(sec_name.to_string(), *storyline_loc);
                }
            }
        }
        for (eid, sec_name) in section_eids {
            self.section_eids.entry(eid).or_insert(sec_name);
        }
        for (sec_name, template) in section_templates {
            self.section_templates.entry(sec_name).or_insert(template);
        }
        // Retained for story-name resolution outside the section hop (spread
        // pages, container story references on the fixed-layout path).
        self.storylines_by_name = storyline_map;

        self.section_storylines_indexed = true;
        Ok(())
    }

    /// Extract section names + `page_progression_direction` from
    /// reading_orders in document_data or metadata. Prefers the "default"
    /// reading order if multiple are present.
    ///
    /// Walks both `document_data` ($538) and `metadata` ($258) — boko's own
    /// exports put ppd only on the `metadata` fragment while sections are in
    /// both, so we collect from each rather than returning on the first hit.
    fn get_reading_order_sections(&self) -> io::Result<(Vec<String>, Option<String>)> {
        let candidate_types = [KfxSymbol::DocumentData as u32, KfxSymbol::Metadata as u32];
        let mut found_sections: Vec<String> = Vec::new();
        let mut found_ppd: Option<String> = None;

        for type_id in candidate_types {
            let loc = self.entities.iter().find(|e| e.type_id == type_id).copied();
            let Some(loc) = loc else { continue };
            let Ok(elem) = self.parse_entity_ion(loc) else {
                continue;
            };
            let Some(fields) = elem.as_struct() else {
                continue;
            };
            let Some(orders) = get_field(fields, sym!(ReadingOrders)).and_then(|v| v.as_list())
            else {
                continue;
            };

            // Prefer reading_order_name == "default"; fall back to first with sections.
            let chosen = orders
                .iter()
                .find(|o| {
                    o.as_struct()
                        .map(|f| {
                            get_field(f, sym!(ReadingOrderName))
                                .and_then(|v| self.get_symbol_text(v))
                                == Some("default")
                        })
                        .unwrap_or(false)
                })
                .or_else(|| orders.iter().find(|o| o.as_struct().is_some()))
                .and_then(|o| o.as_struct());

            if let Some(order_fields) = chosen {
                if found_sections.is_empty()
                    && let Some(sections) = self.extract_sections(order_fields)
                {
                    found_sections = sections;
                }
                if found_ppd.is_none() {
                    found_ppd = self.extract_ppd(order_fields);
                }
            }

            if !found_sections.is_empty() && found_ppd.is_some() {
                break;
            }
        }

        Ok((found_sections, found_ppd))
    }

    /// Extract `page_progression_direction` from a reading_order struct.
    /// The KFX value is a symbol (`$rtl`/`$ltr`/`$default`); we strip the
    /// leading `$` for the EPUB `<spine>` attribute.
    fn extract_ppd(&self, order_fields: &[(u64, IonValue)]) -> Option<String> {
        let raw = get_field(order_fields, sym!(PageProgressionDirection))
            .and_then(|v| self.get_symbol_text(v))?;
        let dir = raw.strip_prefix('$').unwrap_or(raw);
        match dir {
            "rtl" | "ltr" | "default" => Some(dir.to_string()),
            _ => None,
        }
    }

    /// Extract section names from a reading order struct.
    fn extract_sections(&self, order_fields: &[(u64, IonValue)]) -> Option<Vec<String>> {
        let sections = get_field(order_fields, sym!(Sections))?.as_list()?;
        let mut section_names = Vec::new();
        for section in sections {
            if let Some(name) = self.get_symbol_text(section) {
                section_names.push(name.to_string());
            }
        }
        if section_names.is_empty() {
            None
        } else {
            Some(section_names)
        }
    }

    /// Resolve cover_image value which can be a string or list with symbol/string reference.
    fn resolve_cover_value(&self, value: Option<&IonValue>) -> Option<String> {
        let value = value?;

        // Format 1: Direct string
        if let Some(s) = value.as_string() {
            return Some(s.to_string());
        }

        // Format 2: List containing a symbol or string reference
        if let Some(list) = value.as_list()
            && let Some(first) = list.first()
            && let Some(text) = self.get_symbol_text(first)
        {
            return Some(text.to_string());
        }

        None
    }

    /// Look up text content by name and index.
    ///
    /// Lazily loads and caches content entities as needed. `&self` so
    /// parallel chapter builds can share the importer; a race on the same
    /// name parses the entity twice and caches identical lists — harmless.
    fn lookup_content_text(&self, name: &str, index: usize) -> Option<String> {
        // Check cache first
        {
            let cache = self.content_cache.read().unwrap_or_else(|e| e.into_inner());
            if let Some(content_list) = cache.get(name) {
                return content_list.get(index).cloned();
            }
        }

        // Load and cache the content entity
        if let Some(content_list) = self.load_content_entity(name) {
            let result = content_list.get(index).cloned();
            self.content_cache
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name.to_string(), content_list);
            return result;
        }

        None
    }

    /// Load a content entity by name and return its string list.
    fn load_content_entity(&self, name: &str) -> Option<Vec<String>> {
        let loc = self.content_by_name.get(name)?;
        let elem = self.parse_entity_ion(*loc).ok()?;
        let fields = elem.as_struct()?;
        let list = get_field(fields, sym!(ContentList)).and_then(|v| v.as_list())?;
        Some(
            list.iter()
                .filter_map(|v| v.as_string().map(|s| s.to_string()))
                .collect(),
        )
    }

    /// Build the content (`$145`) name → location index: one pass, parsing
    /// each content entity once. The old lookup scanned and parsed every
    /// content entity per cache miss — O(entities) per distinct story name.
    fn index_content_entities(&mut self) {
        let locs: Vec<EntityLoc> = self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::Content as u32)
            .copied()
            .collect();
        for loc in locs {
            if let Ok(elem) = self.parse_entity_ion(loc)
                && let Some(fields) = elem.as_struct()
                // The scan this replaces skipped a name-matching entity
                // with no content_list; only list-bearing entities index.
                && get_field(fields, sym!(ContentList)).is_some_and(|v| v.as_list().is_some())
                && let Some(name) = get_field(fields, sym!(Name))
                    .and_then(|v| self.get_symbol_text(v))
                    .map(|s| s.to_string())
            {
                self.content_by_name.entry(name).or_insert(loc);
            }
        }
    }

    /// Index external resources.
    fn index_resources(&mut self) -> io::Result<()> {
        if self.resources_indexed {
            return Ok(());
        }

        // Build a lookup from resolved bcRawMedia entity names to their binary payload locations.
        // bcRawMedia stores its name as a symbol ID in its own container's doc_symbols.
        let mut raw_media_by_name: HashMap<String, EntityLoc> = HashMap::new();
        for raw_loc in self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::Bcrawmedia as u32)
            .copied()
        {
            if let Some(name) = self.symbols.resolve_opt(raw_loc.id as u64) {
                raw_media_by_name.insert(name.to_string(), raw_loc);
                if let Some(rest) = name.strip_prefix("resource/") {
                    raw_media_by_name.insert(rest.to_string(), raw_loc);
                } else {
                    raw_media_by_name.insert(format!("resource/{name}"), raw_loc);
                }
            }
        }

        // Collect entities to process to avoid borrow conflicts
        let locs: Vec<_> = self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::ExternalResource as u32)
            .copied()
            .collect();

        for loc in locs {
            if let Ok(elem) = self.parse_entity_ion(loc)
                && let Some(fields) = elem.as_struct()
            {
                // Use location as key (e.g., "resource/rsrc7")
                let location = get_field(fields, sym!(Location))
                    .and_then(|v| v.as_string())
                    .map(|s| s.to_string());

                // Also index by resource_name (e.g., "eF") for cover lookup
                let name = get_field(fields, sym!(ResourceName))
                    .and_then(|v| self.symbols.text_of_opt(v))
                    .map(|s| s.to_string());

                let resolved_loc = location
                    .as_ref()
                    .and_then(|key| raw_media_by_name.get(key).copied())
                    .or_else(|| {
                        name.as_ref()
                            .and_then(|key| raw_media_by_name.get(key).copied())
                    })
                    .unwrap_or(loc);

                if let Some(loc_str) = &location
                    && !loc_str.is_empty()
                {
                    self.resources.insert(loc_str.clone(), resolved_loc);
                }
                if let Some(name_str) = &name
                    && !name_str.is_empty()
                    && Some(name_str) != location.as_ref()
                {
                    self.resources.insert(name_str.clone(), resolved_loc);
                }
            }
        }

        self.resources_indexed = true;
        Ok(())
    }

    /// Build the canonical image list via the shared external_resource walk
    /// (`kfx::resource_index`) — the same code the mechanical converter runs,
    /// so filenames, order, and format predictions match it byte-for-byte.
    /// Resolves the cover (declared metadata name, falling back to the first
    /// reading-order section's full-page image), renames it to `cover.<ext>`,
    /// and rewrites `metadata.cover_image` to the exported filename.
    fn build_image_index(&mut self) {
        // bcRawMedia payloads keyed by their resolved entity symbol name —
        // the exact string `external_resource.location` carries.
        let mut media: HashMap<String, EntityLoc> = HashMap::new();
        for loc in self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::Bcrawmedia as u32)
        {
            let name = self.symbols.resolve(loc.id as u64);
            if !name.is_empty() && name != "?" {
                media.insert(name.to_string(), *loc);
            }
        }

        // Parse every external_resource fragment up front (tiny Ion structs).
        let fragments: Vec<(String, IonValue)> = self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::ExternalResource as u32)
            .filter_map(|loc| {
                let ion = self.parse_entity_ion(*loc).ok()?;
                Some((
                    resource_index::entity_fid(loc.id as u64, &self.symbols),
                    ion,
                ))
            })
            .collect();

        let source = Arc::clone(&self.source);
        let mut images = resource_index::build_image_index(
            fragments.iter().map(|(fid, v)| (fid.as_str(), v)).collect(),
            &self.symbols,
            |location| {
                let loc = media.get(location)?;
                let head_len = loc.length.min(64);
                let bytes = source.read_at(loc.offset as u64, head_len).ok()?;
                Some(skip_enty_header(&bytes).to_vec())
            },
        );

        let mut by_name: HashMap<String, usize> = HashMap::new();
        for (i, img) in images.iter().enumerate() {
            by_name.insert(img.resource_name.clone(), i); // duplicates: last wins
        }

        // Cover: metadata's declared resource name, else the first
        // reading-order section's full-page image (Amazon KFX often carries
        // the cover only as that "loc 0" cover page).
        let cover_name = self
            .metadata
            .cover_image
            .clone()
            .or_else(|| self.first_section_cover_candidate(&images));
        if let Some(name) = cover_name
            && let Some(&idx) = by_name.get(&name)
        {
            images[idx].filename = resource_index::cover_filename(&images[idx].filename);
            self.metadata.cover_image = Some(images[idx].filename.clone());
        }

        self.image_by_filename = images
            .iter()
            .enumerate()
            .map(|(i, img)| (img.filename.clone(), i))
            .collect();
        // Asset list: exported image filenames (replacing the raw `#id`
        // bcRawMedia placeholders) + the font paths built in `from_source`.
        let fonts: Vec<PathBuf> = self
            .asset_paths
            .iter()
            .filter(|p| p.to_string_lossy().starts_with("fonts/"))
            .cloned()
            .collect();
        self.asset_paths = images
            .iter()
            .map(|img| PathBuf::from(&img.filename))
            .chain(fonts)
            .collect();
        self.image_by_name = by_name;
        self.image_media = media;
        self.images = images;
    }

    /// Cover fallback: the first `resource_name` laid out by the first
    /// reading-order section's storyline, accepted only when it names a
    /// raster image (a PDF-backed first section is not a cover). Read-only
    /// core of calibre kfxlib's `check_cover_section_and_storyline`.
    fn first_section_cover_candidate(&self, images: &[ImageResource]) -> Option<String> {
        let first_section = self.section_names.first()?;
        let storyline_loc = *self.section_storylines.get(first_section)?;
        let storyline_ion = self.parse_entity_ion(storyline_loc).ok()?;
        let fields = storyline_ion.unwrap_annotated().as_struct()?;
        let content_list = get_field(fields, sym!(ContentList))?;
        let candidate = resource_index::first_content_resource_name(content_list, &self.symbols)?;
        resource_index::is_raster_cover(images, &candidate).then_some(candidate)
    }

    /// Bytes for `images[idx]` as exported: JPEG-XR sources are transcoded to
    /// JPEG (decode failures pass through unchanged, same policy as the
    /// mechanical route); every other format is copied verbatim.
    fn load_image_bytes(&self, idx: usize) -> io::Result<Vec<u8>> {
        let img = &self.images[idx];
        let raw = self.read_image_raw(idx)?;
        if img.is_jxr {
            crate::image::jxr_transcode::transcode(&raw, &img.resource_name)
                .map(|(bytes, _format, _timing)| bytes)
                .map_err(|e| io::Error::other(e.to_string()))
        } else {
            Ok(raw)
        }
    }

    /// Raw (pre-transcode) bcRawMedia bytes for `images[idx]`.
    fn read_image_raw(&self, idx: usize) -> io::Result<Vec<u8>> {
        let img = &self.images[idx];
        let loc = self.image_media.get(&img.location).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("missing bcRawMedia at {:?}", img.location),
            )
        })?;
        self.read_entity(*loc)
    }

    /// Index anchor entities to build anchor_name → uri/position maps.
    ///
    /// This enables resolution of both external and internal links where
    /// `link_to` contains an anchor name.
    fn index_anchor_entities(&mut self) -> io::Result<()> {
        if self.anchors_indexed {
            return Ok(());
        }

        // Real `$266` anchors, registered in sorted-name order — the same
        // deterministic rule the mechanical route uses, so a position carrying
        // several anchors picks the same first (= stamped) name on both
        // engines.
        let locs: Vec<_> = self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::Anchor as u32)
            .copied()
            .collect();
        let mut parsed: Vec<(String, IonValue)> = Vec::with_capacity(locs.len());
        for loc in locs {
            if let Ok(elem) = self.parse_entity_ion(loc)
                && let Some(fields) = elem.as_struct()
                && let Some(name) = get_field(fields, sym!(AnchorName))
                    .and_then(|v| self.symbols.text_of_opt(v))
                    .map(|s| s.to_string())
            {
                parsed.push((name, elem.clone()));
            }
        }
        parsed.sort_by(|(a, _), (b, _)| a.cmp(b));

        let mut table = AnchorTable::default();
        for (name, elem) in &parsed {
            if let Some(fields) = elem.as_struct() {
                table.register_anchor_fields(name, fields);
            }
        }

        // Synthetic anchors at nav target positions: TOC first, then
        // page-list (a page break on a TOC-claimed position reuses the TOC
        // anchor), plus `$798` heading levels. Runs on the RAW nav entries —
        // display filtering (empty labels, placeholder units) must not change
        // the stamped-id set.
        if let Some(loc) = self
            .entities
            .iter()
            .find(|e| e.type_id == KfxSymbol::BookNavigation as u32)
            .copied()
            && let Ok(nav) = self.parse_entity_ion(loc)
        {
            let nav_values = [nav];
            register_heading_levels(
                &mut table,
                nav_values.iter(),
                |c| self.resolve_nav_container(c),
                &self.symbols,
            );
            register_nav_synthetics(
                &mut table,
                nav_values.iter(),
                |c| self.resolve_nav_container(c),
                &self.symbols,
                "toc",
                "toc",
            );
            register_nav_synthetics(
                &mut table,
                nav_values.iter(),
                |c| self.resolve_nav_container(c),
                &self.symbols,
                "page_list",
                "page",
            );
        }

        // The external-URI map keeps its own handle: the storyline tokenizer
        // resolves `link_to` names through it.
        self.anchors = Arc::new(table.anchor_uri.clone());
        self.anchor_table = Arc::new(table);

        self.anchors_indexed = true;
        Ok(())
    }

    /// Index style entities to build style_name → properties map.
    ///
    /// This enables resolution of style references in storyline elements.
    /// Style entities ($157) contain properties like font_weight, text_alignment, margins, etc.
    fn index_styles(&mut self) -> io::Result<()> {
        if self.styles_indexed {
            return Ok(());
        }

        // Find all style entities (type $157)
        let locs: Vec<_> = self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::Style as u32)
            .copied()
            .collect();

        let mut new_styles = Vec::new();

        for loc in locs {
            if let Ok(elem) = self.parse_entity_ion(loc)
                && let Some(fields) = elem.as_struct()
            {
                // Get style_name
                let style_name = get_field(fields, sym!(StyleName))
                    .and_then(|v| self.symbols.text_of_opt(v))
                    .map(|s| s.to_string());

                if let Some(name) = style_name {
                    // Store all fields (cloned) for later interpretation
                    let props: Vec<(u64, IonValue)> = fields
                        .iter()
                        .filter(|(k, _)| *k != sym!(StyleName)) // Exclude the name itself
                        .map(|(k, v)| (*k, v.clone()))
                        .collect();

                    new_styles.push((name, props));
                }
            }
        }

        if !new_styles.is_empty() {
            let styles = Arc::make_mut(&mut self.styles);
            for (name, props) in new_styles {
                styles.insert(name, props);
            }
        }

        self.styles_indexed = true;
        Ok(())
    }

    /// Index `ruby_content` entities (type $756) into `ruby_index`.
    ///
    /// Each ruby_content entity has shape:
    /// ```ion
    /// {
    ///   ruby_name: 'b_ruby_0',
    ///   content_list: [
    ///     { ruby_id: 1, content: "かな", ... },
    ///     ...
    ///   ]
    /// }
    /// ```
    /// We slot each entry's `content` string into a vec at position
    /// `ruby_id - 1` (KFX uses 1-indexed ruby_id) so style_events can read it
    /// with `ruby_index[ruby_name][ruby_id - 1]`.
    fn index_ruby_content(&mut self) -> io::Result<()> {
        if self.ruby_indexed {
            return Ok(());
        }

        let locs: Vec<_> = self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::RubyContent as u32)
            .copied()
            .collect();

        let mut new_entries: Vec<(String, Vec<String>)> = Vec::new();

        for loc in locs {
            if let Ok(elem) = self.parse_entity_ion(loc)
                && let Some(fields) = elem.unwrap_annotated().as_struct()
            {
                let ruby_name = get_field(fields, sym!(RubyName))
                    .and_then(|v| self.symbols.text_of_opt(v))
                    .map(|s| s.to_string());

                let Some(name) = ruby_name else {
                    continue;
                };

                let Some(content_list) =
                    get_field(fields, sym!(ContentList)).and_then(|v| v.as_list())
                else {
                    continue;
                };

                // Collect (ruby_id, text). ruby_id is 1-indexed; build a dense
                // vec by max id so direct subscript works in parse_style_events.
                let mut pairs: Vec<(usize, String)> = Vec::with_capacity(content_list.len());
                let mut max_id = 0usize;
                for entry in content_list {
                    let Some(entry_fields) = entry.as_struct() else {
                        continue;
                    };
                    let ruby_id = get_field(entry_fields, sym!(RubyId))
                        .and_then(|v| v.as_int())
                        .map(|n| n as usize);
                    let content = get_field(entry_fields, sym!(Content))
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_string());
                    if let (Some(id_n), Some(text)) = (ruby_id, content) {
                        if id_n > max_id {
                            max_id = id_n;
                        }
                        pairs.push((id_n, text));
                    }
                }

                let mut annotations = vec![String::new(); max_id];
                for (id_n, text) in pairs {
                    if id_n >= 1 && id_n <= annotations.len() {
                        annotations[id_n - 1] = text;
                    }
                }

                if !annotations.is_empty() {
                    new_entries.push((name, annotations));
                }
            }
        }

        if !new_entries.is_empty() {
            let ruby = Arc::make_mut(&mut self.ruby_index);
            for (name, anns) in new_entries {
                ruby.insert(name, anns);
            }
        }

        self.ruby_indexed = true;
        Ok(())
    }
}

/// The main page template's identity within one section: its own `$155` id,
/// `$157 style` name, and the CSS declarations converted from its remaining
/// outer fields (a per-section `writing_mode` is the common one), carried
/// onto the chapter's root container.
#[derive(Debug, Default, Clone)]
struct SectionTemplate {
    eid: Option<i64>,
    style: Option<String>,
    inline_style: Vec<(String, String)>,
}

/// One fixed-layout page's identity: the owning section and the page's leaf
/// index in that section's spread walk (see `expand_fxl_spine`).
#[derive(Debug, Clone)]
struct FxlPage {
    section: String,
    ordinal: usize,
}

/// Parse a `#eid[:offset]` nav placeholder href into its position. Returns
/// `None` for anything that isn't a purely numeric internal position (real
/// file paths, anchor names, external URLs).
fn parse_position_href(href: &str) -> Option<(i64, i64)> {
    let rest = href.trim().strip_prefix('#')?;
    let (eid_str, off_str) = match rest.find(':') {
        Some(pos) => (&rest[..pos], Some(&rest[pos + 1..])),
        None => (rest, None),
    };
    let eid: i64 = eid_str.parse().ok()?;
    let offset: i64 = match off_str {
        Some(s) => s.parse().ok()?,
        None => 0,
    };
    Some((eid, offset))
}

/// Collect every element id (`$155`) declared anywhere inside `value` — used
/// on section entities, whose page-template containers carry ids that never
/// appear in storyline content.
fn collect_declared_eids(value: &IonValue, out: &mut Vec<i64>) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            if let Some(id) = get_field(fields, sym!(Id)).and_then(|v| v.as_int()) {
                out.push(id);
            }
            for (_, v) in fields {
                collect_declared_eids(v, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                collect_declared_eids(item, out);
            }
        }
        _ => {}
    }
}
