//! KFX format importer.
//!
//! KFX is Amazon's Kindle Format 10, using Ion binary data format.

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
use crate::formats::kfx::position::PositionFragments;
use crate::formats::kfx::resource_index::{self, ImageResource};
use crate::formats::kfx::schema::schema;
use crate::formats::kfx::storyline::{SectionTemplate, Styles, parse_storyline_to_ir};
use crate::formats::kfx::structure::{self, ContentSource};
use crate::formats::kfx::symbols::KfxSymbol;
use crate::import::{ChapterId, CssProgram, Importer, SpineEntry};
use crate::io::{ByteSource, FileSource};
use crate::model::Chapter;
use crate::model::{
    AnchorTarget, CollectionInfo, Contributor, GlobalNodeId, Landmark, Metadata, OrientationLock,
    PageSpread, PositionMap, SourceText, TocEntry,
};
use crate::style::CssDecl;

/// Shorthand for getting a KfxSymbol as u32 for field lookups.
macro_rules! sym {
    ($variant:ident) => {
        KfxSymbol::$variant as u64
    };
}

/// KFX format importer, over one container. `Book::open_format` merges a
/// `.kfx-zip` bundle into a single in-memory container ahead of it.
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
    /// when the KFX carries no page numbers. A KFX→KFX reconvert and the
    /// IR-based export both re-emit it.
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

    /// Element ids declared on section entities → owning section name. Chapter
    /// content holds none of them, and a navigation target naming one falls
    /// back to the section's chapter start.
    section_eids: HashMap<i64, String>,

    /// Section name → the last page template's element id and `$style` name.
    /// The template is the chapter's root container, and an anchor at
    /// `(template_eid, offset)` stamps onto it.
    section_templates: HashMap<String, SectionTemplate>,

    /// Fixed-layout page records parallel to `spine` and `section_names`, one
    /// per page: `section` names the owning section and `ordinal` the leaf
    /// index in its spread walk. Empty on a reflowable book.
    fxl_pages: Vec<FxlPage>,
    /// Section name → its spread-walked leaf pages, each a container and the
    /// spread half it occupies, from the section's first page template.
    /// Walked once at spine expansion; a per-page load indexes it by ordinal.
    fxl_leaves: HashMap<String, Vec<(IonValue, Option<PageSpread>)>>,
    /// story_name → storyline entity location, for resolving spread pages
    /// and container story references without a section hop.
    storylines_by_name: HashMap<String, EntityLoc>,
    /// Structure ($608) entity name → location, for `page_templates` entries
    /// holding a symbol reference.
    structures_by_name: HashMap<String, EntityLoc>,
    /// Content-walk page-progression direction: `document_data.direction` plus
    /// the vertical-writing-mode → rtl override, without the reading-order
    /// override `derive_writing_direction` applies to the OPF value.
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

    /// Content (`$145`) entity name → location, built once in `from_source`.
    /// Text lookups resolve by direct index; first entity wins on duplicate
    /// names.
    content_by_name: HashMap<String, EntityLoc>,
    /// Content cache: name -> list of strings (lazily populated). Behind a
    /// lock, letting `lookup_content_text` work from `&self` — chapter builds
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

    /// The shared KFX anchor table: real `$266` anchors plus synthetic
    /// toc/page anchors at nav target positions. `index_anchor_entities`
    /// builds it and `load_chapter` stamps `semantics.id` from it.
    anchor_table: Arc<AnchorTable>,

    /// Maps element string ID -> GlobalNodeId (built during index_anchors).
    /// Ids are the STAMPED anchor ids (`a85J`, `toc-148-0`, …), the same
    /// namespace the emitted XHTML carries.
    element_id_map: HashMap<String, GlobalNodeId>,

    /// Element id (`$155`) → owning chapter, accumulated as chapters load.
    /// Every eid the parsed storyline holds counts, and a nav target
    /// resolves its file here.
    eid_chapters: HashMap<i64, ChapterId>,

    /// Doc-level CSS writing mode from `derive_writing_direction`, feeding the
    /// stylesheet's `body { writing-mode: … }` header.
    /// `metadata.primary_writing_mode` holds the OPF-vocabulary form.
    css_writing_mode: String,

    /// Worker-thread cap for the parallel chapter build and image transcode,
    /// `0` for the platform's reported parallelism.
    max_workers: usize,
}

impl From<ContainerError> for io::Error {
    fn from(e: ContainerError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e.to_string())
    }
}

/// Content references resolve through the entity index, parsing and caching
/// one content entity per distinct name.
impl ContentSource for KfxImporter {
    fn symbols(&self) -> &SymbolTable {
        &self.symbols
    }

    fn content_string(&self, name: &str, index: usize) -> Option<String> {
        self.lookup_content_text(name, index)
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

    fn landmarks_mut(&mut self) -> &mut [Landmark] {
        &mut self.landmarks
    }

    fn spine(&self) -> &[SpineEntry] {
        &self.spine
    }

    fn source_id(&self, id: ChapterId) -> Option<&str> {
        self.section_names.get(id.0 as usize).map(|s| s.as_str())
    }

    fn chapter_title(&self, id: ChapterId) -> Option<&str> {
        // A fixed-layout page is titled by its owning section.
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

        // Each chapter build is pure. Eid registration is first-wins in spine
        // order and runs serially over `ids` after the parallel phase.
        let built = crate::util::parallel_map(ids, self.max_workers, |id| self.build_chapter(*id));
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

    fn asset_manifest(&mut self) -> Option<Vec<crate::import::AssetInfo>> {
        // Dimensions come off the `external_resource` fragment: no image
        // is read.
        Some(
            self.images
                .iter()
                .map(|img| crate::import::AssetInfo {
                    path: PathBuf::from(&img.filename),
                    media_type: img.mime.clone(),
                    width: img.width,
                    height: img.height,
                })
                .collect(),
        )
    }

    fn load_assets(&mut self, paths: &[PathBuf]) -> Vec<io::Result<Vec<u8>>> {
        // Resolve raw bytes serially, then run the JPEG-XR→JPEG transcodes
        // in parallel across cores.
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
        let transcoded = crate::util::parallel_map(&jxr_jobs, self.max_workers, |(_, idx, raw)| {
            crate::formats::kfx::jxr::transcode(raw, &self.images[*idx].resource_name)
                .map(|(bytes, _format, _timing)| bytes)
                .map_err(|e| io::Error::other(e.to_string()))
        });
        for ((slot, _, _), result) in jxr_jobs.iter().zip(transcoded) {
            results[*slot] = Some(result);
        }
        results.into_iter().map(|r| r.expect("filled")).collect()
    }

    fn load_assets_stored(
        &mut self,
        paths: &[PathBuf],
    ) -> Vec<io::Result<(Vec<u8>, Option<String>)>> {
        paths
            .iter()
            .map(|path| {
                let name = path.to_string_lossy();
                match self.image_by_filename.get(&*name).copied() {
                    Some(idx) if self.images[idx].is_jxr => self
                        .read_image_raw(idx)
                        .map(|raw| (raw, Some("jxr".into()))),
                    _ => self.load_asset(path).map(|bytes| (bytes, None)),
                }
            })
            .collect()
    }

    fn requires_normalized_export(&self) -> bool {
        // KFX load_raw returns binary Ion data, not HTML
        true
    }

    /// The `eid → pid → Location` chain, read straight out of the container's
    /// four position fragments. Amazon ships these on books it produced; a KFX
    /// converted from an EPUB carries none, and reports no scale.
    fn position_map(&mut self) -> Option<PositionMap> {
        let locs: Vec<EntityLoc> = self
            .entities
            .iter()
            .filter(|e| PositionFragments::wants(e.type_id))
            .copied()
            .collect();
        let parsed: Vec<(u32, IonValue)> = locs
            .into_iter()
            .filter_map(|loc| Some((loc.type_id, self.parse_entity_ion(loc).ok()?)))
            .collect();
        let mut fragments = PositionFragments::default();
        for (type_id, value) in &parsed {
            fragments.push(*type_id, value);
        }
        fragments.build()
    }

    /// Every storyline's `eid → base text`, indexed against the position scale
    /// a highlight spanning several elements walks.
    fn source_text(&mut self) -> Option<SourceText> {
        let positions = self.position_map().unwrap_or_default();
        let locs: Vec<EntityLoc> = self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::Storyline as u32)
            .copied()
            .collect();
        let mut text_of = HashMap::new();
        for loc in locs {
            if let Ok(story) = self.parse_entity_ion(loc) {
                structure::collect_eid_text(&story, self, &mut text_of);
            }
        }
        if text_of.is_empty() {
            return None;
        }
        Some(SourceText::new(text_of, &positions))
    }

    fn set_max_workers(&mut self, workers: usize) {
        self.max_workers = workers;
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

        // An internal anchor name resolves through the anchor table to the
        // node its stamped id landed on. An unstamped position falls through
        // to eid resolution.
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
        use crate::formats::kfx::yj_properties::{convert_yj_link_states, convert_yj_properties};
        self.index_styles().ok()?;
        let named = self
            .styles
            .iter()
            .map(|(name, fields)| {
                let mut decl = convert_yj_properties(fields, &self.symbols);
                // A `background-image` names a KFX resource; the sheet ships
                // beside the exported files and points at those.
                self.rewrite_css_image_urls(&mut decl);
                (name.clone(), decl)
            })
            .collect();
        // `link_unvisited_style` and `link_visited_style` are nested styles
        // for one link state each, and become their own pseudo-class rules.
        let pseudo = self
            .styles
            .iter()
            .filter_map(|(name, fields)| {
                let states = convert_yj_link_states(fields, &self.symbols);
                (!states.is_empty()).then(|| {
                    let rules = states
                        .into_iter()
                        .map(|(p, decl)| (p.to_string(), decl))
                        .collect();
                    (name.clone(), rules)
                })
            })
            .collect();
        Some(CssProgram {
            named,
            pseudo,
            writing_mode: self.css_writing_mode.clone(),
            fixed_layout: self.metadata.fixed_layout,
        })
    }

    fn writing_mode(&mut self) -> crate::style::WritingMode {
        self.document_axis()
    }
}

impl KfxImporter {
    /// `document_data.writing_mode`, resolved at open. A `-rl` horizontal mode
    /// is a page-progression value and lays out as `horizontal-tb`.
    fn document_axis(&self) -> crate::style::WritingMode {
        match self.css_writing_mode.as_str() {
            "vertical-rl" => crate::style::WritingMode::VerticalRl,
            "vertical-lr" => crate::style::WritingMode::VerticalLr,
            _ => crate::style::WritingMode::HorizontalTb,
        }
    }

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

        // `from_fragment` seats the doc-symbol base at the container's declared
        // import max_id (§5.4), never at the static table's length.
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
            max_workers: 0,
        };

        importer.parse_metadata()?;
        importer.detect_fxl();
        importer.parse_navigation()?;
        importer.index_section_storylines()?;
        importer.parse_spine()?;
        // Needs the explicit reading-order direction captured by
        // `parse_spine` (it is the strongest override).
        importer.derive_writing_direction();
        // `build_image_index` reads `metadata` and the spine, and runs while
        // `section_names` holds sections, ahead of the fixed-layout expansion
        // that renames entries per page.
        importer.build_image_index();
        // Fixed-layout books: split each section into per-page spine entries
        // (needs `content_ppd` from `derive_writing_direction` for the
        // spread pairing). Reflowable books instead carry the box of any
        // section that states one, the cover among them.
        importer.expand_fxl_spine()?;
        importer.read_page_boxes();
        // Content name → location, which a per-token text lookup reads
        // directly.
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
            // bokai's own exporter emits a plain struct. Handle both.
            if let Some(fields) = elem.unwrap_annotated().as_struct()
                && let Some(list) =
                    get_field(fields, sym!(CategorisedMetadata)).and_then(|m| m.as_list())
            {
                for category_elem in list {
                    if let Some(cat_fields) = category_elem.as_struct() {
                        let category = get_field(cat_fields, sym!(Category))
                            .and_then(|v| self.get_symbol_text(v))
                            .unwrap_or("")
                            .to_string();

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
                                    // First-wins and skip-empty guards over
                                    // a container repeating a key.
                                    "title" if self.metadata.title.is_empty() => {
                                        self.metadata.title = value.to_string()
                                    }
                                    // One entry per repeated `author` key,
                                    // each value pushed verbatim: no trim
                                    // and no `&` split.
                                    "author" if !value.is_empty() => {
                                        self.metadata.authors.push(value.to_string())
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
                                        // Amazon catalogue id, beside
                                        // `book_id`'s per-device UUID in
                                        // `kindle_title_metadata`.
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
                                    // One entry per repeated key, positional
                                    // with `author` (Amazon repeats both in
                                    // the same order).
                                    "author_pronunciation" if !value.is_empty() => {
                                        self.metadata.author_sorts.push(value.to_string())
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

                        if category == "kindle_ebook_metadata"
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
                                if key == "book_orientation_lock" {
                                    self.metadata.orientation_lock = OrientationLock::parse(value);
                                }
                            }
                        }
                    }
                }
            }
        }

        // The flat `$258 metadata` entity fills what `kindle_title_metadata`
        // left empty: `title`, `language`, `publisher`, `author`, `ASIN`,
        // `cover_image`.
        self.parse_flat_metadata_fallback()?;

        Ok(())
    }

    /// Fill empty metadata from the flat `$258 metadata` entity's direct
    /// fields. Every write is empty-guarded: `kindle_title_metadata` wins.
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

    /// Resolve one `book_navigation.nav_containers` entry to its
    /// `nav_container` struct: an inline struct passes through, and a bare
    /// symbol names the `$391` entity carrying that id. `None` for neither.
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

                                    // A book carries one container of a
                                    // `nav_type` per reading order, and
                                    // each appends.
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

                    // The label comes from `representation.label`.
                    // "cover-nav-unit" is a placeholder.
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
                        target: None,
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
                    // The label comes from `representation.label`, then from `label`.
                    // An absent one takes "Untitled" and the entry stays; a
                    // present-but-empty one and "heading-nav-unit" are dropped.
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

    /// Parse the spine from `reading_orders`, sizing entries through the
    /// section→storyline cache and carrying the selected order's
    /// `page_progression_direction` onto `metadata`.
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
                panels: Vec::new(),
            });
        }

        Ok(())
    }

    /// Read the book-level fixed-layout signals from both declaration sites:
    /// `yj_*fixed_layout` switches spine construction to the per-page
    /// expansion, `yj_double_page_spread` marks a spread comic.
    fn detect_fxl(&mut self) {
        let features: Vec<EntityLoc> = self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::ContentFeatures as u32)
            .copied()
            .collect();
        let capabilities: Vec<EntityLoc> = self
            .entities
            .iter()
            .filter(|e| e.type_id == KfxSymbol::BookMetadata as u32)
            .copied()
            .collect();
        let mut acc = fxl::FxlFeatures::default();
        for loc in features {
            if let Ok(elem) = self.parse_entity_ion(loc) {
                fxl::scan_content_features(&elem, &self.symbols, &mut acc);
            }
        }
        for loc in capabilities {
            if let Ok(elem) = self.parse_entity_ion(loc) {
                fxl::scan_capability_metadata(&elem, &self.symbols, &mut acc);
            }
        }
        self.metadata.fixed_layout = acc.fixed_layout;
        if acc.double_page_spread {
            self.metadata.book_type = Some("comic".to_string());
        }
    }

    /// Each section's first page template, by section name, in one pass over
    /// the section entities.
    fn section_templates(&mut self) -> HashMap<String, IonValue> {
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
        templates
    }

    /// Carry each section's own `fixed_width`/`fixed_height` onto its spine
    /// entry. In a reflowable book the cover is such a section: a page
    /// authored to a pixel box, which the reader scales to the screen rather
    /// than reflowing into the reading area. A fixed-layout book reads its
    /// boxes per page in [`Self::expand_fxl_spine`] instead.
    fn read_page_boxes(&mut self) {
        if self.metadata.fixed_layout {
            return;
        }
        let templates = self.section_templates();
        for (entry, section) in self.spine.iter_mut().zip(&self.section_names) {
            entry.viewport = templates
                .get(section)
                .and_then(|template| template.unwrap_annotated().as_struct())
                .and_then(|fields| {
                    Some((
                        fxl::read_px(fields, KfxSymbol::FixedWidth)?,
                        fxl::read_px(fields, KfxSymbol::FixedHeight)?,
                    ))
                });
        }
    }

    /// Replace the one-entry-per-section spine with one entry per page. A
    /// section's first page template is a `page_spread` / `facing_page`
    /// container or a single leaf page, named `{section}[-left|-right]`.
    fn expand_fxl_spine(&mut self) -> io::Result<()> {
        if !self.metadata.fixed_layout || self.section_names.is_empty() {
            return Ok(());
        }

        // Structure ($608) entity name → location, for `page_templates`
        // entries holding a symbol reference.
        for e in &self.entities {
            if e.type_id == KfxSymbol::Structure as u32 {
                let name = self.symbols.resolve(e.id as u64).to_string();
                self.structures_by_name.entry(name).or_insert(*e);
            }
        }

        let templates = self.section_templates();

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
            let leaves = fxl::page_leaves(template, &self.symbols, &self.content_ppd, self);
            for (ordinal, (leaf, page_spread)) in leaves.iter().enumerate() {
                let name = match page_spread {
                    Some(PageSpread::Left) => format!("{sec}-left"),
                    Some(PageSpread::Right) => format!("{sec}-right"),
                    _ => sec.clone(),
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
                // §12.6 — the panels sit in the page's own content list, each
                // a region paired with the `zoom_target` its `activate` names.
                let panels = leaf
                    .unwrap_annotated()
                    .as_struct()
                    .map(|f| fxl::page_panels(&self.page_children(f).0))
                    .unwrap_or_default();
                spine.push(SpineEntry {
                    id: ChapterId(names.len() as u32),
                    size_estimate,
                    page_spread: *page_spread,
                    viewport,
                    panels,
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

    /// Splice each `story_name` reference's `content_list` into the struct
    /// carrying it. `stack` guards a reference cycle; a story two siblings
    /// reference inlines twice.
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

    /// Build one chapter's IR over a shared `&self`, touching no importer
    /// state. Returns the chapter and every element id it declares, in the
    /// order the caller stamps them into `eid_chapters`.
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

        // Every eid the storyline declares counts, the elements the IR
        // drops included: a nav target resolves its file here.
        let mut declared_eids = Vec::new();
        collect_declared_eids(&storyline_ion, &mut declared_eids);

        let writing_mode = self.document_axis();
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
            Styles {
                by_name: Some(styles.as_ref()),
                writing_mode,
            },
            Some(ruby_index.as_ref()),
            Some(anchor_table.as_ref()),
            |name, index| self.lookup_content_text(name, index),
        );

        // Re-root under the section's main page-template container: an anchor
        // targeting the template or the storyline root stamps onto a real
        // element.
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
            &template,
            story_eid,
            &self.symbols,
            Styles {
                by_name: Some(styles.as_ref()),
                writing_mode,
            },
            Some(anchor_table.as_ref()),
        );

        // `export::epub::dom::consolidate_part` cleans up the tree the KFX
        // token→IR builder produces. `html::optimize` serves the HTML-sourced
        // importers through `compile_html`.

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

    /// A page container's children with every `story_name` reference inlined,
    /// and the root element id its storyline declares. An inline
    /// `content_list` wins over the container's story.
    fn page_children(&self, cfields: &[(u64, IonValue)]) -> (Vec<IonValue>, Option<i64>) {
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
        let children = children
            .into_iter()
            .map(|c| self.inline_story_refs(c, &mut Vec::new()))
            .collect();
        (children, story_eid)
    }

    /// Build one fixed-layout page as a chapter: the page's leaf container
    /// from the cached spread walk, a storyline synthesized from its children
    /// (inline `content_list` over its story), the container as the root.
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

        let (children, story_eid) = self.page_children(cfields);
        // §12.6 — `expand_fxl_spine` reads the panel elements into the spine
        // entry's `panels`, and the page's content carries the page image.
        let children: Vec<IonValue> = children
            .into_iter()
            .filter(|c| !fxl::is_panel_element(c))
            .collect();
        let synthetic = IonValue::Struct(vec![(sym!(ContentList), IonValue::List(children))]);

        // Every eid the page subtree declares, the container's own included,
        // counts for nav-target file resolution. The caller registers them
        // first-wins across pages.
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

        let writing_mode = self.document_axis();
        let symbols = Arc::clone(&self.symbols);
        let anchors = Arc::clone(&self.anchors);
        let styles = Arc::clone(&self.styles);
        let ruby_index = Arc::clone(&self.ruby_index);
        let anchor_table = Arc::clone(&self.anchor_table);
        let mut chapter = parse_storyline_to_ir(
            &synthetic,
            symbols.as_ref(),
            Some(anchors.as_ref()),
            Styles {
                by_name: Some(styles.as_ref()),
                writing_mode,
            },
            Some(ruby_index.as_ref()),
            Some(anchor_table.as_ref()),
            |name, index| self.lookup_content_text(name, index),
        );

        // Root the chapter at the page container: class from its `$157` style,
        // inline style from its converted outer fields, `(eid, 0)` anchors
        // stamping onto the root.
        crate::formats::kfx::storyline::apply_section_template(
            &mut chapter,
            &SectionTemplate {
                eid: container_eid,
                style: style_name,
                inline_style,
            },
            story_eid,
            &self.symbols,
            Styles {
                by_name: Some(styles.as_ref()),
                writing_mode,
            },
            Some(anchor_table.as_ref()),
        );

        self.rewrite_image_srcs(&mut chapter);
        Ok((chapter, declared))
    }

    /// Point a converted rule's `background-image` at the exported file.
    /// `convert_yj_properties` renders the KFX symbol as `url(eF)`, and the
    /// resource name becomes the filename here.
    fn rewrite_css_image_urls(&self, decl: &mut CssDecl) {
        let Some(value) = decl.get("background-image") else {
            return;
        };
        let Some(name) = value
            .strip_prefix("url(")
            .and_then(|v| v.strip_suffix(')'))
            .map(|v| v.trim_matches(['"', '\'']))
        else {
            return;
        };
        let Some(&i) = self.image_by_name.get(name) else {
            return;
        };
        let filename = self.images[i].filename.clone();
        decl.set("background-image", format!("url(\"{filename}\")"));
    }

    /// Rewrite image references from KFX resource names (`eF`) to the exported
    /// asset filenames (`image_rsrc7.jpg`), over both places a picture is
    /// named: an element's `src` and a style's `background-image`.
    fn rewrite_image_srcs(&self, chapter: &mut Chapter) {
        let filename_of = |name: &str| {
            self.image_by_name
                .get(name)
                .map(|&i| self.images[i].filename.clone())
        };
        let node_ids: Vec<_> = chapter.iter_dfs().collect();
        for node_id in node_ids {
            let Some(filename) = chapter.semantics.src(node_id).and_then(&filename_of) else {
                continue;
            };
            chapter.semantics.set_src(node_id, &filename);
        }
        chapter.styles.rewrite(|style| {
            if let Some(filename) = style.background_image.as_deref().and_then(&filename_of) {
                style.background_image = Some(filename);
            }
        });
    }

    /// Derive `metadata.primary_writing_mode` and
    /// `metadata.page_progression_direction`. `document_data.writing_mode` is
    /// taken as written, and a `-rl` mode turns the page RTL.
    fn derive_writing_direction(&mut self) {
        let mut writing_mode = None;
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
                    Some(crate::formats::kfx::writing_mode::normalize_writing_mode(wm).to_string());
            }
            if let Some(dir) =
                get_field(fields, sym!(Direction)).and_then(|v| self.symbols.text_of(v))
            {
                ppd = dir.to_string();
            }
        }

        let writing_mode = writing_mode.unwrap_or_else(|| {
            let styles: Vec<IonValue> = self
                .entities
                .iter()
                .filter(|e| e.type_id == KfxSymbol::Style as u32)
                .filter_map(|loc| self.parse_entity_ion(*loc).ok())
                .collect();
            crate::formats::kfx::writing_mode::majority_vertical_mode(styles.iter(), &self.symbols)
                .unwrap_or_else(|| "horizontal-tb".to_string())
        });
        if writing_mode.ends_with("-rl") {
            ppd = "rtl".to_string();
        }
        // The content walk alternates its spread pairing from the value ahead
        // of the explicit reading-order override.
        self.content_ppd = ppd.clone();
        // `$default` defers to the heuristics above. Only a concrete
        // direction overrides them.
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

    /// Resolve an eid to its owning chapter's start: `eid_chapters` first,
    /// then `section_eids`.
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

        // Map each section to its storyline and record the element ids the
        // section struct declares. The main template is the last
        // `page_templates` entry, whose id and style ride onto the root.
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

    /// Section names and `page_progression_direction` from the reading orders
    /// of both `document_data` ($538) and `metadata` ($258), preferring the
    /// "default" order. The walk collects from each fragment.
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
    /// The KFX value is a symbol (`$rtl`/`$ltr`/`$default`); the EPUB `<spine>`
    /// attribute takes it with the leading `$` stripped.
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

    /// Text content by name and index, loading and caching the content entity
    /// on first reach. `&self` is shared across parallel chapter builds, and
    /// a race on one name caches identical lists.
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
    /// each content entity once.
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

    /// Build the canonical image list through `kfx::resource_index`, resolve
    /// the cover from `metadata` or the first section's full-page image,
    /// rename it `cover.<ext>` and point `metadata.cover_image` at it.
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

    /// Cover fallback: the first `resource_name` the first reading-order
    /// section's storyline lays out, taken only where it names a raster
    /// image.
    fn first_section_cover_candidate(&self, images: &[ImageResource]) -> Option<String> {
        let first_section = self.section_names.first()?;
        let storyline_loc = *self.section_storylines.get(first_section)?;
        let storyline_ion = self.parse_entity_ion(storyline_loc).ok()?;
        let fields = storyline_ion.unwrap_annotated().as_struct()?;
        let content_list = get_field(fields, sym!(ContentList))?;
        let candidate = resource_index::first_content_resource_name(content_list, &self.symbols)?;
        resource_index::is_raster_cover(images, &candidate).then_some(candidate)
    }

    /// Bytes for `images[idx]` as exported: a JPEG-XR source is transcoded to
    /// JPEG, a decode failure and every other format copied verbatim.
    fn load_image_bytes(&self, idx: usize) -> io::Result<Vec<u8>> {
        let img = &self.images[idx];
        let raw = self.read_image_raw(idx)?;
        if img.is_jxr {
            crate::formats::kfx::jxr::transcode(&raw, &img.resource_name)
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

    /// Index anchor entities into the `anchor_name` → uri/position maps a
    /// `link_to` resolves through.
    fn index_anchor_entities(&mut self) -> io::Result<()> {
        if self.anchors_indexed {
            return Ok(());
        }

        // Real `$266` anchors, registered in sorted-name order: a position
        // carrying several anchors picks one first name per run.
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

        // Synthetic anchors at nav target positions: TOC first, then page-list,
        // then `$798` heading levels. Runs over the raw nav entries, empty
        // labels and placeholder units included.
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

    /// Index style entities into the `style_name` → properties map a storyline
    /// element's `$157` reference resolves through.
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

    /// Index `ruby_content` ($756) entities into `ruby_index`: each
    /// `content_list` entry's `content` lands at `ruby_id - 1`, which a
    /// style_event reads as `ruby_index[ruby_name][ruby_id - 1]`.
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

                // Collect `(ruby_id, text)` into a dense vec sized by the
                // maximum id, which `parse_style_events` subscripts.
                // `ruby_id` counts from 1.
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

impl fxl::PageContext for KfxImporter {
    fn structure(&self, name: &str) -> Option<IonValue> {
        let loc = self.structures_by_name.get(name)?;
        self.parse_entity_ion(*loc).ok()
    }

    fn storyline_pages(&self, story: &str) -> Vec<IonValue> {
        self.storyline_parts(story).1
    }
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
